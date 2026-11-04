//! Refresh-before-use end to end: an expired OAuth credential, a
//! configured `token_url` pointing at a local token endpoint, and a request
//! through the data plane.

use http::StatusCode;
use janus_daemon::Embedded;
use serde_json::Value;

use super::harness::*;
use super::upstream::{Script, Upstream};

const OLD_ACCESS: &str = "OLD-ACCESS-TOKEN-0001";
const OLD_REFRESH: &str = "OLD-REFRESH-TOKEN-0002";
const NEW_ACCESS: &str = "NEW-ACCESS-TOKEN-0003";
const NEW_REFRESH: &str = "NEW-REFRESH-TOKEN-0004";

const MESSAGE: &str = "{\"id\":\"msg_1\",\"type\":\"message\",\"model\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}";

fn json_ok(body: &'static str) -> Script {
    Script::Fixed {
        status: StatusCode::OK,
        headers: vec![("content-type", "application/json")],
        body,
    }
}

fn expired_credentials(env: &Env, expires_at_ms: i64) {
    let doc = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": OLD_ACCESS,
            "refreshToken": OLD_REFRESH,
            "expiresAt": expires_at_ms,
            "scopes": ["user:inference"],
        }
    });
    std::fs::write(env.path("ant-main.key"), doc.to_string()).expect("credentials");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

/// `GET path` on the admin unix socket, as JSON.
async fn admin_get(env: &Env, path: &str) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(env.path("data").join("admin.sock"))
        .await
        .expect("admin socket");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: janus\r\nconnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).expect("body");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("admin JSON ({e}): {body}"))
}

async fn expires_at_ms(env: &Env) -> i64 {
    let list = admin_get(env, "/v1/credentials").await;
    let entry = list["credentials"]
        .as_array()
        .expect("credentials")
        .iter()
        .find(|c| c["ref"] == "ant-main")
        .unwrap_or_else(|| panic!("ant-main listed: {list}"));
    let at = entry["expires_at"].as_str().expect("expires_at");
    chrono::DateTime::parse_from_rfc3339(at)
        .expect("rfc3339")
        .timestamp_millis()
}

async fn call(rt: &Running) -> reqwest::Response {
    client()
        .post(rt.url("/anthropic/v1/messages"))
        .header("x-api-key", token("alice"))
        .header("anthropic-version", "2023-06-01")
        .body("{}")
        .send()
        .await
        .expect("response")
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_oauth_credential_is_refreshed_at_the_configured_token_url() {
    let env = Env::new();
    let stale = now_ms() - 3_600_000;
    expired_credentials(&env, stale);
    let token_endpoint = Upstream::start(json_ok(
        "{\"access_token\":\"NEW-ACCESS-TOKEN-0003\",\"refresh_token\":\"NEW-REFRESH-TOKEN-0004\",\"expires_in\":3600}",
    ))
    .await;
    let api = Upstream::start(json_ok(MESSAGE)).await;
    let base = api.base_url();
    let extra = format!("token_url = \"{}/oauth/token\"", token_endpoint.base_url());
    let mut o = Opts::new("https://openrouter.ai/api");
    o.anthropic = Some(&base);
    o.anthropic_extra = &extra;
    let mut rt = env.start(&o, Embedded::default()).await;
    assert_eq!(expires_at_ms(&env).await, stale);

    let (status, body) = caller_view(call(&rt).await).await;
    assert_eq!(status, 200, "{body}");
    rt.record().await;

    let grants = token_endpoint.seen();
    assert_eq!(grants.len(), 1, "one refresh");
    assert_eq!(grants[0].target, "/oauth/token");
    let grant: Value = serde_json::from_slice(&grants[0].body).expect("grant JSON");
    assert_eq!(grant["grant_type"], "refresh_token");
    assert_eq!(grant["refresh_token"], OLD_REFRESH);

    let seen = api.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].headers["authorization"],
        format!("Bearer {NEW_ACCESS}")
    );

    let renewed = expires_at_ms(&env).await;
    assert!(renewed > now_ms() + 3_000_000, "expires_at moved forward");

    for token in [OLD_ACCESS, OLD_REFRESH, NEW_ACCESS, NEW_REFRESH] {
        assert!(!body.contains(token), "caller saw {token}");
    }
    let credentials = admin_get(&env, "/v1/credentials").await.to_string();
    for token in [OLD_ACCESS, OLD_REFRESH, NEW_ACCESS, NEW_REFRESH] {
        assert!(!credentials.contains(token), "admin listing shows {token}");
    }
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_refresh_fails_the_request_without_forwarding_or_fall_through() {
    let env = Env::new();
    expired_credentials(&env, now_ms() - 3_600_000);
    let token_endpoint = Upstream::start(Script::Fixed {
        status: StatusCode::BAD_REQUEST,
        headers: vec![("content-type", "application/json")],
        body: "{\"error\":\"invalid_grant\"}",
    })
    .await;
    let api = Upstream::start(json_ok(MESSAGE)).await;
    let base = api.base_url();
    let extra = format!("token_url = \"{}/oauth/token\"", token_endpoint.base_url());
    let mut o = Opts::new("https://openrouter.ai/api");
    o.anthropic = Some(&base);
    o.anthropic_extra = &extra;
    let mut rt = env.start(&o, Embedded::default()).await;

    let (status, body) = caller_view(call(&rt).await).await;
    assert_eq!(status, 503, "{body}");
    rt.record().await;
    assert_eq!(token_endpoint.seen().len(), 1, "the refresh was attempted");
    assert_eq!(api.connections(), 0, "nothing was forwarded");
    for token in [OLD_ACCESS, OLD_REFRESH, NEW_ACCESS, "invalid_grant"] {
        assert!(!body.contains(token), "caller saw {token}: {body}");
    }
    rt.stop().await;
}
