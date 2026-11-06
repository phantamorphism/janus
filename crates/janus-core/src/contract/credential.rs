//! Credentials and the credential pool, as the rest of Janus sees them.
//!
//! [`CredentialMetadata`] is what the admin surface reports for a credential
//! (`docs/admin-surface#credentials-endpoint`) — metadata only, never
//! material. [`CredentialMaterial`] is what the pool hands the substitution
//! stage for one request (step 4 to step 5 of
//! `docs/architecture#request-lifecycle`); with [`Secret`] and
//! [`MaterialDocument`] it is the only kind of value here that holds a
//! secret, and none of them print it.
//!
//! [`CredentialPool`] is the one interface the data plane (resolve, report)
//! and the admin surface (list, show, peek, add, replace, enable, disable,
//! refresh, delete) code against; `janus_core::pool` implements it.
//! [`validate_credential_ref`] is the single `ref` grammar shared by config,
//! the pool and `POST /v1/credentials`.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::plugin::event_type;
use super::selection::{InvalidTag, Pick, Selection};
use super::upstream::UpstreamKind;

/// How auth is carried for the credential's upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A static API key (`x-api-key` / `Authorization: Bearer`, per upstream).
    ApiKey,
    /// An OAuth access token from a vendor CLI's credential file
    /// (`.credentials.json` for Anthropic, `auth.json` for OpenAI/Codex),
    /// refreshed when it expires.
    Oauth,
}

/// Where the credential came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// A pool slot directory under the data directory.
    Pool,
    /// An environment variable named in config (`[[credential]] env`).
    Env,
    /// A file named in config (`[[credential]] file`).
    File,
}

/// Whether the credential is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    /// The upstream rejected it; cleared by replacing the material.
    Invalid,
    /// Taken out of the default chain and plugin resolution by an operator.
    Disabled,
    /// Back-off until `rate_limited_until`.
    RateLimited,
}

/// One credential's metadata, as `GET /v1/credentials[/{ref}]` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMetadata {
    /// The pool slot's name; the same opaque reference a plugin returns in
    /// `credential_ref`.
    #[serde(rename = "ref")]
    pub credential_ref: String,
    pub upstream: UpstreamKind,
    pub kind: CredentialKind,
    pub label: Option<String>,
    pub source: CredentialSource,
    pub status: CredentialStatus,
    pub rate_limited_until: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub request_count: u64,
    pub error_count: u64,
    pub last_error: Option<String>,
    /// Selection tags (`docs/credential-substitution#credential-selection`);
    /// empty when the credential has none.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The `ref` that may never name a credential: `GET /v1/credentials/default`
/// is the default-chain peek, so a credential called `default` would be
/// unaddressable.
pub const RESERVED_CREDENTIAL_REF: &str = "default";

/// A `ref` that breaks the grammar of [`validate_credential_ref`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCredentialRef {
    #[error("a credential ref must not be empty")]
    Empty,
    #[error("a credential ref must not start with '.'")]
    LeadingDot,
    #[error("a credential ref uses only [A-Za-z0-9._-]; found {0:?}")]
    BadCharacter(char),
    #[error("\"default\" is reserved: GET /v1/credentials/default is the default-chain peek")]
    Reserved,
}

/// The `ref` grammar: non-empty, only `[A-Za-z0-9._-]`, no leading `.`, and
/// not [`RESERVED_CREDENTIAL_REF`]. A ref names a pool slot directory, so
/// this is also what keeps a ref from escaping the pool (`..`, `/`, hidden
/// files). Config (`[[credential]] ref`), the pool and
/// `POST /v1/credentials` all apply this one function.
pub fn validate_credential_ref(r: &str) -> Result<(), InvalidCredentialRef> {
    if r.is_empty() {
        return Err(InvalidCredentialRef::Empty);
    }
    if r.starts_with('.') {
        return Err(InvalidCredentialRef::LeadingDot);
    }
    if let Some(c) = r
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(InvalidCredentialRef::BadCharacter(c));
    }
    if r == RESERVED_CREDENTIAL_REF {
        return Err(InvalidCredentialRef::Reserved);
    }
    Ok(())
}

