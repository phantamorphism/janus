//! `GrpcPluginClient`: the plugin contract over gRPC, for both the local
//! socket and the network model (`docs/plugin-contract#plugin-client`).
//!
//! The two models differ only in how a connection is dialed ([`Target`]);
//! everything above the byte stream is one `tonic` channel. The client
//! owns:
//!
//! - **the `ResolveRoute` deadline** (`timeout_ms`): a plugin that has not
//!   answered by then has failed closed as `plugin_timeout`;
//! - **failure classification** from the real outcome of each connection
//!   (see [`super::transport`]): `plugin_unreachable`, `plugin_tls_error`,
//!   `plugin_no_client_auth`, `plugin_rejected_client_cert`, and
//!   `plugin_error` for an RPC error or a contract-violating answer from a
//!   plugin that was reached;
//! - **reconnection**: the channel dials lazily and re-dials after a
//!   connection is lost. Every dial of a unix socket re-runs the
//!   socket-path walk first and checks the uid serving the socket; every
//!   TLS dial is a full handshake with the current material. Every dial is
//!   bounded by `CONNECT_TIMEOUT`, and HTTP/2 keepalive drops a connection
//!   the plugin stopped answering on (see [`super::transport`]), so a
//!   vanished plugin address is noticed and redialed within seconds;
//! - **the `Notify` stream**: one long-lived client-streaming call fed from
//!   a bounded queue (see [`super::notify`] for what is dropped when). When
//!   the stream fails, events already handed to it may be lost; events still
//!   queued are kept and the stream is reopened with exponential backoff.
//!   `Notify` has no deadline. Rotating TLS material ends the stream
//!   cleanly so the next one is opened with the new material. Keepalive
//!   ends a stream whose connection went silent, so it is reopened. The stream
//!   runs on its own connection: a plugin that stops reading it can exhaust
//!   that connection's HTTP/2 flow-control window, and must not thereby
//!   stall `ResolveRoute`.
//!
//! Reachability is not checked at construction: a plugin may come up after
//! the daemon. The static and filesystem checks are
//! (`docs/plugin-contract#plugin-config`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use hyper_util::rt::TokioIo;
use janus_core::contract::plugin::{
    NotifyEvent, PluginClient, PluginError, PluginFailureClass, PluginMode, ResolveRoute,
    RouteOutcome,
};
use janus_core::contract::proto;
use janus_core::contract::proto::janus_plugin_client::JanusPluginClient;
use tokio::sync::{mpsc, watch};
use tonic::transport::{Channel, Endpoint, Uri};

use super::check::{self, ChannelCheck};
use super::notify::{NotifyQueue, NOTIFY_QUEUE_CAPACITY};
use super::socket;
use super::tls::{self, TlsDialer, TlsSettings, MATERIAL_POLL_INTERVAL};
use super::transport::{
    classify_plain, connect_tcp, RecordingIo, TransportLog, KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT,
};
use super::SetupError;
use crate::contract::config::{invalid, ConfigError, PluginTransport};

/// First delay before reopening a failed `Notify` stream.
const NOTIFY_RETRY_MIN: Duration = Duration::from_millis(100);
/// Longest delay between attempts to reopen the `Notify` stream.
const NOTIFY_RETRY_MAX: Duration = Duration::from_secs(5);

/// How a connection to the plugin is made.
#[derive(Debug)]
pub(crate) enum Target {
    /// gRPC over the unix socket at this path.
    Unix(PathBuf),
    /// gRPC over mutual TLS 1.3.
    Tls(Box<TlsDialer>),
    /// gRPC over plaintext TCP (`allow_plaintext`) to `host:port`.
    Plaintext(String),
}

/// Makes connections.
#[derive(Debug)]
pub(crate) struct Dialer {
    pub(crate) target: Target,
}

