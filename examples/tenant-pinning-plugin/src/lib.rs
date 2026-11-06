//! An example Janus policy plugin, served over a unix socket. See README.md.
//!
//! - [`policy`] — the policy file and the routing decision (pure).
//! - [`service`] — the decision plus a two-phase request budget, as the
//!   `JanusPlugin` gRPC service.
//! - [`serve`] — binds the socket with the permissions the daemon requires
//!   and serves until `shutdown` resolves.

pub mod policy;
pub mod service;

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use janus_core::contract::proto::janus_plugin_server::JanusPluginServer;
use janus_core::peer::PeerUid;
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Status};

use crate::service::TenantPinning;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long in-flight calls get to finish once `shutdown` resolves.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Bind `socket` and serve `plugin` on it until `shutdown` resolves (plus at
/// most [`SHUTDOWN_GRACE`]), then remove the socket.
///
/// The daemon refuses a socket whose directory, or any ancestor, is writable
/// by group or others (`docs/plugin-contract#channel-unix`). A missing socket
/// directory is created mode `0750`; an existing one is left as it is, and
/// `janus plugin check` reports whether it passes. The socket itself is
/// `0660`, so the daemon's user reaches it through the directory's group.
///
/// With `daemon_uid`, every call is also checked against the connecting
/// peer's uid and refused with `PERMISSION_DENIED` from any other user.
pub async fn serve(
    plugin: TenantPinning,
    socket: &Path,
    daemon_uid: Option<u32>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    let listener = bind(socket)?;
    let peer = daemon_uid.map(PeerUid::new);
    let check_peer = move |req: Request<()>| -> Result<Request<()>, Status> {
        if let Some(peer) = &peer {
            peer.check(&req)?;
        }
        Ok(req)
    };
    tracing::info!(event = "listening", socket = %socket.display(), peer_uid = daemon_uid.map_or("any".to_owned(), |u| u.to_string()));
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

fn bind(socket: &Path) -> Result<UnixListener, Error> {
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
