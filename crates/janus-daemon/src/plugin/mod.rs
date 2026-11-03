//! The plugin client: `GrpcPluginClient` (unix socket and network, with the
//! `Notify` stream, reconnection, timeout and failure classification) and
//! `LinkedPluginClient` (panic-guarded direct calls), both implementing
//! `janus_core::contract::plugin::PluginClient`, plus channel checks. See
//! `docs/plugin-contract#plugin-client` and `#channel-security`.
//!
//! [`PluginHandle`] is what the daemon holds: the client config selects,
//! and the [`PluginStatus`] folded from every `resolve_route` through it.

mod check;
mod grpc;
mod linked;
mod notify;
mod socket;
mod tls;
pub mod tls_init;
mod transport;

#[cfg(test)]
mod testutil;

use std::sync::{Arc, Mutex};

use chrono::Utc;
use janus_core::contract::plugin::{
    JanusPlugin, NotifyEvent, PluginClient, PluginError, PluginMode, PluginStatus, ResolveRoute,
    RouteOutcome,
};

pub use check::{ChannelCheck, CheckStep, CheckStepKind, PROBE_WAIT};
pub use grpc::GrpcPluginClient;
pub use linked::LinkedPluginClient;
pub use notify::NOTIFY_QUEUE_CAPACITY;
pub use socket::{PathWalk, SocketPathError, WalkedDir};
pub use tls::{TlsDialer, TlsSettings, EXPIRY_WARNING, MATERIAL_POLL_INTERVAL};
pub use tls_init::{tls_init, TlsInitError, TlsInitOptions, TlsInitOutput};

use crate::contract::config::{invalid, ConfigError, PluginConfig, PluginTransport};

/// Why a plugin client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("the plugin client must be built inside a Tokio runtime")]
    NoRuntime,
}

/// The daemon's plugin: the client config selects and its health.
pub struct PluginHandle {
    client: Arc<dyn PluginClient>,
    grpc: Option<Arc<GrpcPluginClient>>,
    linked: Option<Arc<LinkedPluginClient<Arc<dyn JanusPlugin>>>>,
    status: Mutex<PluginStatus>,
}

