//! `janus`: the admin client of janus-daemon (`docs/admin-surface#cli`).
//! A thin client of the `/v1` admin API — each subcommand is one or two
//! calls — over the daemon's admin unix socket by default, or its TCP admin
//! listener. It holds no proxy logic. Secrets never travel on its command
//! line: credential material comes from stdin or a file, the admin token
//! from a file or the environment, and the one secret it prints (a new
//! opaque-identity token) is printed once, on stdout.

mod client;
mod commands;
mod connect;
mod error;
mod render;
mod sse;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use commands::credential::CredentialCommand;
use commands::identity::IdentityCommand;
use commands::log::LogCommand;
use commands::status::PluginCommand;
use commands::Ctx;
use connect::Connection;
use error::CliError;

const AFTER_HELP: &str = "\
Connection: the default address is the daemon's admin socket, <data_dir>/admin.sock, \
with data_dir taken from --config's data_dir, else $JANUS_DATA_DIR, else $HOME/.janus. \
For an http(s) address the admin token is read from --token-file (or $JANUS_TOKEN_FILE), \
else from $JANUS_ADMIN_TOKEN, else from <data_dir>/admin-token. No flag takes a secret.

Exit codes: 0 success; 1 the daemon refused or failed the operation, or a check did not pass; \
2 bad command line or local input; 3 the daemon could not be reached.";

#[derive(Parser)]
#[command(
    name = "janus",
    version,
    about = "Admin client for janus-daemon: credentials, opaque identities, the request log and the plugin channel",
    after_help = AFTER_HELP
)]
struct Cli {
    /// Admin surface address: unix://<path> or http(s)://host:port.
    #[arg(long, global = true, env = "JANUS_ADMIN", value_name = "ADDRESS")]
    admin: Option<String>,
    /// File holding the admin token, for an http(s) address.
    #[arg(
        long,
        global = true,
        env = "JANUS_TOKEN_FILE",
        hide_env_values = true,
        value_name = "PATH"
    )]
    token_file: Option<PathBuf>,
    /// For an https address: the only certificate(s) to trust — a private
    /// CA, or the listener's own self-signed certificate. System roots
    /// otherwise.
    #[arg(long, global = true, env = "JANUS_ADMIN_CA_FILE", value_name = "PATH")]
    ca_file: Option<PathBuf>,
    /// The daemon's config file: its data_dir locates the admin socket and
    /// token, and it is the file `check-config` validates.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Machine-readable output: the admin API's JSON documents (one JSON
    /// line per record for `log tail`); errors as JSON on stderr.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Daemon identity and health: listeners, upstreams, request log, credentials, plugin.
    Status,
    /// The credential pool.
    #[command(subcommand)]
    Credential(CredentialCommand),
    /// The opaque-identity registry.
    #[command(subcommand)]
    Identity(IdentityCommand),
    /// The request log.
    #[command(subcommand)]
    Log(LogCommand),
    /// The plugin channel.
    #[command(subcommand)]
    Plugin(PluginCommand),
    /// Run the daemon's full startup validation on --config and exit
    /// without binding anything (the stock daemon's check; a custom daemon
    /// build checks with its own `check-config`).
    CheckConfig,
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let ctx = Ctx {
        connection: Connection {
            admin: cli.admin,
            token_file: cli.token_file,
            ca_file: cli.ca_file,
            config: cli.config,
        },
        json: cli.json,
    };
    match &cli.command {
        Command::Status => commands::status::status(&ctx).await,
        Command::Credential(c) => commands::credential::run(&ctx, c).await,
        Command::Identity(c) => commands::identity::run(&ctx, c).await,
        Command::Log(c) => commands::log::run(&ctx, c).await,
        Command::Plugin(c) => commands::status::plugin(&ctx, c).await,
        Command::CheckConfig => {
            let path = ctx.connection.config.clone().ok_or_else(|| {
                CliError::usage("check-config needs the daemon's config: --config <path>")
            })?;
            commands::check_config(&ctx, &path).await
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("janus: cannot start the async runtime: {e}");
            return ExitCode::from(error::EXIT_FAILED);
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::StdoutClosed) => ExitCode::SUCCESS,
        Err(e) => {
            if json {
                eprintln!("{}", e.to_json());
            } else {
                eprintln!("janus: {e}");
            }
            ExitCode::from(e.exit_code())
        }
    }
}
