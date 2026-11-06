//! Real plugin servers for the plugin client's tests: the generated `tonic`
//! service over a unix socket, plaintext TCP, or rustls TLS with
//! certificates issued here by `rcgen`.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use janus_core::contract::plugin::{NotifyEvent, ResolveRoute};
use janus_core::contract::proto;
use janus_core::contract::proto::janus_plugin_server::{JanusPlugin, JanusPluginServer};
use janus_core::contract::proto::resolve_route_response::Outcome;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{DigitallySignedStruct, DistinguishedName as TlsName, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tonic::transport::server::UdsConnectInfo;
use tonic::transport::server::{Connected, Server};

use super::grpc::GrpcPluginClient;
use super::tls::provider;
use crate::contract::config::{PluginTransport, TrustAnchor};

/// What the fake plugin answers `ResolveRoute` with.
#[derive(Debug, Clone)]
pub(crate) enum Reply {
    Outcome(Option<Outcome>),
    /// Sleep this long, then answer `use_default`.
    Slow(Duration),
    Status(tonic::Code, &'static str),
    /// Sleep this long, then fail with this status.
    SlowStatus(Duration, tonic::Code, &'static str),
}

/// A scripted out-of-process plugin.
#[derive(Debug)]
pub(crate) struct FakePlugin {
    reply: Mutex<Reply>,
    stall_notify: bool,
    pub requests: Mutex<Vec<proto::ResolveRouteRequest>>,
    pub events: Mutex<Vec<proto::NotifyEvent>>,
    pub peer_uids: Mutex<Vec<u32>>,
    /// Client certificates presented in completed TLS handshakes.
    pub peer_certs: Mutex<Vec<CertificateDer<'static>>>,
    pub failed_handshakes: AtomicU64,
    closing: CancellationToken,
}

impl FakePlugin {
    pub(crate) fn new(reply: Reply) -> Arc<Self> {
        Arc::new(FakePlugin {
            reply: Mutex::new(reply),
            stall_notify: false,
            requests: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            peer_uids: Mutex::new(Vec::new()),
            peer_certs: Mutex::new(Vec::new()),
            failed_handshakes: AtomicU64::new(0),
            closing: CancellationToken::new(),
        })
    }

    pub(crate) fn credential(cred: &str) -> Arc<Self> {
        FakePlugin::new(Reply::Outcome(Some(Outcome::CredentialRef(cred.into()))))
    }

    /// Accepts `Notify` streams and never reads them.
    pub(crate) fn stalling_notify() -> Arc<Self> {
        let mut plugin = FakePlugin::new(Reply::Outcome(Some(Outcome::UseDefault(true))));
        Arc::get_mut(&mut plugin).expect("fresh").stall_notify = true;
        plugin
    }

    pub(crate) fn set_reply(&self, reply: Reply) {
        *self.reply.lock().expect("lock") = reply;
    }

    pub(crate) fn event_types(&self) -> Vec<String> {
        let events = self.events.lock().expect("lock");
        events.iter().map(|e| e.event_type.clone()).collect()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }
}

#[tonic::async_trait]
impl JanusPlugin for FakePlugin {
    async fn resolve_route(
        &self,
        request: tonic::Request<proto::ResolveRouteRequest>,
    ) -> Result<tonic::Response<proto::ResolveRouteResponse>, tonic::Status> {
        if let Some(cred) = request
            .extensions()
            .get::<UdsConnectInfo>()
            .and_then(|i| i.peer_cred)
        {
            self.peer_uids.lock().expect("lock").push(cred.uid());
        }
        self.requests
            .lock()
            .expect("lock")
            .push(request.into_inner());
        let reply = self.reply.lock().expect("lock").clone();
        let outcome = match reply {
            Reply::Outcome(outcome) => outcome,
            Reply::Slow(d) => {
                tokio::select! {
                    _ = tokio::time::sleep(d) => {}
                    _ = self.closing.cancelled() => {}
                }
                Some(Outcome::UseDefault(true))
            }
            Reply::Status(code, message) => return Err(tonic::Status::new(code, message)),
            Reply::SlowStatus(d, code, message) => {
                tokio::time::sleep(d).await;
                return Err(tonic::Status::new(code, message));
            }
        };
        Ok(tonic::Response::new(proto::ResolveRouteResponse {
            outcome,
        }))
    }

    async fn notify(
        &self,
        request: tonic::Request<tonic::Streaming<proto::NotifyEvent>>,
    ) -> Result<tonic::Response<proto::NotifyAck>, tonic::Status> {
        let mut stream = request.into_inner();
        if self.stall_notify {
            self.closing.cancelled().await;
            return Err(tonic::Status::unavailable("shutting down"));
        }
        loop {
            tokio::select! {
                msg = stream.message() => match msg? {
                    Some(ev) => self.events.lock().expect("lock").push(ev),
                    None => return Ok(tonic::Response::new(proto::NotifyAck {})),
                },
                _ = self.closing.cancelled() => {
                    return Err(tonic::Status::unavailable("shutting down"))
                }
            }
        }
    }
}

/// A running server; stopped (connections closed) on [`Running::stop`] or
/// drop.
pub(crate) struct Running {
    plugin: Arc<FakePlugin>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    fn spawn<S, IO>(plugin: Arc<FakePlugin>, incoming: S) -> Self
    where
        S: futures::Stream<Item = io::Result<IO>> + Send + 'static,
        IO: AsyncRead + AsyncWrite + Connected + Send + Unpin + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let service = JanusPluginServer::from_arc(Arc::clone(&plugin));
        let task = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await;
        });
        Running {
            plugin,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    pub(crate) async fn stop(mut self) {
        self.plugin.closing.cancel();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.plugin.closing.cancel();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Serve `plugin` on a unix socket at `path` (replacing a stale file).
pub(crate) fn serve_unix(plugin: &Arc<FakePlugin>, path: &Path) -> Running {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).expect("bind unix socket");
    let incoming = futures::stream::unfold(listener, |l| async move {
        let conn = l.accept().await.map(|(s, _)| s);
        Some((conn, l))
    });
    Running::spawn(Arc::clone(plugin), incoming)
}

/// Serve `plugin` over plaintext TCP on a loopback port.
pub(crate) async fn serve_plaintext(plugin: &Arc<FakePlugin>) -> (Running, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let incoming = futures::stream::unfold(listener, |l| async move {
        let conn = l.accept().await.map(|(s, _)| s);
        Some((conn, l))
    });
    (Running::spawn(Arc::clone(plugin), incoming), addr)
}

/// One connection through a [`Relay`].
#[derive(Debug, Default)]
struct RelayConn {
    blackholed: AtomicBool,
    closed: CancellationToken,
}

/// A TCP relay in front of a plugin, standing in for the network between
/// them. Each connection through it can be black-holed — bytes discarded
/// both ways with the sockets left open, as when the plugin's address
/// vanishes without a reset — or closed.
pub(crate) struct Relay {
    pub addr: SocketAddr,
    conns: Arc<Mutex<Vec<Arc<RelayConn>>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn shovel(
    conn: &RelayConn,
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if conn.blackholed.load(Ordering::SeqCst) {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

impl Relay {
    /// Relay connections on a fresh loopback port to `upstream`.
    pub(crate) async fn start(upstream: SocketAddr) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let conns: Arc<Mutex<Vec<Arc<RelayConn>>>> = Arc::default();
        let registry = Arc::clone(&conns);
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let conn = Arc::new(RelayConn::default());
                registry.lock().expect("lock").push(Arc::clone(&conn));
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let (client_r, client_w) = client.into_split();
                    let (server_r, server_w) = server.into_split();
                    tokio::select! {
                        _ = shovel(&conn, client_r, server_w) => {}
                        _ = shovel(&conn, server_r, client_w) => {}
                        _ = conn.closed.cancelled() => {}
                    }
                });
            }
        });
        Relay { addr, conns, task }
    }

    /// Connections accepted so far.
    pub(crate) fn connections(&self) -> usize {
        self.conns.lock().expect("lock").len()
    }

    /// Black-hole every connection made so far; later ones relay normally.
    pub(crate) fn blackhole_existing(&self) {
        for conn in self.conns.lock().expect("lock").iter() {
            conn.blackholed.store(true, Ordering::SeqCst);
        }
    }

    /// Close the `n`th connection (both ends see it end).
    pub(crate) fn close(&self, n: usize) {
        self.conns.lock().expect("lock")[n].closed.cancel();
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        for conn in self.conns.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            conn.closed.cancel();
        }
    }
}

