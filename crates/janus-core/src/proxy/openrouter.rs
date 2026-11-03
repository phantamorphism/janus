//! OpenRouter (`openrouter.ai/api`): an OpenAI-compatible API.
//!
//! - Auth: `Authorization: Bearer <API key>`. OpenRouter credentials are
//!   API keys only; nothing to refresh.
//! - Target: `base_url` + the inbound path, unchanged.
//! - Forwarded besides the common set: OpenRouter's app-attribution headers
//!   `http-referer` and `x-title`.
//! - Usage: the `usage` object of a completion body, or of the final chunk
//!   of a stream (which includes OpenRouter's `cost`).
//! - Feedback (the rule is in `feedback`): a 401 carrying OpenRouter's own
//!   JSON error (`code` 401) rejects the key; a 401 without one (an edge's
//!   page) is relayed without a verdict. 402 (out of credits) blames the
//!   account; 429 throttles. A 403 is OpenRouter's moderation refusal of
//!   the request's content, not a verdict on the key, so it is left alone.

use async_trait::async_trait;
use chrono::Utc;
use http::header::{self, HeaderMap};
use http::StatusCode;

use super::feedback::{retry_after, ErrorBody};
use super::material::{ensure_for, plain_api_key, secret_value};
use super::usage::{ShapeObserver, UsageShape};
use crate::contract::credential::{
    CredentialKind, CredentialMaterial, MaterialDocument, UpstreamFeedback,
};
use crate::contract::upstream::{
    DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind, UsageObserver,
};

forwarding_allowlist! {
    static FORWARDED = ["http-referer", "x-title"];
}

static USAGE: UsageShape = UsageShape {
    usage: &[&["usage"]],
    model: &[&["model"]],
};

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenRouterUpstream;

#[async_trait]
impl UpstreamImpl for OpenRouterUpstream {
    fn kind(&self) -> UpstreamKind {
        UpstreamKind::OpenRouter
    }

    upstream_forwarding!(FORWARDED, target = base_url);

    fn apply_auth(
        &self,
        request: &mut http::request::Parts,
        material: &CredentialMaterial,
    ) -> Result<(), UpstreamError> {
        ensure_for(self.kind(), material)?;
        if material.kind != CredentialKind::ApiKey {
            return Err(UpstreamError::Auth(
                "OpenRouter credentials are API keys".into(),
            ));
        }
        request.headers.insert(
            header::AUTHORIZATION,
            secret_value("Bearer ", &material.secret)?,
        );
        Ok(())
    }

    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> {
        let secret = plain_api_key(doc).ok_or_else(|| {
            UpstreamError::InvalidMaterial("expected an OpenRouter API key".into())
        })??;
        Ok(DecodedMaterial {
            kind: CredentialKind::ApiKey,
            secret,
            account_id: None,
            expires_at: None,
        })
    }

    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> {
        let decoded = self.decode_material(doc)?;
        Err(UpstreamError::NotRefreshable(decoded.kind))
    }

    fn usage_observer(&self) -> Box<dyn UsageObserver> {
        Box::new(ShapeObserver::new(&USAGE))
    }

