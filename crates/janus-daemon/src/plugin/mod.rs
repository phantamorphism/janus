//! The plugin client: `GrpcPluginClient` (unix socket and network, with the
//! `Notify` stream, reconnection, timeout and failure classification) and
//! `LinkedPluginClient` (panic-guarded direct calls), both implementing
//! `janus_core::contract::plugin::PluginClient`, plus channel checks. See
//! `docs/plugin-contract#plugin-client` and `#channel-security`.
