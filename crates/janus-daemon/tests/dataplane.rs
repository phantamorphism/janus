//! The data plane end to end: a real daemon (`janus_daemon::Daemon`), real
//! fake upstreams on loopback ports, a real gRPC plugin on a unix socket or
//! a linked plugin, and hmac-token callers.

#[path = "dataplane/harness.rs"]
mod harness;
#[path = "dataplane/plugin.rs"]
mod plugin;
#[path = "dataplane/refresh.rs"]
mod refresh;
#[path = "dataplane/upstream.rs"]
mod upstream;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::StatusCode;
use janus_core::contract::binding::Method;
use janus_core::contract::identity::RejectionKind;
use janus_core::contract::plugin::PluginFailureClass;
use janus_core::contract::request_log::{AbandonStage, Outcome};
use janus_core::contract::upstream::UpstreamKind;
use janus_daemon::{ConfigError, Daemon, DaemonConfig, Embedded, RunError};

use harness::*;
use plugin::{GrpcPlugin, Linked, Recorder, Reply};
use upstream::{Script, Upstream};

const COMPLETION: &str = "{\"id\":\"gen-1\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":3,\"total_tokens\":12}}";

fn ok_upstream() -> Script {
    Script::Fixed {
        status: StatusCode::OK,
        headers: vec![
            ("content-type", "application/json"),
            ("x-upstream-marker", "yes"),
        ],
        body: COMPLETION,
    }
}

fn linked(recorder: &Arc<Recorder>) -> Embedded {
    Embedded {
        plugin: Some(Arc::new(Linked(Arc::clone(recorder)))),
        extractors: vec![],
    }
}

async fn post(rt: &Running, path: &str, token: Option<&str>) -> reqwest::Response {
    let mut req = client()
        .post(rt.url(path))
        .header("content-type", "application/json")
        .body("{\"model\":\"m\",\"messages\":[]}");
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req.send().await.expect("request reaches the data plane")
}

const CHAT: &str = "/openrouter/v1/chat/completions";

/// `alice`'s chat completion through the data plane.
async fn post_alice(rt: &Running) -> reqwest::Response {
    post(rt, CHAT, Some(&token("alice"))).await
}

/// An ok upstream and a daemon whose gRPC plugin answers `reply`, with
/// `timeout_ms` as the plugin timeout when given.
async fn with_grpc(
    env: &Env,
    reply: Reply,
    timeout_ms: Option<u64>,
) -> (GrpcPlugin, Upstream, Running) {
    let plugin = GrpcPlugin::serve(&env.socket(), reply);
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Grpc;
    if let Some(timeout_ms) = timeout_ms {
        o.timeout_ms = timeout_ms;
    }
    let rt = env.start(&o, Embedded::default()).await;
    (plugin, up, rt)
}

/// An ok upstream and a daemon with a linked plugin answering `reply`.
async fn with_linked(env: &Env, reply: Reply) -> (Arc<Recorder>, Upstream, Running) {
    let recorder = Recorder::new(reply);
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Linked;
    let rt = env.start(&o, linked(&recorder)).await;
    (recorder, up, rt)
}

/// Nothing accepts connections on `addr`.
async fn not_listening(addr: &str) -> bool {
    tokio::net::TcpStream::connect(addr).await.is_err()
}

/// A loopback address that is free right now.
async fn free_addr() -> String {
    upstream::closed_addr().await.to_string()
}

// ---------------------------------------------------------------- identity

#[tokio::test(flavor = "multi_thread")]
async fn unidentified_request_never_reaches_plugin() {
    let env = Env::new();
    let (plugin, up, mut rt) = with_grpc(&env, Reply::UseDefault, None).await;

    let response = post(&rt, "/openrouter/v1/chat/completions", None).await;
    assert_eq!(response.status(), 401);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::RejectedIdentity {
            kind: RejectionKind::NoMaterial
        }
    );
    assert_eq!(record.binding, Some(Method::Hmac));
    assert_eq!(record.caller_identity, "");
    assert_eq!(record.credential_ref, None);

    // A valid request afterwards proves the plugin is reachable and that
    // its event stream is flowing: the rejected one left no trace there.
    let ok = post_alice(&rt).await;
    assert_eq!(ok.status(), 200);
    let ok_record = rt.record().await;
    plugin
        .recorder
        .wait_for("call_completed", &ok_record.request_id)
        .await;
    let resolved = plugin.recorder.resolved();
    assert_eq!(
        resolved.len(),
        1,
        "only the identified request was resolved"
    );
    assert_eq!(resolved[0].request_id, ok_record.request_id);
    assert!(plugin.recorder.events_for(&record.request_id).is_empty());
    assert_eq!(
        up.seen().len(),
        1,
        "the rejected request never reached the upstream"
    );
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejection_kinds_are_distinguished_and_claimed_identity_recorded() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let cases = [
        (
            token_with(b"wrong-secret", NAMESPACE, "mallory", now + 60, None),
            RejectionKind::Invalid,
        ),
        (
            token_with(SECRET, "some-other-namespace", "bob", now + 60, None),
            RejectionKind::WrongNamespace,
        ),
        (expired_token("carol"), RejectionKind::Expired),
        ("not-a-token".to_owned(), RejectionKind::Invalid),
    ];
    for (token, kind) in &cases {
        let response = post(&rt, "/openrouter/v1/chat/completions", Some(token)).await;
        let (status, body) = caller_view(response).await;
        assert_eq!(status, 401);
        assert!(body.contains(kind.as_str()), "{body}");
        let record = rt.record().await;
        assert_eq!(record.outcome, Outcome::RejectedIdentity { kind: *kind });
        assert_eq!(record.caller_identity, "");
    }
    assert!(up.seen().is_empty());
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejection_does_not_wait_for_the_body() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;

    // A body that never ends: if the daemon read it, the 401 would never come.
    let (_keep_open, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let endless = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client()
            .post(rt.url("/openrouter/v1/chat/completions"))
            .body(endless)
            .send(),
    )
    .await
    .expect("401 arrives without the body being read")
    .expect("response");
    assert_eq!(response.status(), 401);
    let record = rt.record().await;
    assert_eq!(record.outcome.name(), "rejected_identity");
    assert_eq!(up.connections(), 0);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unbound_upstream_is_rejected_never_open() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.default_identity = false;
    let mut rt = env.start(&o, Embedded::default()).await;

    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 401);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::RejectedIdentity {
            kind: RejectionKind::NoMaterial
        }
    );
    assert_eq!(record.binding, None);
    assert_eq!(record.upstream, Some(UpstreamKind::OpenRouter));
    assert!(up.seen().is_empty());
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_paths_on_the_data_plane_are_404() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;
    let admin_token = std::fs::read_to_string(env.path("data").join("admin-token")).ok();

    for path in [
        "/v1/status",
        "/v1/credentials",
        "/v1/requests/stream",
        "/",
        "/openrouterx/v1",
    ] {
        let mut req = client().get(rt.url(path));
        if let Some(t) = &admin_token {
            req = req.bearer_auth(t.trim());
        }
        let (status, body) = caller_view(req.send().await.expect("response")).await;
        assert_eq!(status, 404, "{path}");
        assert_no_credential(&body);
        let record = rt.record().await;
        assert_eq!(record.upstream, None, "{path}");
        assert_eq!(record.binding, None);
        assert_eq!(record.path, path);
    }
    assert!(up.seen().is_empty());
    rt.stop().await;
}

