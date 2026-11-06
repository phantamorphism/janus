//! `tenant-pinning-plugin --policy <file> --socket <path> [--daemon-uid <uid>]`

use std::path::PathBuf;

use clap::Parser;
use tenant_pinning_plugin::policy::Policy;
use tenant_pinning_plugin::service::TenantPinning;

/// Example Janus policy plugin: pins credentials by tenant claim, denies
/// blocked callers, meters a per-identity request budget.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// The policy file (TOML), read once at startup.
    #[arg(long)]
    policy: PathBuf,
    /// The unix socket to serve on; the daemon's `[plugin] endpoint` is
    /// `unix://` followed by this absolute path.
    #[arg(long)]
    socket: PathBuf,
    /// Refuse calls from any peer not running as this uid (the daemon's).
    #[arg(long)]
    daemon_uid: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<(), tenant_pinning_plugin::Error> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let policy = Policy::load(&args.policy)?;
    tracing::info!(event = "policy_loaded", path = %args.policy.display(),
        tenants = policy.tenants.len(), deny_unlisted = policy.deny_unlisted,
        budget_requests = policy.budget.requests, budget_window_secs = policy.budget.window_secs);
    let plugin = TenantPinning::new(policy)?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    };
    tenant_pinning_plugin::serve(plugin, &args.socket, args.daemon_uid, shutdown).await
}
