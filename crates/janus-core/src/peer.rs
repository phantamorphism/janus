//! The peer-uid check a Rust plugin serving on a unix socket can install
//! (`docs/plugin-contract#channel-unix`).
//!
//! Filesystem permissions are the boundary on the unix-socket channel; this
//! is the optional check from the plugin's side that the process on the other
//! end of the socket runs as the daemon's user. The kernel reports the
//! connecting peer's uid (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS);
//! tonic records it per connection as [`UdsConnectInfo`] when the server is
//! fed `tokio::net::UnixStream`s, and [`PeerUid`] reads it there.
//!
//! ```no_run
//! # use janus_core::peer::PeerUid;
//! # use janus_core::contract::proto::janus_plugin_server::{JanusPlugin, JanusPluginServer};
//! # fn serve<P: JanusPlugin>(plugin: P, daemon_uid: u32) {
//! let service = JanusPluginServer::with_interceptor(plugin, PeerUid::new(daemon_uid));
//! # let _ = service;
//! # }
//! ```
//!
//! It fails closed: a request whose connection carries no peer credentials
//! (a non-unix transport, or a platform where the kernel did not report
//! them) is refused exactly like one from the wrong uid.
//!
//! [`serve_unix`] binds the socket with the modes the daemon requires and
//! serves a plugin on it, with this check when a daemon uid is given.

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::service::Interceptor;
use tonic::transport::server::UdsConnectInfo;
use tonic::{Request, Status};

use crate::contract::proto::janus_plugin_server::{JanusPlugin, JanusPluginServer};

/// A tonic [`Interceptor`] that admits a request only if the unix-socket
/// peer that sent it runs as `expected_uid`, and otherwise answers
/// `PERMISSION_DENIED` without reaching the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerUid {
    expected_uid: u32,
}

impl PeerUid {
    /// Admit only peers running as `expected_uid` (the daemon's user).
    pub fn new(expected_uid: u32) -> Self {
        Self { expected_uid }
    }

    /// The uid this check admits.
    pub fn expected_uid(&self) -> u32 {
        self.expected_uid
    }

    /// The check itself, for callers that are not using it as an
    /// interceptor (a tower layer, a hand-written service).
    pub fn check<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let uid = request
            .extensions()
            .get::<UdsConnectInfo>()
            .and_then(|info| info.peer_cred)
            .map(|cred| cred.uid())
            .ok_or_else(|| {
                Status::permission_denied("peer credentials unavailable on this connection")
            })?;
        if uid == self.expected_uid {
            Ok(())
        } else {
            Err(Status::permission_denied(format!(
                "peer uid {uid} is not the expected uid {}",
                self.expected_uid
            )))
        }
    }
}

impl Interceptor for PeerUid {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.check(&request)?;
        Ok(request)
    }
}

/// An error from [`serve_unix`].
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// How long in-flight calls get to finish once [`serve_unix`]'s `shutdown`
/// resolves.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Bind `socket` and serve `plugin` on it until `shutdown` resolves (plus at
/// most [`SHUTDOWN_GRACE`]), then remove the socket.
///
/// The daemon refuses a socket whose directory, or any ancestor, is writable
/// by group or others (`docs/plugin-contract#channel-unix`). A missing socket
/// directory is created mode `0750`; an existing one is left as it is, and
/// `janus plugin check` reports whether it passes. The socket itself is
/// `0660`, so the daemon's user reaches it through the directory's group. A
/// socket left by a previous run is replaced; any other file at the path is
/// an error.
///
/// With `daemon_uid`, every call is also checked with [`PeerUid`] and refused
/// with `PERMISSION_DENIED` from any other user.
pub async fn serve_unix<P: JanusPlugin>(
    plugin: P,
    socket: &Path,
    daemon_uid: Option<u32>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ServeError> {
    let listener = bind(socket)?;
    let peer = daemon_uid.map(PeerUid::new);
    let check_peer = move |req: Request<()>| -> Result<Request<()>, Status> {
        if let Some(peer) = &peer {
            peer.check(&req)?;
        }
        Ok(req)
    };
    tracing::info!(event = "listening", socket = %socket.display(),
        peer_uid = daemon_uid.map_or("any".to_owned(), |u| u.to_string()));
    // A graceful shutdown waits for every connection to close, and the
    // daemon never closes its Notify stream; bound the wait.
    let (stopping, stopped) = oneshot::channel::<()>();
    let signal = async move {
        shutdown.await;
        let _ = stopping.send(());
    };
    let server = tonic::transport::Server::builder()
        .add_service(JanusPluginServer::with_interceptor(plugin, check_peer))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), signal);
    let served = tokio::select! {
        served = server => served,
        _ = async {
            match stopped.await {
                Ok(()) => tokio::time::sleep(SHUTDOWN_GRACE).await,
                // The server ended by itself; its own result wins.
                Err(_) => std::future::pending().await,
            }
        } => Ok(()),
    };
    // Best-effort: the next start removes a stale socket anyway.
    let _ = std::fs::remove_file(socket);
    Ok(served?)
}

