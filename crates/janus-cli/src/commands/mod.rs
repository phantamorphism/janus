//! One module per command group; each maps a subcommand to one or two
//! admin API calls and renders the answer.

pub mod credential;
pub mod identity;
pub mod log;
pub mod status;

use std::path::Path;

use janus_core::contract::upstream::UpstreamKind;
use janus_daemon::Embedded;
use serde_json::json;

use crate::client::Admin;
use crate::connect::Connection;
use crate::error::CliError;
use crate::render;

/// What every command gets: the connection flags and the output mode.
pub struct Ctx {
    pub connection: Connection,
    pub json: bool,
}

impl Ctx {
    pub fn admin(&self) -> Result<Admin, CliError> {
        self.connection.admin()
    }
}

/// `--upstream`: one of the implemented upstreams.
pub fn upstream_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(UpstreamKind::ALL.map(UpstreamKind::as_str))
}

/// `janus check-config`: the daemon's own startup validation
/// (`janus_daemon::check_config`, what `janus-daemon check-config` runs)
/// over the stock build's extractors and upstreams; binds nothing. A custom
/// daemon build links its own extractors, so it is checked with that
/// build's `check-config` instead.
pub async fn check_config(ctx: &Ctx, path: &Path) -> Result<(), CliError> {
    let report = janus_daemon::check_config(path, Embedded::default())
        .await
        .map_err(|e| CliError::failed("invalid_config", format!("{}: {e}", path.display())))?;
    if !ctx.json {
        return render::out(&report.to_string());
    }
    let upstreams: Vec<_> = report
        .upstreams
        .iter()
        .map(|up| {
            json!({
                "upstream": up.upstream,
                "path_prefix": up.path_prefix,
                "binding": up.binding.as_ref().map(ToString::to_string),
                "routes": up.routes.iter().map(|(path, method)| json!({
                    "path": path,
                    "binding": method.to_string(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    render::json(&json!({
        "ok": true,
        "data_dir": report.data_dir,
        "data_plane": report.data_plane.to_string(),
        "plugin": report.plugin,
        "credentials": report.credentials,
        "upstreams": upstreams,
    }))
}