/// Secret bytes: an API key, an access token, or an `hmac` shared secret
/// read from a `[secrets.<name>]` file (which may be arbitrary binary).
///
/// `Debug` and `Display` never print it, equality is constant-time in the
/// contents (it short-circuits only on a length mismatch), and the bytes
/// are overwritten when the value is dropped.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Secret(bytes.into())
    }

    /// The secret bytes, for keying a MAC or applying them to a request.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The secret as text, for header-carried credentials. `None` when the
    /// bytes are not UTF-8.
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for Secret {}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            // Volatile so the overwrite of memory about to be freed is not
            // optimized away.
            // SAFETY: `byte` is a valid, aligned, exclusive reference.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Live, usable credential material for one outbound request: already
/// refreshed if it had expired. Deliberately not `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialMaterial {
    pub credential_ref: String,
    pub upstream: UpstreamKind,
    pub kind: CredentialKind,
    /// The API key or OAuth access token.
    pub secret: Secret,
    /// The account identifier some OAuth upstreams require alongside the
    /// token (the ChatGPT account id in a Codex `auth.json`).
    pub account_id: Option<String>,
}

/// The kind-specific stored form of a credential: the `material` object of
/// `POST /v1/credentials` and `PUT /v1/credentials/{ref}/material` (an API
/// key; the contents of a vendor CLI's credential file), and what a pool
/// slot holds. Only the upstream implementation interprets it
/// (`UpstreamImpl::decode_material`). Serializes transparently, because it
/// has to cross the admin API and reach the slot file; `Debug` is redacted.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MaterialDocument(serde_json::Value);

impl MaterialDocument {
    pub fn new(value: serde_json::Value) -> Self {
        MaterialDocument(value)
    }

    /// The document, for the upstream implementation that decodes it and
    /// the pool that writes it to its slot.
    pub fn expose(&self) -> &serde_json::Value {
        &self.0
    }
}

impl fmt::Debug for MaterialDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MaterialDocument(<redacted>)")
    }
}

/// The body of `POST /v1/credentials`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewCredential {
    pub upstream: UpstreamKind,
    /// Generated by the pool when absent.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Selection tags; see [`super::selection::validate_tag`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub material: MaterialDocument,
}

/// Which credential step 3 selected: the plugin's `credential_ref`, or the
/// default chain (`use_default`, no plugin configured, or a `none` route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialChoice {
    Ref(String),
    Default,
}

/// What an upstream's response said about the credential used for it —
/// `UpstreamImpl::classify`'s answer, reported back with
/// [`CredentialPool::report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamFeedback {
    /// The upstream rejected the credential. The pool marks it `invalid`
    /// (cleared only by replacing the material); the daemon emits
    /// `credential_invalid`.
    CredentialInvalid { reason: String },
    /// The upstream is throttling this credential. The pool marks it
    /// `rate_limited` until `until` (or a pool-chosen back-off when the
    /// upstream gave no time), and the default chain skips it meanwhile.
    RateLimited { until: Option<DateTime<Utc>> },
    /// The upstream reported a problem with the account behind the
    /// credential rather than with the request. The pool records it as the
    /// credential's `last_error`; the daemon emits `account_error`.
    AccountError { reason: String },
}

impl UpstreamFeedback {
    /// The `Notify` event this feedback produces, if any
    /// (`docs/plugin-contract#guaranteed-events`).
    pub fn event_type(&self) -> Option<&'static str> {
        match self {
            UpstreamFeedback::CredentialInvalid { .. } => Some(event_type::CREDENTIAL_INVALID),
            UpstreamFeedback::AccountError { .. } => Some(event_type::ACCOUNT_ERROR),
            UpstreamFeedback::RateLimited { .. } => None,
        }
    }
}

