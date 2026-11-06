//! The admin surface end to end: real listeners (the unix socket, TCP, TLS,
//! the UI listener) over the real credential pool, store and plugin client.
//! See `docs/admin-surface` and `docs/architecture#admin-surface`.

#[path = "admin/support.rs"]
mod support;

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use janus_core::contract::credential::{
    CredentialChoice, CredentialPool, PoolError, UpstreamFeedback,
};
use janus_core::contract::identity::ExtractionError;
use janus_core::contract::plugin::{
    JanusPlugin, NotifyEvent, PluginError, ResolveRoute, RouteOutcome,
};
use janus_core::contract::registry::{OpaqueRegistry, TokenHash};
use janus_core::contract::request_log::Outcome;
use janus_core::contract::selection::{Pick, Selection};
use janus_core::contract::upstream::UpstreamKind;
use janus_daemon::admin::{AdminError, PING_INTERVAL};
use serde_json::{json, Value};
use support::{accepts, free_port, record, write_file, Builder, Client, Logs};

const UPSTREAMS: &str = r#"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }

[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "https://openrouter.ai/api"

[[upstream.route]]
path = "/v1/models"
identity = { method = "none" }

[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "https://api.anthropic.com"
"#;

const ADMIN_TCP: &str = "[admin]\nbind = \"127.0.0.1:0\"\n";

const WAIT: Duration = Duration::from_secs(5);

async fn add_openrouter(client: &Client, key: &str) -> Value {
    let reply = client
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "material": { "api_key": key } })),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    reply.json()
}

fn assert_no_access_control_headers(headers: &http::HeaderMap) {
    for name in headers.keys() {
        assert!(
            !name.as_str().starts_with("access-control-"),
            "CORS header {name} answered"
        );
    }
}

// ---------------------------------------------------------------------------
// Listeners and access control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unix_socket_mode_0600_and_needs_no_token() {
    let d = Builder::new("").start().await;
    let path = d.handle().socket_path().to_path_buf();
    assert_eq!(path, d.data_dir().join("admin.sock"));
    let meta = std::fs::symlink_metadata(&path).expect("socket exists");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let owner = std::fs::metadata(d.data_dir()).expect("dir").uid();
    assert_eq!(meta.uid(), owner);
    // No leftovers from the staged bind.
    let names: Vec<String> = std::fs::read_dir(d.data_dir())
        .expect("ls")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.starts_with(".admin-sock")),
        "{names:?}"
    );

    let reply = d.socket().get("/v1/status").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(
        reply.json()["listeners"]["admin_socket"],
        path.display().to_string()
    );

    d.stop().await;
    assert!(!path.exists(), "socket removed at shutdown");
}

#[tokio::test]
async fn stale_socket_is_replaced_and_a_live_one_refuses_a_second_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("admin.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).expect("bind"));
    assert!(sock.exists(), "stale socket left behind");

    let d = Builder::in_dir(dir, "").start().await;
    assert_eq!(d.socket().get("/v1/status").await.status, 200);

    let second = Builder::sharing(d.data_dir(), "").try_start().await;
    match second {
        Err((AdminError::SocketInUse { path }, _)) => assert_eq!(path, sock),
        Err((e, _)) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("second daemon started over a live socket"),
    }
    assert_eq!(
        d.socket().get("/v1/status").await.status,
        200,
        "first still serves"
    );
    let dir = d.stop().await.expect("owned");

    std::fs::write(&sock, "not a socket").expect("write");
    match Builder::in_dir(dir, "").try_start().await {
        Err((AdminError::Socket { reason, .. }, Some(dir))) => {
            assert!(reason.contains("not a socket"), "{reason}");
            let left = std::fs::read_to_string(dir.path().join("admin.sock")).expect("kept");
            assert_eq!(left, "not a socket", "a non-socket file is never removed");
        }
        Err((e, _)) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("started over a regular file"),
    }
}

#[tokio::test]
async fn admin_requires_token_on_tcp_loopback() {
    let d = Builder::new(&format!("{ADMIN_TCP}{UPSTREAMS}"))
        .start()
        .await;
    let addr = d.handle().admin_addr().expect("bound");
    assert!(addr.ip().is_loopback());
    let token = d.token();

    for client in [
        d.tcp(None),
        d.tcp(Some("not-the-token")),
        d.tcp(Some(&format!("{token}x"))),
    ] {
        for path in ["/v1/status", "/v1/credentials", "/v1/does-not-exist"] {
            let reply = client.get(path).await;
            assert_eq!(reply.status, 401, "{path}: {}", reply.text());
            assert_eq!(reply.error_code(), "unauthorized");
            assert_eq!(reply.headers["www-authenticate"], "Bearer");
        }
    }
    let in_query = d.tcp(None).get(&format!("/v1/status?token={token}")).await;
    assert_eq!(
        in_query.status, 401,
        "the query string never carries the token"
    );
    let basic = d
        .tcp(None)
        .send(
            "GET",
            "/v1/status",
            &[("authorization", &format!("Basic {token}"))],
            None,
        )
        .await;
    assert_eq!(basic.status, 401);

    // A caller identity is a different secret and never admin.
    let created = d
        .socket()
        .post("/v1/identities", Some(json!({ "identity": "sandbox-1" })))
        .await
        .json();
    let caller_token = created["token"].as_str().expect("token");
    assert_eq!(
        d.tcp(Some(caller_token)).get("/v1/status").await.status,
        401
    );

    let ok = d.tcp(Some(&token)).get("/v1/status").await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    let lower = d
        .tcp(None)
        .send(
            "GET",
            "/v1/status",
            &[("authorization", &format!("bearer {token}"))],
            None,
        )
        .await;
    assert_eq!(lower.status, 200, "the scheme is case-insensitive");
    assert_eq!(
        d.socket().get("/v1/status").await.status,
        200,
        "socket needs no token"
    );
}

#[tokio::test]
async fn admin_token_file_is_generated_0600_and_reused_across_restarts() {
    let b = Builder::new(ADMIN_TCP);
    let token_path = b.dir().join("admin-token");
    assert!(!token_path.exists());
    let d = b.start().await;
    assert_eq!(d.handle().token_path(), token_path);
    let meta = std::fs::metadata(&token_path).expect("generated");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let token = d.token();
    assert_eq!(URL_SAFE_NO_PAD.decode(&token).expect("base64url").len(), 32);
    assert_eq!(d.tcp(Some(&token)).get("/v1/status").await.status, 200);
    let dir = d.stop().await.expect("owned");

    let d = Builder::in_dir(dir, ADMIN_TCP).start().await;
    assert_eq!(d.token(), token, "reused, not regenerated");
    assert_eq!(d.tcp(Some(&token)).get("/v1/status").await.status, 200);
    let dir = d.stop().await.expect("owned");

    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
    match Builder::in_dir(dir, ADMIN_TCP).try_start().await {
        Err((AdminError::Token { reason, .. }, Some(dir))) => {
            assert!(reason.contains("0640"), "{reason}");
            assert!(!dir.path().join("admin.sock").exists(), "nothing bound");
        }
        Err((e, _)) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("started with a group-readable token file"),
    }
}

#[tokio::test]
async fn nonloopback_without_tls_refuses_start() {
    let cases = [
        (
            "[admin]\nbind = \"0.0.0.0:{port}\"\nallow_non_loopback = true\n",
            "admin.tls",
        ),
        (
            "[admin]\nbind = \"0.0.0.0:{port}\"\n",
            "admin.allow_non_loopback",
        ),
        ("[ui]\nbind = \"0.0.0.0:{port}\"\n", "ui.tls"),
    ];
    for (config, mentions) in cases {
        let port = free_port();
        let b = Builder::new(&config.replace("{port}", &port.to_string()));
        match b.try_start().await {
            Err((AdminError::Config(e), Some(dir))) => {
                assert!(e.to_string().contains(mentions), "{config}: {e}");
                assert!(!dir.path().join("admin.sock").exists(), "socket bound");
                assert!(!dir.path().join("admin-token").exists(), "token written");
                std::net::TcpListener::bind(("0.0.0.0", port)).expect("port left free");
            }
            Err((e, _)) => panic!("{config}: wrong refusal: {e}"),
            Ok(_) => panic!("{config}: started"),
        }
    }
}

#[tokio::test]
async fn nonloopback_with_allow_plaintext_serves_and_still_requires_the_token() {
    let d = Builder::new(
        "[admin]\nbind = \"0.0.0.0:0\"\nallow_non_loopback = true\nallow_plaintext = true\n",
    )
    .start()
    .await;
    let port = d.handle().admin_addr().expect("bound").port();
    let base = format!("http://127.0.0.1:{port}");
    let token = d.token();
    assert_eq!(
        Client::http(base.clone(), None, None)
            .get("/v1/status")
            .await
            .status,
        401
    );
    let ok = Client::http(base, Some(&token), None)
        .get("/v1/status")
        .await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json()["listeners"]["admin_tcp"]["tls"], false);
}

/// A CA and a `localhost` / `127.0.0.1` server certificate it issued:
/// `(ca_pem, cert_pem, key_pem)`.
fn server_pki() -> (String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("ca");
    let key = KeyPair::generate().expect("key");
    let leaf = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("signed");
    (ca.pem(), leaf.pem(), key.serialize_pem())
}

