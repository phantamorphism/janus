//! Serves the plugin on a real unix socket and drives it with the gRPC client
//! generated from `proto/janus.proto`, as the daemon does.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use janus_core::contract::proto::janus_plugin_client::JanusPluginClient;
use janus_core::contract::proto::resolve_route_response::Outcome;
use janus_core::contract::proto::{NotifyEvent, ResolveRouteRequest};
use serde_json::json;
use tenant_pinning_plugin::policy::Policy;
use tenant_pinning_plugin::service::TenantPinning;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Code;

const POLICY: &str = r#"
    deny_unlisted = true
    budget = { requests = 2, window_secs = 3600 }
    [tenants.acme]
    pin = { claude = "claude-acme" }
    [tenants.globex]
    pin = { claude = "claude-globex" }
"#;

struct Running {
    client: JanusPluginClient<Channel>,
    socket: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<Result<(), tenant_pinning_plugin::Error>>,
    _dir: tempfile::TempDir,
}

async fn start(daemon_uid: Option<u32>) -> Running {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("run").join("policy.sock");
    let plugin = TenantPinning::new(Policy::parse(POLICY).expect("policy")).expect("plugin");
    let (stop, stopped) = oneshot::channel::<()>();
    let path = socket.clone();
    let server = tokio::spawn(async move {
        tenant_pinning_plugin::serve(plugin, &path, daemon_uid, async {
            let _ = stopped.await;
        })
        .await
    });
    let client = JanusPluginClient::new(connect(&socket).await);
    Running {
        client,
        socket,
        stop: Some(stop),
        server,
        _dir: dir,
    }
}

async fn connect(socket: &Path) -> Channel {
    for _ in 0..100 {
        if socket.exists() {
            let path = socket.to_path_buf();
            return Endpoint::try_from("http://[::]:50051")
                .expect("endpoint")
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let path = path.clone();
                    async move {
                        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?))
                    }
                }))
                .await
                .expect("connect");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("plugin never bound {}", socket.display());
}

fn request(request_id: &str, identity: &str, claims: &[(&str, &str)]) -> ResolveRouteRequest {
    ResolveRouteRequest {
        request_id: request_id.into(),
        upstream: "claude".into(),
        caller_identity: identity.into(),
        correlation_id: String::new(),
        identity_claims: claims
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        implementation: "anthropic".into(),
    }
}

impl Running {
    async fn resolve(&mut self, req: ResolveRouteRequest) -> Outcome {
        self.client
            .resolve_route(req)
            .await
            .expect("resolve_route")
            .into_inner()
            .outcome
            .expect("an outcome is always set")
    }

    /// Sends `events` on one Notify stream; the ack arrives after the
    /// plugin has handled every one.
    async fn notify(&mut self, events: Vec<NotifyEvent>) {
        self.client
            .notify(tokio_stream::iter(events))
            .await
            .expect("notify");
    }

    async fn stop(mut self) {
        let _ = self.stop.take().map(|tx| tx.send(()));
        self.server.await.expect("join").expect("serve");
        assert!(!self.socket.exists(), "socket removed at shutdown");
    }
}

fn event(event_type: &str, request_id: &str, payload: serde_json::Value) -> NotifyEvent {
    NotifyEvent {
        event_type: event_type.into(),
        request_id: request_id.into(),
        correlation_id: String::new(),
        payload_json: payload.to_string(),
    }
}