/// A stalled request-log writer never slows or fails the data plane, and the
/// records it cannot take are dropped and counted rather than queued in
/// memory.
#[tokio::test(flavor = "multi_thread")]
async fn stalled_request_log_writer_does_not_affect_requests() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let rt = env.start(&Opts::new(&base), Embedded::default()).await;

    // Hold the database's write lock so the writer cannot commit.
    let blocker = rusqlite::Connection::open(env.path("data").join("janus.db")).expect("open");
    blocker.execute_batch("BEGIN IMMEDIATE").expect("lock");

    let client = client();
    let tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    for _ in 0..5000 {
        let response = client
            .get(rt.url("/nowhere"))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 404);
    }
    let after = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert!(after < tasks + 50, "tasks grew from {tasks} to {after}");
    assert!(rt.daemon.requests().stats().dropped > 0);

    blocker.execute_batch("COMMIT").expect("unlock");
    rt.stop().await;
}

/// One `GET /nowhere` on a fresh connection that closes after the answer:
/// the status code, or `None` when the connection was closed unanswered.
async fn raw_status(addr: &str) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    let request = "GET /nowhere HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n";
    let _ = stream.write_all(request.as_bytes()).await;
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut reply)).await;
    String::from_utf8_lossy(&reply)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
}

/// `data_plane.max_connections` is enforced on the data plane itself:
/// connections beyond it are closed unanswered and counted, and service
/// resumes once slots free up.
#[tokio::test(flavor = "multi_thread")]
async fn data_plane_refuses_connections_beyond_its_cap_and_recovers() {
    const CAP: usize = 3;
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let opts = Opts {
        max_connections: Some(CAP),
        ..Opts::new(&base)
    };
    let rt = env.start(&opts, Embedded::default()).await;
    let addr = rt.base.trim_start_matches("http://").to_owned();

    // Idle connections that never send a request fill every slot.
    let mut idle = Vec::new();
    for _ in 0..CAP {
        idle.push(
            tokio::net::TcpStream::connect(&addr)
                .await
                .expect("connect"),
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    // Once every idle connection is accepted, a new one is closed unanswered.
    while raw_status(&addr).await.is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the cap never applied"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rt.daemon.refused_connections() >= 1);
    let refused = rt.daemon.refused_connections();
    assert_eq!(raw_status(&addr).await, None);
    assert!(rt.daemon.refused_connections() > refused);

    // Freeing the slots restores service.
    idle.clear();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if raw_status(&addr).await == Some(404) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the slots never freed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rt.stop().await;
}

/// A stalled request-log writer cannot hold shutdown: the flush gives up
/// after its timeout and the daemon still stops.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_returns_within_its_bound_when_the_log_writer_is_stalled() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let rt = env.start(&Opts::new(&base), Embedded::default()).await;

    // Hold the write lock and queue several batches behind it, so the
    // writer stays blocked for far longer than the shutdown bound.
    let blocker = rusqlite::Connection::open(env.path("data").join("janus.db")).expect("open");
    blocker.execute_batch("BEGIN IMMEDIATE").expect("lock");
    let client = client();
    for _ in 0..3000 {
        client
            .get(rt.url("/nowhere"))
            .send()
            .await
            .expect("response");
    }
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(45), rt.stop())
        .await
        .expect("shutdown must return while the writer is stalled");
    assert!(started.elapsed() < Duration::from_secs(45));
    blocker.execute_batch("COMMIT").expect("unlock");
}