#[tokio::test]
async fn admin_tls_listener_serves_https_and_requires_the_token() {
    let b = Builder::new(
        "[admin]\nbind = \"127.0.0.1:0\"\ntls = { cert = \"{dir}/admin.pem\", key = \"{dir}/admin-key.pem\" }\n",
    );
    let (ca, cert, key) = server_pki();
    write_file(&b.dir().join("admin.pem"), &cert, 0o644);
    write_file(&b.dir().join("admin-key.pem"), &key, 0o600);
    let d = b.start().await;
    let port = d.handle().admin_addr().expect("bound").port();
    let base = format!("https://localhost:{port}");
    let token = d.token();

    let refused = Client::http(base.clone(), None, Some(&ca))
        .get("/v1/status")
        .await;
    assert_eq!(refused.status, 401);
    let ok = Client::http(base, Some(&token), Some(&ca))
        .get("/v1/status")
        .await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    let tcp = &ok.json()["listeners"]["admin_tcp"];
    assert_eq!(tcp["tls"], true);
    assert!(tcp["url"].as_str().expect("url").starts_with("https://"));

    let plain = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/status"))
        .bearer_auth(&token)
        .timeout(WAIT)
        .send()
        .await;
    assert!(
        plain.as_ref().map_or(true, |r| !r.status().is_success()),
        "plaintext served on the TLS listener"
    );
}

#[tokio::test]
async fn unusable_listener_tls_material_refuses_start() {
    let b = Builder::new(
        "[admin]\nbind = \"127.0.0.1:0\"\ntls = { cert = \"{dir}/admin.pem\", key = \"{dir}/missing-key.pem\" }\n",
    );
    let (_, cert, _) = server_pki();
    write_file(&b.dir().join("admin.pem"), &cert, 0o644);
    match b.try_start().await {
        Err((AdminError::Tls { listener, reason }, Some(dir))) => {
            assert_eq!(listener, "admin");
            assert!(reason.contains("missing-key.pem"), "{reason}");
            assert!(!dir.path().join("admin.sock").exists(), "nothing bound");
        }
        Err((e, _)) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("started without a key"),
    }
}

fn write_assets(dir: &std::path::Path) {
    let ui = dir.join("ui");
    std::fs::create_dir_all(ui.join("assets")).expect("mkdir");
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>janus ui</title>",
    )
    .expect("write");
    std::fs::write(ui.join("assets/app.js"), "console.log('janus')").expect("write");
}

fn assert_hardened(headers: &http::HeaderMap, what: &str) {
    for (name, want) in [
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'self'; frame-ancestors 'none'",
        ),
        ("referrer-policy", "no-referrer"),
    ] {
        assert_eq!(
            headers.get(name).and_then(|v| v.to_str().ok()),
            Some(want),
            "{name} on {what}"
        );
    }
}

#[tokio::test]
async fn ui_listener_sets_hardening_headers_on_assets_and_api() {
    let b = Builder::new(UI);
    write_assets(b.dir());
    let d = b.start().await;
    let token = d.token();
    let anon = d.ui(None);
    assert_hardened(&anon.get("/").await.headers, "index");
    assert_hardened(&anon.get("/assets/app.js").await.headers, "asset");
    assert_hardened(&anon.get("/some/spa/route").await.headers, "SPA fallback");
    assert_hardened(&anon.get("/v1/status").await.headers, "401");
    let authed = d.ui(Some(&token));
    assert_hardened(&authed.get("/v1/status").await.headers, "API 200");
    assert_hardened(&authed.get("/v1/nope").await.headers, "API 404");
}

const UI: &str = "[ui]\nbind = \"127.0.0.1:0\"\nassets_dir = \"{dir}/ui\"\n";

#[tokio::test]
async fn ui_listener_serves_assets_and_token_gated_api() {
    let (logs, _guard) = Logs::capture();
    let b = Builder::new(UI);
    write_assets(b.dir());
    let d = b.start().await;
    let addr = d.handle().ui_addr().expect("ui bound");
    assert!(addr.ip().is_loopback());
    let url = d.handle().ui_url().expect("url");
    assert_eq!(url, format!("http://{addr}/"));
    let token = d.token();
    let anon = d.ui(None);

    let index = anon.get("/").await;
    assert_eq!(index.status, 200);
    assert!(index.text().contains("janus ui"));
    let js = anon.get("/assets/app.js").await;
    assert_eq!(js.status, 200);
    assert!(js.text().contains("console.log"));
    let spa = anon.get("/requests/some-id").await;
    assert_eq!(spa.status, 200, "SPA fallback");
    assert!(spa.text().contains("janus ui"));

    let refused = anon.get("/v1/status").await;
    assert_eq!(refused.status, 401);
    assert_eq!(refused.error_code(), "unauthorized");
    let unknown = anon.get("/v1/not-an-endpoint").await;
    assert_eq!(
        unknown.status, 401,
        "API paths are token-gated, unknown ones too"
    );
    let ok = d
        .ui(Some(&token))
        .send(
            "GET",
            "/v1/status",
            &[("origin", "http://evil.example")],
            None,
        )
        .await;
    assert_eq!(ok.status, 200);
    assert_no_access_control_headers(&ok.headers);
    let ui_info = &ok.json()["listeners"]["ui"];
    assert_eq!(ui_info["address"], addr.to_string());
    assert_eq!(ui_info["url"], url);
    let not_found = d.ui(Some(&token)).get("/v1/not-an-endpoint").await;
    assert_eq!(not_found.status, 404);
    assert_eq!(not_found.error_code(), "not_found");

    let preflight = anon
        .send(
            "OPTIONS",
            "/v1/status",
            &[
                ("origin", "http://evil.example"),
                ("access-control-request-method", "GET"),
                ("access-control-request-headers", "authorization"),
            ],
            None,
        )
        .await;
    assert!(
        !(200..300).contains(&preflight.status),
        "preflight answered: {}",
        preflight.status
    );
    assert_no_access_control_headers(&preflight.headers);

    assert_eq!(
        d.socket().get("/v1/status").await.status,
        200,
        "admin socket too"
    );
    let text = logs.text();
    assert!(text.contains(&url), "UI URL not logged: {text}");
    assert!(
        text.contains(&d.handle().token_path().display().to_string()),
        "token path not logged: {text}"
    );
}

#[tokio::test]
async fn ui_not_enabled_means_no_listener() {
    let port = free_port();
    let d = Builder::new(ADMIN_TCP).start().await;
    assert!(d.handle().ui_addr().is_none());
    assert!(d.handle().ui_url().is_none());
    let status = d.socket().get("/v1/status").await.json();
    assert_eq!(status["listeners"]["ui"], Value::Null);
    assert!(!accepts(format!("127.0.0.1:{port}").parse().expect("addr")));
    assert_eq!(d.socket().get("/v1/status").await.status, 200);
}

#[tokio::test]
async fn ui_without_built_assets_refuses_start() {
    let port = free_port();
    let b = Builder::new(&format!(
        "[ui]\nbind = \"127.0.0.1:{port}\"\nassets_dir = \"{{dir}}/empty\"\n"
    ));
    std::fs::create_dir(b.dir().join("empty")).expect("mkdir");
    match b.try_start().await {
        Err((AdminError::UiAssets { reason, .. }, Some(dir))) => {
            assert!(reason.contains("index.html"), "{reason}");
            assert!(!dir.path().join("admin.sock").exists());
            assert!(!accepts(format!("127.0.0.1:{port}").parse().expect("addr")));
        }
        Err((e, _)) => panic!("wrong refusal: {e}"),
        Ok(_) => panic!("UI started without assets"),
    }
}

#[tokio::test]
async fn data_plane_has_no_admin_routes() {
    let b = Builder::new(&format!("{ADMIN_TCP}{UI}"));
    write_assets(b.dir());
    let data_plane = b.data_plane();
    let d = b.start().await;
    // The admin surface never binds the data-plane address: an admin path
    // sent there cannot reach the admin router.
    for bound in [d.handle().admin_addr(), d.handle().ui_addr()] {
        assert_ne!(bound.expect("bound").port(), data_plane.port());
    }
    assert!(
        !accepts(data_plane),
        "admin surface listens on the data-plane port"
    );
    let status = d.socket().get("/v1/status").await.json();
    assert_eq!(status["listeners"]["data_plane"], data_plane.to_string());
    d.stop().await;

    // Nor can it be configured onto it.
    for config in [
        "[admin]\nbind = \"{data_plane}\"\n",
        "[ui]\nbind = \"{data_plane}\"\n",
    ] {
        match Builder::new(config).try_start().await {
            Err((AdminError::Config(e), _)) => {
                assert!(e.to_string().contains("data_plane.bind"), "{e}")
            }
            Err((e, _)) => panic!("wrong refusal: {e}"),
            Ok(_) => panic!("{config}: admin listener shares the data-plane address"),
        }
    }
}

#[tokio::test]
async fn shutdown_closes_listeners_and_ends_live_tails() {
    let b = Builder::new(&format!("{ADMIN_TCP}{UI}"));
    write_assets(b.dir());
    let d = b.start().await;
    let token = d.token();
    let admin = d.handle().admin_addr().expect("bound");
    let ui = d.handle().ui_addr().expect("bound");
    let socket = d.handle().socket_path().to_path_buf();
    let mut tail = d.tcp(Some(&token)).sse("/v1/requests/stream").await;
    assert_eq!(tail.next(WAIT).await.expect("opening ping").event, "ping");

    let started = std::time::Instant::now();
    d.stop().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a live tail held shutdown up"
    );
    assert!(tail.next(WAIT).await.is_none(), "tail ended");
    assert!(!socket.exists());
    assert!(!accepts(admin));
    assert!(!accepts(ui));
}