impl std::fmt::Debug for PluginHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHandle")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl PluginHandle {
    /// Build the client `[plugin]` selects; `None` without a `[plugin]`
    /// block (pass-through). `linked` is the build's compiled-in plugin,
    /// required by and only allowed with `linked = true`. Must be called
    /// inside a Tokio runtime.
    pub fn from_config(
        config: Option<&PluginConfig>,
        linked: Option<Arc<dyn JanusPlugin>>,
    ) -> Result<Option<Self>, SetupError> {
        let Some(config) = config else {
            if linked.is_some() {
                return Err(invalid(
                    "plugin",
                    "this build links a plugin but config has no [plugin] block with linked = true",
                )
                .into());
            }
            return Ok(None);
        };
        let transport = config.transport()?;
        let handle =
            match (transport, linked) {
                (PluginTransport::Linked, Some(plugin)) => {
                    let linked = Arc::new(LinkedPluginClient::new(plugin, config.timeout())?);
                    PluginHandle {
                        client: Arc::clone(&linked) as Arc<dyn PluginClient>,
                        grpc: None,
                        linked: Some(linked),
                        status: Mutex::new(PluginStatus::new(PluginMode::Linked, None)),
                    }
                }
                (PluginTransport::Linked, None) => {
                    return Err(invalid(
                        "plugin.linked",
                        "config selects a linked plugin but this build links none",
                    )
                    .into())
                }
                (_, Some(_)) => return Err(invalid(
                    "plugin.endpoint",
                    "config names a plugin endpoint and this build also links a plugin; ambiguous",
                )
                .into()),
                (transport, None) => {
                    let grpc = Arc::new(GrpcPluginClient::new(&transport, config.timeout())?);
                    let status = PluginStatus::new(grpc.mode(), Some(grpc.endpoint().to_owned()));
                    PluginHandle {
                        client: Arc::clone(&grpc) as Arc<dyn PluginClient>,
                        grpc: Some(grpc),
                        linked: None,
                        status: Mutex::new(status),
                    }
                }
            };
        Ok(Some(handle))
    }

    /// The client itself (calls through it are not recorded in the status).
    pub fn client(&self) -> &Arc<dyn PluginClient> {
        &self.client
    }

    /// `ResolveRoute`, with the result folded into [`PluginHandle::status`].
    pub async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        let result = self.client.resolve_route(req).await;
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        match &result {
            Ok(_) => status.record_ok(Utc::now()),
            Err(err) => status.record_failure(err),
        }
        result
    }

    /// Hand off a `Notify` event; never blocks.
    pub fn notify(&self, event: NotifyEvent) {
        self.client.notify(event);
    }

    /// The `plugin` object of `/v1/status` and `/v1/plugin`.
    pub fn status(&self) -> PluginStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(grpc) = &self.grpc {
            status.client_cert_expires_at = grpc.client_cert_expires_at();
            status.server_cert_expires_at = grpc.server_cert_expires_at();
            status.dropped_events = grpc.dropped_events();
        }
        if let Some(linked) = &self.linked {
            status.dropped_events = linked.dropped_events();
        }
        status
    }

    /// `janus plugin check` / `POST /v1/plugin/check`.
    pub async fn check(&self) -> ChannelCheck {
        match &self.grpc {
            Some(grpc) => grpc.check().await,
            None => ChannelCheck::linked(),
        }
    }

    /// Re-read TLS material (`SIGHUP`); a no-op on other transports.
    pub fn reload_tls(&self) -> Result<(), ConfigError> {
        match &self.grpc {
            Some(grpc) => grpc.reload_tls(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use janus_core::contract::plugin::{PluginFailureClass, PluginHealth};

    use super::*;
    use crate::plugin::testutil::{event, request, serve_unix, socket_dir, FakePlugin};

    struct Allow;

    #[async_trait]
    impl JanusPlugin for Allow {
        async fn resolve_route(&self, _req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
            Ok(RouteOutcome::UseDefault)
        }

        async fn notify(&self, _event: NotifyEvent) {}
    }

    fn config(toml: &str) -> PluginConfig {
        toml::from_str(toml).expect("config")
    }

    #[tokio::test]
    async fn handle_records_every_call_into_status() {
        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let server = serve_unix(&plugin, &path);
        let cfg = config(&format!("endpoint = \"unix://{}\"", path.display()));
        let handle = PluginHandle::from_config(Some(&cfg), None)
            .expect("built")
            .expect("configured");
        let status = handle.status();
        assert_eq!(status.mode, PluginMode::Unix);
        assert_eq!(status.health, PluginHealth::Unknown);
        assert_eq!(status.endpoint, Some(format!("unix://{}", path.display())));

        handle.resolve_route(request("s")).await.expect("ok");
        let status = handle.status();
        assert_eq!(status.health, PluginHealth::Ok);
        assert!(status.last_ok_at.is_some());

        server.stop().await;
        let _ = std::fs::remove_file(&path);
        let err = handle.resolve_route(request("s")).await.expect_err("down");
        assert_eq!(err.class, PluginFailureClass::Unreachable);
        let status = handle.status();
        assert_eq!(status.health, PluginHealth::Unreachable);
        assert_eq!(status.last_error, Some(err.message));
        assert!(status.last_ok_at.is_some(), "history kept");
        assert_eq!(status.client_cert_expires_at, None);
        handle.notify(crate::plugin::testutil::event("call_completed"));
        handle.reload_tls().expect("no-op off TLS");
    }

    #[tokio::test]
    async fn status_reports_dropped_notify_events_for_every_client() {
        const TOTAL: u64 = 20_000;
        async fn flood(handle: &PluginHandle) {
            assert_eq!(handle.status().dropped_events, 0);
            for n in 0..TOTAL {
                handle.notify(event(&format!("event-{n}-{}", "x".repeat(200))));
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while handle.status().dropped_events == 0 && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let dropped = handle.status().dropped_events;
            assert!(dropped > 0, "a stalled plugin's full queue drops");
            assert!(dropped < TOTAL, "{dropped}: some events were queued");
        }

        struct Stalls;
        #[async_trait]
        impl JanusPlugin for Stalls {
            async fn resolve_route(&self, _req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
                Ok(RouteOutcome::UseDefault)
            }

            async fn notify(&self, _event: NotifyEvent) {
                std::future::pending::<()>().await;
            }
        }
        let linked: Arc<dyn JanusPlugin> = Arc::new(Stalls);
        let handle = PluginHandle::from_config(Some(&config("linked = true")), Some(linked))
            .expect("built")
            .expect("configured");
        flood(&handle).await;

        let (_dir, path) = socket_dir();
        let plugin = FakePlugin::stalling_notify();
        let _server = serve_unix(&plugin, &path);
        let cfg = config(&format!("endpoint = \"unix://{}\"", path.display()));
        let handle = PluginHandle::from_config(Some(&cfg), None)
            .expect("built")
            .expect("configured");
        flood(&handle).await;
    }

    #[tokio::test]
    async fn handle_follows_config_and_refuses_linkage_mismatch() {
        let linked: Arc<dyn JanusPlugin> = Arc::new(Allow);
        assert!(PluginHandle::from_config(None, None).expect("ok").is_none());

        let handle =
            PluginHandle::from_config(Some(&config("linked = true")), Some(Arc::clone(&linked)))
                .expect("built")
                .expect("configured");
        assert_eq!(handle.status().mode, PluginMode::Linked);
        assert_eq!(handle.status().endpoint, None);
        assert_eq!(
            handle.resolve_route(request("s")).await,
            Ok(RouteOutcome::UseDefault)
        );
        assert_eq!(handle.status().health, PluginHealth::Ok);
        assert!(handle.check().await.passed);
        assert_eq!(
            handle.client().resolve_route(request("s")).await,
            Ok(RouteOutcome::UseDefault)
        );

        let refusals = [
            PluginHandle::from_config(None, Some(Arc::clone(&linked))),
            PluginHandle::from_config(Some(&config("linked = true")), None),
            PluginHandle::from_config(
                Some(&config("endpoint = \"unix:///run/janus/p.sock\"")),
                Some(linked),
            ),
        ];
        for refused in refusals {
            let err = refused.expect_err("mismatch");
            assert!(matches!(err, SetupError::Config(_)), "{err}");
        }
    }
}
