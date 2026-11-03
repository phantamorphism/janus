//! The plugin contract in Rust (`docs/plugin-contract`).
//!
//! [`ResolveRoute`], [`RouteOutcome`] and [`NotifyEvent`] mirror the
//! messages of `proto/janus.proto` one to one, with the two deliberate shape
//! differences the docs name: the outcome is an enum (an unset `oneof` is a
//! protocol error on the wire and unconstructible here), and `notify` takes
//! one event. Conversions to and from [`super::proto`] live here so both the
//! gRPC client and a Rust plugin's gRPC server share them.
//!
//! [`JanusPlugin`] is what a linked plugin implements; [`PluginClient`] is
//! the one interface the daemon codes against for all three execution
//! models. [`PluginStatus`] is the admin surface's `plugin` object
//! (`docs/admin-surface#status-endpoint`), which the daemon keeps from the
//! result of every plugin call.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::proto;
use super::request_log::AbandonStage;

/// The `ResolveRoute` request: who is asking, for which upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRoute {
    /// Janus-assigned, unique per proxied request, never caller-supplied.
    pub request_id: String,
    /// `"anthropic"` | `"openai"` | `"openrouter"`; additive.
    pub upstream: String,
    /// The verified identity. Never empty: the daemon does not call
    /// `ResolveRoute` without one.
    pub caller_identity: String,
    /// Grouping hint; possibly caller-controlled. Never key policy off it.
    pub correlation_id: Option<String>,
    /// Other verified claims, if the extraction method produced any.
    pub identity_claims: BTreeMap<String, String>,
}

/// The plugin's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Substitute this credential (`credential_ref`).
    Credential(String),
    /// Defer to the daemon's default credential chain.
    UseDefault,
    /// Refuse the request; the reason reaches the caller, so it is never
    /// empty.
    Deny { reason: String },
}

impl ResolveRoute {
    /// The contract's invariants on a request: non-empty `request_id`,
    /// `upstream` and `caller_identity`. Applied when decoding one from the
    /// wire; a plugin answering a request that fails it must deny.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.request_id.is_empty() {
            return Err(ProtocolError::MissingRequestId);
        }
        if self.upstream.is_empty() {
            return Err(ProtocolError::MissingUpstream);
        }
        if self.caller_identity.is_empty() {
            return Err(ProtocolError::MissingCallerIdentity);
        }
        Ok(())
    }
}

impl RouteOutcome {
    /// The contract's invariants on an answer: a non-empty `credential_ref`
    /// and a non-empty deny reason. Applied when decoding one from the wire
    /// and, by `LinkedPluginClient`, to a linked plugin's answer, so both
    /// execution models refuse the same answers (as `plugin_error`).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            RouteOutcome::Credential(r) if r.is_empty() => Err(ProtocolError::EmptyCredentialRef),
            RouteOutcome::Deny { reason } if reason.is_empty() => {
                Err(ProtocolError::EmptyDenyReason)
            }
            _ => Ok(()),
        }
    }
}

/// The generic event envelope. `event_type` is freeform; the four the
/// daemon itself emits are in [`event_type`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyEvent {
    pub event_type: String,
    /// The `ResolveRoute.request_id` this event belongs to, if it concerns
    /// one request.
    pub request_id: Option<String>,
    /// Same grouping hint, same trust caveat, as `ResolveRoute`.
    pub correlation_id: Option<String>,
    /// Event-specific data, JSON-encoded.
    pub payload_json: String,
}

/// The event types `janus-daemon` guarantees to emit
/// (`docs/plugin-contract#guaranteed-events`).
pub mod event_type {
    /// The request reached the upstream and a response came back. Payload:
    /// [`super::CallCompleted`].
    pub const CALL_COMPLETED: &str = "call_completed";
    /// `ResolveRoute` answered (or was sent and failed) and the call never
    /// reached the upstream. Payload: [`super::CallAbandoned`].
    pub const CALL_ABANDONED: &str = "call_abandoned";
    /// The upstream rejected the credential Janus substituted in.
    pub const CREDENTIAL_INVALID: &str = "credential_invalid";
    /// The upstream reported a problem with the account behind the
    /// credential rather than with the request.
    pub const ACCOUNT_ERROR: &str = "account_error";
}

