//! Turning verified claims into an [`ExtractedIdentity`]: the identity
//! claim, the remaining claims as a string map, and the correlation value
//! (`docs/caller-identity#extractor`, `#correlation`).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::contract::binding::{Correlation, Source};
use crate::contract::identity::{ExtractedIdentity, ExtractionError, RequestHead};

use super::source;

/// A claim value as text: strings as-is, numbers and booleans as their JSON
/// text, arrays and objects as compact JSON. `null` has no text.
pub(crate) fn claim_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            Some(value.to_string())
        }
    }
}

/// The identity a claim names: a non-empty string or a number. Anything
/// else is not an identity.
pub(crate) fn identity_text(claim: &str, value: Option<&Value>) -> Result<String, ExtractionError> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(_) => Err(ExtractionError::Invalid(format!(
            "claim {claim:?} is not a non-empty string or number"
        ))),
        None => Err(ExtractionError::Invalid(format!(
            "verified claims carry no {claim:?} claim"
        ))),
    }
}

/// The binding's correlation value. A `Claim` is read from the verified
/// claims; a `Header` from the request, unverified. Absent either way: no
/// value.
pub(crate) fn correlation_value(
    correlation: Option<&Correlation>,
    claims: &BTreeMap<String, String>,
    head: &RequestHead,
) -> Option<String> {
    let value = match correlation? {
        Correlation::Claim(name) => claims.get(name).cloned(),
        Correlation::Header(name) => head
            .headers
            .get(name)
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            .map(|s| s.trim().to_owned()),
    }?;
    (!value.is_empty()).then_some(value)
}

/// How a signed-claims method (`hmac`, `jwt`) reads its material and
/// projects what it verified, as its binding configures.
pub(crate) struct SignedClaims {
    pub source: Source,
    pub identity_claim: String,
    /// The method's own claims (`ns`, `exp`, `iss`, ...), left out of
    /// `ExtractedIdentity::claims`.
    pub machinery: &'static [&'static str],
    pub correlation: Option<Correlation>,
}

impl SignedClaims {
    /// The raw token at the binding's source.
    pub(crate) fn material(&self, head: &RequestHead) -> Result<String, ExtractionError> {
        source::read(&self.source, head)
    }

    /// The identity `claims` name, for the log-only unverified decode.
    pub(crate) fn claimed(&self, claims: &Map<String, Value>) -> Option<String> {
        identity_text(&self.identity_claim, claims.get(&self.identity_claim)).ok()
    }

    /// The extracted identity for claims that verified.
    pub(crate) fn project(
        &self,
        verified: Map<String, Value>,
        head: &RequestHead,
    ) -> Result<ExtractedIdentity, ExtractionError> {
        let id_claim = self.identity_claim.as_str();
        let caller_identity = identity_text(id_claim, verified.get(id_claim))?;
        let all: BTreeMap<String, String> = verified
            .iter()
            .filter_map(|(k, v)| claim_text(v).map(|t| (k.clone(), t)))
            .collect();
        let correlation_id = correlation_value(self.correlation.as_ref(), &all, head);
        let claims = all
            .into_iter()
            .filter(|(k, _)| k != id_claim && !self.machinery.contains(&k.as_str()))
            .collect();
        Ok(ExtractedIdentity {
            caller_identity,
            claims,
            correlation_id,
            presented_at: self.source.presented_at(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::identity::PresentedAt;
    use crate::identity::testkit::head;
    use http::HeaderName;
    use serde_json::json;

    #[test]
    fn projection_splits_identity_from_remaining_claims() {
        let verified = json!({
            "ns": "janus-data-plane", "exp": 10, "subject": "session-42",
            "agent": "codex", "n": 3, "flag": true, "tags": ["a", "b"], "gone": null
        });
        let p = SignedClaims {
            source: Source::Query { name: "t".into() },
            identity_claim: "subject".into(),
            machinery: &["ns", "exp"],
            correlation: Some(Correlation::Claim("agent".into())),
        };
        let verified = verified.as_object().cloned().expect("object");
        assert_eq!(p.claimed(&verified).as_deref(), Some("session-42"));
        let out = p.project(verified, &head("/", &[])).expect("projects");
        assert_eq!(out.caller_identity, "session-42");
        assert_eq!(
            out.claims,
            BTreeMap::from([
                ("agent".to_owned(), "codex".to_owned()),
                ("flag".to_owned(), "true".to_owned()),
                ("n".to_owned(), "3".to_owned()),
                ("tags".to_owned(), r#"["a","b"]"#.to_owned()),
            ])
        );
        assert_eq!(out.correlation_id.as_deref(), Some("codex"));
        assert_eq!(out.presented_at, PresentedAt::Query("t".into()));
    }

    #[test]
    fn identity_claim_must_be_present_and_usable() {
        for bad in [
            json!({}),
            json!({"sub": ""}),
            json!({"sub": ["x"]}),
            json!({"sub": null}),
        ] {
            assert!(
                matches!(
                    identity_text("sub", bad.get("sub")),
                    Err(ExtractionError::Invalid(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(identity_text("sub", Some(&json!(7))), Ok("7".into()));
    }

    #[test]
    fn header_correlation_is_read_unverified_and_empty_means_none() {
        let h = Correlation::Header(HeaderName::from_static("x-request-id"));
        let claims = BTreeMap::new();
        assert_eq!(
            correlation_value(
                Some(&h),
                &claims,
                &head("/", &[("x-request-id", "trace-1")])
            ),
            Some("trace-1".into())
        );
        assert_eq!(
            correlation_value(Some(&h), &claims, &head("/", &[("x-request-id", " ")])),
            None
        );
        assert_eq!(
            correlation_value(None, &claims, &head("/", &[("x-request-id", "t")])),
            None
        );
        let c = Correlation::Claim("session".into());
        assert_eq!(correlation_value(Some(&c), &claims, &head("/", &[])), None);
    }
}
