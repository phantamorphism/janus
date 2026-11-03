//! Anthropic (`api.anthropic.com`): the Messages API and its siblings.
//!
//! - Auth: an API key goes in `x-api-key`; a Claude subscription OAuth
//!   token (from Claude Code's `.credentials.json`) goes in
//!   `Authorization: Bearer`, with `oauth-2025-04-20` merged into the
//!   caller's `anthropic-beta` list, which the API requires of OAuth calls.
//! - Target: `base_url` + the inbound path, unchanged.
//! - Material: a plain API key, or `.credentials.json`
//!   (`{"claudeAiOauth": {"accessToken", "refreshToken", "expiresAt" (Unix
//!   milliseconds), ...}}`).
//! - Refresh: the OAuth refresh-token grant against the Claude token
//!   endpoint with Claude Code's client id; the new tokens are written back
//!   into the same document, every other field kept.
//! - Usage: `usage` of a message, or of `message_start` then
//!   `message_delta` in a stream.
//! - Feedback (the rule is in `feedback`): only a 401
//!   `authentication_error` that is not about an expired OAuth token
//!   rejects the credential; a 403 (`permission_error`: the key or the
//!   organisation may not do this request) is relayed without a verdict.
//!   402 or a `billing_error` (and the "credit balance is too low" refusal)
//!   blame the account; 429 throttles it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::StatusCode;
use serde_json::{json, Value};
use url::Url;

use super::feedback::{retry_after, ErrorBody};
use super::material::{ensure_for, plain_api_key, refresh_grant, secret_value, string_field};
use super::usage::{ShapeObserver, UsageShape};
use super::{http_client, ClientInitError};
use crate::contract::credential::{
    CredentialKind, CredentialMaterial, MaterialDocument, Secret, UpstreamFeedback,
};
use crate::contract::upstream::{
    DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind, UsageObserver,
};

/// Where a Claude subscription OAuth token is refreshed.
pub const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// The OAuth client Claude Code's tokens are issued to; a refresh must name
/// it.
const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// The beta an OAuth-authenticated call must declare.
const OAUTH_BETA: &str = "oauth-2025-04-20";

const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

forwarding_allowlist! {
    static FORWARDED = [
    "anthropic-version",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "x-app",
];
}

static USAGE: UsageShape = UsageShape {
    usage: &[&["usage"], &["message", "usage"]],
    model: &[&["model"], &["message", "model"]],
};

/// The `.credentials.json` object holding the OAuth tokens.
const OAUTH_DOC: &str = "claudeAiOauth";

#[derive(Debug, Clone)]
pub struct AnthropicUpstream {
    client: reqwest::Client,
    token_url: Url,
}

impl AnthropicUpstream {
    pub fn new() -> Result<Self, ClientInitError> {
        Ok(AnthropicUpstream {
            client: http_client()?,
            token_url: Url::parse(ANTHROPIC_TOKEN_URL).expect("static URL"),
        })
    }

    /// Refresh OAuth tokens against `token_url` instead of
    /// [`ANTHROPIC_TOKEN_URL`].
    pub fn with_token_url(mut self, token_url: Url) -> Self {
        self.token_url = token_url;
        self
    }
}

/// The caller's `anthropic-beta` values (across every header line), with
/// `oauth-2025-04-20` added unless already present.
fn beta_with_oauth(headers: &HeaderMap) -> Result<HeaderValue, UpstreamError> {
    let mut betas: Vec<&str> = headers
        .get_all(&ANTHROPIC_BETA)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    if !betas.contains(&OAUTH_BETA) {
        betas.push(OAUTH_BETA);
    }
    HeaderValue::from_str(&betas.join(","))
        .map_err(|_| UpstreamError::Auth("anthropic-beta is not a valid header value".into()))
}

fn oauth_section(doc: &Value) -> Option<&Value> {
    doc.get(OAUTH_DOC).filter(|v| v.is_object())
}

#[async_trait]
impl UpstreamImpl for AnthropicUpstream {
    fn kind(&self) -> UpstreamKind {
        UpstreamKind::Anthropic
    }

    upstream_forwarding!(FORWARDED, target = base_url);

