//! The network transport's mutual TLS (`docs/plugin-contract#channel-network`).
//!
//! - The plugin is verified against exactly one operator-named trust anchor:
//!   `ca_file` (a private CA, whose chain must reach it) or `pinned_cert`
//!   (the plugin's own certificate, matched byte for byte). System roots
//!   are never loaded.
//! - The plugin's identity is checked against the endpoint's host, or
//!   `server_name` when set; `expected_san` replaces that check with an
//!   exact URI-SAN match. `server_name` (else the host) is also the SNI.
//! - The daemon presents `client_cert` / `client_key`, and refuses a plugin
//!   whose handshake never asked for a client certificate
//!   (`plugin_no_client_auth`). Session resumption is disabled so every
//!   handshake is a full one in which that request is visible.
//! - TLS 1.3 only.
//!
//! Material is validated when the client is built (files exist and parse,
//! key matches certificate, certificate not expired or not yet valid, key
//! mode owner-only). It is re-read by [`TlsDialer::reload`] — called when
//! the files change on disk and on `SIGHUP` — and the last good material is
//! kept when the new files fail validation. New material applies to new
//! connections; the gRPC client reconnects after a reload.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use janus_core::contract::plugin::{PluginError, PluginFailureClass};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ResolvesClientCert, Resumption};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::sign::CertifiedKey;
use rustls::{
    AlertDescription, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use url::{Host, Url};

use crate::contract::config::{invalid, ConfigError, TrustAnchor};

/// A certificate within this long of expiry is warned about.
pub const EXPIRY_WARNING: chrono::TimeDelta = chrono::TimeDelta::days(30);

/// The crypto provider for every plugin TLS connection.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The `[plugin.tls]` settings of an `https://` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSettings {
    pub endpoint: Url,
    pub trust: TrustAnchor,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub server_name: Option<String>,
    pub expected_san: Option<String>,
}

/// What the daemon presents in a handshake.
#[derive(Debug, Clone)]
pub(crate) enum ClientIdentity {
    /// `client_cert` / `client_key`.
    Configured,
    /// Some other certificate (the `plugin check` throwaway probe).
    Other(Arc<CertifiedKey>),
    /// No certificate (the `plugin check` no-cert probe).
    Absent,
}

/// A completed handshake and what it revealed.
pub(crate) struct Handshake {
    pub stream: TlsStream<TcpStream>,
    /// Whether the plugin sent a CertificateRequest.
    pub client_auth_requested: bool,
}

#[derive(Debug, Clone)]
enum Anchor {
    Roots(Arc<RootCertStore>),
    Pinned(CertificateDer<'static>),
}

#[derive(Debug, Clone)]
enum PeerIdentity {
    Name(ServerName<'static>),
    UriSan(String),
}

/// Modification stamp of one material file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        path: path.to_path_buf(),
        modified: meta.modified().ok(),
        len: meta.len(),
        inode: meta.ino(),
    })
}

/// Loaded, validated certificate material.
#[derive(Debug)]
struct Material {
    key: Arc<CertifiedKey>,
    client_not_after: DateTime<Utc>,
    anchor: Anchor,
    stamps: Vec<Option<Stamp>>,
}

/// Validity window of a DER certificate.
pub(crate) fn validity(der: &[u8]) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| format!("not an X.509 certificate: {e}"))?;
    let v = cert.validity();
    let from = DateTime::from_timestamp(v.not_before.timestamp(), 0)
        .ok_or_else(|| "notBefore out of range".to_owned())?;
    let until = DateTime::from_timestamp(v.not_after.timestamp(), 0)
        .ok_or_else(|| "notAfter out of range".to_owned())?;
    Ok((from, until))
}

fn uri_sans(der: &[u8]) -> Vec<String> {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return Vec::new();
    };
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return Vec::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::URI(u) => Some((*u).to_owned()),
            _ => None,
        })
        .collect()
}