/// Send `target` as the raw request-target (no client-side normalisation)
/// and return the status code.
async fn raw_post(addr: &str, target: &str, host: &str, token: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {target} HTTP/1.1\r\nhost: {host}\r\nauthorization: Bearer {token}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let text = String::from_utf8_lossy(&response);
    text.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line for {target}: {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn path_tricks_never_leave_the_upstream_base() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = format!("{}/api", up.base_url());
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;
    let addr = rt.base.trim_start_matches("http://").to_owned();
    let caller = token("alice");

    for target in [
        "/openrouter/../../escape",
        "/openrouter/v1/../../escape",
        "/openrouter/v1/%2e%2e/%2e%2e/escape",
        "/openrouter/v1/.%2e/.%2e/escape",
        "/openrouter/v1/..%2f..%2fescape",
        "/openrouter/v1/%2E%2E%5C%2E%2E%5Cescape",
        "/openrouter//evil.example/v1",
        "/openrouter/@evil.example/v1",
        // Absolute-form: the authority is the caller's, never the upstream's.
        "http://169.254.169.254/openrouter/v1/chat/completions",
    ] {
        let status = raw_post(&addr, target, "169.254.169.254", &caller).await;
        let record = rt.record().await;
        assert!(
            !record.path.contains("169.254.169.254"),
            "authority recorded as path: {}",
            record.path
        );
        if status == 200 {
            assert_eq!(record.outcome, Outcome::Forwarded, "{target}");
        }
    }

    let seen = up.seen();
    assert!(
        seen.iter().any(|s| s.target == "/api/v1/chat/completions"),
        "the absolute-form request was forwarded to the configured upstream: {seen:?}"
    );
    for s in &seen {
        // An upstream that resolves dot segments (decoded or not) must
        // still land under `/api`: none may survive into the target.
        let path = s.target.split('?').next().unwrap_or_default();
        let dot_segment = path
            .split('/')
            .map(|seg| seg.to_ascii_lowercase().replace("%2e", "."))
            .any(|seg| seg == "." || seg == "..");
        assert!(
            path.starts_with("/api/") && !dot_segment,
            "forwarded outside the base path: {}",
            s.target
        );
        assert_eq!(
            s.headers["host"],
            up.addr.to_string(),
            "the upstream's host, not the caller's"
        );
    }
    rt.stop().await;
}

// ---------------------------------------------------- forwarding + substitution

#[tokio::test(flavor = "multi_thread")]
async fn identity_material_not_forwarded_upstream() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.anthropic = Some(&base);
    o.openrouter_extra = "[[upstream.route]]\npath = \"/v1/q\"\nidentity = { method = \"hmac\", source = { query = \"janus_token\" }, secret_ref = \"caller-identity\", namespace = \"janus-data-plane\", identity_claim = \"subject\" }";
    let mut rt = env.start(&o, Embedded::default()).await;
    let caller = token("alice");

    // Bearer header on openrouter.
    let response = post(&rt, "/openrouter/v1/chat/completions?beta=1", Some(&caller)).await;
    assert_eq!(response.status(), 200);
    rt.record().await;
    // x-api-key on anthropic.
    let response = client()
        .post(rt.url("/anthropic/v1/messages"))
        .header("x-api-key", &caller)
        .header("anthropic-version", "2023-06-01")
        .header("cookie", "session=caller-cookie")
        .body("{}")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    rt.record().await;
    // Query parameter on a route.
    let response = client()
        .get(rt.url(&format!("/openrouter/v1/q?a=1&janus_token={caller}&b=2")))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let record = rt.record().await;
    assert_eq!(record.route.as_deref(), Some("/v1/q"));
    assert!(!record.path.contains(&caller));

    let seen = up.seen();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        for (name, value) in &s.headers {
            let value = String::from_utf8_lossy(value.as_bytes());
            assert!(
                !value.contains(&caller),
                "identity in upstream header {name}"
            );
        }
        assert!(
            !s.target.contains(&caller),
            "identity in upstream target {}",
            s.target
        );
        assert!(
            s.headers.get("cookie").is_none(),
            "non-allowlisted header forwarded"
        );
    }
    assert_eq!(seen[0].target, "/v1/chat/completions?beta=1");
    assert_eq!(seen[0].method, http::Method::POST);
    assert_eq!(
        &seen[0].body[..],
        b"{\"model\":\"m\",\"messages\":[]}",
        "body streamed through"
    );
    assert_eq!(
        seen[0].headers["authorization"],
        format!("Bearer {OR_MAIN_KEY}")
    );
    assert_eq!(seen[0].headers["content-type"], "application/json");
    assert_eq!(seen[1].target, "/v1/messages");
    assert_eq!(seen[1].headers["x-api-key"], ANT_MAIN_KEY);
    assert_eq!(seen[1].headers["anthropic-version"], "2023-06-01");
    assert_eq!(seen[2].target, "/v1/q?a=1&b=2");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_leak_through_allowlist_refuses_to_forward() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    // `x-title` is on OpenRouter's forwarding allowlist: reading the
    // identity from it must make substitution refuse, not leak it.
    o.openrouter_extra = "[upstream.identity]\nsource = { header = \"x-title\" }";
    let mut rt = env.start(&o, Embedded::default()).await;

    let response = client()
        .post(rt.url("/openrouter/v1/chat/completions"))
        .header("x-title", token("alice"))
        .body("{}")
        .send()
        .await
        .expect("response");
    let (status, body) = caller_view(response).await;
    assert_eq!(status, 500);
    assert_no_credential(&body);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::Abandoned {
            stage: AbandonStage::Rewrite
        }
    );
    assert!(record
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("x-title"));
    assert_eq!(up.connections(), 0, "the upstream was never contacted");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forwarded_response_is_relayed_unmodified_and_settled() {
    let env = Env::new();
    let (recorder, _up, mut rt) = with_linked(&env, Reply::UseDefault).await;

    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-upstream-marker"], "yes");
    let (_, view) = caller_view(response).await;
    assert!(view.ends_with(COMPLETION), "body relayed byte for byte");
    assert_no_credential(&view);

    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::Forwarded);
    assert_eq!(record.status, Some(200));
    assert_eq!(record.credential_ref.as_deref(), Some("or-main"));
    assert_eq!(record.caller_identity, "alice");
    assert_eq!(record.correlation_id.as_deref(), Some("sess-1"));
    assert_eq!(record.upstream, Some(UpstreamKind::OpenRouter));
    assert_eq!(record.path, "/openrouter/v1/chat/completions");

    let resolved = recorder.resolved();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].request_id, record.request_id);
    assert_eq!(resolved[0].upstream, "openrouter");
    assert_eq!(resolved[0].caller_identity, "alice");
    assert_eq!(resolved[0].correlation_id.as_deref(), Some("sess-1"));
    assert_eq!(
        resolved[0].claims.get("team").map(String::as_str),
        Some("red")
    );

    let completed = recorder
        .wait_for("call_completed", &record.request_id)
        .await;
    assert_eq!(completed.correlation_id.as_deref(), Some("sess-1"));
    assert_eq!(completed.payload["caller_identity"], "alice");
    assert_eq!(completed.payload["upstream"], "openrouter");
    assert_eq!(completed.payload["status"], 200);
    assert_eq!(completed.payload["credential_ref"], "or-main");
    assert_eq!(
        Some(&completed.payload["usage"]),
        record.usage.as_ref(),
        "call_completed usage is the record's usage"
    );
    assert_eq!(record.usage.as_ref().unwrap()["usage"]["total_tokens"], 12);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_selected_credential_is_used() {
    let env = Env::new();
    let (plugin, up, mut rt) = with_grpc(&env, Reply::Credential("or-spare".into()), None).await;

    let response = post_alice(&rt).await;
    let (status, view) = caller_view(response).await;
    assert_eq!(status, 200);
    assert_no_credential(&view);
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-spare"));
    assert_eq!(
        up.seen()[0].headers["authorization"],
        format!("Bearer {OR_SPARE_KEY}")
    );
    let completed = plugin
        .recorder
        .wait_for("call_completed", &record.request_id)
        .await;
    assert_eq!(completed.payload["credential_ref"], "or-spare");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_is_relayed_as_it_arrives() {
    let env = Env::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let up = Upstream::start(Script::Gated {
        first: "data: one\n\n",
        gate: Arc::clone(&gate),
        rest: "data: two\n\n",
    })
    .await;
    let base = up.base_url();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;

    let mut response = post_alice(&rt).await;
    assert_eq!(response.status(), 200);
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("the first event arrives while the upstream is still open")
        .expect("chunk")
        .expect("some bytes");
    assert_eq!(&first[..], b"data: one\n\n");
    gate.notify_one();
    let rest = response.bytes().await.expect("rest");
    assert_eq!(&rest[..], b"data: two\n\n");
    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::Forwarded);
    assert_eq!(record.error, None);
    rt.stop().await;
}

