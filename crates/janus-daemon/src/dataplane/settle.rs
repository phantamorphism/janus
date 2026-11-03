//! Step 7 of the request lifecycle, and the guarantee behind it: every
//! request gets exactly one request-log record, and every request
//! `ResolveRoute` was sent for gets exactly one of `call_completed` /
//! `call_abandoned` (`docs/plugin-contract#guaranteed-events`).
//!
//! A [`Settlement`] is created when a request arrives and consumed exactly
//! once: by [`Settlement::abandon`] when the pipeline stops the request
//! itself, by [`Settlement::relayed`] when an upstream response head came
//! back (ownership then moves to a task that waits for the relay to end),
//! or by its `Drop` when the request future is dropped mid-pipeline — the
//! caller disconnected, or shutdown aborted the connection — which records
//! `abandoned` with the stage it was at. Each path takes the request's
//! facts out of the guard, so no two of them can both settle.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use http::StatusCode;
use janus_core::contract::binding::Method;
use janus_core::contract::credential::UpstreamFeedback;
use janus_core::contract::plugin::{event_type, CallAbandoned, CallCompleted, NotifyEvent};
use janus_core::contract::request_log::{AbandonStage, Outcome, RequestRecord};
use janus_core::contract::upstream::UpstreamKind;
use janus_core::proxy::{RelayEnd, RelayOutcome};
use serde_json::json;
use tokio::sync::oneshot;

use super::tracker::InFlight;
use super::DataPlane;

/// What is known about a request so far; becomes its [`RequestRecord`].
struct Facts {
    request_id: String,
    received_at: DateTime<Utc>,
    started: Instant,
    method: String,
    path: String,
    upstream: Option<UpstreamKind>,
    route: Option<String>,
    binding: Option<Method>,
    caller_identity: String,
    claimed_identity: Option<String>,
    correlation_id: Option<String>,
    credential_ref: Option<String>,
    /// The lifecycle step the request is in; what `abandoned` names.
    stage: AbandonStage,
    /// `ResolveRoute` was sent: a settlement event is owed.
    resolve_sent: bool,
}

/// How a record ends, beyond the facts.
struct Ending {
    outcome: Outcome,
    status: Option<u16>,
    usage: Option<serde_json::Value>,
    error: Option<String>,
}

pub(super) struct Settlement {
    dp: Arc<DataPlane>,
    /// Keeps shutdown waiting until this request has settled, including a
    /// request whose future outlives its connection task (an h2 stream).
    _in_flight: InFlight,
    facts: Option<Facts>,
}

impl Settlement {
    /// A request just arrived. `path` is the path only: a query string can
    /// carry identity material and is never recorded.
    pub(super) fn begin(dp: &Arc<DataPlane>, method: &http::Method, path: &str) -> Self {
        Settlement {
            dp: Arc::clone(dp),
            _in_flight: dp.tracker.token(),
            facts: Some(Facts {
                request_id: uuid::Uuid::now_v7().to_string(),
                received_at: Utc::now(),
                started: Instant::now(),
                method: method.as_str().to_owned(),
                path: path.to_owned(),
                upstream: None,
                route: None,
                binding: None,
                caller_identity: String::new(),
                claimed_identity: None,
                correlation_id: None,
                credential_ref: None,
                stage: AbandonStage::Resolve,
                resolve_sent: false,
            }),
        }
    }

    fn facts(&mut self) -> &mut Facts {
        self.facts
            .as_mut()
            .expect("a settlement is only consumed by value")
    }

    pub(super) fn request_id(&mut self) -> String {
        self.facts().request_id.clone()
    }

    pub(super) fn upstream(&mut self, upstream: UpstreamKind) {
        self.facts().upstream = Some(upstream);
    }

    pub(super) fn binding(&mut self, route: Option<&str>, method: &Method) {
        let facts = self.facts();
        facts.route = route.map(str::to_owned);
        facts.binding = Some(method.clone());
    }

    pub(super) fn claimed(&mut self, claimed_identity: Option<String>) {
        self.facts().claimed_identity = claimed_identity;
    }

    pub(super) fn identified(&mut self, caller_identity: &str, correlation_id: Option<&str>) {
        let facts = self.facts();
        facts.caller_identity = caller_identity.to_owned();
        facts.correlation_id = correlation_id.map(str::to_owned);
    }

    /// Called immediately before `ResolveRoute` is sent: from here on a
    /// settlement event is owed, even if the call never returns.
    pub(super) fn resolve_sent(&mut self) {
        self.facts().resolve_sent = true;
    }

    pub(super) fn stage(&mut self, stage: AbandonStage) {
        self.facts().stage = stage;
    }

    pub(super) fn credential(&mut self, credential_ref: &str) {
        self.facts().credential_ref = Some(credential_ref.to_owned());
    }

    /// The pipeline stopped the request: record `outcome` with `reason` as
    /// its diagnostic, and send `call_abandoned` (at the current stage) if
    /// `ResolveRoute` was sent.
    pub(super) fn abandon(mut self, outcome: Outcome, reason: String) {
        if let Some(facts) = self.facts.take() {
            settle_unforwarded(&self.dp, facts, outcome, reason);
        }
    }

