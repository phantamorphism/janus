//! janus-daemon as a library.
//!
//! Per `docs/plugin-contract#custom-build`, the stock binary (`src/main.rs`)
//! is a few lines over [`run`]`(config, Embedded)`, and a custom build is
//! another binary calling the same entry point with its own [`Embedded`]
//! implementations. [`Daemon`] is the same lifecycle with the shutdown
//! trigger in the caller's hands; [`check_config`] is
//! `janus-daemon check-config`.
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
//! - `run` — daemon lifecycle: startup validation, serving, shutdown.

pub mod admin;
pub mod contract;
pub mod dataplane;
pub mod plugin;
pub mod store;

mod run;

pub use contract::{ConfigError, DaemonConfig, Embedded};
pub use run::{check_config, run, CheckReport, CheckedUpstream, Daemon, RunError};
