//! Fixtures for the admin-surface integration tests: a real data directory,
//! the real `FilePool`, `Store` and `PluginHandle`, the admin surface started
//! through `janus_daemon::admin::start`, and HTTP clients for its unix socket
//! and TCP listeners.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use janus_core::contract::binding::Method;
use janus_core::contract::credential::CredentialPool;
use janus_core::contract::plugin::JanusPlugin;
use janus_core::contract::request_log::{Outcome, RequestRecord};
use janus_core::contract::upstream::{UpstreamImpl, UpstreamKind};
use janus_core::pool::{pool_dir, ConfiguredCredential, ConfiguredSource, FilePool, PoolOptions};
use janus_core::proxy::{AnthropicUpstream, OpenAiUpstream, OpenRouterUpstream};
use janus_daemon::admin::{self, AdminDeps, AdminError, AdminHandle};
use janus_daemon::contract::config::MaterialSource;
use janus_daemon::plugin::PluginHandle;
use janus_daemon::store::Store;
use janus_daemon::DaemonConfig;
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use url::Url;

/// A port nothing listens on (bound, read and released).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Whether anything accepts TCP connections on `addr`.
pub fn accepts(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Send `request` (a whole HTTP/1.1 request, verbatim) to `addr` and read
/// until the server closes: what came back, or `None` if the server closed
/// without answering. Paths go out byte for byte, unnormalised.
pub async fn raw(addr: SocketAddr, request: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut reply = Vec::new();
    match tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut reply)).await {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => panic!("{addr}: no answer and no close"),
    }
    (!reply.is_empty()).then(|| String::from_utf8_lossy(&reply).into_owned())
}

