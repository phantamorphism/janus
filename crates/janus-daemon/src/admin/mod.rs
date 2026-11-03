//! The admin surface: the `/v1` HTTP+JSON API on the admin unix socket, the
//! opt-in TCP admin listener and the UI listener, with token auth and the
//! request-log SSE stream. See `docs/admin-surface` and
//! `docs/architecture#admin-surface`.
//!
//! [`start`] is the one entry point the lifecycle calls. It refuses to start
//! (returning [`AdminError`]) before anything listens when a listener rule is
//! broken, then:
//!
//! - binds `<data_dir>/admin.sock`, mode `0600` ([`listen`]): filesystem
//!   permissions are the authentication, so requests on it need no token and
//!   are audited with the principal `socket`;
//! - loads or generates `<data_dir>/admin-token`, mode `0600` ([`token`]);
//! - binds `admin.bind` when set and `ui.bind` when `[ui]` exists, each with
//!   TLS when `tls = { cert, key }` is configured. Every API request on
//!   either needs `Authorization: Bearer <admin token>`, loopback included,
//!   and is audited with the principal `token`. The UI listener also serves
//!   the built UI ([`ui`]).
//!
//! Every listener mounts the same API ([`api`]): status and plugin
//! ([`status`]), credentials ([`credentials`]), identities
//! ([`identities`]) and the request log with its live tail ([`requests`]).
//! No response ever carries credential material; the one secret handed out
//! is a new opaque-identity token, once, in the `POST /v1/identities`
//! response.
//!
//! Decisions where the docs leave a gap:
//!
//! - list responses are envelopes: `{ "credentials": [...] }`,
//!   `{ "identities": [...] }`, `{ "requests": [...], "next", "disabled" }`;
//!   `PUT /v1/credentials/{ref}/material` takes `{ "material": {...} }`,
//!   the same field `POST /v1/credentials` uses; deletes answer `204`;
//! - the live tail is live only: events carry no SSE `id`, so a reconnect
//!   (with or without `Last-Event-ID`) resumes from "now", and a client
//!   closes the gap with `GET /v1/requests?since=`. A subscriber that falls
//!   behind gets a `lagged` event with the number of records it missed. The
//!   first event is a `ping`, sent once the subscription is in place; with
//!   the request log disabled the stream is one `disabled` event;
//! - `POST /v1/plugin/check` with no `[plugin]` block is `409 no_plugin`:
//!   there is no channel to check;
//! - the UI's assets come from `ui.assets_dir`, else from the `ui/dist`
//!   directory of the source tree this daemon was built from; a directory
//!   without `index.html` is a startup refusal. Unknown non-API paths get
//!   `index.html` (the single-page app routes them);
//! - an `admin-token` file that already exists must be a regular file owned
//!   by the daemon's user with no group or other permission bits, and hold
//!   at least [`token::MIN_TOKEN_LEN`] characters; otherwise the daemon
//!   refuses to start rather than accept a weak or exposed token.

mod api;
mod credentials;
mod error;
mod identities;
mod listen;
mod requests;
mod status;
mod token;
mod ui;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use janus_core::contract::credential::CredentialPool;
use janus_core::contract::paths;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contract::config::{ConfigError, ListenerTls};
use crate::contract::DaemonConfig;
use crate::plugin::PluginHandle;
use crate::store;

pub use requests::PING_INTERVAL;
pub use token::MIN_TOKEN_LEN;

/// What the admin surface serves from. Built by the lifecycle once the pool,
/// the store and the plugin client exist.
pub struct AdminDeps {
    pub config: Arc<DaemonConfig>,
    /// The resolved data directory; `admin.sock` and `admin-token` live
    /// here. Must exist.
    pub data_dir: PathBuf,
    pub pool: Arc<dyn CredentialPool>,
    pub registry: Arc<store::Registry>,
    pub requests: Arc<store::RequestLog>,
    /// `None` without a `[plugin]` block (`plugin.mode = "none"`), the case
    /// [`PluginHandle::from_config`] answers with `None`.
    pub plugin: Option<Arc<PluginHandle>>,
    pub started_at: SystemTime,
}

/// Why the admin surface refused to start. Nothing is left listening when
/// [`start`] returns one of these.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// A listener rule of the config: a non-loopback bind without
    /// `allow_non_loopback`, or without TLS and without `allow_plaintext`,
    /// colliding listener addresses.
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("data directory {}: {source}", path.display())]
    DataDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("admin token file {}: {reason}", path.display())]
    Token { path: PathBuf, reason: String },
    /// Another process answers on the admin socket: a second daemon over the
    /// same data directory.
    #[error("admin socket {} is in use by another process", path.display())]
    SocketInUse { path: PathBuf },
    #[error("admin socket {}: {reason}", path.display())]
    Socket { path: PathBuf, reason: String },
    #[error("cannot bind the {listener} listener on {addr}: {source}")]
    Bind {
        listener: &'static str,
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("{listener} TLS: {reason}")]
    Tls {
        listener: &'static str,
        reason: String,
    },
    #[error("UI assets directory {}: {reason}", path.display())]
    UiAssets { path: PathBuf, reason: String },
}

/// The running admin surface.
pub struct AdminHandle {
    socket_path: PathBuf,
    token_path: PathBuf,
    admin_tcp: Option<SocketAddr>,
    ui: Option<SocketAddr>,
    ui_tls: bool,
    tasks: JoinSet<()>,
}

