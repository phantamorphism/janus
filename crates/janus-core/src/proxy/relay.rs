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
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use http::{HeaderMap, Method, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyDataStream;
use tokio::sync::oneshot;
use url::Url;

#[cfg(test)]
use super::http_client_with;
use super::outbound::strip_hop_by_hop;
use super::{http_client, ClientInitError};
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

/// Executes outbound calls. Holds one connection-pooling client; cheap to
/// clone and share.
#[derive(Debug, Clone)]
pub struct Forwarder {
    client: reqwest::Client,
}

impl Forwarder {
    pub fn new() -> Result<Self, ClientInitError> {
        Ok(Forwarder {
            client: http_client()?,
        })
    }

    /// A forwarder whose idle bound is `read_idle_timeout`, so a test can
    /// see a stall end without waiting the real one out.
    #[cfg(test)]
    fn with_read_idle_timeout(read_idle_timeout: std::time::Duration) -> Self {
        Forwarder {
            client: http_client_with(read_idle_timeout).unwrap(),
        }
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
        let target = outbound.uri.to_string();
        let url = Url::parse(&target).map_err(|_| ForwardError::BadTarget(target.clone()))?;
        let method = outbound.method;
        let mut request = self
            .client
            .request(method.clone(), url)
            .headers(outbound.headers);
        // A request that has no body keeps having none (no chunked framing
        // on a GET); any other body is streamed through without buffering.
        if !(body.is_end_stream() || body.size_hint().exact() == Some(0)) {
            request = request.body(reqwest::Body::wrap_stream(BodyDataStream::new(body)));
        }
        let response = request
            .send()
            .await
            .map_err(|e| ForwardError::Upstream(e.without_url().to_string()))?;

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
        let (tx, rx) = oneshot::channel();
        let body = RelayBody::new(
            Box::pin(chunks),
            upstream,
            status,
            &headers,
            expected_len,
            tx,
        );
        let mut relayed = http::Response::new(body);
        *relayed.status_mut() = status;
        *relayed.headers_mut() = headers;
        Ok(Relayed {
            response: relayed,
            outcome: rx,
        })
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            Forwarder::with_read_idle_timeout(Duration::from_millis(200)).forward(
                openrouter(),
                parts,
                Full::new(Bytes::from_static(b"{}")),
            ),
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
}
