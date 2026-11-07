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
use std::path::Path;

use crate::service::TenantPinning;

pub type Error = janus_core::peer::ServeError;

/// How long in-flight calls get to finish once `shutdown` resolves.
pub use janus_core::peer::SHUTDOWN_GRACE;

/// Bind `socket` and serve `plugin` on it until `shutdown` resolves (plus at
/// most [`SHUTDOWN_GRACE`]), then remove the socket. The socket rules (a
/// missing directory `0750`, the socket `0660`, a stale socket replaced, the
/// optional peer-uid check) are `janus_core::peer::serve_unix`'s.
pub async fn serve(
    plugin: TenantPinning,
    socket: &Path,
    daemon_uid: Option<u32>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    janus_core::peer::serve_unix(plugin, socket, daemon_uid, shutdown).await
}
