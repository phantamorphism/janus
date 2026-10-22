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
//! Not yet implemented — this crate is currently a scaffold.
