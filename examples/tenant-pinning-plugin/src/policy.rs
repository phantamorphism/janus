//! The policy file and the routing decision it drives. Pure: no gRPC, no
//! clock, no counters, so every rule is unit-testable on its own.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// The claim that names a caller's tenant.
pub const TENANT_CLAIM: &str = "tenant";
/// The claim that, set to [`BLOCKED_TIER`], denies a caller outright.
pub const TIER_CLAIM: &str = "tier";
pub const BLOCKED_TIER: &str = "blocked";

/// The policy file, as loaded at startup.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Deny a caller whose tenant is missing or not under `[tenants]`.
    /// `false`: such a caller gets the instance's default chain.
    #[serde(default)]
    pub deny_unlisted: bool,
    /// Every admitted request is charged against this.
    pub budget: Budget,
    /// Tenant name (the `tenant` claim's value) to its pins.
    #[serde(default)]
    pub tenants: BTreeMap<String, Tenant>,
}

/// A per-identity request budget: `requests` calls per `window_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub requests: u64,
    pub window_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    /// Upstream instance name (`[[upstream]].name`) to the credential ref
    /// this tenant uses there. An instance not listed gets the default chain.
    #[serde(default)]
    pub pin: BTreeMap<String, String>,
}

/// Where an admitted request's credential comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Answer `credential_ref`.
    Pin(String),
    /// Answer `use_default`.
    Default,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("cannot read policy file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid policy file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid policy: {0}")]
    Invalid(String),
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let text = std::fs::read_to_string(path).map_err(|source| PolicyError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parse and validate: a policy that could never admit anything, or that
    /// pins an empty ref (which the daemon would refuse as a protocol
    /// error), is refused at startup rather than at the first request.
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        let policy: Policy = toml::from_str(text)?;
        if policy.budget.requests == 0 || policy.budget.window_secs == 0 {
            return Err(PolicyError::Invalid(
                "budget.requests and budget.window_secs must both be above zero".into(),
            ));
        }
        for (name, tenant) in &policy.tenants {
            if let Some(upstream) = tenant.pin.iter().find(|(_, r)| r.is_empty()).map(|p| p.0) {
                return Err(PolicyError::Invalid(format!(
                    "tenants.{name}.pin.{upstream} is empty"
                )));
            }
        }
        Ok(policy)
    }

    /// The decision before the budget: `Err` is the deny reason, which
    /// reaches the caller. `upstream` is the instance name.
    pub fn decide(
        &self,
        caller_identity: &str,
        upstream: &str,
        claims: &BTreeMap<String, String>,
    ) -> Result<Route, String> {
        if claims.get(TIER_CLAIM).map(String::as_str) == Some(BLOCKED_TIER) {
            return Err(format!(
                "identity {caller_identity} is blocked (tier=blocked)"
            ));
        }
        let tenant = claims.get(TENANT_CLAIM);
        match tenant.and_then(|t| self.tenants.get(t).map(|cfg| (t, cfg))) {
            Some((_, cfg)) => Ok(match cfg.pin.get(upstream) {
                Some(credential_ref) => Route::Pin(credential_ref.clone()),
                None => Route::Default,
            }),
            None if self.deny_unlisted => Err(match tenant {
                Some(t) => format!("tenant {t:?} is not allowed by policy"),
                None => format!("identity {caller_identity} has no tenant claim"),
            }),
            None => Ok(Route::Default),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
        deny_unlisted = false
        budget = { requests = 3, window_secs = 60 }

        [tenants.acme]
        pin = { claude = "claude-acme" }

        [tenants.globex]
        pin = { claude = "claude-globex", openai = "openai-globex" }
    "#;

    fn claims(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn policy() -> Policy {
        Policy::parse(POLICY).expect("example policy parses")
    }

    #[test]
    fn a_listed_tenant_is_pinned_on_its_instance() {
        let p = policy();
        let acme = claims(&[("tenant", "acme")]);
        assert_eq!(
            p.decide("a1", "claude", &acme),
            Ok(Route::Pin("claude-acme".into()))
        );
        let globex = claims(&[("tenant", "globex")]);
        assert_eq!(
            p.decide("g1", "openai", &globex),
            Ok(Route::Pin("openai-globex".into()))
        );
    }

    #[test]
    fn a_listed_tenant_without_a_pin_for_the_instance_gets_the_default_chain() {
        let acme = claims(&[("tenant", "acme")]);
        assert_eq!(policy().decide("a1", "openai", &acme), Ok(Route::Default));
    }

    #[test]
    fn unlisted_and_missing_tenants_get_the_default_chain_unless_denied() {
        let mut p = policy();
        let initech = claims(&[("tenant", "initech")]);
        assert_eq!(p.decide("i1", "claude", &initech), Ok(Route::Default));
        assert_eq!(p.decide("n1", "claude", &claims(&[])), Ok(Route::Default));

        p.deny_unlisted = true;
        assert_eq!(
            p.decide("i1", "claude", &initech),
            Err("tenant \"initech\" is not allowed by policy".into())
        );
        assert_eq!(
            p.decide("n1", "claude", &claims(&[])),
            Err("identity n1 has no tenant claim".into())
        );
    }

    #[test]
    fn a_blocked_tier_is_denied_even_for_a_listed_tenant() {
        let blocked = claims(&[("tenant", "acme"), ("tier", "blocked")]);
        assert_eq!(
            policy().decide("a2", "claude", &blocked),
            Err("identity a2 is blocked (tier=blocked)".into())
        );
        let gold = claims(&[("tenant", "acme"), ("tier", "gold")]);
        assert!(policy().decide("a3", "claude", &gold).is_ok());
    }

    #[test]
    fn invalid_policies_are_refused() {
        let zero = "budget = { requests = 0, window_secs = 60 }";
        assert!(matches!(Policy::parse(zero), Err(PolicyError::Invalid(_))));
        let empty_ref =
            "budget = { requests = 1, window_secs = 1 }\n[tenants.a]\npin = { claude = \"\" }";
        assert!(matches!(
            Policy::parse(empty_ref),
            Err(PolicyError::Invalid(_))
        ));
        let typo = "budget = { requests = 1, window_secs = 1 }\ndeny_unlisetd = true";
        assert!(matches!(Policy::parse(typo), Err(PolicyError::Parse(_))));
    }
}