    /// An upstream response head came back and is being relayed with
    /// `status`. Settlement waits for `outcome`, which the relay body sends
    /// once when it ends or is dropped.
    pub(super) fn relayed(
        mut self,
        upstream: UpstreamKind,
        credential_ref: String,
        status: StatusCode,
        outcome: oneshot::Receiver<RelayOutcome>,
    ) {
        let Some(facts) = self.facts.take() else {
            return;
        };
        let dp = Arc::clone(&self.dp);
        self.dp.spawn(async move {
            // The relay body sends its outcome on every path, drop
            // included; a closed channel would mean it vanished without
            // running its destructor, which is a disconnect as far as
            // anyone can tell.
            let outcome = outcome.await.unwrap_or(RelayOutcome {
                status,
                usage: serde_json::Value::Null,
                feedback: None,
                end: RelayEnd::CallerDisconnected,
            });
            settle_relayed(&dp, facts, upstream, credential_ref, outcome).await;
        });
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        if let Some(facts) = self.facts.take() {
            let stage = facts.stage;
            settle_unforwarded(
                &self.dp,
                facts,
                Outcome::Abandoned { stage },
                format!("request dropped at the {stage} stage (caller disconnected or daemon shutting down)"),
            );
        }
    }
}

fn duration_ms(facts: &Facts) -> u64 {
    u64::try_from(facts.started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn settle_unforwarded(dp: &Arc<DataPlane>, facts: Facts, outcome: Outcome, reason: String) {
    if facts.resolve_sent {
        if let Some(plugin) = &dp.plugin {
            plugin.notify(NotifyEvent::call_abandoned(
                facts.request_id.clone(),
                facts.correlation_id.clone(),
                &CallAbandoned {
                    caller_identity: facts.caller_identity.clone(),
                    stage: facts.stage,
                    reason: reason.clone(),
                },
            ));
        }
    }
    let duration = duration_ms(&facts);
    submit(
        dp,
        facts,
        duration,
        Ending {
            outcome,
            status: None,
            usage: None,
            error: Some(reason),
        },
    );
}

async fn settle_relayed(
    dp: &Arc<DataPlane>,
    facts: Facts,
    upstream: UpstreamKind,
    credential_ref: String,
    relay: RelayOutcome,
) {
    let duration = duration_ms(&facts);
    let status = relay.status.as_u16();

    if let Some(feedback) = &relay.feedback {
        if let Err(e) = dp.pool.report(&credential_ref, feedback).await {
            tracing::warn!(
                request_id = %facts.request_id,
                error = %e,
                "could not record the upstream's verdict on the credential"
            );
        }
        // `credential_invalid` / `account_error` go to the plugin whether
        // or not this request was resolved through it (a `none` route's
        // credential can be rejected too); a rate limit is the pool's
        // business only.
        let event = match feedback {
            UpstreamFeedback::CredentialInvalid { reason } => {
                Some((event_type::CREDENTIAL_INVALID, reason))
            }
            UpstreamFeedback::AccountError { reason } => Some((event_type::ACCOUNT_ERROR, reason)),
            UpstreamFeedback::RateLimited { .. } => None,
        };
        if let (Some((event_type, reason)), Some(plugin)) = (event, &dp.plugin) {
            plugin.notify(NotifyEvent {
                event_type: event_type.to_owned(),
                request_id: Some(facts.request_id.clone()),
                correlation_id: facts.correlation_id.clone(),
                payload_json: json!({
                    "credential_ref": credential_ref,
                    "upstream": upstream.as_str(),
                    "status": status,
                    "reason": reason,
                })
                .to_string(),
            });
        }
    }

    if facts.resolve_sent {
        if let Some(plugin) = &dp.plugin {
            plugin.notify(NotifyEvent::call_completed(
                facts.request_id.clone(),
                facts.correlation_id.clone(),
                &CallCompleted {
                    caller_identity: facts.caller_identity.clone(),
                    upstream: upstream.as_str().to_owned(),
                    status,
                    credential_ref: credential_ref.clone(),
                    duration_ms: duration,
                    usage: relay.usage.clone(),
                },
            ));
        }
    }

    let (outcome, error) = match relay.end {
        RelayEnd::Complete => (Outcome::Forwarded, None),
        RelayEnd::CallerDisconnected => (
            Outcome::Forwarded,
            Some("caller disconnected before the response ended".to_owned()),
        ),
        RelayEnd::UpstreamFailed(e) => (
            Outcome::UpstreamError,
            Some(format!("upstream failed mid-response: {e}")),
        ),
    };
    submit(
        dp,
        facts,
        duration,
        Ending {
            outcome,
            status: Some(status),
            // `Null` is "nothing worth reporting": no usage on the record.
            usage: (!relay.usage.is_null()).then_some(relay.usage),
            error,
        },
    )
}

/// Write the record off the caller's critical path.
fn submit(dp: &Arc<DataPlane>, facts: Facts, duration_ms: u64, ending: Ending) {
    let record = RequestRecord {
        request_id: facts.request_id,
        received_at: facts.received_at,
        duration_ms,
        upstream: facts.upstream,
        route: facts.route,
        method: facts.method,
        path: facts.path,
        binding: facts.binding,
        caller_identity: facts.caller_identity,
        claimed_identity: facts.claimed_identity,
        correlation_id: facts.correlation_id,
        outcome: ending.outcome,
        credential_ref: facts.credential_ref,
        status: ending.status,
        usage: ending.usage,
        error: ending.error,
    };
    let requests = Arc::clone(&dp.requests);
    dp.spawn(async move { requests.submit(record).await });
}