// ---------------------------------------------------------------- policy

#[tokio::test(flavor = "multi_thread")]
async fn denied_request_never_reaches_upstream() {
    let env = Env::new();
    let (plugin, up, mut rt) = with_grpc(&env, Reply::Deny("budget exhausted".into()), None).await;

    let response = post_alice(&rt).await;
    let (status, body) = caller_view(response).await;
    assert_eq!(status, 403);
    assert!(
        body.contains("budget exhausted"),
        "the reason reaches the caller"
    );
    assert_no_credential(&body);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::Denied {
            reason: "budget exhausted".into()
        }
    );
    assert_eq!(record.credential_ref, None);
    let abandoned = plugin
        .recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "resolve");
    assert_eq!(abandoned.payload["caller_identity"], "alice");
    assert_eq!(up.connections(), 0);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn call_abandoned_sent_on_resolve_failure() {
    let env = Env::new();
    let (plugin, up, mut rt) = with_grpc(&env, Reply::Fail, None).await;

    let response = post_alice(&rt).await;
    let (status, body) = caller_view(response).await;
    assert_eq!(status, 503, "fail closed");
    assert!(body.contains("plugin_error"));
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::PluginFailed {
            class: PluginFailureClass::Error
        }
    );
    let abandoned = plugin
        .recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "resolve");
    assert!(plugin
        .recorder
        .events_for(&record.request_id)
        .iter()
        .all(|e| e.event_type != "call_completed"));
    assert_eq!(up.connections(), 0, "no fall-back to a default credential");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn call_abandoned_sent_on_resolve_timeout() {
    let env = Env::new();
    let (plugin, up, mut rt) =
        with_grpc(&env, Reply::Slow(Duration::from_millis(600)), Some(150)).await;

    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 503);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::PluginFailed {
            class: PluginFailureClass::Timeout
        }
    );
    let abandoned = plugin
        .recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "resolve");
    assert_eq!(up.connections(), 0);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unplugged_plugin_fails_closed() {
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Grpc; // nothing serves the socket
    let mut rt = env.start(&o, Embedded::default()).await;

    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 503);
    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::PluginFailed {
            class: PluginFailureClass::Unreachable
        }
    );
    assert_eq!(up.connections(), 0);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unusable_credential_fails_without_fall_through() {
    let env = Env::new();
    let (recorder, up, mut rt) =
        with_linked(&env, Reply::Credential("no-such-credential".into())).await;

    let response = post_alice(&rt).await;
    let (status, body) = caller_view(response).await;
    assert_eq!(status, 503);
    assert_no_credential(&body);
    assert!(
        !body.contains("no-such-credential"),
        "the ref is never told to the caller"
    );
    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::CredentialUnavailable);
    let abandoned = recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "credential");
    assert!(!abandoned.payload["reason"].as_str().unwrap().is_empty());
    assert_eq!(up.connections(), 0, "no other credential was tried");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_unreachable_is_upstream_error_and_abandoned_at_forward() {
    let env = Env::new();
    let recorder = Recorder::new(Reply::UseDefault);
    let dead = format!("http://{}", upstream::closed_addr().await);
    let mut o = Opts::new(&dead);
    o.plugin = PluginCfg::Linked;
    let mut rt = env.start(&o, linked(&recorder)).await;

    let response = post_alice(&rt).await;
    let (status, body) = caller_view(response).await;
    assert_eq!(status, 502);
    assert_no_credential(&body);
    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::UpstreamError);
    assert_eq!(record.credential_ref.as_deref(), Some("or-main"));
    let abandoned = recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "forward");
    rt.stop().await;
}

