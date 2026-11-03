//! The upstreams Janus implements: their contract names, and the narrow
//! per-upstream surface each one supplies.
//!
//! `ResolveRouteRequest.upstream` and the `upstream` fields of config, the
//! request log and the admin surface all use the [`UpstreamKind`] names
//! (`docs/index` "First upstreams"). New upstreams are new values here, not
//! a proto change.
//!
//! [`UpstreamImpl`] is the enumerable list of
//! `docs/credential-substitution#general-purpose`: how auth is carried, the
//! base URL and path rewrite, the forwarding allowlist, how a credential of
//! that kind is refreshed, what in a response is observed as `usage` — plus
//! which responses mean the credential or the account is at fault.
//! `janus_core::proxy` supplies one implementation per
//! [`UpstreamKind::ALL`] entry; the pool, the data plane and the forwarding
//! path select it by [`UpstreamImpl::kind`] and never match on the upstream
//! themselves.

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderName, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

use super::credential::{
    CredentialKind, CredentialMaterial, MaterialDocument, Secret, UpstreamFeedback,
};

/// An upstream implementation shipped in `janus-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UpstreamKind {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl UpstreamKind {
    /// Every implemented upstream.
    pub const ALL: [UpstreamKind; 3] = [
        UpstreamKind::Anthropic,
        UpstreamKind::OpenAi,
        UpstreamKind::OpenRouter,
    ];

    /// The contract name: `"anthropic"`, `"openai"` or `"openrouter"`.
    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamKind::Anthropic => "anthropic",
            UpstreamKind::OpenAi => "openai",
            UpstreamKind::OpenRouter => "openrouter",
        }
    }
}

impl fmt::Display for UpstreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name did not match an implemented upstream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown upstream {0:?}: expected one of \"anthropic\", \"openai\", \"openrouter\"")]
pub struct UnknownUpstream(pub String);

impl FromStr for UpstreamKind {
    type Err = UnknownUpstream;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UpstreamKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownUpstream(s.to_owned()))
    }
}

/// A stored credential document, decoded: what the pool needs to know about
/// it without understanding the upstream's file format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMaterial {
    pub kind: CredentialKind,
    /// The API key or current access token.
    pub secret: Secret,
    pub account_id: Option<String>,
    /// When the access token stops working; `None` for a key that does not
    /// expire. The pool refreshes before handing out material past it.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A per-upstream operation that failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// The material document is not one this upstream knows how to store.
    #[error("unusable credential material: {0}")]
    InvalidMaterial(String),
    /// Refresh asked of a credential kind that has none (an API key).
    #[error("a {0:?} credential cannot be refreshed")]
    NotRefreshable(CredentialKind),
    #[error("credential refresh failed: {0}")]
    RefreshFailed(String),
    /// The inbound path cannot be rewritten onto the base URL.
    #[error("cannot rewrite path {0:?} for this upstream")]
    BadPath(String),
    /// The secret cannot be carried in this upstream's auth form (for
    /// example, bytes that are not a valid header value).
    #[error("credential cannot be applied to the request: {0}")]
    Auth(String),
}

/// Watches one response on its way back to the caller and produces the
/// `usage` object of `call_completed` and the request log. It sees the
/// bytes as they are relayed and must not hold up or change them.
pub trait UsageObserver: Send {
    fn observe_head(&mut self, status: StatusCode, headers: &HeaderMap);

    /// One body chunk, in order. Streaming responses arrive in many.
    fn observe_chunk(&mut self, chunk: &[u8]);

    /// The observation, once the body has ended (or the call was cut off,
    /// in which case it reports what was seen). `Value::Null` when there is
    /// nothing worth reporting.
    fn finish(self: Box<Self>) -> serde_json::Value;
}

/// Everything upstream-specific (`docs/credential-substitution#general-purpose`).
#[async_trait]
pub trait UpstreamImpl: Send + Sync {
    fn kind(&self) -> UpstreamKind;

    /// The inbound headers copied onto the outbound request; nothing else is
    /// (`docs/caller-identity#scrub`). The outbound request is constructed
    /// from this list, never copied-and-edited, so whatever carried the
    /// caller's identity is absent unless this list wrongly names it — and
    /// the data plane asserts against `ExtractedIdentity::presented_at` to
    /// catch exactly that.
    fn forwarded_headers(&self) -> &[HeaderName];

    /// The outbound URL: `base_url` (the config's `[[upstream]] base_url`)
    /// joined with `rest`, the inbound path-and-query with the upstream's
    /// `path_prefix` already stripped, plus any rewriting this upstream
    /// needs.
    fn outbound_url(&self, base_url: &Url, rest: &str) -> Result<Url, UpstreamError>;

    /// Apply `material` to the constructed outbound request in the form this
    /// upstream expects: a header and scheme, or a signing procedure over
    /// the request head.
    fn apply_auth(
        &self,
        request: &mut http::request::Parts,
        material: &CredentialMaterial,
    ) -> Result<(), UpstreamError>;

    /// Interpret a stored or submitted material document (auto-detecting
    /// `.credentials.json`, `auth.json` or a plain API key, as this upstream
    /// requires).
    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError>;

    /// Refresh an expiring credential, returning the document to write back
    /// to its slot. `NotRefreshable` for a kind with no refresh.
    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError>;

    /// A fresh observer for one response.
    fn usage_observer(&self) -> Box<dyn UsageObserver>;

    /// Whether a response blames the credential (`CredentialInvalid`,
    /// `RateLimited`) or the account (`AccountError`) rather than the
    /// request. `None` for every other response, including ordinary request
    /// errors. `body` is the start of the response body, as much as the
    /// data plane buffered while relaying it.
    fn classify(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Option<UpstreamFeedback>;
}

impl fmt::Debug for dyn UpstreamImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamImpl")
            .field("kind", &self.kind())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_names_round_trip_through_str_and_serde() {
        for kind in UpstreamKind::ALL {
            assert_eq!(kind.as_str().parse::<UpstreamKind>(), Ok(kind));
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let back: UpstreamKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, kind);
        }
        assert_eq!(
            "gemini".parse::<UpstreamKind>(),
            Err(UnknownUpstream("gemini".into()))
        );
    }
}
