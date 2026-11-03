//! Step 5 of the request lifecycle: the outbound request, constructed.
//!
//! The outbound head is built from nothing: the inbound method, the target
//! rewritten onto the upstream's `base_url` (routed by the credential's
//! kind, which is where OpenAI sends Codex OAuth), and only the inbound headers on
//! the upstream's forwarding allowlist (`docs/caller-identity#scrub`).
//! Whatever carried the caller's identity is absent because it was never
//! added. `presented_at` is then checked on top of that, as an assertion:
//! an allowlist that wrongly names the identity-bearing header is a loud
//! [`SubstitutionError::IdentityLeak`], never a forwarded token.

use std::borrow::Cow;

use http::header::{self, HeaderMap, HeaderName};
use http::{Request, Uri};
use url::{form_urlencoded, Url};

use crate::contract::credential::CredentialMaterial;
use crate::contract::identity::{PresentedAt, RequestHead};
use crate::contract::upstream::{UpstreamError, UpstreamImpl, UpstreamKind};

/// Everything step 5 needs besides the inbound head.
pub struct Substitution<'a> {
    pub upstream: &'a dyn UpstreamImpl,
    /// The `[[upstream]] base_url` of config.
    pub base_url: &'a Url,
    /// The inbound path-and-query with the upstream's `path_prefix`
    /// stripped, e.g. `/v1/messages?beta=true`.
    pub rest: &'a str,
    /// The live credential step 4 resolved. Its `kind` is what
    /// [`UpstreamImpl::outbound_url`] routes by.
    pub material: &'a CredentialMaterial,
    /// Where the caller's identity material sat; `None` on a
    /// `method = "none"` route, which presented none.
    pub presented_at: Option<&'a PresentedAt>,
}

/// The request cannot be forwarded.
#[derive(Debug, thiserror::Error)]
pub enum SubstitutionError {
    /// The upstream implementation refused the target or the credential.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// The caller's identity material would have reached the upstream: the
    /// upstream's forwarding allowlist (or its auth step) carries the
    /// location the identity was read from.
    #[error("caller identity material would be forwarded to {upstream} in {location}; refusing to forward")]
    IdentityLeak {
        upstream: UpstreamKind,
        location: String,
    },
}

/// Construct the outbound request head for `inbound` (step 5): target
/// rewritten, allowlisted headers copied, credential applied, identity
/// location asserted empty. The body is not part of the head; it goes to
/// [`super::Forwarder::forward`] unread.
pub fn build_outbound(
    inbound: &RequestHead,
    sub: Substitution<'_>,
) -> Result<http::request::Parts, SubstitutionError> {
    let kind = sub.upstream.kind();
    // A query-carried identity is the one piece of identity material that
    // arrives inside `rest`; the outbound query is rebuilt without it.
    let rest = match sub.presented_at {
        Some(PresentedAt::Query(name)) => without_query_param(sub.rest, name),
        _ => Cow::Borrowed(sub.rest),
    };
    let url = sub
        .upstream
        .outbound_url(sub.base_url, &rest, sub.material.kind)?;
    let uri: Uri = url
        .as_str()
        .parse()
        .map_err(|_| UpstreamError::BadPath(rest.clone().into_owned()))?;

    let mut request = Request::new(());
    *request.method_mut() = inbound.method.clone();
    *request.uri_mut() = uri;
    let (mut parts, ()) = request.into_parts();
    parts.headers = copy_forwarded(&inbound.headers, sub.upstream.forwarded_headers());

    if let Some(at) = sub.presented_at {
        if let Some(location) = location_carried(&parts, at, None) {
            return Err(SubstitutionError::IdentityLeak {
                upstream: kind,
                location,
            });
        }
    }
    sub.upstream.apply_auth(&mut parts, sub.material)?;
    // Auth legitimately writes `authorization` (say) after the check above;
    // what it must never write there is the caller's own material.
    if let Some(at) = sub.presented_at {
        if let Some(location) = location_carried(&parts, at, Some(&inbound.headers)) {
            return Err(SubstitutionError::IdentityLeak {
                upstream: kind,
                location,
            });
        }
    }
    Ok(parts)
}

