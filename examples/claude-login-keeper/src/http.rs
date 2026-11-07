//! The loopback page: `GET /` (state and authorize URL), `GET /status`
//! (the same as JSON), `POST /login/start`, `POST /login/cancel`,
//! `POST /login/code`. No authentication: it binds only to loopback, every
//! request must name the bind address (or `localhost:<port>`) in `Host`, so
//! a DNS-rebound name is refused, and a `POST` from a browser page of
//! another origin is refused. Every answer forbids framing, sniffing and
//! caching.

use std::future::Future;

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::keeper::{Health, Keeper, StartError};
use crate::login::LoginState;

pub fn router(keeper: Keeper) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/status", get(status))
        .route("/login/start", post(start))
        .route("/login/cancel", post(cancel))
        .route("/login/code", post(code))
        .layer(middleware::from_fn_with_state(keeper.clone(), guard))
        .with_state(keeper)
}

/// The addresses this page answers as: `Host` values and, with `http://`,
/// `Origin` values.
fn own_names(keeper: &Keeper) -> [String; 2] {
    let bind = keeper.config().http_bind;
    [bind.to_string(), format!("localhost:{}", bind.port())]
}

/// Refuse a foreign `Host`, and set the security headers on every answer.
async fn guard(State(keeper): State<Keeper>, request: Request, next: Next) -> Response {
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| own_names(&keeper).iter().any(|n| n == h));
    let mut response = if host_ok {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "unexpected Host header\n").into_response()
    };
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Serve the page on `listener` until `shutdown` resolves.
pub async fn serve(
    keeper: Keeper,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(keeper))
        .with_graceful_shutdown(shutdown)
        .await
}

#[derive(Deserialize)]
struct StartForm {
    credential: Option<String>,
}

#[derive(Deserialize)]
struct CancelForm {
    credential: Option<String>,
}

#[derive(Deserialize)]
struct CodeForm {
    credential: Option<String>,
    code: String,
}

/// The credential a form names; with one configured credential the field
/// may be left out.
fn named(keeper: &Keeper, credential: Option<String>) -> Option<String> {
    credential
        .filter(|c| !c.is_empty())
        .or_else(|| match keeper.config().credentials.as_slice() {
            [only] => Some(only.credential_ref.clone()),
            _ => None,
        })
}

fn credential_required(headers: &HeaderMap) -> Response {
    answer(
        headers,
        StatusCode::BAD_REQUEST,
        "bad_request",
        "credential is required".into(),
        None,
        None,
    )
}

/// No attempt runs for `credential`; names the one that runs, if any.
fn no_flow(keeper: &Keeper, headers: &HeaderMap, credential: &str) -> Response {
    let running = keeper.running_credential();
    answer(
        headers,
        StatusCode::CONFLICT,
        "no_flow",
        format!("no login flow is running for {credential}"),
        running.as_deref(),
        None,
    )
}

/// A browser sends `Origin` on a cross-site `POST`; it must be this page.
/// `curl` and the CLI send none.
fn same_origin(keeper: &Keeper, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    own_names(keeper)
        .iter()
        .any(|n| origin.as_bytes() == format!("http://{n}").as_bytes())
}

