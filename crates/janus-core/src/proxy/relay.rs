//! Step 6 of the request lifecycle: the call forwarded and the response
//! relayed unmodified, streaming if the upstream streams.
//!
//! The response body is a [`RelayBody`]: each chunk the upstream sends is
//! handed to the caller's connection as soon as that connection asks for the
//! next one, and the upstream is read no faster than the caller is written
//! to — so back-pressure reaches the upstream and nothing accumulates here.
//! On its way through, each chunk is shown to the upstream's
//! [`UsageObserver`] and the first [`CLASSIFY_PREFIX_LIMIT`] bytes are kept
//! for [`UpstreamImpl::classify`]; neither can hold up or change a chunk.
//! When the body ends — or is dropped because the caller went away — the
//! observation and the classification are sent once as a [`RelayOutcome`].

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use http::{HeaderMap, Method, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyDataStream;
use tokio::sync::oneshot;
use url::Url;

use super::http_client_with;
use super::outbound::strip_hop_by_hop;
use super::{ClientInitError, READ_IDLE_TIMEOUT};
use crate::contract::credential::UpstreamFeedback;
use crate::contract::upstream::{UpstreamImpl, UsageObserver};

/// The error type of the bodies flowing through the forwarding path.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How much of a response body is kept for [`UpstreamImpl::classify`]. An
/// error body that blames the credential or the account says so in its
/// first few hundred bytes.
pub(crate) const CLASSIFY_PREFIX_LIMIT: usize = 16 * 1024;

/// The call did not produce an upstream response.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// The outbound head's target is not a URL the client can send to.
    #[error("outbound target {0:?} is not an absolute http(s) URL")]
    BadTarget(String),
    /// Connecting, sending the request, or waiting for the response head
    /// failed. The request-log outcome `upstream_error`.
    #[error("upstream request failed: {0}")]
    Upstream(String),
}

/// How a relayed body ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEnd {
    /// The upstream's body was relayed to its end.
    Complete,
    /// The upstream connection failed mid-body; the caller's response was
    /// cut off with an error.
    UpstreamFailed(String),
    /// The caller stopped reading (disconnected) before the body ended.
    CallerDisconnected,
}

/// What the data plane learns once a response has been relayed — the input
/// to the request log, `call_completed` and [`CredentialPool::report`].
///
/// [`CredentialPool::report`]: crate::contract::credential::CredentialPool::report
#[derive(Debug, Clone, PartialEq)]
pub struct RelayOutcome {
    pub status: StatusCode,
    /// The upstream's observation ([`UsageObserver::finish`]); `Null` when
    /// there was nothing worth reporting.
    pub usage: serde_json::Value,
    /// [`UpstreamImpl::classify`]'s verdict on the response.
    pub feedback: Option<UpstreamFeedback>,
    pub end: RelayEnd,
}

/// An upstream response on its way to the caller.
#[derive(Debug)]
pub struct Relayed {
    /// Status and headers as the upstream sent them, minus hop-by-hop
    /// headers; the body relayed as it arrives.
    pub response: http::Response<RelayBody>,
    /// Resolves once, when the body has ended or been dropped. The sender
    /// is never dropped without sending.
    pub outcome: oneshot::Receiver<RelayOutcome>,
}

/// How much of a request body is kept so a call that failed on a stale
/// pooled connection can be sent again. Completion requests are JSON
/// documents of a few KiB to a few MiB; a body past this is not retried
/// (and from then on nothing of it is kept), so the cap bounds the memory
/// one call can pin.
const REPLAY_LIMIT: usize = 1024 * 1024;

/// A request body that remembers what it has handed out, up to
/// [`REPLAY_LIMIT`] bytes (each chunk is copied, so the count is the bytes
/// actually retained, not the size of the buffers they came from), so a second attempt can start from the first byte
/// without the caller sending anything twice. The body is still streamed:
/// each chunk goes upstream as it arrives from the caller.
struct ReplayState {
    /// The caller's body, from wherever the last attempt stopped reading.
    inner: ChunkStream,
    captured: Vec<Bytes>,
    captured_len: usize,
    /// More than [`REPLAY_LIMIT`] bytes were read, or the caller's body
    /// failed: this body can no longer be sent again.
    spent: bool,
    /// The caller's body ended, and all of it is in `captured` unless
    /// `spent`: no attempt reads `inner` again.
    complete: bool,
    /// The body's exact length, when the caller declared one. An attempt
    /// framed by `content-length` stops reading at that length, so reaching
    /// it is how the end is seen.
    exact_len: Option<u64>,
}

/// One attempt's view of a [`ReplayState`]: the captured chunks first, then
/// the rest of the caller's body.
struct ReplayAttempt {
    state: Arc<Mutex<ReplayState>>,
    next: usize,
}

impl Stream for ReplayAttempt {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let mut state = this.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(chunk) = state.captured.get(this.next) {
            let chunk = chunk.clone();
            this.next += 1;
            return Poll::Ready(Some(Ok(chunk)));
        }
        if state.complete {
            return Poll::Ready(None);
        }
        let polled = state.inner.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(chunk))) if !state.spent => {
                if state.captured_len + chunk.len() > REPLAY_LIMIT {
                    state.spent = true;
                    state.captured = Vec::new();
                } else {
                    state.captured_len += chunk.len();
                    // A copy, not a clone: a clone of a slice would pin the
                    // whole buffer it came from, so the count would not be
                    // what is retained.
                    state.captured.push(Bytes::copy_from_slice(chunk));
                    this.next += 1;
                    if state.exact_len == Some(state.captured_len as u64) {
                        state.complete = true;
                    }
                }
            }
            Poll::Ready(Some(Err(_))) => state.spent = true,
            Poll::Ready(None) => state.complete = true,
            _ => {}
        }
        polled
    }
}