// ---------------------------------------------------------------------------
// Status and plugin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_listeners_upstreams_log_pool_and_plugin() {
    let b = Builder::new(&format!(
        "{UPSTREAMS}\n[[credential]]\nref = \"cfg-key\"\nupstream = \"openrouter\"\nfile = \"{{dir}}/cfg-key\"\n"
    ));
    write_file(&b.dir().join("cfg-key"), "sk-or-cfg-0123456789\n", 0o600);
    let d = b.start().await;
    let added = add_openrouter(&d.socket(), "sk-or-pool-0123456789").await;
    let now = Utc::now();
    for (i, up) in [UpstreamKind::OpenRouter, UpstreamKind::Anthropic]
        .into_iter()
        .enumerate()
    {
        d.store
            .requests()
            .submit(record(&format!("r{i}"), "sb", up, Outcome::Forwarded, now))
            .await;
    }
    d.store.requests().flush().await;
    let disabled = d
        .socket()
        .post(
            &format!(
                "/v1/credentials/{}/disable",
                added["ref"].as_str().expect("ref")
            ),
            None,
        )
        .await;
    assert_eq!(disabled.status, 200);

    let s = d.socket().get("/v1/status").await.json();
    assert_eq!(s["version"], env!("CARGO_PKG_VERSION"));
    assert!(s["build"].as_str().is_some_and(|b| !b.is_empty()));
    let started: DateTime<Utc> = s["started_at"]
        .as_str()
        .expect("started_at")
        .parse()
        .expect("rfc3339");
    let age = Utc::now().signed_duration_since(started);
    assert!(age >= chrono::Duration::zero() && age < chrono::Duration::minutes(1));
    assert_eq!(
        s["listeners"]["admin_socket"],
        d.data_dir().join("admin.sock").display().to_string()
    );
    assert_eq!(s["listeners"]["admin_tcp"], Value::Null);
    assert_eq!(s["listeners"]["data_plane"], d.data_plane.to_string());

    let ups = s["upstreams"].as_array().expect("upstreams");
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0]["name"], "openrouter");
    assert_eq!(ups[0]["path_prefix"], "/openrouter");
    assert_eq!(ups[0]["binding"], "opaque");
    assert_eq!(
        ups[0]["routes"],
        json!([{ "path": "/v1/models", "binding": "none" }])
    );
    assert_eq!(ups[1]["name"], "anthropic");
    assert_eq!(ups[1]["binding"], "opaque");

    assert_eq!(s["request_log"]["enabled"], true);
    assert_eq!(s["request_log"]["retain"], "30days");
    assert_eq!(s["request_log"]["retain_seconds"], 30 * 24 * 3600);
    assert_eq!(s["request_log"]["rows"], 2);

    assert_eq!(s["credentials"]["total"], 2);
    assert_eq!(s["credentials"]["by_status"]["active"], 1);
    assert_eq!(s["credentials"]["by_status"]["disabled"], 1);
    assert_eq!(s["credentials"]["by_status"]["invalid"], 0);
    assert_eq!(s["credentials"]["by_status"]["rate_limited"], 0);

    let plugin = &s["plugin"];
    assert_eq!(plugin["mode"], "none");
    assert_eq!(plugin["health"], "unknown");
    assert_eq!(plugin["endpoint"], Value::Null);
    assert_eq!(plugin["last_error"], Value::Null);
    assert_eq!(plugin["last_ok_at"], Value::Null);
}

#[tokio::test]
async fn plugin_endpoint_equals_status_plugin_and_check_needs_a_plugin() {
    let d = Builder::new("").start().await;
    let plugin = d.socket().get("/v1/plugin").await;
    assert_eq!(plugin.status, 200);
    let status = d.socket().get("/v1/status").await.json();
    assert_eq!(plugin.json(), status["plugin"]);
    let check = d.socket().post("/v1/plugin/check", None).await;
    assert_eq!(check.status, 409);
    assert_eq!(check.error_code(), "no_plugin");
}

#[tokio::test]
async fn plugin_check_on_unix_transport_reports_each_step() {
    let d = Builder::new("[plugin]\nendpoint = \"unix://{dir}/plugin.sock\"\n")
        .start()
        .await;
    let endpoint = format!("unix://{}", d.data_dir().join("plugin.sock").display());
    let plugin = d.socket().get("/v1/plugin").await.json();
    assert_eq!(plugin["mode"], "unix");
    assert_eq!(plugin["endpoint"], endpoint);
    assert_eq!(plugin["health"], "unknown", "no call attempted yet");
    assert_eq!(d.socket().get("/v1/status").await.json()["plugin"], plugin);

    let check = d.socket().post("/v1/plugin/check", None).await;
    assert_eq!(check.status, 200, "{}", check.text());
    let check = check.json();
    assert_eq!(check["mode"], "unix");
    assert_eq!(check["endpoint"], endpoint);
    assert_eq!(
        check["passed"], false,
        "nothing listens on the plugin socket"
    );
    let steps = check["steps"].as_array().expect("steps");
    let step = |name: &str| steps.iter().find(|s| s["step"] == name).cloned();
    assert!(step("path_permissions").is_some(), "{check}");
    let connect = step("connect").expect("connect step");
    assert_eq!(connect["passed"], false);
    assert_eq!(connect["class"], "plugin_unreachable");
}

struct UseDefault;

#[async_trait]
impl JanusPlugin for UseDefault {
    async fn resolve_route(&self, _req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        Ok(RouteOutcome::UseDefault)
    }

    async fn notify(&self, _event: NotifyEvent) {}
}

#[tokio::test]
async fn plugin_status_and_check_for_a_linked_plugin() {
    let d = Builder::new("[plugin]\nlinked = true\n")
        .linked(Arc::new(UseDefault))
        .start()
        .await;
    let plugin = d.socket().get("/v1/plugin").await.json();
    assert_eq!(plugin["mode"], "linked");
    assert_eq!(plugin["endpoint"], Value::Null);
    let check = d.socket().post("/v1/plugin/check", None).await.json();
    assert_eq!(check["mode"], "linked");
    assert_eq!(check["passed"], true);
    assert_eq!(check["steps"], json!([]));
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credential_material_never_returned() {
    let (logs, _guard) = Logs::capture();
    let first = "sk-or-v1-FIRSTSECRETMATERIAL0001";
    let second = "sk-or-v1-ROTATEDSECRETMATERIAL02";
    let malformed = "sk-or-v1-MALFORMEDSECRETMATERIAL3";
    let b = Builder::new(ADMIN_TCP);
    let d = b.start().await;
    let token = d.token();
    let socket = d.socket();
    let tcp = d.tcp(Some(&token));

    let mut bodies = Vec::new();
    let added = socket
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "ref": "main", "label": "Main", "material": { "api_key": first } })),
        )
        .await;
    assert_eq!(added.status, 201, "{}", added.text());
    let meta = added.json();
    for field in [
        "ref",
        "upstream",
        "kind",
        "label",
        "source",
        "status",
        "rate_limited_until",
        "expires_at",
        "last_used_at",
        "request_count",
        "error_count",
        "last_error",
        "tags",
    ] {
        assert!(meta.get(field).is_some(), "metadata lacks {field}: {meta}");
    }
    assert_eq!(meta["ref"], "main");
    assert_eq!(meta["kind"], "api_key");
    assert_eq!(meta["source"], "pool");
    assert_eq!(
        meta.as_object().expect("object").len(),
        13,
        "only metadata: {meta}"
    );
    bodies.push(added.text());

    // The material is really in the pool: the test would see it if it leaked.
    let material = d
        .pool
        .resolve(
            &Selection::implementation_wide(UpstreamKind::OpenRouter),
            &CredentialChoice::Ref("main".into()),
            Pick::default(),
        )
        .await
        .expect("usable");
    assert_eq!(material.secret.expose_str(), Some(first));

    for client in [&socket, &tcp] {
        for path in [
            "/v1/credentials",
            "/v1/credentials?upstream=openrouter",
            "/v1/credentials/main",
            "/v1/credentials/default?upstream=openrouter",
            "/v1/status",
        ] {
            let r = client.get(path).await;
            assert_eq!(r.status, 200, "{path}");
            bodies.push(r.text());
        }
    }
    let rotated = tcp
        .put(
            "/v1/credentials/main/material",
            json!({ "material": { "api_key": second } }),
        )
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.text());
    bodies.push(rotated.text());
    for action in ["disable", "enable", "refresh"] {
        bodies.push(
            socket
                .post(&format!("/v1/credentials/main/{action}"), None)
                .await
                .text(),
        );
    }
    let bad = socket
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "material": { "key": malformed } })),
        )
        .await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.error_code(), "invalid_material");
    bodies.push(bad.text());
    let wrong_shape = socket
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "anthropic", "material": { "claudeAiOauth": { "refreshToken": second } } })),
        )
        .await;
    assert_eq!(wrong_shape.status, 400);
    bodies.push(wrong_shape.text());

    for body in &bodies {
        for secret in [first, second, malformed] {
            assert!(!body.contains(secret), "material in a response: {body}");
        }
    }
    let text = logs.text();
    for secret in [first, second, malformed] {
        assert!(!text.contains(secret), "material in the daemon log");
    }
    assert!(
        text.contains("admin mutation"),
        "audit lines were captured: {text}"
    );
}

