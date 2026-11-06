//! The data-plane listener and request pipeline: identity extraction,
//! `ResolveRoute`, credential fetch, substitution, forwarding, and settlement
//! events. See `docs/architecture#request-lifecycle`.
//!
//! - [`pipeline`] — steps 1–6 for one request;
//! - [`settle`] — step 7: the request-log record and the settlement events,
//!   exactly once per request;
//! - [`reply`] — the responses the data plane produces itself;
//! - [`serve`] — the accept loop and graceful shutdown;
//! - [`tracker`] — waits for settlement work at shutdown.
//!
//! The router mounts proxy routes only: a path that is not under an
//! upstream's `path_prefix` — every admin path among them — is a 404
//! (`docs/architecture#admin-surface`).

mod pipeline;
mod reply;
mod serve;
mod settle;
mod tracker;

use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use janus_core::contract::credential::CredentialPool;
use janus_core::contract::selection::Selection;
use janus_core::contract::upstream::UpstreamKind;
use janus_core::identity::BindingTable;
use janus_core::proxy::{Forwarder, UpstreamSet};
use url::Url;

use crate::contract::config::UpstreamConfig;
use crate::plugin::PluginHandle;
use crate::store::RequestLog;

pub(crate) use serve::serve;
pub use serve::SHUTDOWN_GRACE;
pub(crate) use tracker::Tracker;

/// One `[[upstream]]` instance as the router sees it.
#[derive(Debug, Clone)]
pub(crate) struct UpstreamRoute {
    pub(crate) kind: UpstreamKind,
    pub(crate) path_prefix: String,
    pub(crate) base_url: Url,
    /// The instance's name, credential filter and strategy; shared with the
    /// admin surface.
    pub(crate) selection: Arc<Selection>,
}

impl UpstreamRoute {
    /// The instance name.
    pub(crate) fn name(&self) -> &str {
        self.selection.instance()
    }
}

impl From<&UpstreamConfig> for UpstreamRoute {
    fn from(up: &UpstreamConfig) -> Self {
        UpstreamRoute {
            kind: up.implementation,
            path_prefix: up.path_prefix.clone(),
            base_url: up.base_url.clone(),
            selection: Arc::new(up.selection()),
        }
    }
}

/// Everything the pipeline needs, shared by every connection.
pub(crate) struct DataPlane {
    pub(crate) routes: Vec<UpstreamRoute>,
    pub(crate) bindings: BindingTable,
    pub(crate) upstreams: UpstreamSet,
    pub(crate) forwarder: Forwarder,
    pub(crate) pool: Arc<dyn CredentialPool>,
    pub(crate) plugin: Option<Arc<PluginHandle>>,
    pub(crate) requests: Arc<RequestLog>,
    pub(crate) tracker: Tracker,
    /// `data_plane.max_connections`.
    pub(crate) max_connections: usize,
    /// Connections closed unanswered for being over `max_connections`;
    /// shared with the admin surface (`data_plane.refused_connections`).
    pub(crate) refused: Arc<AtomicU64>,
}

impl DataPlane {
    /// The upstream whose `path_prefix` is a whole-segment prefix of
    /// `path`, and the path with that prefix stripped (`/` when nothing is
    /// left). Config validation guarantees at most one matches.
    fn route<'p>(&self, path: &'p str) -> Option<(&UpstreamRoute, &'p str)> {
        self.routes.iter().find_map(|up| {
            let rest = path.strip_prefix(up.path_prefix.as_str())?;
            match rest {
                "" => Some((up, "/")),
                _ if rest.starts_with('/') => Some((up, rest)),
                _ => None,
            }
        })
    }

    /// Run settlement work off the caller's path, counted by the tracker so
    /// shutdown waits for it.
    fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        let in_flight = self.tracker.token();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _in_flight = in_flight;
                    work.await;
                });
            }
            Err(_) => tracing::error!(
                "no runtime to settle a request on; its record and settlement event are lost"
            ),
        }
    }
}

/// The data-plane router: one fallback handler, no other routes.
pub(crate) fn router(dp: Arc<DataPlane>) -> axum::Router {
    axum::Router::new().fallback(pipeline::proxy).with_state(dp)
}