fn read(path: &Path, at: &str) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|e| invalid(at, format!("cannot read {}: {e}", path.display())))
}

fn certs(path: &Path, at: &str) -> Result<Vec<CertificateDer<'static>>, ConfigError> {
    let bytes = read(path, at)?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(at, format!("{} does not parse as PEM: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(invalid(
            at,
            format!("{} contains no certificate", path.display()),
        ));
    }
    Ok(certs)
}

/// Refuse a certificate outside its validity window.
fn check_current(der: &[u8], path: &Path, at: &str) -> Result<DateTime<Utc>, ConfigError> {
    let (from, until) =
        validity(der).map_err(|e| invalid(at, format!("{}: {e}", path.display())))?;
    let now = Utc::now();
    if until < now {
        return Err(invalid(
            at,
            format!("{} expired at {until}", path.display()),
        ));
    }
    if from > now {
        return Err(invalid(
            at,
            format!("{} is not valid until {from}", path.display()),
        ));
    }
    Ok(until)
}

pub(crate) fn warn_if_expiring(which: &str, until: DateTime<Utc>) {
    let left = until - Utc::now();
    if left < EXPIRY_WARNING {
        tracing::warn!(
            certificate = which,
            expires_at = %until,
            "plugin TLS certificate expires in {} days",
            left.num_days()
        );
    }
}

impl Material {
    fn load(s: &TlsSettings, provider: &CryptoProvider) -> Result<Self, ConfigError> {
        // Stamped before reading, so a change made while loading is seen
        // as a change by the next check.
        let stamps = stamps(s, anchor_path(&s.trust));
        let key_meta = std::fs::metadata(&s.client_key).map_err(|e| {
            invalid(
                "plugin.tls.client_key",
                format!("cannot read {}: {e}", s.client_key.display()),
            )
        })?;
        let mode = key_meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(invalid(
                "plugin.tls.client_key",
                format!(
                    "{} must be mode 0600 (owner-only); it is {mode:04o}",
                    s.client_key.display()
                ),
            ));
        }
        let key_bytes = read(&s.client_key, "plugin.tls.client_key")?;
        let key = PrivateKeyDer::from_pem_slice(&key_bytes).map_err(|e| {
            invalid(
                "plugin.tls.client_key",
                format!("{} is not a PEM private key: {e}", s.client_key.display()),
            )
        })?;
        let chain = certs(&s.client_cert, "plugin.tls.client_cert")?;
        let client_not_after = check_current(&chain[0], &s.client_cert, "plugin.tls.client_cert")?;
        let key = CertifiedKey::from_der(chain, key, provider).map_err(|e| {
            invalid(
                "plugin.tls.client_key",
                format!(
                    "{} does not match {}: {e}",
                    s.client_key.display(),
                    s.client_cert.display()
                ),
            )
        })?;
        let anchor = match &s.trust {
            TrustAnchor::CaFile(path) => {
                let mut roots = RootCertStore::empty();
                for cert in certs(path, "plugin.tls.ca_file")? {
                    roots.add(cert).map_err(|e| {
                        invalid(
                            "plugin.tls.ca_file",
                            format!("{} holds an unusable CA certificate: {e}", path.display()),
                        )
                    })?;
                }
                Anchor::Roots(Arc::new(roots))
            }
            TrustAnchor::PinnedCert(path) => {
                let mut pinned = certs(path, "plugin.tls.pinned_cert")?;
                if pinned.len() != 1 {
                    return Err(invalid(
                        "plugin.tls.pinned_cert",
                        format!(
                            "{} must hold exactly the plugin's one certificate, not {}",
                            path.display(),
                            pinned.len()
                        ),
                    ));
                }
                let cert = pinned.remove(0);
                validity(&cert).map_err(|e| {
                    invalid("plugin.tls.pinned_cert", format!("{}: {e}", path.display()))
                })?;
                Anchor::Pinned(cert)
            }
        };
        warn_if_expiring("client", client_not_after);
        Ok(Material {
            key: Arc::new(key),
            client_not_after,
            anchor,
            stamps,
        })
    }
}

