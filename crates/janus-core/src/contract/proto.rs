//! Wire form of the plugin contract, generated from `proto/janus.proto`
//! (package `janus.v1`) by `build.rs`.
//!
//! Message types: [`ResolveRouteRequest`], [`ResolveRouteResponse`] (with
//! [`resolve_route_response::Outcome`]), [`NotifyEvent`], [`NotifyAck`].
//! gRPC stubs: [`janus_plugin_client::JanusPluginClient`] (what the daemon
//! uses) and [`janus_plugin_server::JanusPluginServer`] /
//! [`janus_plugin_server::JanusPlugin`] (what an out-of-process Rust plugin
//! serves). Conversions to and from the Rust contract types are in
//! [`crate::contract::plugin`].

tonic::include_proto!("janus.v1");
