//! janus-daemon: the long-running process. Serves the proxy data plane on one
//! listener and the operator admin surface on a separate, local-only one, and
//! is the *client* of the plugin contract (ResolveRoute, Notify) — a plugin
//! implements the server side.
//!
//! This is the stock binary: it links no plugin, so `Embedded::default()` is
//! all it passes. A custom build is a separate crate that depends on this one
//! as a library and passes its own compiled-in `JanusPlugin` (and optionally
//! its own `IdentityExtractor`) to the same `run` entry point — see
//! `docs/plugin-contract` "linked plugins are a custom build".
//!
//! ```text
//! janus-daemon --config <path>                # run
//! janus-daemon check-config --config <path>   # validate, bind nothing
//! ```
//!
//! Logging goes to stderr; `RUST_LOG` sets the filter (default `info`).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use janus_daemon::{DaemonConfig, Embedded};

#[derive(Parser)]
#[command(
    name = "janus-daemon",
    version,
    about = "The Janus credential-substituting proxy daemon"
)]
struct Cli {
    /// The daemon's config file: the only setting the command line supplies.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full startup validation and exit without binding a listener.
    CheckConfig,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let Some(config_path) = cli.config else {
        eprintln!("janus-daemon: --config <path> is required");
        return ExitCode::from(2);
    };
    let result = match cli.command {
        Some(Command::CheckConfig) => janus_daemon::check_config(&config_path, Embedded::default())
            .await
            .map(|report| print!("{report}")),
        None => match DaemonConfig::load(&config_path) {
            Ok(config) => janus_daemon::run(config, Embedded::default()).await,
            Err(e) => Err(e.into()),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("janus-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}