impl Dialer {
    /// A connection whose failures are recorded in `log`.
    async fn open(&self, log: &Arc<TransportLog>) -> Result<RecordingIo, PluginError> {
        let log = Arc::clone(log);
        match &self.target {
            Target::Unix(path) => {
                let walk = socket::check_full(path).map_err(|e| {
                    PluginError::new(PluginFailureClass::Unreachable, e.to_string())
                })?;
                let stream = walk.connect().await?;
                Ok(RecordingIo::new(Box::new(stream), log, classify_plain))
            }
            Target::Tls(dialer) => {
                let stream = dialer.connect().await?;
                Ok(RecordingIo::new(
                    Box::new(stream),
                    log,
                    tls::classify_post_handshake,
                ))
            }
            Target::Plaintext(authority) => {
                let stream = connect_tcp(authority.as_str(), authority).await?;
                Ok(RecordingIo::new(Box::new(stream), log, classify_plain))
            }
        }
    }

    /// One connection for a channel; a failure is recorded in the
    /// channel's `log` before it is returned, so the call that triggered
    /// the dial can find it.
    async fn dial(&self, log: &Arc<TransportLog>) -> Result<TokioIo<RecordingIo>, PluginError> {
        match self.open(log).await {
            Ok(io) => Ok(TokioIo::new(io)),
            Err(err) => {
                log.record(err.clone());
                Err(err)
            }
        }
    }

    /// The request URI the channel is built from, and the origin requests
    /// carry. Dialing is the connector's, so the URI only names the
    /// authority.
    fn uris(&self) -> (String, Option<String>) {
        match &self.target {
            Target::Unix(_) => ("http://localhost".to_owned(), None),
            Target::Tls(d) => (
                format!("http://{}", d.authority()),
                Some(format!("https://{}", d.authority())),
            ),
            Target::Plaintext(authority) => (format!("http://{authority}"), None),
        }
    }
}

/// A lazily dialed channel whose connections record into `log`. HTTP/2
/// keepalive drops a connection the plugin stopped answering on, so the
/// next call dials afresh instead of timing out against it indefinitely.
fn new_channel(dialer: &Arc<Dialer>, log: &Arc<TransportLog>) -> Result<Channel, ConfigError> {
    let (uri, origin) = dialer.uris();
    let mut endpoint = Endpoint::from_shared(uri)
        .map_err(|e| {
            invalid(
                "plugin.endpoint",
                format!("cannot build a gRPC endpoint: {e}"),
            )
        })?
        .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(true);
    if let Some(origin) = origin {
        let origin: Uri = origin
            .parse()
            .map_err(|e| invalid("plugin.endpoint", format!("cannot build an origin: {e}")))?;
        endpoint = endpoint.origin(origin);
    }
    let dialer = Arc::clone(dialer);
    let log = Arc::clone(log);
    Ok(
        endpoint.connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
            let dialer = Arc::clone(&dialer);
            let log = Arc::clone(&log);
            async move { dialer.dial(&log).await }
        })),
    )
}

/// One connection for `ResolveRoute`, one for the `Notify` stream.
#[derive(Debug, Clone)]
struct Channels {
    rpc: Channel,
    notify: Channel,
}

/// Each channel's transport failures, kept across channel rebuilds.
#[derive(Debug, Default)]
struct Logs {
    rpc: Arc<TransportLog>,
    notify: Arc<TransportLog>,
}

impl Channels {
    fn new(dialer: &Arc<Dialer>, logs: &Logs) -> Result<Self, ConfigError> {
        Ok(Channels {
            rpc: new_channel(dialer, &logs.rpc)?,
            notify: new_channel(dialer, &logs.notify)?,
        })
    }
}

/// State shared by the client, its `Notify` task and its material watcher.
#[derive(Debug)]
struct Shared {
    dialer: Arc<Dialer>,
    mode: PluginMode,
    endpoint: String,
    timeout: Duration,
    logs: Logs,
    channels: Mutex<Channels>,
    /// Bumped when TLS material is rotated.
    generation: watch::Sender<u64>,
}

