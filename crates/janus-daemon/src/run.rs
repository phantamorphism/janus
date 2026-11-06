//! Daemon lifecycle: startup validation, listener binding, and shutdown.
//!
//! [`run`] is the entry point the stock binary and every custom build call
//! (`docs/plugin-contract#custom-build`). Startup constructs and checks
//! everything before the data-plane listener binds, so each refusal the
//! docs list happens with nothing listening on the data-plane port:
//!
//! 1. the config's static rules ([`DaemonConfig::validate`]: listeners,
//!    plugin transport, upstreams, secrets, credentials) and config/linkage
//!    agreement ([`Embedded::check_against`]);
//! 2. the data directory, created mode `0700` when absent;
//! 3. the upstream implementations and the forwarding client;
//! 4. the store (`<data_dir>/janus.db`), whose registry the `opaque`
//!    method probes;
//! 5. every identity binding constructed — a binding naming a missing
//!    secret, an unreadable key file, an unreachable JWKS endpoint or an
//!    unregistered custom extractor refuses here;
//! 6. the credential pool (pool slots and `[[credential]]` entries);
//! 7. the plugin client (socket-path walk, TLS material);
//! 8. the admin surface's static rules ([`admin::check_config`]: admin and
//!    UI listeners, their TLS material, the UI assets, an existing admin
//!    token);
//! 9. the data-plane listener is bound (not yet served), then the admin
//!    surface starts with the bound address, then the data plane serves. A
//!    refusal at this step (a port in use) leaves nothing serving.
//!
//! [`check_config`] runs steps 1–8 and binds nothing. Shutdown (SIGINT or
//! SIGTERM in [`run`], or [`Daemon::shutdown`]) closes the listeners, lets
//! in-flight requests finish for up to [`SHUTDOWN_GRACE`](crate::dataplane::SHUTDOWN_GRACE), waits for their
//! settlement (request-log records, `Notify` events), then stops the
//! store's jobs, whose last act is the final request-log flush. `SIGHUP`
//! re-reads the plugin's TLS material.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use janus_core::contract::binding::Method;
use janus_core::contract::credential::{CredentialPool, PoolError, Secret};
use janus_core::contract::plugin::PluginMode;
use janus_core::contract::registry::OpaqueRegistry;
use janus_core::contract::selection::Strategy;
use janus_core::contract::upstream::UpstreamKind;
use janus_core::identity::{BindingTable, IdentityFactory, MaterialResolver, StartupError};
use janus_core::pool::{
    normalize_tags, pool_dir, ConfiguredCredential, ConfiguredSource, FilePool, OpenError,
    PoolOptions,
};
use janus_core::proxy::{ClientInitError, Forwarder, UpstreamSet};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::admin::{self, AdminDeps, AdminError, AdminHandle};
use crate::contract::config::{invalid, MaterialSource};
use crate::contract::{ConfigError, DaemonConfig, Embedded};
use crate::dataplane::{self, DataPlane, Tracker, UpstreamRoute};
use crate::plugin::{PluginHandle, SetupError};
use crate::store::{JobIntervals, RequestLog, Store, StoreError};