fn deny_reason(outcome: Outcome) -> String {
    match outcome {
        Outcome::DenyReason(reason) => reason,
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[tokio::test]
async fn routes_denies_and_settles_over_the_socket() {
    let mut p = start(None).await;
    let mode = std::fs::metadata(&p.socket)
        .expect("socket")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o660);
    let dir_mode = std::fs::metadata(p.socket.parent().expect("dir"))
        .expect("dir")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o750);

    let acme = [("tenant", "acme")];
    assert_eq!(
        p.resolve(request("r1", "a1", &acme)).await,
        Outcome::CredentialRef("claude-acme".into())
    );
    assert_eq!(
        p.resolve(request("g1", "g1", &[("tenant", "globex")]))
            .await,
        Outcome::CredentialRef("claude-globex".into())
    );
    let unlisted = p
        .resolve(request("u1", "u1", &[("tenant", "initech")]))
        .await;
    assert_eq!(
        deny_reason(unlisted),
        "tenant \"initech\" is not allowed by policy"
    );
    let blocked = p
        .resolve(request(
            "b1",
            "b1",
            &[("tenant", "acme"), ("tier", "blocked")],
        ))
        .await;
    assert_eq!(
        deny_reason(blocked),
        "identity b1 is blocked (tier=blocked)"
    );
    let anonymous = p.resolve(request("e1", "", &acme)).await;
    assert!(deny_reason(anonymous).contains("caller_identity is empty"));

    // a1's budget of 2: r1 above, r2 now, r3 refused.
    p.resolve(request("r2", "a1", &acme)).await;
    let exhausted = p.resolve(request("r3", "a1", &acme)).await;
    assert!(deny_reason(exhausted).starts_with("request budget exhausted: 2/2"));

    // r2 never reached the upstream: its refund lets the next call through.
    // An unknown event type on the same stream is ignored.
    let abandon =
        json!({"caller_identity": "a1", "stage": "forward", "reason": "connection refused"});
    p.notify(vec![
        event("tenant_rebalanced", "", json!({})),
        event("call_abandoned", "r2", abandon),
    ])
    .await;
    assert_eq!(
        p.resolve(request("r4", "a1", &acme)).await,
        Outcome::CredentialRef("claude-acme".into())
    );

    // r1 completed: settled, so a stray abandonment for it refunds nothing.
    let completed = json!({
        "caller_identity": "a1", "upstream": "claude", "implementation": "anthropic",
        "status": 200, "credential_ref": "claude-acme", "duration_ms": 5,
        "usage": {"usage": {"input_tokens": 10, "output_tokens": 2}}, "attempts": []
    });
    let stray = json!({"caller_identity": "a1", "stage": "forward", "reason": "late"});
    p.notify(vec![
        event("call_completed", "r1", completed),
        event("call_abandoned", "r1", stray),
    ])
    .await;
    let still_exhausted = p.resolve(request("r5", "a1", &acme)).await;
    assert!(deny_reason(still_exhausted).starts_with("request budget exhausted"));

    p.stop().await;
}

#[tokio::test]
async fn the_peer_uid_check_refuses_other_users() {
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    let mut same = start(Some(me)).await;
    assert!(matches!(
        same.resolve(request("r1", "a1", &[("tenant", "acme")]))
            .await,
        Outcome::CredentialRef(_)
    ));
    same.stop().await;

    let mut other = start(Some(me.wrapping_add(1))).await;
    let status = other
        .client
        .resolve_route(request("r1", "a1", &[("tenant", "acme")]))
        .await
        .expect_err("a different uid is refused");
    assert_eq!(status.code(), Code::PermissionDenied);
    other.stop().await;
}

#[tokio::test]
async fn shutdown_does_not_wait_forever_on_an_open_notify_stream() {
    let mut p = start(None).await;
    // The daemon keeps its Notify stream open for as long as it runs.
    let mut client = p.client.clone();
    let held =
        tokio::spawn(async move { client.notify(tokio_stream::pending::<NotifyEvent>()).await });
    p.resolve(request("r1", "a1", &[("tenant", "acme")])).await;
    let bound = tenant_pinning_plugin::SHUTDOWN_GRACE + Duration::from_secs(3);
    tokio::time::timeout(bound, p.stop())
        .await
        .expect("serve returns within the shutdown grace");
    held.abort();
}

#[test]
fn the_shipped_example_policy_parses() {
    let text = include_str!("../policy.example.toml");
    let policy = Policy::parse(text).expect("policy.example.toml");
    assert_eq!(policy.tenants.len(), 2);
}
