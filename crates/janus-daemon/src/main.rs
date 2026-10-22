//! janus-daemon: the long-running process. Serves the proxy data plane on one
//! listener and the operator admin surface on a separate, local-only one, and
//! is the *client* of the plugin contract (ResolveRoute, Notify) — a plugin
//! implements the server side.
//!
//! This is the stock binary: it links no plugin, so `Embedded::default()` is
//! all it passes. A custom build is a separate crate that depends on this one
//! as a library and passes its own compiled-in `JanusPlugin` (and optionally
//! its own `IdentityExtractor`) to the same `run` entry point — see
//! `docs/plugin-contract` "linked plugins are a custom build".
//!
//! Scaffold only — not yet implemented.

fn main() {
    println!("janus-daemon: not yet implemented");
}