fn stamps(s: &TlsSettings, anchor: &Path) -> Vec<Option<Stamp>> {
    [s.client_cert.as_path(), s.client_key.as_path(), anchor]
        .into_iter()
        .map(stamp)
        .collect()
}

fn anchor_path(trust: &TrustAnchor) -> &Path {
    match trust {
        TrustAnchor::CaFile(p) | TrustAnchor::PinnedCert(p) => p,
    }
}

/// Verifies the plugin's certificate against the configured anchor and
/// identity, and remembers its expiry.
#[derive(Debug)]
struct PluginVerifier {
    anchor: Anchor,
    identity: PeerIdentity,
    provider: Arc<CryptoProvider>,
    server_not_after: Arc<Mutex<Option<DateTime<Utc>>>>,
}

impl ServerCertVerifier for PluginVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let parsed = ParsedCertificate::try_from(end_entity)?;
        let (from, until) = validity(end_entity).map_err(rustls::Error::General)?;
        match &self.anchor {
            Anchor::Roots(roots) => rustls::client::verify_server_cert_signed_by_trust_anchor(
                &parsed,
                roots,
                intermediates,
                now,
                self.provider.signature_verification_algorithms.all,
            )?,
            Anchor::Pinned(pinned) => {
                if end_entity.as_ref() != pinned.as_ref() {
                    return Err(rustls::Error::General(
                        "the plugin's certificate is not the pinned_cert".to_owned(),
                    ));
                }
                let now = DateTime::from_timestamp(now.as_secs() as i64, 0).unwrap_or_default();
                if until < now {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::Expired,
                    ));
                }
                if from > now {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::NotValidYet,
                    ));
                }
            }
        }
        match &self.identity {
            PeerIdentity::Name(name) => rustls::client::verify_server_name(&parsed, name)?,
            PeerIdentity::UriSan(want) => {
                let presented = uri_sans(end_entity);
                if !presented.iter().any(|u| u == want) {
                    return Err(rustls::Error::General(format!(
                        "the plugin's certificate does not carry URI SAN {want:?} (presented: {presented:?})"
                    )));
                }
            }
        }
        let mut seen = self
            .server_not_after
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *seen != Some(until) {
            warn_if_expiring("server", until);
            *seen = Some(until);
        }
        Ok(ServerCertVerified::assertion())
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
            &self.provider.signature_verification_algorithms,
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
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Presents a fixed certificate (or none) and records whether the plugin
/// asked for one.
#[derive(Debug)]
struct RecordingResolver {
    key: Option<Arc<CertifiedKey>>,
    requested: Arc<AtomicBool>,
}

impl ResolvesClientCert for RecordingResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        self.requested.store(true, Ordering::SeqCst);
        self.key.clone()
    }

    fn has_certs(&self) -> bool {
        self.key.is_some()
    }
}

/// Alerts with which a TLS server refuses the client's certificate (or its
/// absence).
fn rejects_client_cert(alert: AlertDescription) -> bool {
    matches!(
        alert,
        AlertDescription::BadCertificate
            | AlertDescription::UnsupportedCertificate
            | AlertDescription::CertificateRevoked
            | AlertDescription::CertificateExpired
            | AlertDescription::CertificateUnknown
            | AlertDescription::UnknownCA
            | AlertDescription::AccessDenied
            | AlertDescription::CertificateRequired
    )
}

fn rustls_error(err: &io::Error) -> Option<&rustls::Error> {
    err.get_ref()
        .and_then(|e| e.downcast_ref::<rustls::Error>())
}

