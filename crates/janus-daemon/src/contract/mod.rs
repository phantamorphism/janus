//! The daemon-side contract every daemon module codes against.
//!
//! - [`config`] — the daemon's config file (`DaemonConfig` and its
//!   sections), parsed with serde and statically validated.
//! - [`embedded`] — [`Embedded`], what a custom build passes to the daemon entry point, and
//!   the startup check that config and linkage agree.
//!
//! The plugin contract itself (`JanusPlugin`, `PluginClient`, `ResolveRoute`,
//! ...) and the identity contract live in `janus_core::contract`.

pub mod config;
pub mod embedded;

pub use config::{ConfigError, DaemonConfig};
pub use embedded::Embedded;