impl Shared {
    fn channels(&self) -> Channels {
        self.channels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn tls(&self) -> Option<&TlsDialer> {
        match &self.dialer.target {
            Target::Tls(d) => Some(d.as_ref()),
            _ => None,
        }
    }

    /// Classify a failed call: a transport failure recorded while it was in
    /// flight, else an RPC error from the plugin.
    fn failure(&self, log: &TransportLog, mark: u64, status: &tonic::Status) -> PluginError {
        log.since(mark).unwrap_or_else(|| {
            PluginError::new(
                PluginFailureClass::Error,
                format!(
                    "plugin returned gRPC status {:?}: {}",
                    status.code(),
                    status.message()
                ),
            )
        })
    }

    fn reload_tls(&self) -> Result<(), ConfigError> {
        let Some(dialer) = self.tls() else {
            return Ok(());
        };
        dialer.reload()?;
        // A fresh channel, so the next call dials with the new material;
        // calls in flight finish on the old connection.
        let fresh = Channels::new(&self.dialer, &self.logs)?;
        *self.channels.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        self.generation.send_modify(|g| *g += 1);
        tracing::info!(endpoint = %self.endpoint, "plugin TLS material reloaded");
        Ok(())
    }
}

/// The gRPC plugin client (unix socket, mutual TLS, or opted-in plaintext).
#[derive(Debug)]
pub struct GrpcPluginClient {
    shared: Arc<Shared>,
    notify: NotifyQueue,
}

fn settings_for(transport: &PluginTransport) -> Result<(Target, PluginMode, String), ConfigError> {
    match transport {
        PluginTransport::Linked => Err(invalid(
            "plugin.linked",
            "a linked plugin is called directly, not over gRPC",
        )),
        PluginTransport::Unix { path } => {
            socket::check_existing(path).map_err(|e| invalid("plugin.endpoint", e.to_string()))?;
            Ok((
                Target::Unix(path.clone()),
                PluginMode::Unix,
                format!("unix://{}", path.display()),
            ))
        }
        PluginTransport::Tls {
            endpoint,
            trust,
            client_cert,
            client_key,
            server_name,
            expected_san,
        } => {
            let dialer = TlsDialer::new(TlsSettings {
                endpoint: endpoint.clone(),
                trust: trust.clone(),
                client_cert: client_cert.clone(),
                client_key: client_key.clone(),
                server_name: server_name.clone(),
                expected_san: expected_san.clone(),
            })?;
            Ok((
                Target::Tls(Box::new(dialer)),
                PluginMode::Tls,
                endpoint.to_string(),
            ))
        }
        PluginTransport::Plaintext { endpoint } => {
            let host = endpoint
                .host_str()
                .ok_or_else(|| invalid("plugin.endpoint", "http endpoint has no host"))?;
            let port = endpoint
                .port_or_known_default()
                .ok_or_else(|| invalid("plugin.endpoint", "http endpoint has no port"))?;
            tracing::warn!(
                endpoint = %endpoint,
                "plugin.allow_plaintext is set: the plugin channel is NEITHER encrypted NOR authenticated by Janus; this is correct only inside a service mesh that secures both ends"
            );
            Ok((
                Target::Plaintext(format!("{host}:{port}")),
                PluginMode::Plaintext,
                endpoint.to_string(),
            ))
        }
    }
}

impl GrpcPluginClient {
    /// Build the client for a `unix://`, `https://` or `http://`
    /// transport, running the startup checks: the socket-path walk over the
    /// part that exists, or the TLS material validation. Starts the
    /// `Notify` task (and, for TLS, the material watcher), so it must be
    /// called from within a Tokio runtime. Does not connect.
    pub fn new(transport: &PluginTransport, timeout: Duration) -> Result<Self, SetupError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| SetupError::NoRuntime)?;
        let (target, mode, endpoint) = settings_for(transport)?;
        let dialer = Arc::new(Dialer { target });
        let logs = Logs::default();
        // Entering the runtime: the channel spawns its worker when built.
        let _guard = runtime.enter();
        let channels = Channels::new(&dialer, &logs)?;
        let (generation, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            dialer,
            mode,
            endpoint,
            timeout,
            logs,
            channels: Mutex::new(channels),
            generation,
        });
        let (notify, rx) = NotifyQueue::new(NOTIFY_QUEUE_CAPACITY);
        runtime.spawn(run_notify(Arc::clone(&shared), rx));
        if shared.tls().is_some() {
            runtime.spawn(watch_material(Arc::downgrade(&shared)));
        }
        Ok(GrpcPluginClient { shared, notify })
    }

    /// `unix`, `tls` or `plaintext`.
    pub fn mode(&self) -> PluginMode {
        self.shared.mode
    }

    /// The configured endpoint.
    pub fn endpoint(&self) -> &str {
        &self.shared.endpoint
    }

    /// Re-read the TLS material (on `SIGHUP`; file changes are picked up
    /// on their own). On failure the last good material stays in use. A
    /// no-op on other transports.
    pub fn reload_tls(&self) -> Result<(), ConfigError> {
        self.shared.reload_tls()
    }

    /// TLS transport: expiry of the daemon's client certificate.
    pub fn client_cert_expires_at(&self) -> Option<DateTime<Utc>> {
        self.shared.tls().map(TlsDialer::client_cert_expires_at)
    }

    /// TLS transport: expiry of the plugin's certificate, once a handshake
    /// has seen it.
    pub fn server_cert_expires_at(&self) -> Option<DateTime<Utc>> {
        self.shared
            .tls()
            .and_then(TlsDialer::server_cert_expires_at)
    }

    /// `notify` events dropped because the queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.notify.dropped()
    }

    /// `janus plugin check` / `POST /v1/plugin/check` for this channel.
    pub async fn check(&self) -> ChannelCheck {
        check::run(&self.shared.dialer, self.shared.mode, &self.shared.endpoint).await
    }
}