    fn apply_auth(
        &self,
        request: &mut http::request::Parts,
        material: &CredentialMaterial,
    ) -> Result<(), UpstreamError> {
        ensure_for(self.kind(), material)?;
        match material.kind {
            CredentialKind::ApiKey => {
                request
                    .headers
                    .insert(X_API_KEY, secret_value("", &material.secret)?);
            }
            CredentialKind::Oauth => {
                request.headers.insert(
                    header::AUTHORIZATION,
                    secret_value("Bearer ", &material.secret)?,
                );
                let beta = beta_with_oauth(&request.headers)?;
                request.headers.insert(ANTHROPIC_BETA, beta);
            }
        }
        Ok(())
    }

    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> {
        if let Some(key) = plain_api_key(doc) {
            return Ok(DecodedMaterial {
                kind: CredentialKind::ApiKey,
                secret: key?,
                account_id: None,
                expires_at: None,
            });
        }
        let oauth = oauth_section(doc.expose()).ok_or_else(|| {
            UpstreamError::InvalidMaterial(
                "expected an Anthropic API key or a Claude Code .credentials.json (claudeAiOauth)"
                    .into(),
            )
        })?;
        let access = string_field(oauth, "accessToken", OAUTH_DOC)?;
        let expires_at = oauth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .and_then(DateTime::from_timestamp_millis);
        Ok(DecodedMaterial {
            kind: CredentialKind::Oauth,
            secret: Secret::new(access),
            account_id: None,
            expires_at,
        })
    }

    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> {
        let decoded = self.decode_material(doc)?;
        if decoded.kind == CredentialKind::ApiKey {
            return Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey));
        }
        let oauth = oauth_section(doc.expose())
            .ok_or_else(|| UpstreamError::InvalidMaterial(format!("no {OAUTH_DOC} object")))?;
        let refresh_token = string_field(oauth, "refreshToken", OAUTH_DOC)?;
        let answer = refresh_grant(
            &self.client,
            &self.token_url,
            &json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLAUDE_CODE_CLIENT_ID,
            }),
        )
        .await?;
        let failed = |e: UpstreamError| UpstreamError::RefreshFailed(e.to_string());
        let access =
            string_field(&answer, "access_token", "the token endpoint's answer").map_err(failed)?;
        let lifetime = answer
            .get("expires_in")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                UpstreamError::RefreshFailed("the token endpoint's answer has no expires_in".into())
            })?;
        let mut updated = doc.expose().clone();
        let section = &mut updated[OAUTH_DOC];
        section["accessToken"] = Value::String(access.to_owned());
        if let Some(rotated) = answer.get("refresh_token").and_then(Value::as_str) {
            section["refreshToken"] = Value::String(rotated.to_owned());
        }
        section["expiresAt"] =
            json!((Utc::now() + chrono::Duration::seconds(lifetime)).timestamp_millis());
        Ok(MaterialDocument::new(updated))
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
        let low_credit = error
            .message
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().contains("credit balance"));
        match status {
            StatusCode::UNAUTHORIZED
                if error.is("authentication_error") && !error.says_expired() =>
            {
                Some(UpstreamFeedback::CredentialInvalid {
                    reason: error.reason(status),
                })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => None,
            StatusCode::TOO_MANY_REQUESTS => Some(UpstreamFeedback::RateLimited {
                until: retry_after(headers, Utc::now()),
            }),
            StatusCode::PAYMENT_REQUIRED => Some(UpstreamFeedback::AccountError {
                reason: error.reason(status),
            }),
            _ if error.is("billing_error") || (status == StatusCode::BAD_REQUEST && low_credit) => {
                Some(UpstreamFeedback::AccountError {
                    reason: error.reason(status),
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::proxy::testkit::{self, Reply};

    fn upstream() -> AnthropicUpstream {
        AnthropicUpstream::new().unwrap()
    }

    #[tokio::test]
    async fn upstream_auth_applied_per_scheme_for_anthropic() {
        let reply = |_: &testkit::Recorded| Reply::json(200, json!({"ok": true}));
        // API key: x-api-key, nothing in authorization.
        let key = testkit::material(
            UpstreamKind::Anthropic,
            CredentialKind::ApiKey,
            "sk-ant-api-1",
            None,
        );
        let trip = testkit::round_trip(
            Arc::new(upstream()),
            &key,
            "/v1/messages",
            &[
                ("anthropic-version", "2023-06-01"),
                ("anthropic-beta", "tools-1"),
            ],
            reply,
        )
        .await;
        let got = &trip.received.headers;
        assert_eq!(trip.received.target, "/v1/messages");
        assert_eq!(got["x-api-key"], "sk-ant-api-1");
        assert!(got.get("authorization").is_none());
        assert_eq!(got["anthropic-version"], "2023-06-01");
        assert_eq!(got["anthropic-beta"], "tools-1");

        // OAuth: bearer token plus the OAuth beta, merged with the caller's.
        let oauth = testkit::material(
            UpstreamKind::Anthropic,
            CredentialKind::Oauth,
            "sk-ant-oat-2",
            None,
        );
        let trip = testkit::round_trip(
            Arc::new(upstream()),
            &oauth,
            "/v1/messages?beta=true",
            &[("anthropic-beta", "a-1, b-2"), ("anthropic-beta", "c-3")],
            reply,
        )
        .await;
        let got = &trip.received.headers;
        assert_eq!(trip.received.target, "/v1/messages?beta=true");
        assert_eq!(got["authorization"], "Bearer sk-ant-oat-2");
        assert!(got.get("x-api-key").is_none());
        assert_eq!(got["anthropic-beta"], "a-1,b-2,c-3,oauth-2025-04-20");
        assert_eq!(got.get_all("anthropic-beta").iter().count(), 1);
        assert!(!format!("{got:?}").contains("caller-identity-token"));

        // Already declared: not repeated.
        let mut parts = http::Request::new(()).into_parts().0;
        parts
            .headers
            .insert(ANTHROPIC_BETA, HeaderValue::from_static("oauth-2025-04-20"));
        upstream().apply_auth(&mut parts, &oauth).unwrap();
        assert_eq!(parts.headers["anthropic-beta"], "oauth-2025-04-20");
        assert!(parts.headers["authorization"].is_sensitive());
    }

    #[test]
    fn anthropic_refuses_material_for_another_upstream_or_unusable_bytes() {
        let mut parts = http::Request::new(()).into_parts().0;
        let other = testkit::material(
            UpstreamKind::OpenRouter,
            CredentialKind::ApiKey,
            "sk-or",
            None,
        );
        assert!(matches!(
            upstream().apply_auth(&mut parts, &other),
            Err(UpstreamError::Auth(_))
        ));
        let mut bad = testkit::material(UpstreamKind::Anthropic, CredentialKind::ApiKey, "x", None);
        bad.secret = Secret::new("line\nbreak");
        assert!(matches!(
            upstream().apply_auth(&mut parts, &bad),
            Err(UpstreamError::Auth(_))
        ));
        bad.secret = Secret::new(vec![0xff, 0xfe]);
        assert!(matches!(
            upstream().apply_auth(&mut parts, &bad),
            Err(UpstreamError::Auth(_))
        ));
        assert!(parts.headers.is_empty());
    }

    #[test]
    fn anthropic_material_documents_are_detected() {
        let decode = |v: Value| upstream().decode_material(&MaterialDocument::new(v));
        let key = decode(json!("sk-ant-api03-x")).unwrap();
        assert_eq!(key.kind, CredentialKind::ApiKey);
        assert_eq!(key.expires_at, None);
        let creds = decode(json!({"claudeAiOauth": {
            "accessToken": "sk-ant-oat01-a",
            "refreshToken": "sk-ant-ort01-r",
            "expiresAt": 1_900_000_000_000i64,
            "scopes": ["user:inference"],
            "subscriptionType": "max"
        }}))
        .unwrap();
        assert_eq!(creds.kind, CredentialKind::Oauth);
        assert_eq!(creds.secret, Secret::new("sk-ant-oat01-a"));
        assert_eq!(creds.expires_at, DateTime::from_timestamp(1_900_000_000, 0));
        for bad in [
            json!({"claudeAiOauth": {}}),
            json!({"tokens": {}}),
            json!(null),
        ] {
            assert!(matches!(
                decode(bad),
                Err(UpstreamError::InvalidMaterial(_))
            ));
        }
    }

    #[tokio::test]
    async fn anthropic_oauth_refresh_rewrites_the_document_against_the_token_endpoint() {
        let endpoint = testkit::spawn(|req| {
            let grant: Value = serde_json::from_slice(&req.body).unwrap();
            if grant["refresh_token"] == "sk-ant-ort01-old" {
                Reply::json(
                    200,
                    json!({"access_token": "sk-ant-oat01-new", "refresh_token": "sk-ant-ort01-new", "expires_in": 28800}),
                )
            } else {
                Reply::json(400, json!({"error": "invalid_grant", "error_description": "sk-ant-ort01-bad is revoked"}))
            }
        })
        .await;
        let upstream = upstream().with_token_url(endpoint.url.join("v1/oauth/token").unwrap());
        let doc = MaterialDocument::new(json!({"claudeAiOauth": {
            "accessToken": "sk-ant-oat01-old",
            "refreshToken": "sk-ant-ort01-old",
            "expiresAt": 1000,
            "subscriptionType": "max"
        }, "other": 1}));
        let before = Utc::now();
        let refreshed = upstream.refresh(&doc).await.unwrap();
        let v = refreshed.expose();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "sk-ant-oat01-new");
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "sk-ant-ort01-new");
        assert_eq!(v["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(v["other"], 1);
        let decoded = upstream.decode_material(&refreshed).unwrap();
        assert!(decoded.expires_at.unwrap() > before + chrono::Duration::hours(7));
        let sent = &endpoint.received()[0];
        assert_eq!(sent.target, "/v1/oauth/token");
        let grant: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(grant["grant_type"], "refresh_token");
        assert_eq!(grant["client_id"], CLAUDE_CODE_CLIENT_ID);

        let revoked = MaterialDocument::new(json!({"claudeAiOauth": {
            "accessToken": "a", "refreshToken": "sk-ant-ort01-bad", "expiresAt": 1000
        }}));
        let err = upstream.refresh(&revoked).await.unwrap_err();
        assert_eq!(
            err,
            UpstreamError::RefreshFailed(
                "token endpoint answered 400 Bad Request: invalid_grant".into()
            )
        );
        assert!(!err.to_string().contains("sk-ant-ort01-bad"));
        assert_eq!(
            upstream
                .refresh(&MaterialDocument::new(json!("sk-ant-api")))
                .await,
            Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey))
        );
    }

    #[test]
    fn upstream_rejection_detected_for_anthropic() {
        let classify = |status: u16, body: Value| {
            upstream().classify(
                StatusCode::from_u16(status).unwrap(),
                &HeaderMap::new(),
                &serde_json::to_vec(&body).unwrap(),
            )
        };
        let err = |kind: &str, message: &str| json!({"type": "error", "error": {"type": kind, "message": message}});
        assert_eq!(
            classify(401, err("authentication_error", "invalid x-api-key")),
            Some(UpstreamFeedback::CredentialInvalid {
                reason: "401 Unauthorized: authentication_error: invalid x-api-key".into()
            })
        );
        assert!(matches!(
            classify(
                401,
                err("authentication_error", "OAuth token has been revoked.")
            ),
            Some(UpstreamFeedback::CredentialInvalid { .. })
        ));
        // No verdict on what the request (not the credential) provoked, nor
        // on a 401 that is not Anthropic's authentication error.
        for (status, body) in [
            (
                403,
                err(
                    "permission_error",
                    "Your API key does not have permission to use the specified resource.",
                ),
            ),
            (403, json!({})),
            (401, json!({})),
            (401, err("invalid_request_error", "x")),
            (
                401,
                err(
                    "authentication_error",
                    "OAuth token has expired. Please obtain a new token or refresh your existing token.",
                ),
            ),
        ] {
            assert_eq!(classify(status, body.clone()), None, "{status} {body}");
        }
        assert_eq!(
            upstream().classify(
                StatusCode::FORBIDDEN,
                &HeaderMap::new(),
                b"<html><title>Just a moment...</title></html>"
            ),
            None
        );
        assert!(matches!(
            classify(
                400,
                err(
                    "invalid_request_error",
                    "Your credit balance is too low to access the Anthropic API."
                )
            ),
            Some(UpstreamFeedback::AccountError { .. })
        ));
        assert!(matches!(
            classify(402, err("billing_error", "payment failed")),
            Some(UpstreamFeedback::AccountError { .. })
        ));
        assert_eq!(
            classify(400, err("invalid_request_error", "max_tokens: too large")),
            None
        );
        assert_eq!(classify(529, err("overloaded_error", "Overloaded")), None);
        assert_eq!(classify(200, json!({"usage": {}})), None);
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "20".parse().unwrap());
        let before = Utc::now();
        match upstream().classify(StatusCode::TOO_MANY_REQUESTS, &headers, b"") {
            Some(UpstreamFeedback::RateLimited { until: Some(until) }) => {
                assert!(until >= before + chrono::Duration::seconds(20));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn usage_observation_extracted_from_anthropic_stream_and_message() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-1\",\"usage\":{\"input_tokens\":25,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let key = testkit::material(UpstreamKind::Anthropic, CredentialKind::ApiKey, "sk", None);
        let trip =
            testkit::round_trip(Arc::new(upstream()), &key, "/v1/messages", &[], move |_| {
                Reply::stream(
                    200,
                    &[("content-type", "text/event-stream")],
                    futures::stream::iter(
                        stream
                            .as_bytes()
                            .chunks(7)
                            .map(bytes::Bytes::copy_from_slice)
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .await;
        assert_eq!(trip.status, StatusCode::OK);
        assert_eq!(&trip.body[..], stream.as_bytes(), "relayed byte for byte");
        assert_eq!(
            trip.outcome.usage,
            json!({"model": "claude-opus-4-1", "usage": {
                "input_tokens": 25, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 10, "output_tokens": 15
            }})
        );

        let trip = testkit::round_trip(Arc::new(upstream()), &key, "/v1/messages", &[], |_| {
            Reply::json(
                200,
                json!({
                    "id": "msg_2", "type": "message", "model": "claude-haiku",
                    "content": [{"type": "text", "text": "the word usage"}],
                    "usage": {"input_tokens": 3, "output_tokens": 4}
                }),
            )
        })
        .await;
        assert_eq!(
            trip.outcome.usage,
            json!({"model": "claude-haiku", "usage": {"input_tokens": 3, "output_tokens": 4}})
        );
    }
}
