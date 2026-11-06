//! `janus status` and `janus plugin status|check|tls init`
//! (`docs/admin-surface#status-endpoint`, `#plugin-endpoint`,
//! `docs/plugin-contract#channel-bootstrap`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use http::Method;
use janus_core::contract::plugin::PluginStatus;
use janus_daemon::plugin::tls_init::{self, TlsInitError, TlsInitOptions};
use serde::Deserialize;
use serde_json::{json, Value};

use super::Ctx;
use crate::client::api_path;
use crate::error::CliError;
use crate::render::{self, fields, Table};

#[derive(Deserialize)]
struct Status {
    version: String,
    build: String,
    started_at: DateTime<Utc>,
    listeners: Listeners,
    upstreams: Vec<Upstream>,
    #[serde(default)]
    data_plane: DataPlane,
    request_log: RequestLog,
    credentials: Credentials,
    plugin: PluginStatus,
}

#[derive(Deserialize)]
struct Listeners {
    admin_socket: PathBuf,
    admin_tcp: Option<TcpListener>,
    ui: Option<TcpListener>,
    data_plane: String,
}

#[derive(Deserialize)]
struct TcpListener {
    url: String,
}

#[derive(Deserialize)]
struct Upstream {
    name: String,
    implementation: String,
    strategy: String,
    failover: bool,
    /// The credentials the instance may use right now, in chain order.
    #[serde(default)]
    eligible: Vec<String>,
    path_prefix: String,
    base_url: String,
    binding: Option<String>,
    routes: Vec<Route>,
}

#[derive(Deserialize)]
struct Route {
    path: String,
    binding: String,
}

#[derive(Deserialize, Default)]
struct DataPlane {
    refused_connections: u64,
}

#[derive(Deserialize)]
struct RequestLog {
    enabled: bool,
    retain: String,
    rows: u64,
    #[serde(default)]
    dropped_records: u64,
}

#[derive(Deserialize)]
struct Credentials {
    total: usize,
    by_status: BTreeMap<String, usize>,
}

