//! `janus` end to end: the real binary against the real daemon
//! (`docs/admin-surface#cli`). Every test starts a daemon over its own
//! temporary data directory and drives `janus` as a child process.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use serde_json::Value;
use support::{contains, files_under, janus, TestDaemon};

const ADMIN_TCP: &str = "[admin]\nbind = \"127.0.0.1:0\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn status_command_json() {
    let d = TestDaemon::start("").await;
    let s = d.janus(&["status", "--json"]).await.ok_json();
    assert_eq!(
        s["listeners"]["admin_socket"].as_str(),
        Some(d.data_dir.join("admin.sock").to_str().unwrap())
    );
    assert_eq!(s["plugin"]["mode"], "none");
    assert_eq!(s["plugin"]["health"], "unknown");
    assert_eq!(s["upstreams"][0]["name"], "openrouter");
    assert_eq!(s["upstreams"][0]["binding"], "opaque");
    assert_eq!(s["request_log"]["enabled"], true);
    assert!(s["version"].is_string() && s["started_at"].is_string());

    let human = d.janus(&["status"]).await;
    human.ok();
    for needle in [
        "version",
        "admin socket",
        "openrouter",
        "opaque",
        "plugin",
        "none",
    ] {
        assert!(human.stdout.contains(needle), "{needle}: {}", human.stdout);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn default_address_resolves_data_dir_like_the_daemon() {
    let d = TestDaemon::start("").await;
    // $JANUS_DATA_DIR, with no --admin.
    let via_env = janus(&["status", "--json"])
        .env("JANUS_DATA_DIR", &d.data_dir)
        .run()
        .await
        .ok_json();
    assert_eq!(via_env["plugin"]["mode"], "none");
    // The config's data_dir wins over $JANUS_DATA_DIR, as in the daemon.
    let via_config = janus(&[
        "--config",
        d.config_path.to_str().unwrap(),
        "status",
        "--json",
    ])
    .env("JANUS_DATA_DIR", d.root.path().join("elsewhere"))
    .run()
    .await
    .ok_json();
    assert_eq!(via_config["listeners"], via_env["listeners"]);
    // $HOME/.janus is the last resort; here it holds no daemon.
    let none = janus(&["status"]).run().await;
    assert_eq!(none.code, 3, "{}", none.stderr);
    assert!(
        none.stderr
            .contains("/nonexistent-janus-test-home/.janus/admin.sock"),
        "{}",
        none.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_daemon_exits_3_with_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let sock = format!("unix://{}/admin.sock", dir.path().display());
    let r = janus(&["--admin", &sock, "status"]).run().await;
    assert_eq!(r.code, 3);
    assert!(
        r.stderr.contains("cannot reach the admin surface"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("JANUS_ADMIN"), "{}", r.stderr);
    let j = janus(&["--admin", &sock, "status", "--json"]).run().await;
    assert_eq!(j.code, 3);
    assert_eq!(j.error_json()["error"]["code"], "unreachable");

    let bad = janus(&["--admin", "localhost:7071", "status"]).run().await;
    assert_eq!(bad.code, 2, "{}", bad.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn token_file_flag_not_argv_secret() {
    let d = TestDaemon::start(ADMIN_TCP).await;
    let url = d.admin_url().await;
    let token = d.admin_token();
    let token_file = d.root.path().join("operator-token");
    std::fs::write(&token_file, format!("{token}\n")).unwrap();
    let token_file = token_file.to_str().unwrap();

    // --token-file.
    let s = janus(&[
        "--admin",
        &url,
        "--token-file",
        token_file,
        "status",
        "--json",
    ])
    .run()
    .await
    .ok_json();
    assert_eq!(s["listeners"]["admin_tcp"]["url"], url.as_str());
    // $JANUS_TOKEN_FILE.
    janus(&["--admin", &url, "status"])
        .env("JANUS_TOKEN_FILE", token_file)
        .run()
        .await
        .ok();
    // $JANUS_ADMIN_TOKEN holds the token itself.
    janus(&["--admin", &url, "status"])
        .env("JANUS_ADMIN_TOKEN", &token)
        .run()
        .await
        .ok();
    // On the daemon's host: <data_dir>/admin-token.
    janus(&["--admin", &url, "status"])
        .env("JANUS_DATA_DIR", &d.data_dir)
        .run()
        .await
        .ok();
    // $JANUS_ADMIN names the address as --admin does.
    janus(&["status"])
        .env("JANUS_ADMIN", &url)
        .env("JANUS_TOKEN_FILE", token_file)
        .run()
        .await
        .ok();

    // No token: the daemon refuses, and the CLI says where a token comes from.
    let refused = janus(&["--admin", &url, "status"]).run().await;
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("unauthorized"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("--token-file"),
        "{}",
        refused.stderr
    );
    let wrong = d.root.path().join("wrong-token");
    std::fs::write(&wrong, "not-the-admin-token-at-all-0123456789").unwrap();
    let wrong = janus(&[
        "--admin",
        &url,
        "--token-file",
        wrong.to_str().unwrap(),
        "status",
        "--json",
    ])
    .run()
    .await;
    assert_eq!(wrong.code, 1);
    assert_eq!(wrong.error_json()["error"]["status"], 401);

    // There is no flag that takes the token itself.
    for flag in ["--token", "--admin-token"] {
        let r = janus(&["--admin", &url, flag, &token, "status"])
            .run()
            .await;
        assert_eq!(r.code, 2, "{flag} must not exist: {}", r.stdout);
        assert!(!r.stdout.contains(&token));
    }
    // Help never displays the environment's secret values.
    let help = janus(&["--help"])
        .env("JANUS_ADMIN_TOKEN", &token)
        .env("JANUS_TOKEN_FILE", token_file)
        .run()
        .await;
    help.ok();
    assert!(!help.stdout.contains(&token), "{}", help.stdout);
    assert!(!help.stdout.contains(token_file), "{}", help.stdout);
    assert!(help.stdout.contains("JANUS_ADMIN_TOKEN"));
    // The token is never echoed by any command.
    let out = janus(&["--admin", &url, "credential", "list"])
        .env("JANUS_ADMIN_TOKEN", &token)
        .run()
        .await;
    out.ok();
    assert!(!out.stdout.contains(&token) && !out.stderr.contains(&token));
}

#[tokio::test(flavor = "multi_thread")]
async fn https_admin_listener_with_pinned_ca_file() {
    let root = tempfile::tempdir().unwrap();
    let pair = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = root.path().join("admin.pem");
    let key = root.path().join("admin-key.pem");
    std::fs::write(&cert, pair.cert.pem()).unwrap();
    std::fs::write(&key, pair.signing_key.serialize_pem()).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let d = TestDaemon::start(&format!(
        "[admin]\nbind = \"127.0.0.1:0\"\ntls = {{ cert = \"{}\", key = \"{}\" }}\n",
        cert.display(),
        key.display()
    ))
    .await;
    let url = d.admin_url().await;
    assert!(url.starts_with("https://"), "{url}");
    let token = d.admin_token();

    let ok = janus(&[
        "--admin",
        &url,
        "--ca-file",
        cert.to_str().unwrap(),
        "status",
        "--json",
    ])
    .env("JANUS_ADMIN_TOKEN", &token)
    .run()
    .await
    .ok_json();
    assert_eq!(ok["listeners"]["admin_tcp"]["tls"], true);
    // Without the pinned certificate the self-signed listener is not trusted.
    let untrusted = janus(&["--admin", &url, "status"])
        .env("JANUS_ADMIN_TOKEN", &token)
        .run()
        .await;
    assert_eq!(untrusted.code, 3, "{}", untrusted.stderr);
    // --ca-file makes no sense for plain http.
    let misuse = janus(&[
        "--admin",
        "http://127.0.0.1:1",
        "--ca-file",
        cert.to_str().unwrap(),
        "status",
    ])
    .run()
    .await;
    assert_eq!(misuse.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_add_reads_stdin() {
    let d = TestDaemon::start("").await;
    let key = "sk-or-v1-stdin-secret-4f9a0c";
    let added = d
        .cli(&[
            "credential",
            "add",
            "--upstream",
            "openrouter",
            "--ref",
            "or-main",
            "--label",
            "main",
            "--api-key-stdin",
            "--json",
        ])
        .stdin(format!("{key}\n").as_bytes())
        .run()
        .await;
    let meta = added.ok_json();
    assert_eq!(meta["ref"], "or-main");
    assert_eq!(meta["kind"], "api_key");
    assert_eq!(meta["status"], "active");
    assert_eq!(meta["label"], "main");
    assert!(!added.stdout.contains(key) && !added.stderr.contains(key));
    // The material reached the daemon's pool slot.
    assert!(
        files_under(&d.data_dir)
            .iter()
            .any(|(_, b)| contains(b, key)),
        "the key is in the pool"
    );

    // Human list and show: metadata only.
    for args in [
        vec!["credential", "list"],
        vec!["credential", "show", "or-main"],
    ] {
        let r = d.janus(&args).await;
        r.ok();
        assert!(
            r.stdout.contains("or-main") && r.stdout.contains("active"),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains(key));
    }
    let listed = d
        .janus(&["credential", "list", "--upstream", "openrouter", "--json"])
        .await
        .ok_json();
    assert_eq!(listed["credentials"].as_array().unwrap().len(), 1);

    // Empty stdin is refused locally, before anything is sent.
    let empty = d
        .cli(&[
            "credential",
            "add",
            "--upstream",
            "openrouter",
            "--api-key-stdin",
        ])
        .stdin(b"\n")
        .run()
        .await;
    assert_eq!(empty.code, 2, "{}", empty.stderr);
    // A key as an argument is not accepted, and a source is required.
    let argv = d
        .janus(&["credential", "add", "--upstream", "openrouter", key])
        .await;
    assert_eq!(argv.code, 2);
    let none = d
        .janus(&["credential", "add", "--upstream", "openrouter"])
        .await;
    assert_eq!(none.code, 2);
    // A duplicate ref is the daemon's refusal, with its stable code.
    let dup = d
        .cli(&[
            "credential",
            "add",
            "--upstream",
            "openrouter",
            "--ref",
            "or-main",
            "--api-key-stdin",
            "--json",
        ])
        .stdin(b"sk-other")
        .run()
        .await;
    assert_eq!(dup.code, 1);
    assert_eq!(dup.error_json()["error"]["code"], "credential_exists");
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_from_file_and_rotate_replace_material() {
    let d = TestDaemon::start("").await;
    let first = d.root.path().join("key-1");
    std::fs::write(&first, "sk-or-v1-from-file-1111\n").unwrap();
    let added = d
        .janus(&[
            "credential",
            "add",
            "--upstream",
            "openrouter",
            "--from-file",
            first.to_str().unwrap(),
            "--json",
        ])
        .await
        .ok_json();
    let r = added["ref"].as_str().expect("generated ref").to_owned();

    // A JSON credential file is sent as its document.
    let second = d.root.path().join("key-2.json");
    std::fs::write(&second, r#"{"api_key": "sk-or-v1-rotated-2222"}"#).unwrap();
    let rotated = d
        .janus(&[
            "credential",
            "rotate",
            &r,
            "--from-file",
            second.to_str().unwrap(),
        ])
        .await;
    rotated.ok();
    assert!(rotated.stdout.contains(&format!("rotated credential {r}")));
    let files = files_under(&d.data_dir);
    assert!(files
        .iter()
        .any(|(_, b)| contains(b, "sk-or-v1-rotated-2222")));
    assert!(!files
        .iter()
        .any(|(_, b)| contains(b, "sk-or-v1-from-file-1111")));

    let missing = d
        .janus(&[
            "credential",
            "rotate",
            &r,
            "--from-file",
            "/nonexistent/key",
        ])
        .await;
    assert_eq!(missing.code, 2);
    let unknown = d
        .cli(&[
            "credential",
            "rotate",
            "no-such-ref",
            "--api-key-stdin",
            "--json",
        ])
        .stdin(b"sk-x")
        .run()
        .await;
    assert_eq!(unknown.code, 1);
    assert_eq!(
        unknown.error_json()["error"]["code"],
        "credential_not_found"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_enable_disable_default_refresh_rm() {
    let d = TestDaemon::start("").await;
    d.cli(&[
        "credential",
        "add",
        "--upstream",
        "openrouter",
        "--ref",
        "or-a",
        "--api-key-stdin",
    ])
    .stdin(b"sk-or-v1-aaaa")
    .run()
    .await
    .ok();
    let peek = d
        .janus(&[
            "credential",
            "default",
            "--upstream",
            "openrouter",
            "--json",
        ])
        .await
        .ok_json();
    assert_eq!(peek["ref"], "or-a");

    let disabled = d
        .janus(&["credential", "disable", "or-a", "--json"])
        .await
        .ok_json();
    assert_eq!(disabled["status"], "disabled");
    let no_default = d
        .janus(&[
            "credential",
            "default",
            "--upstream",
            "openrouter",
            "--json",
        ])
        .await;
    assert_eq!(no_default.code, 1);
    assert_eq!(
        no_default.error_json()["error"]["code"],
        "no_default_credential"
    );
    let enabled = d.janus(&["credential", "enable", "or-a"]).await;
    enabled.ok();
    assert!(enabled.stdout.contains("enabled credential or-a"));
    assert!(enabled.stdout.contains("active"));

    // An API key has nothing to refresh: the daemon's 409, reported as such.
    let refresh = d.janus(&["credential", "refresh", "or-a"]).await;
    assert_eq!(refresh.code, 1);
    assert!(
        refresh.stderr.contains("not_refreshable") && refresh.stderr.contains("409"),
        "{}",
        refresh.stderr
    );

    let rm = d.janus(&["credential", "rm", "or-a"]).await;
    rm.ok();
    assert!(rm.stdout.contains("removed credential or-a"));
    let gone = d.janus(&["credential", "show", "or-a", "--json"]).await;
    assert_eq!(gone.code, 1);
    assert_eq!(gone.error_json()["error"]["code"], "credential_not_found");
    let again = d.janus(&["credential", "delete", "or-a"]).await;
    assert_eq!(again.code, 1, "the delete alias reaches the same endpoint");
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_create_prints_token_once() {
    let d = TestDaemon::start("").await;
    let created = d
        .janus(&[
            "identity",
            "create",
            "--identity",
            "sandbox-7",
            "--claim",
            "team=infra",
            "--label",
            "ci",
            "--expires-in",
            "1h",
        ])
        .await;
    created.ok();
    let token = created.stdout.trim().to_owned();
    assert_eq!(
        created.stdout,
        format!("{token}\n"),
        "stdout is the token alone"
    );
    assert!(token.len() >= 20, "{token}");
    assert!(
        !created.stderr.contains(&token),
        "the token is printed once, on stdout"
    );
    assert!(created.stderr.contains("shown once"), "{}", created.stderr);

    // Nothing else ever shows it.
    let listed = d.janus(&["identity", "list", "--json"]).await;
    let entries = listed.ok_json();
    let entry = &entries["identities"][0];
    assert_eq!(entry["identity"], "sandbox-7");
    assert_eq!(entry["claims"]["team"], "infra");
    assert_eq!(entry["label"], "ci");
    assert!(entry["expires_at"].is_string());
    assert!(!listed.stdout.contains(&token));
    let id = entry["id"].as_str().unwrap().to_owned();
    let shown = d.janus(&["identity", "show", &id]).await;
    shown.ok();
    assert!(shown.stdout.contains("sandbox-7") && !shown.stdout.contains(&token));

    // The printed token is the caller's real lookup key.
    d.call(Some(&token)).await;
    d.flush().await;
    let log = d
        .janus(&["log", "query", "--identity", "sandbox-7", "--json"])
        .await
        .ok_json();
    assert_eq!(log["requests"].as_array().unwrap().len(), 1, "{log}");
    assert_eq!(log["requests"][0]["caller_identity"], "sandbox-7");

    // --json carries the token in the one creation document.
    let j = d
        .janus(&["identity", "create", "--identity", "sandbox-8", "--json"])
        .await;
    let doc = j.ok_json();
    let t = doc["token"].as_str().unwrap();
    assert_eq!(j.stdout.matches(t).count(), 1);
    assert!(!j.stderr.contains(t));
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_list_filters_revoke_and_rm() {
    let d = TestDaemon::start("").await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let doc = d
            .janus(&["identity", "create", "--identity", "worker", "--json"])
            .await
            .ok_json();
        ids.push(doc["id"].as_str().unwrap().to_owned());
    }
    d.janus(&["identity", "create", "--identity", "other"])
        .await
        .ok();

    let revoked = d
        .janus(&["identity", "revoke", &ids[0], "--json"])
        .await
        .ok_json();
    assert!(revoked["revoked_at"].is_string());
    let again = d.janus(&["identity", "revoke", &ids[0]]).await;
    again.ok();
    assert!(again.stdout.contains("revoked identity"), "idempotent");

    let count = |v: &Value| v["identities"].as_array().unwrap().len();
    let active = d
        .janus(&["identity", "list", "--identity", "worker", "--json"])
        .await
        .ok_json();
    assert_eq!(count(&active), 1);
    let all = d
        .janus(&[
            "identity",
            "list",
            "--identity",
            "worker",
            "--include-revoked",
            "--json",
        ])
        .await
        .ok_json();
    assert_eq!(count(&all), 2);
    let human = d.janus(&["identity", "list", "--include-revoked"]).await;
    human.ok();
    assert!(human.stdout.starts_with("ID ") && human.stdout.contains("other"));

    d.janus(&["identity", "rm", &ids[0]]).await.ok();
    let gone = d.janus(&["identity", "show", &ids[0], "--json"]).await;
    assert_eq!(gone.code, 1);
    assert_eq!(gone.error_json()["error"]["code"], "identity_not_found");

    let bad = d
        .janus(&[
            "identity",
            "create",
            "--identity",
            "x",
            "--expires-in",
            "soon",
        ])
        .await;
    assert_eq!(bad.code, 2);
    let claim = d
        .janus(&[
            "identity",
            "create",
            "--identity",
            "x",
            "--claim",
            "novalue",
        ])
        .await;
    assert_eq!(claim.code, 2);
    let both = d
        .janus(&[
            "identity",
            "create",
            "--identity",
            "x",
            "--expires-in",
            "1h",
            "--expires-at",
            "2030-01-01T00:00:00Z",
        ])
        .await;
    assert_eq!(both.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_query_filters_paginates_and_shows() {
    let d = TestDaemon::start("").await;
    for _ in 0..3 {
        d.call(None).await;
    }
    let token = d
        .janus(&["identity", "create", "--identity", "alice"])
        .await
        .stdout
        .trim()
        .to_owned();
    d.call(Some(&token)).await;
    d.flush().await;

    let rejected = d
        .janus(&[
            "log",
            "query",
            "--outcome",
            "rejected_identity",
            "--limit",
            "2",
            "--json",
        ])
        .await
        .ok_json();
    let page = rejected["requests"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert!(page
        .iter()
        .all(|r| r["outcome"] == "rejected_identity" && r["kind"] == "no_material"));
    let next = rejected["next"].as_str().expect("a next cursor").to_owned();
    let older = d
        .janus(&[
            "log",
            "query",
            "--outcome",
            "rejected_identity",
            "--limit",
            "2",
            "--before",
            &next,
            "--json",
        ])
        .await
        .ok_json();
    assert_eq!(older["requests"].as_array().unwrap().len(), 1);
    assert!(older["next"].is_null());

    let alice = d
        .janus(&[
            "log",
            "query",
            "--identity",
            "alice",
            "--upstream",
            "openrouter",
            "--since",
            "10m",
            "--json",
        ])
        .await
        .ok_json();
    let alice = alice["requests"].as_array().unwrap();
    assert_eq!(alice.len(), 1);
    assert_ne!(alice[0]["outcome"], "rejected_identity");
    let future = d
        .janus(&["log", "query", "--since", "2999-01-01T00:00:00Z", "--json"])
        .await
        .ok_json();
    assert!(future["requests"].as_array().unwrap().is_empty());

    let human = d.janus(&["log", "query", "--limit", "2"]).await;
    human.ok();
    assert!(human.stdout.starts_with("RECEIVED"), "{}", human.stdout);
    assert!(
        human.stderr.contains("--before"),
        "the next cursor is offered"
    );

    let id = alice[0]["request_id"].as_str().unwrap();
    let shown = d.janus(&["log", "show", id]).await;
    shown.ok();
    assert!(shown.stdout.contains("alice") && shown.stdout.contains(id));
    let missing = d.janus(&["log", "show", "no-such-request", "--json"]).await;
    assert_eq!(missing.code, 1);
    assert_eq!(missing.error_json()["error"]["code"], "request_not_found");
    let bad = d.janus(&["log", "query", "--outcome", "exploded"]).await;
    assert_eq!(bad.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_tail_follows_filters_and_reconnects_after_restart() {
    let mut d = TestDaemon::start("").await;
    let token = d
        .janus(&["identity", "create", "--identity", "bob"])
        .await
        .stdout
        .trim()
        .to_owned();
    let mut tail = d
        .cli(&["log", "tail", "--identity", "bob", "--json"])
        .spawn();
    tail.stderr_until("following the request log").await;

    d.call(None).await; // filtered out: no identity
    d.call(Some(&token)).await;
    d.flush().await;
    let first: Value = serde_json::from_str(&tail.stdout_line().await).unwrap();
    assert_eq!(first["caller_identity"], "bob");

    // The daemon restarts; the tail reconnects on its own and the next
    // record still arrives.
    d.restart().await;
    tail.stderr_until("stream lost").await;
    tail.stderr_until("reconnected").await;
    d.call(Some(&token)).await;
    d.flush().await;
    let second: Value = serde_json::from_str(&tail.stdout_line().await).unwrap();
    assert_eq!(second["caller_identity"], "bob");
    assert_ne!(second["request_id"], first["request_id"], "no duplicate");

    // A record committed while the tail is disconnected is not lost: the
    // tail closes the gap with a query when it reconnects.
    d.stop().await;
    tail.stderr_until("stream lost").await;
    tail.stderr_until("still disconnected").await;
    d.boot().await;
    d.call(Some(&token)).await;
    d.flush().await;
    let gap: Value = serde_json::from_str(&tail.stdout_line().await).unwrap();
    tail.stderr_until("reconnected").await;
    assert_eq!(gap["caller_identity"], "bob");
    assert_ne!(gap["request_id"], second["request_id"]);
    tail.child.kill().await.unwrap();

    // Human output: one line per record.
    let mut human = d
        .cli(&["log", "tail", "--outcome", "rejected_identity"])
        .spawn();
    human.stderr_until("following").await;
    d.call(None).await;
    d.flush().await;
    let line = human.stdout_line().await;
    assert!(
        line.contains("rejected_identity(no_material)") && line.contains("POST"),
        "{line}"
    );
    human.child.kill().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn log_commands_report_a_disabled_request_log() {
    let d = TestDaemon::start("[request_log]\nenabled = false\n").await;
    let q = d.janus(&["log", "query", "--json"]).await.ok_json();
    assert_eq!(q["disabled"], true);
    let human = d.janus(&["log", "query"]).await;
    human.ok();
    assert!(human.stderr.contains("disabled"));
    let (code, stderr) = d.cli(&["log", "tail"]).spawn().exit().await;
    assert_eq!(code, 1);
    assert!(stderr.contains("request log is disabled"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_status_and_check_on_the_socket_transport() {
    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join("plugin");
    std::fs::create_dir(&plugin_dir).unwrap();
    std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let d = TestDaemon::start(&format!(
        "[plugin]\nendpoint = \"unix://{}/plugin.sock\"\n",
        plugin_dir.display()
    ))
    .await;
    let s = d.janus(&["plugin", "status", "--json"]).await.ok_json();
    assert_eq!(s["mode"], "unix");
    let human = d.janus(&["plugin", "status"]).await;
    human.ok();
    assert!(human.stdout.contains("unix") && human.stdout.contains("plugin.sock"));

    // Nothing serves the socket: the walk passes, the connection fails, and
    // the command says so with exit 1.
    let check = d.janus(&["plugin", "check", "--json"]).await;
    assert_eq!(check.code, 1, "{}", check.stderr);
    let report: Value = serde_json::from_str(&check.stdout).unwrap();
    assert_eq!(report["passed"], false);
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps[0]["step"], "path_permissions");
    assert_eq!(steps[0]["passed"], true);
    assert!(steps
        .iter()
        .any(|s| s["step"] == "connect" && s["passed"] == false));
    let human = d.janus(&["plugin", "check"]).await;
    assert_eq!(human.code, 1);
    assert!(human.stdout.contains("FAIL connect"), "{}", human.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_check_without_a_plugin_is_the_daemons_409() {
    let d = TestDaemon::start("").await;
    let r = d.janus(&["plugin", "check", "--json"]).await;
    assert_eq!(r.code, 1);
    assert_eq!(r.error_json()["error"]["code"], "no_plugin");
    assert_eq!(r.error_json()["error"]["status"], 409);
}

/// A TLS plugin endpoint built from `tls init`'s plugin files: TLS 1.3,
/// answering a handshake that passes its client verification with an
/// HTTP/2 SETTINGS frame. With `require_daemon_cert`, only `daemon.pem` is
/// accepted as a client; without, any client (or none) is.
async fn tls_plugin(
    plugin_cert: &str,
    plugin_key: &str,
    daemon_cert: &str,
    require_daemon_cert: bool,
) -> std::net::SocketAddr {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = vec![CertificateDer::from_pem_file(plugin_cert).unwrap()];
    let key = PrivateKeyDer::from_pem_file(plugin_key).unwrap();
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap();
    let config = if require_daemon_cert {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(daemon_cert).unwrap())
            .unwrap();
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .unwrap();
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    }
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 64];
                // The client's preface, then our SETTINGS.
                if tls.read(&mut buf).await.is_err() {
                    return;
                }
                let _ = tls.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await;
                let _ = tls.flush().await;
                while matches!(tls.read(&mut buf).await, Ok(n) if n > 0) {}
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_init_produces_mutually_pinned_pair() {
    let certs = tempfile::tempdir().unwrap();
    let out = certs.path().join("plugin-certs");
    let init = janus(&[
        "plugin",
        "tls",
        "init",
        "--plugin-host",
        "127.0.0.1",
        "--out",
        out.to_str().unwrap(),
        "--days",
        "30",
        "--json",
    ])
    .run()
    .await
    .ok_json();
    let path = |k: &str| init[k].as_str().unwrap().to_owned();
    let mode = |p: &str| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path("daemon_key")), 0o600);
    assert_eq!(mode(&path("plugin_key")), 0o600);
    let block = path("config_block");
    assert!(
        block.starts_with("[plugin.tls]") && block.contains("pinned_cert"),
        "{block}"
    );
    assert!(path("plugin_instructions").contains(&path("daemon_cert")));

    // The printed block, pasted into the daemon's config, against a plugin
    // serving plugin.pem and accepting only daemon.pem: every probe passes.
    let addr = tls_plugin(
        &path("plugin_cert"),
        &path("plugin_key"),
        &path("daemon_cert"),
        true,
    )
    .await;
    let d = TestDaemon::start(&format!("[plugin]\nendpoint = \"https://{addr}\"\n{block}")).await;
    let check = d.janus(&["plugin", "check", "--json"]).await.ok_json();
    assert_eq!(check["mode"], "tls");
    assert_eq!(check["passed"], true, "{check:#}");
    let steps: Vec<&str> = check["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["step"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        ["handshake", "throwaway_cert_rejected", "no_cert_rejected"]
    );
    let human = d.janus(&["plugin", "check"]).await;
    human.ok();
    assert!(human.stdout.contains("PASSED"), "{}", human.stdout);
    let status = d.janus(&["plugin", "status", "--json"]).await.ok_json();
    assert!(status["client_cert_expires_at"].is_string());

    // A plugin that does not pin the daemon is caught: the check fails.
    let lax = tls_plugin(
        &path("plugin_cert"),
        &path("plugin_key"),
        &path("daemon_cert"),
        false,
    )
    .await;
    let d2 = TestDaemon::start(&format!("[plugin]\nendpoint = \"https://{lax}\"\n{block}")).await;
    let failed = d2.janus(&["plugin", "check", "--json"]).await;
    assert_eq!(failed.code, 1);
    let report: Value = serde_json::from_str(&failed.stdout).unwrap();
    assert_eq!(report["passed"], false);

    // Human output prints the block; a rerun rotates the pair.
    let before = std::fs::read(path("plugin_cert")).unwrap();
    let again = janus(&[
        "plugin",
        "tls",
        "init",
        "--plugin-host",
        "127.0.0.1",
        "--out",
        out.to_str().unwrap(),
    ])
    .run()
    .await;
    again.ok();
    assert!(again.stdout.contains("[plugin.tls]") && again.stdout.contains("For the plugin"));
    assert_ne!(std::fs::read(path("plugin_cert")).unwrap(), before);

    let bad = janus(&["plugin", "tls", "init", "--plugin-host", "not a host!"])
        .run()
        .await;
    assert_eq!(bad.code, 2);
    let zero = janus(&[
        "plugin",
        "tls",
        "init",
        "--plugin-host",
        "p",
        "--days",
        "0",
        "--out",
        out.to_str().unwrap(),
    ])
    .run()
    .await;
    assert_eq!(zero.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn check_config_runs_the_daemons_validation() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let good = root.path().join("good.toml");
    std::fs::write(&good, support::config_text(root.path(), &data, "")).unwrap();
    let ok = janus(&["--config", good.to_str().unwrap(), "check-config"])
        .run()
        .await;
    ok.ok();
    assert!(ok.stdout.starts_with("config OK"), "{}", ok.stdout);
    assert!(ok.stdout.contains("identity=opaque"));
    let j = janus(&["--config", good.to_str().unwrap(), "check-config", "--json"])
        .run()
        .await
        .ok_json();
    assert_eq!(j["ok"], true);
    assert_eq!(j["upstreams"][0]["binding"], "opaque");

    // A binding naming a secret that is not configured: a startup refusal.
    let bad = root.path().join("bad.toml");
    std::fs::write(
        &bad,
        format!(
            "data_dir = \"{}\"\n[data_plane]\nbind = \"127.0.0.1:0\"\n[identity]\nmethod = \"hmac\"\nsource = {{ header = \"authorization\" }}\nsecret_ref = \"missing\"\nnamespace = \"n\"\nidentity_claim = \"sub\"\n[[upstream]]\nname = \"openrouter\"\npath_prefix = \"/or\"\nbase_url = \"https://openrouter.ai/api\"\n",
            data.display()
        ),
    )
    .unwrap();
    let refused = janus(&["--config", bad.to_str().unwrap(), "check-config"])
        .run()
        .await;
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    assert!(refused.stderr.contains("bad.toml"), "{}", refused.stderr);
    let missing = janus(&["check-config"]).run().await;
    assert_eq!(missing.code, 2);
}
