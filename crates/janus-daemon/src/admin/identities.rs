//! `/v1/identities` (`docs/admin-surface#identities-endpoint`): the admin
//! half of the `opaque` method. Entries are addressed by their
//! server-assigned `id`; the token exists only in the create response.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use janus_core::contract::registry::IdentityRecord;
use serde::{Deserialize, Serialize};

use super::api::{audit, blocking, AdminState, Principal};
use super::error::{ApiError, Body, PathParam, Query};
use crate::store::{Expiry, IdentityFilter, NewIdentity};

/// The body of `POST /v1/identities`. Unknown fields are refused, so a
/// provisioner cannot supply its own `token`: the daemon generates it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateBody {
    identity: String,
    #[serde(default)]
    claims: BTreeMap<String, String>,
    label: Option<String>,
    /// A duration such as `"90s"` or `"30d"`.
    #[serde(default, with = "humantime_serde")]
    expires_in: Option<Duration>,
    expires_at: Option<DateTime<Utc>>,
}

/// The create response: the entry plus its token, this once.
#[derive(Serialize)]
pub(crate) struct Created {
    #[serde(flatten)]
    record: IdentityRecord,
    token: String,
}

pub(crate) async fn create(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    Body(body): Body<CreateBody>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let expiry = match (body.expires_in, body.expires_at) {
        (Some(_), Some(_)) => {
            let e = Err(ApiError::bad_request(
                "set at most one of `expires_in` and `expires_at`",
            ));
            audit(principal, "identity.create", "identity/<new>", &e);
            return e;
        }
        (Some(d), None) => Some(Expiry::In(d)),
        (None, Some(at)) => Some(Expiry::At(at)),
        (None, None) => None,
    };
    let new = NewIdentity {
        identity: body.identity,
        claims: body.claims,
        label: body.label,
        expiry,
    };
    let registry = state.deps.registry.clone();
    let result = blocking(move || registry.create(new, Utc::now())).await;
    let resource = match &result {
        Ok(created) => format!(
            "identity/{} ({})",
            created.record.id, created.record.identity
        ),
        Err(_) => "identity/<new>".to_owned(),
    };
    audit(principal, "identity.create", &resource, &result);
    let created = result?;
    Ok((
        StatusCode::CREATED,
        Json(Created {
            record: created.record,
            token: created.token,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    identity: Option<String>,
    #[serde(default)]
    include_revoked: bool,
    #[serde(default)]
    include_expired: bool,
}

#[derive(Serialize)]
pub(crate) struct IdentityList {
    identities: Vec<IdentityRecord>,
}

pub(crate) async fn list(
    State(state): State<AdminState>,
    Query(params): Query<ListParams>,
) -> Result<Json<IdentityList>, ApiError> {
    let filter = IdentityFilter {
        identity: params.identity,
        include_revoked: params.include_revoked,
        include_expired: params.include_expired,
    };
    let registry = state.deps.registry.clone();
    let identities =
        blocking(move || Ok::<_, ApiError>(registry.list(&filter, Utc::now()))).await?;
    Ok(Json(IdentityList { identities }))
}

pub(crate) async fn show(
    State(state): State<AdminState>,
    PathParam(id): PathParam,
) -> Result<Json<IdentityRecord>, ApiError> {
    let registry = state.deps.registry.clone();
    let key = id.clone();
    blocking(move || Ok::<_, ApiError>(registry.get(&key)))
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found("identity_not_found", format!("no identity with id {id:?}"))
        })
}

/// Immediate and idempotent: the next lookup of the token is rejected.
pub(crate) async fn revoke(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(id): PathParam,
) -> Result<Json<IdentityRecord>, ApiError> {
    let registry = state.deps.registry.clone();
    let key = id.clone();
    let result = blocking(move || registry.revoke(&key, Utc::now())).await;
    audit(
        principal,
        "identity.revoke",
        &format!("identity/{id}"),
        &result,
    );
    result.map(Json)
}

pub(crate) async fn delete(
    State(state): State<AdminState>,
    Extension(principal): Extension<Principal>,
    PathParam(id): PathParam,
) -> Result<StatusCode, ApiError> {
    let registry = state.deps.registry.clone();
    let key = id.clone();
    let result = blocking(move || registry.delete(&key)).await;
    audit(
        principal,
        "identity.delete",
        &format!("identity/{id}"),
        &result,
    );
    result.map(|()| StatusCode::NO_CONTENT)
}