/// A TCP listener that accepts connections, holds them open and never
/// sends a byte: a peer that stalls a handshake.
pub(crate) async fn silent_listener() -> (tokio::task::JoinHandle<()>, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            held.push(conn);
        }
    });
    (task, addr)
}

/// A loopback port nothing listens on.
pub(crate) async fn closed_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("addr")
}

/// A TLS connection accepted by the test server.
struct ServerConn(tokio_rustls::server::TlsStream<TcpStream>);

impl Connected for ServerConn {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for ServerConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for ServerConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// How the test TLS server treats client certificates.
pub(crate) enum ClientAuth {
    /// Required, chained to one of these.
    Required(Vec<CertificateDer<'static>>),
    /// Requested, verified if presented, not required.
    Optional(Vec<CertificateDer<'static>>),
    /// Required, but any certificate is accepted (misconfigured plugin).
    AcceptAny,
    /// Required, and must be exactly this certificate (pinning).
    Pinned(CertificateDer<'static>),
    /// Never requested.
    Off,
}

/// Requires a client certificate but does not chain it: accepts any, or
/// exactly the pinned one.
#[derive(Debug)]
struct LaxVerifier {
    pin: Option<CertificateDer<'static>>,
}

impl ClientCertVerifier for LaxVerifier {
    fn root_hint_subjects(&self) -> &[TlsName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        match &self.pin {
            Some(pin) if pin.as_ref() != end_entity.as_ref() => Err(
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
            ),
            _ => Ok(ClientCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn webpki_verifier(
    roots: Vec<CertificateDer<'static>>,
    optional: bool,
) -> Arc<dyn ClientCertVerifier> {
    let mut store = RootCertStore::empty();
    for root in roots {
        store.add(root).expect("root");
    }
    let builder = WebPkiClientVerifier::builder_with_provider(Arc::new(store), provider());
    let builder = if optional {
        builder.allow_unauthenticated()
    } else {
        builder
    };
    builder.build().expect("client verifier")
}

/// The test TLS server's configuration.
pub(crate) struct TlsServer {
    pub identity: Issued,
    pub client_auth: ClientAuth,
    pub tls12_only: bool,
}

/// Serve `plugin` over TLS on a loopback port. Failed handshakes are
/// counted and skipped.
pub(crate) async fn serve_tls(
    plugin: &Arc<FakePlugin>,
    server: TlsServer,
) -> (Running, SocketAddr) {
    let version = if server.tls12_only {
        &rustls::version::TLS12
    } else {
        &rustls::version::TLS13
    };
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[version])
        .expect("versions");
    let builder = match server.client_auth {
        ClientAuth::Required(roots) => {
            builder.with_client_cert_verifier(webpki_verifier(roots, false))
        }
        ClientAuth::Optional(roots) => {
            builder.with_client_cert_verifier(webpki_verifier(roots, true))
        }
        ClientAuth::AcceptAny => {
            builder.with_client_cert_verifier(Arc::new(LaxVerifier { pin: None }))
        }
        ClientAuth::Pinned(pin) => {
            builder.with_client_cert_verifier(Arc::new(LaxVerifier { pin: Some(pin) }))
        }
        ClientAuth::Off => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(vec![server.identity.der.clone()], server.identity.key_der())
        .expect("server cert");
    config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = (listener, acceptor, Arc::clone(plugin));
    let incoming = futures::stream::unfold(state, |(l, a, p)| async move {
        loop {
            let tcp = match l.accept().await {
                Ok((tcp, _)) => tcp,
                Err(e) => return Some((Err(e), (l, a, p))),
            };
            match a.accept(tcp).await {
                Ok(tls) => {
                    if let Some(cert) = tls.get_ref().1.peer_certificates().and_then(|c| c.first())
                    {
                        p.peer_certs
                            .lock()
                            .expect("lock")
                            .push(cert.clone().into_owned());
                    }
                    return Some((Ok(ServerConn(tls)), (l, a, p)));
                }
                Err(_) => {
                    p.failed_handshakes.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    });
    (Running::spawn(Arc::clone(plugin), incoming), addr)
}

/// A certificate and its key.
#[derive(Debug, Clone)]
pub(crate) struct Issued {
    pub cert_pem: String,
    pub key_pem: String,
    pub der: CertificateDer<'static>,
}

impl Issued {
    pub(crate) fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).expect("key pem")
    }
}

/// A calendar date as `(year, month, day)`.
pub(crate) type Ymd = (i32, u8, u8);

/// The calendar date of `at`.
pub(crate) fn ymd(at: chrono::DateTime<chrono::Utc>) -> Ymd {
    use chrono::Datelike;
    (at.year(), at.month() as u8, at.day() as u8)
}

/// What a leaf certificate says.
pub(crate) struct Leaf {
    pub cn: &'static str,
    pub dns: Vec<&'static str>,
    pub uri: Option<&'static str>,
    pub usage: ExtendedKeyUsagePurpose,
    /// `(not_before, not_after)` as `(year, month, day)`; `None` is
    /// rcgen's default (1975 to 4096).
    pub window: Option<(Ymd, Ymd)>,
}

impl Leaf {
    pub(crate) fn server(dns: &'static str) -> Self {
        Leaf {
            cn: "plugin",
            dns: vec![dns],
            uri: None,
            usage: ExtendedKeyUsagePurpose::ServerAuth,
            window: None,
        }
    }

    pub(crate) fn client() -> Self {
        Leaf {
            cn: "janus-daemon",
            dns: Vec::new(),
            uri: None,
            usage: ExtendedKeyUsagePurpose::ClientAuth,
            window: None,
        }
    }

    fn params(&self) -> CertificateParams {
        let mut params =
            CertificateParams::new(self.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>())
                .expect("params");
        if let Some(uri) = self.uri {
            params
                .subject_alt_names
                .push(SanType::URI(uri.try_into().expect("uri")));
        }
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, self.cn);
        params.distinguished_name = dn;
        params.extended_key_usages = vec![self.usage.clone()];
        if let Some(((y1, m1, d1), (y2, m2, d2))) = self.window {
            params.not_before = rcgen::date_time_ymd(y1, m1, d1);
            params.not_after = rcgen::date_time_ymd(y2, m2, d2);
        }
        params
    }

    /// A self-signed certificate.
    pub(crate) fn self_signed(&self) -> Issued {
        let key = KeyPair::generate().expect("key");
        let cert = self.params().self_signed(&key).expect("self-signed");
        Issued {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            der: cert.der().clone(),
        }
    }
}

/// A private CA.
pub(crate) struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl Ca {
    pub(crate) fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().expect("key");
        Ca {
            issuer: CertifiedIssuer::self_signed(params, key).expect("CA"),
        }
    }

    pub(crate) fn pem(&self) -> String {
        self.issuer.pem()
    }

    pub(crate) fn der(&self) -> CertificateDer<'static> {
        self.issuer.der().clone()
    }

    pub(crate) fn issue(&self, leaf: &Leaf) -> Issued {
        let key = KeyPair::generate().expect("key");
        let cert = leaf.params().signed_by(&key, &self.issuer).expect("signed");
        Issued {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            der: cert.der().clone(),
        }
    }
}

/// Write `contents` to `dir/name` with `mode`.
pub(crate) fn write(dir: &Path, name: &str, contents: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Files for the daemon's side of mutual TLS.
pub(crate) struct ClientFiles {
    pub trust: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Write the daemon's trust anchor PEM and client identity into `dir`.
pub(crate) fn client_files(dir: &Path, trust_pem: &str, client: &Issued) -> ClientFiles {
    ClientFiles {
        trust: write(dir, "trust.pem", trust_pem, 0o644),
        cert: write(dir, "client.pem", &client.cert_pem, 0o644),
        key: write(dir, "client-key.pem", &client.key_pem, 0o600),
    }
}

pub(crate) fn request(caller: &str) -> ResolveRoute {
    ResolveRoute {
        request_id: format!("req-{caller}"),
        upstream: "anthropic".into(),
        implementation: "anthropic".into(),
        caller_identity: caller.into(),
        correlation_id: Some("conv-1".into()),
        identity_claims: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
    }
}

pub(crate) fn event(kind: &str) -> NotifyEvent {
    NotifyEvent {
        event_type: kind.into(),
        request_id: Some("req-1".into()),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

/// Poll `cond` until it holds or `within` passes; returns whether it held.
pub(crate) async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

/// A directory fit to hold a plugin socket (`0700`, owned by us).
pub(crate) fn socket_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("plugin.sock");
    (tmp, path)
}

/// The `PluginTransport` for a TLS plugin at `addr`.
pub(crate) fn tls_transport(
    addr: SocketAddr,
    files: &ClientFiles,
    pinned: bool,
    server_name: Option<&str>,
    expected_san: Option<&str>,
) -> PluginTransport {
    let trust = if pinned {
        TrustAnchor::PinnedCert(files.trust.clone())
    } else {
        TrustAnchor::CaFile(files.trust.clone())
    };
    PluginTransport::Tls {
        endpoint: url::Url::parse(&format!("https://{addr}")).expect("url"),
        trust,
        client_cert: files.cert.clone(),
        client_key: files.key.clone(),
        server_name: server_name.map(str::to_owned),
        expected_san: expected_san.map(str::to_owned),
    }
}

/// A test CA, a daemon identity it issued, and the daemon's files trusting
/// it.
pub(crate) struct TlsFixture {
    pub dir: tempfile::TempDir,
    pub ca: Ca,
    pub client: Issued,
    pub files: ClientFiles,
}

impl TlsFixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = Ca::new("plugin test CA");
        let client = ca.issue(&Leaf::client());
        let files = client_files(dir.path(), &ca.pem(), &client);
        TlsFixture {
            dir,
            ca,
            client,
            files,
        }
    }

    /// A server identity for `127.0.0.1`, issued by the fixture's CA.
    pub(crate) fn server_identity(&self) -> Issued {
        self.ca.issue(&Leaf::server("127.0.0.1"))
    }

    /// Serve `plugin` with the fixture CA's server identity and
    /// `client_auth`.
    pub(crate) async fn serve(
        &self,
        plugin: &Arc<FakePlugin>,
        client_auth: ClientAuth,
    ) -> (Running, SocketAddr) {
        let server = TlsServer {
            identity: self.server_identity(),
            client_auth,
            tls12_only: false,
        };
        serve_tls(plugin, server).await
    }

    /// Requires client certificates from the fixture's CA.
    pub(crate) fn strict(&self) -> ClientAuth {
        ClientAuth::Required(vec![self.ca.der()])
    }

    /// The daemon's client for the plugin at `addr`, trusting the CA.
    pub(crate) fn client(&self, addr: SocketAddr) -> GrpcPluginClient {
        GrpcPluginClient::new(
            &tls_transport(addr, &self.files, false, None, None),
            Duration::from_secs(5),
        )
        .expect("client")
    }
}
