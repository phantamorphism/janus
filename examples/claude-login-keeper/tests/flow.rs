//! The keeper end to end against a fake admin API (a unix-socket HTTP
//! server) and `tests/fake-claude.sh` in place of `claude`, driven over its
//! real gRPC socket and its HTTP router.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use claude_login_keeper::admin::{Address, Admin};
use claude_login_keeper::config::Config;
use claude_login_keeper::keeper::Keeper;
use claude_login_keeper::service::KeeperPlugin;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use janus_core::contract::proto::janus_plugin_client::JanusPluginClient;
use janus_core::contract::proto::resolve_route_response::Outcome;
use janus_core::contract::proto::{NotifyEvent, ResolveRouteRequest};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::ServiceExt;

const REF: &str = "claude-sub";
const HOST: &str = "127.0.0.1:27711";
/// How long a flow may take to show its URL. macOS checks each newly
/// written executable (the shims) on its first exec, one at a time across
/// the machine, so with many tests in parallel this takes seconds.
const STARTUP: Duration = Duration::from_secs(40);

/// What the fake daemon holds for one ref.
#[derive(Clone)]
struct Slot {
    status: String,
    expires_at: DateTime<Utc>,
    refresh_refused: bool,
}

impl Slot {
    fn new(status: &str) -> Slot {
        Slot {
            status: status.into(),
            expires_at: Utc::now() + chrono::Duration::hours(8),
            refresh_refused: false,
        }
    }
}

/// The fake daemon: its slots, injected faults, and what it was asked.
#[derive(Default)]
struct Pool {
    slots: HashMap<String, Slot>,
    /// `GET` answers 500.
    show_fails: bool,
    /// `GET` answers this much later.
    show_delay: Duration,
    /// `PUT .../material` answers this status.
    rotate_fails: Option<u16>,
    refreshes: u32,
    /// `GET` requests so far.
    shows: u32,
    /// Every `PUT .../material`, answered or not: (ref, body).
    rotations: Vec<(String, Value)>,
}

type Shared = Arc<Mutex<Pool>>;

fn metadata(r: &str, slot: &Slot) -> Value {
    json!({
        "ref": r, "upstream": "anthropic", "kind": "oauth", "label": null, "source": "pool",
        "status": slot.status, "rate_limited_until": null,
        "expires_at": slot.expires_at.to_rfc3339(),
        "last_used_at": null, "request_count": 0, "error_count": 0,
        "last_error": if slot.status == "invalid" { json!("OAuth token has been revoked.") } else { Value::Null },
        "tags": [],
    })
}

fn api_error(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
}

/// Answers with the slot as it was when the request came in, after
/// `show_delay`: a slow check reports what is by then out of date.
async fn show(State(p): State<Shared>, UrlPath(r): UrlPath<String>) -> impl IntoResponse {
    let (delay, answer) = {
        let mut pool = p.lock().unwrap();
        pool.shows += 1;
        let answer = if pool.show_fails {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "injected")
        } else {
            match pool.slots.get(&r) {
                Some(slot) => (StatusCode::OK, Json(metadata(&r, slot))),
                None => api_error(StatusCode::NOT_FOUND, "not_found", &r),
            }
        };
        (pool.show_delay, answer)
    };
    tokio::time::sleep(delay).await;
    answer
}

async fn refresh(State(p): State<Shared>, UrlPath(r): UrlPath<String>) -> impl IntoResponse {
    let mut pool = p.lock().unwrap();
    pool.refreshes += 1;
    let slot = pool.slots.get_mut(&r).expect("known ref");
    if slot.refresh_refused {
        let message = format!("refreshing credential {r:?} failed: token endpoint answered 400 Bad Request: invalid_grant");
        return api_error(StatusCode::BAD_GATEWAY, "refresh_failed", &message);
    }
    slot.expires_at = Utc::now() + chrono::Duration::hours(8);
    (StatusCode::OK, Json(metadata(&r, slot)))
}

async fn rotate(
    State(p): State<Shared>,
    UrlPath(r): UrlPath<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let mut pool = p.lock().unwrap();
    pool.rotations.push((r.clone(), body));
    if let Some(code) = pool.rotate_fails {
        let status = StatusCode::from_u16(code).unwrap();
        // The daemon's code for material it cannot use.
        let code = if code == 400 {
            "invalid_material"
        } else {
            "injected"
        };
        return api_error(status, code, "rotation refused");
    }
    let slot = pool.slots.get_mut(&r).expect("known ref");
    *slot = Slot::new("active");
    (StatusCode::OK, Json(metadata(&r, slot)))
}

struct Rig {
    keeper: Keeper,
    pool: Shared,
    state_dir: PathBuf,
    /// The keeper's config, as a file for the CLI.
    config: PathBuf,
    client: JanusPluginClient<Channel>,
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

fn fake_claude() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh")
}

/// The fake admin API on `<dir>/admin.sock`.
fn fake_admin(dir: &Path, pool: Shared) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let admin_sock = dir.join("admin.sock");
    let app = Router::new()
        .route("/v1/credentials/{ref}", get(show))
        .route("/v1/credentials/{ref}/refresh", post(refresh))
        .route("/v1/credentials/{ref}/material", put(rotate))
        .with_state(pool);
    let listener = tokio::net::UnixListener::bind(&admin_sock).expect("bind admin");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (admin_sock, task)
}

/// A keeper config for `refs` (each serving instance `claude`).
fn config_text(dir: &Path, admin_sock: &Path, refs: &[&str], extra: &str) -> String {
    let mut text = format!(
        "socket = {socket:?}\nstate_dir = {state:?}\nclaude_bin = {bin:?}\n\
         admin = \"unix:{admin}\"\n{debounce}backoff_min = \"2s\"\nbackoff_max = \"8s\"\n{extra}\n",
        socket = dir.join("run/keeper.sock").display().to_string(),
        state = dir.join("state").display().to_string(),
        bin = fake_claude().display().to_string(),
        admin = admin_sock.display(),
        debounce = if extra.contains("debounce") {
            ""
        } else {
            "debounce = \"200ms\"\n"
        },
    );
    for r in refs {
        text.push_str(&format!(
            "[[credential]]\nref = \"{r}\"\ninstances = [\"claude\"]\n"
        ));
    }
    text
}