/// Classify an I/O failure on an established TLS connection. Under TLS 1.3
/// a server's refusal of the client certificate arrives here, after the
/// client's side of the handshake has finished.
pub(crate) fn classify_post_handshake(err: &io::Error) -> PluginError {
    match rustls_error(err) {
        Some(rustls::Error::AlertReceived(alert)) if rejects_client_cert(*alert) => {
            PluginError::new(
                PluginFailureClass::RejectedClientCert,
                format!(
                    "the plugin rejected the daemon's client certificate (TLS alert {alert:?})"
                ),
            )
        }
        Some(tls) => PluginError::new(
            PluginFailureClass::TlsError,
            format!("TLS error on the plugin connection: {tls}"),
        ),
        None => PluginError::new(
            PluginFailureClass::Unreachable,
            format!("plugin connection failed: {err}"),
        ),
    }
}

/// Dials the plugin over mutual TLS and owns the rotating material.
#[derive(Debug)]
pub struct TlsDialer {
    settings: TlsSettings,
    provider: Arc<CryptoProvider>,
    host: String,
    port: u16,
    sni: ServerName<'static>,
    identity: PeerIdentity,
    material: RwLock<Arc<Material>>,
    server_not_after: Arc<Mutex<Option<DateTime<Utc>>>>,
}

fn server_name(name: &str, at: &str) -> Result<ServerName<'static>, ConfigError> {
    ServerName::try_from(name.to_owned())
        .map_err(|e| invalid(at, format!("{name:?} is not a valid TLS server name: {e}")))
}

impl TlsDialer {
    /// Validate the settings and load the material: the startup half.
    pub fn new(settings: TlsSettings) -> Result<Self, ConfigError> {
        let provider = provider();
        let (host, sni_default) = match settings.endpoint.host() {
            Some(Host::Domain(d)) => (d.to_owned(), d.to_owned()),
            Some(Host::Ipv4(ip)) => (ip.to_string(), ip.to_string()),
            Some(Host::Ipv6(ip)) => (ip.to_string(), ip.to_string()),
            None => return Err(invalid("plugin.endpoint", "https endpoint has no host")),
        };
        let port = settings
            .endpoint
            .port_or_known_default()
            .ok_or_else(|| invalid("plugin.endpoint", "https endpoint has no port"))?;
        let sni = match &settings.server_name {
            Some(n) => server_name(n, "plugin.tls.server_name")?,
            None => server_name(&sni_default, "plugin.endpoint")?,
        };
        let identity = match &settings.expected_san {
            Some(san) if san.is_empty() => {
                return Err(invalid("plugin.tls.expected_san", "must not be empty"))
            }
            Some(san) => PeerIdentity::UriSan(san.clone()),
            None => PeerIdentity::Name(sni.clone()),
        };
        let material = Material::load(&settings, &provider)?;
        Ok(TlsDialer {
            settings,
            provider,
            host,
            port,
            sni,
            identity,
            material: RwLock::new(Arc::new(material)),
            server_not_after: Arc::new(Mutex::new(None)),
        })
    }

