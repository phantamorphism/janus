//! Steps 1–6 of `docs/architecture#request-lifecycle` for one request; step
//! 7 is [`super::settle`].
//!
//! The order is the security argument: identity is extracted from the head
//! alone and a rejection returns before the plugin, the pool or the body is
//! touched; only a verified identity reaches `ResolveRoute`, and only when a
//! plugin is configured (no plugin, or a `method = "none"` route, makes zero
//! plugin calls); every plugin failure fails the request closed; a
//! credential that cannot be had fails the request rather than falling
//! through to another. The caller's body is handed to the forwarder unread.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use janus_core::contract::credential::CredentialChoice;
use janus_core::contract::identity::{RejectionKind, RequestHead};
use janus_core::contract::plugin::{ResolveRoute, RouteOutcome};
use janus_core::contract::request_log::{AbandonStage, Outcome};
use janus_core::contract::upstream::UpstreamError;
use janus_core::proxy::{build_outbound, ForwardError, Substitution, SubstitutionError};

use super::settle::Settlement;
use super::{reply, DataPlane};

/// The data plane's one handler: every path is a proxy path or a 404.
pub(super) async fn proxy(State(dp): State<Arc<DataPlane>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let mut settle = Settlement::begin(&dp, &parts.method, &path);

    // Step 1: which upstream. Unrouted paths (admin paths included) are
    // 404 and recorded with no upstream and no binding.
    let Some((upstream, stripped)) = dp.route(&path) else {
        settle.abandon(
            Outcome::RejectedIdentity {
                kind: RejectionKind::NoMaterial,
            },
            "no upstream is routed at this path".to_owned(),
        );
        return reply::not_routed();
    };
    settle.upstream(upstream.kind);

    // Step 2: identity, from the head only.
    let selected = match dp.bindings.select(upstream.kind, stripped) {
        Ok(selected) => selected,
        Err(_) => {
            settle.abandon(
                Outcome::RejectedIdentity {
                    kind: RejectionKind::NoMaterial,
                },
                "no identity binding applies to this path".to_owned(),
            );
            return reply::identity_rejected(RejectionKind::NoMaterial);
        }
    };
    settle.binding(selected.route, selected.binding.method());
    let rest = match parts.uri.query() {
        Some(query) => format!("{stripped}?{query}"),
        None => stripped.to_owned(),
    };
    let head = RequestHead::new(parts.method, parts.uri, parts.headers);
    let identity = match selected.binding.identify(&head).await {
        Ok(identity) => identity,
        Err(rejection) => {
            // The body is dropped unread with the request.
            let kind = rejection.error.kind();
            settle.claimed(rejection.claimed_identity);
            settle.abandon(
                Outcome::RejectedIdentity { kind },
                rejection.error.to_string(),
            );
            return reply::identity_rejected(kind);
        }
    };
    if let Some(identity) = &identity {
        settle.identified(
            &identity.caller_identity,
            identity.correlation_id.as_deref(),
        );
    }

    // Step 3: the plugin, only for a verified identity and only if one is
    // configured.
    let choice = match (&identity, &dp.plugin) {
        (Some(identity), Some(plugin)) => {
            let request = ResolveRoute {
                request_id: settle.request_id(),
                upstream: upstream.kind.as_str().to_owned(),
                caller_identity: identity.caller_identity.clone(),
                correlation_id: identity.correlation_id.clone(),
                identity_claims: identity.claims.clone(),
            };
            settle.resolve_sent();
            match plugin.resolve_route(request).await {
                Ok(RouteOutcome::Credential(credential_ref)) => {
                    CredentialChoice::Ref(credential_ref)
                }
                Ok(RouteOutcome::UseDefault) => CredentialChoice::Default,
                Ok(RouteOutcome::Deny { reason }) => {
                    let response = reply::denied(&reason);
                    settle.abandon(
                        Outcome::Denied {
                            reason: reason.clone(),
                        },
                        format!("denied by the plugin: {reason}"),
                    );
                    return response;
                }
                Err(e) => {
                    settle.abandon(Outcome::PluginFailed { class: e.class }, e.to_string());
                    return reply::plugin_failed(e.class);
                }
            }
        }
        _ => CredentialChoice::Default,
    };

    // Step 4: the credential.
    settle.stage(AbandonStage::Credential);
    let material = match dp.pool.resolve(upstream.kind, &choice).await {
        Ok(material) => material,
        Err(e) => {
            settle.abandon(Outcome::CredentialUnavailable, e.to_string());
            return reply::credential_unavailable();
        }
    };
    settle.credential(&material.credential_ref);

    // Step 5: the outbound request.
    settle.stage(AbandonStage::Rewrite);
    let implementation = Arc::clone(dp.upstreams.get(upstream.kind));
    let outbound = build_outbound(
        &head,
        Substitution {
            upstream: implementation.as_ref(),
            base_url: &upstream.base_url,
            rest: &rest,
            material: &material,
            presented_at: identity.as_ref().map(|i| &i.presented_at),
        },
    );
    let outbound = match outbound {
        Ok(outbound) => outbound,
        Err(e) => {
            let (reason, response) = rewrite_failure(&e);
            settle.abandon(
                Outcome::Abandoned {
                    stage: AbandonStage::Rewrite,
                },
                reason,
            );
            return response;
        }
    };
    let credential_ref = material.credential_ref.clone();
    drop(material);

    // Step 6: forward, relay.
    settle.stage(AbandonStage::Forward);
    match dp.forwarder.forward(implementation, outbound, body).await {
        Ok(relayed) => {
            let status = relayed.response.status();
            settle.relayed(upstream.kind, credential_ref, status, relayed.outcome);
            relayed.response.map(Body::new)
        }
        Err(ForwardError::Upstream(e)) => {
            // The cause chain carries no URL, query or credential.
            tracing::warn!(upstream = %upstream.path_prefix, error = %e, "upstream request failed");
            settle.abandon(
                Outcome::UpstreamError,
                format!("upstream request failed: {e}"),
            );
            reply::upstream_error()
        }
        Err(e @ ForwardError::BadTarget(_)) => {
            settle.abandon(
                Outcome::Abandoned {
                    stage: AbandonStage::Forward,
                },
                e.to_string(),
            );
            reply::rewrite_failed()
        }
    }
}

/// The record's diagnostic and the caller's response for a refused
/// substitution. A credential that cannot be applied is described without
/// the upstream's detail, which may quote the material.
fn rewrite_failure(e: &SubstitutionError) -> (String, Response) {
    match e {
        SubstitutionError::Upstream(UpstreamError::BadPath(_)) => {
            (e.to_string(), reply::bad_path())
        }
        SubstitutionError::IdentityLeak { .. } => (e.to_string(), reply::rewrite_failed()),
        SubstitutionError::Upstream(
            UpstreamError::Auth(_)
            | UpstreamError::InvalidMaterial(_)
            | UpstreamError::NotRefreshable(_)
            | UpstreamError::RefreshFailed(_),
        ) => (
            "the credential cannot be applied to the request".to_owned(),
            reply::rewrite_failed(),
        ),
    }
}