/// Whether `error` is the signature of a request sent down a connection the
/// upstream had already closed: the connection ended before any response
/// byte arrived (hyper's "connection closed before message completed", or a
/// cancelled dispatch) or was reset under the write.
pub(super) fn is_dead_connection(error: &reqwest::Error) -> bool {
    if error.is_connect() || error.is_timeout() || error.is_builder() || error.is_body() {
        return false;
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
            if hyper.is_incomplete_message() || hyper.is_canceled() {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
            ) {
                return true;
            }
        }
        source = cause.source();
    }
    false
}

/// A transport failure with its causes (reqwest's own text is only "error
/// sending request"), minus the URL: the URL carries the caller's query,
/// which can hold a credential and must reach no log or diagnostic.
pub(super) fn transport_error(error: reqwest::Error) -> String {
    use std::error::Error as _;
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Executes outbound calls. Holds one connection-pooling client; cheap to
/// clone and share.
///
/// ## Stale pooled connections
///
/// The client keeps idle connections for reuse, and an upstream closes idle
/// connections on its own schedule (Node's default is 5 s, load balancers
/// 60 s and up). A request can therefore be written to a connection whose
/// close is already on its way, and fail with no response byte at all. For
/// a transparent proxy that is not an upstream failure, so:
///
/// - the pool drops connections idle for [`POOL_IDLE_TIMEOUT`] (4 s, under
///   the shortest common server keep-alive, so the usual case never
///   arises and the long-lived streaming calls lose nothing), and
/// - a call that fails with a dead-connection error is sent once more on a
///   fresh connection, but only when all of these hold: the failed attempt
///   used a reused connection (no new connection was opened while it ran —
///   a concurrent call opening one makes us not retry, the safe direction);
///   no response byte was seen (`send` returned an error, so the response
///   was never handed to the caller); and the request body can be sent
///   again (none, or at most [`REPLAY_LIMIT`] bytes, kept as it streams
///   through). A fresh connection that fails, a refused connect, a timeout
///   and any failure after the response began are never retried, so an
///   unreachable upstream is attempted exactly once.
///
/// The remaining risk is inherent to HTTP/1.1: an upstream that read the
/// request and then closed without answering looks the same as one that
/// closed before reading it, so that request may reach the upstream twice.
/// That is the same trade-off browsers and Go's `net/http` make.
#[derive(Debug, Clone)]
pub struct Forwarder {
    client: reqwest::Client,
    /// Connections opened by `client`, to tell a reused connection from a
    /// new one.
    connects: Arc<AtomicUsize>,
}

/// How long the pool keeps an idle connection; see [`Forwarder`].
pub(super) const POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

impl Forwarder {
    pub fn new() -> Result<Self, ClientInitError> {
        Forwarder::with_read_idle_timeout(READ_IDLE_TIMEOUT)
    }

    /// A forwarder whose idle bound is `read_idle_timeout`.
    fn with_read_idle_timeout(
        read_idle_timeout: std::time::Duration,
    ) -> Result<Self, ClientInitError> {
        let connects = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connects);
        let client = http_client_with(read_idle_timeout, move |builder| {
            builder.connector_layer(tower::layer::layer_fn(move |inner| CountConnects {
                inner,
                connects: Arc::clone(&counter),
            }))
        })?;
        Ok(Forwarder { client, connects })
    }

    /// Send `outbound` (from [`super::build_outbound`]) with `body` — the
    /// caller's body, streamed through unread — and return the upstream's
    /// response for relaying. `upstream` is the implementation the head was
    /// built with; it observes and classifies the response.
    pub async fn forward<B>(
        &self,
        upstream: Arc<dyn UpstreamImpl>,
        outbound: http::request::Parts,
        body: B,
    ) -> Result<Relayed, ForwardError>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let body = OutboundBody::new(body);
        Ok(self.send(&outbound, &body).await?.relay(upstream))
    }

    /// Send one attempt of `outbound` with `body`, retrying once on a dead
    /// pooled connection, and return the response head with its body still
    /// unread. The same `body` may be sent again with another head (a
    /// credential failover) while [`OutboundBody::replayable`] holds.
    pub async fn send(
        &self,
        outbound: &http::request::Parts,
        body: &OutboundBody,
    ) -> Result<Sent, ForwardError> {
        let target = outbound.uri.to_string();
        let url = Url::parse(&target).map_err(|_| ForwardError::BadTarget(target.clone()))?;
        let method = outbound.method.clone();
        let mut retried = false;
        let response = loop {
            let mut request = self
                .client
                .request(method.clone(), url.clone())
                .headers(outbound.headers.clone());
            if let Some(attempt) = body.attempt() {
                request = request.body(attempt);
            }
            let connects_before = self.connects.load(Ordering::SeqCst);
            match request.send().await {
                Ok(response) => break response,
                Err(e) => {
                    let reused = self.connects.load(Ordering::SeqCst) == connects_before;
                    if !retried && reused && body.replayable() && is_dead_connection(&e) {
                        tracing::debug!(
                            error = %transport_error(e),
                            "pooled upstream connection was closed; retrying once on a new one"
                        );
                        retried = true;
                        continue;
                    }
                    return Err(ForwardError::Upstream(transport_error(e)));
                }
            }
        };

        let status = response.status();
        let mut headers = response.headers().clone();
        strip_hop_by_hop(&mut headers);
        let expected_len = if is_bodiless(&method, status) {
            Some(0)
        } else {
            response.content_length()
        };
        let chunks = response
            .bytes_stream()
            .map_err(|e| BoxError::from(e.without_url()));
        Ok(Sent {
            status,
            headers,
            expected_len,
            read: Vec::new(),
            read_len: 0,
            done: false,
            chunks: Box::pin(chunks),
        })
    }
}

/// A caller's request body on its way upstream, kept (up to
/// [`REPLAY_LIMIT`] bytes) so it can be sent again: on a fresh connection
/// after a dead pooled one, or with another credential after a rejection.
pub struct OutboundBody {
    /// `None` when the request has no body: it keeps having none (no
    /// chunked framing on a GET).
    replay: Option<Arc<Mutex<ReplayState>>>,
}

impl std::fmt::Debug for OutboundBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundBody")
            .field("replayable", &self.replayable())
            .finish()
    }
}