struct Setup<'a> {
    refs: &'a [&'a str],
    status: &'a str,
    extra: &'a str,
    mode: &'a str,
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Setup {
            refs: &[REF],
            status: "invalid",
            extra: "",
            mode: "normal",
        }
    }
}

async fn rig(setup: Setup<'_>) -> Rig {
    rig_with(setup, |_, _| {}).await
}

/// A rig whose fake daemon is set up by `prepare` (with the state dir)
/// before the keeper starts.
async fn rig_with(setup: Setup<'_>, prepare: impl FnOnce(&mut Pool, &Path)) -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(state_dir.join("fake-claude")).expect("ctl dir");
    std::fs::write(state_dir.join("fake-claude/mode"), setup.mode).expect("mode");
    let mut pool = Pool::default();
    for r in setup.refs {
        pool.slots.insert((*r).to_owned(), Slot::new(setup.status));
    }
    prepare(&mut pool, &state_dir);
    let pool: Shared = Arc::new(Mutex::new(pool));
    let (admin_sock, admin_task) = fake_admin(dir.path(), pool.clone());
    let mut tasks = vec![admin_task];

    let text = config_text(dir.path(), &admin_sock, setup.refs, setup.extra);
    let config_file = dir.path().join("keeper.toml");
    std::fs::write(&config_file, &text).expect("config file");
    let config = Config::parse(&text).expect("config");
    let socket = config.socket.clone();
    let keeper = Keeper::new(config, Admin::new(Address::Unix(admin_sock), None));
    let (stop, stopped) = watch::channel(false);
    let wait = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|s| *s).await;
    };
    {
        let keeper = keeper.clone();
        let rx = stopped.clone();
        tasks.push(tokio::spawn(async move { keeper.run(wait(rx)).await }));
    }
    {
        let plugin = KeeperPlugin::new(keeper.clone());
        let path = socket.clone();
        let rx = stopped.clone();
        tasks.push(tokio::spawn(async move {
            claude_login_keeper::serve(plugin, &path, None, wait(rx))
                .await
                .expect("serve");
        }));
    }
    let client = JanusPluginClient::new(connect(&socket).await);
    Rig {
        keeper,
        pool,
        state_dir,
        config: config_file,
        client,
        stop,
        tasks,
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

fn event(event_type: &str, payload: Value) -> NotifyEvent {
    NotifyEvent {
        event_type: event_type.into(),
        request_id: "r1".into(),
        correlation_id: String::new(),
        payload_json: payload.to_string(),
    }
}

fn invalid_event_for(r: &str) -> NotifyEvent {
    event(
        "credential_invalid",
        json!({"credential_ref": r, "upstream": "claude", "implementation": "anthropic",
               "status": 401, "reason": "OAuth token has been revoked."}),
    )
}

fn invalid_event() -> NotifyEvent {
    invalid_event_for(REF)
}

/// Poll `f` every 50 ms until it holds, for at most `within`.
async fn eventually(within: Duration, what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The process exists and is not a zombie.
fn alive(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let stat = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            o.status.success() && !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}

impl Rig {
    fn pool(&self) -> MutexGuard<'_, Pool> {
        self.pool.lock().unwrap()
    }

    async fn notify(&mut self, events: Vec<NotifyEvent>) {
        self.client
            .notify(tokio_stream::iter(events))
            .await
            .expect("notify");
    }

    async fn route(&mut self) -> Outcome {
        let req = ResolveRouteRequest {
            request_id: "q1".into(),
            upstream: "claude".into(),
            caller_identity: "agent-1".into(),
            correlation_id: String::new(),
            identity_claims: HashMap::new(),
            implementation: "anthropic".into(),
        };
        self.client
            .resolve_route(req)
            .await
            .expect("resolve")
            .into_inner()
            .outcome
            .expect("outcome")
    }

    fn status(&self) -> Value {
        serde_json::to_value(self.keeper.snapshot()).expect("snapshot")
    }

    fn credential(&self, r: &str) -> Value {
        self.status()["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["ref"] == r)
            .cloned()
            .expect("credential")
    }

    /// Wait until the latest flow's state is `state`.
    async fn wait_flow(&self, state: &str, within: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let flow = self.status()["flow"].clone();
            if flow["state"] == state {
                return flow;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "flow never reached {state}: {flow}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The stub's pids, one per run, in order.
    fn run_pids(&self) -> Vec<i32> {
        std::fs::read_to_string(self.state_dir.join("fake-claude/runs"))
            .map(|s| {
                s.lines()
                    .filter_map(|l| l.split_whitespace().nth(1)?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn runs(&self) -> usize {
        self.run_pids().len()
    }

    fn pending(&self, r: &str) -> PathBuf {
        self.state_dir.join(format!("pending/{r}.json"))
    }

    async fn request(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = claude_login_keeper::http::router(self.keeper.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// A form `POST` to the page, as a browser on it sends one.
    async fn post(&self, path: &str, form: &str, origin: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::post(path)
            .header("host", HOST)
            .header("content-type", "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let (status, _, body) = self
            .request(req.body(Body::from(form.to_owned())).unwrap())
            .await;
        (status, body)
    }

    /// A `POST` as the CLI sends one: it accepts JSON.
    async fn post_json(&self, path: &str, form: &str) -> (StatusCode, Value) {
        let req = Request::post(path)
            .header("host", HOST)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form.to_owned()))
            .unwrap();
        let (status, _, body) = self.request(req).await;
        (status, serde_json::from_str(&body).expect("JSON answer"))
    }

    /// Serve the page on its configured port, as `serve` does.
    fn serve_page(&mut self) {
        let listener =
            std::net::TcpListener::bind(self.keeper.config().http_bind).expect("bind page");
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let mut rx = self.stop.subscribe();
        let keeper = self.keeper.clone();
        self.tasks.push(tokio::spawn(async move {
            let _ = claude_login_keeper::http::serve(keeper, listener, async move {
                let _ = rx.wait_for(|s| *s).await;
            })
            .await;
        }));
    }

    fn security_log(&self) -> String {
        std::fs::read_to_string(self.state_dir.join("run/security.log")).unwrap_or_default()
    }

    async fn stop(self) {
        self.stop.send_replace(true);
        for t in self.tasks.into_iter().skip(1) {
            let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
        }
    }
}

#[tokio::test]
async fn a_burst_of_invalid_events_runs_one_flow_that_rotates_the_new_login_in() {
    let mut rig = rig(Setup::default()).await;
    rig.notify(vec![invalid_event(), invalid_event(), invalid_event()])
        .await;
    let flow = rig.wait_flow("awaiting_browser", STARTUP).await;
    assert_eq!(flow["credential_ref"], REF);
    let auth = flow["auth_url"].as_str().expect("auth_url");
    assert!(
        auth.starts_with("https://claude.com/cai/oauth/authorize?")
            && auth.contains("platform.claude.com")
    );
    // The `open` shim's URL is polled, so it may trail the printed one.
    eventually(Duration::from_secs(2), "local_url", || {
        rig.status()["flow"]["local_url"]
            .as_str()
            .is_some_and(|u| u.contains("localhost"))
    })
    .await;
    assert_eq!(rig.runs(), 1, "three events, one flow");

    // While the only credential is unusable, callers are told how.
    let Outcome::DenyReason(reason) = rig.route().await else {
        panic!("expected a deny")
    };
    assert!(
        reason.starts_with(
            "claude login needs re-authorization: open https://claude.com/cai/oauth/authorize?"
        ),
        "{reason}"
    );

    // A wrong code is re-prompted, not fatal; the right one finishes.
    let (code, _) = rig.post("/login/code", "code=nonsense", None).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    eventually(Duration::from_secs(5), "an Invalid code notice", || {
        rig.status()["flow"]["notice"]
            .as_str()
            .is_some_and(|n| n.starts_with("Invalid code"))
    })
    .await;
    rig.post("/login/code", "code=good%23state", None).await;
    rig.wait_flow("succeeded", Duration::from_secs(10)).await;

    let rotations = rig.pool().rotations.clone();
    assert_eq!(rotations.len(), 1);
    let oauth = &rotations[0].1["material"]["claudeAiOauth"];
    assert!(oauth["accessToken"]
        .as_str()
        .is_some_and(|t| t.starts_with("sk-ant-oat01-FAKE-NEW")));
    assert!(oauth["refreshToken"]
        .as_str()
        .is_some_and(|t| t.starts_with("sk-ant-ort01-FAKE-NEW")));
    assert!(
        !rig.state_dir.join("claude/.credentials.json").exists() && !rig.pending(REF).exists(),
        "the keeper's copy is deleted"
    );
    assert_eq!(rig.credential(REF)["health"], "healthy");
    assert_eq!(rig.route().await, Outcome::UseDefault(true));

    // The child ran isolated: its own HOME and config dir, the keychain
    // shim. The stub asks for the unsuffixed item name, the one the
    // operator's own Claude Code uses; PATH resolves `security` to the
    // shim, which answers 44 and logs the name, so the system binary never
    // saw it.
    let ctl = rig.state_dir.join("fake-claude");
    assert_eq!(
        std::fs::read_to_string(ctl.join("security.path"))
            .unwrap()
            .trim(),
        rig.state_dir.join("bin/security").display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(ctl.join("security.rc"))
            .unwrap()
            .trim(),
        "44"
    );
    assert_eq!(
        rig.security_log(),
        "find-generic-password -s Claude Code-credentials\n"
    );
    let env_file = std::fs::read_dir(rig.state_dir.join("fake-claude"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("env."))
        .expect("env record");
    let env = std::fs::read_to_string(env_file.path()).unwrap();
    let state = rig.state_dir.display().to_string();
    assert!(
        env.lines().any(|l| l == format!("HOME={state}/home")),
        "{env}"
    );
    assert!(
        env.lines()
            .any(|l| l == format!("CLAUDE_CONFIG_DIR={state}/claude")),
        "{env}"
    );
    let allowed = [
        "PATH",
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "BROWSER",
        "TERM",
        "NO_COLOR",
        "DISABLE_AUTOUPDATER",
        "USER",
        "LOGNAME",
        "LANG",
        "TMPDIR",
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
    ];
    for line in env.lines() {
        let key = line.split('=').next().unwrap_or_default();
        assert!(
            allowed.contains(&key),
            "unexpected variable {key} reached claude"
        );
    }
    rig.stop().await;
}

#[tokio::test]
async fn a_failed_login_backs_off_and_retries_once_the_wait_is_over() {
    let mut rig = rig(Setup::default()).await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/code", "code=fail", None).await;
    let flow = rig.wait_flow("failed", Duration::from_secs(10)).await;
    assert_eq!(
        flow["reason"],
        "Login failed: Request failed with status code 400"
    );
    let cred = rig.credential(REF);
    assert_eq!(cred["failed_flows"], 1);
    assert!(
        cred["next_flow_in_secs"].as_u64().is_some_and(|s| s <= 2),
        "{cred}"
    );
    // More events inside the backoff start nothing.
    rig.notify(vec![invalid_event(), invalid_event()]).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.runs(), 1);
    // After the backoff, a second flow.
    let flow = rig.wait_flow("awaiting_browser", STARTUP).await;
    assert_eq!(flow["id"], 2);
    assert_eq!(rig.runs(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn an_unanswered_flow_times_out_and_its_child_is_gone() {
    let mut rig = rig(Setup {
        extra: "login_timeout = \"1s\"",
        mode: "hang",
        ..Setup::default()
    })
    .await;
    rig.notify(vec![invalid_event()]).await;
    let flow = rig.wait_flow("failed", Duration::from_secs(15)).await;
    assert_eq!(flow["reason"], "no authorization within 1s");
    assert!(!alive(rig.run_pids()[0]), "the stub was killed");
    rig.stop().await;
}

#[tokio::test]
async fn an_account_error_on_a_healthy_credential_starts_no_login() {
    let mut rig = rig(Setup {
        status: "active",
        ..Setup::default()
    })
    .await;
    rig.notify(vec![event(
        "account_error",
        json!({"credential_ref": REF, "upstream": "claude", "implementation": "anthropic",
               "status": 403, "reason": "organization disabled"}),
    )])
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.route().await, Outcome::UseDefault(true));
    assert_eq!(rig.pool().refreshes, 0);
    rig.stop().await;
}

/// Review finding 2 and 8: a refused refresh token starts a flow but does
/// not deny while the access token is valid, and the vendor is asked once.
#[tokio::test]
async fn a_refused_refresh_starts_a_flow_serves_until_expiry_and_is_not_retried() {
    let expires_at = Utc::now() + chrono::Duration::seconds(20);
    let mut rig = rig_with(
        Setup {
            status: "active",
            extra: "validate_interval = \"300ms\"",
            ..Setup::default()
        },
        |pool, _| {
            let slot = pool.slots.get_mut(REF).unwrap();
            slot.expires_at = expires_at;
            slot.refresh_refused = true;
        },
    )
    .await;
    let flow = rig.wait_flow("awaiting_browser", STARTUP).await;
    assert_eq!(flow["credential_ref"], REF);
    let cred = rig.credential(REF);
    assert_eq!(cred["health"], "refresh_refused", "{cred}");
    assert!(cred["reason"]
        .as_str()
        .is_some_and(|r| r.contains("invalid_grant")));
    assert_eq!(
        rig.route().await,
        Outcome::UseDefault(true),
        "the access token still works"
    );
    // Several validations later the token endpoint was still asked once.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(rig.pool().refreshes, 1);
    assert_eq!(rig.status()["flow"]["state"], "awaiting_browser");
    // Past the expiry, the credential is unusable.
    let wait = (expires_at - Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(wait + Duration::from_millis(200)).await;
    assert!(matches!(rig.route().await, Outcome::DenyReason(_)));
    rig.stop().await;
}

/// Review finding 3: a credential healed outside the keeper during the
/// debounce starts no flow, and the pre-flow check trusts the daemon.
#[tokio::test]
async fn a_credential_healed_during_the_debounce_starts_no_flow() {
    let mut rig = rig(Setup {
        extra: "debounce = \"1s\"",
        ..Setup::default()
    })
    .await;
    rig.notify(vec![invalid_event()]).await;
    assert!(matches!(rig.route().await, Outcome::DenyReason(_)));
    rig.pool().slots.insert(REF.into(), Slot::new("active"));
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.route().await, Outcome::UseDefault(true));
    let cred = rig.credential(REF);
    assert_eq!(cred["health"], "healthy");
    assert_eq!(cred["next_flow_in_secs"], Value::Null);
    rig.stop().await;
}

/// Review finding 3: checks keep running during a flow, and a healthy one
/// ends the automatic flow and lifts the deny.
#[tokio::test]
async fn a_credential_healed_during_a_flow_ends_the_flow() {
    let mut rig = rig(Setup {
        extra: "validate_interval = \"300ms\"",
        ..Setup::default()
    })
    .await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    assert!(matches!(rig.route().await, Outcome::DenyReason(_)));
    rig.pool().slots.insert(REF.into(), Slot::new("active"));
    let flow = rig.wait_flow("cancelled", Duration::from_secs(5)).await;
    assert_eq!(flow["reason"], "the credential is usable again");
    assert_eq!(rig.route().await, Outcome::UseDefault(true));
    let cred = rig.credential(REF);
    assert_eq!(cred["failed_flows"], 0);
    assert_eq!(cred["next_flow_in_secs"], Value::Null);
    let pid = rig.run_pids()[0];
    eventually(Duration::from_secs(3), "the stub ends", || !alive(pid)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.runs(), 1);
    rig.stop().await;
}

/// Review finding 4: a check the admin API did not answer confirms nothing.
#[tokio::test]
async fn a_failed_check_is_not_a_verdict() {
    let mut rig = rig_with(Setup::default(), |pool, _| pool.show_fails = true).await;
    rig.notify(vec![invalid_event()]).await;
    eventually(
        Duration::from_secs(3),
        "a backoff after the failed check",
        || rig.credential(REF)["failed_flows"] == 1,
    )
    .await;
    let cred = rig.credential(REF);
    assert_eq!(cred["checked_at"], Value::Null, "{cred}");
    assert_eq!(rig.runs(), 0, "no flow without the admin surface");
    {
        let mut pool = rig.pool();
        pool.show_fails = false;
        pool.slots.insert(REF.into(), Slot::new("active"));
    }
    eventually(Duration::from_secs(5), "healthy after the retry", || {
        rig.credential(REF)["health"] == "healthy"
    })
    .await;
    assert!(rig.credential(REF)["checked_at"].is_string());
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.route().await, Outcome::UseDefault(true));
    rig.stop().await;
}

/// Review finding 5, pending recovery and the `/status` after it: a login
/// whose rotation failed is kept, retried without a new flow, and the flow
/// then reads `succeeded`.
#[tokio::test]
async fn a_login_whose_rotation_failed_is_kept_and_rotated_later_without_a_new_flow() {
    let mut rig = rig_with(Setup::default(), |pool, _| pool.rotate_fails = Some(503)).await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/code", "code=good", None).await;
    let flow = rig.wait_flow("failed", Duration::from_secs(10)).await;
    assert!(flow["reason"]
        .as_str()
        .is_some_and(|r| r.starts_with("logged in, but the rotation failed")));
    assert!(rig.pending(REF).exists());
    // The retry after the backoff fails again: the file is kept, no flow.
    eventually(Duration::from_secs(5), "a second rotation", || {
        rig.pool().rotations.len() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.runs(), 1);
    assert!(rig.pending(REF).exists());
    rig.pool().rotate_fails = None;
    eventually(Duration::from_secs(10), "the kept login rotated", || {
        rig.credential(REF)["health"] == "healthy"
    })
    .await;
    assert_eq!(rig.runs(), 1);
    assert!(!rig.pending(REF).exists());
    let rotations = rig.pool().rotations.clone();
    assert_eq!(
        rotations[0].1,
        rotations.last().unwrap().1,
        "the same login"
    );
    assert_eq!(rig.status()["flow"]["state"], "succeeded");
    assert_eq!(rig.route().await, Outcome::UseDefault(true));
    rig.stop().await;
}

/// A login the daemon refuses as material is dropped; the next attempt
/// runs a flow instead of retrying it forever.
#[tokio::test]
async fn a_login_the_daemon_refuses_is_discarded() {
    let mut rig = rig_with(Setup::default(), |pool, _| pool.rotate_fails = Some(400)).await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/code", "code=good", None).await;
    rig.wait_flow("failed", Duration::from_secs(10)).await;
    assert!(!rig.pending(REF).exists());
    let flow = rig.wait_flow("awaiting_browser", STARTUP).await;
    assert_eq!(flow["id"], 2);
    assert_eq!(rig.pool().rotations.len(), 1);
    rig.stop().await;
}

/// Review finding 1: each credential's login goes into that credential
/// only, also when one is kept for a later rotation.
#[tokio::test]
async fn two_credentials_never_take_each_others_login() {
    let mut rig = rig_with(
        Setup {
            refs: &["sub-a", "sub-b"],
            ..Setup::default()
        },
        |pool, _| {
            pool.rotate_fails = Some(503);
            pool.slots.insert("sub-b".into(), Slot::new("active"));
        },
    )
    .await;
    rig.notify(vec![invalid_event_for("sub-a")]).await;
    let flow = rig.wait_flow("awaiting_browser", STARTUP).await;
    assert_eq!(flow["credential_ref"], "sub-a");
    rig.post("/login/code", "credential=sub-a&code=good", None)
        .await;
    rig.wait_flow("failed", Duration::from_secs(10)).await;
    assert!(rig.pending("sub-a").exists());

    // sub-b's attempt finds no login of its own and runs its own flow.
    rig.pool()
        .slots
        .insert("sub-b".into(), Slot::new("invalid"));
    rig.notify(vec![invalid_event_for("sub-b")]).await;
    eventually(Duration::from_secs(10), "sub-b's flow", || {
        let flow = rig.status()["flow"].clone();
        flow["credential_ref"] == "sub-b" && flow["state"] == "awaiting_browser"
    })
    .await;
    assert!(rig.pool().rotations.iter().all(|(r, _)| r == "sub-a"));
    rig.pool().rotate_fails = None;
    rig.post("/login/code", "credential=sub-b&code=good", None)
        .await;
    eventually(Duration::from_secs(15), "both rotated", || {
        rig.credential("sub-a")["health"] == "healthy"
            && rig.credential("sub-b")["health"] == "healthy"
    })
    .await;
    let pids = rig.run_pids();
    assert_eq!(pids.len(), 2, "sub-a's kept login needed no second flow");
    let token = |r: &str| {
        let rotations = rig.pool().rotations.clone();
        let (_, body) = rotations
            .iter()
            .rev()
            .find(|(x, _)| x == r)
            .unwrap()
            .clone();
        body["material"]["claudeAiOauth"]["accessToken"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(token("sub-a"), format!("sk-ant-oat01-FAKE-NEW-{}", pids[0]));
    assert_eq!(token("sub-b"), format!("sk-ant-oat01-FAKE-NEW-{}", pids[1]));
    assert!(!rig.pending("sub-a").exists() && !rig.pending("sub-b").exists());
    rig.stop().await;
}

/// Review finding 7: once an operator's start is taken, a second start is
/// refused, also while the pre-flow check is still running.
#[tokio::test]
async fn an_operator_start_is_taken_once() {
    let rig = rig_with(
        Setup {
            status: "active",
            ..Setup::default()
        },
        |pool, _| pool.show_delay = Duration::from_millis(800),
    )
    .await;
    // Let the startup check finish first.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let (code, _) = rig
        .post("/login/start", "credential=claude-sub", None)
        .await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (code, body) = rig
        .post("/login/start", "credential=claude-sub", None)
        .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/cancel", "", None).await;
    rig.wait_flow("cancelled", Duration::from_secs(10)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(rig.runs(), 1);
    rig.stop().await;
}

/// Review finding 10, and round 2 finding 7: an operator's cancel is not a
/// failure; the credential still needs a login, so the next automatic
/// attempt is scheduled at once, after a hold of `backoff_max`.
#[tokio::test]
async fn an_operator_cancel_pauses_automatic_flows() {
    let mut rig = rig(Setup {
        extra: "validate_interval = \"1h\"",
        ..Setup::default()
    })
    .await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/cancel", "", None).await;
    let flow = rig.wait_flow("cancelled", Duration::from_secs(10)).await;
    assert_eq!(flow["reason"], "cancelled by the operator");
    let mut next = None;
    eventually(
        Duration::from_secs(2),
        "the next attempt is scheduled",
        || {
            next = rig.credential(REF)["next_flow_in_secs"].as_u64();
            next.is_some()
        },
    )
    .await;
    assert!(
        next.is_some_and(|s| s >= 6),
        "held for backoff_max: {next:?}"
    );
    assert_eq!(rig.credential(REF)["failed_flows"], 0);
    rig.notify(vec![invalid_event()]).await;
    let next = rig.credential(REF)["next_flow_in_secs"].as_u64();
    assert!(next.is_some_and(|s| s >= 6), "still held: {next:?}");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(rig.runs(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn the_page_checks_host_and_origin_and_answers_json_to_the_cli() {
    let rig = rig(Setup {
        status: "active",
        extra: "validate_interval = \"300ms\"",
        ..Setup::default()
    })
    .await;
    // DNS rebinding: a foreign Host is refused on every route.
    for path in ["/", "/status"] {
        let req = Request::get(path)
            .header("host", "attacker.example:27711")
            .body(Body::empty())
            .unwrap();
        let (code, headers, _) = rig.request(req).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(headers["x-frame-options"], "DENY");
    }
    let (code, body) = rig
        .post(
            "/login/start",
            "credential=claude-sub",
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    let (code, body) = rig.post("/login/start", "credential=nope", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
    let (code, _) = rig
        .post(
            "/login/start",
            "credential=claude-sub",
            Some("http://127.0.0.1:27711"),
        )
        .await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    // Forced: runs although the credential is healthy, and healthy checks
    // do not stop it.
    rig.wait_flow("awaiting_browser", STARTUP).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(rig.status()["flow"]["state"], "awaiting_browser");
    let busy = Request::post("/login/start")
        .header("host", "localhost:27711")
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("credential=claude-sub"))
        .unwrap();
    let (code, _, body) = rig.request(busy).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"]["code"], "busy");
    assert_eq!(body["error"]["credential_ref"], REF);
    let page = Request::get("/")
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let (code, headers, html) = rig.request(page).await;
    assert_eq!(code, StatusCode::OK);
    assert!(headers["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'none'"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(
        html.contains("https://claude.com/cai/oauth/authorize?") && html.contains("Submit code")
    );
    let (code, _) = rig.post("/login/cancel", "", None).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    rig.wait_flow("cancelled", Duration::from_secs(10)).await;
    assert_eq!(rig.pool().rotations.len(), 0);
    rig.stop().await;
}

/// Start a stand-in `claude auth login` as a previous keeper would have
/// left it: its own process group, its pid in `<state>/run/login.pid`.
fn leave_orphan(state_dir: &Path, program: &Path, args: &[&str]) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(state_dir.join("run")).unwrap();
    std::fs::create_dir_all(state_dir.join("claude")).unwrap();
    let child = std::process::Command::new(program)
        .args(args)
        .env("CLAUDE_CONFIG_DIR", state_dir.join("claude"))
        .stdout(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn orphan");
    std::fs::write(state_dir.join("run/login.pid"), child.id().to_string()).unwrap();
    child
}

#[tokio::test]
async fn a_flow_left_by_a_killed_keeper_is_ended_at_startup() {
    let mut orphan = None;
    let rig = rig_with(
        Setup {
            status: "active",
            mode: "hang",
            ..Setup::default()
        },
        |_, state| {
            orphan = Some(leave_orphan(
                state,
                &fake_claude(),
                &["auth", "login", "--claudeai"],
            ))
        },
    )
    .await;
    let mut orphan = orphan.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while orphan.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the orphan was not ended"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!rig.state_dir.join("run/login.pid").exists());
    rig.stop().await;
}

#[tokio::test]
async fn a_pid_file_naming_another_process_is_left_alone() {
    let mut other = None;
    let rig = rig_with(
        Setup {
            status: "active",
            ..Setup::default()
        },
        |_, state| other = Some(leave_orphan(state, Path::new("/bin/sleep"), &["30"])),
    )
    .await;
    let mut other = other.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        other.try_wait().unwrap().is_none(),
        "an unrelated process was killed"
    );
    assert!(!rig.state_dir.join("run/login.pid").exists());
    other.kill().unwrap();
    let _ = other.wait();
    rig.stop().await;
}

/// The `claude-login-keeper` binary, with a fake daemon of one invalid
/// credential and a config file using a free page port.
struct Cli {
    pool: Shared,
    config: PathBuf,
    state_dir: PathBuf,
    _admin: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

async fn cli() -> Cli {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(state_dir.join("fake-claude")).unwrap();
    let mut pool = Pool::default();
    pool.slots.insert(REF.into(), Slot::new("invalid"));
    let pool: Shared = Arc::new(Mutex::new(pool));
    let (admin_sock, admin) = fake_admin(dir.path(), pool.clone());
    let port = free_port();
    let text = format!(
        "http_bind = \"127.0.0.1:{port}\"\n{}",
        config_text(dir.path(), &admin_sock, &[REF], "")
    );
    let config = dir.path().join("keeper.toml");
    std::fs::write(&config, text).unwrap();
    Cli {
        pool,
        config,
        state_dir,
        _admin: admin,
        _dir: dir,
    }
}

impl Cli {
    fn login(&self, credential: &str) -> tokio::process::Command {
        keeper_cmd(&["login", "--credential", credential], &self.config)
    }
}

/// `claude-login-keeper <args> --config <config>`, with piped stdio.
fn keeper_cmd(args: &[&str], config: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_claude-login-keeper"));
    cmd.args(args)
        .arg("--config")
        .arg(config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// A free loopback port.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Read `child`'s stdout until a line contains `needle`; returns all of it.
async fn read_until(stdout: &mut tokio::process::ChildStdout, needle: &str) -> String {
    use tokio::io::AsyncReadExt;
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + STARTUP;
    while !seen.contains(needle) {
        let n = tokio::time::timeout_at(deadline, stdout.read(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("never printed {needle:?}: {seen}"))
            .unwrap();
        assert!(n > 0, "stdout closed before {needle:?}: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    seen
}

/// Review finding 9: Ctrl-C on `login` ends the in-process keeper's flow
/// properly: the child is gone and so is its pid file.
#[tokio::test]
async fn ctrl_c_on_the_login_cli_ends_its_flow_cleanly() {
    let cli = cli().await;
    let mut child = cli.login(REF).spawn().expect("spawn");
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "Paste code").await;
    let pid_file = cli.state_dir.join("run/login.pid");
    let stub: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: a signal to the child this test spawned.
    unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) };
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("exits")
        .unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(!pid_file.exists(), "the flow cleaned up after itself");
    eventually(Duration::from_secs(3), "the stub ends", || !alive(stub)).await;
    assert_eq!(cli.pool.lock().unwrap().rotations.len(), 0);
}

#[tokio::test]
async fn the_login_cli_reports_a_failed_login_once_and_exits_1() {
    let cli = cli().await;
    let mut child = cli.login(REF).spawn().expect("spawn");
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "Paste code").await;
    {
        use tokio::io::AsyncWriteExt;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"fail-on-purpose\n").await.unwrap();
        stdin.flush().await.unwrap();
    }
    let out = read_until(&mut stdout, "status code 400").await;
    assert!(
        out.contains("Not logged in: Login failed: Request failed with status code 400"),
        "{out}"
    );
    assert!(!out.contains("Login failed: Login failed"), "{out}");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("exits")
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

#[tokio::test]
async fn the_login_cli_refuses_an_unknown_credential_and_a_bad_config_in_one_line() {
    let cli = cli().await;
    let out = cli.login("nope").output().await.unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        err.trim(),
        "claude-login-keeper: credential \"nope\" is not in the config's [[credential]] list"
    );
    assert!(!cli.state_dir.join("run").exists(), "no keeper was started");

    std::fs::write(&cli.config, "socket = 3\n").unwrap();
    let out = cli.login(REF).output().await.unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("claude-login-keeper: "), "{err}");
    assert_eq!(err.trim().lines().count(), 1, "{err}");
    assert!(err.contains("line 1, column 10"), "{err}");
}

/// Round 2 finding 1: `login` follows the flow its start names, not the
/// one before it. Here an automatic attempt is still checking when the CLI
/// starts, after an earlier flow for the same credential failed.
#[tokio::test]
async fn the_login_cli_follows_its_own_flow_not_the_previous_one() {
    let port = free_port();
    let extra = format!("http_bind = \"127.0.0.1:{port}\"\nvalidate_interval = \"1h\"");
    let mut rig = rig(Setup {
        extra: &extra,
        ..Setup::default()
    })
    .await;
    rig.serve_page();
    // Flow 1 runs at startup and fails.
    rig.wait_flow("awaiting_browser", STARTUP).await;
    assert!(rig.keeper.submit_code(REF, "fail"));
    rig.wait_flow("failed", Duration::from_secs(10)).await;
    // The retry's check takes a while.
    rig.pool().show_delay = Duration::from_secs(3);
    eventually(Duration::from_secs(10), "the retry is checking", || {
        rig.keeper.running_credential().as_deref() == Some(REF)
    })
    .await;
    assert_eq!(rig.status()["flow"]["id"], 1, "no new flow yet");

    let mut child = keeper_cmd(&["login", "--credential", REF], &rig.config)
        .spawn()
        .expect("spawn");
    let mut stdout = child.stdout.take().unwrap();
    let out = read_until(&mut stdout, "Paste code").await;
    assert!(!out.contains("Not logged in"), "{out}");
    assert_eq!(rig.status()["flow"]["id"], 2);
    {
        use tokio::io::AsyncWriteExt;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"good\n").await.unwrap();
        stdin.flush().await.unwrap();
    }
    read_until(&mut stdout, "Logged in; credential claude-sub rotated.").await;
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("exits")
        .unwrap();
    assert_eq!(status.code(), Some(0));
    rig.stop().await;
}

/// Round 2 finding 2: a 2xx `call_completed` does not lift a
/// `credential_invalid`; only an admin check clears it.
#[tokio::test]
async fn a_success_after_credential_invalid_keeps_the_deny() {
    let mut rig = rig(Setup {
        status: "active",
        extra: "validate_interval = \"1h\"\ndebounce = \"1h\"",
        ..Setup::default()
    })
    .await;
    eventually(Duration::from_secs(5), "the startup check", || {
        rig.credential(REF)["health"] == "healthy"
    })
    .await;
    rig.notify(vec![
        invalid_event(),
        event(
            "call_completed",
            json!({"caller_identity": "agent-1", "upstream": "claude", "implementation": "anthropic",
                   "status": 200, "credential_ref": REF, "duration_ms": 12, "usage": {}}),
        ),
    ])
    .await;
    assert_eq!(rig.credential(REF)["health"], "needs_login");
    assert!(matches!(rig.route().await, Outcome::DenyReason(_)));
    rig.stop().await;
}

/// Round 2 finding 3: `serve` that cannot bind its socket exits at once,
/// with the error on one line.
#[tokio::test]
async fn serve_exits_at_once_when_its_socket_cannot_be_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The socket's directory is a file.
    std::fs::write(dir.path().join("run"), "").unwrap();
    let text = format!(
        "http_bind = \"127.0.0.1:{}\"\n{}",
        free_port(),
        config_text(dir.path(), &dir.path().join("no-admin.sock"), &[REF], "")
    );
    let config = dir.path().join("keeper.toml");
    std::fs::write(&config, text).unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        keeper_cmd(&["serve"], &config).output(),
    )
    .await
    .expect("serve exits")
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    let last = err.trim().lines().last().unwrap_or_default();
    assert!(last.starts_with("claude-login-keeper: "), "{err}");
}

/// Round 2 finding 4: a start takes the slot at once, so a second start
/// (for any credential) is busy; cancel and code reach only the named
/// credential's flow.
#[tokio::test]
async fn starts_and_cancels_are_per_credential() {
    let rig = rig_with(
        Setup {
            refs: &["sub-a", "sub-b"],
            status: "active",
            extra: "validate_interval = \"1h\"",
            ..Setup::default()
        },
        |pool, _| pool.show_delay = Duration::from_millis(800),
    )
    .await;
    eventually(Duration::from_secs(5), "the startup checks", || {
        rig.credential("sub-b")["health"] == "healthy"
    })
    .await;
    let (code, body) = rig.post_json("/login/start", "credential=sub-a").await;
    assert_eq!(code, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["flow_id"], 1);
    // sub-a's attempt is still in its check.
    let (code, body) = rig.post_json("/login/start", "credential=sub-b").await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "busy");
    assert_eq!(body["error"]["credential_ref"], "sub-a");
    assert_eq!(body["error"]["flow_id"], 1);

    rig.wait_flow("awaiting_browser", STARTUP).await;
    for path in ["/login/cancel", "/login/code"] {
        let (code, body) = rig.post_json(path, "credential=sub-b&code=good").await;
        assert_eq!(code, StatusCode::CONFLICT, "{path}: {body}");
        assert_eq!(body["error"]["code"], "no_flow");
        assert_eq!(body["error"]["credential_ref"], "sub-a");
    }
    let (code, body) = rig.post_json("/login/cancel", "").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "two credentials: {body}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let flow = rig.status()["flow"].clone();
    assert_eq!(flow["state"], "awaiting_browser", "{flow}");
    assert_eq!(rig.credential("sub-b")["next_flow_in_secs"], Value::Null);

    let (code, _) = rig.post_json("/login/cancel", "credential=sub-a").await;
    assert_eq!(code, StatusCode::OK);
    rig.wait_flow("cancelled", Duration::from_secs(10)).await;
    assert_eq!(rig.pool().rotations.len(), 0);
    rig.stop().await;
}

/// Round 2 finding 6: a check that started before a rotation does not
/// overwrite its result when it answers late.
#[tokio::test]
async fn a_late_check_does_not_overwrite_a_rotation() {
    let mut rig = rig_with(
        Setup {
            extra: "validate_interval = \"300ms\"",
            ..Setup::default()
        },
        |pool, _| pool.show_delay = Duration::from_millis(1000),
    )
    .await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    // A validation check that saw `invalid` is in flight now.
    let shows = rig.pool().shows;
    eventually(Duration::from_secs(3), "a check in flight", || {
        rig.pool().shows > shows
    })
    .await;
    rig.post("/login/code", "code=good", None).await;
    rig.wait_flow("succeeded", Duration::from_secs(10)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(rig.credential(REF)["health"], "healthy");
        assert_eq!(rig.route().await, Outcome::UseDefault(true));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rig.runs(), 1);
    rig.stop().await;
}

/// Round 2 finding 7: a `credential_invalid` that arrives while an
/// attempt's check runs stands over the check's older answer, and the
/// attempt goes on to a flow.
#[tokio::test]
async fn an_event_during_an_attempts_check_is_not_overwritten() {
    let mut rig = rig_with(
        Setup {
            status: "active",
            extra: "validate_interval = \"1h\"",
            ..Setup::default()
        },
        |pool, _| pool.show_delay = Duration::from_millis(1000),
    )
    .await;
    eventually(Duration::from_secs(5), "the startup check", || {
        rig.credential(REF)["health"] == "healthy"
    })
    .await;
    // An account error asks for a check: an attempt starts, sees `active`.
    rig.notify(vec![event(
        "account_error",
        json!({"credential_ref": REF, "upstream": "claude", "implementation": "anthropic",
               "status": 403, "reason": "organization disabled"}),
    )])
    .await;
    eventually(Duration::from_secs(5), "the attempt's check", || {
        rig.pool().shows >= 2
    })
    .await;
    rig.pool().slots.insert(REF.into(), Slot::new("invalid"));
    rig.notify(vec![invalid_event()]).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(rig.credential(REF)["health"], "needs_login");
    assert!(matches!(rig.route().await, Outcome::DenyReason(_)));
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.stop().await;
}

/// Round 2 finding 8: only `400 invalid_material` discards a kept login; a
/// 401 or a 409 keeps it for the retry.
#[tokio::test]
async fn a_rotation_refused_for_another_reason_keeps_the_login() {
    for status in [401, 409] {
        let mut rig = rig_with(Setup::default(), |pool, _| pool.rotate_fails = Some(status)).await;
        rig.notify(vec![invalid_event()]).await;
        rig.wait_flow("awaiting_browser", STARTUP).await;
        rig.post("/login/code", "code=good", None).await;
        rig.wait_flow("failed", Duration::from_secs(10)).await;
        assert!(rig.pending(REF).exists(), "{status}: kept");
        eventually(Duration::from_secs(5), "the retry rotates it again", || {
            rig.pool().rotations.len() == 2
        })
        .await;
        assert_eq!(rig.runs(), 1, "{status}: no second flow");
        rig.stop().await;
    }
}

/// Round 2 finding 9: a kept login is deleted when a check finds the
/// credential healthy; it is not rotated over working material.
#[tokio::test]
async fn a_healthy_check_discards_a_kept_login() {
    let mut rig = rig_with(
        Setup {
            extra: "validate_interval = \"300ms\"",
            ..Setup::default()
        },
        |pool, _| pool.rotate_fails = Some(503),
    )
    .await;
    rig.notify(vec![invalid_event()]).await;
    rig.wait_flow("awaiting_browser", STARTUP).await;
    rig.post("/login/code", "code=good", None).await;
    rig.wait_flow("failed", Duration::from_secs(10)).await;
    assert!(rig.pending(REF).exists());
    // Healed outside the keeper before the retry.
    rig.pool().slots.insert(REF.into(), Slot::new("active"));
    eventually(Duration::from_secs(3), "the kept login is deleted", || {
        !rig.pending(REF).exists()
    })
    .await;
    assert_eq!(rig.credential(REF)["health"], "healthy");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(rig.pool().rotations.len(), 1, "not rotated again");
    assert_eq!(rig.runs(), 1);
    rig.stop().await;
}