    fn current(&self) -> Arc<Material> {
        Arc::clone(&self.material.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// `host:port` the dialer connects to.
    pub fn authority(&self) -> String {
        match self.settings.endpoint.host() {
            Some(Host::Ipv6(ip)) => format!("[{ip}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    /// Whether any material file changed on disk since it was last loaded.
    pub fn files_changed(&self) -> bool {
        let current = self.current();
        stamps(&self.settings, anchor_path(&self.settings.trust)) != current.stamps
    }

    /// Re-read the material. On failure the last good material stays in
    /// use and the error is returned (and should be logged).
    pub fn reload(&self) -> Result<(), ConfigError> {
        let fresh = Material::load(&self.settings, &self.provider)?;
        *self.material.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(fresh);
        Ok(())
    }

    /// Expiry of the client certificate in use.
    pub fn client_cert_expires_at(&self) -> DateTime<Utc> {
        self.current().client_not_after
    }

    /// Expiry of the plugin certificate, once a handshake has seen it.
    pub fn server_cert_expires_at(&self) -> Option<DateTime<Utc>> {
        *self
            .server_not_after
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn client_config(
        &self,
        material: &Material,
        presented: &ClientIdentity,
        requested: Arc<AtomicBool>,
    ) -> Result<ClientConfig, rustls::Error> {
        let key = match presented {
            ClientIdentity::Configured => Some(Arc::clone(&material.key)),
            ClientIdentity::Other(k) => Some(Arc::clone(k)),
            ClientIdentity::Absent => None,
        };
        let verifier = PluginVerifier {
            anchor: material.anchor.clone(),
            identity: self.identity.clone(),
            provider: Arc::clone(&self.provider),
            server_not_after: Arc::clone(&self.server_not_after),
        };
        let mut config = ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_cert_resolver(Arc::new(RecordingResolver { key, requested }));
        config.alpn_protocols = vec![b"h2".to_vec()];
        config.resumption = Resumption::disabled();
        Ok(config)
    }

    /// TCP connect and a full TLS 1.3 handshake presenting `presented`.
    /// Fails as `plugin_unreachable` (TCP) or `plugin_tls_error`
    /// (handshake). Whether the plugin asked for a client certificate is
    /// reported, not enforced: [`TlsDialer::connect`] enforces it.
    pub(crate) async fn handshake(
        &self,
        presented: &ClientIdentity,
    ) -> Result<Handshake, PluginError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| {
                PluginError::new(
                    PluginFailureClass::Unreachable,
                    format!("cannot connect to plugin at {}: {e}", self.authority()),
                )
            })?;
        // Nagle only adds latency to small gRPC frames.
        let _ = tcp.set_nodelay(true);
        let requested = Arc::new(AtomicBool::new(false));
        let material = self.current();
        let config = self
            .client_config(&material, presented, Arc::clone(&requested))
            .map_err(|e| {
                PluginError::new(
                    PluginFailureClass::TlsError,
                    format!("TLS configuration: {e}"),
                )
            })?;
        let stream = TlsConnector::from(Arc::new(config))
            .connect(self.sni.clone(), tcp)
            .await
            .map_err(|e| {
                let detail = match rustls_error(&e) {
                    Some(tls) => tls.to_string(),
                    None => e.to_string(),
                };
                PluginError::new(
                    PluginFailureClass::TlsError,
                    format!(
                        "TLS handshake with plugin at {} failed: {detail}",
                        self.authority()
                    ),
                )
            })?;
        Ok(Handshake {
            stream,
            client_auth_requested: requested.load(Ordering::SeqCst),
        })
    }

    /// The connection the gRPC client uses: a handshake with the configured
    /// material from a plugin that requested a client certificate.
    pub(crate) async fn connect(&self) -> Result<TlsStream<TcpStream>, PluginError> {
        let hs = self.handshake(&ClientIdentity::Configured).await?;
        if !hs.client_auth_requested {
            return Err(PluginError::new(
                PluginFailureClass::NoClientAuth,
                format!(
                    "the plugin at {} did not request a client certificate; it must verify the daemon",
                    self.authority()
                ),
            ));
        }
        Ok(hs.stream)
    }
}

/// How long a [`TlsDialer`] waits between checks of its files for change.
pub const MATERIAL_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use janus_core::contract::plugin::{PluginClient, PluginMode, RouteOutcome};

    use super::*;
    use crate::plugin::grpc::GrpcPluginClient;
    use crate::plugin::testutil::{
        client_files, request, serve_tls, tls_transport, write, Ca, ClientAuth, FakePlugin, Issued,
        Leaf, TlsFixture, TlsServer,
    };
    use crate::plugin::{tls_init, SetupError, TlsInitOptions};

    const T: Duration = Duration::from_secs(5);

    async fn call(client: &GrpcPluginClient) -> Result<RouteOutcome, PluginError> {
        client.resolve_route(request("session-1")).await
    }

    async fn fails_as(client: &GrpcPluginClient, class: PluginFailureClass) -> PluginError {
        let err = call(client).await.expect_err("must fail closed");
        assert_eq!(err.class, class, "{err:?}");
        err
    }

    fn build(
        transport: &crate::contract::config::PluginTransport,
    ) -> Result<GrpcPluginClient, SetupError> {
        GrpcPluginClient::new(transport, T)
    }

    #[tokio::test]
    async fn mtls_with_private_ca_round_trips_and_reports_expiry() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("cred-tls");
        let (_server, addr) = fx.serve(&plugin, fx.strict()).await;
        let client = fx.client(addr);
        assert_eq!(client.mode(), PluginMode::Tls);
        assert_eq!(client.server_cert_expires_at(), None, "no handshake yet");
        assert_eq!(
            call(&client).await,
            Ok(RouteOutcome::Credential("cred-tls".into()))
        );
        assert_eq!(
            *plugin.peer_certs.lock().expect("lock"),
            vec![fx.client.der.clone()]
        );
        let (_, client_until) = validity(&fx.client.der).expect("validity");
        assert_eq!(client.client_cert_expires_at(), Some(client_until));
        assert!(client.server_cert_expires_at().is_some());
    }

    #[tokio::test]
    async fn mtls_with_pinned_certs_from_tls_init() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tls_init(&TlsInitOptions {
            plugin_host: "127.0.0.1".into(),
            out_dir: tmp.path().join("tls"),
            days: 30,
        })
        .expect("tls init");
        let read = |p: &Path| std::fs::read_to_string(p).expect("read");
        let identity = Issued {
            cert_pem: read(&out.plugin_cert),
            key_pem: read(&out.plugin_key),
            der: CertificateDer::from_pem_slice(read(&out.plugin_cert).as_bytes()).expect("pem"),
        };
        let daemon_der =
            CertificateDer::from_pem_slice(read(&out.daemon_cert).as_bytes()).expect("pem");
        let plugin = FakePlugin::credential("pinned");
        let server = TlsServer {
            identity: identity.clone(),
            client_auth: ClientAuth::Pinned(daemon_der),
            tls12_only: false,
        };
        let (_server, addr) = serve_tls(&plugin, server).await;
        // The printed block, as an operator would paste it.
        let config: crate::contract::config::PluginConfig = toml::from_str(&format!(
            "endpoint = \"https://{addr}\"\n{}",
            out.config_block.replace("[plugin.tls]", "[tls]")
        ))
        .expect("config");
        let client = build(&config.transport().expect("transport")).expect("client");
        assert_eq!(
            call(&client).await,
            Ok(RouteOutcome::Credential("pinned".into()))
        );

        // A plugin pinning some other daemon certificate refuses this one.
        let other = Leaf::client().self_signed();
        let server = TlsServer {
            identity,
            client_auth: ClientAuth::Pinned(other.der),
            tls12_only: false,
        };
        let (_other, addr) = serve_tls(&plugin, server).await;
        let config: crate::contract::config::PluginConfig = toml::from_str(&format!(
            "endpoint = \"https://{addr}\"\n{}",
            out.config_block.replace("[plugin.tls]", "[tls]")
        ))
        .expect("config");
        let client = build(&config.transport().expect("transport")).expect("client");
        fails_as(&client, PluginFailureClass::RejectedClientCert).await;
    }

    #[tokio::test]
    async fn mtls_refuses_server_without_client_auth() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("never");
        let (_server, addr) = fx.serve(&plugin, ClientAuth::Off).await;
        let client = fx.client(addr);
        let err = fails_as(&client, PluginFailureClass::NoClientAuth).await;
        assert!(
            err.message.contains("did not request a client certificate"),
            "{err}"
        );
        assert_eq!(
            plugin.request_count(),
            0,
            "no request sent to an unauthenticating plugin"
        );
    }

