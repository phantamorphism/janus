//! OpenAI: the platform API (`api.openai.com`) for API keys, and the
//! ChatGPT Codex backend for ChatGPT-account OAuth tokens.
//!
//! - Auth: `Authorization: Bearer` either way. An OAuth token (from the
//!   Codex CLI's `auth.json`) also needs its account: `chatgpt-account-id`.
//! - Target, chosen by the credential's kind: an API key goes to
//!   `base_url` + the inbound path. A Codex OAuth token is only valid at the
//!   ChatGPT backend, whose Codex API is the platform API's `/v1` surface
//!   unversioned (`/v1/responses` -> `<codex base>/responses`), so an OAuth
//!   request is targeted there ([`CODEX_BASE_URL`] unless configured with
//!   [`OpenAiUpstream::with_codex_base_url`]); a path outside that surface
//!   is refused.
//! - Material: a plain API key, or `auth.json` (`{"auth_mode",
//!   "OPENAI_API_KEY", "tokens": {"id_token", "access_token",
//!   "refresh_token", "account_id"}, "last_refresh"}`): `auth_mode =
//!   "apikey"` selects the key, otherwise the tokens when present, otherwise
//!   the key. The access token's JWT `exp` is its expiry.
//! - Refresh: the OAuth refresh-token grant against the OpenAI auth server
//!   with the Codex CLI's client id; tokens and `last_refresh` written back
//!   into the same document, every other field kept.
//! - Usage: `usage` of a Chat Completions or Responses body or stream chunk,
//!   or `response.usage` of a Responses stream's `response.completed`.
//! - Feedback (the rule is in `feedback`): only a 401 whose error code is
//!   [`INVALID_CREDENTIAL_CODES`] rejects the credential. A 403 (a
//!   restricted key's missing scopes, an unsupported region, a ChatGPT
//!   edge challenge) and any other 401 (the platform's "insufficient
//!   permissions ... missing scopes" carries no code) are relayed without
//!   a verdict. 429 `insufficient_quota` blames the account; any other 429
//!   throttles (a Codex `usage_limit_reached` until its reset time).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::header::{self, HeaderMap, HeaderName};
use http::StatusCode;
use serde_json::{json, Value};
use url::Url;

use super::feedback::{retry_after, ErrorBody};
use super::material::{
    ensure_for, jwt_expiry, plain_api_key, refresh_grant, secret_value, string_field,
};
use super::outbound::join_base;
use super::usage::{ShapeObserver, UsageShape};
use super::{http_client, ClientInitError};
use crate::contract::credential::{
    CredentialKind, CredentialMaterial, MaterialDocument, Secret, UpstreamFeedback,
};
use crate::contract::upstream::{
    DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind, UsageObserver,
};

/// Where Codex OAuth traffic goes.
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Where a Codex OAuth token is refreshed.
pub const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The OAuth client Codex CLI tokens are issued to.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The 401 error codes that say the key or token itself is not valid: a
/// wrong or revoked key, a key of a deactivated account.
const INVALID_CREDENTIAL_CODES: [&str; 2] = ["invalid_api_key", "account_deactivated"];

const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");

forwarding_allowlist! {
    static FORWARDED = [
    "openai-beta",
    "originator",
    "x-codex-installation-id",
    "x-codex-window-id",
    "x-codex-parent-thread-id",
    "x-openai-internal-codex-residency",
];
}

static USAGE: UsageShape = UsageShape {
    usage: &[&["usage"], &["response", "usage"]],
    model: &[&["model"], &["response", "model"]],
};

#[derive(Debug, Clone)]
pub struct OpenAiUpstream {
    client: reqwest::Client,
    token_url: Url,
    codex_base_url: Url,
}

impl OpenAiUpstream {
    pub fn new() -> Result<Self, ClientInitError> {
        Ok(OpenAiUpstream {
            client: http_client()?,
            token_url: Url::parse(OPENAI_TOKEN_URL).expect("static URL"),
            codex_base_url: Url::parse(CODEX_BASE_URL).expect("static URL"),
        })
    }

    /// Refresh OAuth tokens against `token_url` instead of
    /// [`OPENAI_TOKEN_URL`].
    pub fn with_token_url(mut self, token_url: Url) -> Self {
        self.token_url = token_url;
        self
    }

