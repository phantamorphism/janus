//! The identity-extraction stage's interface (`docs/caller-identity#extractor`).
//!
//! An extractor takes the head of an inbound request and produces either a
//! verified identity or a rejection. It runs before any policy call, never
//! sees the body, and must not have side effects.
//!
//! # Constructing extractors
//!
//! Every binding is turned into a constructed extractor at startup, before
//! the data-plane listener binds; any failure is a startup failure
//! (`docs/caller-identity#binding`). The division of labor, per resolved
//! [`BindingSpec`](super::binding::BindingSpec):
//!
//! - `None` — no extractor; the route is open.
//! - `Custom(c)` — the extractor the custom build registered as `c.name`
//!   (`Embedded::extractors` in `janus-daemon`); not constructed here.
//! - `Hmac`, `Jwt`, `Opaque`, `TrustedHeader` — the daemon resolves the
//!   material the spec only names and hands an [`ExtractorInput`] to an
//!   [`ExtractorFactory`], which `janus_core::identity` implements:
//!   - `hmac`: `secret_ref` resolved through `[secrets.<name>]` to the
//!     shared secret's bytes;
//!   - `jwt`: `public_key_file` read to its PEM bytes, or the JWKS URL,
//!     which the factory fetches (unreachable is a startup failure) and the
//!     extractor keeps current;
//!   - `opaque`: the daemon store's [`OpaqueRegistry`] index;
//!   - `trusted_header`: nothing beyond the spec.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, Method, Uri};
use serde::{Deserialize, Serialize};
use url::Url;

use super::binding::{HmacSpec, JwtSpec, OpaqueSpec, TrustedHeaderSpec};
use super::credential::Secret;
use super::registry::OpaqueRegistry;

/// Method, URI (path and query) and headers of an inbound request — never the
/// body. Extraction happens before the body is read, so an unauthenticated
/// request is rejected without Janus buffering anything the caller sends.
#[derive(Debug, Clone)]
pub struct RequestHead {
    pub method: Method,
    /// The request target as received on the data plane: path plus query.
    /// A `query` source reads from here.
    pub uri: Uri,
    pub headers: HeaderMap,
}

impl RequestHead {
    pub fn new(method: Method, uri: Uri, headers: HeaderMap) -> Self {
        RequestHead {
            method,
            uri,
            headers,
        }
    }

    /// The path component of [`RequestHead::uri`].
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    /// The raw query string, if any.
    pub fn query(&self) -> Option<&str> {
        self.uri.query()
    }
}

/// A verified caller identity: the output of a successful `extract`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedIdentity {
    /// The identity policy keys off. Becomes
    /// `ResolveRouteRequest.caller_identity`. Opaque downstream: a stable
    /// string to compare and index, never a structure to parse.
    pub caller_identity: String,

    /// Remaining verified claims, if the method produced any. Becomes
    /// `ResolveRouteRequest.identity_claims`.
    pub claims: BTreeMap<String, String>,

    /// A grouping value, if the binding asked for one. Feeds
    /// `correlation_id`. May come from an unverified passthrough header
    /// (`docs/caller-identity#correlation`), so it must never key policy.
    pub correlation_id: Option<String>,

    /// Exactly where the identity material was found, so the substitution
    /// stage can assert it is absent from the outbound request
    /// (`docs/caller-identity#scrub`).
    pub presented_at: PresentedAt,
}

/// Where in the inbound request the identity material sat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentedAt {
    /// A request header (any scheme prefix included).
    Header(HeaderName),
    /// A query parameter of this name.
    Query(String),
    /// The password half of HTTP basic auth, carried in `Authorization`.
    BasicPassword,
}

impl PresentedAt {
    /// The header the material travelled in, if it travelled in one.
    /// `BasicPassword` reports `authorization`.
    pub fn header(&self) -> Option<&HeaderName> {
        match self {
            PresentedAt::Header(name) => Some(name),
            PresentedAt::BasicPassword => Some(&http::header::AUTHORIZATION),
            PresentedAt::Query(_) => None,
        }
    }
}

/// Why extraction rejected a request. Every variant is a 401 to the caller
/// and nothing else happens (`docs/caller-identity#failure`); the variants
/// exist because each is a different operator problem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractionError {
    /// No identity material where the binding says it sits — a caller that
    /// was never configured.
    #[error("no identity material presented")]
    NoMaterial,
    /// Material present but not genuine: bad signature, malformed, unknown
    /// opaque token.
    #[error("identity material invalid: {0}")]
    Invalid(String),
    /// Material was genuine once but its lifetime has passed.
    #[error("identity material expired")]
    Expired,
    /// An opaque-registry entry that an operator revoked.
    #[error("identity revoked")]
    Revoked,
    /// Material valid, but for a different namespace (`hmac`) or issuer or
    /// audience (`jwt`) than this binding expects — a caller pointed at the
    /// wrong Janus binding.
    #[error("identity material is for a different namespace or issuer: {0}")]
    WrongNamespace(String),
}

