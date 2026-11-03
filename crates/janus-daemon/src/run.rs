//! Daemon lifecycle: startup validation, listener binding, and shutdown.
//!
//! This module will hold the entry point that the stock binary and every
//! custom build call. Its shape is fixed by `docs/plugin-contract#custom-build`:
//! an async `run` taking the parsed [`DaemonConfig`](crate::DaemonConfig)
//! value and an [`Embedded`](crate::Embedded), re-exported at the crate root
//! as `janus_daemon::run`. It does not exist yet; it is delivered with the
//! data plane.