/// `payload_json` of a `call_completed` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallCompleted {
    /// The same value `ResolveRoute` was given.
    pub caller_identity: String,
    pub upstream: String,
    /// The upstream's HTTP status.
    pub status: u16,
    /// What was actually used, including when the plugin said `use_default`.
    pub credential_ref: String,
    pub duration_ms: u64,
    /// Whatever the per-upstream implementation observed about what the call
    /// consumed; the same object the request log records.
    pub usage: serde_json::Value,
}

/// `payload_json` of a `call_abandoned` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallAbandoned {
    pub caller_identity: String,
    /// Which lifecycle step the call died at (`resolve` when `ResolveRoute`
    /// itself failed).
    pub stage: AbandonStage,
    pub reason: String,
}

impl NotifyEvent {
    /// A `call_completed` settlement event for `request_id`.
    pub fn call_completed(
        request_id: impl Into<String>,
        correlation_id: Option<String>,
        payload: &CallCompleted,
    ) -> Self {
        NotifyEvent {
            event_type: event_type::CALL_COMPLETED.to_owned(),
            request_id: Some(request_id.into()),
            correlation_id,
            payload_json: encode_payload(payload),
        }
    }

    /// A `call_abandoned` settlement event for `request_id`.
    pub fn call_abandoned(
        request_id: impl Into<String>,
        correlation_id: Option<String>,
        payload: &CallAbandoned,
    ) -> Self {
        NotifyEvent {
            event_type: event_type::CALL_ABANDONED.to_owned(),
            request_id: Some(request_id.into()),
            correlation_id,
            payload_json: encode_payload(payload),
        }
    }

    /// Decode `payload_json` as `T` (e.g. [`CallCompleted`]).
    pub fn payload<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(&self.payload_json)
    }
}

/// Encode a payload struct. The payload types are plain structs of strings,
/// integers and a `serde_json::Value`, for which JSON encoding cannot fail.
fn encode_payload<T: Serialize>(payload: &T) -> String {
    serde_json::to_string(payload).expect("settlement payloads always encode as JSON")
}

/// How a plugin call failed (`docs/plugin-contract#plugin-config`). Appears
/// on the request-log record (`outcome = plugin_failed`) and, without its
/// `plugin_` prefix, as the plugin's `health` on the admin surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginFailureClass {
    #[serde(rename = "plugin_unreachable")]
    Unreachable,
    #[serde(rename = "plugin_timeout")]
    Timeout,
    #[serde(rename = "plugin_tls_error")]
    TlsError,
    #[serde(rename = "plugin_no_client_auth")]
    NoClientAuth,
    #[serde(rename = "plugin_rejected_client_cert")]
    RejectedClientCert,
    /// An RPC error from a plugin that was reached — including a protocol
    /// violation in its answer and a panic in a linked plugin.
    #[serde(rename = "plugin_error")]
    Error,
}

impl PluginFailureClass {
    pub const ALL: [PluginFailureClass; 6] = [
        PluginFailureClass::Unreachable,
        PluginFailureClass::Timeout,
        PluginFailureClass::TlsError,
        PluginFailureClass::NoClientAuth,
        PluginFailureClass::RejectedClientCert,
        PluginFailureClass::Error,
    ];

    /// The request-log class: `plugin_unreachable`, `plugin_timeout`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            PluginFailureClass::Unreachable => "plugin_unreachable",
            PluginFailureClass::Timeout => "plugin_timeout",
            PluginFailureClass::TlsError => "plugin_tls_error",
            PluginFailureClass::NoClientAuth => "plugin_no_client_auth",
            PluginFailureClass::RejectedClientCert => "plugin_rejected_client_cert",
            PluginFailureClass::Error => "plugin_error",
        }
    }

    /// The admin surface's `health` after a call failed this way: the class
    /// without `plugin_`.
    pub fn health(self) -> PluginHealth {
        match self {
            PluginFailureClass::Unreachable => PluginHealth::Unreachable,
            PluginFailureClass::Timeout => PluginHealth::Timeout,
            PluginFailureClass::TlsError => PluginHealth::TlsError,
            PluginFailureClass::NoClientAuth => PluginHealth::NoClientAuth,
            PluginFailureClass::RejectedClientCert => PluginHealth::RejectedClientCert,
            PluginFailureClass::Error => PluginHealth::Error,
        }
    }
}

impl fmt::Display for PluginFailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed plugin call. On the resolve path the daemon turns every one into
/// a rejected request (fail closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{class}: {message}")]
pub struct PluginError {
    pub class: PluginFailureClass,
    /// Diagnostic for `last_error` and the daemon log.
    pub message: String,
}