#[tokio::test]
async fn credential_add_is_live_with_a_generated_ref_and_a_pool_slot() {
    let d = Builder::new("").start().await;
    let s = d.socket();
    let meta = add_openrouter(&s, "sk-or-live-0123456789").await;
    let r = meta["ref"].as_str().expect("generated ref").to_owned();
    assert!(r.starts_with("openrouter-"), "{r}");
    assert!(
        d.data_dir().join("credentials").join(&r).is_dir(),
        "slot directory"
    );

    let list = s.get("/v1/credentials?upstream=openrouter").await.json();
    assert_eq!(list["credentials"].as_array().expect("list").len(), 1);
    assert_eq!(list["credentials"][0]["ref"], r);
    let other = s.get("/v1/credentials?upstream=anthropic").await.json();
    assert_eq!(other["credentials"], json!([]));
    let peek = s.get("/v1/credentials/default?upstream=openrouter").await;
    assert_eq!(peek.status, 200);
    assert_eq!(peek.json()["ref"], r);
    let shown = s.get(&format!("/v1/credentials/{r}")).await.json();
    assert_eq!(shown["ref"], r);

    // Usable at once, without a restart: the default chain picks it.
    let material = d
        .pool
        .resolve(
            &Selection::implementation_wide(UpstreamKind::OpenRouter),
            &CredentialChoice::Default,
            Pick::default(),
        )
        .await
        .expect("default resolves");
    assert_eq!(material.credential_ref, r);
    assert_eq!(
        s.get(&format!("/v1/credentials/{r}")).await.json()["request_count"],
        1
    );

    let dup = s
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "ref": r, "material": { "api_key": "sk-or-other-1" } })),
        )
        .await;
    assert_eq!(dup.status, 409);
    assert_eq!(dup.error_code(), "credential_exists");
    let reserved = s
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "ref": "default", "material": { "api_key": "k" } })),
        )
        .await;
    assert_eq!(reserved.status, 400);
    assert_eq!(reserved.error_code(), "invalid_ref");
}

#[tokio::test]
async fn credential_disable_enable_rotate_and_delete() {
    let d = Builder::new("").start().await;
    let s = d.socket();
    add_openrouter(&s, "sk-or-cycle-0123456789").await;
    let r = s.get("/v1/credentials").await.json()["credentials"][0]["ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    let path = format!("/v1/credentials/{r}");

    let off = s.post(&format!("{path}/disable"), None).await;
    assert_eq!(off.status, 200);
    assert_eq!(off.json()["status"], "disabled");
    let peek = s.get("/v1/credentials/default?upstream=openrouter").await;
    assert_eq!(peek.status, 404);
    assert_eq!(peek.error_code(), "no_default_credential");
    let named = d
        .pool
        .resolve(
            &Selection::implementation_wide(UpstreamKind::OpenRouter),
            &CredentialChoice::Ref(r.clone()),
            Pick::default(),
        )
        .await;
    assert!(
        matches!(named, Err(PoolError::Unavailable { .. })),
        "no fall-through"
    );
    let on = s.post(&format!("{path}/enable"), None).await;
    assert_eq!(on.json()["status"], "active");
    assert_eq!(
        s.get("/v1/credentials/default?upstream=openrouter")
            .await
            .json()["ref"],
        r
    );

    d.pool
        .report(
            &r,
            &UpstreamFeedback::CredentialInvalid {
                reason: "401".into(),
            },
        )
        .await
        .expect("report");
    assert_eq!(s.get(&path).await.json()["status"], "invalid");
    let rotated = s
        .put(
            &format!("{path}/material"),
            json!({ "material": { "api_key": "sk-or-new-0123456789" } }),
        )
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.text());
    assert_eq!(
        rotated.json()["status"],
        "active",
        "rotation clears invalid"
    );
    let material = d
        .pool
        .resolve(
            &Selection::implementation_wide(UpstreamKind::OpenRouter),
            &CredentialChoice::Ref(r.clone()),
            Pick::default(),
        )
        .await
        .expect("usable");
    assert_eq!(material.secret.expose_str(), Some("sk-or-new-0123456789"));

    let refresh = s.post(&format!("{path}/refresh"), None).await;
    assert_eq!(refresh.status, 409);
    assert_eq!(refresh.error_code(), "not_refreshable");

    let deleted = s.delete(&path).await;
    assert_eq!(deleted.status, 204);
    assert!(deleted.body.is_empty());
    let gone = s.get(&path).await;
    assert_eq!(gone.status, 404);
    assert_eq!(gone.error_code(), "credential_not_found");
    assert!(
        !d.data_dir().join("credentials").join(&r).exists(),
        "slot removed"
    );
    assert_eq!(s.delete(&path).await.status, 404);
    for action in ["enable", "disable", "refresh"] {
        assert_eq!(
            s.post(&format!("{path}/{action}"), None).await.status,
            404,
            "{action}"
        );
    }
}