impl std::fmt::Debug for AdminHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminHandle")
            .field("socket_path", &self.socket_path)
            .field("token_path", &self.token_path)
            .field("admin_tcp", &self.admin_tcp)
            .field("ui", &self.ui)
            .finish_non_exhaustive()
    }
}

impl AdminHandle {
    /// `<data_dir>/admin.sock`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// `<data_dir>/admin-token`.
    pub fn token_path(&self) -> &Path {
        &self.token_path
    }

    /// The bound address of the TCP admin listener, when `admin.bind` is set.
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.admin_tcp
    }

    /// The bound address of the UI listener, when `[ui]` is configured.
    pub fn ui_addr(&self) -> Option<SocketAddr> {
        self.ui
    }

    /// The URL the UI is reached at.
    pub fn ui_url(&self) -> Option<String> {
        self.ui.map(|addr| listen::url_of(addr, self.ui_tls))
    }

    /// Wait until every listener has stopped: after the cancellation token
    /// passed to [`start`] fires, open connections have been closed (live
    /// tails end at once; other requests get [`listen::SHUTDOWN_GRACE`]),
    /// and the admin socket file has been removed.
    pub async fn wait(mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(e) = result {
                tracing::error!(error = %e, "admin listener task failed");
            }
        }
    }
}

/// Start the admin surface: bind every admin listener and serve until
/// `cancel` fires.
pub async fn start(deps: AdminDeps, cancel: CancellationToken) -> Result<AdminHandle, AdminError> {
    // The listener rules are part of the config's static validation; run it
    // again here so no caller can start a listener the docs forbid, and so
    // the refusal happens before anything is bound.
    deps.config.validate()?;
    let upstreams = status::upstream_summaries(&deps.config)?;
    let meta = std::fs::metadata(&deps.data_dir).map_err(|source| AdminError::DataDir {
        path: deps.data_dir.clone(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(AdminError::DataDir {
            path: deps.data_dir.clone(),
            source: std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"),
        });
    }

    let token_path = paths::admin_token_path(&deps.data_dir);
    let admin_token = Arc::new(token::AdminToken::load_or_create(&token_path)?);

    // Everything that can fail is checked before the first bind.
    let admin_tls = load_tls("admin", deps.config.admin.tls.as_ref())?;
    let ui_setup = match &deps.config.ui {
        Some(ui_config) => Some((
            ui_config.bind,
            load_tls("UI", ui_config.tls.as_ref())?,
            ui::assets(ui_config.assets_dir.as_deref())?,
        )),
        None => None,
    };

    let admin_tcp = match deps.config.admin.bind {
        Some(addr) => Some(listen::bind_tcp("admin", addr).await?),
        None => None,
    };
    let ui_tcp = match &ui_setup {
        Some((addr, _, _)) => Some(listen::bind_tcp("UI", *addr).await?),
        None => None,
    };
    let socket_path = paths::admin_socket_path(&deps.data_dir);
    let unix = listen::UnixSocket::bind(&deps.data_dir, &socket_path)?;

    let admin_addr = admin_tcp.as_ref().map(|(_, a)| *a);
    let ui_addr = ui_tcp.as_ref().map(|(_, a)| *a);
    let ui_tls = ui_setup.as_ref().is_some_and(|(_, t, _)| t.is_some());
    let listeners = status::Listeners {
        admin_socket: socket_path.clone(),
        admin_tcp: admin_addr.map(|address| status::TcpListenerInfo {
            address,
            tls: admin_tls.is_some(),
            url: listen::url_of(address, admin_tls.is_some()),
        }),
        ui: ui_addr.map(|address| status::TcpListenerInfo {
            address,
            tls: ui_tls,
            url: listen::url_of(address, ui_tls),
        }),
        data_plane: deps.config.data_plane.bind,
    };
    let state = api::AdminState::new(deps, listeners, upstreams, cancel.clone());

    let mut tasks = JoinSet::new();
    tasks.spawn(listen::serve(
        listen::Acceptor::Unix(unix),
        api::socket_router(state.clone()),
        cancel.clone(),
    ));
    if let Some((listener, addr)) = admin_tcp {
        tasks.spawn(listen::serve(
            listen::Acceptor::Tcp(listener, admin_tls),
            api::token_router(state.clone(), Arc::clone(&admin_token)),
            cancel.clone(),
        ));
        tracing::info!(address = %addr, token_file = %token_path.display(), "admin TCP listener ready");
    }
    if let (Some((listener, addr)), Some((_, tls, assets))) = (ui_tcp, ui_setup) {
        tasks.spawn(listen::serve(
            listen::Acceptor::Tcp(listener, tls),
            api::ui_router(state.clone(), Arc::clone(&admin_token), &assets),
            cancel.clone(),
        ));
        tracing::info!(
            url = %listen::url_of(addr, ui_tls),
            token_file = %token_path.display(),
            "UI listener ready; the UI asks for the admin token in the token file"
        );
    }
    tracing::info!(socket = %socket_path.display(), "admin socket ready");

    Ok(AdminHandle {
        socket_path,
        token_path,
        admin_tcp: admin_addr,
        ui: ui_addr,
        ui_tls,
        tasks,
    })
}

fn load_tls(
    listener: &'static str,
    tls: Option<&ListenerTls>,
) -> Result<Option<tokio_rustls::TlsAcceptor>, AdminError> {
    tls.map(|t| listen::tls_acceptor(listener, t)).transpose()
}