impl PluginError {
    pub fn new(class: PluginFailureClass, message: impl Into<String>) -> Self {
        PluginError {
            class,
            message: message.into(),
        }
    }
}

/// A wire message that violates the contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("ResolveRouteRequest.request_id is empty")]
    MissingRequestId,
    #[error("ResolveRouteRequest.upstream is empty")]
    MissingUpstream,
    #[error("ResolveRouteRequest.caller_identity is empty; a plugin must deny such a request")]
    MissingCallerIdentity,
    #[error("ResolveRouteResponse has no outcome set")]
    MissingOutcome,
    #[error("ResolveRouteResponse.use_default is set to false, which names no outcome")]
    UseDefaultFalse,
    #[error("ResolveRouteResponse.credential_ref is empty")]
    EmptyCredentialRef,
    #[error("ResolveRouteResponse.deny_reason is empty; the reason reaches the caller")]
    EmptyDenyReason,
}

impl From<ProtocolError> for PluginError {
    fn from(err: ProtocolError) -> Self {
        PluginError::new(PluginFailureClass::Error, err.to_string())
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

impl From<ResolveRoute> for proto::ResolveRouteRequest {
    fn from(r: ResolveRoute) -> Self {
        proto::ResolveRouteRequest {
            request_id: r.request_id,
            upstream: r.upstream,
            caller_identity: r.caller_identity,
            correlation_id: r.correlation_id.unwrap_or_default(),
            identity_claims: r.identity_claims.into_iter().collect(),
        }
    }
}

impl TryFrom<proto::ResolveRouteRequest> for ResolveRoute {
    type Error = ProtocolError;

    fn try_from(r: proto::ResolveRouteRequest) -> Result<Self, Self::Error> {
        let req = ResolveRoute {
            request_id: r.request_id,
            upstream: r.upstream,
            caller_identity: r.caller_identity,
            correlation_id: non_empty(r.correlation_id),
            identity_claims: r.identity_claims.into_iter().collect(),
        };
        req.validate()?;
        Ok(req)
    }
}

impl From<RouteOutcome> for proto::ResolveRouteResponse {
    fn from(o: RouteOutcome) -> Self {
        use proto::resolve_route_response::Outcome;
        let outcome = match o {
            RouteOutcome::Credential(r) => Outcome::CredentialRef(r),
            RouteOutcome::UseDefault => Outcome::UseDefault(true),
            RouteOutcome::Deny { reason } => Outcome::DenyReason(reason),
        };
        proto::ResolveRouteResponse {
            outcome: Some(outcome),
        }
    }
}

impl TryFrom<proto::ResolveRouteResponse> for RouteOutcome {
    type Error = ProtocolError;

    fn try_from(r: proto::ResolveRouteResponse) -> Result<Self, Self::Error> {
        use proto::resolve_route_response::Outcome;
        let outcome = match r.outcome {
            None => return Err(ProtocolError::MissingOutcome),
            Some(Outcome::CredentialRef(c)) => RouteOutcome::Credential(c),
            Some(Outcome::UseDefault(true)) => RouteOutcome::UseDefault,
            Some(Outcome::UseDefault(false)) => return Err(ProtocolError::UseDefaultFalse),
            Some(Outcome::DenyReason(reason)) => RouteOutcome::Deny { reason },
        };
        outcome.validate()?;
        Ok(outcome)
    }
}

impl From<NotifyEvent> for proto::NotifyEvent {
    fn from(e: NotifyEvent) -> Self {
        proto::NotifyEvent {
            event_type: e.event_type,
            request_id: e.request_id.unwrap_or_default(),
            correlation_id: e.correlation_id.unwrap_or_default(),
            payload_json: e.payload_json,
        }
    }
}

impl From<proto::NotifyEvent> for NotifyEvent {
    fn from(e: proto::NotifyEvent) -> Self {
        NotifyEvent {
            event_type: e.event_type,
            request_id: non_empty(e.request_id),
            correlation_id: non_empty(e.correlation_id),
            payload_json: e.payload_json,
        }
    }
}

/// A plugin compiled into a custom build (`docs/plugin-contract#linked`).
#[async_trait]
pub trait JanusPlugin: Send + Sync + 'static {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError>;

    async fn notify(&self, event: NotifyEvent);
}

#[async_trait]
impl<P: JanusPlugin + ?Sized> JanusPlugin for Arc<P> {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        (**self).resolve_route(req).await
    }

    async fn notify(&self, event: NotifyEvent) {
        (**self).notify(event).await
    }
}

