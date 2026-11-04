//! Binding and serving the admin listeners: the unix socket at
//! `<data_dir>/admin.sock` and the TCP listeners (admin and UI), optionally
//! behind TLS. One accept loop per listener; one task per connection;
//! `cancel` stops accepting and closes open connections gracefully.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rand::RngCore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::AdminError;
use crate::contract::config::ListenerTls;

/// How long open requests get to finish once shutdown starts.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long a client gets to complete a TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an HTTP/1 client gets to send a request's headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Open connections one listener serves at once. A connection accepted
/// beyond this is closed at once, so a peer that opens connections and
/// never finishes a request cannot exhaust the daemon's file descriptors
/// (which the data plane shares). The data plane has its own, configurable
/// cap (`data_plane.max_connections`).
pub const MAX_CONNECTIONS: usize = 256;

/// `http://addr/` or `https://addr/`.
pub(crate) fn url_of(addr: SocketAddr, tls: bool) -> String {
    format!("{}://{addr}/", if tls { "https" } else { "http" })
}

/// A bound listener, ready for [`serve`].
pub(crate) enum Acceptor {
    Unix(UnixSocket),
    Tcp(TcpListener, Option<TlsAcceptor>),
}

pub(crate) async fn bind_tcp(
    listener: &'static str,
    addr: SocketAddr,
) -> Result<(TcpListener, SocketAddr), AdminError> {
    let bind_error = |source| AdminError::Bind {
        listener,
        addr,
        source,
    };
    let tcp = TcpListener::bind(addr).await.map_err(bind_error)?;
    let bound = tcp.local_addr().map_err(bind_error)?;
    Ok((tcp, bound))
}

/// A TLS acceptor over `tls.cert` (a PEM chain, leaf first) and `tls.key`.
pub(crate) fn tls_acceptor(
    listener: &'static str,
    tls: &ListenerTls,
) -> Result<TlsAcceptor, AdminError> {
    let fail = |reason: String| AdminError::Tls { listener, reason };
    let cert_pem = std::fs::read(&tls.cert)
        .map_err(|e| fail(format!("cannot read cert {}: {e}", tls.cert.display())))?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| fail(format!("cert {} is not PEM: {e}", tls.cert.display())))?;
    if chain.is_empty() {
        return Err(fail(format!(
            "cert {} holds no certificate",
            tls.cert.display()
        )));
    }
    let key_pem = std::fs::read(&tls.key)
        .map_err(|e| fail(format!("cannot read key {}: {e}", tls.key.display())))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| {
        fail(format!(
            "key {} is not a PEM private key: {e}",
            tls.key.display()
        ))
    })?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| fail(e.to_string()))?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|e| fail(format!("cert and key do not form a usable identity: {e}")))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The longest path a `sockaddr_un` holds, including the terminating NUL.
fn sun_path_capacity() -> usize {
    // SAFETY: sockaddr_un is plain data; all-zero is a valid value.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len()
}

/// Refuse a socket path longer than a `sockaddr_un` holds.
pub(crate) fn check_socket_path(path: &Path) -> Result<(), AdminError> {
    if path.as_os_str().len() >= sun_path_capacity() {
        return Err(AdminError::Socket {
            path: path.to_path_buf(),
            reason: format!(
                "the path is longer than a unix socket address holds ({} bytes); use a shorter data_dir",
                sun_path_capacity() - 1
            ),
        });
    }
    Ok(())
}

/// The admin unix socket, bound at its final path with mode `0600`.
pub(crate) struct UnixSocket {
    listener: UnixListener,
    path: PathBuf,
    /// `(dev, ino)` of the socket file this process created, so shutdown
    /// removes only its own socket.
    identity: (u64, u64),
}

