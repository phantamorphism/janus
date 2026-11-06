//! The plugin: the policy plus a request budget metered in two phases, served
//! as the `JanusPlugin` gRPC service (`docs/plugin-contract`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use janus_core::contract::plugin::{
    event_type, CallAbandoned, CallCompleted, NotifyEvent, ResolveRoute, RouteOutcome,
};
use janus_core::contract::proto::{self, janus_plugin_server::JanusPlugin};
use janus_core::metering::{Charge, MemoryUsageStore, Meter, Quota, UsageError, Window};
use serde::Deserialize;
use tonic::{Request, Response, Status, Streaming};

use crate::policy::{Policy, Route};

/// The meter every admitted request is charged one unit of.
const REQUESTS: &str = "requests";

pub struct TenantPinning {
    policy: Policy,
    meter: Meter<MemoryUsageStore>,
    /// Tokens observed in `call_completed` usage, per identity, since start.
    tokens: Mutex<HashMap<String, u64>>,
}

/// `payload_json` of `credential_invalid` and `account_error`.
#[derive(Debug, Deserialize)]
struct CredentialProblem {
    credential_ref: String,
    upstream: String,
    status: u16,
    reason: String,
}

impl TenantPinning {
    pub fn new(policy: Policy) -> Result<Self, UsageError> {
        let quota = Quota {
            limit: policy.budget.requests,
            window: Window::Fixed {
                period: Duration::from_secs(policy.budget.window_secs),
            },
        };
        let meter = Meter::new(MemoryUsageStore::new(), vec![(REQUESTS.to_owned(), quota)])?;
        Ok(Self {
            policy,
            meter,
            tokens: Mutex::default(),
        })
    }

    /// Phase 1: the policy decides, then an admitted request is charged.
    /// A denied request is never charged, so it needs no refund.
    pub async fn resolve(&self, req: &ResolveRoute) -> RouteOutcome {
        let id = req.caller_identity.as_str();
        let route = match self.policy.decide(id, &req.upstream, &req.identity_claims) {
            Ok(route) => route,
            Err(reason) => return deny(req, reason),
        };
        match self.meter.charge(&req.request_id, id, REQUESTS, 1).await {
            Ok(Charge::Allowed { used, limit }) => {
                let (outcome, answer, credential_ref) = match route {
                    Route::Pin(r) => (RouteOutcome::Credential(r.clone()), "credential_ref", r),
                    Route::Default => (RouteOutcome::UseDefault, "use_default", String::new()),
                };
                tracing::info!(event = "resolve_route", request_id = %req.request_id,
                    caller_identity = id, upstream = %req.upstream, outcome = answer,
                    credential_ref, budget_used = used, budget_limit = limit);
                outcome
            }
            Ok(Charge::Exceeded { used, limit }) => deny(
                req,
                format!(
                    "request budget exhausted: {used}/{limit} requests in the current {}s window",
                    self.policy.budget.window_secs
                ),
            ),
            Err(e) => deny(req, format!("request budget unavailable: {e}")),
        }
    }

    /// Phase 2: settle on `call_completed`, refund on `call_abandoned`; log
    /// the credential events; ignore every other event type.
    pub async fn handle(&self, event: NotifyEvent) {
        let request_id = event.request_id.clone().unwrap_or_default();
        match event.event_type.as_str() {
            event_type::CALL_COMPLETED => {
                let Some(p) = decode::<CallCompleted>(&event) else {
                    return;
                };
                let call_tokens = observed_tokens(&p.usage);
                let total = self.add_tokens(&p.caller_identity, call_tokens);
                let settled = self
                    .meter
                    .settle(&request_id, &p.caller_identity, REQUESTS)
                    .await;
                tracing::info!(event = "call_completed", request_id, caller_identity = %p.caller_identity,
                    upstream = %p.upstream, status = p.status, credential_ref = %p.credential_ref,
                    duration_ms = p.duration_ms, tokens = call_tokens, tokens_total = total,
                    failed_over = p.attempts.len(), settled = settled.is_ok());
            }
            event_type::CALL_ABANDONED => {
                let Some(p) = decode::<CallAbandoned>(&event) else {
                    return;
                };
                // `false` for a request that was denied, and so never charged.
                let refunded = self
                    .meter
                    .refund(&request_id, &p.caller_identity, REQUESTS)
                    .await;
                tracing::info!(event = "call_abandoned", request_id, caller_identity = %p.caller_identity,
                    stage = p.stage.as_str(), reason = %p.reason,
                    refunded = refunded.unwrap_or(false));
            }
            kind @ (event_type::CREDENTIAL_INVALID | event_type::ACCOUNT_ERROR) => {
                let Some(p) = decode::<CredentialProblem>(&event) else {
                    return;
                };
                tracing::warn!(event = kind, request_id, credential_ref = %p.credential_ref,
                    upstream = %p.upstream, status = p.status, reason = %p.reason);
            }
            // The envelope is extensible: an event type this plugin does not
            // know is not an error.
            other => tracing::debug!(event = "ignored", event_type = other),
        }
    }

    /// Tokens observed for `identity` since start.
    pub fn tokens_of(&self, identity: &str) -> u64 {
        self.lock_tokens().get(identity).copied().unwrap_or(0)
    }

    fn add_tokens(&self, identity: &str, tokens: u64) -> u64 {
        let mut map = self.lock_tokens();
        let total = map.entry(identity.to_owned()).or_default();
        *total += tokens;
        *total
    }