/// Write `contents` to `path` with `mode`.
pub fn write_file(path: &Path, contents: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// How the daemon under test is put together.
pub struct Builder {
    /// `None` when another daemon owns the directory ([`Builder::sharing`]).
    dir: Option<TempDir>,
    path: PathBuf,
    data_plane: SocketAddr,
    /// What `AdminDeps::data_plane_addr` reports; `data_plane` by default.
    data_plane_bound: Option<SocketAddr>,
    toml: String,
    linked: Option<Arc<dyn JanusPlugin>>,
    anthropic_token_url: Option<Url>,
}

impl Builder {
    /// A daemon whose config is `[data_plane]` plus `extra`. In `extra`,
    /// `{dir}` is the data directory and `{data_plane}` the data-plane
    /// address.
    pub fn new(extra: &str) -> Self {
        Builder::in_dir(tempfile::tempdir().expect("tempdir"), extra)
    }

    /// The same, over an existing data directory (a restart).
    pub fn in_dir(dir: TempDir, extra: &str) -> Self {
        let path = dir.path().to_path_buf();
        Builder::over(Some(dir), path, extra)
    }

    /// A second daemon over a directory another `Daemon` owns.
    pub fn sharing(path: &Path, extra: &str) -> Self {
        Builder::over(None, path.to_path_buf(), extra)
    }

    fn over(dir: Option<TempDir>, path: PathBuf, extra: &str) -> Self {
        let data_plane: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().expect("addr");
        let extra = extra
            .replace("{dir}", &path.display().to_string())
            .replace("{data_plane}", &data_plane.to_string());
        let toml = format!("[data_plane]\nbind = \"{data_plane}\"\n\n{extra}\n");
        Builder {
            dir,
            path,
            data_plane,
            data_plane_bound: None,
            toml,
            linked: None,
            anthropic_token_url: None,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.path
    }

    pub fn data_plane(&self) -> SocketAddr {
        self.data_plane
    }

    /// The config this builder starts with.
    pub fn config(&self) -> DaemonConfig {
        toml::from_str(&self.toml).expect("config parses")
    }

    /// The address the data plane "actually bound", when it differs from
    /// `data_plane.bind` (as with port `0`).
    pub fn data_plane_bound(mut self, addr: SocketAddr) -> Self {
        self.data_plane_bound = Some(addr);
        self
    }

    pub fn linked(mut self, plugin: Arc<dyn JanusPlugin>) -> Self {
        self.linked = Some(plugin);
        self
    }

    pub fn anthropic_token_url(mut self, url: Url) -> Self {
        self.anthropic_token_url = Some(url);
        self
    }

    /// Build every component and call `admin::start`. On a refusal the data
    /// directory comes back with the error.
    pub async fn try_start(self) -> Result<Daemon, (AdminError, Option<TempDir>)> {
        // Parsed without `validate`, as a caller that skipped it would: the
        // admin surface must enforce its listener rules itself.
        let config = Arc::new(self.config());
        let data_dir = self.path.clone();
        let mut anthropic = AnthropicUpstream::new().expect("client");
        if let Some(url) = self.anthropic_token_url {
            anthropic = anthropic.with_token_url(url);
        }
        let upstreams: Vec<Arc<dyn UpstreamImpl>> = vec![
            Arc::new(anthropic),
            Arc::new(OpenAiUpstream::new().expect("client")),
            Arc::new(OpenRouterUpstream),
        ];
        let configured = config
            .credentials
            .iter()
            .map(|c| ConfiguredCredential {
                credential_ref: c.credential_ref.clone(),
                upstream: c.upstream,
                label: c.label.clone(),
                tags: janus_core::pool::normalize_tags(c.tags.clone()),
                source: match c.source().expect("credential source") {
                    MaterialSource::Env(var) => ConfiguredSource::Env(var),
                    MaterialSource::File(path) => ConfiguredSource::File(path),
                },
            })
            .collect();
        let pool = Arc::new(
            FilePool::open(PoolOptions {
                pool_dir: pool_dir(&data_dir),
                upstreams,
                configured,
            })
            .expect("pool opens"),
        );
        let store = Store::open(&data_dir, &config.request_log).expect("store opens");
        let plugin = PluginHandle::from_config(config.plugin.as_ref(), self.linked)
            .expect("plugin client")
            .map(Arc::new);
        let cancel = CancellationToken::new();
        let deps = AdminDeps {
            config: Arc::clone(&config),
            data_dir,
            pool: Arc::clone(&pool) as Arc<dyn CredentialPool>,
            registry: Arc::clone(store.registry()),
            requests: Arc::clone(store.requests()),
            selections: config
                .upstreams
                .iter()
                .map(|u| Arc::new(u.selection()))
                .collect(),
            plugin,
            started_at: SystemTime::now(),
            data_plane_addr: self.data_plane_bound.unwrap_or(self.data_plane),
            data_plane_refused: Arc::default(),
        };
        match admin::start(deps, cancel.clone()).await {
            Ok(handle) => Ok(Daemon {
                dir: self.dir,
                path: self.path,
                data_plane: self.data_plane,
                pool,
                store,
                cancel,
                handle: Some(handle),
            }),
            Err(e) => Err((e, self.dir)),
        }
    }

    pub async fn start(self) -> Daemon {
        match self.try_start().await {
            Ok(d) => d,
            Err((e, _)) => panic!("admin surface refused to start: {e}"),
        }
    }
}

/// A running admin surface over real components.
pub struct Daemon {
    dir: Option<TempDir>,
    path: PathBuf,
    pub data_plane: SocketAddr,
    pub pool: Arc<FilePool>,
    pub store: Store,
    pub cancel: CancellationToken,
    handle: Option<AdminHandle>,
}

impl Daemon {
    pub fn data_dir(&self) -> &Path {
        &self.path
    }

    pub fn handle(&self) -> &AdminHandle {
        self.handle.as_ref().expect("running")
    }

    /// The admin token, as generated into `<data_dir>/admin-token`.
    pub fn token(&self) -> String {
        std::fs::read_to_string(self.handle().token_path())
            .expect("token file")
            .trim()
            .to_owned()
    }

    /// A client on the admin unix socket.
    pub fn socket(&self) -> Client {
        Client::socket(self.handle().socket_path())
    }

    /// A client on the TCP admin listener, with `token`.
    pub fn tcp(&self, token: Option<&str>) -> Client {
        let addr = self.handle().admin_addr().expect("admin.bind set");
        Client::http(format!("http://{addr}"), token, None)
    }

    /// A client on the UI listener, with `token`.
    pub fn ui(&self, token: Option<&str>) -> Client {
        let addr = self.handle().ui_addr().expect("[ui] set");
        Client::http(format!("http://{addr}"), token, None)
    }

    /// Cancel and wait for every listener to stop; the data directory comes
    /// back for a restart (when this daemon owns it).
    pub async fn stop(mut self) -> Option<TempDir> {
        self.cancel.cancel();
        let handle = self.handle.take().expect("running");
        tokio::time::timeout(Duration::from_secs(15), handle.wait())
            .await
            .expect("admin surface stops");
        self.dir
    }
}

/// A response, fully read.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Assert the documented error shape and return its `code`.
    pub fn error_code(&self) -> String {
        let json = self.json();
        let err = json
            .get("error")
            .unwrap_or_else(|| panic!("no error object: {json}"));
        let code = err["code"].as_str().expect("code").to_owned();
        assert!(
            err["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{json}"
        );
        assert!(
            !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "code is not snake_case: {code}"
        );
        code
    }
}

enum Target {
    Socket(PathBuf),
    Http {
        base: String,
        client: reqwest::Client,
    },
}

/// An admin API client over the unix socket or TCP.
pub struct Client {
    target: Target,
    token: Option<String>,
}

impl Client {
    pub fn socket(path: &Path) -> Self {
        Client {
            target: Target::Socket(path.to_path_buf()),
            token: None,
        }
    }

    /// `base` is `http(s)://host:port`; `root_pem` a CA to trust for HTTPS.
    pub fn http(base: String, token: Option<&str>, root_pem: Option<&str>) -> Self {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if let Some(pem) = root_pem {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).expect("pem"));
        }
        Client {
            target: Target::Http {
                base,
                client: builder.build().expect("client"),
            },
            token: token.map(str::to_owned),
        }
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.send("GET", path, &[], None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> Reply {
        self.send("POST", path, &[], body.map(|b| b.to_string().into_bytes()))
            .await
    }

    pub async fn put(&self, path: &str, body: Value) -> Reply {
        self.send("PUT", path, &[], Some(body.to_string().into_bytes()))
            .await
    }

    pub async fn delete(&self, path: &str) -> Reply {
        self.send("DELETE", path, &[], None).await
    }

    /// Any request. A body is sent as `application/json`.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Reply {
        let method: http::Method = method.parse().expect("method");
        let mut all: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(t) = &self.token {
            all.push(("authorization".into(), format!("Bearer {t}")));
        }
        if body.is_some() {
            all.push(("content-type".into(), "application/json".into()));
        }
        match &self.target {
            Target::Http { base, client } => {
                let mut req = client.request(method, format!("{base}{path}"));
                for (k, v) in &all {
                    req = req.header(k, v);
                }
                if let Some(b) = body {
                    req = req.body(b);
                }
                let resp = req.send().await.expect("request");
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let body = resp.bytes().await.expect("body");
                Reply {
                    status,
                    headers,
                    body,
                }
            }
            Target::Socket(sock) => {
                let stream = tokio::net::UnixStream::connect(sock)
                    .await
                    .expect("connect");
                let (mut sender, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream))
                        .await
                        .expect("handshake");
                tokio::spawn(conn);
                let mut req = http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "janus-admin");
                for (k, v) in &all {
                    req = req.header(k, v);
                }
                let req = req
                    .body(Full::new(Bytes::from(body.unwrap_or_default())))
                    .expect("request");
                let resp = sender.send_request(req).await.expect("response");
                let (parts, body) = resp.into_parts();
                Reply {
                    status: parts.status.as_u16(),
                    headers: parts.headers,
                    body: body.collect().await.expect("body").to_bytes(),
                }
            }
        }
    }

    /// Open `GET path` as a live SSE stream (HTTP targets).
    pub async fn sse(&self, path: &str) -> Sse {
        let Target::Http { base, client } = &self.target else {
            panic!("SSE helper is for TCP listeners")
        };
        let mut req = client
            .get(format!("{base}{path}"))
            .timeout(Duration::from_secs(120));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.expect("stream request");
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().expect("ascii").to_owned());
        Sse {
            status,
            content_type,
            body: Box::pin(resp.bytes_stream()),
            buffer: String::new(),
        }
    }
}