    fn classify(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Option<UpstreamFeedback> {
        let error = ErrorBody::parse(body);
        match status {
            StatusCode::UNAUTHORIZED if error.is("401") => {
                Some(UpstreamFeedback::CredentialInvalid {
                    reason: error.reason(status),
                })
            }
            StatusCode::PAYMENT_REQUIRED => Some(UpstreamFeedback::AccountError {
                reason: error.reason(status),
            }),
            StatusCode::TOO_MANY_REQUESTS => Some(UpstreamFeedback::RateLimited {
                until: retry_after(headers, Utc::now()),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::proxy::testkit::{self, Reply};

    #[tokio::test]
    async fn upstream_auth_applied_per_scheme_for_openrouter() {
        let key = testkit::material(
            UpstreamKind::OpenRouter,
            CredentialKind::ApiKey,
            "sk-or-v1-abc",
            None,
        );
        let trip = testkit::round_trip(
            Arc::new(OpenRouterUpstream),
            &key,
            "/v1/chat/completions",
            &[
                ("http-referer", "https://agent.example"),
                ("x-title", "agent"),
                ("x-api-key", "caller"),
            ],
            |_| Reply::json(200, json!({"id": "gen-1"})),
        )
        .await;
        let got = &trip.received.headers;
        assert_eq!(got["authorization"], "Bearer sk-or-v1-abc");
        assert_eq!(got["http-referer"], "https://agent.example");
        assert_eq!(got["x-title"], "agent");
        assert!(got.get("x-api-key").is_none());
        let mut parts = http::Request::new(()).into_parts().0;
        let oauth = testkit::material(UpstreamKind::OpenRouter, CredentialKind::Oauth, "t", None);
        assert!(matches!(
            OpenRouterUpstream.apply_auth(&mut parts, &oauth),
            Err(UpstreamError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn openrouter_material_is_a_plain_key_that_never_refreshes() {
        let decode = |v: Value| OpenRouterUpstream.decode_material(&MaterialDocument::new(v));
        assert_eq!(
            decode(json!("sk-or-v1-x\n")).unwrap().kind,
            CredentialKind::ApiKey
        );
        assert!(matches!(
            decode(json!({"tokens": {}})),
            Err(UpstreamError::InvalidMaterial(_))
        ));
        assert_eq!(
            OpenRouterUpstream
                .refresh(&MaterialDocument::new(json!("sk-or")))
                .await,
            Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey))
        );
    }

    #[test]
    fn upstream_rejection_detected_for_openrouter() {
        let classify = |status: u16, body: Value| {
            OpenRouterUpstream.classify(
                StatusCode::from_u16(status).unwrap(),
                &HeaderMap::new(),
                &serde_json::to_vec(&body).unwrap(),
            )
        };
        assert_eq!(
            classify(
                401,
                json!({"error": {"code": 401, "message": "User not found."}})
            ),
            Some(UpstreamFeedback::CredentialInvalid {
                reason: "401 Unauthorized: 401: User not found.".into()
            })
        );
        // An edge's page or a body that is not OpenRouter's error says
        // nothing about the key.
        assert_eq!(classify(401, json!({})), None);
        assert_eq!(
            OpenRouterUpstream.classify(StatusCode::UNAUTHORIZED, &HeaderMap::new(), b"<html>"),
            None
        );
        assert!(matches!(
            classify(
                402,
                json!({"error": {"code": 402, "message": "Insufficient credits"}})
            ),
            Some(UpstreamFeedback::AccountError { .. })
        ));
        assert_eq!(
            classify(
                403,
                json!({"error": {"code": 403, "message": "flagged", "metadata": {"reasons": ["x"]}}})
            ),
            None
        );
        assert_eq!(
            classify(429, json!({})),
            Some(UpstreamFeedback::RateLimited { until: None })
        );
        assert_eq!(classify(502, json!({})), None);
    }

    #[tokio::test]
    async fn usage_observation_extracted_from_openrouter_stream_with_cost() {
        let key = testkit::material(UpstreamKind::OpenRouter, CredentialKind::ApiKey, "sk", None);
        let stream = concat!(
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"id\":\"gen-1\",\"model\":\"anthropic/claude-sonnet-4\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"id\":\"gen-1\",\"model\":\"anthropic/claude-sonnet-4\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":3,\"total_tokens\":12,\"cost\":0.00042}}\n\n",
            "data: [DONE]\n\n",
        );
        let trip = testkit::round_trip(
            Arc::new(OpenRouterUpstream),
            &key,
            "/v1/chat/completions",
            &[],
            move |_| {
                Reply::stream(
                    200,
                    &[("content-type", "text/event-stream")],
                    futures::stream::iter(
                        stream
                            .split_inclusive("\n\n")
                            .map(|e| bytes::Bytes::from(e.to_owned()))
                            .collect::<Vec<_>>(),
                    ),
                )
            },
        )
        .await;
        assert_eq!(&trip.body[..], stream.as_bytes());
        assert_eq!(
            trip.outcome.usage,
            json!({"model": "anthropic/claude-sonnet-4", "usage": {"prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12, "cost": 0.00042}})
        );
    }
}