pub async fn status(ctx: &Ctx) -> Result<(), CliError> {
    let admin = ctx.admin()?;
    let (raw, s): (Value, Status) = admin
        .json(Method::GET, &api_path(&["v1", "status"], &[])?, None)
        .await?;
    if ctx.json {
        return render::json(&raw);
    }
    let listener = |l: &Option<TcpListener>| l.as_ref().map_or("-".to_owned(), |l| l.url.clone());
    let counts = s
        .credentials
        .by_status
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{n} {k}"))
        .collect::<Vec<_>>()
        .join(", ");
    let log = if s.request_log.enabled {
        format!(
            "on, {} rows, retained {}",
            s.request_log.rows, s.request_log.retain
        )
    } else {
        "off (request_log.enabled = false)".to_owned()
    };
    let mut text = fields(&[
        ("version", format!("{} (build {})", s.version, s.build)),
        ("started", render::time(Some(s.started_at))),
        ("data plane", s.listeners.data_plane.clone()),
        (
            "admin socket",
            s.listeners.admin_socket.display().to_string(),
        ),
        ("admin tcp", listener(&s.listeners.admin_tcp)),
        ("ui", listener(&s.listeners.ui)),
        ("request log", log),
        (
            "dropped records",
            render::alert_if_nonzero(s.request_log.dropped_records, "request-log queue was full"),
        ),
        (
            "refused conns",
            render::alert_if_nonzero(
                s.data_plane.refused_connections,
                "over data_plane.max_connections",
            ),
        ),
        (
            "credentials",
            if counts.is_empty() {
                format!("{}", s.credentials.total)
            } else {
                format!("{} ({counts})", s.credentials.total)
            },
        ),
    ]);
    text.push('\n');
    let mut upstreams = Table::new(&[
        "UPSTREAM",
        "IMPLEMENTATION",
        "PREFIX",
        "IDENTITY",
        "STRATEGY",
        "CREDENTIALS",
        "BASE URL",
    ]);
    for up in &s.upstreams {
        let strategy = if up.failover {
            format!("{} +failover", up.strategy)
        } else {
            up.strategy.clone()
        };
        let eligible = if up.eligible.is_empty() {
            "-".to_owned()
        } else {
            up.eligible.join(",")
        };
        upstreams.row(vec![
            up.name.clone(),
            up.implementation.clone(),
            up.path_prefix.clone(),
            render::text(up.binding.as_deref()),
            strategy,
            eligible,
            up.base_url.clone(),
        ]);
        for route in &up.routes {
            upstreams.row(vec![
                String::new(),
                String::new(),
                format!("{}{}", up.path_prefix, route.path),
                route.binding.clone(),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
    }
    text.push_str(&upstreams.render());
    text.push('\n');
    text.push_str(&plugin_fields(&s.plugin));
    render::out(&text)
}

fn plugin_fields(p: &PluginStatus) -> String {
    let mut rows = vec![
        ("plugin", p.mode.as_str().to_owned()),
        ("endpoint", render::text(p.endpoint.as_deref())),
        ("health", p.health.as_str().to_owned()),
        ("last ok", render::time(p.last_ok_at)),
        ("last error", render::text(p.last_error.as_deref())),
    ];
    if p.client_cert_expires_at.is_some() || p.server_cert_expires_at.is_some() {
        rows.push((
            "client cert expires",
            render::time(p.client_cert_expires_at),
        ));
        rows.push((
            "server cert expires",
            render::time(p.server_cert_expires_at),
        ));
    }
    if p.dropped_events > 0 {
        rows.push(("dropped events", p.dropped_events.to_string()));
    }
    fields(&rows)
}

#[derive(Subcommand)]
pub enum PluginCommand {
    /// The plugin channel's mode and health (`GET /v1/plugin`).
    Status,
    /// Actively verify the channel (`POST /v1/plugin/check`): on TLS, a full
    /// handshake plus two handshakes the plugin must reject; on a socket,
    /// the path-permission walk and a connection. Exits 1 unless every step
    /// passes.
    Check,
    /// Bootstrap the TLS plugin transport without a PKI.
    #[command(subcommand)]
    Tls(TlsCommand),
}

#[derive(Subcommand)]
pub enum TlsCommand {
    /// Generate a self-signed key pair for the daemon and one for the
    /// plugin (SAN = --plugin-host), each side pinning the other, and print
    /// the [plugin.tls] block plus what the plugin must be configured with.
    /// Needs no running daemon. Rerun to rotate.
    Init {
        /// The plugin's host name or IP address, carried as its certificate's SAN.
        #[arg(long, value_name = "NAME")]
        plugin_host: String,
        /// Directory to write daemon.pem, daemon-key.pem, plugin.pem and plugin-key.pem into.
        #[arg(long, value_name = "DIR", default_value = ".")]
        out: PathBuf,
        /// Validity of both certificates, in days.
        #[arg(long, value_name = "N", default_value_t = tls_init::DEFAULT_DAYS)]
        days: u32,
    },
}

#[derive(Deserialize)]
struct ChannelCheck {
    passed: bool,
    mode: String,
    endpoint: Option<String>,
    steps: Vec<CheckStep>,
}

#[derive(Deserialize)]
struct CheckStep {
    step: String,
    passed: bool,
    detail: String,
    class: Option<String>,
    walk: Option<Walk>,
}

#[derive(Deserialize)]
struct Walk {
    dirs: Vec<WalkedDir>,
}

#[derive(Deserialize)]
struct WalkedDir {
    path: PathBuf,
    uid: u32,
    mode: u32,
}

pub async fn plugin(ctx: &Ctx, command: &PluginCommand) -> Result<(), CliError> {
    match command {
        PluginCommand::Status => {
            let (raw, p): (Value, PluginStatus) = ctx
                .admin()?
                .json(Method::GET, &api_path(&["v1", "plugin"], &[])?, None)
                .await?;
            if ctx.json {
                render::json(&raw)
            } else {
                render::out(&plugin_fields(&p))
            }
        }
        PluginCommand::Check => {
            let (raw, check): (Value, ChannelCheck) = ctx
                .admin()?
                .json(
                    Method::POST,
                    &api_path(&["v1", "plugin", "check"], &[])?,
                    None,
                )
                .await?;
            if ctx.json {
                render::json(&raw)?;
            } else {
                render::out(&check_text(&check))?;
            }
            if check.passed {
                Ok(())
            } else {
                Err(CliError::failed(
                    "plugin_check_failed",
                    "the plugin channel check did not pass",
                ))
            }
        }
        PluginCommand::Tls(TlsCommand::Init {
            plugin_host,
            out,
            days,
        }) => {
            let written = tls_init::tls_init(&TlsInitOptions {
                plugin_host: plugin_host.clone(),
                out_dir: out.clone(),
                days: *days,
            })
            .map_err(|e| match e {
                TlsInitError::Io { .. } | TlsInitError::Generate(_) => {
                    CliError::failed("tls_init_failed", e.to_string())
                }
                _ => CliError::usage(e.to_string()),
            })?;
            if ctx.json {
                render::json(&json!({
                    "daemon_cert": written.daemon_cert,
                    "daemon_key": written.daemon_key,
                    "plugin_cert": written.plugin_cert,
                    "plugin_key": written.plugin_key,
                    "config_block": written.config_block,
                    "plugin_instructions": written.plugin_instructions,
                }))
            } else {
                render::out(&format!(
                    "wrote {}, {} (mode 0600), {}, {} (mode 0600)\n\n{written}",
                    written.daemon_cert.display(),
                    written.daemon_key.display(),
                    written.plugin_cert.display(),
                    written.plugin_key.display(),
                ))
            }
        }
    }
}

fn check_text(check: &ChannelCheck) -> String {
    let mut text = format!(
        "plugin {} {}: {}\n",
        check.mode,
        render::text(check.endpoint.as_deref()),
        if check.passed { "PASSED" } else { "FAILED" }
    );
    if check.steps.is_empty() {
        text.push_str("  no channel to check (the plugin is linked into the daemon)\n");
    }
    for step in &check.steps {
        let class = step
            .class
            .as_deref()
            .map_or_else(String::new, |c| format!(" [{c}]"));
        text.push_str(&format!(
            "  {} {}{class}: {}\n",
            if step.passed { "ok  " } else { "FAIL" },
            step.step,
            step.detail
        ));
        if let Some(walk) = &step.walk {
            for dir in &walk.dirs {
                text.push_str(&format!(
                    "         {:04o} uid={} {}\n",
                    dir.mode,
                    dir.uid,
                    dir.path.display()
                ));
            }
        }
    }
    text
}