type ByteStream = std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// A server-sent event stream being read.
pub struct Sse {
    pub status: u16,
    pub content_type: Option<String>,
    body: ByteStream,
    buffer: String,
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

impl Sse {
    /// The next event, or `None` if the stream ended or `wait` passed.
    pub async fn next(&mut self, wait: Duration) -> Option<SseEvent> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut event = SseEvent {
                    event: "message".into(),
                    data: String::new(),
                    id: None,
                };
                let mut any = false;
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event.event = v.trim().to_owned();
                        any = true;
                    } else if let Some(v) = line.strip_prefix("data:") {
                        if !event.data.is_empty() {
                            event.data.push('\n');
                        }
                        event.data.push_str(v.strip_prefix(' ').unwrap_or(v));
                        any = true;
                    } else if let Some(v) = line.strip_prefix("id:") {
                        event.id = Some(v.trim().to_owned());
                    }
                }
                if any {
                    return Some(event);
                }
                continue;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.body.next()).await {
                Ok(Some(Ok(chunk))) => self.buffer.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            }
        }
    }

    /// The next event that is not a `ping`.
    pub async fn next_non_ping(&mut self, wait: Duration) -> Option<SseEvent> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let event = self.next(left).await?;
            if event.event != "ping" {
                return Some(event);
            }
        }
    }
}

