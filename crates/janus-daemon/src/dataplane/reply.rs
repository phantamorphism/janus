//! The responses the data plane itself produces, for requests it does not
//! forward. A JSON error object in the shape the launch upstreams' SDKs
//! already parse (`{"type":"error","error":{"type","message"}}`), plus a
//! stable `code`. Messages name what failed, never which credential was
//! selected or any material (`docs/credential-substitution`: the caller is
//! never told which credential was used).

use axum::body::Body;
use axum::response::Response;
use http::{header, HeaderValue, StatusCode};
use janus_core::contract::identity::RejectionKind;
use janus_core::contract::plugin::PluginFailureClass;
use serde_json::json;

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "type": "error",
        "error": { "type": "janus_error", "code": code, "message": message },
    });
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// The path matches no upstream's `path_prefix` (this includes every admin
/// path: the data plane mounts no admin routes).
pub(super) fn not_routed() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        "no upstream is routed at this path",
    )
}

/// Identity extraction failed, or no binding applies.
pub(super) fn identity_rejected(kind: RejectionKind) -> Response {
    error(
        StatusCode::UNAUTHORIZED,
        kind.as_str(),
        &format!("caller identity rejected: {}", kind.as_str()),
    )
}

/// The plugin denied the request; its reason reaches the caller.
pub(super) fn denied(reason: &str) -> Response {
    error(StatusCode::FORBIDDEN, "denied", reason)
}

/// `ResolveRoute` failed: fail closed.
pub(super) fn plugin_failed(class: PluginFailureClass) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        class.as_str(),
        &format!("the policy plugin could not be consulted ({class})"),
    )
}

pub(super) fn credential_unavailable() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "credential_unavailable",
        "no usable upstream credential for this request",
    )
}

/// The request path cannot be rewritten onto the upstream.
pub(super) fn bad_path() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "bad_path",
        "the request path cannot be forwarded to this upstream",
    )
}

/// Substitution refused to build the outbound request.
pub(super) fn rewrite_failed() -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "rewrite_failed",
        "the request could not be prepared for the upstream; see the daemon's request log",
    )
}

pub(super) fn upstream_error() -> Response {
    error(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        "the upstream could not be reached",
    )
}
