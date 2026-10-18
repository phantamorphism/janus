//! janus-core: the proxy brain.
//!
//! Default behavior is a pure pass-through dual-upstream (Anthropic +
//! OpenAI) proxy with no external dependencies — credential pool and HMAC
//! auth live here. Optional behavior (account routing, event notifications)
//! is reached only through the gRPC plugin contract defined in `proto/`,
//! never by depending on a specific host app's types.
//!
//! Lifted and generalized from Shadow's `shadow-server/src/gateway/` +
//! `shadow-core/src/gateway/token.rs`. Not yet implemented — this crate is
//! currently a scaffold.