    /// Send Codex OAuth traffic to `codex_base_url` instead of
    /// [`CODEX_BASE_URL`].
    pub fn with_codex_base_url(mut self, codex_base_url: Url) -> Self {
        self.codex_base_url = codex_base_url;
        self
    }

    /// The Codex-backend equivalent of a platform-API target: the part of
    /// the path after the `/v1` segment, on the Codex base. That segment is
    /// either the last of `base_url`'s path (`https://proxy.corp/openai/v1`)
    /// or the first of `rest`; a `/v1` anywhere else (`/dashboard/v1/x`,
    /// `/v1x/v1/...`) is not the platform surface, and is refused.
    fn codex_target(&self, base_url: &Url, rest: &str) -> Result<Url, UpstreamError> {
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };
        let base_is_v1 = base_url.path().trim_end_matches('/').rsplit('/').next() == Some("v1");
        let after = if base_is_v1 {
            path
        } else if path == "/v1" {
            ""
        } else if path.starts_with("/v1/") {
            &path["/v1".len()..]
        } else {
            return Err(UpstreamError::BadPath(rest.to_owned()));
        };
        let rest = match query {
            Some(query) => format!("{after}?{query}"),
            None => after.to_owned(),
        };
        join_base(&self.codex_base_url, &rest)
    }
}

/// Which credential an `auth.json` (or plain key) document holds.
enum AuthJson<'d> {
    ApiKey(Secret),
    Tokens(&'d Value),
}

fn read_auth_json(doc: &MaterialDocument) -> Result<AuthJson<'_>, UpstreamError> {
    if let Some(key) = plain_api_key(doc) {
        return Ok(AuthJson::ApiKey(key?));
    }
    let doc = doc.expose();
    let api_key = doc
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let tokens = doc.get("tokens").filter(|t| t.is_object());
    let wants_key = doc.get("auth_mode").and_then(Value::as_str) == Some("apikey");
    match (wants_key, tokens, api_key) {
        (true, _, Some(key)) | (false, None, Some(key)) => Ok(AuthJson::ApiKey(Secret::new(key))),
        (false, Some(tokens), _) => Ok(AuthJson::Tokens(tokens)),
        (true, _, None) => Err(UpstreamError::InvalidMaterial(
            "auth.json has auth_mode \"apikey\" but no OPENAI_API_KEY".into(),
        )),
        (false, None, None) => Err(UpstreamError::InvalidMaterial(
            "expected an OpenAI API key or a Codex auth.json (tokens or OPENAI_API_KEY)".into(),
        )),
    }
}

#[async_trait]
impl UpstreamImpl for OpenAiUpstream {
    fn kind(&self) -> UpstreamKind {
        UpstreamKind::OpenAi
    }

    upstream_forwarding!(FORWARDED);

    fn outbound_url(
        &self,
        base_url: &Url,
        rest: &str,
        kind: CredentialKind,
    ) -> Result<Url, UpstreamError> {
        match kind {
            CredentialKind::ApiKey => join_base(base_url, rest),
            CredentialKind::Oauth => self.codex_target(base_url, rest),
        }
    }

