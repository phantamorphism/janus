//! janus-daemon as a library.
//!
//! Per `docs/plugin-contract#custom-build`, the stock binary (`src/main.rs`)
//! is to be a few lines over a `janus_daemon::run(config, Embedded)` entry
//! point, and a custom build another binary calling the same entry point
//! with its own [`Embedded`] implementations. That entry point is not part
//! of this crate yet (see the `run` module); [`DaemonConfig`] and
//! [`Embedded`], its two arguments, are.
//!
//! Layout:
//!
//! - [`contract`] — the daemon config file and [`Embedded`]; the plugin and
//!   identity contracts are in `janus_core::contract`.
//! - [`plugin`] — the plugin client (`GrpcPluginClient`, `LinkedPluginClient`).
//! - [`store`] — `<data_dir>/janus.db`: opaque-identity registry and request
//!   log.
//! - [`dataplane`] — the proxy listener and request pipeline.
//! - [`admin`] — the `/v1` admin API on the unix socket, TCP and UI listeners.
//! - `run` — daemon lifecycle, where the entry point will live.

pub mod admin;
pub mod contract;
pub mod dataplane;
pub mod plugin;
pub mod store;

mod run;

pub use contract::{ConfigError, DaemonConfig, Embedded};