/// Whether the outbound request carries the identity location. Before auth
/// (`inbound = None`) any value there is a leak; after auth only a value
/// identical to one the caller presented is.
fn location_carried(
    parts: &http::request::Parts,
    at: &PresentedAt,
    inbound: Option<&HeaderMap>,
) -> Option<String> {
    match at {
        PresentedAt::Query(name) => {
            let query = parts.uri.query().unwrap_or("");
            form_urlencoded::parse(query.as_bytes())
                .any(|(k, _)| k == name.as_str())
                .then(|| format!("query parameter {name:?}"))
        }
        PresentedAt::Header(_) | PresentedAt::BasicPassword => {
            let name = at.header()?;
            let mut outbound = parts.headers.get_all(name).iter().peekable();
            let carried = match inbound {
                None => outbound.peek().is_some(),
                Some(inbound) => {
                    outbound.any(|v| inbound.get_all(name).iter().any(|presented| presented == v))
                }
            };
            carried.then(|| format!("header {name}"))
        }
    }
}

/// `rest` with every query parameter named `name` removed; the other pairs
/// keep their exact encoding and order.
fn without_query_param<'a>(rest: &'a str, name: &str) -> Cow<'a, str> {
    let Some((path, query)) = rest.split_once('?') else {
        return Cow::Borrowed(rest);
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            !form_urlencoded::parse(key.as_bytes()).any(|(k, _)| k == name)
        })
        .collect();
    if kept.is_empty() {
        Cow::Owned(path.to_owned())
    } else {
        Cow::Owned(format!("{path}?{}", kept.join("&")))
    }
}

/// Headers that describe one connection rather than the message, and are
/// never copied across a proxy (RFC 9110 section 7.6.1), plus `host`, which
/// the client sets from the outbound URL.
const HOP_BY_HOP: [HeaderName; 9] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// The hop-by-hop set for one message: the fixed list plus every header
/// its `Connection` header names.
fn hop_by_hop(headers: &HeaderMap) -> Vec<HeaderName> {
    let mut names = HOP_BY_HOP.to_vec();
    for value in headers.get_all(header::CONNECTION) {
        let Ok(value) = value.to_str() else { continue };
        names.extend(
            value
                .split(',')
                .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok()),
        );
    }
    names
}

/// A fresh header map holding only the allowlisted inbound headers (every
/// value of each, in order), never a hop-by-hop header or `host`.
fn copy_forwarded(inbound: &HeaderMap, allowlist: &[HeaderName]) -> HeaderMap {
    let excluded = hop_by_hop(inbound);
    let mut out = HeaderMap::new();
    for name in allowlist {
        if *name == header::HOST || excluded.contains(name) {
            continue;
        }
        for value in inbound.get_all(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Remove the hop-by-hop headers from a response before it is relayed; the
/// caller's connection gets its own framing.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in hop_by_hop(headers) {
        headers.remove(&name);
    }
}

/// `base_url` joined with `rest`: the base's path, then `rest`'s path, then
/// `rest`'s query. The shared target rewrite of every shipped upstream.
///
/// `rest` must be empty or start with `/`, carry no fragment, and contain no
/// `.`/`..` segment (literal or percent-encoded): URL normalisation would
/// otherwise resolve one against the base path and send the request outside
/// the configured base.
pub(crate) fn join_base(base_url: &Url, rest: &str) -> Result<Url, UpstreamError> {
    let bad = || UpstreamError::BadPath(rest.to_owned());
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    if !(path.is_empty() || path.starts_with('/')) || rest.contains('#') {
        return Err(bad());
    }
    let dot_segment = path.split(['/', '\\']).any(|segment| {
        let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
        decoded == "." || decoded == ".."
    });
    if dot_segment {
        return Err(bad());
    }
    let mut url = base_url.clone();
    let base_path = base_url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}{path}"));
    url.set_query(query.filter(|q| !q.is_empty()));
    url.set_fragment(None);
    // The authoritative check, on the URL as the url crate normalised it
    // (which can differ from what the segment check above saw: it drops
    // tabs and newlines, so `.\t.` becomes `..` only here). By segment, so
    // base `/api` does not admit `/apix`.
    if !within_base(base_path, url.path()) {
        return Err(bad());
    }
    Ok(url)
}