#[async_trait]
impl PluginClient for GrpcPluginClient {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        let shared = &self.shared;
        let mark = shared.logs.rpc.mark();
        let mut client = JanusPluginClient::new(shared.channels().rpc);
        let request = proto::ResolveRouteRequest::from(req);
        let response =
            match tokio::time::timeout(shared.timeout, client.resolve_route(request)).await {
                Err(_) => {
                    return Err(PluginError::new(
                        PluginFailureClass::Timeout,
                        format!(
                            "plugin did not answer ResolveRoute within {} ms",
                            shared.timeout.as_millis()
                        ),
                    ))
                }
                Ok(Err(status)) => return Err(shared.failure(&shared.logs.rpc, mark, &status)),
                Ok(Ok(response)) => response.into_inner(),
            };
        Ok(RouteOutcome::try_from(response)?)
    }

    fn notify(&self, event: NotifyEvent) {
        self.notify.push(event);
    }
}

/// What one `Notify` stream reads from.
struct StreamFeed {
    /// The event that opened the stream, until the stream takes it.
    first: Arc<Mutex<Option<proto::NotifyEvent>>>,
    queue: Arc<tokio::sync::Mutex<mpsc::Receiver<NotifyEvent>>>,
    generation: watch::Receiver<u64>,
    /// Events handed to the stream.
    taken: Arc<AtomicU64>,
}

/// The request stream of one `Notify` call. Ends when the queue closes
/// (the client was dropped) or the TLS material rotates.
fn feed(state: StreamFeed) -> impl Stream<Item = proto::NotifyEvent> + Send + 'static {
    futures::stream::unfold(state, |mut st| async move {
        let first = st.first.lock().unwrap_or_else(|p| p.into_inner()).take();
        let next = match first {
            Some(ev) => Some(ev),
            None => {
                let mut queue = st.queue.lock().await;
                tokio::select! {
                    ev = queue.recv() => ev.map(proto::NotifyEvent::from),
                    _ = st.generation.changed() => None,
                }
            }
        };
        let ev = next?;
        st.taken.fetch_add(1, Ordering::Relaxed);
        Some((ev, st))
    })
}

