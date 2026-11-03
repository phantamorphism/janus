//! The `trusted_header` method: the header value *is* the identity, with no
//! cryptographic check, because a front proxy or mesh already authenticated
//! the caller (`docs/caller-identity#methods`). Correct in exactly that
//! topology; reachable only by naming `method = "trusted_header"` in config.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::contract::binding::{Source, TrustedHeaderSpec};
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, IdentityExtractor, RequestHead,
};

use super::claims::correlation_value;
use super::source;

pub(crate) struct TrustedHeaderExtractor {
    spec: TrustedHeaderSpec,
    source: Source,
}

impl TrustedHeaderExtractor {
    pub(crate) fn new(spec: TrustedHeaderSpec) -> Self {
        let source = Source::Header {
            name: spec.header.clone(),
            scheme: spec.scheme.clone(),
        };
        TrustedHeaderExtractor { spec, source }
    }
}

#[async_trait]
impl IdentityExtractor for TrustedHeaderExtractor {
    fn name(&self) -> &str {
        "trusted_header"
    }

    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let caller_identity = source::read(&self.source, req)?;
        let claims = BTreeMap::new();
        let correlation_id = correlation_value(self.spec.correlation.as_ref(), &claims, req);
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
    use crate::contract::binding::{BindingSpec, Correlation, IdentityBindingConfig};
    use crate::contract::identity::PresentedAt;
    use crate::identity::testkit::head;
    use http::HeaderName;

    #[tokio::test]
    async fn trusted_header_takes_the_value_as_identity() {
        let ex = TrustedHeaderExtractor::new(TrustedHeaderSpec {
            header: HeaderName::from_static("x-authenticated-user"),
            scheme: None,
            correlation: Some(Correlation::Header(HeaderName::from_static("x-request-id"))),
        });
        let id = ex
            .extract(&head(
                "/",
                &[
                    ("x-authenticated-user", "alice@corp"),
                    ("x-request-id", "r-1"),
                ],
            ))
            .await
            .expect("present");
        assert_eq!(id.caller_identity, "alice@corp");
        assert!(id.claims.is_empty());
        assert_eq!(id.correlation_id.as_deref(), Some("r-1"));
        assert_eq!(
            id.presented_at,
            PresentedAt::Header(HeaderName::from_static("x-authenticated-user"))
        );
        assert_eq!(
            ex.extract(&head("/", &[("authorization", "Bearer alice")]))
                .await,
            Err(ExtractionError::NoMaterial)
        );
    }

    #[test]
    fn trusted_header_off_by_default_and_never_reached_by_omission() {
        // A binding that only names a header source has no method: refused,
        // not silently treated as "take the header".
        let only_source: IdentityBindingConfig =
            toml::from_str(r#"source = { header = "x-user" }"#).expect("parses");
        assert!(only_source.resolve().is_err());
        // The default block's method survives an upstream that only changes
        // the source: it does not degrade into trusted_header.
        let default: IdentityBindingConfig = toml::from_str(
            r#"
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }
"#,
        )
        .expect("parses");
        let merged = only_source
            .merged_over(&default)
            .resolve()
            .expect("resolves");
        assert!(matches!(merged, BindingSpec::Opaque(_)), "{merged:?}");
        assert!(IdentityBindingConfig::default().resolve().is_err());
        // Only the explicit spelling reaches it.
        let explicit: IdentityBindingConfig = toml::from_str(
            r#"
method = "trusted_header"
source = { header = "x-user" }
"#,
        )
        .expect("parses");
        assert!(matches!(
            explicit.resolve(),
            Ok(BindingSpec::TrustedHeader(_))
        ));
    }
}