/// A token endpoint that issues `at-new` for any refresh grant.
async fn token_endpoint() -> (url::Url, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(body["grant_type"], "refresh_token");
                axum::Json(json!({
                    "access_token": "at-new-REFRESHEDSECRET",
                    "refresh_token": "rt-new-REFRESHEDSECRET",
                    "expires_in": 3600,
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (format!("http://{addr}/token").parse().expect("url"), calls)
}

#[tokio::test]
async fn credential_refresh_updates_oauth_expiry_now() {
    let (url, calls) = token_endpoint().await;
    let d = Builder::new("").anthropic_token_url(url).start().await;
    let s = d.socket();
    let soon = (Utc::now() + chrono::Duration::minutes(30)).timestamp_millis();
    let added = s
        .post(
            "/v1/credentials",
            Some(json!({
                "upstream": "anthropic",
                "ref": "claude-max",
                "material": { "claudeAiOauth": {
                    "accessToken": "at-old-OLDSECRET",
                    "refreshToken": "rt-old-OLDSECRET",
                    "expiresAt": soon,
                } },
            })),
        )
        .await;
    assert_eq!(added.status, 201, "{}", added.text());
    let before: DateTime<Utc> = added.json()["expires_at"]
        .as_str()
        .expect("expiry")
        .parse()
        .expect("ts");
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let refreshed = s.post("/v1/credentials/claude-max/refresh", None).await;
    assert_eq!(refreshed.status, 200, "{}", refreshed.text());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the token endpoint was called now"
    );
    let text = refreshed.text();
    for secret in [
        "at-new-REFRESHEDSECRET",
        "rt-new-REFRESHEDSECRET",
        "OLDSECRET",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
    let after: DateTime<Utc> = refreshed.json()["expires_at"]
        .as_str()
        .expect("expiry")
        .parse()
        .expect("ts");
    assert!(
        after > before + chrono::Duration::minutes(20),
        "{before} -> {after}"
    );
    let material = d
        .pool
        .resolve(
            &Selection::implementation_wide(UpstreamKind::Anthropic),
            &CredentialChoice::Ref("claude-max".into()),
            Pick::default(),
        )
        .await
        .expect("usable");
    assert_eq!(material.secret.expose_str(), Some("at-new-REFRESHEDSECRET"));
}

#[tokio::test]
async fn admin_errors_are_json_with_a_snake_case_code() {
    let b = Builder::new(
        "[[credential]]\nref = \"cfg-key\"\nupstream = \"openrouter\"\nfile = \"{dir}/cfg-key\"\n",
    );
    write_file(&b.dir().join("cfg-key"), "sk-or-cfg-0123456789\n", 0o600);
    let d = b.start().await;
    let s = d.socket();
    let cases: Vec<(support::Reply, u16, &str)> = vec![
        (
            s.get("/v1/credentials/does-not-exist").await,
            404,
            "credential_not_found",
        ),
        (s.get("/v1/no-such-endpoint").await, 404, "not_found"),
        (s.get("/not-v1").await, 404, "not_found"),
        (s.delete("/v1/status").await, 405, "method_not_allowed"),
        (
            s.get("/v1/credentials/default").await,
            400,
            "invalid_request",
        ),
        (
            s.get("/v1/credentials?upstream=nowhere").await,
            400,
            "invalid_request",
        ),
        (
            s.get("/v1/credentials?bogus=1").await,
            400,
            "invalid_request",
        ),
        (
            s.send("POST", "/v1/credentials", &[], Some(b"{not json".to_vec()))
                .await,
            400,
            "invalid_request",
        ),
        (
            s.post(
                "/v1/credentials",
                Some(json!({ "upstream": "nowhere", "material": {} })),
            )
            .await,
            400,
            "invalid_request",
        ),
        (
            s.delete("/v1/credentials/cfg-key").await,
            409,
            "not_pool_managed",
        ),
        (
            s.put(
                "/v1/credentials/cfg-key/material",
                json!({ "material": { "api_key": "x" } }),
            )
            .await,
            409,
            "not_pool_managed",
        ),
        (
            s.get("/v1/identities/nope").await,
            404,
            "identity_not_found",
        ),
        (
            s.post("/v1/identities/nope/revoke", None).await,
            404,
            "identity_not_found",
        ),
        (
            s.delete("/v1/identities/nope").await,
            404,
            "identity_not_found",
        ),
        (s.get("/v1/requests/nope").await, 404, "request_not_found"),
    ];
    for (reply, status, code) in cases {
        assert_eq!(reply.status, status, "{}", reply.text());
        assert_eq!(reply.error_code(), code, "{}", reply.text());
        assert_eq!(reply.headers["content-type"], "application/json");
    }
    let no_type = s
        .send(
            "POST",
            "/v1/identities",
            &[("content-type", "text/plain")],
            Some(b"{}".to_vec()),
        )
        .await;
    assert_eq!(no_type.status, 415);
    assert_eq!(no_type.error_code(), "unsupported_media_type");
}

// ---------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identity_token_returned_once() {
    let d = Builder::new("").start().await;
    let s = d.socket();
    let created = s
        .post(
            "/v1/identities",
            Some(json!({ "identity": "sandbox-1", "claims": { "team": "a" }, "label": "box", "expires_in": "1h" })),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.text());
    let created = created.json();
    let token = created["token"].as_str().expect("token").to_owned();
    assert_eq!(URL_SAFE_NO_PAD.decode(&token).expect("base64url").len(), 32);
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["identity"], "sandbox-1");
    assert_eq!(created["claims"], json!({ "team": "a" }));
    assert_eq!(created["label"], "box");
    assert!(created["expires_at"].is_string());

    let shown = s.get(&format!("/v1/identities/{id}")).await;
    assert_eq!(shown.status, 200);
    let shown_json = shown.json();
    for field in [
        "id",
        "identity",
        "claims",
        "label",
        "created_at",
        "expires_at",
        "revoked_at",
        "last_seen_at",
    ] {
        assert!(
            shown_json.get(field).is_some(),
            "{field} missing: {shown_json}"
        );
    }
    assert!(shown_json.get("token").is_none());
    let listed = s.get("/v1/identities").await;
    let revoked = s.post(&format!("/v1/identities/{id}/revoke"), None).await;
    for later in [&shown, &listed, &revoked] {
        assert!(
            !later.text().contains(&token),
            "token returned again: {}",
            later.text()
        );
    }

    // Only the hash is stored.
    let db = rusqlite::Connection::open(d.data_dir().join("janus.db")).expect("open db");
    let (hash, row): (String, String) = db
        .query_row(
            "SELECT token_hash, id || identity || claims || COALESCE(label, '') FROM identities WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(hash, TokenHash::of(token.as_bytes()).to_hex());
    assert!(!row.contains(&token));

    // A provisioner cannot choose the token.
    let chosen = s
        .post(
            "/v1/identities",
            Some(json!({ "identity": "sandbox-2", "token": "guessable123" })),
        )
        .await;
    assert_eq!(chosen.status, 400);
    assert_eq!(chosen.error_code(), "invalid_request");
    let registry = d.store.registry();
    assert!(registry
        .lookup(&TokenHash::of(b"guessable123"), Utc::now())
        .is_none());
    assert!(registry
        .list(&Default::default(), Utc::now())
        .iter()
        .all(|r| r.identity != "sandbox-2"));
}

#[tokio::test]
async fn identity_list_filters_revoke_is_idempotent_and_delete_removes() {
    let d = Builder::new("").start().await;
    let s = d.socket();
    let mut tokens = Vec::new();
    let mut ids = Vec::new();
    for identity in ["sb-x", "sb-x", "sb-y"] {
        let c = s
            .post("/v1/identities", Some(json!({ "identity": identity })))
            .await
            .json();
        tokens.push(c["token"].as_str().expect("token").to_owned());
        ids.push(c["id"].as_str().expect("id").to_owned());
    }
    let registry = d.store.registry();
    let verdict = |token: &str| {
        registry
            .lookup(&TokenHash::of(token.as_bytes()), Utc::now())
            .expect("registered")
            .verdict(Utc::now())
    };
    assert_eq!(verdict(&tokens[0]), Ok(()));
    assert_eq!(
        verdict(&tokens[1]),
        Ok(()),
        "two entries share one identity"
    );

    let first = s
        .post(&format!("/v1/identities/{}/revoke", ids[0]), None)
        .await;
    assert_eq!(first.status, 200);
    let again = s
        .post(&format!("/v1/identities/{}/revoke", ids[0]), None)
        .await;
    assert_eq!(again.status, 200, "idempotent");
    assert_eq!(first.json()["revoked_at"], again.json()["revoked_at"]);
    assert!(first.json()["revoked_at"].is_string());
    assert_eq!(
        verdict(&tokens[0]),
        Err(ExtractionError::Revoked),
        "immediate"
    );
    assert_eq!(verdict(&tokens[1]), Ok(()));

    let expiring = s
        .post(
            "/v1/identities",
            Some(json!({ "identity": "sb-z", "expires_in": "1s" })),
        )
        .await
        .json();
    let expiring_id = expiring["id"].as_str().expect("id").to_owned();
    tokio::time::sleep(Duration::from_millis(1300)).await;

    let ids_of = |v: Value| -> Vec<String> {
        v["identities"]
            .as_array()
            .expect("list")
            .iter()
            .map(|e| e["id"].as_str().expect("id").to_owned())
            .collect()
    };
    let default = ids_of(s.get("/v1/identities").await.json());
    assert!(!default.contains(&ids[0]), "revoked hidden by default");
    assert!(!default.contains(&expiring_id), "expired hidden by default");
    assert!(default.contains(&ids[1]) && default.contains(&ids[2]));
    let with_revoked = ids_of(s.get("/v1/identities?include_revoked=true").await.json());
    assert!(with_revoked.contains(&ids[0]));
    assert!(!with_revoked.contains(&expiring_id));
    let with_expired = ids_of(s.get("/v1/identities?include_expired=true").await.json());
    assert!(with_expired.contains(&expiring_id));
    assert!(!with_expired.contains(&ids[0]));
    let only_x = ids_of(
        s.get("/v1/identities?identity=sb-x&include_revoked=true")
            .await
            .json(),
    );
    assert_eq!(only_x.len(), 2);
    assert!(only_x.contains(&ids[0]) && only_x.contains(&ids[1]));

    let deleted = s.delete(&format!("/v1/identities/{}", ids[0])).await;
    assert_eq!(deleted.status, 204);
    let gone = s.get(&format!("/v1/identities/{}", ids[0])).await;
    assert_eq!(gone.status, 404);
    assert_eq!(gone.error_code(), "identity_not_found");
    assert!(registry
        .lookup(&TokenHash::of(tokens[0].as_bytes()), Utc::now())
        .is_none());
}

#[tokio::test]
async fn identity_create_validates_its_body() {
    let d = Builder::new("").start().await;
    let s = d.socket();
    let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    for body in [
        json!({ "identity": "" }),
        json!({ "identity": "a", "expires_at": past }),
        json!({ "identity": "a", "expires_in": "1h", "expires_at": future }),
        json!({ "identity": "a", "expires_in": "soon" }),
        json!({ "identity": "a", "claims": { "n": 5 } }),
        json!({ "claims": {} }),
    ] {
        let r = s.post("/v1/identities", Some(body.clone())).await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
        assert_eq!(r.error_code(), "invalid_request");
    }
    assert_eq!(
        s.get("/v1/identities?include_expired=true&include_revoked=true")
            .await
            .json()["identities"],
        json!([])
    );
    let ok = s
        .post(
            "/v1/identities",
            Some(json!({ "identity": "a", "expires_at": future })),
        )
        .await;
    assert_eq!(ok.status, 201);
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[tokio::test]
async fn requests_query_filters_and_paginates_newest_first() {
    let d = Builder::new("").start().await;
    let t0 = Utc::now() - chrono::Duration::hours(2);
    for i in 0..1100i64 {
        let identity = if i % 2 == 0 { "a" } else { "b" };
        let upstream = if i % 3 == 0 {
            UpstreamKind::OpenAi
        } else {
            UpstreamKind::OpenRouter
        };
        let outcome = if i % 10 == 0 {
            Outcome::Denied {
                reason: "quota".into(),
            }
        } else {
            Outcome::Forwarded
        };
        d.store
            .requests()
            .submit(record(
                &format!("req-{i:04}"),
                identity,
                upstream,
                outcome,
                t0 + chrono::Duration::seconds(i),
            ))
            .await;
    }
    d.store.requests().flush().await;
    let s = d.socket();

    let page = s.get("/v1/requests").await.json();
    let records = page["requests"].as_array().expect("records");
    assert_eq!(records.len(), 100, "default limit");
    assert_eq!(records[0]["request_id"], "req-1099", "newest first");
    assert_eq!(records[99]["request_id"], "req-1000");
    assert_eq!(page["disabled"], false);
    assert!(page["next"].is_string());
    let capped = s.get("/v1/requests?limit=5000").await.json();
    assert_eq!(
        capped["requests"].as_array().expect("records").len(),
        1000,
        "capped"
    );

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/v1/requests?limit=300&before={c}"),
            None => "/v1/requests?limit=300".to_owned(),
        };
        let page = s.get(&path).await.json();
        for r in page["requests"].as_array().expect("records") {
            seen.push(r["request_id"].as_str().expect("id").to_owned());
        }
        match page["next"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    let expected: Vec<String> = (0..1100).rev().map(|i| format!("req-{i:04}")).collect();
    assert_eq!(seen, expected, "every record once, newest first");

    let count = |path: String| {
        let s = &s;
        async move {
            let page = s.get(&path).await;
            assert_eq!(page.status, 200, "{path}: {}", page.text());
            page.json()["requests"].as_array().expect("records").clone()
        }
    };
    let a = count("/v1/requests?identity=a&limit=1000".into()).await;
    assert_eq!(a.len(), 550);
    assert!(a.iter().all(|r| r["caller_identity"] == "a"));
    let openai = count("/v1/requests?upstream=openai&limit=1000".into()).await;
    assert_eq!(openai.len(), 367);
    assert!(openai.iter().all(|r| r["upstream"] == "openai"));
    let denied = count("/v1/requests?outcome=denied&limit=1000".into()).await;
    assert_eq!(denied.len(), 110);
    assert!(denied
        .iter()
        .all(|r| r["outcome"] == "denied" && r["reason"] == "quota"));
    assert_eq!(
        count("/v1/requests?credential=cred-a&limit=10".into())
            .await
            .len(),
        10
    );
    assert_eq!(count("/v1/requests?credential=other".into()).await.len(), 0);
    let window = count(format!(
        "/v1/requests?since={}&until={}",
        ts(t0 + chrono::Duration::seconds(100)),
        ts(t0 + chrono::Duration::seconds(110))
    ))
    .await;
    assert_eq!(window.len(), 10);
    assert_eq!(window[0]["request_id"], "req-0109");
    let combined =
        count("/v1/requests?identity=a&outcome=denied&upstream=openai&limit=1000".into()).await;
    assert_eq!(combined.len(), 37, "i % 30 == 0");

    let one = s.get("/v1/requests/req-0042").await;
    assert_eq!(one.status, 200);
    let one = one.json();
    assert_eq!(one["request_id"], "req-0042");
    assert_eq!(one["caller_identity"], "a");
    assert_eq!(one["upstream"], "openai");
    assert_eq!(one["binding"], "opaque");

    // `upstream` is an instance name, and the log keeps the names of
    // instances a past config had, so an unknown name matches nothing
    // rather than being refused.
    let gone = s.get("/v1/requests?upstream=retired-instance").await;
    assert_eq!(gone.status, 200, "{}", gone.text());
    assert_eq!(gone.json()["requests"], json!([]));

    for bad in [
        "/v1/requests?outcome=bogus",
        "/v1/requests?before=garbage",
        "/v1/requests?limit=0",
        "/v1/requests?since=yesterday",
        "/v1/requests?unknown=1",
    ] {
        let r = s.get(bad).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.text());
        assert_eq!(r.error_code(), "invalid_request");
    }
}

/// The request ids a tail delivers until it goes quiet.
async fn streamed_ids(tail: &mut support::Sse) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(e) = tail.next_non_ping(Duration::from_millis(800)).await {
        assert_eq!(e.event, "request");
        assert_eq!(e.id, None, "live-only tail: no event ids");
        let r: Value = serde_json::from_str(&e.data).expect("record JSON");
        out.push(r["request_id"].as_str().expect("id").to_owned());
    }
    out
}

#[tokio::test]
async fn request_stream_sse_applies_filters_and_needs_the_header_token() {
    let d = Builder::new(ADMIN_TCP).start().await;
    let token = d.token();
    let tcp = d.tcp(Some(&token));
    let mut openai = tcp.sse("/v1/requests/stream?upstream=openai").await;
    assert_eq!(openai.status, 200);
    assert!(
        openai
            .content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("text/event-stream")),
        "{:?}",
        openai.content_type
    );
    let mut denied = tcp
        .sse("/v1/requests/stream?upstream=openai&outcome=denied")
        .await;
    let mut everything = tcp.sse("/v1/requests/stream").await;
    for tail in [&mut openai, &mut denied, &mut everything] {
        let ping = tail.next(WAIT).await.expect("opening ping");
        assert_eq!(ping.event, "ping");
        let at: Value = serde_json::from_str(&ping.data).expect("ping data");
        assert!(at["at"].is_string());
    }

    let now = Utc::now();
    let log = d.store.requests();
    log.submit(record(
        "r-1",
        "sb",
        UpstreamKind::OpenRouter,
        Outcome::Forwarded,
        now,
    ))
    .await;
    log.submit(record(
        "r-2",
        "sb",
        UpstreamKind::OpenAi,
        Outcome::Forwarded,
        now,
    ))
    .await;
    log.submit(record(
        "r-3",
        "sb",
        UpstreamKind::OpenAi,
        Outcome::Denied {
            reason: "no".into(),
        },
        now,
    ))
    .await;
    log.flush().await;

    assert_eq!(streamed_ids(&mut openai).await, ["r-2", "r-3"]);
    assert_eq!(streamed_ids(&mut denied).await, ["r-3"]);
    assert_eq!(streamed_ids(&mut everything).await, ["r-1", "r-2", "r-3"]);

    let in_query = d
        .tcp(None)
        .get(&format!("/v1/requests/stream?token={token}"))
        .await;
    assert_eq!(in_query.status, 401);
    let unsupported = tcp.get("/v1/requests/stream?credential=x").await;
    assert_eq!(
        unsupported.status, 400,
        "the stream filters on identity, upstream and outcome only"
    );
    let bad = tcp.get("/v1/requests/stream?outcome=bogus").await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.error_code(), "invalid_request");
}