/// The one interface the daemon's request path calls, whichever execution
/// model is in use (`docs/plugin-contract#plugin-client`). Implemented by
/// `GrpcPluginClient` and `LinkedPluginClient`; pass-through is `None`, not
/// a third implementation.
///
/// The client does not track health. The data plane folds the result of
/// every `resolve_route` into the daemon's [`PluginStatus`]
/// ([`PluginStatus::record_ok`] / [`PluginStatus::record_failure`]), which
/// is what `/v1/status` and `/v1/plugin` serve.
#[async_trait]
pub trait PluginClient: Send + Sync {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError>;

    /// Hand off an event. Returns immediately; delivery is best-effort and a
    /// failure is logged, never surfaced to the request path.
    fn notify(&self, event: NotifyEvent);
}

/// The admin surface's `plugin.mode`: which execution model config selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginMode {
    /// No `[plugin]` block: pass-through.
    None,
    Linked,
    Unix,
    Tls,
    Plaintext,
}

impl PluginMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginMode::None => "none",
            PluginMode::Linked => "linked",
            PluginMode::Unix => "unix",
            PluginMode::Tls => "tls",
            PluginMode::Plaintext => "plaintext",
        }
    }
}

impl fmt::Display for PluginMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The admin surface's `plugin.health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginHealth {
    /// No call has been attempted yet.
    Unknown,
    /// The last call succeeded.
    Ok,
    Unreachable,
    Timeout,
    TlsError,
    NoClientAuth,
    RejectedClientCert,
    Error,
}

impl PluginHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginHealth::Unknown => "unknown",
            PluginHealth::Ok => "ok",
            PluginHealth::Unreachable => "unreachable",
            PluginHealth::Timeout => "timeout",
            PluginHealth::TlsError => "tls_error",
            PluginHealth::NoClientAuth => "no_client_auth",
            PluginHealth::RejectedClientCert => "rejected_client_cert",
            PluginHealth::Error => "error",
        }
    }
}