impl OutboundBody {
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let exact_len = body.size_hint().exact();
        let replay = if body.is_end_stream() || exact_len == Some(0) {
            None
        } else {
            let inner: ChunkStream = Box::pin(BodyDataStream::new(body).map_err(Into::into));
            Some(Arc::new(Mutex::new(ReplayState {
                inner,
                captured: Vec::new(),
                captured_len: 0,
                spent: false,
                complete: false,
                exact_len,
            })))
        };
        OutboundBody { replay }
    }

    /// Whether another attempt can send the body from its first byte: there
    /// is none, or no more than [`REPLAY_LIMIT`] bytes of it have been read
    /// and reading it has not failed.
    pub fn replayable(&self) -> bool {
        self.replay
            .as_ref()
            .is_none_or(|s| !s.lock().unwrap_or_else(|e| e.into_inner()).spent)
    }

    /// Whether the body can be sent again while an earlier attempt may still
    /// be reading it (a credential failover: the first upstream may answer
    /// before it has read the whole body, and its connection keeps
    /// reading). That needs the caller's whole body read and within
    /// [`REPLAY_LIMIT`], so no attempt reads the caller's body any more;
    /// a dead connection's retry needs only [`OutboundBody::replayable`],
    /// since the dead attempt reads nothing further.
    pub fn resendable(&self) -> bool {
        self.replay.as_ref().is_none_or(|s| {
            let s = s.lock().unwrap_or_else(|e| e.into_inner());
            s.complete && !s.spent
        })
    }

    /// One attempt's body: what was captured, then the rest of the caller's.
    fn attempt(&self) -> Option<reqwest::Body> {
        self.replay.as_ref().map(|state| {
            reqwest::Body::wrap_stream(ReplayAttempt {
                state: Arc::clone(state),
                next: 0,
            })
        })
    }
}

/// An upstream response whose head has arrived and whose body has not been
/// relayed yet.
pub struct Sent {
    status: StatusCode,
    headers: HeaderMap,
    expected_len: Option<u64>,
    /// Body items already read by [`Sent::verdict`], relayed first.
    read: Vec<Result<Bytes, BoxError>>,
    read_len: usize,
    /// The body ended or failed while being read: nothing follows `read`.
    done: bool,
    chunks: ChunkStream,
}

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sent")
            .field("status", &self.status)
            .finish()
    }
}

impl Sent {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// For a `4xx` response, [`UpstreamImpl::classify`]'s verdict, read
    /// from the first [`CLASSIFY_PREFIX_LIMIT`] bytes of the body (or all of
    /// it, if shorter); `None` for any other status. What is read stays
    /// here and is relayed first if the response is relayed after all.
    pub async fn verdict(&mut self, upstream: &dyn UpstreamImpl) -> Option<UpstreamFeedback> {
        use futures::StreamExt as _;
        if !self.status.is_client_error() {
            return None;
        }
        while !self.done && self.read_len < CLASSIFY_PREFIX_LIMIT {
            match self.chunks.next().await {
                Some(Ok(chunk)) => {
                    self.read_len += chunk.len();
                    self.read.push(Ok(chunk));
                }
                Some(Err(e)) => {
                    self.read.push(Err(e));
                    self.done = true;
                }
                None => self.done = true,
            }
        }
        let mut prefix = Vec::with_capacity(self.read_len.min(CLASSIFY_PREFIX_LIMIT));
        for chunk in self.read.iter().flatten() {
            let room = CLASSIFY_PREFIX_LIMIT - prefix.len();
            prefix.extend_from_slice(&chunk[..room.min(chunk.len())]);
        }
        upstream.classify(self.status, &self.headers, &prefix)
    }

    /// Relay the response to the caller; `upstream` observes and classifies
    /// it on the way.
    pub fn relay(self, upstream: Arc<dyn UpstreamImpl>) -> Relayed {
        use futures::StreamExt as _;
        let read = futures::stream::iter(self.read);
        let chunks: ChunkStream = if self.done {
            Box::pin(read)
        } else {
            Box::pin(read.chain(self.chunks))
        };
        let (tx, rx) = oneshot::channel();
        let body = RelayBody::new(
            chunks,
            upstream,
            self.status,
            &self.headers,
            self.expected_len,
            tx,
        );
        let mut relayed = http::Response::new(body);
        *relayed.status_mut() = self.status;
        *relayed.headers_mut() = self.headers;
        Relayed {
            response: relayed,
            outcome: rx,
        }
    }
}

/// Counts the connections the client opens.
#[derive(Clone)]
struct CountConnects<S> {
    inner: S,
    connects: Arc<AtomicUsize>,
}

impl<S, R> tower::Service<R> for CountConnects<S>
where
    S: tower::Service<R>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.inner.call(request)
    }
}

/// A response to a HEAD request, and 1xx/204/304 responses, carry no body
/// whatever their `content-length` says.
fn is_bodiless(method: &Method, status: StatusCode) -> bool {
    *method == Method::HEAD
        || status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;

/// The body of a relayed response. Implements [`http_body::Body`], so it can
/// be handed to a hyper or axum response as is (`axum::body::Body::new`).
pub struct RelayBody {
    chunks: ChunkStream,
    tap: Option<Tap>,
}

impl std::fmt::Debug for RelayBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayBody")
            .field("finished", &self.tap.is_none())
            .finish()
    }
}

/// What watches the body go by, until it ends.
struct Tap {
    observer: Box<dyn UsageObserver>,
    upstream: Arc<dyn UpstreamImpl>,
    status: StatusCode,
    headers: HeaderMap,
    prefix: Vec<u8>,
    relayed: u64,
    expected_len: Option<u64>,
    outcome: oneshot::Sender<RelayOutcome>,
}

impl Tap {
    fn chunk(&mut self, chunk: &[u8]) {
        self.observer.observe_chunk(chunk);
        let room = CLASSIFY_PREFIX_LIMIT.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&chunk[..room.min(chunk.len())]);
        self.relayed += chunk.len() as u64;
    }

    fn finish(self, end: RelayEnd) {
        let feedback = self
            .upstream
            .classify(self.status, &self.headers, &self.prefix);
        let outcome = RelayOutcome {
            status: self.status,
            usage: self.observer.finish(),
            feedback,
            end,
        };
        // The data plane may have stopped listening; the outcome is then
        // nobody's business.
        let _ = self.outcome.send(outcome);
    }
}

