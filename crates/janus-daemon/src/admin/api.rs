//! The `/v1` router, its shared state, the token check, and mutation
//! auditing. Every listener mounts the same API; they differ only in how a
//! request is authenticated ([`Principal`]).

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use tokio_util::sync::CancellationToken;

use super::error::ApiError;
use super::status::{Listeners, UpstreamSummary};
use super::token::AdminToken;
use super::{credentials, identities, requests, status, ui, AdminDeps};

/// Who made an admin call, as the audit log names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Principal {
    /// The unix socket: filesystem permissions authenticated the caller.
    Socket,
    /// A TCP listener (admin or UI): the admin token did.
    Token,
}

impl Principal {
    fn as_str(self) -> &'static str {
        match self {
            Principal::Socket => "socket",
            Principal::Token => "token",
        }
    }
}

pub(crate) struct Inner {
    pub deps: AdminDeps,
    pub listeners: Listeners,
    pub upstreams: Vec<UpstreamSummary>,
    /// Daemon shutdown: ends live tails.
    pub cancel: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct AdminState(Arc<Inner>);

impl std::ops::Deref for AdminState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AdminState {
    pub(crate) fn new(
        deps: AdminDeps,
        listeners: Listeners,
        upstreams: Vec<UpstreamSummary>,
        cancel: CancellationToken,
    ) -> Self {
        AdminState(Arc::new(Inner {
            deps,
            listeners,
            upstreams,
            cancel,
        }))
    }
}

/// Record a mutating call in the daemon log: who, what, on which resource,
/// and whether it worked. Never given material or tokens.
pub(crate) fn audit<T>(
    principal: Principal,
    action: &'static str,
    resource: &str,
    result: &Result<T, ApiError>,
) {
    match result {
        Ok(_) => tracing::info!(
            target: "janus::admin::audit",
            principal = principal.as_str(),
            action,
            resource,
            outcome = "ok",
            "admin mutation"
        ),
        Err(e) => tracing::info!(
            target: "janus::admin::audit",
            principal = principal.as_str(),
            action,
            resource,
            outcome = "error",
            code = e.code,
            "admin mutation"
        ),
    }
}

/// Run a short synchronous store call (one SQLite statement) off the async
/// workers.
pub(crate) async fn blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    E: Into<ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ApiError::from)?
        .map_err(Into::into)
}

/// The `/v1` routes, relative to `/v1`.
fn v1(state: AdminState) -> Router {
    Router::new()
        .route("/status", get(status::status))
        .route("/plugin", get(status::plugin))
        .route("/plugin/check", post(status::plugin_check))
        .route(
            "/credentials",
            get(credentials::list).post(credentials::add),
        )
        .route("/credentials/default", get(credentials::peek_default))
        .route(
            "/credentials/{ref}",
            get(credentials::show).delete(credentials::delete),
        )
        .route(
            "/credentials/{ref}/material",
            put(credentials::replace_material),
        )
        .route("/credentials/{ref}/enable", post(credentials::enable))
        .route("/credentials/{ref}/disable", post(credentials::disable))
        .route("/credentials/{ref}/refresh", post(credentials::refresh))
        .route(
            "/identities",
            get(identities::list).post(identities::create),
        )
        .route(
            "/identities/{id}",
            get(identities::show).delete(identities::delete),
        )
        .route("/identities/{id}/revoke", post(identities::revoke))
        .route("/requests", get(requests::query))
        .route("/requests/stream", get(requests::stream))
        .route("/requests/{request_id}", get(requests::show))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

fn api(state: AdminState) -> Router {
    Router::new().nest("/v1", v1(state)).fallback(not_found)
}

/// The unix socket's router: no token.
pub(crate) fn socket_router(state: AdminState) -> Router {
    api(state).layer(Extension(Principal::Socket))
}

/// The TCP admin listener's router: the token on every request.
pub(crate) fn token_router(state: AdminState, token: Arc<AdminToken>) -> Router {
    api(state).layer(middleware::from_fn_with_state(token, require_token))
}

/// The UI listener's router: the token-gated API under `/v1`, and the UI's
/// static assets everywhere else.
pub(crate) fn ui_router(state: AdminState, token: Arc<AdminToken>, assets: &Path) -> Router {
    let api = Router::new()
        .nest("/v1", v1(state))
        .layer(middleware::from_fn_with_state(token, require_token));
    api.fallback_service(ui::service(assets))
}

/// The admin token check. The token is taken only from the `Authorization`
/// header, never from the query string. No CORS preflight is answered: an
/// `OPTIONS` carries no token and is refused like any other request.
async fn require_token(
    State(token): State<Arc<AdminToken>>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        });
    match presented {
        None => ApiError::unauthorized(
            "this listener requires the admin token as `Authorization: Bearer <token>`",
        )
        .into_response(),
        Some(t) if !token.verify(t.as_bytes()) => {
            ApiError::unauthorized("the admin token is not valid").into_response()
        }
        Some(_) => {
            req.extensions_mut().insert(Principal::Token);
            next.run(req).await
        }
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("not_found", "no such admin endpoint")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed on this endpoint",
    )
}