impl fmt::Display for PluginHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `plugin` object of `GET /v1/status` and `GET /v1/plugin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub mode: PluginMode,
    /// The configured endpoint; `None` for `none` and `linked`.
    pub endpoint: Option<String>,
    pub health: PluginHealth,
    /// The message of the most recent failed call.
    pub last_error: Option<String>,
    pub last_ok_at: Option<DateTime<Utc>>,
    /// TLS transport only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_expires_at: Option<DateTime<Utc>>,
    /// TLS transport only, once a handshake has seen the plugin's
    /// certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cert_expires_at: Option<DateTime<Utc>>,
}

impl PluginStatus {
    /// The status before any call: `health = unknown`.
    pub fn new(mode: PluginMode, endpoint: Option<String>) -> Self {
        PluginStatus {
            mode,
            endpoint,
            health: PluginHealth::Unknown,
            last_error: None,
            last_ok_at: None,
            client_cert_expires_at: None,
            server_cert_expires_at: None,
        }
    }

    /// A call succeeded at `at`. `last_error` is kept as history.
    pub fn record_ok(&mut self, at: DateTime<Utc>) {
        self.health = PluginHealth::Ok;
        self.last_ok_at = Some(at);
    }

    /// A call failed with `err`.
    pub fn record_failure(&mut self, err: &PluginError) {
        self.health = err.class.health();
        self.last_error = Some(err.message.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::resolve_route_response::Outcome;

    fn sample_request(correlation: Option<&str>) -> ResolveRoute {
        let mut claims = BTreeMap::new();
        claims.insert("agent_type".to_owned(), "coder".to_owned());
        claims.insert("tenant".to_owned(), "acme".to_owned());
        ResolveRoute {
            request_id: "req-1".into(),
            upstream: "anthropic".into(),
            caller_identity: "session-42".into(),
            correlation_id: correlation.map(str::to_owned),
            identity_claims: claims,
        }
    }

    #[test]
    fn resolve_route_request_round_trips_through_proto() {
        for correlation in [Some("conv-7"), None] {
            let rust = sample_request(correlation);
            let wire: proto::ResolveRouteRequest = rust.clone().into();
            assert_eq!(wire.correlation_id, correlation.unwrap_or(""));
            assert_eq!(wire.identity_claims.len(), 2);
            assert_eq!(wire.identity_claims["tenant"], "acme");
            // Survives actual protobuf encoding, not just struct conversion.
            let bytes = prost::Message::encode_to_vec(&wire);
            let decoded: proto::ResolveRouteRequest =
                prost::Message::decode(bytes.as_slice()).expect("decode");
            let back = ResolveRoute::try_from(decoded).expect("valid");
            assert_eq!(back, rust);
        }
    }

    #[test]
    fn resolve_route_request_without_ids_is_a_protocol_error() {
        let mut wire: proto::ResolveRouteRequest = sample_request(None).into();
        wire.caller_identity.clear();
        assert_eq!(
            ResolveRoute::try_from(wire.clone()),
            Err(ProtocolError::MissingCallerIdentity)
        );
        wire.request_id.clear();
        assert_eq!(
            ResolveRoute::try_from(wire),
            Err(ProtocolError::MissingRequestId)
        );
        let mut wire: proto::ResolveRouteRequest = sample_request(None).into();
        wire.upstream.clear();
        assert_eq!(
            ResolveRoute::try_from(wire),
            Err(ProtocolError::MissingUpstream)
        );
    }

    #[test]
    fn every_route_outcome_round_trips_through_proto() {
        let cases = [
            (
                RouteOutcome::Credential("team-a-anthropic".into()),
                Outcome::CredentialRef("team-a-anthropic".into()),
            ),
            (RouteOutcome::UseDefault, Outcome::UseDefault(true)),
            (
                RouteOutcome::Deny {
                    reason: "budget exhausted".into(),
                },
                Outcome::DenyReason("budget exhausted".into()),
            ),
        ];
        for (rust, wire_outcome) in cases {
            let wire: proto::ResolveRouteResponse = rust.clone().into();
            assert_eq!(wire.outcome, Some(wire_outcome));
            let bytes = prost::Message::encode_to_vec(&wire);
            let decoded: proto::ResolveRouteResponse =
                prost::Message::decode(bytes.as_slice()).expect("decode");
            assert_eq!(RouteOutcome::try_from(decoded), Ok(rust));
        }
    }

    #[test]
    fn malformed_route_outcomes_are_protocol_errors_classed_plugin_error() {
        let cases = [
            (None, ProtocolError::MissingOutcome),
            (
                Some(Outcome::UseDefault(false)),
                ProtocolError::UseDefaultFalse,
            ),
            (
                Some(Outcome::CredentialRef(String::new())),
                ProtocolError::EmptyCredentialRef,
            ),
            (
                Some(Outcome::DenyReason(String::new())),
                ProtocolError::EmptyDenyReason,
            ),
        ];
        for (outcome, expected) in cases {
            let err = RouteOutcome::try_from(proto::ResolveRouteResponse { outcome })
                .expect_err("must be refused");
            assert_eq!(err, expected);
            let plugin_err: PluginError = err.into();
            assert_eq!(plugin_err.class, PluginFailureClass::Error);
        }
        // The same rule applies to a linked plugin's answer, which never
        // crosses the wire.
        assert_eq!(
            RouteOutcome::Deny {
                reason: String::new()
            }
            .validate(),
            Err(ProtocolError::EmptyDenyReason)
        );
        assert_eq!(RouteOutcome::UseDefault.validate(), Ok(()));
    }

    #[test]
    fn notify_event_round_trips_through_proto() {
        let full = NotifyEvent {
            event_type: "credential_invalid".into(),
            request_id: Some("req-9".into()),
            correlation_id: Some("conv-1".into()),
            payload_json: r#"{"credential_ref":"a"}"#.into(),
        };
        let bare = NotifyEvent {
            event_type: "something_new".into(),
            request_id: None,
            correlation_id: None,
            payload_json: "{}".into(),
        };
        for event in [full, bare] {
            let wire: proto::NotifyEvent = event.clone().into();
            let bytes = prost::Message::encode_to_vec(&wire);
            let decoded: proto::NotifyEvent =
                prost::Message::decode(bytes.as_slice()).expect("decode");
            assert_eq!(NotifyEvent::from(decoded), event);
        }
    }

    #[test]
    fn settlement_events_carry_documented_payloads() {
        let completed = CallCompleted {
            caller_identity: "session-42".into(),
            upstream: "openai".into(),
            status: 200,
            credential_ref: "default-openai".into(),
            duration_ms: 812,
            usage: serde_json::json!({"input_tokens": 10, "output_tokens": 3}),
        };
        let ev = NotifyEvent::call_completed("req-1", Some("conv".into()), &completed);
        assert_eq!(ev.event_type, event_type::CALL_COMPLETED);
        assert_eq!(ev.request_id.as_deref(), Some("req-1"));
        let raw: serde_json::Value = serde_json::from_str(&ev.payload_json).expect("json");
        for key in [
            "caller_identity",
            "upstream",
            "status",
            "credential_ref",
            "duration_ms",
            "usage",
        ] {
            assert!(raw.get(key).is_some(), "missing {key}");
        }
        assert_eq!(ev.payload::<CallCompleted>().expect("decode"), completed);

        let abandoned = CallAbandoned {
            caller_identity: "session-42".into(),
            stage: AbandonStage::Resolve,
            reason: "plugin_timeout".into(),
        };
        let ev = NotifyEvent::call_abandoned("req-2", None, &abandoned);
        assert_eq!(ev.event_type, event_type::CALL_ABANDONED);
        let raw: serde_json::Value = serde_json::from_str(&ev.payload_json).expect("json");
        assert_eq!(raw["stage"], "resolve");
        assert_eq!(ev.payload::<CallAbandoned>().expect("decode"), abandoned);
    }

    #[test]
    fn failure_classes_use_documented_names() {
        let expected = [
            ("plugin_unreachable", "unreachable"),
            ("plugin_timeout", "timeout"),
            ("plugin_tls_error", "tls_error"),
            ("plugin_no_client_auth", "no_client_auth"),
            ("plugin_rejected_client_cert", "rejected_client_cert"),
            ("plugin_error", "error"),
        ];
        assert_eq!(PluginFailureClass::ALL.len(), expected.len());
        for (class, (name, health)) in PluginFailureClass::ALL.into_iter().zip(expected) {
            assert_eq!(class.as_str(), name);
            assert_eq!(class.health().as_str(), health);
            assert_eq!(
                serde_json::to_string(&class.health()).expect("ser"),
                format!("\"{health}\"")
            );
            assert_eq!(
                serde_json::to_string(&class).expect("ser"),
                format!("\"{name}\"")
            );
        }
        let err = PluginError::new(PluginFailureClass::Timeout, "no answer in 250ms");
        assert_eq!(err.to_string(), "plugin_timeout: no answer in 250ms");
    }

    #[test]
    fn plugin_status_starts_unknown_and_tracks_call_results() {
        let mut status =
            PluginStatus::new(PluginMode::Tls, Some("https://policy.internal:7443".into()));
        assert_eq!(status.health, PluginHealth::Unknown);
        let json = serde_json::to_value(&status).expect("ser");
        assert_eq!(json["mode"], "tls");
        assert_eq!(json["health"], "unknown");
        assert!(json.get("client_cert_expires_at").is_none());

        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("ts");
        status.record_ok(now);
        assert_eq!(status.health, PluginHealth::Ok);
        assert_eq!(status.last_ok_at, Some(now));
        status.record_failure(&PluginError::new(
            PluginFailureClass::RejectedClientCert,
            "bad certificate",
        ));
        assert_eq!(status.health, PluginHealth::RejectedClientCert);
        assert_eq!(status.last_error.as_deref(), Some("bad certificate"));
        assert_eq!(status.last_ok_at, Some(now), "last success is kept");
        let json = serde_json::to_value(&status).expect("ser");
        assert_eq!(json["health"], "rejected_client_cert");
        assert_eq!(
            serde_json::from_value::<PluginStatus>(json).expect("de"),
            status
        );
        assert_eq!(
            [
                PluginMode::None,
                PluginMode::Linked,
                PluginMode::Unix,
                PluginMode::Tls,
                PluginMode::Plaintext
            ]
            .map(PluginMode::as_str),
            ["none", "linked", "unix", "tls", "plaintext"]
        );
    }

    struct Fixed(RouteOutcome);

    #[async_trait]
    impl JanusPlugin for Fixed {
        async fn resolve_route(&self, _req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
            Ok(self.0.clone())
        }

        async fn notify(&self, _event: NotifyEvent) {}
    }

    #[tokio::test]
    async fn arc_dyn_plugin_forwards_to_the_linked_implementation() {
        let plugin: Arc<dyn JanusPlugin> = Arc::new(Fixed(RouteOutcome::Deny {
            reason: "no".into(),
        }));
        let outcome = JanusPlugin::resolve_route(&plugin, sample_request(None))
            .await
            .expect("answers");
        assert_eq!(
            outcome,
            RouteOutcome::Deny {
                reason: "no".into()
            }
        );
    }
}