/// A finished request's record.
pub fn record(
    id: &str,
    identity: &str,
    upstream: UpstreamKind,
    outcome: Outcome,
    received_at: DateTime<Utc>,
) -> RequestRecord {
    RequestRecord {
        request_id: id.into(),
        received_at,
        duration_ms: 7,
        upstream: Some(upstream.as_str().to_owned()),
        implementation: Some(upstream),
        attempts: Vec::new(),
        route: None,
        method: "POST".into(),
        path: format!("/{upstream}/v1/chat/completions"),
        binding: Some(Method::Opaque),
        caller_identity: identity.into(),
        claimed_identity: None,
        correlation_id: None,
        outcome,
        credential_ref: Some("cred-a".into()),
        status: Some(200),
        usage: None,
        error: None,
    }
}

/// Log output captured from the current thread (the tests run on a
/// current-thread runtime, so the admin surface's tasks log here too).
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

thread_local! {
    /// The capture active on this thread, if any. Each test runs on its own
    /// thread, so a thread-local sink keeps concurrent captures apart.
    static SINK: std::cell::RefCell<Option<Logs>> = const { std::cell::RefCell::new(None) };
}

/// Ends a capture when dropped.
pub struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| *s.borrow_mut() = None);
    }
}

/// The one process-wide subscriber, routing each event to the capture on its
/// thread. A `set_default` subscriber per test races with tracing's global
/// callsite-interest cache when tests run concurrently, which dropped events
/// on machines with few cores.
#[derive(Clone, Copy)]
struct Router;

impl std::io::Write for Router {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(logs) = s.borrow().as_ref() {
                logs.0.lock().expect("lock").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Router {
    type Writer = Router;

    fn make_writer(&'a self) -> Router {
        *self
    }
}

impl Logs {
    /// Capture this thread's events until the guard drops.
    pub fn capture() -> (Logs, CaptureGuard) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(Router)
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in the admin tests");
        });
        let logs = Logs::default();
        SINK.with(|s| *s.borrow_mut() = Some(logs.clone()));
        (logs, CaptureGuard)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}