    #[tokio::test]
    async fn rejected_client_cert_classified() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("never");
        let other_ca = Ca::new("some other CA");
        let (_server, addr) = fx
            .serve(&plugin, ClientAuth::Required(vec![other_ca.der()]))
            .await;
        let client = fx.client(addr);
        let err = fails_as(&client, PluginFailureClass::RejectedClientCert).await;
        assert!(
            err.message
                .contains("rejected the daemon's client certificate"),
            "{err}"
        );
        assert_eq!(plugin.request_count(), 0);
        assert!(plugin.failed_handshakes.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn plugin_cert_from_wrong_ca_is_tls_error() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("never");
        let impostor = Ca::new("impostor CA");
        let server = TlsServer {
            identity: impostor.issue(&Leaf::server("127.0.0.1")),
            client_auth: fx.strict(),
            tls12_only: false,
        };
        let (_server, addr) = serve_tls(&plugin, server).await;
        let err = fails_as(&fx.client(addr), PluginFailureClass::TlsError).await;
        assert!(err.message.contains("TLS handshake"), "{err}");
        assert_eq!(plugin.request_count(), 0);
    }

    #[tokio::test]
    async fn server_name_override_is_the_name_checked() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("named");
        let server = TlsServer {
            identity: fx.ca.issue(&Leaf::server("plugin.internal")),
            client_auth: fx.strict(),
            tls12_only: false,
        };
        let (_server, addr) = serve_tls(&plugin, server).await;
        // The certificate does not name 127.0.0.1.
        fails_as(&fx.client(addr), PluginFailureClass::TlsError).await;
        let client = build(&tls_transport(
            addr,
            &fx.files,
            false,
            Some("plugin.internal"),
            None,
        ))
        .expect("client");
        assert_eq!(
            call(&client).await,
            Ok(RouteOutcome::Credential("named".into()))
        );
    }

    #[tokio::test]
    async fn expected_san_replaces_the_hostname_check() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("spiffe");
        let leaf = Leaf {
            uri: Some("spiffe://example.org/janus-plugin"),
            ..Leaf::server("unrelated.example")
        };
        let server = TlsServer {
            identity: fx.ca.issue(&leaf),
            client_auth: fx.strict(),
            tls12_only: false,
        };
        let (_server, addr) = serve_tls(&plugin, server).await;
        let with_san =
            |san| build(&tls_transport(addr, &fx.files, false, None, Some(san))).expect("client");
        let client = with_san("spiffe://example.org/janus-plugin");
        assert_eq!(
            call(&client).await,
            Ok(RouteOutcome::Credential("spiffe".into()))
        );
        let err = fails_as(
            &with_san("spiffe://example.org/other"),
            PluginFailureClass::TlsError,
        )
        .await;
        assert!(err.message.contains("URI SAN"), "{err}");
        let err = build(&tls_transport(addr, &fx.files, false, None, Some(""))).expect_err("empty");
        assert!(err.to_string().contains("expected_san"), "{err}");
    }

    #[tokio::test]
    async fn tls12_only_plugin_is_refused() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("never");
        let server = TlsServer {
            identity: fx.server_identity(),
            client_auth: fx.strict(),
            tls12_only: true,
        };
        let (_server, addr) = serve_tls(&plugin, server).await;
        fails_as(&fx.client(addr), PluginFailureClass::TlsError).await;
    }

    #[tokio::test]
    async fn material_is_validated_when_the_client_is_built() {
        let fx = TlsFixture::new();
        let addr: std::net::SocketAddr = "127.0.0.1:9".parse().expect("addr");
        let dir = fx.dir.path();
        let refused = |files: &crate::plugin::testutil::ClientFiles, pinned: bool| {
            build(&tls_transport(addr, files, pinned, None, None))
                .expect_err("refused")
                .to_string()
        };

        let loose = write(dir, "loose-key.pem", &fx.client.key_pem, 0o640);
        let files = crate::plugin::testutil::ClientFiles {
            key: loose,
            ..client_files(dir, &fx.ca.pem(), &fx.client)
        };
        assert!(refused(&files, false).contains("0600"));

        let stranger = fx.ca.issue(&Leaf::client());
        let mismatched = write(dir, "stranger-key.pem", &stranger.key_pem, 0o600);
        let files = crate::plugin::testutil::ClientFiles {
            key: mismatched,
            ..client_files(dir, &fx.ca.pem(), &fx.client)
        };
        assert!(refused(&files, false).contains("does not match"));

        let expired = fx.ca.issue(&Leaf {
            window: Some(((2020, 1, 1), (2021, 1, 1))),
            ..Leaf::client()
        });
        assert!(refused(&client_files(dir, &fx.ca.pem(), &expired), false).contains("expired"));

        let two = format!("{}{}", fx.ca.pem(), fx.client.cert_pem);
        assert!(refused(&client_files(dir, &two, &fx.client), true).contains("exactly"));

        let files = crate::plugin::testutil::ClientFiles {
            trust: dir.join("missing.pem"),
            ..client_files(dir, &fx.ca.pem(), &fx.client)
        };
        assert!(refused(&files, false).contains("cannot read"));
    }

    #[tokio::test]
    async fn rotated_material_is_picked_up_and_used_on_reconnect() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("rotating");
        let (_server, addr) = fx.serve(&plugin, fx.strict()).await;
        let client = fx.client(addr);
        call(&client).await.expect("first identity");
        let rotated = fx.ca.issue(&Leaf::client());
        // Written the way `tls init` writes: new files renamed into place.
        let staged = tempfile::tempdir_in(fx.dir.path()).expect("stage");
        let fresh = client_files(staged.path(), &fx.ca.pem(), &rotated);
        std::fs::rename(&fresh.cert, &fx.files.cert).expect("rename");
        std::fs::rename(&fresh.key, &fx.files.key).expect("rename");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            call(&client).await.expect("still answering");
            if plugin.peer_certs.lock().expect("lock").last() == Some(&rotated.der) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rotation never applied"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (_, until) = validity(&rotated.der).expect("validity");
        assert_eq!(client.client_cert_expires_at(), Some(until));
    }

    #[tokio::test]
    async fn failed_reload_keeps_the_last_good_material() {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("kept");
        let (_server, addr) = fx.serve(&plugin, fx.strict()).await;
        let client = fx.client(addr);
        call(&client).await.expect("ok");
        write(fx.dir.path(), "client-key.pem", "not a key", 0o600);
        let err = client.reload_tls().expect_err("bad material");
        assert!(err.to_string().contains("client_key"), "{err}");
        // Fresh handshakes still use the old, good material.
        let check = client.check().await;
        assert!(
            check
                .step(crate::plugin::CheckStepKind::Handshake)
                .expect("ran")
                .passed,
            "{check:?}"
        );
        assert_eq!(
            call(&client).await,
            Ok(RouteOutcome::Credential("kept".into()))
        );
    }

    #[test]
    fn post_handshake_errors_are_classified() {
        let alert = |a| io::Error::new(io::ErrorKind::InvalidData, rustls::Error::AlertReceived(a));
        for a in [
            AlertDescription::BadCertificate,
            AlertDescription::UnknownCA,
            AlertDescription::CertificateRequired,
            AlertDescription::AccessDenied,
        ] {
            assert_eq!(
                classify_post_handshake(&alert(a)).class,
                PluginFailureClass::RejectedClientCert
            );
        }
        assert_eq!(
            classify_post_handshake(&alert(AlertDescription::InternalError)).class,
            PluginFailureClass::TlsError
        );
        assert_eq!(
            classify_post_handshake(&io::Error::from(io::ErrorKind::ConnectionReset)).class,
            PluginFailureClass::Unreachable
        );
    }
}