fn bind(socket: &Path) -> Result<UnixListener, ServeError> {
    let dir = socket
        .parent()
        .ok_or("socket path has no parent directory")?;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(dir)?;
    }
    // A socket left by a previous run; never remove anything else.
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(socket)?,
        Ok(_) => return Err(format!("{} exists and is not a socket", socket.display()).into()),
        Err(_) => {}
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use hyper_util::rt::TokioIo;
    use tokio::net::{UnixListener, UnixStream};
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Endpoint, Server, Uri};
    use tonic::{Code, Response, Streaming};

    use super::*;
    use crate::contract::proto::janus_plugin_client::JanusPluginClient;
    use crate::contract::proto::{
        NotifyAck, NotifyEvent, ResolveRouteRequest, ResolveRouteResponse,
    };

    /// Counts the calls that got past the interceptor.
    #[derive(Default)]
    struct Counting(Arc<AtomicUsize>);

    #[tonic::async_trait]
    impl JanusPlugin for Counting {
        async fn resolve_route(
            &self,
            _request: Request<ResolveRouteRequest>,
        ) -> Result<Response<ResolveRouteResponse>, Status> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::new(ResolveRouteResponse { outcome: None }))
        }

        async fn notify(
            &self,
            _request: Request<Streaming<NotifyEvent>>,
        ) -> Result<Response<NotifyAck>, Status> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Response::new(NotifyAck {}))
        }
    }

    fn current_uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Serve the plugin service behind `PeerUid::new(expected_uid)` on a
    /// real unix socket, call `ResolveRoute` once over it, and return the
    /// call's result and how many calls reached the service.
    async fn call_over_socket(expected_uid: u32) -> (Result<(), Status>, usize) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path: PathBuf = dir.path().join("plugin.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let reached = Arc::new(AtomicUsize::new(0));
        let service = JanusPluginServer::with_interceptor(
            Counting(reached.clone()),
            PeerUid::new(expected_uid),
        );
        let server = tokio::spawn(
            Server::builder()
                .add_service(service)
                .serve_with_incoming(UnixListenerStream::new(listener)),
        );

        let channel =
            Endpoint::try_from("http://[::]:50051")
                .expect("endpoint")
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let path = path.clone();
                    async move {
                        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?))
                    }
                }))
                .await
                .expect("connect over unix socket");
        let result = JanusPluginClient::new(channel)
            .resolve_route(ResolveRouteRequest::default())
            .await
            .map(|_| ());
        server.abort();
        (result, reached.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_peer_with_the_expected_uid_reaches_the_service() {
        let (result, reached) = call_over_socket(current_uid()).await;
        result.expect("same-uid peer is admitted");
        assert_eq!(reached, 1);
    }

    #[tokio::test]
    async fn a_peer_with_a_different_uid_is_refused_before_the_service() {
        let (result, reached) = call_over_socket(current_uid().wrapping_add(1)).await;
        let status = result.expect_err("different-uid peer must be refused");
        assert_eq!(status.code(), Code::PermissionDenied);
        assert!(
            status.message().contains("is not the expected uid"),
            "{}",
            status.message()
        );
        assert_eq!(reached, 0, "the service must not run for a refused peer");
    }

    #[test]
    fn a_request_without_peer_credentials_is_refused() {
        let mut check = PeerUid::new(current_uid());
        let status = check
            .call(Request::new(()))
            .expect_err("no UdsConnectInfo must fail closed");
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(
            status.message(),
            "peer credentials unavailable on this connection"
        );
    }
}
