//! The `opaque` method: a bare random token, verified by probing the
//! daemon's opaque-identity registry with its SHA-256
//! (`docs/caller-identity#registry`).

use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::binding::OpaqueSpec;
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, IdentityExtractor, RequestHead,
};
use crate::contract::registry::{OpaqueRegistry, TokenHash};

use super::claims::correlation_value;
use super::factory::Clock;
use super::source;

pub(crate) struct OpaqueExtractor {
    spec: OpaqueSpec,
    registry: Arc<dyn OpaqueRegistry>,
    clock: Clock,
}

impl OpaqueExtractor {
    pub(crate) fn new(spec: OpaqueSpec, registry: Arc<dyn OpaqueRegistry>, clock: Clock) -> Self {
        OpaqueExtractor {
            spec,
            registry,
            clock,
        }
    }
}

#[async_trait]
impl IdentityExtractor for OpaqueExtractor {
    fn name(&self) -> &str {
        "opaque"
    }

    /// Unknown token: `invalid`. Revoked or expired entry: `revoked` /
    /// `expired`, per [`crate::contract::registry::IdentityRecord::verdict`].
    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let token = source::read(&self.spec.source, req)?;
        let now = self.clock.now();
        let record = self
            .registry
            .lookup(&TokenHash::of(token.as_bytes()), now)
            .ok_or_else(|| ExtractionError::Invalid("unknown opaque token".into()))?;
        record.verdict(now)?;
        if record.identity.is_empty() {
            return Err(ExtractionError::Invalid(
                "registry entry has an empty identity".into(),
            ));
        }
        let correlation_id = correlation_value(self.spec.correlation.as_ref(), &record.claims, req);
        Ok(ExtractedIdentity {
            caller_identity: record.identity,
            claims: record.claims,
            correlation_id,
            presented_at: self.spec.source.presented_at(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::binding::{Correlation, Source};
    use crate::contract::identity::PresentedAt;
    use crate::identity::testkit::{at, clock_at, head, record, FakeRegistry};

    const NOW: i64 = 1_700_000_000;

    fn extractor(registry: Arc<FakeRegistry>) -> OpaqueExtractor {
        OpaqueExtractor::new(
            OpaqueSpec {
                source: Source::Query { name: "key".into() },
                correlation: Some(Correlation::Claim("tenant".into())),
            },
            registry,
            clock_at(NOW),
        )
    }

    #[tokio::test]
    async fn opaque_resolves_a_live_token_and_records_last_seen() {
        let reg = Arc::new(FakeRegistry::default());
        reg.insert("tok-live", record("sandbox-7", None, None));
        let id = extractor(reg.clone())
            .extract(&head("/v1/models?key=tok-live", &[]))
            .await
            .expect("live");
        assert_eq!(id.caller_identity, "sandbox-7");
        assert_eq!(id.claims.get("tenant").map(String::as_str), Some("acme"));
        assert_eq!(id.correlation_id.as_deref(), Some("acme"));
        assert_eq!(id.presented_at, PresentedAt::Query("key".into()));
        assert_eq!(reg.last_seen("tok-live"), Some(at(NOW)));
    }

    #[tokio::test]
    async fn opaque_rejects_revoked_expired_and_unknown_tokens() {
        let reg = Arc::new(FakeRegistry::default());
        reg.insert("tok-rev", record("sandbox-7", None, Some(at(NOW - 1))));
        reg.insert("tok-exp", record("sandbox-8", Some(at(NOW)), None));
        reg.insert("tok-later", record("sandbox-9", None, Some(at(NOW + 10))));
        let ex = extractor(reg.clone());
        assert_eq!(
            ex.extract(&head("/?key=tok-rev", &[])).await,
            Err(ExtractionError::Revoked)
        );
        assert_eq!(
            ex.extract(&head("/?key=tok-exp", &[])).await,
            Err(ExtractionError::Expired)
        );
        assert!(ex.extract(&head("/?key=tok-later", &[])).await.is_ok());
        assert!(matches!(
            ex.extract(&head("/?key=tok-unknown", &[])).await,
            Err(ExtractionError::Invalid(_))
        ));
        assert_eq!(
            ex.extract(&head("/", &[])).await,
            Err(ExtractionError::NoMaterial)
        );
        // Revocation takes effect on the very next request.
        reg.revoke("tok-later", at(NOW));
        assert_eq!(
            ex.extract(&head("/?key=tok-later", &[])).await,
            Err(ExtractionError::Revoked)
        );
        // Opaque material claims nothing that could be logged.
        assert_eq!(ex.claimed_identity(&head("/?key=tok-rev", &[])), None);
    }
}
