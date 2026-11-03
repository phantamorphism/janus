//! Admin API errors: `{ "error": { "code": "<snake_case>", "message": "..." } }`
//! with a conventional HTTP status (`docs/admin-surface#principles`). `code`
//! is the stable contract; `message` is for humans and never carries
//! credential material.
//!
//! Also the request extractors, wrapped so a malformed body, query or path
//! is answered in the same JSON shape rather than axum's plain text.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use janus_core::contract::credential::PoolError;
use serde::de::DeserializeOwned;

use super::api::MAX_BODY_BYTES;
use crate::store::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub(crate) fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, code, message)
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    fn internal(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// From an axum extractor rejection: its status, and its text only when
    /// the rejection is about the request's shape.
    fn rejected(status: StatusCode, text: String) -> Self {
        match status {
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(status, "payload_too_large", text),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                ApiError::new(status, "unsupported_media_type", text)
            }
            // A body that parses but does not fit (axum's 422) is as much a
            // bad request as one that does not parse.
            s if s.is_client_error() => ApiError::bad_request(text),
            _ => ApiError::internal(text),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "code": self.code, "message": self.message }
        });
        let mut response = (self.status, axum::Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

impl From<PoolError> for ApiError {
    fn from(e: PoolError) -> Self {
        let message = e.to_string();
        let (status, code) = match &e {
            PoolError::InvalidRef(_) => (StatusCode::BAD_REQUEST, "invalid_ref"),
            PoolError::NotFound(_) => (StatusCode::NOT_FOUND, "credential_not_found"),
            PoolError::AlreadyExists(_) => (StatusCode::CONFLICT, "credential_exists"),
            PoolError::WrongUpstream { .. } => (StatusCode::CONFLICT, "wrong_upstream"),
            PoolError::Unavailable { .. } => (StatusCode::CONFLICT, "credential_unavailable"),
            PoolError::NoDefault(_) => (StatusCode::NOT_FOUND, "no_default_credential"),
            PoolError::RefreshFailed { .. } => (StatusCode::BAD_GATEWAY, "refresh_failed"),
            PoolError::InvalidMaterial { .. } => (StatusCode::BAD_REQUEST, "invalid_material"),
            PoolError::NotPoolManaged { .. } => (StatusCode::CONFLICT, "not_pool_managed"),
            PoolError::Io(_) => {
                tracing::error!(error = %e, "credential pool I/O failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        ApiError::new(status, code, message)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(message) => ApiError::bad_request(message),
            StoreError::NotFound(_) => ApiError::not_found("identity_not_found", e.to_string()),
            StoreError::Io { .. }
            | StoreError::Sqlite(_)
            | StoreError::Schema(_)
            | StoreError::Corrupt(_) => {
                tracing::error!(error = %e, "daemon database error");
                ApiError::internal(e.to_string())
            }
        }
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> Self {
        tracing::error!(error = %e, "admin blocking task failed");
        ApiError::internal("internal task failed")
    }
}

/// How much of an oversized body is still read, and discarded, before the
/// `413`. A client is typically still sending when the limit is crossed;
/// answering and closing at once leaves unread bytes behind, the kernel
/// resets the connection, and the client sees the reset instead of the
/// `413`. Past this much the connection is given up on (and closed).
const DRAIN_LIMIT: usize = 16 * MAX_BODY_BYTES;

/// A JSON body of at most [`MAX_BODY_BYTES`].
pub(crate) struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let (parts, body) = req.into_parts();
        let bytes = read_limited(&parts, body).await?;
        let req = Request::from_parts(parts, axum::body::Body::from(bytes));
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| Body(v))
            .map_err(|e| ApiError::rejected(e.status(), e.body_text()))
    }
}

/// The body, if it is at most [`MAX_BODY_BYTES`]; otherwise `413`, after
/// draining up to [`DRAIN_LIMIT`] so the client gets to read the answer.
async fn read_limited(parts: &Parts, mut body: axum::body::Body) -> Result<Vec<u8>, ApiError> {
    let too_large = || {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("the request body is larger than {MAX_BODY_BYTES} bytes"),
        )
    };
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > DRAIN_LIMIT as u64) {
        return Err(too_large());
    }
    let mut kept = Vec::new();
    let mut seen = 0usize;
    while let Some(frame) = body.frame().await {
        let frame = frame
            .map_err(|e| ApiError::bad_request(format!("cannot read the request body: {e}")))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        seen = seen.saturating_add(data.len());
        if seen > DRAIN_LIMIT {
            break;
        }
        if seen <= MAX_BODY_BYTES {
            kept.extend_from_slice(&data);
        }
    }
    if seen > MAX_BODY_BYTES {
        return Err(too_large());
    }
    Ok(kept)
}

/// A query string.
pub(crate) struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(v)| Query(v))
            .map_err(|e| ApiError::rejected(e.status(), e.body_text()))
    }
}

/// A path parameter.
pub(crate) struct PathParam(pub String);

impl<S: Send + Sync> FromRequestParts<S> for PathParam {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::Path::<String>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(v)| PathParam(v))
            .map_err(|e| ApiError::rejected(e.status(), e.body_text()))
    }
}
