//! Substitution and forwarding: constructing the outbound request from a
//! per-upstream forwarding allowlist, applying the real credential's auth,
//! rewriting the target, relaying (including streaming) responses unmodified,
//! and observing `usage`. Codes against [`crate::contract`]. See
//! `docs/credential-substitution` and `docs/architecture` "Request lifecycle".
