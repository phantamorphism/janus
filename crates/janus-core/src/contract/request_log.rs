//! The request log's record and its fixed vocabulary
//! (`docs/admin-surface#request-log`).
//!
//! The data plane writes a [`RequestRecord`], the daemon store persists it,
//! the admin surface serves and filters it, and the `janus` CLI renders it;
//! all four use this one type and its JSON shape.
//!
//! The docs define the outcomes and the `rejected_identity` kinds but do not
//! name the `abandoned` stages beyond "which step of the lifecycle it died
//! at". [`AbandonStage`] is that vocabulary, an implementation decision
//! pinned here so the request log and `call_abandoned` agree: one name per
//! step of `docs/architecture#request-lifecycle` after step 3's answer.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::binding::Method;
use super::identity::RejectionKind;
use super::plugin::PluginFailureClass;
use super::upstream::UpstreamKind;

/// The lifecycle step a call died at before reaching the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbandonStage {
    /// Step 3: `ResolveRoute` was sent and failed (timed out, errored, the
    /// plugin became unreachable mid-call).
    Resolve,
    /// Step 4: the pool could not make the selected credential usable.
    Credential,
    /// Step 5: the outbound request could not be constructed.
    Rewrite,
    /// Step 6: the upstream could not be reached, or the caller went away
    /// before the request was sent.
    Forward,
}

impl AbandonStage {
    pub const ALL: [AbandonStage; 4] = [
        AbandonStage::Resolve,
        AbandonStage::Credential,
        AbandonStage::Rewrite,
        AbandonStage::Forward,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AbandonStage::Resolve => "resolve",
            AbandonStage::Credential => "credential",
            AbandonStage::Rewrite => "rewrite",
            AbandonStage::Forward => "forward",
        }
    }
}

impl fmt::Display for AbandonStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the request ended. Serialized as an `outcome` field plus, where the
/// docs give one, its qualifier (`kind`, `reason`, `class`, `stage`) beside
/// it on the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Forwarded,
    RejectedIdentity {
        kind: RejectionKind,
    },
    /// The plugin's deny reason.
    Denied {
        reason: String,
    },
    PluginFailed {
        class: PluginFailureClass,
    },
    CredentialUnavailable,
    UpstreamError,
    Abandoned {
        stage: AbandonStage,
    },
}

impl Outcome {
    /// The `outcome` value, also the `?outcome=` filter of `/v1/requests`.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Forwarded => "forwarded",
            Outcome::RejectedIdentity { .. } => "rejected_identity",
            Outcome::Denied { .. } => "denied",
            Outcome::PluginFailed { .. } => "plugin_failed",
            Outcome::CredentialUnavailable => "credential_unavailable",
            Outcome::UpstreamError => "upstream_error",
            Outcome::Abandoned { .. } => "abandoned",
        }
    }
}

/// One record per request that reached the data plane, including the ones
/// rejected at identity extraction. Metadata only: never a request or
/// response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// The same value a plugin sees in `ResolveRoute` and `Notify`.
    pub request_id: String,
    pub received_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// `None` when the path matched no upstream's `path_prefix`.
    pub upstream: Option<UpstreamKind>,
    /// The matched route path, if a route binding applied.
    pub route: Option<String>,
    /// The HTTP method.
    pub method: String,
    pub path: String,
    /// The identity method that ran; `None` when no binding resolved.
    pub binding: Option<Method>,
    /// The verified identity; empty on a `none` route and on a rejected
    /// request.
    pub caller_identity: String,
    /// Rejected requests only: what the material claimed, decoded without
    /// verification. Log-only; never authorizes anything.
    pub claimed_identity: Option<String>,
    pub correlation_id: Option<String>,
    #[serde(flatten)]
    pub outcome: Outcome,
    /// What was actually used, when one was.
    pub credential_ref: Option<String>,
    /// The upstream's HTTP status, when the call was forwarded.
    pub status: Option<u16>,
    /// The upstream's observation object, identical to `call_completed`'s.
    pub usage: Option<serde_json::Value>,
    /// A short diagnostic.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: Outcome) -> RequestRecord {
        RequestRecord {
            request_id: "req-1".into(),
            received_at: DateTime::from_timestamp(1_700_000_000, 0).expect("ts"),
            duration_ms: 12,
            upstream: Some(UpstreamKind::OpenAi),
            route: None,
            method: "POST".into(),
            path: "/openai/v1/chat/completions".into(),
            binding: Some(Method::Hmac),
            caller_identity: String::new(),
            claimed_identity: Some("session-9".into()),
            correlation_id: None,
            outcome,
            credential_ref: None,
            status: None,
            usage: None,
            error: None,
        }
    }

    #[test]
    fn every_outcome_serializes_with_its_documented_name_and_qualifier() {
        let cases = [
            (Outcome::Forwarded, "forwarded", None),
            (
                Outcome::RejectedIdentity {
                    kind: RejectionKind::WrongNamespace,
                },
                "rejected_identity",
                Some(("kind", "wrong_namespace")),
            ),
            (
                Outcome::Denied {
                    reason: "budget exhausted".into(),
                },
                "denied",
                Some(("reason", "budget exhausted")),
            ),
            (
                Outcome::PluginFailed {
                    class: PluginFailureClass::Timeout,
                },
                "plugin_failed",
                Some(("class", "plugin_timeout")),
            ),
            (
                Outcome::CredentialUnavailable,
                "credential_unavailable",
                None,
            ),
            (Outcome::UpstreamError, "upstream_error", None),
            (
                Outcome::Abandoned {
                    stage: AbandonStage::Credential,
                },
                "abandoned",
                Some(("stage", "credential")),
            ),
        ];
        for (outcome, name, qualifier) in cases {
            assert_eq!(outcome.name(), name);
            let rec = record(outcome);
            let json = serde_json::to_value(&rec).expect("ser");
            assert_eq!(json["outcome"], name);
            if let Some((key, value)) = qualifier {
                assert_eq!(json[key], value, "{name}");
            }
            assert_eq!(json["binding"], "hmac");
            assert_eq!(json["upstream"], "openai");
            assert_eq!(
                serde_json::from_value::<RequestRecord>(json).expect("de"),
                rec
            );
        }
    }

    #[test]
    fn abandon_stages_are_one_per_lifecycle_step() {
        assert_eq!(
            AbandonStage::ALL.map(AbandonStage::as_str),
            ["resolve", "credential", "rewrite", "forward"]
        );
        for stage in AbandonStage::ALL {
            assert_eq!(
                serde_json::to_string(&stage).expect("ser"),
                format!("\"{stage}\"")
            );
        }
    }
}