#[tokio::test]
async fn request_stream_pings_periodically() {
    let d = Builder::new(ADMIN_TCP).start().await;
    let token = d.token();
    let mut tail = d.tcp(Some(&token)).sse("/v1/requests/stream").await;
    assert_eq!(tail.next(WAIT).await.expect("opening ping").event, "ping");
    let started = std::time::Instant::now();
    let second = tail
        .next(PING_INTERVAL + WAIT)
        .await
        .expect("periodic ping");
    assert_eq!(second.event, "ping");
    assert!(started.elapsed() >= PING_INTERVAL - Duration::from_millis(500));
}

#[tokio::test]
async fn request_log_disabled_answers_empty_with_a_marker() {
    let d = Builder::new(&format!("{ADMIN_TCP}[request_log]\nenabled = false\n"))
        .start()
        .await;
    d.store
        .requests()
        .submit(record(
            "r-1",
            "sb",
            UpstreamKind::OpenAi,
            Outcome::Forwarded,
            Utc::now(),
        ))
        .await;
    d.store.requests().flush().await;
    let s = d.socket();
    let page = s.get("/v1/requests").await;
    assert_eq!(page.status, 200);
    assert_eq!(
        page.json(),
        json!({ "requests": [], "next": null, "disabled": true })
    );
    let one = s.get("/v1/requests/r-1").await;
    assert_eq!(one.status, 404);
    assert_eq!(one.error_code(), "request_log_disabled");
    assert_eq!(
        s.get("/v1/status").await.json()["request_log"]["enabled"],
        false
    );

    let token = d.token();
    let mut tail = d.tcp(Some(&token)).sse("/v1/requests/stream").await;
    assert_eq!(tail.status, 200);
    let marker = tail.next(WAIT).await.expect("marker");
    assert_eq!(marker.event, "disabled");
    assert_eq!(
        serde_json::from_str::<Value>(&marker.data).expect("json"),
        json!({ "disabled": true })
    );
    assert!(tail.next(WAIT).await.is_none(), "stream ends");
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutations_are_audited_with_principal_and_resource() {
    let (logs, _guard) = Logs::capture();
    let b = Builder::new(&format!("{ADMIN_TCP}{UI}"));
    write_assets(b.dir());
    let d = b.start().await;
    let token = d.token();
    let socket = d.socket();
    let tcp = d.tcp(Some(&token));
    let ui = d.ui(Some(&token));
    socket
        .post(
            "/v1/credentials",
            Some(json!({ "upstream": "openrouter", "ref": "audited", "material": { "api_key": "sk-or-audit-1" } })),
        )
        .await;
    assert_eq!(
        socket
            .post("/v1/credentials/audited/disable", None)
            .await
            .status,
        200
    );
    assert_eq!(
        tcp.post("/v1/credentials/audited/enable", None)
            .await
            .status,
        200
    );
    assert_eq!(
        tcp.post("/v1/credentials/missing/disable", None)
            .await
            .status,
        404
    );
    let created = tcp
        .post("/v1/identities", Some(json!({ "identity": "sb-audit" })))
        .await
        .json();
    let id = created["id"].as_str().expect("id");
    let identity_token = created["token"].as_str().expect("token");
    assert_eq!(
        socket
            .post(&format!("/v1/identities/{id}/revoke"), None)
            .await
            .status,
        200
    );
    assert_eq!(socket.get("/v1/credentials/audited").await.status, 200);
    assert_eq!(
        ui.post("/v1/credentials/audited/disable", None)
            .await
            .status,
        200
    );

    let text = logs.text();
    let audit: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("admin mutation"))
        .collect();
    let find = |action: &str, principal: &str, resource: &str, outcome: &str| {
        audit.iter().any(|l| {
            l.contains(action)
                && l.contains(&format!("principal=\"{principal}\""))
                && l.contains(resource)
                && l.contains(&format!("outcome=\"{outcome}\""))
        })
    };
    assert!(
        find("credential.add", "socket", "credential/audited", "ok"),
        "{text}"
    );
    assert!(
        find("credential.disable", "socket", "credential/audited", "ok"),
        "{text}"
    );
    assert!(
        find("credential.enable", "admin_tcp", "credential/audited", "ok"),
        "{text}"
    );
    assert!(
        find(
            "credential.disable",
            "admin_tcp",
            "credential/missing",
            "error"
        ),
        "{text}"
    );
    assert!(find("identity.create", "admin_tcp", id, "ok"), "{text}");
    assert!(find("identity.revoke", "socket", id, "ok"), "{text}");
    assert!(
        find("credential.disable", "ui", "credential/audited", "ok"),
        "{text}"
    );
    assert_eq!(audit.len(), 7, "reads are not audited: {audit:#?}");
    assert!(!text.contains(identity_token), "identity token logged");
    assert!(!text.contains(&token), "admin token logged");
    assert!(!text.contains("sk-or-audit-1"), "material logged");
}