impl RelayBody {
    fn new(
        chunks: ChunkStream,
        upstream: Arc<dyn UpstreamImpl>,
        status: StatusCode,
        headers: &HeaderMap,
        expected_len: Option<u64>,
        outcome: oneshot::Sender<RelayOutcome>,
    ) -> Self {
        let mut observer = upstream.usage_observer();
        observer.observe_head(status, headers);
        RelayBody {
            chunks,
            tap: Some(Tap {
                observer,
                upstream,
                status,
                headers: headers.clone(),
                prefix: Vec::new(),
                relayed: 0,
                expected_len,
                outcome,
            }),
        }
    }

    fn finish(&mut self, end: RelayEnd) {
        if let Some(tap) = self.tap.take() {
            tap.finish(end);
        }
    }
}

impl Body for RelayBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.tap.is_none() {
            return Poll::Ready(None);
        }
        match self.chunks.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(tap) = self.tap.as_mut() {
                    tap.chunk(&chunk);
                }
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            Poll::Ready(Some(Err(e))) => {
                self.finish(RelayEnd::UpstreamFailed(e.to_string()));
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                self.finish(RelayEnd::Complete);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.tap.is_none()
    }
}

impl Drop for RelayBody {
    fn drop(&mut self) {
        let Some(tap) = self.tap.take() else { return };
        // A server may stop polling once it has written as many bytes as
        // `content-length` promised; that is a complete relay, not a
        // disconnect.
        let end = if tap.expected_len == Some(tap.relayed) {
            RelayEnd::Complete
        } else {
            RelayEnd::CallerDisconnected
        };
        tap.finish(end);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use http_body_util::{BodyExt, Full};

    use super::*;
    use crate::contract::credential::{CredentialKind, CredentialMaterial, Secret};
    use crate::contract::identity::{PresentedAt, RequestHead};
    use crate::contract::upstream::UpstreamKind;
    use crate::proxy::testkit::{self, Reply};
    use crate::proxy::{build_outbound, OpenRouterUpstream, Substitution};

    fn openrouter() -> Arc<dyn UpstreamImpl> {
        Arc::new(OpenRouterUpstream)
    }

    fn material() -> CredentialMaterial {
        CredentialMaterial {
            credential_ref: "or".into(),
            upstream: UpstreamKind::OpenRouter,
            kind: CredentialKind::ApiKey,
            secret: Secret::new("sk-or-real"),
            account_id: None,
        }
    }

    fn outbound(
        base: &Url,
        method: Method,
        rest: &str,
        headers: &[(&str, &str)],
    ) -> http::request::Parts {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        map.insert("authorization", "Bearer caller".parse().unwrap());
        let head = RequestHead::new(method, format!("/openrouter{rest}").parse().unwrap(), map);
        let upstream = OpenRouterUpstream;
        build_outbound(
            &head,
            Substitution {
                upstream: &upstream,
                base_url: base,
                rest,
                material: &material(),
                presented_at: Some(&PresentedAt::Header(http::header::AUTHORIZATION)),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn streaming_response_is_not_buffered() {
        // The upstream sends one event and then waits for the test to see
        // that event through the relay before sending the rest. A relay
        // that buffered the body would never deliver the first event, and
        // the timeout fails the test instead of hanging it.
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release = std::sync::Mutex::new(Some(release_rx));
        let upstream = testkit::spawn(move |_req| {
            let release = release.lock().unwrap().take().expect("one request");
            Reply::stream(
                200,
                &[("content-type", "text/event-stream")],
                futures::stream::once(async {
                    Bytes::from_static(b"data: {\"id\":\"1\",\"choices\":[]}\n\n")
                })
                .chain(
                    futures::stream::once(async move {
                        release.await.expect("released");
                        Bytes::from_static(
                            b"data: {\"id\":\"1\",\"model\":\"m\",\"usage\":{\"total_tokens\":3}}\n\ndata: [DONE]\n\n",
                        )
                    }),
                ),
            )
        })
        .await;
        let parts = outbound(&upstream.url, Method::POST, "/v1/chat/completions", &[]);
        let relayed = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}")))
            .await
            .expect("forwarded");
        assert_eq!(relayed.response.status(), 200);
        let mut body = relayed.response.into_body();
        let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("first event relayed before the upstream finished")
            .expect("a frame")
            .expect("ok")
            .into_data()
            .unwrap();
        assert_eq!(&first[..], b"data: {\"id\":\"1\",\"choices\":[]}\n\n");
        release_tx.send(()).unwrap();
        let rest = tokio::time::timeout(Duration::from_secs(5), body.collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(rest.ends_with(b"data: [DONE]\n\n"));
        let outcome = relayed.outcome.await.unwrap();
        assert_eq!(outcome.end, RelayEnd::Complete);
        assert_eq!(outcome.usage["usage"]["total_tokens"], 3);
    }

    #[tokio::test]
    async fn streaming_response_is_not_buffered_through_a_listening_proxy() {
        // End to end over sockets: caller -> test data plane -> slow
        // upstream. Each event is emitted only after the caller has
        // received the previous one, so a relay that held anything back
        // deadlocks into the timeout.
        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let seen_rx = std::sync::Mutex::new(Some(seen_rx));
        let upstream = testkit::spawn(move |_req| {
            let rx = seen_rx.lock().unwrap().take().expect("one request");
            let events = futures::stream::unfold((0u32, rx), |(i, mut rx)| async move {
                if i == 5 {
                    return None;
                }
                if i > 0 {
                    rx.recv().await?;
                }
                Some((Bytes::from(format!("data: {{\"n\":{i}}}\n\n")), (i + 1, rx)))
            });
            Reply::stream(200, &[("content-type", "text/event-stream")], events)
        })
        .await;
        let proxy = testkit::spawn_proxy(openrouter(), upstream.url.clone(), material()).await;

        let response = reqwest::Client::new()
            .post(format!("{}openrouter/v1/chat/completions", proxy.url))
            .header("authorization", "Bearer caller")
            .body("{\"stream\":true}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut stream = response.bytes_stream();
        let mut received = Vec::new();
        let mut buf = Vec::new();
        while received.len() < 5 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("event relayed while the upstream was still waiting")
                .expect("stream open")
                .unwrap();
            buf.extend_from_slice(&chunk);
            while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                received.push(String::from_utf8(buf.drain(..end + 2).collect()).unwrap());
                let _ = seen_tx.send(());
            }
        }
        assert_eq!(received[4], "data: {\"n\":4}\n\n");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn relay_reads_upstream_only_as_fast_as_the_caller() {
        // No read-ahead: one poll of the relay body pulls exactly one chunk
        // from the upstream stream, so back-pressure from a slow caller
        // reaches the upstream connection.
        let pulled = Arc::new(AtomicUsize::new(0));
        let counter = pulled.clone();
        let chunks = futures::stream::iter(0..1000).map(move |i| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, BoxError>(Bytes::from(format!("chunk {i};")))
        });
        let (tx, rx) = oneshot::channel();
        let mut body = RelayBody::new(
            Box::pin(chunks),
            openrouter(),
            StatusCode::OK,
            &HeaderMap::new(),
            None,
            tx,
        );
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(&first[..], b"chunk 0;");
        assert_eq!(pulled.load(Ordering::SeqCst), 1);
        body.frame().await.unwrap().unwrap();
        assert_eq!(pulled.load(Ordering::SeqCst), 2);
        // The caller goes away mid-body.
        drop(body);
        assert_eq!(rx.await.unwrap().end, RelayEnd::CallerDisconnected);
        assert_eq!(pulled.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn response_relayed_unmodified() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
        let sent = payload.clone();
        let upstream = testkit::spawn(move |_req| {
            Reply::bytes(
                418,
                &[
                    ("content-type", "application/octet-stream"),
                    ("x-upstream-header", "kept"),
                    ("set-cookie", "a=1"),
                    ("set-cookie", "b=2"),
                    ("content-encoding", "gzip"),
                ],
                sent.clone(),
            )
        })
        .await;
        let parts = outbound(&upstream.url, Method::GET, "/v1/blob", &[]);
        let relayed = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::new()))
            .await
            .unwrap();
        let (head, body) = relayed.response.into_parts();
        assert_eq!(head.status.as_u16(), 418);
        assert_eq!(head.headers["x-upstream-header"], "kept");
        assert_eq!(head.headers["content-encoding"], "gzip");
        let cookies: Vec<_> = head.headers.get_all("set-cookie").iter().collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert_eq!(head.headers["content-length"], "200000");
        let got = body.collect().await.unwrap().to_bytes();
        assert_eq!(&got[..], &payload[..]);
        let outcome = relayed.outcome.await.unwrap();
        assert_eq!(outcome.end, RelayEnd::Complete);
        assert_eq!(outcome.usage, serde_json::Value::Null);
        assert_eq!(outcome.feedback, None);
    }

    #[tokio::test]
    async fn request_body_and_target_reach_the_upstream_intact() {
        let upstream = testkit::spawn(|_req| Reply::bytes(200, &[], b"ok".to_vec())).await;
        // A streamed (chunked, length-unknown) request body is relayed as
        // the same bytes.
        let parts = outbound(
            &upstream.url,
            Method::POST,
            "/v1/chat/completions?x=1",
            &[
                ("content-type", "application/json"),
                ("x-sandbox-secret", "abc"),
            ],
        );
        let body = http_body_util::StreamBody::new(futures::stream::iter(
            ["{\"a\":", "1}"].map(|s| Ok::<_, BoxError>(Frame::data(Bytes::from(s)))),
        ));
        let relayed = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, body)
            .await
            .unwrap();
        relayed.response.into_body().collect().await.unwrap();
        let got = upstream.received();
        assert_eq!(got.len(), 1);
        let req = &got[0];
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.target, "/v1/chat/completions?x=1");
        assert_eq!(&req.body[..], b"{\"a\":1}");
        assert_eq!(req.headers["authorization"], "Bearer sk-or-real");
        assert_eq!(req.headers["content-type"], "application/json");
        assert!(req.headers.get("x-sandbox-secret").is_none());

        // A GET with no body goes out with no body framing at all.
        let parts = outbound(&upstream.url, Method::GET, "/v1/models", &[]);
        Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::new()))
            .await
            .unwrap();
        let got = upstream.received();
        let req = &got[1];
        assert!(
            req.headers.get("transfer-encoding").is_none(),
            "{:?}",
            req.headers
        );
        assert!(req.body.is_empty());
    }

    #[tokio::test]
    async fn redirects_are_relayed_not_followed() {
        // Every redirect status, including the ones a client follows even
        // for a POST with a streamed body (301/302/303 turn it into a GET),
        // pointing at a second live server that must never hear of it.
        for status in [301u16, 302, 303, 307, 308] {
            let elsewhere = testkit::spawn(|_req| Reply::bytes(200, &[], b"stolen".to_vec())).await;
            let location = elsewhere.url.join("steal").unwrap().to_string();
            let sent = location.clone();
            let upstream = testkit::spawn(move |_req| {
                Reply::bytes(status, &[("location", sent.as_str())], Vec::new())
            })
            .await;
            let parts = outbound(&upstream.url, Method::POST, "/v1/x", &[]);
            let relayed = Forwarder::new()
                .unwrap()
                .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}")))
                .await
                .unwrap();
            assert_eq!(relayed.response.status(), status);
            assert_eq!(relayed.response.headers()["location"], location.as_str());
            assert_eq!(upstream.received().len(), 1);
            assert!(elsewhere.received().is_empty(), "{status} was followed");
        }
    }

    #[tokio::test]
    async fn hop_by_hop_headers_stripped_from_the_forwarded_response() {
        // A streamed (chunked) upstream response: its framing headers are
        // the upstream connection's, not the caller's.
        let upstream = testkit::spawn(|_req| {
            Reply::stream(
                200,
                &[
                    ("content-type", "text/event-stream"),
                    ("keep-alive", "timeout=5"),
                    ("x-kept", "1"),
                ],
                futures::stream::iter([Bytes::from_static(b"data: {}\n\n")]),
            )
        })
        .await;
        let parts = outbound(&upstream.url, Method::POST, "/v1/x", &[]);
        let relayed = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}")))
            .await
            .unwrap();
        let headers = relayed.response.headers();
        assert!(headers.get("transfer-encoding").is_none(), "{headers:?}");
        assert!(headers.get("keep-alive").is_none(), "{headers:?}");
        assert_eq!(headers["x-kept"], "1");
        assert_eq!(headers["content-type"], "text/event-stream");
    }

    #[tokio::test]
    async fn an_empty_request_body_is_sent_with_no_body_framing() {
        // Any method: a POST with an empty body is not turned into a
        // chunked one.
        let upstream = testkit::spawn(|_req| Reply::bytes(200, &[], Vec::new())).await;
        let parts = outbound(&upstream.url, Method::POST, "/v1/x", &[]);
        Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::new()))
            .await
            .unwrap();
        let req = &upstream.received()[0];
        assert!(
            req.headers.get("transfer-encoding").is_none(),
            "{:?}",
            req.headers
        );
        assert!(req.body.is_empty());
    }

    #[tokio::test]
    async fn classify_sees_at_most_the_prefix_limit() {
        // A long error body is relayed whole, but only its first
        // CLASSIFY_PREFIX_LIMIT bytes are held for the verdict.
        let probe = Arc::new(testkit::Probe::new(openrouter()));
        let chunks = futures::stream::iter(
            (0..10).map(|_| Ok::<_, BoxError>(Bytes::from(vec![b'x'; 5000]))),
        );
        let (tx, rx) = oneshot::channel();
        let body = RelayBody::new(
            Box::pin(chunks),
            probe.clone(),
            StatusCode::UNAUTHORIZED,
            &HeaderMap::new(),
            None,
            tx,
        );
        let relayed = body.collect().await.unwrap().to_bytes();
        assert_eq!(relayed.len(), 50_000);
        assert_eq!(rx.await.unwrap().end, RelayEnd::Complete);
        assert_eq!(*probe.classified.lock().unwrap(), [CLASSIFY_PREFIX_LIMIT]);
    }

    #[tokio::test]
    async fn a_body_dropped_after_its_promised_length_is_complete() {
        // A server stops polling once content-length bytes are written: a
        // complete relay. One byte short is a disconnect.
        for (relay_chunks, want) in [(2, RelayEnd::Complete), (1, RelayEnd::CallerDisconnected)] {
            let chunks = futures::stream::iter(
                [b"hello ".as_slice(), b"world"].map(|c| Ok::<_, BoxError>(Bytes::from_static(c))),
            )
            .chain(futures::stream::pending());
            let (tx, rx) = oneshot::channel();
            let mut body = RelayBody::new(
                Box::pin(chunks),
                openrouter(),
                StatusCode::OK,
                &HeaderMap::new(),
                Some(11),
                tx,
            );
            for _ in 0..relay_chunks {
                body.frame().await.unwrap().unwrap();
            }
            drop(body);
            assert_eq!(rx.await.unwrap().end, want);
        }
    }

    #[tokio::test]
    async fn an_upstream_silent_before_its_response_head_times_out() {
        // Accepts the connection, reads the request, never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let silent = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut sink = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut socket, &mut sink).await;
        });
        let parts = outbound(&base, Method::POST, "/v1/x", &[]);
        let forwarded = tokio::time::timeout(
            Duration::from_secs(5),
            Forwarder::with_read_idle_timeout(Duration::from_millis(200))
                .unwrap()
                .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}"))),
        )
        .await
        .expect("a stalled upstream ends the call");
        assert!(matches!(forwarded, Err(ForwardError::Upstream(_))));
        silent.abort();
    }

    #[tokio::test]
    async fn an_upstream_that_stalls_mid_body_times_out() {
        let upstream = testkit::spawn(|_req| {
            Reply::stream(
                200,
                &[("content-type", "text/event-stream")],
                futures::stream::once(async { Bytes::from_static(b"data: {}\n\n") })
                    .chain(futures::stream::pending()),
            )
        })
        .await;
        let parts = outbound(&upstream.url, Method::POST, "/v1/x", &[]);
        let relayed = Forwarder::with_read_idle_timeout(Duration::from_millis(200))
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}")))
            .await
            .unwrap();
        let mut body = relayed.response.into_body();
        body.frame().await.unwrap().unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("a stalled body ends");
        assert!(matches!(next, Some(Err(_))), "{next:?}");
        assert!(matches!(
            relayed.outcome.await.unwrap().end,
            RelayEnd::UpstreamFailed(_)
        ));
    }

    #[tokio::test]
    async fn unreachable_upstream_is_a_forward_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let parts = outbound(&base, Method::POST, "/v1/x", &[]);
        let err = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::new()))
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Upstream(_)), "{err}");
        assert!(!err.to_string().contains("sk-or-real"));
    }

    /// Reads one request (head and `content-length` body) from `socket`;
    /// `None` when the peer closed first.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let chunked = head.contains("transfer-encoding: chunked");
                if (chunked && buf.ends_with(b"\r\n0\r\n\r\n"))
                    || (!chunked && buf.len() >= end + 4 + len)
                {
                    return Some(buf);
                }
            }
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    const OK_KEEP_ALIVE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

    /// A raw-socket upstream: `script(connection_number, socket)` runs per
    /// accepted connection. Returns the base URL and the accept count.
    async fn raw_upstream<F, Fut>(script: F) -> (Url, Arc<AtomicUsize>)
    where
        F: Fn(usize, tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::spawn(script(n, socket));
            }
        });
        (base, accepted)
    }

    async fn post_to(
        forwarder: &Forwarder,
        base: &Url,
        body: Bytes,
    ) -> Result<Relayed, ForwardError> {
        let parts = outbound(base, Method::POST, "/v1/x", &[]);
        forwarder
            .forward(openrouter(), parts, Full::new(body))
            .await
    }

    #[tokio::test]
    async fn a_request_on_a_connection_the_upstream_closed_is_sent_again_on_a_new_one() {
        use tokio::io::AsyncWriteExt;
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&bodies);
        let (base, accepted) = raw_upstream(move |n, mut socket| {
            let seen = Arc::clone(&seen);
            async move {
                while let Some(request) = read_request(&mut socket).await {
                    let text = String::from_utf8_lossy(&request).into_owned();
                    seen.lock().unwrap().push(text);
                    // Connection 1 answers once, then closes on the next
                    // request without reading it through to an answer: the
                    // idle-close race, made deterministic.
                    if n == 1 && seen.lock().unwrap().len() > 1 {
                        return;
                    }
                    socket.write_all(OK_KEEP_ALIVE).await.unwrap();
                }
            }
        })
        .await;
        let forwarder = Forwarder::new().unwrap();
        for body in ["{\"a\":1}", "{\"b\":2}"] {
            let relayed = post_to(&forwarder, &base, Bytes::from(body)).await.unwrap();
            assert_eq!(relayed.response.status(), 200);
            let collected = relayed.response.into_body().collect().await.unwrap();
            assert_eq!(collected.to_bytes(), "ok");
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        // The retried request carried the whole body.
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        for (request, body) in bodies.iter().zip(["{\"a\":1}", "{\"b\":2}", "{\"b\":2}"]) {
            assert!(request.contains(body), "{request:?} lacks {body}");
        }
    }

    #[tokio::test]
    async fn a_request_over_the_replay_limit_is_not_sent_again() {
        use tokio::io::AsyncWriteExt;
        let (base, accepted) = raw_upstream(|n, mut socket| async move {
            if read_request(&mut socket).await.is_some() && n == 1 {
                socket.write_all(OK_KEEP_ALIVE).await.unwrap();
                // Next request: close without answering.
                let _ = read_request(&mut socket).await;
            }
        })
        .await;
        let forwarder = Forwarder::new().unwrap();
        let first = post_to(&forwarder, &base, Bytes::from_static(b"{}"))
            .await
            .unwrap();
        first.response.into_body().collect().await.unwrap();
        let big = Bytes::from(vec![b'x'; REPLAY_LIMIT + 1]);
        let err = post_to(&forwarder, &base, big).await.unwrap_err();
        assert!(matches!(err, ForwardError::Upstream(_)), "{err}");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_is_sent_again_once_the_response_has_begun() {
        use tokio::io::AsyncWriteExt;
        let (base, accepted) = raw_upstream(|_, mut socket| async move {
            if read_request(&mut socket).await.is_some() {
                // A head promising ten bytes, three of them, then gone.
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
                    .await
                    .unwrap();
            }
        })
        .await;
        let forwarder = Forwarder::new().unwrap();
        let relayed = post_to(&forwarder, &base, Bytes::from_static(b"{}"))
            .await
            .unwrap();
        let mut body = relayed.response.into_body();
        let mut got = Vec::new();
        let mut failed = false;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => got.extend(frame.into_data().unwrap_or_default()),
                Err(_) => failed = true,
            }
        }
        assert!(failed, "the cut-off body must surface as an error");
        assert_eq!(got, b"abc");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        assert!(matches!(
            relayed.outcome.await.unwrap().end,
            RelayEnd::UpstreamFailed(_)
        ));
    }

    #[tokio::test]
    async fn an_error_that_is_not_a_dead_connection_is_not_sent_again() {
        use tokio::io::AsyncWriteExt;
        // Connection 1 answers once, then replies to the next request with
        // garbage: an upstream that did process it. Any later connection
        // would answer fine, so a retry would hide the failure.
        let (base, accepted) = raw_upstream(|n, mut socket| async move {
            let mut served = 0;
            while read_request(&mut socket).await.is_some() {
                served += 1;
                let reply: &[u8] = if n == 1 && served > 1 {
                    b"this is not http\r\n\r\n"
                } else {
                    OK_KEEP_ALIVE
                };
                socket.write_all(reply).await.unwrap();
            }
        })
        .await;
        let forwarder = Forwarder::new().unwrap();
        let first = post_to(&forwarder, &base, Bytes::from_static(b"{}"))
            .await
            .unwrap();
        first.response.into_body().collect().await.unwrap();
        let err = post_to(&forwarder, &base, Bytes::from_static(b"{}"))
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Upstream(_)), "{err}");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_upstream_error_text_never_carries_the_request_query() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let parts = outbound(&base, Method::POST, "/v1/x?key=sk-secret-query", &[]);
        let err = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::new()))
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Upstream(_)), "{err}");
        let text = format!("{err} {err:?}");
        assert!(!text.contains("sk-secret-query"), "{text}");
        assert!(!text.contains("/v1/x"), "{text}");
    }

    #[tokio::test]
    async fn an_upstream_that_drops_every_connection_is_tried_once() {
        // Not a stale connection: every connection is new, so a failure is
        // the upstream's and is reported after a single attempt.
        let (base, accepted) = raw_upstream(|_, socket| async move { drop(socket) }).await;
        let err = post_to(&Forwarder::new().unwrap(), &base, Bytes::from_static(b"{}"))
            .await
            .unwrap_err();
        assert!(matches!(err, ForwardError::Upstream(_)), "{err}");
        assert!(!err.to_string().contains("sk-or-real"));
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upstream_rejection_detected_from_the_relayed_response() {
        let upstream = testkit::spawn(|_req| {
            Reply::bytes(
                401,
                &[("content-type", "application/json")],
                br#"{"error":{"message":"No auth credentials found","code":401}}"#.to_vec(),
            )
        })
        .await;
        let parts = outbound(&upstream.url, Method::POST, "/v1/chat/completions", &[]);
        let relayed = Forwarder::new()
            .unwrap()
            .forward(openrouter(), parts, Full::new(Bytes::from_static(b"{}")))
            .await
            .unwrap();
        assert_eq!(relayed.response.status(), 401);
        let body = relayed
            .response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert!(body.starts_with(b"{\"error\""));
        let outcome = relayed.outcome.await.unwrap();
        assert!(
            matches!(&outcome.feedback, Some(UpstreamFeedback::CredentialInvalid { reason }) if reason.contains("401")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_verdict_is_read_from_a_4xx_and_the_buffered_body_still_relays_intact() {
        // Longer than the classify prefix, so the relay must join what the
        // verdict read with what it did not.
        let mut long = br#"{"error":{"message":"rate limited","code":429},"pad":""#.to_vec();
        long.extend(std::iter::repeat_n(b'p', CLASSIFY_PREFIX_LIMIT * 2));
        long.extend_from_slice(b"\"}");
        let expected = long.clone();
        let upstream =
            testkit::spawn(move |_req| Reply::bytes(429, &[("retry-after", "30")], long.clone()))
                .await;
        let forwarder = Forwarder::new().unwrap();
        let parts = outbound(&upstream.url, Method::POST, "/v1/chat/completions", &[]);
        let body = OutboundBody::new(Full::new(Bytes::from_static(b"{}")));
        let mut sent = forwarder.send(&parts, &body).await.unwrap();
        assert_eq!(sent.status(), 429);
        let verdict = sent.verdict(openrouter().as_ref()).await;
        assert!(
            matches!(
                verdict,
                Some(UpstreamFeedback::RateLimited { until: Some(_) })
            ),
            "{verdict:?}"
        );
        let relayed = sent.relay(openrouter());
        assert_eq!(relayed.response.status(), 429);
        let got = relayed
            .response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(got.len(), expected.len());
        assert_eq!(&got[..], &expected[..]);
    }

    #[tokio::test]
    async fn no_verdict_is_read_from_a_success_or_a_server_error() {
        for status in [200u16, 500] {
            let upstream = testkit::spawn(move |_req| {
                Reply::bytes(status, &[], br#"{"error":{"code":401}}"#.to_vec())
            })
            .await;
            let parts = outbound(&upstream.url, Method::POST, "/v1/x", &[]);
            let body = OutboundBody::new(Full::new(Bytes::from_static(b"{}")));
            let mut sent = Forwarder::new().unwrap().send(&parts, &body).await.unwrap();
            assert_eq!(sent.verdict(openrouter().as_ref()).await, None, "{status}");
            let got = sent
                .relay(openrouter())
                .response
                .into_body()
                .collect()
                .await;
            assert_eq!(got.unwrap().to_bytes(), &br#"{"error":{"code":401}}"#[..]);
        }
    }

    #[tokio::test]
    async fn a_body_read_to_its_end_is_resent_whole_to_another_upstream() {
        let first = testkit::spawn(|_req| Reply::bytes(401, &[], b"{}".to_vec())).await;
        let second = testkit::spawn(|_req| Reply::bytes(200, &[], b"ok".to_vec())).await;
        let forwarder = Forwarder::new().unwrap();
        let payload = Bytes::from(vec![b'b'; 64 * 1024]);
        let len = payload.len().to_string();
        // The caller declared its length: the attempt is framed by it, and
        // the body still counts as read to its end.
        let headers = [("content-length", len.as_str())];
        let body = OutboundBody::new(Full::new(payload.clone()));
        assert!(body.replayable());
        assert!(!body.resendable(), "nothing read yet");
        let sent = forwarder
            .send(
                &outbound(&first.url, Method::POST, "/v1/x", &headers),
                &body,
            )
            .await
            .unwrap();
        assert_eq!(sent.status(), 401);
        assert!(body.resendable());
        let sent = forwarder
            .send(
                &outbound(&second.url, Method::POST, "/v1/x", &headers),
                &body,
            )
            .await
            .unwrap();
        assert_eq!(sent.status(), 200);
        for server in [&first, &second] {
            let received = server.received();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].body, payload);
        }
    }

    #[tokio::test]
    async fn a_chunked_body_of_unknown_length_is_resendable_once_it_ended() {
        let first = testkit::spawn(|_req| Reply::bytes(429, &[], b"{}".to_vec())).await;
        let second = testkit::spawn(|_req| Reply::bytes(200, &[], b"ok".to_vec())).await;
        let forwarder = Forwarder::new().unwrap();
        let chunks =
            futures::stream::iter([Bytes::from_static(b"{\"a\":"), Bytes::from_static(b"1}")])
                .map(|b| Ok::<_, std::convert::Infallible>(http_body::Frame::data(b)));
        let body = OutboundBody::new(http_body_util::StreamBody::new(chunks));
        forwarder
            .send(&outbound(&first.url, Method::POST, "/v1/x", &[]), &body)
            .await
            .unwrap();
        assert!(body.resendable());
        forwarder
            .send(&outbound(&second.url, Method::POST, "/v1/x", &[]), &body)
            .await
            .unwrap();
        assert_eq!(second.received()[0].body, &b"{\"a\":1}"[..]);
        assert_eq!(first.received()[0].body, second.received()[0].body);
    }

    #[tokio::test]
    async fn a_body_over_the_replay_limit_or_without_one_is_judged_correctly() {
        let upstream = testkit::spawn(|_req| Reply::bytes(429, &[], b"{}".to_vec())).await;
        let forwarder = Forwarder::new().unwrap();
        let big = OutboundBody::new(Full::new(Bytes::from(vec![b'x'; REPLAY_LIMIT + 1])));
        forwarder
            .send(&outbound(&upstream.url, Method::POST, "/v1/x", &[]), &big)
            .await
            .unwrap();
        assert!(!big.replayable());
        assert!(!big.resendable());
        // No body: always sendable again.
        let none = OutboundBody::new(Full::new(Bytes::new()));
        assert!(none.replayable() && none.resendable());
    }
}
