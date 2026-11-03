//! `/v1/credentials` (`docs/admin-surface#credentials-endpoint`). Every
//! response is [`CredentialMetadata`]: the pool's metadata type has no field
//! that can hold material, so nothing here can return it. Material arrives
//! only in request bodies, as a [`MaterialDocument`], which never formats
//! its contents.

use std::future::Future;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use janus_core::contract::credential::{
    CredentialKind, CredentialMetadata, MaterialDocument, NewCredential, PoolError,
};
use janus_core::contract::upstream::UpstreamKind;
use serde::{Deserialize, Serialize};

use super::api::{audit, AdminState, Principal};
use super::error::{ApiError, Body, PathParam, Query};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    upstream: Option<UpstreamKind>,
}

#[derive(Serialize)]
pub(crate) struct CredentialList {
    credentials: Vec<CredentialMetadata>,
}

pub(crate) async fn list(
    State(state): State<AdminState>,
    Query(params): Query<ListParams>,
) -> Result<Json<CredentialList>, ApiError> {
    let credentials = state.deps.pool.list(params.upstream).await?;
    Ok(Json(CredentialList { credentials }))
}

pub(crate) async fn show(
    State(state): State<AdminState>,
    PathParam(credential_ref): PathParam,
) -> Result<Json<CredentialMetadata>, ApiError> {
    Ok(Json(state.deps.pool.show(&credential_ref).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultParams {
    upstream: Option<UpstreamKind>,
}

/// The credential the default chain would pick now, without touching its
/// material.
pub(crate) async fn peek_default(
    State(state): State<AdminState>,
    Query(params): Query<DefaultParams>,
) -> Result<Json<CredentialMetadata>, ApiError> {
    let upstream = params
        .upstream
        .ok_or_else(|| ApiError::bad_request("`upstream` is required"))?;
    Ok(Json(state.deps.pool.peek_default(upstream).await?))
}

pub(crate) async fn add(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    Body(new): Body<NewCredential>,
) -> Result<(StatusCode, Json<CredentialMetadata>), ApiError> {
    let requested = new.credential_ref.clone();
    let upstream = new.upstream;
    let result = state.deps.pool.add(new).await.map_err(ApiError::from);
    let resource = match (&result, &requested) {
        (Ok(meta), _) => format!("credential/{}", meta.credential_ref),
        (Err(_), Some(r)) => format!("credential/{r}"),
        (Err(_), None) => format!("credential/<new {upstream}>"),
    };
    audit(principal, "credential.add", &resource, &result);
    result.map(|meta| (StatusCode::CREATED, Json(meta)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterialBody {
    material: MaterialDocument,
}

/// Rotation: replace the material and clear an `invalid` status.
pub(crate) async fn replace_material(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(credential_ref): PathParam,
    Body(body): Body<MaterialBody>,
) -> Result<Json<CredentialMetadata>, ApiError> {
    let pool = &state.deps.pool;
    let change = pool.replace_material(&credential_ref, body.material);
    audited(
        principal,
        "credential.replace_material",
        &credential_ref,
        change,
    )
    .await
    .map(Json)
}

pub(crate) async fn enable(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(credential_ref): PathParam,
) -> Result<Json<CredentialMetadata>, ApiError> {
    let change = state.deps.pool.enable(&credential_ref);
    audited(principal, "credential.enable", &credential_ref, change)
        .await
        .map(Json)
}

pub(crate) async fn disable(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(credential_ref): PathParam,
) -> Result<Json<CredentialMetadata>, ApiError> {
    let change = state.deps.pool.disable(&credential_ref);
    audited(principal, "credential.disable", &credential_ref, change)
        .await
        .map(Json)
}

/// Force a refresh now rather than on next use. An API key has nothing to
/// refresh: that is the caller's mistake (`409 not_refreshable`), told apart
/// here from a refresh that failed at the token endpoint (`502`).
pub(crate) async fn refresh(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(credential_ref): PathParam,
) -> Result<Json<CredentialMetadata>, ApiError> {
    let pool = &state.deps.pool;
    let change = async {
        let meta = pool.show(&credential_ref).await?;
        if meta.kind == CredentialKind::ApiKey {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "not_refreshable",
                format!("credential {credential_ref:?} is an API key; there is nothing to refresh"),
            ));
        }
        Ok(pool.refresh(&credential_ref).await?)
    };
    let result = change.await;
    audit(
        principal,
        "credential.refresh",
        &format!("credential/{credential_ref}"),
        &result,
    );
    result.map(Json)
}

/// Remove a pool slot.
pub(crate) async fn delete(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(credential_ref): PathParam,
) -> Result<StatusCode, ApiError> {
    let change = state.deps.pool.delete(&credential_ref);
    audited(principal, "credential.delete", &credential_ref, change)
        .await
        .map(|()| StatusCode::NO_CONTENT)
}

/// Run one pool mutation and audit it against `credential/<ref>`.
async fn audited<T>(
    principal: Principal,
    action: &'static str,
    credential_ref: &str,
    change: impl Future<Output = Result<T, PoolError>>,
) -> Result<T, ApiError> {
    let result = change.await.map_err(ApiError::from);
    audit(
        principal,
        action,
        &format!("credential/{credential_ref}"),
        &result,
    );
    result
}
