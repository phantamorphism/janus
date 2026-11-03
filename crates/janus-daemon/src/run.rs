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
//! 8. the admin surface, whose own listener and TLS rules refuse before
//!    it binds;
//! 9. the data-plane listener.
//!
//! [`check_config`] runs steps 1–7 and binds nothing. Shutdown (SIGINT or
//! SIGTERM in [`run`], or [`Daemon::shutdown`]) closes the listeners, lets
//! in-flight requests finish for up to [`SHUTDOWN_GRACE`](crate::dataplane::SHUTDOWN_GRACE), waits for their
//! settlement (request-log records, `Notify` events), then stops the
//! store's jobs, whose last act is the final request-log flush. `SIGHUP`
//! re-reads the plugin's TLS material.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use janus_core::contract::binding::Method;
use janus_core::contract::credential::{CredentialPool, PoolError, Secret};
use janus_core::contract::plugin::PluginMode;
use janus_core::contract::registry::OpaqueRegistry;
use janus_core::contract::upstream::UpstreamKind;
use janus_core::identity::{BindingTable, IdentityFactory, MaterialResolver, StartupError};
use janus_core::pool::{
    pool_dir, ConfiguredCredential, ConfiguredSource, FilePool, OpenError, PoolOptions,
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
                source,
            })
        })
        .collect()
}

/// Steps 1–7 of startup.
async fn prepare(config: &DaemonConfig, embedded: &Embedded) -> Result<Prepared, RunError> {
    config.validate()?;
    embedded.check_against(config)?;
    let data_dir = config.data_dir().ok_or(RunError::NoDataDir)?;
    ensure_data_dir(&data_dir)?;

    let upstreams = UpstreamSet::new()?;
    let forwarder = Forwarder::new()?;
    let store = Store::open(&data_dir, &config.request_log)?;

    let factory = IdentityFactory::new();
    let materials = Materials {
        secrets: &config.secrets,
        registry: Arc::clone(store.registry()) as Arc<dyn OpaqueRegistry>,
    };
    let mut builder = BindingTable::builder(&factory, &materials, &embedded.extractors)?;
    for up in config.resolve_bindings()? {
        builder.upstream(up.upstream, up.binding.as_ref()).await?;
        for route in &up.routes {
            builder
                .route(up.upstream, &route.path, &route.binding)
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

        let admin = admin::start(
            AdminDeps {
                config: Arc::clone(&config),
                data_dir: prepared.data_dir.clone(),
                pool: Arc::clone(&pool),
                registry: Arc::clone(prepared.store.registry()),
                requests: Arc::clone(&requests),
                plugin: prepared.plugin.clone(),
                started_at: SystemTime::now(),
            },
            cancel.clone(),
        )
        .await?;

        let addr = config.data_plane.bind;
        let listener = match TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(source) => {
                cancel.cancel();
                admin.wait().await;
                return Err(RunError::Bind { addr, source });
            }
        };
        let data_plane_addr = listener.local_addr().unwrap_or(addr);

        let jobs_cancel = CancellationToken::new();
        let jobs = prepared.store.spawn_jobs(
            JobIntervals::for_retention(config.request_log.retain),
            jobs_cancel.clone(),
        );
        let tracker = Tracker::default();
        let dp = Arc::new(DataPlane {
            routes: config.upstreams.iter().map(UpstreamRoute::from).collect(),
            bindings: prepared.bindings,
            upstreams: prepared.upstreams,
            forwarder: prepared.forwarder,
            pool,
            plugin: prepared.plugin.clone(),
            requests: Arc::clone(&requests),
            tracker: tracker.clone(),
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
        self.tracker.idle().await;
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
    /// Credentials in the pool (slots and `[[credential]]` entries).
    pub credentials: usize,
}

/// One upstream's resolved bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpstream {
    pub upstream: UpstreamKind,
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
                "upstream    {} {} identity={binding}",
                up.upstream, up.path_prefix
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
/// Like a first start, it may create the data directory, the database and
/// the pool directory.
pub async fn check_config(path: &Path, embedded: Embedded) -> Result<CheckReport, RunError> {
    let config = DaemonConfig::load(path)?;
    let prepared = prepare(&config, &embedded).await?;
    let credentials = prepared.pool.list(None).await?.len();
    let upstreams = config
        .resolve_bindings()?
        .into_iter()
        .map(|up| CheckedUpstream {
            upstream: up.upstream,
            path_prefix: config
                .upstreams
                .iter()
                .find(|c| c.name == up.upstream)
                .map(|c| c.path_prefix.clone())
                .unwrap_or_default(),
            binding: up.binding.as_ref().map(|b| b.method()),
            routes: up
                .routes
                .iter()
                .map(|r| (r.path.clone(), r.binding.method()))
                .collect(),
        })
        .collect();
    Ok(CheckReport {
        data_dir: prepared.data_dir,
        data_plane: config.data_plane.bind,
        plugin: config.plugin_mode()?,
        upstreams,
        credentials,
    })
}