/// Why the daemon did not start (or `check-config` failed). Every variant
/// is a refusal before the data plane serves anything.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("no data directory: set data_dir in the config, $JANUS_DATA_DIR or $HOME")]
    NoDataDir,
    #[error("data directory {}: {source}", path.display())]
    DataDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    ClientInit(#[from] ClientInitError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("identity binding {0}")]
    Identity(#[from] StartupError),
    #[error("{0}")]
    Pool(#[from] OpenError),
    #[error("credential pool: {0}")]
    PoolList(#[from] PoolError),
    #[error("plugin: {0}")]
    Plugin(#[from] SetupError),
    #[error("admin surface: {0}")]
    Admin(#[from] AdminError),
    #[error("cannot bind the data plane on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("cannot install the signal handlers: {0}")]
    Signals(std::io::Error),
}

/// Everything startup constructs before anything listens.
struct Prepared {
    data_dir: PathBuf,
    store: Store,
    pool: Arc<FilePool>,
    plugin: Option<Arc<PluginHandle>>,
    bindings: BindingTable,
    upstreams: UpstreamSet,
    forwarder: Forwarder,
}

/// Resolves the material identity bindings name: `[secrets.<name>]` and
/// key files are read as exact bytes (no trimming), and the store's
/// registry backs `opaque`.
struct Materials<'a> {
    secrets: &'a BTreeMap<String, MaterialSource>,
    registry: Arc<dyn OpaqueRegistry>,
}

impl MaterialResolver for Materials<'_> {
    fn secret(&self, secret_ref: &str) -> Result<Secret, String> {
        let bytes = match self.secrets.get(secret_ref) {
            None => return Err(format!("no [secrets.{secret_ref}] is declared")),
            Some(MaterialSource::Env(var)) => {
                use std::os::unix::ffi::OsStringExt;
                std::env::var_os(var)
                    .ok_or_else(|| format!("environment variable {var} is not set"))?
                    .into_vec()
            }
            Some(MaterialSource::File(path)) => {
                std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?
            }
        };
        if bytes.is_empty() {
            return Err(format!("secret {secret_ref:?} is empty"));
        }
        Ok(Secret::new(bytes))
    }

    fn public_key_file(&self, path: &Path) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    }

    fn opaque_registry(&self) -> Option<Arc<dyn OpaqueRegistry>> {
        Some(Arc::clone(&self.registry))
    }
}

/// Create the data directory (mode `0700`) when absent.
fn ensure_data_dir(path: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::DirBuilderExt;
    let err = |source| RunError::DataDir {
        path: path.to_path_buf(),
        source,
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "not a directory",
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(err),
        Err(e) => Err(err(e)),
    }
}

fn configured_credentials(config: &DaemonConfig) -> Result<Vec<ConfiguredCredential>, ConfigError> {
    config
        .credentials
        .iter()
        .map(|c| {
            let source = match c.source() {
                Some(MaterialSource::Env(var)) => ConfiguredSource::Env(var),
                Some(MaterialSource::File(path)) => ConfiguredSource::File(path),
                None => {
                    return Err(invalid(
                        format!("credential[{}]", c.credential_ref),
                        "exactly one of env / file is required",
                    ))
                }
            };
            Ok(ConfiguredCredential {
                credential_ref: c.credential_ref.clone(),
                upstream: c.upstream,
                label: c.label.clone(),
                tags: normalize_tags(c.tags.clone()),
                source,
            })
        })
        .collect()
}

/// Steps 1–8 of startup.
async fn prepare(config: &DaemonConfig, embedded: &Embedded) -> Result<Prepared, RunError> {
    config.validate()?;
    embedded.check_against(config)?;
    let data_dir = config.data_dir().ok_or(RunError::NoDataDir)?;
    ensure_data_dir(&data_dir)?;
    let prepared = build(config, embedded, data_dir).await?;
    check_selections(config, prepared.pool.as_ref()).await?;
    Ok(prepared)
}

/// The selection rules only the pool can decide: a filter `ref` naming a
/// pool slot of another implementation is a refusal; a `ref` naming no
/// credential, and an instance that admits no credential, are warnings
/// (credentials can be added at runtime).
async fn check_selections(config: &DaemonConfig, pool: &FilePool) -> Result<(), RunError> {
    let all = pool.list(None).await?;
    for up in &config.upstreams {
        let filter = up.credentials.clone().unwrap_or_default();
        for r in filter.refs.iter().flatten() {
            match all.iter().find(|m| &m.credential_ref == r) {
                Some(m) if m.upstream != up.implementation => {
                    return Err(invalid(
                        format!("upstream[{}].credentials.refs", up.name),
                        format!(
                            "credential {r:?} is for {}, not {}",
                            m.upstream, up.implementation
                        ),
                    )
                    .into())
                }
                Some(_) => {}
                None => tracing::warn!(
                    upstream = %up.name,
                    credential = %r,
                    "upstream credential filter names a credential that does not exist (yet)"
                ),
            }
        }
        let admitted = all
            .iter()
            .filter(|m| {
                m.upstream == up.implementation && filter.admits(&m.credential_ref, m.kind, &m.tags)
            })
            .count();
        if admitted == 0 {
            tracing::warn!(
                upstream = %up.name,
                implementation = %up.implementation,
                "upstream admits no credential; its requests fail as credential_unavailable until one is added"
            );
        }
    }
    Ok(())
}

/// The validation and construction of [`prepare`] against `data_dir`, which
/// must exist. Everything stateful (database, pool directory) is created
/// under `data_dir`; `admin::check_config` only reads the real data
/// directory configured, which [`check_config`] passes separately.
async fn build(
    config: &DaemonConfig,
    embedded: &Embedded,
    data_dir: PathBuf,
) -> Result<Prepared, RunError> {
    let token_url = |kind| {
        config
            .upstreams
            .iter()
            .find(|up| up.implementation == kind)
            .and_then(|up| up.token_url.clone())
    };
    let upstreams = UpstreamSet::with_token_urls(
        token_url(UpstreamKind::Anthropic),
        token_url(UpstreamKind::OpenAi),
    )?;
    let forwarder = Forwarder::new()?;
    let store = Store::open(&data_dir, &config.request_log)?;

    let factory = IdentityFactory::new();
    let materials = Materials {
        secrets: &config.secrets,
        registry: Arc::clone(store.registry()) as Arc<dyn OpaqueRegistry>,
    };
    let mut builder = BindingTable::builder(&factory, &materials, &embedded.extractors)?;
    for up in config.resolve_bindings()? {
        builder.upstream(&up.upstream, up.binding.as_ref()).await?;
        for route in &up.routes {
            builder
                .route(&up.upstream, &route.path, &route.binding)
                .await?;
        }
    }
    let bindings = builder.finish();

    let pool = FilePool::open(PoolOptions {
        pool_dir: pool_dir(&data_dir),
        upstreams: UpstreamKind::ALL
            .iter()
            .map(|kind| Arc::clone(upstreams.get(*kind)))
            .collect(),
        configured: configured_credentials(config)?,
    })?;
    let plugin = PluginHandle::from_config(config.plugin.as_ref(), embedded.plugin.clone())?;
    admin::check_config(config, &config.data_dir().ok_or(RunError::NoDataDir)?)?;

    Ok(Prepared {
        data_dir,
        store,
        pool: Arc::new(pool),
        plugin: plugin.map(Arc::new),
        bindings,
        upstreams,
        forwarder,
    })
}

/// How long shutdown waits for settlement work after the data plane has
/// stopped, so a wedged task cannot hold the process up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// A running daemon. [`run`] is this plus signal handling; a custom build or
/// a test that wants the bound address or its own shutdown trigger uses it
/// directly.
pub struct Daemon {
    data_plane_addr: SocketAddr,
    cancel: CancellationToken,
    serving: JoinHandle<()>,
    tracker: Tracker,
    admin: AdminHandle,
    jobs_cancel: CancellationToken,
    jobs: JoinHandle<()>,
    plugin: Option<Arc<PluginHandle>>,
    requests: Arc<RequestLog>,
    refused: Arc<AtomicU64>,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("data_plane_addr", &self.data_plane_addr)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Validate, construct and bind everything, and start serving.
    pub async fn start(config: DaemonConfig, embedded: Embedded) -> Result<Daemon, RunError> {
        let prepared = prepare(&config, &embedded).await?;
        let plugin_mode = config.plugin_mode()?;
        let config = Arc::new(config);
        let cancel = CancellationToken::new();
        let requests = Arc::clone(prepared.store.requests());
        let pool: Arc<dyn CredentialPool> = prepared.pool;

        // Bind the data plane first, without serving, so the admin surface
        // reports the address actually bound (port 0 included); nothing is
        // accepted until `serve` runs, after the admin surface is up.
        let addr = config.data_plane.bind;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| RunError::Bind { addr, source })?;
        let data_plane_addr = listener
            .local_addr()
            .map_err(|source| RunError::Bind { addr, source })?;

        let refused = Arc::new(AtomicU64::new(0));
        let routes: Vec<UpstreamRoute> = config.upstreams.iter().map(UpstreamRoute::from).collect();
        let admin = admin::start(
            AdminDeps {
                data_plane_refused: Arc::clone(&refused),
                config: Arc::clone(&config),
                data_dir: prepared.data_dir.clone(),
                pool: Arc::clone(&pool),
                selections: routes.iter().map(|r| Arc::clone(&r.selection)).collect(),
                registry: Arc::clone(prepared.store.registry()),
                requests: Arc::clone(&requests),
                plugin: prepared.plugin.clone(),
                started_at: SystemTime::now(),
                data_plane_addr,
            },
            cancel.clone(),
        )
        .await?;

        let jobs_cancel = CancellationToken::new();
        let jobs = prepared.store.spawn_jobs(
            JobIntervals::for_retention(config.request_log.retain),
            jobs_cancel.clone(),
        );
        let tracker = Tracker::default();
        let dp = Arc::new(DataPlane {
            routes,
            bindings: prepared.bindings,
            upstreams: prepared.upstreams,
            forwarder: prepared.forwarder,
            pool,
            plugin: prepared.plugin.clone(),
            requests: Arc::clone(&requests),
            tracker: tracker.clone(),
            max_connections: config.data_plane.max_connections,
            refused: Arc::clone(&refused),
        });
        let serving = tokio::spawn(dataplane::serve(listener, dp, cancel.clone()));
        tracing::info!(
            data_plane = %data_plane_addr,
            data_dir = %prepared.data_dir.display(),
            plugin = %plugin_mode,
            "janus-daemon started"
        );

        Ok(Daemon {
            data_plane_addr,
            cancel,
            serving,
            tracker,
            admin,
            jobs_cancel,
            jobs,
            plugin: prepared.plugin,
            requests,
            refused,
        })
    }

    /// The address the data plane is listening on (useful with port 0).
    pub fn data_plane_addr(&self) -> SocketAddr {
        self.data_plane_addr
    }

    /// The request log the data plane writes to.
    pub fn requests(&self) -> &Arc<RequestLog> {
        &self.requests
    }

    /// Data-plane connections closed unanswered since start for being over
    /// `data_plane.max_connections`.
    pub fn refused_connections(&self) -> u64 {
        self.refused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Re-read the plugin's TLS material (what `SIGHUP` does); a no-op
    /// without a TLS plugin. On error the last good material stays in use.
    pub fn reload_tls(&self) -> Result<(), ConfigError> {
        match &self.plugin {
            Some(plugin) => plugin.reload_tls(),
            None => Ok(()),
        }
    }

    /// Stop: close the listeners, give in-flight requests up to
    /// [`SHUTDOWN_GRACE`](crate::dataplane::SHUTDOWN_GRACE), wait for every request's settlement, then stop
    /// the store's jobs after their final flush.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        if let Err(e) = self.serving.await {
            tracing::error!(error = %e, "data-plane task failed");
        }
        if tokio::time::timeout(IDLE_TIMEOUT, self.tracker.idle())
            .await
            .is_err()
        {
            tracing::warn!("settlement work still running after the timeout; shutting down anyway");
        }
        // Commit the settled records before anything else can delay the
        // process (an admin connection draining, a supervisor's kill
        // timer): the jobs' final flush below then only covers stragglers.
        self.requests.flush().await;
        self.admin.wait().await;
        self.jobs_cancel.cancel();
        if let Err(e) = self.jobs.await {
            tracing::error!(error = %e, "store jobs task failed");
        }
        tracing::info!("janus-daemon stopped");
    }
}

