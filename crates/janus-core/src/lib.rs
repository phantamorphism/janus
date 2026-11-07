//! janus-core: the proxy brain.
//!
//! Holds everything that is true of the proxy regardless of how it is
//! deployed: caller-identity extraction and verification, the credential pool,
//! the substitution of a real upstream credential for the caller's identity,
//! the forwarding path, and the metering primitives a policy plugin can link
//! instead of reimplementing. Callers never see credential material; it does
//! not leave this crate in the direction of a caller.
//!
//! Janus verifies caller identities; it does not mint them. The `hmac`, `jwt`,
//! `opaque` and `trusted_header` extraction methods all take material some
//! other authority issued and turn it into a verified identity string. See
//! `docs/caller-identity`.
//!
//! Default behavior is a pass-through credential-substituting proxy with no
//! external dependencies. Policy (which credential a caller gets, whether the
//! call is allowed at all) is reached only through the plugin contract in
//! `proto/`, never by depending on a specific host app's types.
//!
//! Layout:
//!
//! - [`contract`] — the types and traits every other module (and the daemon,
//!   and plugins) codes against: identity extraction, binding config, the
//!   plugin contract in Rust and wire form, metering signatures, credential
//!   metadata, upstream names and data-directory layout.
//! - [`identity`], [`pool`], [`metering`], [`proxy`] — the implementations.
//! - [`peer`] (unix only) — the tonic peer-uid interceptor a Rust plugin can
//!   install on its unix socket, and `serve_unix`, which binds that socket
//!   and serves a plugin on it (`docs/plugin-contract#channel-unix`).

pub mod contract;
pub mod identity;
pub mod metering;
#[cfg(unix)]
pub mod peer;
pub mod pool;
pub mod proxy;
