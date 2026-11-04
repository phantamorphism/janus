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

use url::Url;

use crate::contract::binding::is_loopback_ip_literal;
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

/// The [`UpstreamImpl`] methods an upstream shares verbatim with the
/// others, emitted inside its `impl UpstreamImpl` block:
/// `forwarded_headers` returning the static allowlist `$forwarded`, and
/// with `target = base_url` also an `outbound_url` that is the plain
/// [`outbound::join_base`] rewrite, whatever the credential's kind.
macro_rules! upstream_forwarding {
    ($forwarded:ident) => {
        fn forwarded_headers(&self) -> &[http::HeaderName] {
            &$forwarded
        }
    };
    ($forwarded:ident, target = base_url) => {
        upstream_forwarding!($forwarded);

        fn outbound_url(
            &self,
            base_url: &url::Url,
            rest: &str,
            _kind: $crate::contract::credential::CredentialKind,
        ) -> Result<url::Url, $crate::contract::upstream::UpstreamError> {
            super::outbound::join_base(base_url, rest)
        }
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
/// may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the connection to an upstream may go without delivering a
/// byte: waiting for the response head, or between two reads of the body.
/// There is deliberately no whole-request timeout on the forwarding path,
/// since a streamed response legitimately runs for many minutes; what is
/// bounded is silence. Streaming upstreams send events (or keep-alive
/// pings) every few seconds, but a non-streamed completion sends nothing
/// at all until it is done, so the bound is the ten minutes the launch
/// upstreams' own SDKs allow a request before giving up on it.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Why `url` may not be an OAuth token endpoint, or `None` when it may.
///
/// A refresh sends the long-lived refresh token to the endpoint and trusts
/// what comes back as the credential, so the URL must be `https://` with a
/// host. Plain `http://` is accepted only for a loopback IP literal
/// (`127.0.0.1`, `[::1]`), for a mirror or test rig on the same host.
pub fn token_url_refusal(url: &Url) -> Option<&'static str> {
    if url.host_str().is_none_or(str::is_empty) {
        return Some("must be an https:// URL with a host");
    }
    match url.scheme() {
        "https" => None,
        "http" => (!is_loopback_ip_literal(url)).then_some(
            "plain http:// is accepted only for a loopback IP literal (127.0.0.1, [::1]); \
             the refresh token would cross the network unprotected — use https://",
        ),
        _ => Some("must be an https:// URL with a host"),
    }
}

/// The HTTP client could not be constructed (TLS initialisation failed).
#[derive(Debug, thiserror::Error)]
#[error("cannot construct the upstream HTTP client: {0}")]
pub struct ClientInitError(String);

/// The client every outbound call uses.
///
/// - Redirects are never followed: a redirect is part of the upstream's
///   response and is relayed to the caller as is, and following one would
///   carry the substituted credential to wherever the redirect points
///   (reqwest strips only `authorization` on a cross-host hop, not
///   `x-api-key` or `chatgpt-account-id`).
/// - No proxy, whatever the environment says: `HTTP(S)_PROXY`/`ALL_PROXY`
///   in the daemon's environment would otherwise route every substituted
///   credential and token refresh through that proxy, in plaintext for an
///   `http://` base URL. An operator who wants egress through a proxy
///   points `base_url` at it.
/// - Bounded silence ([`READ_IDLE_TIMEOUT`]), so a stalled upstream ends
///   the call (and settles it) instead of holding it until the caller
///   gives up.
fn http_client() -> Result<reqwest::Client, ClientInitError> {
    http_client_with(READ_IDLE_TIMEOUT, |builder| builder)
}

/// [`http_client`] with its idle bound set and `extend` applied to the
/// builder before it is built. Idle connections are dropped after
/// [`relay::POOL_IDLE_TIMEOUT`], see [`Forwarder`].
fn http_client_with(
    read_idle_timeout: Duration,
    extend: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> Result<reqwest::Client, ClientInitError> {
    extend(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(read_idle_timeout)
            .pool_idle_timeout(relay::POOL_IDLE_TIMEOUT),
    )
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

    /// The shipped implementations, with the given OAuth token endpoints in
    /// place of the vendors' (`None` keeps the vendor default).
    pub fn with_token_urls(
        anthropic: Option<Url>,
        openai: Option<Url>,
    ) -> Result<Self, ClientInitError> {
        let mut a = AnthropicUpstream::new()?;
        if let Some(url) = anthropic {
            a = a.with_token_url(url);
        }
        let mut o = OpenAiUpstream::new()?;
        if let Some(url) = openai {
            o = o.with_token_url(url);
        }
        Ok(UpstreamSet::from_impls(a, o, OpenRouterUpstream))
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

    /// Set in the environment of the child process
    /// [`outbound_calls_ignore_proxy_environment`] runs.
    const PROXY_ENV_CHILD: &str = "JANUS_TEST_PROXY_ENV_CHILD";

    #[test]
    fn token_urls_need_https_unless_loopback_literal() {
        let refused = |u: &str| token_url_refusal(&Url::parse(u).unwrap()).is_some();
        assert!(!refused("https://auth.example.com/oauth/token"));
        assert!(!refused("http://127.0.0.1:9000/token"));
        assert!(!refused("http://[::1]:9000/token"));
        assert!(refused("http://auth.example.com/token"));
        assert!(refused("http://localhost:9000/token"));
        assert!(refused("http://10.0.0.5/token"));
        assert!(refused("ftp://127.0.0.1/token"));
        assert!(refused("file:///etc/token"));
    }

    #[test]
    fn outbound_calls_ignore_proxy_environment() {
        // The proxy variables are process-wide and read when a client is
        // built, so the check runs in a child process of its own rather
        // than racing every other test's clients.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proxy::tests::proxy_environment_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(PROXY_ENV_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The child half of [`outbound_calls_ignore_proxy_environment`]; a
    /// no-op anywhere else.
    #[test]
    fn proxy_environment_child() {
        if std::env::var_os(PROXY_ENV_CHILD).is_none() {
            return;
        }
        // A proxy that accepts connections and never answers: a client
        // that used it would hang into the timeout below.
        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        let trap_url = format!("http://{}", trap.local_addr().unwrap());
        for var in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(var, &trap_url);
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let upstream =
                testkit::spawn(|_req| testkit::Reply::bytes(200, &[], b"ok".to_vec())).await;
            let mut parts = http::Request::new(()).into_parts().0;
            parts.uri = upstream.url.join("v1/x").unwrap().as_str().parse().unwrap();
            let relayed = tokio::time::timeout(
                Duration::from_secs(5),
                Forwarder::new().unwrap().forward(
                    Arc::new(OpenRouterUpstream),
                    parts,
                    http_body_util::Empty::<bytes::Bytes>::new(),
                ),
            )
            .await
            .expect("sent directly, not through the environment's proxy")
            .unwrap();
            assert_eq!(relayed.response.status(), 200);
            assert_eq!(upstream.received().len(), 1);
        });
        assert_eq!(
            trap.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the proxy was contacted"
        );
    }

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
