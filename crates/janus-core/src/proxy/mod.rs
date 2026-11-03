//! Substitution and forwarding: constructing the outbound request from a
//! per-upstream forwarding allowlist, applying the real credential's auth,
//! rewriting the target, relaying (including streaming) responses unmodified,
//! and observing `usage`. Codes against [`crate::contract`]. See
//! `docs/credential-substitution` and `docs/architecture` "Request lifecycle".
//!
//! What the data plane calls, in lifecycle order:
//!
//! 1. [`UpstreamSet::get`] — the [`UpstreamImpl`] for the request's upstream
//!    (one of [`AnthropicUpstream`], [`OpenAiUpstream`],
//!    [`OpenRouterUpstream`]). The pool uses the same set for
//!    `decode_material` and `refresh`.
//! 2. [`build_outbound`] — step 5: the outbound request head, constructed
//!    from the upstream's forwarding allowlist, the target rewritten onto
//!    `base_url` and routed by the credential's kind, the credential applied, and the caller's identity location
//!    (`ExtractedIdentity::presented_at`) asserted empty. A
//!    [`SubstitutionError`] means the request must not be forwarded.
//! 3. [`Forwarder::forward`] — step 6: sends it with the caller's body
//!    streamed through, and returns a [`Relayed`]: the upstream's response
//!    (status and headers as received, minus hop-by-hop headers; a body that
//!    is relayed chunk by chunk as it arrives) plus a receiver for the
//!    [`RelayOutcome`] — the `usage` observation, the
//!    [`classify`](UpstreamImpl::classify) verdict for the pool and the
//!    `credential_invalid` / `account_error` events, and how the body ended
//!    — which resolves once the body has been relayed or abandoned, off the
//!    caller's critical path.
//!
//! The per-upstream modules hold only the narrow list of
//! `docs/credential-substitution#general-purpose`; everything else here is
//! upstream-agnostic and never matches on [`UpstreamKind`].

use std::sync::Arc;
use std::time::Duration;

use crate::contract::upstream::{UpstreamImpl, UpstreamKind};

/// An upstream's forwarding allowlist: the headers every launch upstream's
/// SDKs send that describe the request itself — body framing and type,
/// what is accepted back, the client's identification (`user-agent` and
/// the `x-stainless-*` telemetry the official SDKs attach) — followed by
/// the upstream's own. Deliberately never on it: any credential-bearing
/// header (`authorization`, `x-api-key`, cookies), `host` and hop-by-hop
/// headers, and `accept-encoding`, so the response arrives unencoded and its
/// `usage` can be observed (the caller still receives exactly the bytes the
/// upstream sent).
macro_rules! forwarding_allowlist {
    (static $name:ident = [$($upstream_specific:literal),* $(,)?];) => {
        static $name: [http::HeaderName; 12 + [$($upstream_specific),*].len()] = [
            http::header::CONTENT_TYPE,
            http::header::CONTENT_LENGTH,
            http::header::ACCEPT,
            http::header::USER_AGENT,
            http::HeaderName::from_static("x-stainless-arch"),
            http::HeaderName::from_static("x-stainless-lang"),
            http::HeaderName::from_static("x-stainless-os"),
            http::HeaderName::from_static("x-stainless-package-version"),
            http::HeaderName::from_static("x-stainless-retry-count"),
            http::HeaderName::from_static("x-stainless-runtime"),
            http::HeaderName::from_static("x-stainless-runtime-version"),
            http::HeaderName::from_static("x-stainless-timeout"),
            $(http::HeaderName::from_static($upstream_specific),)*
        ];
    };
}

mod anthropic;
mod feedback;
mod material;
mod openai;
mod openrouter;
mod outbound;
mod relay;
mod usage;

#[cfg(test)]
mod testkit;

pub use anthropic::{AnthropicUpstream, ANTHROPIC_TOKEN_URL};
pub use openai::{OpenAiUpstream, CODEX_BASE_URL, OPENAI_TOKEN_URL};
pub use openrouter::OpenRouterUpstream;
pub use outbound::{build_outbound, Substitution, SubstitutionError};
pub use relay::{BoxError, ForwardError, Forwarder, RelayBody, RelayEnd, RelayOutcome, Relayed};

/// How long establishing a connection to an upstream (or a token endpoint)
/// may take. There is deliberately no whole-request timeout on the
/// forwarding path: a streamed response legitimately runs for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP client could not be constructed (TLS initialisation failed).
#[derive(Debug, thiserror::Error)]
#[error("cannot construct the upstream HTTP client: {0}")]
pub struct ClientInitError(String);

/// The client every outbound call uses. Redirects are never followed: a
/// redirect is part of the upstream's response and is relayed to the caller
/// as is, and following one would carry the substituted credential to
/// wherever the redirect points.
fn http_client() -> Result<reqwest::Client, ClientInitError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| ClientInitError(e.to_string()))
}

/// One [`UpstreamImpl`] per [`UpstreamKind::ALL`] entry, looked up by kind.
#[derive(Clone)]
pub struct UpstreamSet {
    anthropic: Arc<dyn UpstreamImpl>,
    openai: Arc<dyn UpstreamImpl>,
    openrouter: Arc<dyn UpstreamImpl>,
}

impl UpstreamSet {
    /// The shipped implementations, talking to the real upstreams' token
    /// endpoints and (for Codex OAuth) the ChatGPT backend.
    pub fn new() -> Result<Self, ClientInitError> {
        Ok(UpstreamSet::from_impls(
            AnthropicUpstream::new()?,
            OpenAiUpstream::new()?,
            OpenRouterUpstream,
        ))
    }

    /// A set from individually configured implementations, for a daemon
    /// (or a test) that points their token endpoints or the Codex backend
    /// elsewhere.
    pub fn from_impls(
        anthropic: AnthropicUpstream,
        openai: OpenAiUpstream,
        openrouter: OpenRouterUpstream,
    ) -> Self {
        UpstreamSet {
            anthropic: Arc::new(anthropic),
            openai: Arc::new(openai),
            openrouter: Arc::new(openrouter),
        }
    }

    /// The implementation for `kind`.
    pub fn get(&self, kind: UpstreamKind) -> &Arc<dyn UpstreamImpl> {
        match kind {
            UpstreamKind::Anthropic => &self.anthropic,
            UpstreamKind::OpenAi => &self.openai,
            UpstreamKind::OpenRouter => &self.openrouter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_set_supplies_an_implementation_for_every_kind() {
        let set = UpstreamSet::new().expect("client");
        for kind in UpstreamKind::ALL {
            assert_eq!(set.get(kind).kind(), kind);
            assert!(
                !set.get(kind).forwarded_headers().is_empty(),
                "{kind} forwards nothing"
            );
        }
    }
}