/// Deliver queued events over a long-lived `Notify` stream, reopening it
/// after failures. Never touches the request path.
async fn run_notify(shared: Arc<Shared>, rx: mpsc::Receiver<NotifyEvent>) {
    let queue = Arc::new(tokio::sync::Mutex::new(rx));
    let mut pending: Option<proto::NotifyEvent> = None;
    let mut backoff = NOTIFY_RETRY_MIN;
    loop {
        let first = match pending.take() {
            Some(ev) => ev,
            None => match queue.lock().await.recv().await {
                Some(ev) => ev.into(),
                None => return,
            },
        };
        let first = Arc::new(Mutex::new(Some(first)));
        let taken = Arc::new(AtomicU64::new(0));
        let stream = feed(StreamFeed {
            first: Arc::clone(&first),
            queue: Arc::clone(&queue),
            generation: shared.generation.subscribe(),
            taken: Arc::clone(&taken),
        });
        let mark = shared.logs.notify.mark();
        let mut client = JanusPluginClient::new(shared.channels().notify);
        let result = client.notify(stream).await;
        // An opening event the stream never took was not sent.
        pending = first.lock().unwrap_or_else(|p| p.into_inner()).take();
        match result {
            Ok(_) => backoff = NOTIFY_RETRY_MIN,
            Err(status) => {
                let err = shared.failure(&shared.logs.notify, mark, &status);
                tracing::warn!(
                    endpoint = %shared.endpoint,
                    class = %err.class,
                    possibly_lost = taken.load(Ordering::Relaxed),
                    "plugin Notify stream failed, reopening in {} ms: {}",
                    backoff.as_millis(),
                    err.message
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(NOTIFY_RETRY_MAX);
            }
        }
    }
}

/// Reload TLS material when its files change, and warn about certificates
/// nearing expiry. Ends when the client does.
async fn watch_material(shared: Weak<Shared>) {
    let mut tick = tokio::time::interval(MATERIAL_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_failure: Option<String> = None;
    loop {
        tick.tick().await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let Some(tls) = shared.tls() else {
            return;
        };
        tls.warn_expiring();
        if !tls.files_changed() {
            continue;
        }
        match shared.reload_tls() {
            Ok(()) => last_failure = None,
            Err(e) => {
                let message = e.to_string();
                if last_failure.as_deref() != Some(message.as_str()) {
                    tracing::warn!(
                        endpoint = %shared.endpoint,
                        "plugin TLS material changed on disk but does not load; keeping the last good material: {message}"
                    );
                    last_failure = Some(message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testutil::{
        client_files, closed_port, event, eventually, request, serve_plaintext, serve_unix,
        socket_dir, tls_transport, ymd, FakePlugin, Leaf, Relay, Reply, TlsFixture,
    };
    use crate::plugin::transport::CONNECT_TIMEOUT;
    use janus_core::contract::proto::resolve_route_response::Outcome;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Instant;

    const T: Duration = Duration::from_secs(5);

    fn unix_client(path: &Path, timeout: Duration) -> GrpcPluginClient {
        GrpcPluginClient::new(
            &PluginTransport::Unix {
                path: path.to_path_buf(),
            },
            timeout,
        )
        .expect("client")
    }

    fn plaintext_client(addr: std::net::SocketAddr) -> GrpcPluginClient {
        plaintext_client_with(addr, T)
    }

    fn plaintext_client_with(addr: std::net::SocketAddr, timeout: Duration) -> GrpcPluginClient {
        let endpoint = url::Url::parse(&format!("http://{addr}")).expect("url");
        GrpcPluginClient::new(&PluginTransport::Plaintext { endpoint }, timeout).expect("client")
    }

    #[tokio::test]
    async fn deny_reason_is_returned() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::new(Reply::Outcome(Some(Outcome::DenyReason(
            "budget exhausted for session-9".into(),
        ))));
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        assert_eq!(
            client.resolve_route(request("session-9")).await,
            Ok(RouteOutcome::Deny {
                reason: "budget exhausted for session-9".into()
            })
        );
    }

    #[tokio::test]
    async fn grpc_outcomes_and_request_fields_round_trip() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("pool/acme-1");
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        assert_eq!(client.mode(), PluginMode::Unix);
        assert_eq!(client.endpoint(), format!("unix://{}", path.display()));
        assert_eq!(
            client.resolve_route(request("session-1")).await,
            Ok(RouteOutcome::Credential("pool/acme-1".into()))
        );
        plugin.set_reply(Reply::Outcome(Some(Outcome::UseDefault(true))));
        assert_eq!(
            client.resolve_route(request("session-2")).await,
            Ok(RouteOutcome::UseDefault)
        );
        let seen = plugin.requests.lock().expect("lock").clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].request_id, "req-session-1");
        assert_eq!(seen[0].upstream, "anthropic");
        assert_eq!(seen[0].caller_identity, "session-1");
        assert_eq!(seen[0].correlation_id, "conv-1");
        assert_eq!(seen[0].identity_claims["tenant"], "acme");
    }

    #[tokio::test]
    async fn plugin_unreachable_fails_closed() {
        // A socket path with nothing listening.
        let (_dir, path) = socket_dir();
        let client = unix_client(&path, T);
        let started = Instant::now();
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("no plugin");
        assert_eq!(err.class, PluginFailureClass::Unreachable, "{err:?}");
        assert!(err.message.contains("plugin.sock"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));

        // A TCP port with nothing listening.
        let client = plaintext_client(closed_port().await);
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("no plugin");
        assert_eq!(err.class, PluginFailureClass::Unreachable, "{err:?}");
    }

    #[tokio::test]
    async fn plugin_timeout_fails_closed() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::new(Reply::Slow(Duration::from_secs(30)));
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, Duration::from_millis(100));
        let started = Instant::now();
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("too slow");
        assert_eq!(err.class, PluginFailureClass::Timeout);
        assert!(err.message.contains("100 ms"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(plugin.request_count(), 1, "the call reached the plugin");
    }

    #[test]
    fn default_timeout_is_250ms() {
        let config: crate::contract::config::PluginConfig =
            toml::from_str("endpoint = \"unix:///run/janus/plugin.sock\"").expect("config");
        assert_eq!(config.timeout(), Duration::from_millis(250));
        assert_eq!(
            crate::contract::config::DEFAULT_PLUGIN_TIMEOUT_MS,
            250,
            "the documented default"
        );
    }

    #[tokio::test]
    async fn rpc_error_and_malformed_answers_are_plugin_error() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::new(Reply::Status(tonic::Code::Internal, "policy db down"));
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("status");
        assert_eq!(err.class, PluginFailureClass::Error);
        assert!(err.message.contains("policy db down"), "{err}");
        for bad in [
            None,
            Some(Outcome::UseDefault(false)),
            Some(Outcome::DenyReason(String::new())),
        ] {
            plugin.set_reply(Reply::Outcome(bad.clone()));
            let err = client
                .resolve_route(request("s"))
                .await
                .expect_err("malformed");
            assert_eq!(err.class, PluginFailureClass::Error, "{bad:?}: {err:?}");
        }
    }

    #[tokio::test]
    async fn socket_path_is_rechecked_before_each_connect() {
        let (dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("unsafe dir");
        assert_eq!(err.class, PluginFailureClass::Unreachable);
        assert!(err.message.contains("writable"), "{err}");
        assert_eq!(plugin.request_count(), 0, "never connected");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        assert!(client.resolve_route(request("s")).await.is_ok());
    }

    #[tokio::test]
    async fn socket_symlink_in_a_writable_directory_is_never_connected() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let _server = serve_unix(&plugin, &path);
        let links = tempfile::tempdir().expect("tempdir");
        let link = links.path().join("p.sock");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        let client = unix_client(&link, T);
        // After startup, anyone is let into the link's directory and could
        // repoint the link: the dial must refuse it.
        std::fs::set_permissions(links.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("link's directory is writable");
        assert_eq!(err.class, PluginFailureClass::Unreachable);
        assert!(err.message.contains("writable"), "{err}");
        assert_eq!(plugin.request_count(), 0, "never connected through it");
        std::fs::set_permissions(links.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        assert!(
            client.resolve_route(request("s")).await.is_ok(),
            "a link in a sound directory is followed"
        );
    }

    #[tokio::test]
    async fn blackholed_connection_is_dropped_and_redialed() {
        // The plugin's address vanishes without a reset: every byte on the
        // existing connections disappears, while a fresh dial reaches it.
        let plugin = FakePlugin::credential("c");
        let (_server, addr) = serve_plaintext(&plugin).await;
        let relay = Relay::start(addr).await;
        let client = plaintext_client_with(relay.addr, Duration::from_millis(300));
        client.notify(event("before"));
        assert!(eventually(T, || plugin.event_types() == ["before"]).await);
        assert!(client.resolve_route(request("s")).await.is_ok());
        assert_eq!(
            relay.connections(),
            2,
            "one for Notify, one for ResolveRoute"
        );

        relay.blackhole_existing();
        let started = Instant::now();
        let err = client
            .resolve_route(request("s"))
            .await
            .expect_err("black-holed");
        assert_eq!(err.class, PluginFailureClass::Timeout, "{err:?}");
        let bound =
            KEEPALIVE_INTERVAL + KEEPALIVE_TIMEOUT + CONNECT_TIMEOUT + Duration::from_secs(3);
        while client.resolve_route(request("s")).await.is_err() {
            assert!(started.elapsed() < bound, "ResolveRoute never redialed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The Notify stream went silent too, and is reopened.
        let mut n = 0;
        while !plugin.event_types().iter().any(|e| e.starts_with("after-")) {
            assert!(started.elapsed() < bound, "Notify stream never reopened");
            client.notify(event(&format!("after-{n}")));
            n += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(relay.connections() >= 4, "both channels redialed");
    }

    #[tokio::test]
    async fn notify_connection_closing_does_not_reclassify_a_resolve_route_error() {
        let plugin = FakePlugin::credential("c");
        let (_server, addr) = serve_plaintext(&plugin).await;
        let relay = Relay::start(addr).await;
        let client = Arc::new(plaintext_client(relay.addr));
        client.notify(event("open"));
        assert!(eventually(T, || plugin.event_types() == ["open"]).await);
        assert_eq!(relay.connections(), 1, "the Notify connection");
        plugin.set_reply(Reply::SlowStatus(
            Duration::from_millis(500),
            tonic::Code::Internal,
            "policy db down",
        ));
        let call = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.resolve_route(request("s")).await })
        };
        assert!(eventually(T, || plugin.request_count() == 1).await);
        // The Notify connection ends while ResolveRoute is in flight.
        relay.close(0);
        let err = call.await.expect("joined").expect_err("rpc error");
        assert_eq!(err.class, PluginFailureClass::Error, "{err:?}");
        assert!(err.message.contains("policy db down"), "{err}");
    }

    #[tokio::test]
    async fn client_reconnects_after_plugin_restart() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        assert!(client.resolve_route(request("s")).await.is_ok());
        server.stop().await;
        std::fs::remove_file(&path).expect("socket gone with the plugin");
        let err = client.resolve_route(request("s")).await.expect_err("down");
        assert_eq!(err.class, PluginFailureClass::Unreachable, "{err:?}");
        let _server = serve_unix(&plugin, &path);
        assert!(
            client.resolve_route(request("s")).await.is_ok(),
            "reconnected"
        );
    }

    #[tokio::test]
    async fn plugin_sees_the_daemon_uid_as_socket_peer() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        client.resolve_route(request("s")).await.expect("ok");
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(*plugin.peer_uids.lock().expect("lock"), vec![uid]);
    }

    #[tokio::test]
    async fn notify_events_are_delivered_in_order() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        let kinds: Vec<String> = (0..50).map(|n| format!("event-{n}")).collect();
        for kind in &kinds {
            client.notify(event(kind));
        }
        assert!(eventually(T, || plugin.event_types().len() == kinds.len()).await);
        assert_eq!(plugin.event_types(), kinds);
        let first = plugin.events.lock().expect("lock")[0].clone();
        assert_eq!(first.request_id, "req-1");
        assert_eq!(client.dropped_events(), 0);
    }

    #[tokio::test]
    async fn notify_never_blocks_caller() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::stalling_notify();
        let _server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        let started = Instant::now();
        let total = 20_000u64;
        for n in 0..total {
            client.notify(event(&format!("event-{n}-{}", "x".repeat(200))));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "notify blocked: {elapsed:?}"
        );
        assert!(
            eventually(T, || client.dropped_events() > 0).await,
            "a full queue drops"
        );
        assert!(client.dropped_events() < total);
        // The stalled stream does not stall the request path.
        assert_eq!(
            client.resolve_route(request("s")).await,
            Ok(RouteOutcome::UseDefault)
        );
    }

    #[tokio::test]
    async fn notify_stream_is_reopened_after_plugin_restart() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let server = serve_unix(&plugin, &path);
        let client = unix_client(&path, T);
        client.notify(event("before"));
        assert!(eventually(T, || plugin.event_types() == ["before"]).await);
        server.stop().await;

        let restarted = FakePlugin::credential("c");
        let _server = serve_unix(&restarted, &path);
        client.notify(event("after"));
        assert!(
            eventually(Duration::from_secs(15), || restarted
                .event_types()
                .contains(&"after".to_owned()))
            .await,
            "delivered over a new stream"
        );
    }

    #[tokio::test]
    async fn material_watcher_warns_about_expiring_certificates() {
        let fx = TlsFixture::new();
        let now = Utc::now();
        let expiring = fx.ca.issue(&Leaf {
            window: Some((
                ymd(now - chrono::TimeDelta::days(1)),
                ymd(now + chrono::TimeDelta::days(20)),
            )),
            ..Leaf::client()
        });
        let files = client_files(fx.dir.path(), &fx.ca.pem(), &expiring);
        let addr: std::net::SocketAddr = "127.0.0.1:9".parse().expect("addr");
        let client = GrpcPluginClient::new(&tls_transport(addr, &files, false, None, None), T)
            .expect("client");
        let tls = client.shared.tls().expect("tls");
        assert!(
            eventually(T, || tls.warned() == ["client"]).await,
            "the running client warns without a reload"
        );
    }

    #[tokio::test]
    async fn plaintext_transport_round_trips() {
        let plugin = FakePlugin::credential("plain");
        let (_server, addr) = serve_plaintext(&plugin).await;
        let client = plaintext_client(addr);
        assert_eq!(client.mode(), PluginMode::Plaintext);
        assert_eq!(
            client.resolve_route(request("s")).await,
            Ok(RouteOutcome::Credential("plain".into()))
        );
        assert_eq!(client.client_cert_expires_at(), None);
        client.reload_tls().expect("no-op");
    }

    #[test]
    fn grpc_client_refuses_linked_and_needs_a_runtime() {
        let err = GrpcPluginClient::new(&PluginTransport::Linked, T).expect_err("no runtime");
        assert!(matches!(err, SetupError::NoRuntime));
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = rt.enter();
        let err = GrpcPluginClient::new(&PluginTransport::Linked, T).expect_err("linked");
        assert!(err.to_string().contains("linked"), "{err}");
        let (dir, path) = socket_dir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");
        let err =
            GrpcPluginClient::new(&PluginTransport::Unix { path }, T).expect_err("startup walk");
        assert!(err.to_string().contains("writable"), "{err}");
    }
}
