//! Real local HTTP servers for the proxy tests: a recording fake upstream
//! whose replies (including slow streams) the test scripts, and a minimal
//! data plane that runs `build_outbound` + `Forwarder::forward` per request.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body::Frame;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use url::Url;

use base64::Engine;

use crate::contract::credential::{
    CredentialKind, CredentialMaterial, MaterialDocument, Secret, UpstreamFeedback,
};
use crate::contract::identity::{PresentedAt, RequestHead};
use crate::contract::upstream::{
    DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind, UsageObserver,
};
use crate::proxy::{build_outbound, Forwarder, RelayOutcome, Substitution};

/// One request as the fake upstream received it.
#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    pub method: Method,
    /// Path and query.
    pub target: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

type ReplyBody = UnsyncBoxBody<Bytes, Infallible>;

/// A scripted response.
pub(crate) struct Reply(Response<ReplyBody>);

impl Reply {
    fn with_body(status: u16, headers: &[(&str, &str)], body: ReplyBody) -> Reply {
        let mut response = Response::new(body);
        *response.status_mut() = StatusCode::from_u16(status).unwrap();
        for (k, v) in headers {
            response.headers_mut().append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        Reply(response)
    }

    pub fn bytes(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Reply {
        Reply::with_body(status, headers, Full::new(Bytes::from(body)).boxed_unsync())
    }

    pub fn json(status: u16, body: serde_json::Value) -> Reply {
        Reply::bytes(
            status,
            &[("content-type", "application/json")],
            serde_json::to_vec(&body).unwrap(),
        )
    }

    /// A chunked body whose chunks are sent as `chunks` yields them.
    pub fn stream(
        status: u16,
        headers: &[(&str, &str)],
        chunks: impl Stream<Item = Bytes> + Send + 'static,
    ) -> Reply {
        let body = StreamBody::new(chunks.map(|b| Ok::<_, Infallible>(Frame::data(b))));
        Reply::with_body(status, headers, BodyExt::boxed_unsync(body))
    }
}

/// A running server; stops when dropped.
pub(crate) struct Server {
    /// `http://127.0.0.1:<port>/`.
    pub url: Url,
    received: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub fn received(&self) -> Vec<Recorded> {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve<F, Fut>(handle: F) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response<ReplyBodyAny>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handle = handle.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let handle = handle.clone();
                    async move { Ok::<_, Infallible>(handle(req).await) }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, task)
}

type ReplyBodyAny = UnsyncBoxBody<Bytes, crate::proxy::BoxError>;

/// A fake upstream that records every request (body read in full) and
/// answers with `reply(&request)`.
pub(crate) async fn spawn<F>(reply: F) -> Server
where
    F: Fn(&Recorded) -> Reply + Send + Sync + 'static,
{
    let received = Arc::new(Mutex::new(Vec::new()));
    let log = received.clone();
    let reply = Arc::new(reply);
    let (addr, task) = serve(move |req: Request<Incoming>| {
        let log = log.clone();
        let reply = reply.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = body
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            let recorded = Recorded {
                method: parts.method,
                target: parts
                    .uri
                    .path_and_query()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_default(),
                headers: parts.headers,
                body,
            };
            let Reply(response) = reply(&recorded);
            log.lock().unwrap().push(recorded);
            response.map(|b| b.map_err(|never| match never {}).boxed_unsync())
        }
    })
    .await;
    Server {
        url: Url::parse(&format!("http://{addr}/")).unwrap(),
        received,
        task,
    }
}

/// A minimal data plane for one upstream, mounted at `/<kind>`: the caller
/// presents its identity in `authorization`, and every request is
/// substituted with `material` and forwarded to `base_url`.
pub(crate) async fn spawn_proxy(
    upstream: Arc<dyn UpstreamImpl>,
    base_url: Url,
    material: CredentialMaterial,
) -> Server {
    let forwarder = Forwarder::new().unwrap();
    let ctx = Arc::new((upstream, base_url, material, forwarder));
    let (addr, task) = serve(move |req: Request<Incoming>| {
        let ctx = ctx.clone();
        async move {
            let (upstream, base_url, material, forwarder) = &*ctx;
            let (parts, body) = req.into_parts();
            let head = RequestHead::new(parts.method, parts.uri, parts.headers);
            let prefix = format!("/{}", upstream.kind());
            let target = head.uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
            let rest = target.strip_prefix(&prefix).unwrap_or(target);
            let at = PresentedAt::Header(http::header::AUTHORIZATION);
            let outbound = build_outbound(
                &head,
                Substitution {
                    upstream: upstream.as_ref(),
                    base_url,
                    rest,
                    material,
                    presented_at: Some(&at),
                },
            );
            let relayed = match outbound {
                Ok(parts) => forwarder.forward(upstream.clone(), parts, body).await,
                Err(e) => return bad_gateway(e.to_string()),
            };
            match relayed {
                Ok(relayed) => relayed.response.map(BodyExt::boxed_unsync),
                Err(e) => bad_gateway(e.to_string()),
            }
        }
    })
    .await;
    Server {
        url: Url::parse(&format!("http://{addr}/")).unwrap(),
        received: Arc::default(),
        task,
    }
}

/// Live material for `upstream`.
pub(crate) fn material(
    upstream: UpstreamKind,
    kind: CredentialKind,
    secret: &str,
    account_id: Option<&str>,
) -> CredentialMaterial {
    CredentialMaterial {
        credential_ref: format!("{upstream}-test"),
        upstream,
        kind,
        secret: Secret::new(secret),
        account_id: account_id.map(str::to_owned),
    }
}

/// What one forwarded call produced.
pub(crate) struct RoundTrip {
    /// What the fake upstream received.
    pub received: Recorded,
    pub status: StatusCode,
    pub body: Bytes,
    pub outcome: RelayOutcome,
}

/// Send one caller request (identity presented as `authorization: Bearer
/// caller-identity-token`, plus `headers`) through `build_outbound` and
/// `Forwarder::forward` to a fake upstream (the `base_url`) answering
/// `reply`, and collect everything the call produced.
pub(crate) async fn round_trip<F>(
    upstream: Arc<dyn UpstreamImpl>,
    material: &CredentialMaterial,
    rest: &str,
    headers: &[(&str, &str)],
    reply: F,
) -> RoundTrip
where
    F: Fn(&Recorded) -> Reply + Send + Sync + 'static,
{
    let server = spawn(reply).await;
    let mut inbound = HeaderMap::new();
    inbound.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-identity-token"),
    );
    for (k, v) in headers {
        inbound.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    let head = RequestHead::new(
        Method::POST,
        format!("/{}{rest}", upstream.kind()).parse().unwrap(),
        inbound,
    );
    let at = PresentedAt::Header(http::header::AUTHORIZATION);
    let parts = build_outbound(
        &head,
        Substitution {
            upstream: upstream.as_ref(),
            base_url: &server.url,
            rest,
            material,
            presented_at: Some(&at),
        },
    )
    .expect("substituted");
    let relayed = Forwarder::new()
        .unwrap()
        .forward(
            upstream,
            parts,
            Full::new(Bytes::from_static(b"{\"model\":\"m\"}")),
        )
        .await
        .expect("forwarded");
    let status = relayed.response.status();
    let body = relayed
        .response
        .into_body()
        .collect()
        .await
        .expect("relayed body")
        .to_bytes();
    let outcome = relayed.outcome.await.expect("outcome");
    let mut received = server.received();
    assert_eq!(received.len(), 1);
    RoundTrip {
        received: received.remove(0),
        status,
        body,
        outcome,
    }
}

/// An unsigned JWT carrying `claims`, as a vendor access token looks to
/// code that only reads it.
pub(crate) fn jwt(claims: serde_json::Value) -> String {
    let b64 = |v: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    };
    format!(
        "{}.{}.sig",
        b64(&serde_json::json!({"alg": "RS256"})),
        b64(&claims)
    )
}