    fn apply_auth(
        &self,
        request: &mut http::request::Parts,
        material: &CredentialMaterial,
    ) -> Result<(), UpstreamError> {
        ensure_for(self.kind(), material)?;
        let bearer = secret_value("Bearer ", &material.secret)?;
        if material.kind == CredentialKind::Oauth {
            let account = material.account_id.as_deref().ok_or_else(|| {
                UpstreamError::Auth("a Codex OAuth credential needs its ChatGPT account id".into())
            })?;
            let account = account.parse().map_err(|_| {
                UpstreamError::Auth("the ChatGPT account id is not a valid header value".into())
            })?;
            request.headers.insert(CHATGPT_ACCOUNT_ID, account);
        }
        request.headers.insert(header::AUTHORIZATION, bearer);
        Ok(())
    }

    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> {
        Ok(match read_auth_json(doc)? {
            AuthJson::ApiKey(secret) => DecodedMaterial {
                kind: CredentialKind::ApiKey,
                secret,
                account_id: None,
                expires_at: None,
            },
            AuthJson::Tokens(tokens) => {
                let access = string_field(tokens, "access_token", "auth.json tokens")?;
                DecodedMaterial {
                    kind: CredentialKind::Oauth,
                    secret: Secret::new(access),
                    account_id: tokens
                        .get("account_id")
                        .and_then(Value::as_str)
                        .filter(|a| !a.is_empty())
                        .map(str::to_owned),
                    expires_at: jwt_expiry(access),
                }
            }
        })
    }

    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> {
        let tokens = match read_auth_json(doc)? {
            AuthJson::ApiKey(_) => {
                return Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey))
            }
            AuthJson::Tokens(tokens) => tokens,
        };
        let refresh_token = string_field(tokens, "refresh_token", "auth.json tokens")?;
        let answer = refresh_grant(
            &self.client,
            &self.token_url,
            &json!({
                "grant_type": "refresh_token",
                "client_id": CODEX_CLIENT_ID,
                "refresh_token": refresh_token,
                "scope": "openid profile email",
            }),
        )
        .await?;
        let access = string_field(&answer, "access_token", "the token endpoint's answer")
            .map_err(|e| UpstreamError::RefreshFailed(e.to_string()))?;
        let mut updated = doc.expose().clone();
        let section = &mut updated["tokens"];
        section["access_token"] = Value::String(access.to_owned());
        for field in ["id_token", "refresh_token"] {
            if let Some(rotated) = answer.get(field).and_then(Value::as_str) {
                section[field] = Value::String(rotated.to_owned());
            }
        }
        updated["last_refresh"] = Value::String(Utc::now().to_rfc3339());
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
        match status {
            StatusCode::UNAUTHORIZED if INVALID_CREDENTIAL_CODES.iter().any(|c| error.is(c)) => {
                Some(UpstreamFeedback::CredentialInvalid {
                    reason: error.reason(status),
                })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => None,
            StatusCode::TOO_MANY_REQUESTS if error.is("insufficient_quota") => {
                Some(UpstreamFeedback::AccountError {
                    reason: error.reason(status),
                })
            }
            StatusCode::TOO_MANY_REQUESTS => Some(UpstreamFeedback::RateLimited {
                until: codex_reset(&error.raw, Utc::now())
                    .or_else(|| retry_after(headers, Utc::now())),
            }),
            _ => None,
        }
    }
}