/// Run the daemon until SIGINT or SIGTERM, re-reading plugin TLS material
/// on SIGHUP.
pub async fn run(config: DaemonConfig, embedded: Embedded) -> Result<(), RunError> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hangup = signal(SignalKind::hangup()).map_err(RunError::Signals)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(RunError::Signals)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(RunError::Signals)?;

    let daemon = Daemon::start(config, embedded).await?;
    loop {
        tokio::select! {
            _ = hangup.recv() => match daemon.reload_tls() {
                Ok(()) => tracing::info!("SIGHUP: plugin TLS material re-read"),
                Err(e) => tracing::error!(error = %e, "SIGHUP: plugin TLS reload failed; keeping the last good material"),
            },
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    tracing::info!("shutting down");
    daemon.shutdown().await;
    Ok(())
}

/// What `check-config` validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub data_dir: PathBuf,
    pub data_plane: SocketAddr,
    pub plugin: PluginMode,
    pub upstreams: Vec<CheckedUpstream>,
    /// The `[[credential]]` entries the config declares (pool slots already
    /// on disk are not read: the check touches no state).
    pub credentials: usize,
}

/// One upstream's resolved bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpstream {
    /// The instance name.
    pub upstream: String,
    pub implementation: UpstreamKind,
    pub strategy: Strategy,
    pub failover: bool,
    pub path_prefix: String,
    /// `None`: requests matched by no route are rejected.
    pub binding: Option<Method>,
    pub routes: Vec<(String, Method)>,
}