impl ExtractionError {
    /// The request log's `rejected_identity` kind
    /// (`docs/admin-surface#request-log`).
    pub fn kind(&self) -> RejectionKind {
        match self {
            ExtractionError::NoMaterial => RejectionKind::NoMaterial,
            ExtractionError::Invalid(_) => RejectionKind::Invalid,
            ExtractionError::Expired => RejectionKind::Expired,
            ExtractionError::Revoked => RejectionKind::Revoked,
            ExtractionError::WrongNamespace(_) => RejectionKind::WrongNamespace,
        }
    }
}

/// The `kind` of a `rejected_identity` request-log outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionKind {
    NoMaterial,
    Invalid,
    WrongNamespace,
    Expired,
    Revoked,
}

impl RejectionKind {
    pub const ALL: [RejectionKind; 5] = [
        RejectionKind::NoMaterial,
        RejectionKind::Invalid,
        RejectionKind::WrongNamespace,
        RejectionKind::Expired,
        RejectionKind::Revoked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RejectionKind::NoMaterial => "no_material",
            RejectionKind::Invalid => "invalid",
            RejectionKind::WrongNamespace => "wrong_namespace",
            RejectionKind::Expired => "expired",
            RejectionKind::Revoked => "revoked",
        }
    }
}

impl fmt::Display for RejectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity-extraction stage. Constructed from config for every shipped
/// method; implemented by hand for a custom extractor registered in a custom
/// build and referenced as `method = "custom:<name>"`
/// (`docs/caller-identity#custom`).
#[async_trait]
pub trait IdentityExtractor: Send + Sync {
    /// Stable name, used in config bindings and in the request log.
    fn name(&self) -> &str;

    /// Produce a verified identity, or reject the request. Must not have
    /// side effects: it may be called on requests that are later denied.
    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError>;

    /// For the request log only: the identity the presented material
    /// *claims*, decoded without verification, on a request `extract`
    /// rejected. Its result may be written to a log and must never authorize
    /// anything (`docs/caller-identity#failure`). Methods whose material has
    /// no decodable claim (opaque tokens) keep the default, which decodes
    /// nothing.
    fn claimed_identity(&self, _req: &RequestHead) -> Option<String> {
        None
    }
}

impl fmt::Debug for dyn IdentityExtractor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityExtractor")
            .field("name", &self.name())
            .finish()
    }
}

/// The key a `jwt` extractor verifies against, as the daemon resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtKeyInput {
    /// Fetched by the factory at construction, and kept current by the
    /// extractor.
    Jwks(Url),
    /// The contents of `public_key_file`.
    PublicKeyPem(Vec<u8>),
}

/// A resolved binding plus the material it names: everything needed to
/// construct one shipped extractor (see the module docs).
#[derive(Debug, Clone)]
pub enum ExtractorInput {
    Hmac {
        spec: HmacSpec,
        secret: Secret,
    },
    Jwt {
        spec: JwtSpec,
        key: JwtKeyInput,
    },
    Opaque {
        spec: OpaqueSpec,
        registry: Arc<dyn OpaqueRegistry>,
    },
    TrustedHeader(TrustedHeaderSpec),
}

/// Why a shipped extractor could not be constructed. Startup fails on any.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractorBuildError {
    #[error("hmac shared secret is empty")]
    EmptySecret,
    #[error("jwt verification key is unusable: {0}")]
    InvalidKey(String),
    #[error("JWKS endpoint {url} is unreachable: {reason}")]
    JwksUnreachable { url: String, reason: String },
}

/// Constructs the shipped extractors; implemented by `janus_core::identity`.
#[async_trait]
pub trait ExtractorFactory: Send + Sync {
    async fn build(
        &self,
        input: ExtractorInput,
    ) -> Result<Arc<dyn IdentityExtractor>, ExtractorBuildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_error_kinds_match_request_log_vocabulary() {
        let cases = [
            (ExtractionError::NoMaterial, "no_material"),
            (ExtractionError::Invalid("bad mac".into()), "invalid"),
            (
                ExtractionError::WrongNamespace("ns".into()),
                "wrong_namespace",
            ),
            (ExtractionError::Expired, "expired"),
            (ExtractionError::Revoked, "revoked"),
        ];
        assert_eq!(cases.len(), RejectionKind::ALL.len());
        for ((err, kind), listed) in cases.into_iter().zip(RejectionKind::ALL) {
            assert_eq!(err.kind(), listed);
            assert_eq!(err.kind().as_str(), kind);
            assert_eq!(
                serde_json::to_string(&err.kind()).expect("ser"),
                format!("\"{kind}\"")
            );
        }
    }

    #[test]
    fn presented_at_reports_the_carrying_header() {
        let h = HeaderName::from_static("x-api-key");
        assert_eq!(PresentedAt::Header(h.clone()).header(), Some(&h));
        assert_eq!(
            PresentedAt::BasicPassword.header(),
            Some(&http::header::AUTHORIZATION)
        );
        assert_eq!(PresentedAt::Query("token".into()).header(), None);
        let head = RequestHead::new(
            Method::GET,
            Uri::from_static("/v1/models?token=abc"),
            HeaderMap::new(),
        );
        assert_eq!(head.path(), "/v1/models");
        assert_eq!(head.query(), Some("token=abc"));
    }
}