/// When a Codex `usage_limit_reached` error says the plan's limit resets:
/// `resets_at` (Unix seconds) or `resets_in_seconds`.
fn codex_reset(error: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(at) = error.get("resets_at").and_then(Value::as_i64) {
        return DateTime::from_timestamp(at, 0);
    }
    let seconds = error.get("resets_in_seconds").and_then(Value::as_i64)?;
    Some(now + chrono::Duration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::proxy::testkit::{self, jwt, Reply};

    #[tokio::test]
    async fn upstream_auth_applied_per_scheme_for_openai_including_codex_routing() {
        let reply = |_: &testkit::Recorded| Reply::json(200, json!({"id": "resp_1"}));
        let key = testkit::material(
            UpstreamKind::OpenAi,
            CredentialKind::ApiKey,
            "sk-proj-1",
            None,
        );
        let trip = testkit::round_trip(
            Arc::new(OpenAiUpstream::new().unwrap()),
            &key,
            "/v1/responses",
            &[
                ("openai-beta", "responses=v1"),
                ("openai-organization", "org-caller"),
            ],
            reply,
        )
        .await;
        let got = &trip.received.headers;
        assert_eq!(trip.received.target, "/v1/responses");
        assert_eq!(got["authorization"], "Bearer sk-proj-1");
        assert_eq!(got["openai-beta"], "responses=v1");
        assert!(got.get("chatgpt-account-id").is_none());
        assert!(
            got.get("openai-organization").is_none(),
            "org is the credential's"
        );

        // OAuth: targeted at the Codex backend, not the API-key base.
        let codex = testkit::spawn(reply).await;
        let codex_base = codex.url.join("backend-api/codex").unwrap();
        let oauth = testkit::material(
            UpstreamKind::OpenAi,
            CredentialKind::Oauth,
            "eyJ-access",
            Some("acct-123"),
        );
        let upstream = OpenAiUpstream::new()
            .unwrap()
            .with_codex_base_url(codex_base);
        let unused_platform = testkit::spawn(reply).await;
        let head = crate::contract::identity::RequestHead::new(
            http::Method::POST,
            "/openai/v1/responses?stream=true".parse().unwrap(),
            [
                (header::AUTHORIZATION, "Bearer caller".parse().unwrap()),
                (
                    HeaderName::from_static("originator"),
                    "codex_cli_rs".parse().unwrap(),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let parts = crate::proxy::build_outbound(
            &head,
            crate::proxy::Substitution {
                upstream: &upstream,
                base_url: &unused_platform.url,
                rest: "/v1/responses?stream=true",
                material: &oauth,
                presented_at: Some(&crate::contract::identity::PresentedAt::Header(
                    header::AUTHORIZATION,
                )),
            },
        )
        .unwrap();
        let relayed = crate::proxy::Forwarder::new()
            .unwrap()
            .forward(
                Arc::new(upstream),
                parts,
                http_body_util::Full::new(bytes::Bytes::from_static(b"{}")),
            )
            .await
            .unwrap();
        assert_eq!(relayed.response.status(), 200);
        assert!(unused_platform.received().is_empty());
        let got = &codex.received()[0];
        assert_eq!(got.target, "/backend-api/codex/responses?stream=true");
        assert_eq!(got.headers["authorization"], "Bearer eyJ-access");
        assert_eq!(got.headers["chatgpt-account-id"], "acct-123");
        assert_eq!(got.headers["originator"], "codex_cli_rs");
    }

    #[test]
    fn codex_routing_maps_the_v1_surface_and_refuses_anything_else() {
        let upstream = OpenAiUpstream::new().unwrap();
        let target = |base: &str, rest: &str, kind| {
            upstream
                .outbound_url(&Url::parse(base).unwrap(), rest, kind)
                .map(|u| u.to_string())
        };
        let platform = "https://api.openai.com";
        assert_eq!(
            target(platform, "/v1/responses", CredentialKind::ApiKey).unwrap(),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            target(platform, "/v1/responses", CredentialKind::Oauth).unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // A base_url that already ends in /v1 maps the same way.
        assert_eq!(
            target(
                "https://proxy.corp/openai/v1",
                "/models?client_version=1",
                CredentialKind::Oauth
            )
            .unwrap(),
            "https://chatgpt.com/backend-api/codex/models?client_version=1"
        );
        assert_eq!(
            target(platform, "/v1", CredentialKind::Oauth).unwrap(),
            "https://chatgpt.com/backend-api/codex"
        );
        // `/v1` must be the surface's own segment, not one found anywhere.
        for outside in [
            "/dashboard/x",
            "/dashboard/v1/x",
            "/v1x/v1/responses",
            "/v1x",
            "/x/v1",
        ] {
            assert!(
                matches!(
                    target(platform, outside, CredentialKind::Oauth),
                    Err(UpstreamError::BadPath(_))
                ),
                "{outside}"
            );
        }
        // A base whose path merely ends in the letters `v1` is not `/v1`.
        assert!(matches!(
            target("https://proxy.corp/apiv1", "/models", CredentialKind::Oauth),
            Err(UpstreamError::BadPath(_))
        ));
        // The Codex target gets the same traversal checks as any other.
        assert!(matches!(
            target(platform, "/v1/../../x", CredentialKind::Oauth),
            Err(UpstreamError::BadPath(_))
        ));
        assert!(target(platform, "/dashboard/x", CredentialKind::ApiKey).is_ok());

        // Auth applies material only; the target is outbound_url's alone.
        let mut parts = http::Request::new(()).into_parts().0;
        parts.uri = "https://api.openai.com/v1/responses".parse().unwrap();
        let oauth = testkit::material(
            UpstreamKind::OpenAi,
            CredentialKind::Oauth,
            "t",
            Some("acct"),
        );
        upstream.apply_auth(&mut parts, &oauth).unwrap();
        assert_eq!(parts.uri, "https://api.openai.com/v1/responses");
        assert_eq!(parts.headers["chatgpt-account-id"], "acct");

        let mut parts = http::Request::new(()).into_parts().0;
        let no_account = testkit::material(UpstreamKind::OpenAi, CredentialKind::Oauth, "t", None);
        assert!(matches!(
            upstream.apply_auth(&mut parts, &no_account),
            Err(UpstreamError::Auth(_))
        ));
        assert!(parts.headers.get("authorization").is_none());
    }

    #[test]
    fn openai_material_documents_are_detected() {
        let upstream = OpenAiUpstream::new().unwrap();
        let decode = |v: Value| upstream.decode_material(&MaterialDocument::new(v));
        let access = jwt(json!({"exp": 1_900_000_000, "sub": "u"}));
        let auth_json = json!({
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": "id", "access_token": access, "refresh_token": "rt", "account_id": "acct-9"},
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        let oauth = decode(auth_json.clone()).unwrap();
        assert_eq!(oauth.kind, CredentialKind::Oauth);
        assert_eq!(oauth.account_id.as_deref(), Some("acct-9"));
        assert_eq!(oauth.expires_at, DateTime::from_timestamp(1_900_000_000, 0));

        let mut apikey_mode = auth_json.clone();
        apikey_mode["auth_mode"] = json!("apikey");
        apikey_mode["OPENAI_API_KEY"] = json!("sk-from-auth-json");
        let key = decode(apikey_mode).unwrap();
        assert_eq!(key.kind, CredentialKind::ApiKey);
        assert_eq!(key.secret, Secret::new("sk-from-auth-json"));

        let key_only = decode(json!({"OPENAI_API_KEY": "sk-2", "tokens": null})).unwrap();
        assert_eq!(key_only.kind, CredentialKind::ApiKey);
        assert_eq!(
            decode(json!({"api_key": "sk-3"})).unwrap().kind,
            CredentialKind::ApiKey
        );
        for bad in [
            json!({"auth_mode": "apikey", "tokens": {"access_token": "a"}}),
            json!({"tokens": {"refresh_token": "r"}}),
            json!({"claudeAiOauth": {}}),
        ] {
            assert!(matches!(
                decode(bad),
                Err(UpstreamError::InvalidMaterial(_))
            ));
        }
    }

    #[tokio::test]
    async fn openai_oauth_refresh_rewrites_auth_json_against_the_token_endpoint() {
        let fresh = jwt(json!({"exp": 1_950_000_000}));
        let answer = fresh.clone();
        let endpoint = testkit::spawn(move |req| {
            let grant: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(grant["client_id"], CODEX_CLIENT_ID);
            assert_eq!(grant["refresh_token"], "rt-old");
            Reply::json(
                200,
                json!({"access_token": answer, "id_token": "id-new", "refresh_token": "rt-new"}),
            )
        })
        .await;
        let upstream = OpenAiUpstream::new()
            .unwrap()
            .with_token_url(endpoint.url.join("oauth/token").unwrap());
        let doc = MaterialDocument::new(json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": "id-old", "access_token": "a-old", "refresh_token": "rt-old", "account_id": "acct"},
            "last_refresh": "2020-01-01T00:00:00Z"
        }));
        let refreshed = upstream.refresh(&doc).await.unwrap();
        let v = refreshed.expose();
        assert_eq!(v["tokens"]["access_token"], fresh.as_str());
        assert_eq!(v["tokens"]["id_token"], "id-new");
        assert_eq!(v["tokens"]["refresh_token"], "rt-new");
        assert_eq!(v["tokens"]["account_id"], "acct");
        assert_eq!(v["auth_mode"], "chatgpt");
        assert_ne!(v["last_refresh"], "2020-01-01T00:00:00Z");
        let decoded = upstream.decode_material(&refreshed).unwrap();
        assert_eq!(
            decoded.expires_at,
            DateTime::from_timestamp(1_950_000_000, 0)
        );
        assert_eq!(endpoint.received()[0].target, "/oauth/token");
        assert_eq!(
            upstream
                .refresh(&MaterialDocument::new(json!("sk-key")))
                .await,
            Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey))
        );
    }

    #[test]
    fn upstream_rejection_detected_for_openai() {
        let upstream = OpenAiUpstream::new().unwrap();
        let classify = |status: u16, body: Value| {
            upstream.classify(
                StatusCode::from_u16(status).unwrap(),
                &HeaderMap::new(),
                &serde_json::to_vec(&body).unwrap(),
            )
        };
        assert_eq!(
            classify(401, json!({"error": {"message": "Incorrect API key provided: sk-ab***yz.", "type": "invalid_request_error", "code": "invalid_api_key"}})),
            Some(UpstreamFeedback::CredentialInvalid {
                reason: "401 Unauthorized: invalid_request_error: Incorrect API key provided: sk-ab***yz.".into()
            })
        );
        assert!(matches!(
            classify(
                401,
                json!({"error": {"message": "This key is associated with a deactivated account.", "type": "invalid_request_error", "code": "account_deactivated"}})
            ),
            Some(UpstreamFeedback::CredentialInvalid { .. })
        ));
        // A caller can provoke these with a request the key may not make;
        // none of them may bench the shared credential.
        for (status, body) in [
            (
                403,
                json!({"error": {"message": "You have insufficient permissions for this operation. Missing scopes: api.files.read.", "type": "invalid_request_error", "code": null}}),
            ),
            (
                401,
                json!({"error": {"message": "You have insufficient permissions for this operation. Missing scopes: api.model.read.", "type": "invalid_request_error", "param": null, "code": null}}),
            ),
            (
                403,
                json!({"error": {"code": "unsupported_country_region_territory", "message": "Country, region, or territory not supported"}}),
            ),
            (403, json!({})),
            (401, json!({})),
        ] {
            assert_eq!(classify(status, body.clone()), None, "{status} {body}");
        }
        assert_eq!(
            upstream.classify(
                StatusCode::FORBIDDEN,
                &HeaderMap::new(),
                b"<html><title>Just a moment...</title></html>"
            ),
            None
        );
        assert!(matches!(
            classify(
                429,
                json!({"error": {"message": "You exceeded your current quota", "type": "insufficient_quota", "code": "insufficient_quota"}})
            ),
            Some(UpstreamFeedback::AccountError { .. })
        ));
        assert_eq!(
            classify(
                429,
                json!({"error": {"type": "usage_limit_reached", "resets_at": 1_900_000_000}})
            ),
            Some(UpstreamFeedback::RateLimited {
                until: DateTime::from_timestamp(1_900_000_000, 0)
            })
        );
        assert_eq!(
            classify(429, json!({"error": {"type": "requests"}})),
            Some(UpstreamFeedback::RateLimited { until: None })
        );
        assert_eq!(
            classify(400, json!({"error": {"type": "invalid_request_error"}})),
            None
        );
        assert_eq!(classify(500, json!({})), None);
    }

    #[tokio::test]
    async fn usage_observation_extracted_from_openai_responses_and_chat_streams() {
        let key = testkit::material(UpstreamKind::OpenAi, CredentialKind::ApiKey, "sk", None);
        let responses = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":null}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":2},\"total_tokens\":18}}}\n\n",
        );
        let trip = testkit::round_trip(
            Arc::new(OpenAiUpstream::new().unwrap()),
            &key,
            "/v1/responses",
            &[],
            move |_| {
                Reply::bytes(
                    200,
                    &[("content-type", "text/event-stream")],
                    responses.as_bytes().to_vec(),
                )
            },
        )
        .await;
        assert_eq!(&trip.body[..], responses.as_bytes());
        assert_eq!(
            trip.outcome.usage,
            json!({"model": "gpt-5", "usage": {"input_tokens": 11, "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 2}, "total_tokens": 18}})
        );

        let chat = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"x\"}}],\"usage\":null}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let trip = testkit::round_trip(
            Arc::new(OpenAiUpstream::new().unwrap()),
            &key,
            "/v1/chat/completions",
            &[],
            move |_| {
                Reply::bytes(
                    200,
                    &[("content-type", "text/event-stream")],
                    chat.as_bytes().to_vec(),
                )
            },
        )
        .await;
        assert_eq!(
            trip.outcome.usage,
            json!({"model": "gpt-4o", "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}})
        );
    }
}