/// A pool operation that failed. On the request path every `resolve`
/// failure is the request-log outcome `credential_unavailable`: a
/// credential that cannot be made usable fails the request, never falls
/// through to a different one.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(transparent)]
    InvalidRef(#[from] InvalidCredentialRef),
    #[error("no credential {0:?}")]
    NotFound(String),
    #[error("credential {0:?} already exists")]
    AlreadyExists(String),
    #[error("credential {credential_ref:?} is for {actual}, not {expected}")]
    WrongUpstream {
        credential_ref: String,
        expected: UpstreamKind,
        actual: UpstreamKind,
    },
    /// Named explicitly but disabled, invalid or rate-limited.
    #[error("credential {credential_ref:?} is {status:?}")]
    Unavailable {
        credential_ref: String,
        status: CredentialStatus,
    },
    /// The default chain has no usable credential for this upstream.
    #[error("no usable default credential for {0}")]
    NoDefault(String),
    /// A named credential the instance's credential filter does not admit.
    #[error("credential {credential_ref:?} is not eligible for upstream {instance:?}")]
    NotEligible {
        credential_ref: String,
        instance: String,
    },
    #[error(transparent)]
    InvalidTag(#[from] InvalidTag),
    #[error("refreshing credential {credential_ref:?} failed: {reason}")]
    RefreshFailed {
        credential_ref: String,
        reason: String,
    },
    /// The material object is not one this upstream knows how to store.
    #[error("material is not usable for {upstream}: {reason}")]
    InvalidMaterial {
        upstream: UpstreamKind,
        reason: String,
    },
    /// An `env`/`file` credential from config: its material and existence
    /// are the config's, so the admin surface cannot replace or delete it.
    #[error("credential {credential_ref:?} comes from config ({origin:?}), not a pool slot")]
    NotPoolManaged {
        credential_ref: String,
        origin: CredentialSource,
    },
    #[error("credential pool I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The credential pool: directory-per-slot under the data directory, plus
/// the `[[credential]]` entries of config (`docs/scope#extractable`). Every
/// mutation is live without a restart.
#[async_trait]
pub trait CredentialPool: Send + Sync {
    /// Step 4 of the request lifecycle: resolve `choice` for the instance
    /// `selection` describes to live material, refreshing it first if it has
    /// expired. A `Ref` that is disabled, invalid, rate-limited, unknown, for
    /// another implementation or not admitted by the instance's filter is an
    /// error, never a fall-through to the default chain. `Default` picks
    /// among the usable credentials the filter admits, minus
    /// `pick.exclude`, by the selection's strategy. Counts as a use
    /// (`last_used_at`, `request_count`).
    async fn resolve(
        &self,
        selection: &Selection,
        choice: &CredentialChoice,
        pick: Pick<'_>,
    ) -> Result<CredentialMaterial, PoolError>;

    /// Record what the upstream's response said about `credential_ref`.
    async fn report(
        &self,
        credential_ref: &str,
        feedback: &UpstreamFeedback,
    ) -> Result<(), PoolError>;

    /// `GET /v1/credentials[?upstream=]`.
    async fn list(
        &self,
        upstream: Option<UpstreamKind>,
    ) -> Result<Vec<CredentialMetadata>, PoolError>;

    /// `GET /v1/credentials/{ref}`.
    async fn show(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError>;

    /// `GET /v1/credentials/default?upstream=`: what the default chain of
    /// `selection` would pick right now for a request from `identity`.
    /// Never touches the material, is not a use and does not advance a
    /// round-robin cursor.
    async fn peek_default(
        &self,
        selection: &Selection,
        identity: Option<&str>,
    ) -> Result<CredentialMetadata, PoolError>;

    /// `PUT /v1/credentials/{ref}/tags`: replace a pool slot's tags.
    async fn set_tags(
        &self,
        credential_ref: &str,
        tags: Vec<String>,
    ) -> Result<CredentialMetadata, PoolError>;

    /// `POST /v1/credentials`: write a new pool slot.
    async fn add(&self, new: NewCredential) -> Result<CredentialMetadata, PoolError>;

    /// `PUT /v1/credentials/{ref}/material`: replace the material and clear
    /// an `invalid` status.
    async fn replace_material(
        &self,
        credential_ref: &str,
        material: MaterialDocument,
    ) -> Result<CredentialMetadata, PoolError>;

    /// `POST /v1/credentials/{ref}/enable`.
    async fn enable(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError>;

    /// `POST /v1/credentials/{ref}/disable`: out of the default chain and
    /// out of plugin resolution.
    async fn disable(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError>;

    /// `POST /v1/credentials/{ref}/refresh`: refresh now rather than on next
    /// use.
    async fn refresh(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError>;

    /// `DELETE /v1/credentials/{ref}`: remove the slot.
    async fn delete(&self, credential_ref: &str) -> Result<(), PoolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_metadata_uses_admin_surface_field_names() {
        let meta = CredentialMetadata {
            credential_ref: "team-a".into(),
            upstream: UpstreamKind::Anthropic,
            kind: CredentialKind::Oauth,
            label: Some("Team A".into()),
            source: CredentialSource::Pool,
            status: CredentialStatus::RateLimited,
            rate_limited_until: None,
            expires_at: None,
            last_used_at: None,
            request_count: 3,
            error_count: 1,
            last_error: Some("429".into()),
            tags: vec!["team".into()],
        };
        let json = serde_json::to_value(&meta).expect("ser");
        assert_eq!(json["tags"], serde_json::json!(["team"]));
        assert_eq!(json["ref"], "team-a");
        assert_eq!(json["upstream"], "anthropic");
        assert_eq!(json["kind"], "oauth");
        assert_eq!(json["source"], "pool");
        assert_eq!(json["status"], "rate_limited");
        let back: CredentialMetadata = serde_json::from_value(json).expect("de");
        assert_eq!(back, meta);
    }

    #[test]
    fn secret_material_never_prints() {
        let m = CredentialMaterial {
            credential_ref: "k".into(),
            upstream: UpstreamKind::OpenRouter,
            kind: CredentialKind::ApiKey,
            secret: Secret::new("sk-live-123"),
            account_id: None,
        };
        let dbg = format!("{m:?}");
        assert!(!dbg.contains("sk-live-123"), "{dbg}");
        assert!(!m.secret.to_string().contains("sk-live"));
        assert_eq!(m.secret.expose_str(), Some("sk-live-123"));
        let doc = MaterialDocument::new(serde_json::json!({"api_key": "sk-live-123"}));
        assert!(!format!("{doc:?}").contains("sk-live"));
        let body = NewCredential {
            upstream: UpstreamKind::OpenRouter,
            credential_ref: None,
            label: None,
            tags: Vec::new(),
            material: doc,
        };
        assert!(!format!("{body:?}").contains("sk-live"));
        // The admin API body does carry the material, transparently.
        let json = serde_json::to_value(&body).expect("ser");
        assert_eq!(json["material"]["api_key"], "sk-live-123");
        assert!(json.get("ref").is_none());
    }

    #[test]
    fn secret_holds_binary_bytes_and_compares_by_content() {
        let raw = vec![0x00, 0xff, 0x80, 0x10];
        let s = Secret::new(raw.clone());
        assert_eq!(s.expose(), raw.as_slice());
        assert_eq!(s.expose_str(), None, "not UTF-8");
        assert_eq!(s.len(), 4);
        assert!(!s.is_empty());
        assert_eq!(s, Secret::new(raw.clone()));
        assert_ne!(s, Secret::new(vec![0x00, 0xff, 0x80, 0x11]));
        assert_ne!(s, Secret::new(vec![0x00, 0xff, 0x80]));
        assert_eq!(s.clone(), s);
    }

    #[test]
    fn credential_ref_grammar_refuses_traversal_and_the_reserved_name() {
        for ok in ["team-a", "openrouter.main", "K_1", "a..b"] {
            assert_eq!(validate_credential_ref(ok), Ok(()), "{ok}");
        }
        let cases = [
            ("", InvalidCredentialRef::Empty),
            ("..", InvalidCredentialRef::LeadingDot),
            (".hidden", InvalidCredentialRef::LeadingDot),
            ("../x", InvalidCredentialRef::LeadingDot),
            ("a/b", InvalidCredentialRef::BadCharacter('/')),
            ("a b", InvalidCredentialRef::BadCharacter(' ')),
            ("é", InvalidCredentialRef::BadCharacter('é')),
            ("default", InvalidCredentialRef::Reserved),
        ];
        for (bad, expected) in cases {
            assert_eq!(validate_credential_ref(bad), Err(expected), "{bad:?}");
        }
        assert_eq!(
            PoolError::from(InvalidCredentialRef::Reserved).to_string(),
            InvalidCredentialRef::Reserved.to_string()
        );
    }

    #[test]
    fn upstream_feedback_maps_to_the_documented_notify_events() {
        assert_eq!(
            UpstreamFeedback::CredentialInvalid {
                reason: "401".into()
            }
            .event_type(),
            Some("credential_invalid")
        );
        assert_eq!(
            UpstreamFeedback::AccountError {
                reason: "billing".into()
            }
            .event_type(),
            Some("account_error")
        );
        assert_eq!(
            UpstreamFeedback::RateLimited { until: None }.event_type(),
            None
        );
    }
}
