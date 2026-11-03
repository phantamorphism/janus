//! A real HTTP upstream on a loopback port (hyper), recording every request
//! it receives and answering with a scripted response: fixed, streamed in
//! gated chunks, or streamed until the reader goes away.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// One request as the upstream saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: http::Method,
    /// Path and query as received.
    pub target: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// What the upstream answers.
#[derive(Clone)]
pub enum Script {
    Fixed {
        status: StatusCode,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
    },
    /// Send `first`, wait for `gate`, then send `rest` and end.
    Gated {
        first: &'static str,
        gate: Arc<Notify>,
        rest: &'static str,
    },
    /// Send `chunk` every 20 ms until the reader goes away.
    Endless { chunk: &'static str },
}

pub struct Upstream {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<Mutex<Script>>,
    connections: Arc<AtomicUsize>,
    /// An `Endless` stream noticed its reader was gone.
    pub stream_abandoned: Arc<AtomicBool>,
}

impl Upstream {
    pub async fn start(script: Script) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let upstream = Upstream {
            addr,
            seen: Arc::default(),
            script: Arc::new(Mutex::new(script)),
            connections: Arc::default(),
            stream_abandoned: Arc::default(),
        };
        let seen = Arc::clone(&upstream.seen);
        let script = Arc::clone(&upstream.script);
        let connections = Arc::clone(&upstream.connections);
        let abandoned = Arc::clone(&upstream.stream_abandoned);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                let seen = Arc::clone(&seen);
                let script = Arc::clone(&script);
                let abandoned = Arc::clone(&abandoned);
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let seen = Arc::clone(&seen);
                    let script = script.lock().expect("script").clone();
                    let abandoned = Arc::clone(&abandoned);
                    async move {
                        let (parts, body) = req.into_parts();
                        let body = body
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        seen.lock().expect("seen").push(Seen {
                            method: parts.method,
                            target: parts
                                .uri
                                .path_and_query()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            headers: parts.headers,
                            body,
                        });
                        Ok::<_, Infallible>(respond(script, abandoned))
                    }
                });
                tokio::spawn(async move {
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        upstream
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_script(&self, script: Script) {
        *self.script.lock().expect("script") = script;
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }

    /// TCP connections accepted, whether or not a request followed.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

fn respond(script: Script, abandoned: Arc<AtomicBool>) -> Response<BoxBody> {
    match script {
        Script::Fixed {
            status,
            headers,
            body,
        } => {
            let mut response =
                Response::new(Full::new(Bytes::from_static(body.as_bytes())).boxed());
            *response.status_mut() = status;
            for (name, value) in headers {
                response
                    .headers_mut()
                    .insert(name, value.parse().expect("header value"));
            }
            response
        }
        Script::Gated { first, gate, rest } => {
            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(4);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(Frame::data(Bytes::from_static(first.as_bytes()))))
                    .await;
                gate.notified().await;
                let _ = tx
                    .send(Ok(Frame::data(Bytes::from_static(rest.as_bytes()))))
                    .await;
            });
            streaming(rx)
        }
        Script::Endless { chunk } => {
            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(1);
            tokio::spawn(async move {
                loop {
                    if tx
                        .send(Ok(Frame::data(Bytes::from_static(chunk.as_bytes()))))
                        .await
                        .is_err()
                    {
                        abandoned.store(true, Ordering::SeqCst);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            streaming(rx)
        }
    }
}

fn streaming(rx: mpsc::Receiver<Result<Frame<Bytes>, Infallible>>) -> Response<BoxBody> {
    let mut response = Response::new(BodyExt::boxed(StreamBody::new(ReceiverStream::new(rx))));
    response
        .headers_mut()
        .insert("content-type", "text/event-stream".parse().expect("value"));
    response
}

/// A loopback address nothing listens on.
pub async fn closed_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("addr")
}
