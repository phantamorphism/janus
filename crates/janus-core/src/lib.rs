//! janus-core: the proxy brain.
//!
//! Holds everything that is true of the proxy regardless of how it is
//! deployed: caller-identity verification, the credential pool, the
//! substitution of a real upstream credential for the caller's identity, and
//! the forwarding path. Callers never see credential material; it does not
//! leave this crate in the direction of a caller.
//!
//! Default behavior is a pass-through credential-substituting proxy with no
//! external dependencies. Policy (which credential a caller gets, whether the
//! call is allowed at all) is reached only through the gRPC plugin contract in
//! `proto/`, never by depending on a specific host app's types.
//!
//! Not yet implemented — this crate is currently a scaffold.
