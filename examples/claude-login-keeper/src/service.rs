//! The keeper as the `JanusPlugin` gRPC service (`docs/plugin-contract`).

use std::collections::HashMap;
use std::sync::Mutex;

use janus_core::contract::plugin::{event_type, NotifyEvent, ResolveRoute, RouteOutcome};
use janus_core::contract::proto::{self, janus_plugin_server::JanusPlugin};
use serde::Deserialize;
use tonic::{Request, Response, Status, Streaming};

use crate::keeper::Keeper;

/// `payload_json` of `credential_invalid` and `account_error`.
#[derive(Debug, Deserialize)]
struct CredentialProblem {
    credential_ref: String,
    upstream: String,
    status: u16,
    reason: String,
}

pub struct KeeperPlugin {
    keeper: Keeper,
    denies: DenyLog,
}

/// The deny reason last logged per instance, so the log records a change
/// of answer once, not every request.
#[derive(Default)]
pub struct DenyLog(Mutex<HashMap<String, String>>);

impl DenyLog {
    /// `true` when `reason` (`None`: not denied) differs from the last
    /// answer for `instance`.
    pub fn changed(&self, instance: &str, reason: Option<&str>) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match reason {
            Some(r) if last.get(instance).map(String::as_str) != Some(r) => {
                last.insert(instance.to_owned(), r.to_owned());
                true
            }
            None => last.remove(instance).is_some(),
            Some(_) => false,
        }
    }
}

impl KeeperPlugin {
    pub fn new(keeper: Keeper) -> Self {
        KeeperPlugin {
            keeper,
            denies: DenyLog::default(),
        }
    }

    pub fn resolve(&self, req: &ResolveRoute) -> RouteOutcome {
        let outcome = self.keeper.route(&req.upstream);
        let reason = match &outcome {
            RouteOutcome::Deny { reason } => Some(reason.as_str()),
            _ => None,
        };
        if self.denies.changed(&req.upstream, reason) {
            tracing::info!(event = "route_answer_changed", upstream = %req.upstream,
                outcome = if reason.is_some() { "deny_reason" } else { "use_default" },
                reason = reason.unwrap_or_default());
        } else {
            tracing::debug!(event = "resolve_route", request_id = %req.request_id,
                upstream = %req.upstream, denied = reason.is_some());
        }
        outcome
    }

    pub fn handle(&self, event: NotifyEvent) {
        match event.event_type.as_str() {
            kind @ (event_type::CREDENTIAL_INVALID | event_type::ACCOUNT_ERROR) => {
                let p = match event.payload::<CredentialProblem>() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(event = kind, error = %e, "undecodable payload_json");
                        return;
                    }
                };
                let invalid = kind == event_type::CREDENTIAL_INVALID;
                let managed = self.keeper.report(&p.credential_ref, invalid, &p.reason);
                tracing::warn!(event = kind, managed, credential_ref = %p.credential_ref,
                    upstream = %p.upstream, status = p.status, reason = %p.reason);
            }
            // Every other event (and any future one) is not the keeper's.
            // `call_completed` is among them: a success shows that an
            // access token served one request, and may predate a
            // `credential_invalid`. Only an admin check clears a recorded
            // `needs_login`.
            other => tracing::debug!(event = "ignored", event_type = other),
        }
    }
}

#[tonic::async_trait]
impl JanusPlugin for KeeperPlugin {
    async fn resolve_route(
        &self,
        request: Request<proto::ResolveRouteRequest>,
    ) -> Result<Response<proto::ResolveRouteResponse>, Status> {
        let outcome = match ResolveRoute::try_from(request.into_inner()) {
            Ok(req) => self.resolve(&req),
            Err(e) => RouteOutcome::Deny {
                reason: e.to_string(),
            },
        };
        Ok(Response::new(outcome.into()))
    }

    async fn notify(
        &self,
        request: Request<Streaming<proto::NotifyEvent>>,
    ) -> Result<Response<proto::NotifyAck>, Status> {
        let mut stream = request.into_inner();
        while let Some(event) = stream.message().await? {
            self.handle(event.into());
        }
        Ok(Response::new(proto::NotifyAck {}))
    }
}

#[cfg(test)]
mod tests {
    use super::DenyLog;

    #[test]
    fn a_deny_is_logged_once_per_change() {
        let log = DenyLog::default();
        assert!(
            !log.changed("claude", None),
            "use_default from the start is no change"
        );
        assert!(log.changed("claude", Some("open the page")));
        assert!(!log.changed("claude", Some("open the page")));
        assert!(log.changed("claude", Some("open https://claude.com/x")));
        assert!(log.changed("claude", None));
        assert!(!log.changed("claude", None));
    }
}
