//! A registered custom extractor as a binding uses it
//! (`docs/caller-identity#custom`): the build's own `IdentityExtractor`,
//! with the binding's correlation source applied on top and the
//! non-empty-identity invariant enforced.

use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::binding::Correlation;
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, IdentityExtractor, RequestHead,
};

use super::claims::correlation_value;

pub(crate) struct CustomBinding {
    inner: Arc<dyn IdentityExtractor>,
    correlation: Option<Correlation>,
}

impl CustomBinding {
    pub(crate) fn new(inner: Arc<dyn IdentityExtractor>, correlation: Option<Correlation>) -> Self {
        CustomBinding { inner, correlation }
    }
}

#[async_trait]
impl IdentityExtractor for CustomBinding {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let mut id = self.inner.extract(req).await?;
        if id.caller_identity.is_empty() {
            return Err(ExtractionError::Invalid(format!(
                "custom extractor {:?} produced an empty identity",
                self.inner.name()
            )));
        }
        if self.correlation.is_some() {
            id.correlation_id = correlation_value(self.correlation.as_ref(), &id.claims, req);
        }
        Ok(id)
    }

    fn claimed_identity(&self, req: &RequestHead) -> Option<String> {
        self.inner.claimed_identity(req)
    }
}