/// A deliberately misbehaving upstream for the guards' tests: `inner` in
/// every respect except the overrides set.
pub(crate) struct Probe {
    pub inner: Arc<dyn UpstreamImpl>,
    /// Replaces the forwarding allowlist (the D6 misconfiguration when it
    /// names an identity-bearing header).
    pub forwarded: Option<Vec<HeaderName>>,
    /// Appended to `rest` before the target rewrite: an upstream rewrite
    /// that re-adds a query parameter.
    pub query_suffix: Option<&'static str>,
    /// After `inner`'s auth, copy the value of the first header onto the
    /// second: an auth step that carries caller-supplied bytes.
    pub auth_copies: Option<(HeaderName, HeaderName)>,
    /// The length of every body prefix `classify` was shown.
    pub classified: Mutex<Vec<usize>>,
}

impl Probe {
    pub fn new(inner: Arc<dyn UpstreamImpl>) -> Probe {
        Probe {
            inner,
            forwarded: None,
            query_suffix: None,
            auth_copies: None,
            classified: Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl UpstreamImpl for Probe {
    fn kind(&self) -> UpstreamKind {
        self.inner.kind()
    }
    fn forwarded_headers(&self) -> &[HeaderName] {
        self.forwarded
            .as_deref()
            .unwrap_or_else(|| self.inner.forwarded_headers())
    }
    fn outbound_url(
        &self,
        base_url: &Url,
        rest: &str,
        kind: CredentialKind,
    ) -> Result<Url, UpstreamError> {
        let rest = format!("{rest}{}", self.query_suffix.unwrap_or(""));
        self.inner.outbound_url(base_url, &rest, kind)
    }
    fn apply_auth(
        &self,
        request: &mut http::request::Parts,
        material: &CredentialMaterial,
    ) -> Result<(), UpstreamError> {
        self.inner.apply_auth(request, material)?;
        if let Some((from, to)) = &self.auth_copies {
            if let Some(value) = request.headers.get(from).cloned() {
                request.headers.insert(to.clone(), value);
            }
        }
        Ok(())
    }
    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> {
        self.inner.decode_material(doc)
    }
    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> {
        self.inner.refresh(doc).await
    }
    fn usage_observer(&self) -> Box<dyn UsageObserver> {
        self.inner.usage_observer()
    }
    fn classify(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Option<UpstreamFeedback> {
        self.classified.lock().unwrap().push(body.len());
        self.inner.classify(status, headers, body)
    }
}

fn bad_gateway(message: String) -> Response<ReplyBodyAny> {
    let mut response = Response::new(
        Full::new(Bytes::from(message))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response
}