// ---- hardening: the listener-facing failure classes ----------------------

/// A `GET` for `path` with `Connection: close`, optionally with the token.
fn get_request(path: &str, token: Option<&str>) -> String {
    let auth = token.map_or_else(String::new, |t| format!("authorization: Bearer {t}\r\n"));
    format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n{auth}connection: close\r\n\r\n")
}

fn status_of(reply: &str) -> u16 {
    reply
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP reply: {reply:?}"))
}

#[tokio::test]
async fn ui_assets_never_escape_the_assets_directory() {
    let b = Builder::new(UI);
    write_assets(b.dir());
    // Siblings of the assets directory a traversal would reach.
    write_file(&b.dir().join("secret.txt"), "TOP-SECRET-SIBLING", 0o600);
    let d = b.start().await;
    let addr = d.handle().ui_addr().expect("ui bound");
    let token = d.token();
    let paths = [
        "/../secret.txt",
        "/../admin-token",
        "/assets/../../secret.txt",
        "/%2e%2e/secret.txt",
        "/%2E%2E/admin-token",
        "/assets/..%2f..%2fsecret.txt",
        "/assets/%2e%2e%2f%2e%2e%2fadmin-token",
        "/..%5csecret.txt",
        "/%2f..%2fsecret.txt",
        "//../secret.txt",
        &format!("/{}", b_dir_absolute(&d)),
        &format!("/%2f{}", b_dir_absolute(&d).trim_start_matches('/')),
    ];
    for path in paths {
        let reply = support::raw(addr, &get_request(path, None))
            .await
            .unwrap_or_default();
        assert!(
            !reply.contains("TOP-SECRET-SIBLING") && !reply.contains(&token),
            "{path} escaped the assets directory: {reply}"
        );
        if !reply.is_empty() && status_of(&reply) == 200 {
            assert!(
                reply.contains("janus ui"),
                "{path}: 200 but not index.html: {reply}"
            );
        }
    }
}

/// `<data_dir>/secret.txt` as an absolute path, for absolute-path probes.
fn b_dir_absolute(d: &support::Daemon) -> String {
    d.data_dir().join("secret.txt").display().to_string()
}

#[tokio::test]
async fn every_api_path_needs_the_token_and_the_spa_never_answers_one() {
    let b = Builder::new(&format!("{ADMIN_TCP}{UI}"));
    write_assets(b.dir());
    let d = b.start().await;
    let token = d.token();
    let api_paths = [
        "/v1",
        "/v1/",
        "/v1/status/",
        "/v1//status",
        "/v1/requests/stream",
        "/v1/requests/some-id",
        "/v1/credentials/x/material",
        "/v1/identities?include_revoked=true",
        "/v1/no-such-endpoint",
    ];
    for (name, addr) in [
        ("admin", d.handle().admin_addr().expect("bound")),
        ("UI", d.handle().ui_addr().expect("bound")),
    ] {
        for path in api_paths {
            let reply = support::raw(addr, &get_request(path, None))
                .await
                .expect("an answer");
            assert_eq!(status_of(&reply), 401, "{name} {path}: {reply}");
            assert!(reply.contains("\"unauthorized\""), "{name} {path}: {reply}");
            if path == "/v1/requests/stream" {
                // Authorised, it never closes; the SSE tests cover it.
                continue;
            }
            let authed = support::raw(addr, &get_request(path, Some(&token)))
                .await
                .expect("an answer");
            assert!(
                !authed.contains("janus ui"),
                "{name} {path}: the SPA answered an API path: {authed}"
            );
            assert!(
                authed.contains("application/json"),
                "{name} {path}: not an API answer: {authed}"
            );
        }
        for method in ["DELETE", "PUT", "PATCH", "OPTIONS"] {
            let reply = support::raw(
                addr,
                &format!(
                    "{method} /v1/status HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                ),
            )
            .await
            .expect("an answer");
            assert_eq!(
                status_of(&reply),
                401,
                "{name} {method} /v1/status: {reply}"
            );
        }
    }
}

#[tokio::test]
async fn oversized_request_bodies_are_refused() {
    use janus_daemon::admin::MAX_BODY_BYTES;
    let d = Builder::new(ADMIN_TCP).start().await;
    let material = "k".repeat(MAX_BODY_BYTES);
    let body = json!({ "upstream": "openrouter", "material": { "api_key": material } });
    for client in [d.socket(), d.tcp(Some(&d.token()))] {
        let reply = client.post("/v1/credentials", Some(body.clone())).await;
        assert_eq!(reply.status, 413, "{}", reply.text());
        assert_eq!(reply.error_code(), "payload_too_large");
    }
    // Far past the limit, the client is still sending when the server has
    // decided; it still gets to read the 413 (the rest is drained).
    let huge = json!({ "upstream": "openrouter", "material": { "api_key": "k".repeat(4 * MAX_BODY_BYTES) } });
    for client in [d.socket(), d.tcp(Some(&d.token()))] {
        let reply = client.post("/v1/credentials", Some(huge.clone())).await;
        assert_eq!(reply.status, 413, "{}", reply.text());
    }
    let listed = d.socket().get("/v1/credentials").await.json();
    assert_eq!(listed["credentials"], json!([]), "nothing was added");
    // Without the token the body is never read. A client that waits for
    // `100 Continue` before sending it gets the 401 instead: the token is
    // checked before anything asks for the body. (A stranger's body is not
    // drained either, so a client that sends one regardless may see the
    // connection close under it.)
    let addr = d.handle().admin_addr().expect("bound");
    let reply = support::raw(
        addr,
        &format!(
            "POST /v1/credentials HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nexpect: 100-continue\r\nconnection: close\r\n\r\n",
            body.to_string().len()
        ),
    )
    .await
    .expect("an answer");
    assert!(reply.starts_with("HTTP/1.1 401 "), "{reply}");
}

#[tokio::test]
async fn live_tails_are_capped_and_a_closed_one_frees_its_slot() {
    use janus_daemon::admin::MAX_TAILS;
    let d = Builder::new(ADMIN_TCP).start().await;
    let token = d.token();
    let mut open = Vec::new();
    for _ in 0..MAX_TAILS {
        // A client each: one connection per tail.
        let mut tail = d.tcp(Some(&token)).sse("/v1/requests/stream").await;
        assert_eq!(tail.status, 200);
        assert_eq!(tail.next(WAIT).await.expect("ping").event, "ping");
        open.push(tail);
    }
    let refused = d.tcp(Some(&token)).get("/v1/requests/stream").await;
    assert_eq!(refused.status, 503, "{}", refused.text());
    assert_eq!(refused.error_code(), "too_many_streams");

    drop(open.pop());
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let mut tail = d.tcp(Some(&token)).sse("/v1/requests/stream").await;
        if tail.status == 200 {
            assert_eq!(tail.next(WAIT).await.expect("ping").event, "ping");
            break;
        }
        assert_eq!(tail.status, 503);
        assert!(
            std::time::Instant::now() < deadline,
            "the slot was never freed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn connections_beyond_the_limit_are_closed_unanswered() {
    use janus_daemon::admin::MAX_CONNECTIONS;
    let d = Builder::new(ADMIN_TCP).start().await;
    let addr = d.handle().admin_addr().expect("bound");
    let token = d.token();
    // Idle connections that never send a request, up to the limit.
    let mut idle = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        idle.push(tokio::net::TcpStream::connect(addr).await.expect("connect"));
    }
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        // Once every idle connection is accepted, a new one is closed.
        if support::raw(addr, &get_request("/v1/status", Some(&token)))
            .await
            .is_none()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the limit never applied"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    idle.clear();
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Some(reply) = support::raw(addr, &get_request("/v1/status", Some(&token))).await {
            assert_eq!(status_of(&reply), 200, "{reply}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "slots never freed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_client_that_never_finishes_its_headers_is_disconnected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let d = Builder::new(ADMIN_TCP).start().await;
    let addr = d.handle().admin_addr().expect("bound");
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET /v1/status HTTP/1.1\r\nhost: localhost\r\n")
        .await
        .expect("write");
    let started = std::time::Instant::now();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut rest)).await;
    assert!(
        closed.is_ok(),
        "a half-sent request held its connection open"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "closed too early: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn status_reports_the_bound_data_plane_address() {
    let bound: std::net::SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let b = Builder::new("");
    assert_ne!(b.data_plane(), bound);
    let d = b.data_plane_bound(bound).start().await;
    let status = d.socket().get("/v1/status").await.json();
    assert_eq!(status["listeners"]["data_plane"], bound.to_string());
}

/// Every entry of `dir`, recursively, for "nothing was created" checks.
fn tree(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            out.extend(tree(&path));
        }
        out.push(path);
    }
    out.sort();
    out
}