/// An upstream that closes a kept-alive connection under the proxy's next
/// request (what Node's 5 s keep-alive does after an idle gap) must not
/// surface as a 502: the proxy sends the request again on a new connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_on_a_connection_the_upstream_closed_still_succeeds() {
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let mut served = 0;
                let mut buf = vec![0u8; 16384];
                // Each request here is small and arrives in one read.
                while let Ok(read) = socket.read(&mut buf).await {
                    if read == 0 || (n == 1 && served == 1) {
                        return; // closed under the second request
                    }
                    served += 1;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{COMPLETION}",
                        COMPLETION.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
        }
    });
    let env = Env::new();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;

    for _ in 0..2 {
        let response = post_alice(&rt).await;
        let (status, body) = caller_view(response).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(rt.record().await.outcome, Outcome::Forwarded);
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_plugin_makes_zero_proto_calls() {
    let env = Env::new();
    // A plugin server is running, but the config has no [plugin] block.
    let plugin = GrpcPlugin::serve(&env.socket(), Reply::Deny("must not be asked".into()));
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut rt = env.start(&Opts::new(&base), Embedded::default()).await;

    for _ in 0..3 {
        let response = post_alice(&rt).await;
        assert_eq!(response.status(), 200, "the default chain decides");
        let record = rt.record().await;
        assert_eq!(record.outcome, Outcome::Forwarded);
        assert_eq!(record.credential_ref.as_deref(), Some("or-main"));
        assert_eq!(record.caller_identity, "alice");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(plugin.recorder.resolved().is_empty());
    assert!(plugin.recorder.events().is_empty());
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn none_route_skips_plugin_and_settlement_but_reports_credential_events() {
    let env = Env::new();
    let plugin = GrpcPlugin::serve(&env.socket(), Reply::UseDefault);
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Grpc;
    o.openrouter_extra =
        "[[upstream.route]]\npath = \"/v1/models\"\nidentity = { method = \"none\" }";
    let mut rt = env.start(&o, Embedded::default()).await;

    let response = client()
        .get(rt.url("/openrouter/v1/models"))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let open = rt.record().await;
    assert_eq!(open.outcome, Outcome::Forwarded);
    assert_eq!(open.binding, Some(Method::None));
    assert_eq!(open.caller_identity, "");
    assert_eq!(open.route.as_deref(), Some("/v1/models"));
    assert_eq!(open.credential_ref.as_deref(), Some("or-main"));

    // The upstream now rejects the credential on that route.
    up.set_script(Script::Fixed {
        status: StatusCode::UNAUTHORIZED,
        headers: vec![("content-type", "application/json")],
        body: "{\"error\":{\"message\":\"No auth credentials found\",\"code\":401}}",
    });
    let response = client()
        .get(rt.url("/openrouter/v1/models"))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 401, "the upstream's response is relayed");
    let rejected = rt.record().await;
    let invalid = plugin
        .recorder
        .wait_for("credential_invalid", &rejected.request_id)
        .await;
    assert_eq!(invalid.payload["credential_ref"], "or-main");
    assert_eq!(invalid.payload["upstream"], "openrouter");

    assert!(
        plugin.recorder.resolved().is_empty(),
        "ResolveRoute never called"
    );
    for record in [&open, &rejected] {
        assert!(plugin
            .recorder
            .events_for(&record.request_id)
            .iter()
            .all(|e| e.event_type != "call_completed" && e.event_type != "call_abandoned"));
    }
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_invalid_reaches_pool_and_plugin() {
    let env = Env::new();
    let recorder = Recorder::new(Reply::Credential("or-spare".into()));
    let up = Upstream::start(Script::Fixed {
        status: StatusCode::UNAUTHORIZED,
        headers: vec![("content-type", "application/json")],
        body: "{\"error\":{\"message\":\"User not found.\",\"code\":401}}",
    })
    .await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Linked;
    let mut rt = env.start(&o, linked(&recorder)).await;

    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 401);
    let first = rt.record().await;
    assert_eq!(first.outcome, Outcome::Forwarded);
    assert_eq!(first.status, Some(401));
    recorder
        .wait_for("credential_invalid", &first.request_id)
        .await;
    recorder.wait_for("call_completed", &first.request_id).await;

    // The pool now holds or-spare invalid: naming it again fails at the
    // credential step instead of reaching the upstream.
    let response = post_alice(&rt).await;
    assert_eq!(response.status(), 503);
    let second = rt.record().await;
    assert_eq!(second.outcome, Outcome::CredentialUnavailable);
    assert_eq!(up.seen().len(), 1);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_settlement_event() {
    let env = Env::new();
    let plugin = GrpcPlugin::serve(&env.socket(), Reply::UseDefault);
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Grpc;
    o.timeout_ms = 300;
    o.openrouter_extra =
        "[[upstream.route]]\npath = \"/v1/models\"\nidentity = { method = \"none\" }";
    let mut rt = env.start(&o, Embedded::default()).await;
    let r = &plugin.recorder;
    let path = "/openrouter/v1/chat/completions";

    let mut resolved_ids = Vec::new();
    let mut unresolved_ids = Vec::new();
    for reply in [
        Reply::UseDefault,
        Reply::Deny("no".into()),
        Reply::Credential("missing".into()),
        Reply::Fail,
        Reply::Slow(Duration::from_millis(800)),
        Reply::Credential("or-spare".into()),
    ] {
        r.set_reply(reply);
        post(&rt, path, Some(&token("alice")))
            .await
            .bytes()
            .await
            .expect("body");
        resolved_ids.push(rt.record().await.request_id.clone());
    }
    post(&rt, path, None).await.bytes().await.expect("body");
    unresolved_ids.push(rt.record().await.request_id.clone());
    client()
        .get(rt.url("/openrouter/v1/models"))
        .send()
        .await
        .expect("none route");
    unresolved_ids.push(rt.record().await.request_id.clone());

    // Every record is committed after its settlement event was handed to
    // the (ordered) Notify stream; one more settled request is a barrier.
    r.set_reply(Reply::Deny("barrier".into()));
    post(&rt, path, Some(&token("alice"))).await;
    let barrier = rt.record().await;
    r.wait_for("call_abandoned", &barrier.request_id).await;

    let settlements = |id: &str| {
        r.events_for(id)
            .into_iter()
            .filter(|e| e.event_type == "call_completed" || e.event_type == "call_abandoned")
            .count()
    };
    let asked: Vec<String> = r.resolved().into_iter().map(|x| x.request_id).collect();
    for id in &resolved_ids {
        assert!(asked.contains(id), "{id} was resolved");
        assert_eq!(
            settlements(id),
            1,
            "exactly one settlement for {id}: {:?}",
            r.events_for(id)
        );
    }
    for id in &unresolved_ids {
        assert!(!asked.contains(id));
        assert_eq!(settlements(id), 0, "no settlement for unresolved {id}");
    }
    rt.stop().await;
}

// ------------------------------------------------------- disconnects + shutdown

#[tokio::test(flavor = "multi_thread")]
async fn caller_disconnect_mid_stream_settles_exactly_once() {
    let env = Env::new();
    let recorder = Recorder::new(Reply::UseDefault);
    let up = Upstream::start(Script::Endless {
        chunk: "data: tick\n\n",
    })
    .await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Linked;
    let mut rt = env.start(&o, linked(&recorder)).await;

    let mut response = post_alice(&rt).await;
    assert_eq!(response.status(), 200);
    response.chunk().await.expect("chunk").expect("bytes");
    drop(response);

    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::Forwarded);
    assert_eq!(record.status, Some(200));
    assert!(record
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("disconnected"));
    recorder
        .wait_for("call_completed", &record.request_id)
        .await;
    eventually("the upstream stream to be dropped", || {
        up.stream_abandoned.load(Ordering::SeqCst)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settlements: Vec<_> = recorder
        .events_for(&record.request_id)
        .into_iter()
        .filter(|e| e.event_type.starts_with("call_"))
        .collect();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_disconnect_during_resolve_is_abandoned_at_resolve() {
    let env = Env::new();
    let recorder = Recorder::new(Reply::Slow(Duration::from_millis(1500)));
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Linked;
    o.timeout_ms = 5000;
    let mut rt = env.start(&o, linked(&recorder)).await;

    let request = client()
        .post(rt.url("/openrouter/v1/chat/completions"))
        .bearer_auth(token("alice"))
        .body("{}")
        .timeout(Duration::from_millis(300))
        .send();
    assert!(request.await.is_err(), "the caller gives up first");

    let record = rt.record().await;
    assert_eq!(
        record.outcome,
        Outcome::Abandoned {
            stage: AbandonStage::Resolve
        }
    );
    let abandoned = recorder
        .wait_for("call_abandoned", &record.request_id)
        .await;
    assert_eq!(abandoned.payload["stage"], "resolve");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(recorder.events_for(&record.request_id).len(), 1);
    assert_eq!(up.connections(), 0);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_lets_inflight_streams_finish_and_records_them() {
    let env = Env::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let up = Upstream::start(Script::Gated {
        first: "data: one\n\n",
        gate: Arc::clone(&gate),
        rest: "data: two\n\n",
    })
    .await;
    let base = up.base_url();
    let o = Opts::new(&base);
    let rt = env.start(&o, Embedded::default()).await;
    let addr = rt.base.trim_start_matches("http://").to_owned();
    let mut records = rt.daemon.requests().subscribe();

    let mut response = post_alice(&rt).await;
    response.chunk().await.expect("chunk").expect("first bytes");
    let shutdown = tokio::spawn(rt.stop());
    // The listener closes while the stream is still open.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !not_listening(&addr).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listener still open"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !shutdown.is_finished(),
        "shutdown waits for the in-flight stream"
    );
    gate.notify_one();
    let rest = response.bytes().await.expect("the stream completes");
    assert_eq!(&rest[..], b"data: two\n\n");
    tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .expect("shutdown completes")
        .expect("shutdown task");
    let record = records
        .try_recv()
        .expect("the record was committed before shutdown returned");
    assert_eq!(record.outcome, Outcome::Forwarded);
    assert_eq!(record.error, None);
}

/// Send `count` concurrent chat requests to `base` and read each response
/// to its end; returns how many were answered.
async fn answer_requests(base: &str, count: usize) -> usize {
    let caller = token("alice");
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..count {
        let request = client()
            .post(format!("{base}{CHAT}"))
            .bearer_auth(&caller)
            .body("{}");
        tasks.spawn(async move {
            let response = request.send().await.expect("response");
            response.bytes().await.expect("body");
        });
    }
    let mut answered = 0;
    while let Some(done) = tasks.join_next().await {
        done.expect("caller task");
        answered += 1;
    }
    answered
}

/// How many records the daemon's request log holds.
fn logged_requests(rt: &Running) -> usize {
    rt.daemon
        .requests()
        .query(&janus_daemon::store::RequestQuery {
            limit: Some(1000),
            ..Default::default()
        })
        .expect("query")
        .records
        .len()
}

/// D110: every request whose response reached its caller has a request-log
/// record after an immediate shutdown and a restart on the same data dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answered_requests_survive_immediate_shutdown_and_restart() {
    const ROUNDS: usize = 25;
    const PER_ROUND: usize = 20;
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let o = Opts::new(&base);
    let env = Env::new();
    let mut answered = 0;
    for round in 0..ROUNDS {
        let rt = env.start(&o, Embedded::default()).await;
        answered += answer_requests(&rt.base, PER_ROUND).await;
        rt.stop().await;
        let restarted = env.start(&o, Embedded::default()).await;
        assert_eq!(
            logged_requests(&restarted),
            answered,
            "round {round}: a request that was answered has no record after restart"
        );
        restarted.stop().await;
    }
}

/// The same guarantee through the real binary and a real SIGTERM, which is
/// how D110 stops the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answered_requests_survive_sigterm_of_the_binary() {
    const ROUNDS: usize = 40;
    const PER_ROUND: usize = 5;
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let env = Env::new();
    let mut answered = 0;
    for round in 0..ROUNDS {
        let mut o = Opts::new(&base);
        let bind = free_addr().await;
        o.data_plane = &bind;
        let config = env.write_config(&o);
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_janus-daemon"))
            .arg("--config")
            .arg(&config)
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("spawn janus-daemon");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while not_listening(&bind).await {
            assert!(tokio::time::Instant::now() < deadline, "daemon never bound");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        answered += answer_requests(&format!("http://{bind}"), PER_ROUND).await;
        let pid = child.id().expect("pid") as libc::pid_t;
        // SAFETY: signalling a child process this test owns.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .expect("exits on SIGTERM")
            .expect("wait");
        assert!(status.success(), "round {round}: {status}");

        let mut o = Opts::new(&base);
        o.data_plane = "127.0.0.1:0";
        let restarted = env.start(&o, Embedded::default()).await;
        assert_eq!(
            logged_requests(&restarted),
            answered,
            "round {round}: a request that was answered has no record after SIGTERM and restart"
        );
        restarted.stop().await;
    }
}

/// A small deterministic generator, so a failing run can be replayed from
/// the seed it prints.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_with_random_aborts_settle_exactly_once() {
    const REQUESTS: u64 = 200;
    let env = Env::new();
    let plugin = GrpcPlugin::serve(&env.socket(), Reply::Slow(Duration::from_millis(15)));
    // openrouter streams forever (a caller that leaves aborts mid-stream);
    // anthropic answers at once (a caller that waits long enough completes).
    let endless = Upstream::start(Script::Endless {
        chunk: "data: tick\n\n",
    })
    .await;
    let quick = Upstream::start(ok_upstream()).await;
    let (endless_base, quick_base) = (endless.base_url(), quick.base_url());
    let mut o = Opts::new(&endless_base);
    o.anthropic = Some(&quick_base);
    o.plugin = PluginCfg::Grpc;
    o.timeout_ms = 5000;
    let rt = env.start(&o, Embedded::default()).await;

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    eprintln!("seed {seed}");
    let mut rng = Lcg(seed);
    let caller = token("alice");
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..REQUESTS {
        // Abort anywhere from before the head is sent to mid-stream.
        let mut abort_after = Duration::from_millis(rng.below(120));
        // One caller in four is patient: it waits far longer than a loaded CI
        // runner needs, so some quick-upstream requests always complete.
        let patient = rng.below(4) == 0;
        let request = if rng.below(2) == 0 {
            client().post(rt.url(CHAT)).bearer_auth(&caller).body("{}")
        } else {
            if patient {
                abort_after = Duration::from_secs(4);
            }
            client()
                .post(rt.url("/anthropic/v1/messages"))
                .header("x-api-key", &caller)
                .body("{}")
        };
        tasks.spawn(async move {
            let exchange = async {
                let mut response = match request.send().await {
                    Ok(r) => r,
                    Err(e) => return format!("senderr {e:?}"),
                };
                let status = response.status();
                while let Ok(Some(_)) = response.chunk().await {}
                format!("done {status}")
            };
            let out = tokio::time::timeout(abort_after, exchange).await;
            format!("patient={} {}", abort_after.as_secs() > 0, out.unwrap_or_else(|_| "aborted".into()))
        });
    }
    let mut diag = std::collections::BTreeMap::<String, usize>::new();
    while let Some(r) = tasks.join_next().await {
        *diag.entry(r.unwrap()).or_default() += 1;
    }
    eprintln!("DIAG {diag:#?}");

    let recorder = &plugin.recorder;
    let settled = |request_id: &str| {
        recorder
            .events_for(request_id)
            .into_iter()
            .filter(|e| e.event_type == "call_completed" || e.event_type == "call_abandoned")
            .count()
    };
    eventually("every resolved request to settle", || {
        recorder
            .resolved()
            .iter()
            .all(|r| settled(&r.request_id) >= 1)
    })
    .await;
    // Late duplicates (a relay outcome after an abandon, say) would land now.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resolved = recorder.resolved();
    assert!(
        resolved.len() as u64 > REQUESTS / 4,
        "enough requests reached the plugin (seed {seed}): {}",
        resolved.len()
    );
    let ids: std::collections::HashSet<_> = resolved.iter().map(|r| r.request_id.clone()).collect();
    assert_eq!(
        ids.len(),
        resolved.len(),
        "a request was resolved twice (seed {seed})"
    );
    for r in &resolved {
        assert_eq!(
            settled(&r.request_id),
            1,
            "request {} (seed {seed}): {:?}",
            r.request_id,
            recorder.events_for(&r.request_id)
        );
    }
    // A caller that leaves while `ResolveRoute` is on the wire may get a
    // `call_abandoned` the plugin never saw the request for: the contract
    // allows that (a refund of a charge never made is a no-op), but only at
    // the resolve stage, and still only once per request.
    let mut per_request = std::collections::HashMap::<String, usize>::new();
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    for e in recorder.events() {
        if e.event_type == "call_completed" || e.event_type == "call_abandoned" {
            let id = e
                .request_id
                .clone()
                .expect("a settlement names its request");
            if !ids.contains(&id) {
                assert_eq!(e.event_type, "call_abandoned", "{e:?}");
                assert_eq!(e.payload["stage"], "resolve", "{e:?}");
            }
            *per_request.entry(id).or_default() += 1;
            *kinds.entry(e.event_type.clone()).or_default() += 1;
        }
    }
    for (id, count) in &per_request {
        assert_eq!(
            *count, 1,
            "request {id} settled {count} times (seed {seed})"
        );
    }
    eprintln!("settlements {kinds:?}");
    assert!(
        kinds.get("call_completed").is_some_and(|n| *n > 0)
            && kinds.get("call_abandoned").is_some_and(|n| *n > 0),
        "the run exercised both endings (seed {seed}): {kinds:?}"
    );
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_status_reports_the_bound_data_plane_address() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let env = Env::new();
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    // `127.0.0.1:0`: only the bound address says which port was taken.
    let rt = env.start(&Opts::new(&base), Embedded::default()).await;
    let bound = rt.daemon.data_plane_addr();
    assert_ne!(bound.port(), 0);

    let socket = env.path("data").join("admin.sock");
    let mut stream = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("admin socket");
    stream
        .write_all(b"GET /v1/status HTTP/1.1\r\nhost: janus\r\nconnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).expect("body");
    let status: serde_json::Value = serde_json::from_str(body).expect("status JSON");
    assert_eq!(
        status["listeners"]["data_plane"],
        bound.to_string(),
        "{status}"
    );
    rt.stop().await;
}

// ------------------------------------------------------------ startup refusals

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_linked_without_plugin() {
    let env = Env::new();
    let addr = free_addr().await;
    let mut o = Opts::new("https://openrouter.ai/api");
    o.plugin = PluginCfg::Linked;
    o.data_plane = &addr;
    let config = DaemonConfig::from_toml_str(&env.toml(&o)).expect("config parses");
    let err = Daemon::start(config, Embedded::default())
        .await
        .expect_err("a linked config without a linked plugin must not start");
    match &err {
        RunError::Config(e) => assert_eq!(e.at(), Some("plugin.linked"), "{e}"),
        other => panic!("unexpected error: {other}"),
    }
    assert!(
        not_listening(&addr).await,
        "nothing listens on the data-plane port"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_endpoint_with_linked_plugin() {
    let env = Env::new();
    let addr = free_addr().await;
    let mut o = Opts::new("https://openrouter.ai/api");
    o.plugin = PluginCfg::Grpc;
    o.data_plane = &addr;
    let config = DaemonConfig::from_toml_str(&env.toml(&o)).expect("config parses");
    let err = Daemon::start(config, linked(&Recorder::new(Reply::UseDefault)))
        .await
        .expect_err("endpoint plus a linked plugin is ambiguous");
    assert!(
        matches!(&err, RunError::Config(ConfigError::Invalid { at, .. }) if at == "plugin.endpoint"),
        "{err}"
    );
    assert!(not_listening(&addr).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_missing_secret_and_unregistered_extractor() {
    let env = Env::new();
    let addr = free_addr().await;
    let mut o = Opts::new("https://openrouter.ai/api");
    o.data_plane = &addr;
    std::fs::remove_file(env.path("caller.secret")).expect("remove secret");
    let config = DaemonConfig::from_toml_str(&env.toml(&o)).expect("config parses");
    let err = Daemon::start(config, Embedded::default())
        .await
        .expect_err("a binding whose secret cannot be read must not start");
    assert!(matches!(err, RunError::Identity(_)), "{err}");
    assert!(err.to_string().contains("caller.secret"), "{err}");
    assert!(not_listening(&addr).await);

    let env = Env::new();
    let mut o = Opts::new("https://openrouter.ai/api");
    o.data_plane = &addr;
    o.openrouter_extra =
        "[[upstream.route]]\npath = \"/v1/x\"\nidentity = { method = \"custom:acme\" }";
    let config = DaemonConfig::from_toml_str(&env.toml(&o)).expect("config parses");
    let err = Daemon::start(config, Embedded::default())
        .await
        .expect_err("an unregistered custom extractor must not start");
    assert!(err.to_string().contains("acme"), "{err}");
    assert!(not_listening(&addr).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_occupied_data_plane_port() {
    let env = Env::new();
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = held.local_addr().expect("addr").to_string();
    let mut o = Opts::new("https://openrouter.ai/api");
    o.data_plane = &addr;
    let config = DaemonConfig::from_toml_str(&env.toml(&o)).expect("config parses");
    let err = Daemon::start(config, Embedded::default())
        .await
        .expect_err("port in use");
    assert!(matches!(err, RunError::Bind { .. }), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_refusals_happen_before_the_data_plane_binds() {
    let env = Env::new();
    let addr = free_addr().await;
    let mut o = Opts::new("https://openrouter.ai/api");
    o.data_plane = &addr;
    let no_index = env.path("no-ui");
    std::fs::create_dir(&no_index).expect("assets dir");
    let toml = format!(
        "{}\n[ui]\nbind = \"127.0.0.1:0\"\nassets_dir = \"{}\"\n",
        env.toml(&o),
        no_index.display()
    );
    let path = env.path("janus.toml");
    std::fs::write(&path, &toml).expect("write config");

    let config = DaemonConfig::from_toml_str(&toml).expect("config parses");
    let err = Daemon::start(config, Embedded::default())
        .await
        .expect_err("UI assets without index.html must not start");
    assert!(matches!(err, RunError::Admin(_)), "{err}");
    assert!(not_listening(&addr).await);

    let bad = std::process::Command::new(env!("CARGO_BIN_EXE_janus-daemon"))
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run check-config");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(!bad.status.success(), "{stderr}");
    assert!(stderr.contains("UI assets"), "{stderr}");
    assert!(!env.path("data").join("admin.sock").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn check_config_validates_without_binding() {
    let env = Env::new();
    // Another process holds the data-plane port: check-config must not care.
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = held.local_addr().expect("addr").to_string();
    let mut o = Opts::new("https://openrouter.ai/api");
    o.data_plane = &addr;
    o.openrouter_extra =
        "[[upstream.route]]\npath = \"/v1/models\"\nidentity = { method = \"none\" }";
    let path = env.write_config(&o);
    let bin = env!("CARGO_BIN_EXE_janus-daemon");

    let ok = std::process::Command::new(bin)
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run check-config");
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(
        ok.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(stdout.contains("identity=hmac"), "{stdout}");
    assert!(
        stdout.contains("/openrouter/v1/models identity=none"),
        "{stdout}"
    );
    assert!(stdout.contains("credentials 2"), "{stdout}");
    assert!(
        !env.path("data").join("admin.sock").exists(),
        "no socket created"
    );
    assert!(
        !env.path("data").exists(),
        "check-config must not create the data directory"
    );

    std::fs::remove_file(env.path("caller.secret")).expect("remove secret");
    let bad = std::process::Command::new(bin)
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run check-config");
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("caller.secret"));
    assert!(!env.path("data").join("admin.sock").exists());
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_extractor_identity_reaches_the_plugin() {
    use async_trait::async_trait;
    use janus_core::contract::identity::{
        ExtractedIdentity, ExtractionError, IdentityExtractor, PresentedAt, RequestHead,
    };

    struct Acme;
    #[async_trait]
    impl IdentityExtractor for Acme {
        fn name(&self) -> &str {
            "acme"
        }
        async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
            let value = req
                .headers
                .get("x-acme")
                .ok_or(ExtractionError::NoMaterial)?
                .to_str()
                .map_err(|_| ExtractionError::Invalid("not text".into()))?;
            let subject = value
                .strip_prefix("acme-")
                .ok_or_else(|| ExtractionError::Invalid("unknown acme token".into()))?;
            Ok(ExtractedIdentity {
                caller_identity: subject.to_owned(),
                claims: [("tenant".to_owned(), "t1".to_owned())].into(),
                correlation_id: None,
                presented_at: PresentedAt::Header(http::HeaderName::from_static("x-acme")),
            })
        }
    }

    let env = Env::new();
    let recorder = Recorder::new(Reply::UseDefault);
    let up = Upstream::start(ok_upstream()).await;
    let base = up.base_url();
    let mut o = Opts::new(&base);
    o.plugin = PluginCfg::Linked;
    o.openrouter_extra = "[upstream.identity]\nmethod = \"custom:acme\"";
    let embedded = Embedded {
        plugin: Some(Arc::new(Linked(Arc::clone(&recorder)))),
        extractors: vec![Arc::new(Acme)],
    };
    let mut rt = env.start(&o, embedded).await;

    let response = client()
        .post(rt.url("/openrouter/v1/chat/completions"))
        .header("x-acme", "acme-dave")
        .body("{}")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let record = rt.record().await;
    assert_eq!(record.binding, Some(Method::Custom("acme".into())));
    assert_eq!(record.caller_identity, "dave");
    let resolved = recorder.resolved();
    assert_eq!(resolved[0].caller_identity, "dave");
    assert_eq!(
        resolved[0].claims.get("tenant").map(String::as_str),
        Some("t1")
    );
    assert!(up.seen()[0].headers.get("x-acme").is_none());
    rt.stop().await;
}
