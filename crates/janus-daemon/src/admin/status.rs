//! `GET /v1/status`, `GET /v1/plugin` and `POST /v1/plugin/check`
//! (`docs/admin-surface#status-endpoint`, `#plugin-endpoint`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use janus_core::contract::credential::CredentialStatus;
use janus_core::contract::plugin::{PluginMode, PluginStatus};
use janus_core::contract::upstream::UpstreamKind;
use serde::Serialize;
use url::Url;

use super::api::AdminState;
use super::error::ApiError;
use super::AdminError;
use crate::contract::DaemonConfig;
use crate::plugin::ChannelCheck;

/// The `build` field of `/v1/status`: `JANUS_BUILD` at compile time (a
/// release pipeline stamps it), `dev` otherwise.
const BUILD: &str = match option_env!("JANUS_BUILD") {
    Some(build) => build,
    None => "dev",
};

/// The bound listeners, as `/v1/status` reports them.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Listeners {
    pub admin_socket: PathBuf,
    pub admin_tcp: Option<TcpListenerInfo>,
    pub ui: Option<TcpListenerInfo>,
    /// `data_plane.bind` as configured; the data plane binds it.
    pub data_plane: SocketAddr,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TcpListenerInfo {
    pub address: SocketAddr,
    pub tls: bool,
    pub url: String,
}

/// One `[[upstream]]` with the identity binding methods that apply to it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpstreamSummary {
    name: UpstreamKind,
    path_prefix: String,
    base_url: Url,
    /// The upstream-wide binding's method; `None` when no binding resolves
    /// (requests outside a route binding are then rejected).
    binding: Option<String>,
    routes: Vec<RouteSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct RouteSummary {
    path: String,
    binding: String,
}

/// Resolve every upstream's bindings once, at startup.
pub(crate) fn upstream_summaries(
    config: &DaemonConfig,
) -> Result<Vec<UpstreamSummary>, AdminError> {
    let bindings = config.resolve_bindings()?;
    Ok(config
        .upstreams
        .iter()
        .zip(bindings)
        .map(|(up, resolved)| UpstreamSummary {
            name: up.name,
            path_prefix: up.path_prefix.clone(),
            base_url: up.base_url.clone(),
            binding: resolved.binding.map(|b| b.method().name()),
            routes: resolved
                .routes
                .into_iter()
                .map(|r| RouteSummary {
                    path: r.path,
                    binding: r.binding.method().name(),
                })
                .collect(),
        })
        .collect())
}

#[derive(Serialize)]
struct RequestLogSummary {
    enabled: bool,
    /// `request_log.retain`, human-readable (`30days`).
    retain: String,
    retain_seconds: u64,
    /// Rows currently stored.
    rows: u64,
}

/// Credential health at a glance: how many credentials are in each status.
#[derive(Serialize)]
struct PoolSummary {
    total: usize,
    by_status: BTreeMap<&'static str, usize>,
}

#[derive(Serialize)]
struct Status<'a> {
    version: &'static str,
    build: &'static str,
    started_at: DateTime<Utc>,
    listeners: &'a Listeners,
    upstreams: &'a [UpstreamSummary],
    request_log: RequestLogSummary,
    credentials: PoolSummary,
    plugin: PluginStatus,
}

fn status_name(s: CredentialStatus) -> &'static str {
    match s {
        CredentialStatus::Active => "active",
        CredentialStatus::Invalid => "invalid",
        CredentialStatus::Disabled => "disabled",
        CredentialStatus::RateLimited => "rate_limited",
    }
}

fn plugin_status(state: &AdminState) -> PluginStatus {
    match &state.deps.plugin {
        Some(handle) => handle.status(),
        None => PluginStatus::new(PluginMode::None, None),
    }
}

pub(crate) async fn status(
    State(state): State<AdminState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let credentials = state.deps.pool.list(None).await?;
    let mut by_status: BTreeMap<&'static str, usize> = [
        CredentialStatus::Active,
        CredentialStatus::Invalid,
        CredentialStatus::Disabled,
        CredentialStatus::RateLimited,
    ]
    .into_iter()
    .map(|s| (status_name(s), 0))
    .collect();
    for c in &credentials {
        *by_status.entry(status_name(c.status)).or_default() += 1;
    }
    let log = state.deps.requests.stats();
    let body = Status {
        version: env!("CARGO_PKG_VERSION"),
        build: BUILD,
        started_at: DateTime::<Utc>::from(state.deps.started_at),
        listeners: &state.listeners,
        upstreams: &state.upstreams,
        request_log: RequestLogSummary {
            enabled: log.enabled,
            retain: humantime::format_duration(log.retain).to_string(),
            retain_seconds: log.retain.as_secs(),
            rows: log.rows,
        },
        credentials: PoolSummary {
            total: credentials.len(),
            by_status,
        },
        plugin: plugin_status(&state),
    };
    serde_json::to_value(&body)
        .map(Json)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))
}

pub(crate) async fn plugin(State(state): State<AdminState>) -> Json<PluginStatus> {
    Json(plugin_status(&state))
}

pub(crate) async fn plugin_check(
    State(state): State<AdminState>,
) -> Result<Json<ChannelCheck>, ApiError> {
    match &state.deps.plugin {
        Some(handle) => Ok(Json(handle.check().await)),
        None => Err(ApiError::new(
            StatusCode::CONFLICT,
            "no_plugin",
            "no [plugin] block is configured; there is no channel to check",
        )),
    }
}