/// A config, how to prepare its data directory, and the refusal expected.
type Refusal = (&'static str, fn(&std::path::Path), fn(&AdminError) -> bool);

#[tokio::test]
async fn check_config_refuses_what_start_refuses_and_creates_nothing() {
    use janus_daemon::admin::check_config;

    // A good config passes, binds nothing and writes nothing.
    let port = free_port();
    let b = Builder::new(&format!("[admin]\nbind = \"127.0.0.1:{port}\"\n{UI}"));
    write_assets(b.dir());
    let before = tree(b.dir());
    check_config(&b.config(), b.dir()).expect("valid");
    assert_eq!(tree(b.dir()), before, "check_config created files");
    assert!(!accepts(format!("127.0.0.1:{port}").parse().expect("addr")));

    // A valid existing token file passes too.
    write_file(&b.dir().join("admin-token"), &"t".repeat(40), 0o600);
    check_config(&b.config(), b.dir()).expect("valid token");

    let refusals: Vec<Refusal> = vec![
        (
            "[admin]\nbind = \"0.0.0.0:0\"\nallow_non_loopback = true\n",
            |_| {},
            |e| matches!(e, AdminError::Config(_)),
        ),
        (
            "[admin]\nbind = \"127.0.0.1:0\"\ntls = { cert = \"{dir}/c.pem\", key = \"{dir}/k.pem\" }\n",
            |_| {},
            |e| matches!(e, AdminError::Tls { .. }),
        ),
        (
            "[ui]\nbind = \"127.0.0.1:0\"\nassets_dir = \"{dir}/empty\"\n",
            |dir| std::fs::create_dir(dir.join("empty")).expect("mkdir"),
            |e| matches!(e, AdminError::UiAssets { .. }),
        ),
        (
            ADMIN_TCP,
            |dir| write_file(&dir.join("admin-token"), &"t".repeat(40), 0o644),
            |e| matches!(e, AdminError::Token { .. }),
        ),
        (
            ADMIN_TCP,
            |dir| write_file(&dir.join("admin-token"), "short", 0o600),
            |e| matches!(e, AdminError::Token { .. }),
        ),
    ];
    for (config, setup, expected) in refusals {
        let b = Builder::new(config);
        setup(b.dir());
        let before = tree(b.dir());
        let err = check_config(&b.config(), b.dir()).expect_err(config);
        assert!(expected(&err), "{config}: wrong refusal: {err}");
        assert_eq!(
            tree(b.dir()),
            before,
            "{config}: check_config created files"
        );
        // And start refuses the same way.
        match b.try_start().await {
            Err((e, _)) => assert!(expected(&e), "{config}: start refused differently: {e}"),
            Ok(_) => panic!("{config}: start accepted what check_config refused"),
        }
    }

    let gone = tempfile::tempdir().expect("tempdir").path().join("missing");
    // A first start's data directory does not exist yet: that passes, and
    // the check does not create it.
    check_config(&Builder::new("").config(), &gone).expect("absent data dir is a first start");
    assert!(!gone.exists(), "check_config created the data directory");
    let file = tempfile::NamedTempFile::new().expect("file");
    let err = check_config(&Builder::new("").config(), file.path()).expect_err("not a dir");
    assert!(matches!(err, AdminError::DataDir { .. }), "{err}");
}

const INSTANCES: &str = r#"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }

[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "https://api.anthropic.com"
strategy = "round_robin"
failover = true
[upstream.credentials]
kind = "api_key"

[[upstream]]
name = "claude"
implementation = "anthropic"
path_prefix = "/claude"
base_url = "https://api.anthropic.com"
strategy = "sticky"
[upstream.credentials]
kind = "oauth"
tags = ["max"]
"#;

#[tokio::test]
async fn instances_scope_credential_listing_default_and_status_and_tags_are_settable() {
    let d = Builder::new(INSTANCES).start().await;
    let s = d.socket();
    let far = (Utc::now() + chrono::Duration::days(30)).timestamp_millis();
    let oauth = |r: &str, tags: Value| {
        json!({
            "upstream": "anthropic",
            "ref": r,
            "tags": tags,
            "material": { "claudeAiOauth": {
                "accessToken": format!("at-{r}-SECRET"),
                "refreshToken": format!("rt-{r}-SECRET"),
                "expiresAt": far,
            } },
        })
    };
    for body in [
        json!({ "upstream": "anthropic", "ref": "key-1", "material": { "api_key": "sk-ant-api03-KEYONE" } }),
        json!({ "upstream": "anthropic", "ref": "key-2", "material": { "api_key": "sk-ant-api03-KEYTWO" } }),
        oauth("max-1", json!(["max", "personal"])),
        oauth("max-2", json!([])),
    ] {
        let added = s.post("/v1/credentials", Some(body)).await;
        assert_eq!(added.status, 201, "{}", added.text());
    }
    let refs = |reply: support::Reply| -> Vec<String> {
        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.json()["credentials"]
            .as_array()
            .expect("list")
            .iter()
            .map(|c| c["ref"].as_str().expect("ref").to_owned())
            .collect()
    };
    // `?upstream=` names an instance; an implementation name no instance
    // has would list everything of it, but here `anthropic` is an instance.
    assert_eq!(
        refs(s.get("/v1/credentials?upstream=anthropic").await),
        ["key-1", "key-2"]
    );
    assert_eq!(
        refs(s.get("/v1/credentials?upstream=claude").await),
        ["max-1"]
    );
    assert_eq!(
        refs(s.get("/v1/credentials?upstream=openai").await),
        Vec::<String>::new()
    );
    let unknown = s.get("/v1/credentials?upstream=nope").await;
    assert_eq!(unknown.status, 400);
    assert_eq!(unknown.error_code(), "invalid_request");

    // Tags: normalised, validated, only on pool slots; they change what an
    // instance admits at once.
    let tagged = s
        .put(
            "/v1/credentials/max-2/tags",
            json!({ "tags": ["team", "max", "max"] }),
        )
        .await;
    assert_eq!(tagged.status, 200, "{}", tagged.text());
    assert_eq!(tagged.json()["tags"], json!(["max", "team"]));
    assert_eq!(
        refs(s.get("/v1/credentials?upstream=claude").await),
        ["max-1", "max-2"]
    );
    let bad = s
        .put("/v1/credentials/max-2/tags", json!({ "tags": ["a b"] }))
        .await;
    assert_eq!(bad.status, 400, "{}", bad.text());
    assert_eq!(bad.error_code(), "invalid_tag");
    let missing = s
        .put("/v1/credentials/nope/tags", json!({ "tags": [] }))
        .await;
    assert_eq!(missing.status, 404, "{}", missing.text());

    // The default peek honours the instance's filter and strategy, and
    // never advances round-robin.
    for _ in 0..3 {
        let peek = s.get("/v1/credentials/default?upstream=anthropic").await;
        assert_eq!(peek.status, 200, "{}", peek.text());
        assert_eq!(peek.json()["ref"], "key-1");
    }
    let sticky = |who: &str| {
        let s = &s;
        let path = format!("/v1/credentials/default?upstream=claude&identity={who}");
        async move {
            s.get(&path).await.json()["ref"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let alice = sticky("alice").await;
    assert!(["max-1", "max-2"].contains(&alice.as_str()));
    assert_eq!(
        sticky("alice").await,
        alice,
        "the same identity, the same pick"
    );
    let no_identity = s.get("/v1/credentials/default?upstream=claude").await;
    assert_eq!(
        no_identity.json()["ref"],
        "max-1",
        "without an identity: ordered"
    );

    // Status: every instance with its implementation, strategy, filter and
    // the credentials it admits right now.
    let status = s.get("/v1/status").await.json();
    let ups = status["upstreams"].as_array().expect("upstreams");
    assert_eq!(ups[0]["name"], "anthropic");
    assert_eq!(ups[0]["implementation"], "anthropic");
    assert_eq!(ups[0]["strategy"], "round_robin");
    assert_eq!(ups[0]["failover"], true);
    assert_eq!(ups[0]["credentials"], json!({ "kind": "api_key" }));
    assert_eq!(ups[0]["eligible"], json!(["key-1", "key-2"]));
    assert_eq!(ups[1]["name"], "claude");
    assert_eq!(ups[1]["implementation"], "anthropic");
    assert_eq!(ups[1]["strategy"], "sticky");
    assert_eq!(ups[1]["failover"], false);
    assert_eq!(
        ups[1]["credentials"],
        json!({ "kind": "oauth", "tags": ["max"] })
    );
    assert_eq!(ups[1]["eligible"], json!(["max-1", "max-2"]));
}

#[tokio::test]
async fn a_legacy_upstream_reports_ordered_with_no_filter_and_admits_everything() {
    let d = Builder::new(UPSTREAMS).start().await;
    let s = d.socket();
    add_openrouter(&s, "sk-or-v1-LEGACYKEY000001").await;
    let status = s.get("/v1/status").await.json();
    let up = &status["upstreams"][0];
    assert_eq!(up["name"], "openrouter");
    assert_eq!(up["implementation"], "openrouter");
    assert_eq!(up["strategy"], "ordered");
    assert_eq!(up["failover"], false);
    assert_eq!(up["credentials"], Value::Null);
    assert_eq!(up["eligible"].as_array().map(Vec::len), Some(1));
}