impl UnixSocket {
    /// Bind `path` (inside `data_dir`).
    ///
    /// An existing socket nobody answers on is stale and replaced; one that
    /// answers belongs to a running daemon, and anything that is not a
    /// socket is left alone — both refuse. The socket is bound inside a
    /// fresh `0700` directory, set to `0600`, then renamed into place, so it
    /// is never reachable with the process umask's permissions. Its mode and
    /// owner are verified at the final path.
    pub(crate) fn bind(data_dir: &Path, path: &Path) -> Result<Self, AdminError> {
        let fail = |reason: String| AdminError::Socket {
            path: path.to_path_buf(),
            reason,
        };
        check_socket_path(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => {
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => {
                        return Err(AdminError::SocketInUse {
                            path: path.to_path_buf(),
                        })
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::NotFound
                        ) =>
                    {
                        std::fs::remove_file(path)
                            .map_err(|e| fail(format!("cannot remove the stale socket: {e}")))?;
                        tracing::info!(path = %path.display(), "removed a stale admin socket");
                    }
                    Err(e) => return Err(fail(format!("cannot probe the existing socket: {e}"))),
                }
            }
            Ok(_) => {
                return Err(fail(
                    "exists and is not a socket; refusing to replace it".into(),
                ))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(fail(format!("cannot stat: {e}"))),
        }

        let mut suffix = [0u8; 4];
        rand::rng().fill_bytes(&mut suffix);
        let staging = data_dir.join(format!(".admin-sock-{}", hex::encode(suffix)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&staging)
            .map_err(|e| fail(format!("cannot create {}: {e}", staging.display())))?;
        let staged = staging.join("s");
        let bound: Result<UnixListener, AdminError> = (|| {
            let listener =
                UnixListener::bind(&staged).map_err(|e| fail(format!("cannot bind: {e}")))?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| fail(format!("cannot set mode 0600: {e}")))?;
            std::fs::rename(&staged, path)
                .map_err(|e| fail(format!("cannot move the socket into place: {e}")))?;
            Ok(listener)
        })();
        // Leave nothing behind whichever step failed.
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir(&staging);
        let listener = bound?;

        let meta =
            std::fs::symlink_metadata(path).map_err(|e| fail(format!("cannot stat: {e}")))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let mode = meta.mode() & 0o777;
        let problem = if !meta.file_type().is_socket() {
            Some("is not a socket after binding".to_owned())
        } else if mode != 0o600 {
            Some(format!("has mode {mode:04o}, not 0600"))
        } else if meta.uid() != euid {
            Some(format!("is owned by uid {}, not {euid}", meta.uid()))
        } else {
            None
        };
        if let Some(problem) = problem {
            let _ = std::fs::remove_file(path);
            return Err(fail(problem));
        }
        Ok(UnixSocket {
            listener,
            path: path.to_path_buf(),
            identity: (meta.dev(), meta.ino()),
        })
    }

    fn remove(&self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if (meta.dev(), meta.ino()) == self.identity {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    tracing::warn!(path = %self.path.display(), error = %e, "cannot remove the admin socket");
                }
            }
        }
    }
}

/// Accept connections on `acceptor` and serve `router` on each until
/// `cancel` fires; then close every connection and, for the unix socket,
/// remove the socket file.
pub(crate) async fn serve(acceptor: Acceptor, router: Router, cancel: CancellationToken) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = accept(&acceptor) => match accepted {
                Ok(_) if conns.len() >= MAX_CONNECTIONS => {
                    // Dropping the accepted stream closes it.
                    tracing::warn!(limit = MAX_CONNECTIONS, "admin listener at its connection limit; closing a new connection");
                }
                Ok(Accepted::Unix(stream)) => {
                    conns.spawn(serve_connection(stream, router.clone(), cancel.clone()));
                }
                Ok(Accepted::Tcp(stream, peer, None)) => {
                    tracing::debug!(%peer, "admin connection");
                    conns.spawn(serve_connection(stream, router.clone(), cancel.clone()));
                }
                Ok(Accepted::Tcp(stream, peer, Some(tls))) => {
                    let (router, cancel) = (router.clone(), cancel.clone());
                    conns.spawn(async move {
                        let handshake = tokio::select! {
                            () = cancel.cancelled() => return,
                            r = tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)) => r,
                        };
                        match handshake {
                            Ok(Ok(stream)) => serve_connection(stream, router, cancel).await,
                            Ok(Err(e)) => tracing::debug!(%peer, error = %e, "admin TLS handshake failed"),
                            Err(_) => tracing::debug!(%peer, "admin TLS handshake timed out"),
                        }
                    });
                }
                Err(e) => {
                    // Typically EMFILE: back off rather than spin.
                    tracing::warn!(error = %e, "admin accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    while conns.join_next().await.is_some() {}
    if let Acceptor::Unix(socket) = &acceptor {
        socket.remove();
    }
}

enum Accepted {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream, SocketAddr, Option<TlsAcceptor>),
}

async fn accept(acceptor: &Acceptor) -> std::io::Result<Accepted> {
    match acceptor {
        Acceptor::Unix(socket) => socket
            .listener
            .accept()
            .await
            .map(|(s, _)| Accepted::Unix(s)),
        Acceptor::Tcp(listener, tls) => listener
            .accept()
            .await
            .map(|(s, peer)| Accepted::Tcp(s, peer, tls.clone())),
    }
}

/// The HTTP/1 + h2 connection builder every listener uses, the data plane
/// included: a client gets [`HEADER_READ_TIMEOUT`] to send a request's
/// headers.
pub(crate) fn connection_builder() -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder.http2().timer(TokioTimer::new());
    builder
}

async fn serve_connection<I>(io: I, router: Router, cancel: CancellationToken)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let builder = connection_builder();
    let conn = builder.serve_connection(TokioIo::new(io), TowerToHyperService::new(router));
    tokio::pin!(conn);
    let result = tokio::select! {
        r = conn.as_mut() => r,
        () = cancel.cancelled() => {
            conn.as_mut().graceful_shutdown();
            match tokio::time::timeout(SHUTDOWN_GRACE, conn.as_mut()).await {
                Ok(r) => r,
                Err(_) => return,
            }
        }
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, "admin connection ended with an error");
    }
}
