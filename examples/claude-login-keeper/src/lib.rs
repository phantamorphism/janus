//! An example Janus plugin that keeps a Claude subscription credential
//! logged in. See README.md and `docs/claude-login-keeper`.
//!
//! - [`config`] — the TOML file.
//! - [`output`], [`login`] — one isolated `claude auth login` run as a state
//!   machine.
//! - [`schedule`] — debounce and backoff (pure).
//! - [`admin`] — show, refresh and rotate through the daemon's admin API.
//! - [`keeper`] — health, single-flight flows, periodic checks, rotation.
//! - [`service`] — the `JanusPlugin` gRPC service.
//! - [`http`] — the loopback page.
//! - [`serve`] — the plugin socket, through `janus_core::peer::serve_unix`.

pub mod admin;
pub mod config;
pub mod http;
pub mod keeper;
pub mod login;
pub mod output;
pub mod schedule;
pub mod service;

use std::future::Future;
use std::path::Path;

use crate::service::KeeperPlugin;

pub type Error = janus_core::peer::ServeError;

/// Serve `plugin` on `socket` until `shutdown` resolves, with
/// `janus_core::peer::serve_unix`'s socket rules
/// (`docs/plugin-contract#channel-unix`).
pub async fn serve(
    plugin: KeeperPlugin,
    socket: &Path,
    daemon_uid: Option<u32>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    janus_core::peer::serve_unix(plugin, socket, daemon_uid, shutdown).await
}