/// Whether `path` is `base` or lies below it, segment by segment.
fn within_base(base: &str, path: &str) -> bool {
    match path.strip_prefix(base) {
        Some(below) => base.is_empty() || below.is_empty() || below.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::credential::{CredentialKind, Secret};
    use crate::proxy::testkit::{self, Probe};
    use crate::proxy::{AnthropicUpstream, OpenAiUpstream, OpenRouterUpstream};
    use http::{HeaderValue, Method};
    use std::sync::Arc;

    fn head(method: Method, uri: &str, headers: &[(&str, &str)]) -> RequestHead {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        RequestHead::new(method, uri.parse().unwrap(), map)
    }

    fn key(upstream: UpstreamKind, secret: &str) -> CredentialMaterial {
        CredentialMaterial {
            credential_ref: "k".into(),
            upstream,
            kind: CredentialKind::ApiKey,
            secret: Secret::new(secret),
            account_id: None,
        }
    }

    /// Every header value and the full target of the outbound request, as
    /// one string to search for leaked material.
    fn wire_text(parts: &http::request::Parts) -> String {
        let mut s = parts.uri.to_string();
        for (k, v) in &parts.headers {
            s.push_str(&format!("\n{k}: {}", String::from_utf8_lossy(v.as_bytes())));
        }
        s
    }

    #[test]
    fn identity_material_absent_from_outbound_for_every_location_and_upstream() {
        let token = "caller-token-0123456789abcdef";
        let anthropic = AnthropicUpstream::new().unwrap();
        let openai = OpenAiUpstream::new().unwrap();
        let upstreams: [&dyn UpstreamImpl; 3] = [&anthropic, &openai, &OpenRouterUpstream];
        let base = Url::parse("https://upstream.example").unwrap();
        let basic = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("user:{token}")
            )
        );
        let cases: Vec<(PresentedAt, RequestHead, &str)> = vec![
            (
                PresentedAt::Header(header::AUTHORIZATION),
                head(
                    Method::POST,
                    "/p/v1/x",
                    &[("authorization", &format!("Bearer {token}"))],
                ),
                "/v1/x",
            ),
            (
                PresentedAt::Header(HeaderName::from_static("x-api-key")),
                head(Method::POST, "/p/v1/x", &[("x-api-key", token)]),
                "/v1/x",
            ),
            (
                PresentedAt::Header(HeaderName::from_static("x-caller-identity")),
                head(Method::POST, "/p/v1/x", &[("x-caller-identity", token)]),
                "/v1/x",
            ),
            (
                PresentedAt::Query("janus_token".into()),
                head(Method::GET, "/p/v1/x?a=1&janus_token=t&b=2", &[]),
                "/v1/x?a=1&janus_token=caller-token-0123456789abcdef&b=2",
            ),
            (
                PresentedAt::BasicPassword,
                head(Method::POST, "/p/v1/x", &[("authorization", &basic)]),
                "/v1/x",
            ),
        ];
        for upstream in upstreams {
            for (at, inbound, rest) in &cases {
                let material = key(upstream.kind(), "sk-real-credential");
                let parts = build_outbound(
                    inbound,
                    Substitution {
                        upstream,
                        base_url: &base,
                        rest,
                        material: &material,
                        presented_at: Some(at),
                    },
                )
                .unwrap_or_else(|e| panic!("{} {at:?}: {e}", upstream.kind()));
                let wire = wire_text(&parts);
                assert!(
                    !wire.contains(token),
                    "{} {at:?} leaked:\n{wire}",
                    upstream.kind()
                );
                assert!(!wire.contains(&basic), "{wire}");
                assert!(!wire.contains("janus_token"), "{wire}");
                assert!(wire.contains("sk-real-credential"), "{wire}");
            }
        }
        // The other query parameters survive in order and encoding.
        let material = key(UpstreamKind::OpenRouter, "sk");
        let parts = build_outbound(
            &cases[3].1,
            Substitution {
                upstream: &OpenRouterUpstream,
                base_url: &base,
                rest: "/v1/x?a=1&janus%5Ftoken=t&b=%20two",
                material: &material,
                presented_at: Some(&cases[3].0),
            },
        )
        .unwrap();
        assert_eq!(parts.uri.query(), Some("a=1&b=%20two"));
    }

    /// An upstream whose allowlist is `forwarded`: the D6 misconfiguration
    /// when it names an identity-bearing header.
    fn leaky(forwarded: Vec<HeaderName>) -> Probe {
        Probe {
            forwarded: Some(forwarded),
            ..Probe::new(Arc::new(AnthropicUpstream::new().unwrap()))
        }
    }

    #[test]
    fn identity_material_absent_from_outbound_even_when_allowlist_names_it() {
        let leaky = Probe {
            // Deliberately re-adds the identity parameter the substitution
            // stripped, to prove the query assertion runs on the result.
            query_suffix: Some("?token=again"),
            ..leaky(vec![
                header::CONTENT_TYPE,
                HeaderName::from_static("x-caller-identity"),
            ])
        };
        let base = Url::parse("https://api.anthropic.com").unwrap();
        let material = key(UpstreamKind::Anthropic, "sk-real");
        let at = PresentedAt::Header(HeaderName::from_static("x-caller-identity"));
        let inbound = head(
            Method::POST,
            "/anthropic/v1/messages",
            &[
                ("x-caller-identity", "tok"),
                ("content-type", "application/json"),
            ],
        );
        let err = build_outbound(
            &inbound,
            Substitution {
                upstream: &leaky,
                base_url: &base,
                rest: "/v1/messages",
                material: &material,
                presented_at: Some(&at),
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, SubstitutionError::IdentityLeak { location, .. } if location == "header x-caller-identity"),
            "{err}"
        );

        // The same allowlist forwards that header untouched when no
        // identity was read from it (a `none` route).
        let parts = build_outbound(
            &inbound,
            Substitution {
                upstream: &leaky,
                base_url: &base,
                rest: "/v1/messages",
                material: &material,
                presented_at: None,
            },
        )
        .unwrap();
        assert_eq!(parts.headers["x-caller-identity"], "tok");

        // A query identity that the upstream's own rewrite re-adds.
        let at = PresentedAt::Query("token".into());
        let err = build_outbound(
            &head(Method::GET, "/anthropic/v1/models?token=t", &[]),
            Substitution {
                upstream: &leaky,
                base_url: &base,
                rest: "/v1/models?token=t",
                material: &material,
                presented_at: Some(&at),
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, SubstitutionError::IdentityLeak { .. }),
            "{err}"
        );
    }

    #[test]
    fn identity_material_absent_from_outbound_when_allowlist_names_the_auth_header() {
        // Basic-auth identity and an allowlist naming `authorization`, with
        // an OAuth credential whose auth then overwrites `authorization`:
        // only the pre-auth assertion sees the misconfiguration (afterwards
        // the header holds the credential, not the caller's value).
        let leaky = leaky(vec![header::AUTHORIZATION]);
        let base = Url::parse("https://api.anthropic.com").unwrap();
        let material = testkit::material(
            UpstreamKind::Anthropic,
            CredentialKind::Oauth,
            "sk-ant-oat-real",
            None,
        );
        let err = build_outbound(
            &head(
                Method::POST,
                "/a/v1/messages",
                &[("authorization", "Basic dTpw")],
            ),
            Substitution {
                upstream: &leaky,
                base_url: &base,
                rest: "/v1/messages",
                material: &material,
                presented_at: Some(&PresentedAt::BasicPassword),
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, SubstitutionError::IdentityLeak { location, .. } if location == "header authorization"),
            "{err}"
        );
    }

    #[test]
    fn identity_material_absent_from_outbound_when_auth_would_copy_it() {
        // The allowlist is sound, but the auth step writes caller-supplied
        // bytes into the identity header: only the post-auth assertion,
        // which compares against what the caller presented, catches it.
        let identity = HeaderName::from_static("x-caller-identity");
        let copier = Probe {
            forwarded: Some(vec![
                header::CONTENT_TYPE,
                HeaderName::from_static("x-other"),
            ]),
            auth_copies: Some((HeaderName::from_static("x-other"), identity.clone())),
            ..Probe::new(Arc::new(AnthropicUpstream::new().unwrap()))
        };
        let base = Url::parse("https://api.anthropic.com").unwrap();
        let material = key(UpstreamKind::Anthropic, "sk-real");
        let err = build_outbound(
            &head(
                Method::POST,
                "/a/v1/messages",
                &[("x-caller-identity", "tok"), ("x-other", "tok")],
            ),
            Substitution {
                upstream: &copier,
                base_url: &base,
                rest: "/v1/messages",
                material: &material,
                presented_at: Some(&PresentedAt::Header(identity)),
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, SubstitutionError::IdentityLeak { location, .. } if location == "header x-caller-identity"),
            "{err}"
        );
    }

    #[test]
    fn hop_by_hop_headers_stripped_from_outbound_request() {
        // An allowlist that names hop-by-hop headers still never copies
        // them, nor a header the inbound `Connection` declares hop-by-hop.
        let leaky = leaky(vec![
            header::CONNECTION,
            header::TRANSFER_ENCODING,
            header::TE,
            header::UPGRADE,
            header::HOST,
            HeaderName::from_static("keep-alive"),
            HeaderName::from_static("x-per-hop"),
            header::CONTENT_TYPE,
        ]);
        let inbound = head(
            Method::POST,
            "/a/v1/messages",
            &[
                ("connection", "keep-alive, X-Per-Hop"),
                ("keep-alive", "timeout=5"),
                ("transfer-encoding", "chunked"),
                ("te", "trailers"),
                ("upgrade", "h2c"),
                ("host", "janus.local"),
                ("x-per-hop", "1"),
                ("content-type", "application/json"),
            ],
        );
        let base = Url::parse("https://api.anthropic.com").unwrap();
        let material = key(UpstreamKind::Anthropic, "sk-real");
        let parts = build_outbound(
            &inbound,
            Substitution {
                upstream: &leaky,
                base_url: &base,
                rest: "/v1/messages",
                material: &material,
                presented_at: None,
            },
        )
        .unwrap();
        let mut names: Vec<&str> = parts.headers.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["content-type", "x-api-key"], "{names:?}");
    }

    #[test]
    fn hop_by_hop_headers_stripped_from_relayed_response_headers() {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("connection", "close, x-upstream-hop"),
            ("transfer-encoding", "chunked"),
            ("keep-alive", "timeout=5"),
            ("x-upstream-hop", "1"),
            ("content-type", "text/event-stream"),
            ("request-id", "req_1"),
        ] {
            headers.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        strip_hop_by_hop(&mut headers);
        let mut names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["content-type", "request-id"]);
    }

    #[test]
    fn target_rewritten_onto_base_url_with_prefix_stripped() {
        let cases = [
            (
                "https://api.anthropic.com",
                "/v1/messages",
                "https://api.anthropic.com/v1/messages",
            ),
            (
                "https://openrouter.ai/api",
                "/v1/chat/completions",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api/",
                "/v1/models?x=1",
                "https://openrouter.ai/api/v1/models?x=1",
            ),
            ("http://127.0.0.1:9/base", "", "http://127.0.0.1:9/base"),
            (
                "https://h.example",
                "/a%20b/c?q=%2F",
                "https://h.example/a%20b/c?q=%2F",
            ),
            ("https://h.example", "/v1/x?", "https://h.example/v1/x"),
        ];
        for (base, rest, want) in cases {
            let got = join_base(&Url::parse(base).unwrap(), rest).unwrap();
            assert_eq!(got.as_str(), want, "{base} + {rest}");
        }
        let base = Url::parse("https://openrouter.ai/api").unwrap();
        for bad in [
            "v1/x",
            "/../v1/x",
            "/v1/../../x",
            "/%2e%2E/x",
            "/v1/%2E",
            "/./x",
            "/v1\\..\\x",
            "/v1/x#frag",
            // Passes the segment check (`.\t.` is not `..`), but the url
            // crate drops the tabs and resolves `/api/v1/../../apix/y`:
            // outside the base by segment, though it shares its prefix.
            "/v1/.\t./.\t./apix/y",
        ] {
            assert_eq!(
                join_base(&base, bad),
                Err(UpstreamError::BadPath(bad.into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn within_base_compares_whole_segments() {
        assert!(within_base("/api", "/api"));
        assert!(within_base("/api", "/api/v1/x"));
        assert!(within_base("", "/anything"));
        assert!(!within_base("/api", "/apix/y"));
        assert!(!within_base("/api", "/"));
    }

    #[test]
    fn outbound_keeps_method_and_only_allowlisted_headers() {
        let inbound = head(
            Method::PUT,
            "/openrouter/v1/x",
            &[
                ("content-type", "application/json"),
                ("x-title", "my agent"),
                ("x-sandbox-secret", "abc"),
                ("x-request-id", "corr-1"),
                ("cookie", "s=1"),
                ("accept", "text/event-stream"),
                ("accept", "application/json"),
            ],
        );
        let base = Url::parse("https://openrouter.ai/api").unwrap();
        let material = key(UpstreamKind::OpenRouter, "sk-or");
        let parts = build_outbound(
            &inbound,
            Substitution {
                upstream: &OpenRouterUpstream,
                base_url: &base,
                rest: "/v1/x",
                material: &material,
                presented_at: None,
            },
        )
        .unwrap();
        assert_eq!(parts.method, Method::PUT);
        assert_eq!(parts.uri, "https://openrouter.ai/api/v1/x");
        assert!(parts.headers.get("x-sandbox-secret").is_none());
        assert!(parts.headers.get("x-request-id").is_none());
        assert!(parts.headers.get("cookie").is_none());
        assert_eq!(parts.headers["x-title"], "my agent");
        assert_eq!(parts.headers["content-type"], "application/json");
        let accept: Vec<_> = parts.headers.get_all("accept").iter().collect();
        assert_eq!(accept, ["text/event-stream", "application/json"]);
    }
}
