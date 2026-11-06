//! Recording policy plugins: a real out-of-process gRPC server (the
//! generated tonic service on a unix socket) and a linked `JanusPlugin`,
//! both answering from the same script and recording what they saw.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use janus_core::contract::plugin::{
    JanusPlugin, NotifyEvent, PluginError, PluginFailureClass, ResolveRoute, RouteOutcome,
};
use janus_core::contract::proto;
use janus_core::contract::proto::janus_plugin_server::{self, JanusPluginServer};
use janus_core::contract::proto::resolve_route_response::Outcome;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

/// How the plugin answers `ResolveRoute`.
#[derive(Debug, Clone)]
pub enum Reply {
    UseDefault,
    Credential(String),
    Deny(String),
    /// An RPC error (gRPC `INTERNAL`; a linked plugin's `PluginError`).
    Fail,
    /// Answer `use_default` after this long.
    Slow(Duration),
}

/// One `ResolveRoute` as the plugin received it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub request_id: String,
    pub upstream: String,
    pub implementation: String,
    pub caller_identity: String,
    pub correlation_id: Option<String>,
    pub claims: BTreeMap<String, String>,
}

/// One `Notify` event as the plugin received it.
#[derive(Debug, Clone)]
pub struct Event {
    pub event_type: String,
    pub request_id: Option<String>,
    pub correlation_id: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug)]
pub struct Recorder {
    reply: Mutex<Reply>,
    resolved: Mutex<Vec<Resolved>>,
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    pub fn new(reply: Reply) -> Arc<Recorder> {
        Arc::new(Recorder {
            reply: Mutex::new(reply),
            resolved: Mutex::default(),
            events: Mutex::default(),
        })
    }

    pub fn set_reply(&self, reply: Reply) {
        *self.reply.lock().expect("reply") = reply;
    }

    pub fn resolved(&self) -> Vec<Resolved> {
        self.resolved.lock().expect("resolved").clone()
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().expect("events").clone()
    }

    /// Events about `request_id`.
    pub fn events_for(&self, request_id: &str) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| e.request_id.as_deref() == Some(request_id))
            .collect()
    }

    /// Wait until an event of `event_type` about `request_id` has arrived.
    pub async fn wait_for(&self, event_type: &str, request_id: &str) -> Event {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(e) = self
                .events_for(request_id)
                .into_iter()
                .find(|e| e.event_type == event_type)
            {
                return e;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no {event_type} for {request_id}; events: {:?}",
                self.events()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn answer(&self, resolved: Resolved) -> Reply {
        self.resolved.lock().expect("resolved").push(resolved);
        let reply = self.reply.lock().expect("reply").clone();
        if let Reply::Slow(delay) = reply {
            tokio::time::sleep(delay).await;
            return Reply::UseDefault;
        }
        reply
    }

    fn record(
        &self,
        event_type: String,
        request_id: Option<String>,
        correlation_id: Option<String>,
        payload_json: &str,
    ) {
        self.events.lock().expect("events").push(Event {
            event_type,
            request_id,
            correlation_id,
            payload: serde_json::from_str(payload_json).expect("payload_json is JSON"),
        });
    }
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// The gRPC side.
struct Grpc(Arc<Recorder>);

#[tonic::async_trait]
impl janus_plugin_server::JanusPlugin for Grpc {
    async fn resolve_route(
        &self,
        request: tonic::Request<proto::ResolveRouteRequest>,
    ) -> Result<tonic::Response<proto::ResolveRouteResponse>, tonic::Status> {
        let r = request.into_inner();
        let reply = self
            .0
            .answer(Resolved {
                request_id: r.request_id,
                upstream: r.upstream,
                implementation: r.implementation,
                caller_identity: r.caller_identity,
                correlation_id: non_empty(r.correlation_id),
                claims: r.identity_claims.into_iter().collect(),
            })
            .await;
        let outcome = match reply {
            Reply::UseDefault | Reply::Slow(_) => Outcome::UseDefault(true),
            Reply::Credential(r) => Outcome::CredentialRef(r),
            Reply::Deny(reason) => Outcome::DenyReason(reason),
            Reply::Fail => return Err(tonic::Status::internal("scripted failure")),
        };
        Ok(tonic::Response::new(proto::ResolveRouteResponse {
            outcome: Some(outcome),
        }))
    }

    async fn notify(
        &self,
        request: tonic::Request<tonic::Streaming<proto::NotifyEvent>>,
    ) -> Result<tonic::Response<proto::NotifyAck>, tonic::Status> {
        let mut stream = request.into_inner();
        while let Some(e) = stream.message().await? {
            self.0.record(
                e.event_type,
                non_empty(e.request_id),
                non_empty(e.correlation_id),
                &e.payload_json,
            );
        }
        Ok(tonic::Response::new(proto::NotifyAck {}))
    }
}

/// A gRPC plugin server; stops when dropped.
pub struct GrpcPlugin {
    pub recorder: Arc<Recorder>,
    stop: CancellationToken,
}

impl GrpcPlugin {
    pub fn serve(socket: &Path, reply: Reply) -> GrpcPlugin {
        let recorder = Recorder::new(reply);
        let listener = UnixListener::bind(socket).expect("bind plugin socket");
        let incoming = futures::stream::unfold(listener, |l| async move {
            Some((l.accept().await.map(|(s, _)| s), l))
        });
        let stop = CancellationToken::new();
        let service = JanusPluginServer::new(Grpc(Arc::clone(&recorder)));
        let until = stop.clone();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(incoming, until.cancelled_owned())
                .await;
        });
        GrpcPlugin { recorder, stop }
    }
}

impl Drop for GrpcPlugin {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// The linked side: what a custom build passes in `Embedded::plugin`.
pub struct Linked(pub Arc<Recorder>);

#[async_trait]
impl JanusPlugin for Linked {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        let reply = self
            .0
            .answer(Resolved {
                request_id: req.request_id,
                upstream: req.upstream,
                implementation: req.implementation,
                caller_identity: req.caller_identity,
                correlation_id: req.correlation_id,
                claims: req.identity_claims,
            })
            .await;
        match reply {
            Reply::UseDefault | Reply::Slow(_) => Ok(RouteOutcome::UseDefault),
            Reply::Credential(r) => Ok(RouteOutcome::Credential(r)),
            Reply::Deny(reason) => Ok(RouteOutcome::Deny { reason }),
            Reply::Fail => Err(PluginError {
                class: PluginFailureClass::Error,
                message: "scripted failure".into(),
            }),
        }
    }

    async fn notify(&self, event: NotifyEvent) {
        self.0.record(
            event.event_type,
            event.request_id,
            event.correlation_id,
            &event.payload_json,
        );
    }
}