impl fmt::Display for CheckReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "config OK")?;
        writeln!(f, "data_dir    {}", self.data_dir.display())?;
        writeln!(f, "data_plane  {}", self.data_plane)?;
        writeln!(f, "plugin      {}", self.plugin)?;
        writeln!(f, "credentials {}", self.credentials)?;
        for up in &self.upstreams {
            let binding = up
                .binding
                .as_ref()
                .map_or_else(|| "unbound".to_owned(), ToString::to_string);
            writeln!(
                f,
                "upstream    {} ({}) {} identity={binding} strategy={} failover={}",
                up.upstream, up.implementation, up.path_prefix, up.strategy, up.failover
            )?;
            for (path, method) in &up.routes {
                writeln!(f, "  route     {}{path} identity={method}", up.path_prefix)?;
            }
        }
        Ok(())
    }
}

/// `janus-daemon check-config`: load the config at `path` and run the full
/// startup validation against `embedded` without binding any listener.
/// It has no side effects: the database and pool are built in a scratch
/// directory that is removed afterwards, so the configured data directory
/// is neither created nor touched.
pub async fn check_config(path: &Path, embedded: Embedded) -> Result<CheckReport, RunError> {
    let config = DaemonConfig::load(path)?;
    config.validate()?;
    embedded.check_against(&config)?;
    let data_dir = config.data_dir().ok_or(RunError::NoDataDir)?;
    let scratch = tempfile::tempdir().map_err(|source| RunError::DataDir {
        path: data_dir.clone(),
        source,
    })?;
    let prepared = build(&config, &embedded, scratch.path().to_path_buf()).await?;
    let credentials = prepared.pool.list(None).await?.len();
    let upstreams = config
        .resolve_bindings()?
        .into_iter()
        .zip(&config.upstreams)
        .map(|(up, c)| CheckedUpstream {
            upstream: up.upstream,
            implementation: c.implementation,
            strategy: c.strategy,
            failover: c.failover,
            path_prefix: c.path_prefix.clone(),
            binding: up.binding.as_ref().map(|b| b.method()),
            routes: up
                .routes
                .iter()
                .map(|r| (r.path.clone(), r.binding.method()))
                .collect(),
        })
        .collect();
    Ok(CheckReport {
        data_dir,
        data_plane: config.data_plane.bind,
        plugin: config.plugin_mode()?,
        upstreams,
        credentials,
    })
}