    fn lock_tokens(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn deny(req: &ResolveRoute, reason: String) -> RouteOutcome {
    tracing::info!(event = "resolve_route", request_id = %req.request_id,
        caller_identity = %req.caller_identity, upstream = %req.upstream, outcome = "deny_reason",
        reason = %reason);
    RouteOutcome::Deny { reason }
}

fn decode<T: serde::de::DeserializeOwned>(event: &NotifyEvent) -> Option<T> {
    match event.payload() {
        Ok(payload) => Some(payload),
        Err(e) => {
            tracing::warn!(event = %event.event_type, error = %e, "undecodable payload_json");
            None
        }
    }
}

/// Tokens in a `call_completed` `usage` object. Its shape is the
/// implementation's: Anthropic reports `usage.input_tokens` /
/// `usage.output_tokens`, OpenAI-style APIs `usage.prompt_tokens` /
/// `usage.completion_tokens`. Anything else counts as zero.
pub fn observed_tokens(usage: &serde_json::Value) -> u64 {
    [
        "input_tokens",
        "output_tokens",
        "prompt_tokens",
        "completion_tokens",
    ]
    .iter()
    .filter_map(|k| usage.get("usage")?.get(k)?.as_u64())
    .sum()
}

#[tonic::async_trait]
impl JanusPlugin for TenantPinning {
    async fn resolve_route(
        &self,
        request: Request<proto::ResolveRouteRequest>,
    ) -> Result<Response<proto::ResolveRouteResponse>, Status> {
        // Decoding enforces the contract's invariants; a request that
        // violates them (an empty caller_identity, say) is denied.
        let outcome = match ResolveRoute::try_from(request.into_inner()) {
            Ok(req) => self.resolve(&req).await,
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
            self.handle(event.into()).await;
        }
        Ok(Response::new(proto::NotifyAck {}))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use janus_core::contract::request_log::AbandonStage;
    use serde_json::json;

    use super::*;

    fn plugin() -> TenantPinning {
        let policy = Policy::parse(
            "budget = { requests = 2, window_secs = 3600 }\n[tenants.acme]\npin = { claude = \"claude-acme\" }",
        )
        .expect("policy");
        TenantPinning::new(policy).expect("meter")
    }

    fn request(request_id: &str, claims: &[(&str, &str)]) -> ResolveRoute {
        ResolveRoute {
            request_id: request_id.into(),
            upstream: "claude".into(),
            implementation: "anthropic".into(),
            caller_identity: "agent-1".into(),
            correlation_id: None,
            identity_claims: claims
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn abandoned(request_id: &str) -> NotifyEvent {
        let payload = CallAbandoned {
            caller_identity: "agent-1".into(),
            stage: AbandonStage::Forward,
            reason: "upstream unreachable".into(),
        };
        NotifyEvent::call_abandoned(request_id, None, &payload)
    }

    #[tokio::test]
    async fn budget_is_charged_on_admit_and_refunded_on_abandon() {
        let p = plugin();
        let acme = [("tenant", "acme")];
        let pinned = RouteOutcome::Credential("claude-acme".into());
        assert_eq!(p.resolve(&request("r1", &acme)).await, pinned);
        assert_eq!(p.resolve(&request("r2", &acme)).await, pinned);
        let RouteOutcome::Deny { reason } = p.resolve(&request("r3", &acme)).await else {
            panic!("third request must exceed a budget of two");
        };
        assert!(
            reason.starts_with("request budget exhausted: 2/2"),
            "{reason}"
        );

        // The denied request was never charged: refunding it changes nothing.
        p.handle(abandoned("r3")).await;
        assert!(matches!(
            p.resolve(&request("r4", &acme)).await,
            RouteOutcome::Deny { .. }
        ));

        // An abandoned admitted call gives its unit back, once.
        p.handle(abandoned("r2")).await;
        p.handle(abandoned("r2")).await;
        assert_eq!(p.resolve(&request("r5", &acme)).await, pinned);
        assert!(matches!(
            p.resolve(&request("r6", &acme)).await,
            RouteOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_policy_denial_does_not_consume_budget() {
        let p = plugin();
        for n in 0..5 {
            let blocked = request(&format!("b{n}"), &[("tier", "blocked")]);
            assert!(matches!(
                p.resolve(&blocked).await,
                RouteOutcome::Deny { .. }
            ));
        }
        assert_eq!(
            p.resolve(&request("ok", &[])).await,
            RouteOutcome::UseDefault
        );
    }

    #[tokio::test]
    async fn completed_calls_record_observed_tokens_and_unknown_events_are_ignored() {
        let p = plugin();
        p.resolve(&request("r1", &[])).await;
        let payload = CallCompleted {
            caller_identity: "agent-1".into(),
            upstream: "claude".into(),
            implementation: "anthropic".into(),
            status: 200,
            credential_ref: "claude-shared".into(),
            duration_ms: 12,
            usage: json!({"model": "m", "usage": {"input_tokens": 25, "output_tokens": 15}}),
            attempts: vec![],
        };
        p.handle(NotifyEvent::call_completed("r1", None, &payload))
            .await;
        p.handle(NotifyEvent {
            event_type: "budget_rebalanced".into(),
            request_id: None,
            correlation_id: None,
            payload_json: "not json".into(),
        })
        .await;
        assert_eq!(p.tokens_of("agent-1"), 40);
        // Settled: a late abandonment for the same request cannot refund it.
        p.handle(abandoned("r1")).await;
        p.resolve(&request("r2", &[])).await;
        assert!(matches!(
            p.resolve(&request("r3", &[])).await,
            RouteOutcome::Deny { .. }
        ));
    }

    #[test]
    fn tokens_are_read_from_either_usage_shape() {
        let openai = json!({"usage": {"prompt_tokens": 7, "completion_tokens": 3}});
        assert_eq!(observed_tokens(&openai), 10);
        assert_eq!(observed_tokens(&serde_json::Value::Null), 0);
    }
}