fn refused() -> Response {
    (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response()
}

/// A client that accepts JSON (the CLI) gets `{"ok": message, "flow_id"}`
/// or the admin API's error envelope with a `code` (and the
/// `credential_ref` and `flow_id` it is about). A form post from the page
/// redirects back to it on success. Anything else gets plain text.
fn answer(
    headers: &HeaderMap,
    status: StatusCode,
    code: &str,
    text: String,
    credential_ref: Option<&str>,
    flow_id: Option<u64>,
) -> Response {
    let wants_json = headers
        .get(header::ACCEPT)
        .is_some_and(|v| v.to_str().is_ok_and(|a| a.contains("application/json")));
    if wants_json {
        let body = if status.is_success() {
            serde_json::json!({ "ok": text, "flow_id": flow_id })
        } else {
            serde_json::json!({ "error": {
                "code": code, "message": text, "credential_ref": credential_ref,
                "flow_id": flow_id,
            }})
        };
        return (status, Json(body)).into_response();
    }
    let from_form = headers.get(header::CONTENT_TYPE).is_some_and(|v| {
        v.as_bytes()
            .starts_with(b"application/x-www-form-urlencoded")
    });
    if from_form && status.is_success() {
        Redirect::to("/").into_response()
    } else {
        (status, format!("{text}\n")).into_response()
    }
}

async fn status(State(keeper): State<Keeper>) -> impl IntoResponse {
    Json(keeper.snapshot())
}

async fn start(
    State(keeper): State<Keeper>,
    headers: HeaderMap,
    form: Result<Form<StartForm>, FormRejection>,
) -> Response {
    if !same_origin(&keeper, &headers) {
        return refused();
    }
    let Some(credential) = named(&keeper, form.ok().and_then(|Form(f)| f.credential)) else {
        return credential_required(&headers);
    };
    match keeper.start(&credential) {
        Ok(id) => answer(
            &headers,
            StatusCode::ACCEPTED,
            "",
            format!("login flow {id} for {credential} starting"),
            Some(&credential),
            Some(id),
        ),
        Err(e @ StartError::Unknown(_)) => answer(
            &headers,
            StatusCode::NOT_FOUND,
            "unknown_credential",
            e.to_string(),
            Some(&credential),
            None,
        ),
        Err(
            ref e @ StartError::Busy {
                ref credential_ref,
                flow_id,
            },
        ) => answer(
            &headers,
            StatusCode::CONFLICT,
            "busy",
            e.to_string(),
            Some(credential_ref),
            flow_id,
        ),
        Err(e @ StartError::ShuttingDown) => answer(
            &headers,
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            e.to_string(),
            None,
            None,
        ),
    }
}

async fn cancel(
    State(keeper): State<Keeper>,
    headers: HeaderMap,
    form: Result<Form<CancelForm>, FormRejection>,
) -> Response {
    if !same_origin(&keeper, &headers) {
        return refused();
    }
    let Some(credential) = named(&keeper, form.ok().and_then(|Form(f)| f.credential)) else {
        return credential_required(&headers);
    };
    if keeper.cancel(&credential) {
        answer(
            &headers,
            StatusCode::OK,
            "",
            format!("cancelling the login flow for {credential}"),
            Some(&credential),
            None,
        )
    } else {
        no_flow(&keeper, &headers, &credential)
    }
}

async fn code(
    State(keeper): State<Keeper>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    if !same_origin(&keeper, &headers) {
        return refused();
    }
    if form.code.trim().is_empty() {
        return answer(
            &headers,
            StatusCode::BAD_REQUEST,
            "bad_request",
            "code is empty".into(),
            None,
            None,
        );
    }
    let Some(credential) = named(&keeper, form.credential) else {
        return credential_required(&headers);
    };
    if keeper.submit_code(&credential, &form.code) {
        answer(
            &headers,
            StatusCode::ACCEPTED,
            "",
            format!("code passed to the login flow for {credential}"),
            Some(&credential),
            None,
        )
    } else {
        no_flow(&keeper, &headers, &credential)
    }
}

async fn page(State(keeper): State<Keeper>) -> Html<String> {
    let snap = keeper.snapshot();
    let mut rows = String::new();
    for c in &snap.credentials {
        let health = match &c.health {
            Health::Unknown => "unknown".to_owned(),
            Health::Healthy => "healthy".to_owned(),
            Health::Disabled => "disabled by an operator".to_owned(),
            Health::RefreshRefused { reason, expires_at } => format!(
                "refresh refused, serving until {}: {}",
                expires_at.to_rfc3339(),
                escape(reason)
            ),
            Health::NeedsLogin { reason } => format!("needs a login: {}", escape(reason)),
        };
        let next = c
            .next_flow_in_secs
            .map_or(String::new(), |s| format!(" (next flow in {s}s)"));
        rows.push_str(&format!(
            "<tr><td>{r}</td><td>{inst}</td><td>{health}{next}</td><td>{f}</td>\
             <td><form method=post action=/login/start><input type=hidden name=credential value=\"{r}\">\
             <button>Start login</button></form></td></tr>\n",
            r = escape(&c.credential_ref),
            inst = escape(&c.instances.join(", ")),
            f = c.failed_flows,
        ));
    }
    let flow = match &snap.flow {
        None => "<p>No login flow has run.</p>".to_owned(),
        Some(f) => {
            let r = escape(&f.credential_ref);
            match &f.state {
                LoginState::AwaitingBrowser {
                    auth_url,
                    local_url,
                    notice,
                } => {
                    let local = local_url.as_ref().map_or(String::new(), |u| {
                        format!(
                            "<p>From a browser on <em>this</em> host, this link completes by itself: \
                             <a href=\"{u}\">local sign-in</a></p>",
                            u = escape(u)
                        )
                    });
                    let notice = notice.as_ref().map_or(String::new(), |n| {
                        format!("<p><strong>{}</strong></p>", escape(n))
                    });
                    format!(
                        "<p>Login for <b>{r}</b> is waiting for authorization.</p>\
                         <p>1. Open <a href=\"{u}\" target=_blank>{u}</a> and sign in.<br>\
                         2. Paste the code the page shows:</p>\
                         <form method=post action=/login/code><input type=hidden name=credential value=\"{r}\">\
                         <input name=code size=60 autocomplete=off>\
                         <button>Submit code</button></form>{local}{notice}\
                         <form method=post action=/login/cancel><input type=hidden name=credential value=\"{r}\">\
                         <button>Cancel</button></form>",
                        u = escape(auth_url),
                    )
                }
                state if state.is_running() => format!(
                    "<p>Login for <b>{r}</b>: {}</p><form method=post action=/login/cancel>\
                     <input type=hidden name=credential value=\"{r}\"><button>Cancel</button></form>",
                    state_name(state)
                ),
                LoginState::Failed { reason } => {
                    format!(
                        "<p>Last login for <b>{r}</b> failed: {}</p>",
                        escape(reason)
                    )
                }
                state => format!("<p>Last login for <b>{r}</b>: {}</p>", state_name(state)),
            }
        }
    };
    // Reloading would clear a code being typed; refresh only without one.
    let typing =
        matches!(&snap.flow, Some(f) if matches!(f.state, LoginState::AwaitingBrowser { .. }));
    let refresh = if typing {
        ""
    } else {
        "<meta http-equiv=refresh content=5>"
    };
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>claude-login-keeper</title>\
         {refresh}</head><body>\
         <h1>claude-login-keeper</h1>{flow}\
         <table border=1 cellpadding=4><tr><th>credential</th><th>instances</th><th>health</th>\
         <th>failed flows</th><th></th></tr>\n{rows}</table></body></html>\n"
    ))
}

fn state_name(state: &LoginState) -> String {
    match state {
        LoginState::Idle => "idle".into(),
        LoginState::Starting => "starting".into(),
        LoginState::AwaitingUrl => "waiting for the authorize URL".into(),
        LoginState::AwaitingBrowser { .. } => "waiting for authorization".into(),
        LoginState::Succeeded { at } => format!("succeeded at {}", at.to_rfc3339()),
        LoginState::Failed { reason } => format!("failed: {}", escape(reason)),
        LoginState::Cancelled { reason } => format!("cancelled: {}", escape(reason)),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
