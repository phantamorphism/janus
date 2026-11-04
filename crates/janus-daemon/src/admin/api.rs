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
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::error::ApiError;
use super::status::{Listeners, UpstreamSummary};
use super::token::AdminToken;
use super::{credentials, identities, requests, status, ui, AdminDeps};

/// Who made an admin call, as the audit log names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Principal {
    /// The unix socket: filesystem permissions authenticated the caller.
    Socket,
    /// The TCP admin listener: the admin token did.
    AdminTcp,
    /// The UI listener: the admin token did.
    Ui,
}

impl Principal {
    fn as_str(self) -> &'static str {
        match self {
            Principal::Socket => "socket",
            Principal::AdminTcp => "admin_tcp",
            Principal::Ui => "ui",
        }
    }
}

pub(crate) struct Inner {
    pub deps: AdminDeps,
    pub listeners: Listeners,
    pub upstreams: Vec<UpstreamSummary>,
    /// Daemon shutdown: ends live tails.
    pub cancel: CancellationToken,
    /// One permit per open live tail, across every listener.
    pub tails: Arc<Semaphore>,
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
            tails: Arc::new(Semaphore::new(requests::MAX_TAILS)),
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

/// The largest request body the API accepts (enforced by the `Body`
/// extractor). The biggest body it takes is a credential's material
/// document, a few kilobytes at most.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

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
    api(state).layer(middleware::from_fn_with_state(
        (token, Principal::AdminTcp),
        require_token,
    ))
}

/// The UI listener's router: the token-gated API under `/v1`, and the UI's
/// static assets everywhere else. Every path under `/v1` is the API's —
/// one no route matches (`/v1/`, a trailing slash) is the API's token-gated
/// `404`, never the single-page app's `index.html`.
pub(crate) fn ui_router(state: AdminState, token: Arc<AdminToken>, assets: &Path) -> Router {
    let assets = ui::service(assets);
    Router::new()
        .nest("/v1", v1(state))
        .fallback(move |req: Request| {
            let assets = assets.clone();
            async move {
                if is_api_path(req.uri().path()) {
                    not_found().await.into_response()
                } else {
                    assets.oneshot(req).await.into_response()
                }
            }
        })
        .layer(middleware::from_fn_with_state(
            (token, Principal::Ui),
            require_token_on_api,
        ))
        .layer(middleware::from_fn(hardening_headers))
}

/// The browser-hardening headers on every response of the UI listener, the
/// token-gated API and its error responses included (`docs/admin-surface#ui`).
/// The built UI loads only same-origin scripts, styles and fetches, so
/// `default-src 'self'` is enough; `frame-ancestors` is not covered by
/// `default-src` and is set explicitly.
async fn hardening_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    res
}

/// `/v1` and everything below it.
fn is_api_path(path: &str) -> bool {
    path.strip_prefix("/v1")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// [`require_token`] on API paths; the UI's assets are public.
async fn require_token_on_api(State(gate): State<TokenGate>, req: Request, next: Next) -> Response {
    if is_api_path(req.uri().path()) {
        require_token(State(gate), req, next).await
    } else {
        next.run(req).await
    }
}

/// The admin token and the principal a request that presents it is audited as.
type TokenGate = (Arc<AdminToken>, Principal);

/// The admin token check. The token is taken only from the `Authorization`
/// header, never from the query string. No CORS preflight is answered: an
/// `OPTIONS` carries no token and is refused like any other request.
async fn require_token(
    State((token, principal)): State<TokenGate>,
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
            req.extensions_mut().insert(principal);
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
