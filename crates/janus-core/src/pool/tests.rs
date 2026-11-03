//! Pool behaviour against a real filesystem (tempdirs) and a fake
//! `UpstreamImpl` that decodes vendor-shaped documents and counts refreshes.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use http::{HeaderMap, HeaderName, StatusCode};
use serde_json::{json, Value};
use url::Url;

use super::*;
use crate::contract::credential::{CredentialKind, Secret};
use crate::contract::upstream::UsageObserver;

// ---------------------------------------------------------------- fakes

/// An upstream that understands three document shapes: a plain key
/// (`{"api_key"}`, or a bare JSON string, as the real upstreams accept), a Claude CLI `.credentials.json` (`claudeAiOauth`), and
/// a Codex `auth.json` (`tokens`, with an `expires_at` for the test).
struct FakeUpstream {
    kind: UpstreamKind,
    refreshes: AtomicUsize,
    refresh_delay: Duration,
    fail_refresh: AtomicBool,
    /// Refresh to a token that is already within `REFRESH_MARGIN`.
    stale_refresh: AtomicBool,
}

impl FakeUpstream {
    fn new(kind: UpstreamKind) -> Arc<FakeUpstream> {
        Self::with_delay(kind, Duration::ZERO)
    }

    fn with_delay(kind: UpstreamKind, refresh_delay: Duration) -> Arc<FakeUpstream> {
        Arc::new(FakeUpstream {
            kind,
            refreshes: AtomicUsize::new(0),
            refresh_delay,
            fail_refresh: AtomicBool::new(false),
            stale_refresh: AtomicBool::new(false),
        })
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

struct NoUsage;

impl UsageObserver for NoUsage {
    fn observe_head(&mut self, _: StatusCode, _: &HeaderMap) {}
    fn observe_chunk(&mut self, _: &[u8]) {}
    fn finish(self: Box<Self>) -> Value {
        Value::Null
    }
}

fn bad(reason: &str) -> UpstreamError {
    UpstreamError::InvalidMaterial(reason.into())
}

#[async_trait]
impl UpstreamImpl for FakeUpstream {
    fn kind(&self) -> UpstreamKind {
        self.kind
    }

    fn forwarded_headers(&self) -> &[HeaderName] {
        &[]
    }

    fn outbound_url(
        &self,
        base_url: &Url,
        rest: &str,
        _kind: crate::contract::credential::CredentialKind,
    ) -> Result<Url, UpstreamError> {
        base_url
            .join(rest)
            .map_err(|_| UpstreamError::BadPath(rest.into()))
    }

    fn apply_auth(
        &self,
        _request: &mut http::request::Parts,
        _material: &CredentialMaterial,
    ) -> Result<(), UpstreamError> {
        Ok(())
    }

    fn decode_material(&self, doc: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> {
        let v = doc.expose();
        if let Some(key) = v.get("api_key").or(v.is_string().then_some(v)) {
            let key = key
                .as_str()
                .filter(|k| !k.is_empty())
                .ok_or(bad("api_key"))?;
            return Ok(DecodedMaterial {
                kind: CredentialKind::ApiKey,
                secret: Secret::new(key),
                account_id: None,
                expires_at: None,
            });
        }
        if let Some(oauth) = v.get("claudeAiOauth") {
            let token = oauth["accessToken"].as_str().ok_or(bad("accessToken"))?;
            let ms = oauth["expiresAt"].as_i64().ok_or(bad("expiresAt"))?;
            return Ok(DecodedMaterial {
                kind: CredentialKind::Oauth,
                secret: Secret::new(token),
                account_id: None,
                expires_at: Utc.timestamp_millis_opt(ms).single(),
            });
        }
        if let Some(tokens) = v.get("tokens") {
            let token = tokens["access_token"].as_str().ok_or(bad("access_token"))?;
            let expires = v["expires_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));
            return Ok(DecodedMaterial {
                kind: CredentialKind::Oauth,
                secret: Secret::new(token),
                account_id: tokens["account_id"].as_str().map(str::to_owned),
                expires_at: expires,
            });
        }
        Err(bad("unrecognised material document"))
    }

    async fn refresh(&self, doc: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> {
        if doc.expose().get("api_key").is_some() {
            return Err(UpstreamError::NotRefreshable(CredentialKind::ApiKey));
        }
        let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(self.refresh_delay).await;
        if self.fail_refresh.load(Ordering::SeqCst) {
            return Err(UpstreamError::RefreshFailed("invalid_grant".into()));
        }
        let later = if self.stale_refresh.load(Ordering::SeqCst) {
            Utc::now() + REFRESH_MARGIN / 2
        } else {
            Utc::now() + chrono::Duration::hours(1)
        };
        let mut v = doc.expose().clone();
        if let Some(oauth) = v.get_mut("claudeAiOauth") {
            let old = oauth["accessToken"].as_str().unwrap_or_default().to_owned();
            oauth["accessToken"] = json!(format!("{old}+r{n}"));
            oauth["expiresAt"] = json!(later.timestamp_millis());
        } else {
            let old = v["tokens"]["access_token"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            v["tokens"]["access_token"] = json!(format!("{old}+r{n}"));
            v["expires_at"] = json!(later.to_rfc3339());
        }
        Ok(MaterialDocument::new(v))
    }

    fn usage_observer(&self) -> Box<dyn UsageObserver> {
        Box::new(NoUsage)
    }

    fn classify(&self, _: StatusCode, _: &HeaderMap, _: &[u8]) -> Option<UpstreamFeedback> {
        None
    }
}

// ---------------------------------------------------------------- fixture

struct Fixture {
    _tmp: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    anthropic: Arc<FakeUpstream>,
    openai: Arc<FakeUpstream>,
    openrouter: Arc<FakeUpstream>,
}

impl Fixture {
    fn new() -> Fixture {
        Self::with_anthropic(FakeUpstream::new(UpstreamKind::Anthropic))
    }

    fn with_anthropic(anthropic: Arc<FakeUpstream>) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        Fixture {
            data_dir: tmp.path().join("data"),
            _tmp: tmp,
            anthropic,
            openai: FakeUpstream::new(UpstreamKind::OpenAi),
            openrouter: FakeUpstream::new(UpstreamKind::OpenRouter),
        }
    }

    fn pool_dir(&self) -> std::path::PathBuf {
        pool_dir(&self.data_dir)
    }

    fn outside(&self) -> std::path::PathBuf {
        self._tmp.path().to_path_buf()
    }

    fn try_open(&self, configured: Vec<ConfiguredCredential>) -> Result<FilePool, OpenError> {
        FilePool::open(PoolOptions {
            pool_dir: self.pool_dir(),
            upstreams: vec![
                self.anthropic.clone(),
                self.openai.clone(),
                self.openrouter.clone(),
            ],
            configured,
        })
    }

    fn open(&self) -> FilePool {
        self.try_open(Vec::new()).expect("open pool")
    }

    /// A slot written by hand, as an operator dropping a vendor file in.
    fn hand_slot(&self, name: &str, file: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = self.pool_dir().join(name);
        fs::create_dir_all(&dir).expect("mkdir slot");
        fs::write(dir.join(file), bytes).expect("write material");
        dir
    }
}

fn key_doc(key: &str) -> MaterialDocument {
    MaterialDocument::new(json!({ "api_key": key }))
}

fn claude_json(token: &str, expires: DateTime<Utc>) -> Value {
    json!({ "claudeAiOauth": {
        "accessToken": token,
        "refreshToken": format!("rt-for-{token}"),
        "expiresAt": expires.timestamp_millis(),
        "scopes": ["user:inference"],
    }})
}

fn codex_json(token: &str, expires: DateTime<Utc>) -> Value {
    json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "access_token": token,
            "refresh_token": format!("rt-for-{token}"),
            "account_id": "acct-123",
        },
        "expires_at": expires.to_rfc3339(),
    })
}

fn hours(h: i64) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(h)
}

fn new_cred(upstream: UpstreamKind, r: Option<&str>, doc: MaterialDocument) -> NewCredential {
    NewCredential {
        upstream,
        credential_ref: r.map(str::to_owned),
        label: None,
        material: doc,
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .expect("ls")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .collect();
    v.sort();
    v
}

fn secret_of(m: &CredentialMaterial) -> String {
    m.secret.expose_str().expect("utf8").to_owned()
}

fn env_cred(r: &str, upstream: UpstreamKind, var: &str, value: &str) -> ConfiguredCredential {
    std::env::set_var(var, value);
    ConfiguredCredential {
        credential_ref: r.into(),
        upstream,
        label: None,
        source: ConfiguredSource::Env(var.into()),
    }
}

fn refs(list: &[CredentialMetadata]) -> Vec<&str> {
    list.iter().map(|m| m.credential_ref.as_str()).collect()
}

/// `n` default-chain Anthropic resolves released together.
async fn resolve_all_at_once(
    pool: &Arc<FilePool>,
    n: usize,
) -> Vec<Result<CredentialMaterial, PoolError>> {
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            let pool = pool.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                pool.resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
                    .await
            })
        })
        .collect();
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.expect("join"));
    }
    results
}

/// A pool whose Anthropic refreshes take 300ms, holding `a` (expired, first
/// in the chain) and `b` (healthy).
async fn slow_refresh_pool_a_expired_b_healthy() -> (Fixture, Arc<FilePool>) {
    let fx = Fixture::with_anthropic(FakeUpstream::with_delay(
        UpstreamKind::Anthropic,
        Duration::from_millis(300),
    ));
    let pool = Arc::new(fx.open());
    for (r, token, expires) in [("a", "at-a", hours(-1)), ("b", "at-b", hours(5))] {
        let doc = MaterialDocument::new(claude_json(token, expires));
        pool.add(new_cred(UpstreamKind::Anthropic, Some(r), doc))
            .await
            .expect("add");
    }
    (fx, pool)
}

/// End a credential's refresh-failure back-off, as if it had run out.
fn end_refresh_backoff(pool: &FilePool, credential_ref: &str) {
    let entry = pool.get(credential_ref).expect("entry");
    let mut st = entry.lock();
    let failure = st.refresh_failure.as_mut().expect("a refresh failed");
    failure.retry_at = Utc::now() - chrono::Duration::seconds(1);
}

// ------------------------------------------------------------ detection

#[tokio::test]
async fn detects_kind_from_credentials_json_as_anthropic_oauth() {
    let fx = Fixture::new();
    let body = serde_json::to_vec(&claude_json("at-1", hours(2))).expect("json");
    let dir = fx.hand_slot("claude-main", ".credentials.json", &body);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
    let material = dir.join(".credentials.json");
    fs::set_permissions(&material, fs::Permissions::from_mode(0o644)).expect("chmod");
    let pool = fx.open();
    assert_eq!(mode(&dir), 0o700, "hand-made slot made private");
    assert_eq!(mode(&material), 0o600);
    let meta = pool.show("claude-main").await.expect("show");
    assert_eq!(meta.upstream, UpstreamKind::Anthropic);
    assert_eq!(meta.kind, CredentialKind::Oauth);
    assert_eq!(meta.source, CredentialSource::Pool);
    assert_eq!(meta.status, CredentialStatus::Active);
    assert!(meta.expires_at.is_some_and(|t| t > Utc::now()));
    // The detection is recorded, so it survives the file being renamed.
    let state: SlotState =
        serde_json::from_slice(&fs::read(dir.join(slot::SLOT_STATE_FILE)).expect("slot.json"))
            .expect("parse");
    assert_eq!(state.upstream, UpstreamKind::Anthropic);
    assert_eq!(mode(&dir.join(slot::SLOT_STATE_FILE)), 0o600);
}

#[tokio::test]
async fn detects_kind_from_auth_json_as_openai_oauth_with_account() {
    let fx = Fixture::new();
    let body = serde_json::to_vec(&codex_json("at-codex", hours(2))).expect("json");
    fx.hand_slot("codex", "auth.json", &body);
    let pool = fx.open();
    let meta = pool.show("codex").await.expect("show");
    assert_eq!(meta.upstream, UpstreamKind::OpenAi);
    assert_eq!(meta.kind, CredentialKind::Oauth);
    let m = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Ref("codex".into()))
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-codex");
    assert_eq!(m.account_id.as_deref(), Some("acct-123"));
}

#[tokio::test]
async fn detects_kind_from_plain_api_key_as_openrouter() {
    let fx = Fixture::new();
    fx.hand_slot("or", "api_key", b"sk-or-v1-abc\n");
    let pool = fx.open();
    let meta = pool.show("or").await.expect("show");
    assert_eq!(meta.upstream, UpstreamKind::OpenRouter);
    assert_eq!(meta.kind, CredentialKind::ApiKey);
    assert_eq!(meta.expires_at, None);
    let m = pool
        .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-or-v1-abc", "trailing newline trimmed");
}

#[tokio::test]
async fn recorded_upstream_wins_over_file_name_detection() {
    let fx = Fixture::new();
    let dir = fx.hand_slot("oai-key", "api_key", b"sk-proj-1");
    let state = SlotState {
        upstream: UpstreamKind::OpenAi,
        label: Some("OpenAI key".into()),
        created_at: Utc::now(),
        disabled: false,
        invalid: None,
        rate_limited_until: None,
        last_error: None,
        material_file: None,
    };
    slot::write_state(&dir, &state).expect("slot.json");
    let pool = fx.open();
    let meta = pool.show("oai-key").await.expect("show");
    assert_eq!(meta.upstream, UpstreamKind::OpenAi);
    assert_eq!(meta.label.as_deref(), Some("OpenAI key"));
    assert!(pool
        .list(Some(UpstreamKind::OpenRouter))
        .await
        .expect("list")
        .is_empty());
}

// ---------------------------------------------------------------- refresh

#[tokio::test]
async fn expired_credential_refreshed_before_use_and_written_back() {
    let fx = Fixture::new();
    let body = serde_json::to_vec(&codex_json("at-old", hours(-1))).expect("json");
    let dir = fx.hand_slot("codex", "auth.json", &body);
    let pool = fx.open();
    let before = pool.show("codex").await.expect("show");
    assert!(before.expires_at.is_some_and(|t| t < Utc::now()));

    let m = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-old+r1");
    assert_eq!(fx.openai.refreshes(), 1);
    let after = pool.show("codex").await.expect("show");
    assert!(after
        .expires_at
        .is_some_and(|t| t > Utc::now() + REFRESH_MARGIN));

    // Written back to the slot, privately, with no scratch left behind.
    let on_disk: Value =
        serde_json::from_slice(&fs::read(dir.join("auth.json")).expect("read")).expect("json");
    assert_eq!(on_disk["tokens"]["access_token"], "at-old+r1");
    assert_eq!(
        on_disk["tokens"]["account_id"], "acct-123",
        "rest of the file kept"
    );
    assert_eq!(mode(&dir.join("auth.json")), 0o600);
    assert_eq!(names(&dir), vec!["auth.json", "slot.json"]);

    // A fresh token is not refreshed again, here or after a restart.
    pool.resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect("again");
    assert_eq!(fx.openai.refreshes(), 1);
    drop(pool);
    let reopened = fx.open();
    let m = reopened
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect("after restart");
    assert_eq!(secret_of(&m), "at-old+r1");
    assert_eq!(fx.openai.refreshes(), 1);
}

#[tokio::test]
async fn refresh_failure_is_request_failure_not_fallback() {
    let fx = Fixture::new();
    let expired = serde_json::to_vec(&claude_json("at-expired", hours(-1))).expect("json");
    let healthy = serde_json::to_vec(&claude_json("at-healthy", hours(5))).expect("json");
    fx.hand_slot("a-expired", ".credentials.json", &expired);
    let dir = fx.hand_slot("b-healthy", ".credentials.json", &healthy);
    // Same directory age would tie on ref anyway; make the order explicit.
    let mut state: SlotState = {
        let pool = fx.open();
        drop(pool);
        serde_json::from_slice(&fs::read(dir.join(slot::SLOT_STATE_FILE)).expect("read"))
            .expect("parse")
    };
    state.created_at += chrono::Duration::days(1);
    slot::write_state(&dir, &state).expect("write");

    fx.anthropic.fail_refresh.store(true, Ordering::SeqCst);
    let pool = fx.open();
    let named = pool
        .resolve(
            UpstreamKind::Anthropic,
            &CredentialChoice::Ref("a-expired".into()),
        )
        .await;
    assert!(
        matches!(&named, Err(PoolError::RefreshFailed { credential_ref, .. }) if credential_ref == "a-expired"),
        "{named:?}"
    );
    let default = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await;
    assert!(
        matches!(&default, Err(PoolError::RefreshFailed { credential_ref, .. }) if credential_ref == "a-expired"),
        "the default chain's pick failed; it must not move on: {default:?}"
    );
    let healthy = pool.show("b-healthy").await.expect("show");
    assert_eq!(healthy.request_count, 0, "no other credential was used");
    let failed = pool.show("a-expired").await.expect("show");
    assert_eq!(failed.request_count, 0);
    // The second request was answered by the first's failure (back-off).
    assert_eq!(fx.anthropic.refreshes(), 1);
    assert_eq!(failed.error_count, 1);
    assert!(failed
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("refresh failed") && e.contains("invalid_grant")));

    // Once refresh works again (and the back-off is over), the same
    // credential is used.
    fx.anthropic.fail_refresh.store(false, Ordering::SeqCst);
    end_refresh_backoff(&pool, "a-expired");
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(m.credential_ref, "a-expired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_single_flight() {
    let fx = Fixture::with_anthropic(FakeUpstream::with_delay(
        UpstreamKind::Anthropic,
        Duration::from_millis(200),
    ));
    let body = serde_json::to_vec(&claude_json("at-0", hours(-1))).expect("json");
    fx.hand_slot("shared", ".credentials.json", &body);
    let pool = Arc::new(fx.open());
    for result in resolve_all_at_once(&pool, 16).await {
        let m = result.expect("resolve");
        assert_eq!(secret_of(&m), "at-0+r1", "everyone got the one refresh");
    }
    assert_eq!(fx.anthropic.refreshes(), 1, "exactly one refresh ran");
    let meta = pool.show("shared").await.expect("show");
    assert_eq!(meta.request_count, 16);
}

#[tokio::test]
async fn forced_refresh_renews_oauth_and_refuses_api_keys_without_blame() {
    let fx = Fixture::new();
    let body = serde_json::to_vec(&claude_json("at-fresh", hours(3))).expect("json");
    fx.hand_slot("claude", ".credentials.json", &body);
    fx.hand_slot("key", "api_key", b"sk-or-1");
    let pool = fx.open();
    let before = pool.show("claude").await.expect("show").expires_at;
    let meta = pool.refresh("claude").await.expect("refresh");
    assert_eq!(fx.anthropic.refreshes(), 1);
    assert_ne!(meta.expires_at, before);
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-fresh+r1");

    let err = pool.refresh("key").await.expect_err("api key");
    assert!(matches!(err, PoolError::RefreshFailed { .. }), "{err:?}");
    let key = pool.show("key").await.expect("show");
    assert_eq!(key.error_count, 0);
    assert_eq!(key.status, CredentialStatus::Active);
}

// ---------------------------------------------------------- default chain

#[tokio::test]
async fn default_chain_order() {
    let fx = Fixture::new();
    let at = |month| {
        Utc.with_ymd_and_hms(2026, month, 1, 0, 0, 0)
            .single()
            .expect("date")
    };
    for (name, created) in [("slot-z", at(1)), ("slot-a", at(2)), ("slot-m", at(1))] {
        let dir = fx.hand_slot(name, "api_key", format!("sk-{name}").as_bytes());
        let state = SlotState {
            upstream: UpstreamKind::OpenRouter,
            label: None,
            created_at: created,
            disabled: false,
            invalid: None,
            rate_limited_until: None,
            last_error: None,
            material_file: None,
        };
        slot::write_state(&dir, &state).expect("slot.json");
    }
    let other = serde_json::to_vec(&claude_json("at-other", hours(2))).expect("json");
    fx.hand_slot("claude", ".credentials.json", &other);
    let configured = vec![
        env_cred(
            "cfg-b",
            UpstreamKind::OpenRouter,
            "JANUS_POOL_T_CHAIN_B",
            "sk-b",
        ),
        env_cred(
            "cfg-a",
            UpstreamKind::OpenRouter,
            "JANUS_POOL_T_CHAIN_A",
            "sk-a",
        ),
    ];
    let pool = fx.try_open(configured).expect("open");
    let list = pool
        .list(Some(UpstreamKind::OpenRouter))
        .await
        .expect("list");
    // Config in declaration order, then slots oldest first, ties by ref.
    assert_eq!(
        refs(&list),
        ["cfg-b", "cfg-a", "slot-m", "slot-z", "slot-a"]
    );

    let pick = || async {
        pool.peek_default(UpstreamKind::OpenRouter)
            .await
            .expect("peek")
            .credential_ref
    };
    assert_eq!(pick().await, "cfg-b");
    pool.disable("cfg-b").await.expect("disable");
    assert_eq!(pick().await, "cfg-a");
    let invalid = UpstreamFeedback::CredentialInvalid {
        reason: "401".into(),
    };
    pool.report("cfg-a", &invalid).await.expect("report");
    assert_eq!(pick().await, "slot-m");
    let limited = UpstreamFeedback::RateLimited {
        until: Some(hours(1)),
    };
    pool.report("slot-m", &limited).await.expect("report");
    assert_eq!(pick().await, "slot-z");
    let m = pool
        .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(m.credential_ref, "slot-z");
    assert_eq!(secret_of(&m), "sk-slot-z");

    // A back-off that has ended puts the credential back in its place.
    let ended = UpstreamFeedback::RateLimited {
        until: Some(hours(-1)),
    };
    pool.report("slot-m", &ended).await.expect("report");
    assert_eq!(pick().await, "slot-m");

    // Nothing for an upstream with no credentials.
    let none = pool.peek_default(UpstreamKind::OpenAi).await;
    assert!(
        matches!(none, Err(PoolError::NoDefault(UpstreamKind::OpenAi))),
        "{none:?}"
    );
    let none = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await;
    assert!(matches!(none, Err(PoolError::NoDefault(_))), "{none:?}");
}

#[tokio::test]
async fn disabled_credential_not_selected() {
    let fx = Fixture::new();
    let pool = fx.open();
    pool.add(new_cred(UpstreamKind::OpenAi, Some("a1"), key_doc("sk-a1")))
        .await
        .expect("add a1");
    pool.add(new_cred(UpstreamKind::OpenAi, Some("a2"), key_doc("sk-a2")))
        .await
        .expect("add a2");
    assert_eq!(
        pool.peek_default(UpstreamKind::OpenAi)
            .await
            .expect("peek")
            .credential_ref,
        "a1"
    );
    let meta = pool.disable("a1").await.expect("disable");
    assert_eq!(meta.status, CredentialStatus::Disabled);
    for _ in 0..3 {
        let m = pool
            .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
            .await
            .expect("resolve");
        assert_eq!(m.credential_ref, "a2");
    }
    let named = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Ref("a1".into()))
        .await;
    assert!(
        matches!(
            named,
            Err(PoolError::Unavailable {
                status: CredentialStatus::Disabled,
                ..
            })
        ),
        "{named:?}"
    );
    assert_eq!(pool.show("a1").await.expect("show").request_count, 0);
    assert_eq!(pool.show("a2").await.expect("show").request_count, 3);
    assert_eq!(
        pool.peek_default(UpstreamKind::OpenAi)
            .await
            .expect("peek")
            .credential_ref,
        "a2"
    );

    // Disabling is persisted, and enabling restores the chain.
    drop(pool);
    let pool = fx.open();
    assert_eq!(
        pool.show("a1").await.expect("show").status,
        CredentialStatus::Disabled
    );
    pool.enable("a1").await.expect("enable");
    assert_eq!(
        pool.peek_default(UpstreamKind::OpenAi)
            .await
            .expect("peek")
            .credential_ref,
        "a1"
    );
}

#[tokio::test]
async fn resolve_counts_a_use_and_peek_does_not() {
    let fx = Fixture::new();
    fx.hand_slot("k", "api_key", b"sk-or-k");
    let pool = fx.open();
    let peeked = pool
        .peek_default(UpstreamKind::OpenRouter)
        .await
        .expect("peek");
    assert_eq!(peeked.request_count, 0);
    assert_eq!(peeked.last_used_at, None);
    let t0 = Utc::now();
    pool.resolve(UpstreamKind::OpenRouter, &CredentialChoice::Default)
        .await
        .expect("resolve");
    pool.resolve(UpstreamKind::OpenRouter, &CredentialChoice::Ref("k".into()))
        .await
        .expect("resolve");
    let meta = pool.show("k").await.expect("show");
    assert_eq!(meta.request_count, 2);
    assert!(meta.last_used_at.is_some_and(|t| t >= t0));
}

#[tokio::test]
async fn named_ref_errors_never_fall_through() {
    let fx = Fixture::new();
    fx.hand_slot("or", "api_key", b"sk-or-1");
    let pool = fx.open();
    let missing = pool
        .resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("nope".into()),
        )
        .await;
    assert!(
        matches!(&missing, Err(PoolError::NotFound(r)) if r == "nope"),
        "{missing:?}"
    );
    let wrong = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Ref("or".into()))
        .await;
    assert!(
        matches!(
            wrong,
            Err(PoolError::WrongUpstream {
                expected: UpstreamKind::Anthropic,
                actual: UpstreamKind::OpenRouter,
                ..
            })
        ),
        "{wrong:?}"
    );
    assert_eq!(pool.show("or").await.expect("show").request_count, 0);
}

// ---------------------------------------------------- health and feedback

#[tokio::test]
async fn upstream_rejection_marks_invalid_until_rotation_clears_it() {
    let fx = Fixture::new();
    let pool = fx.open();
    pool.add(new_cred(
        UpstreamKind::Anthropic,
        Some("ant"),
        key_doc("sk-ant-old"),
    ))
    .await
    .expect("add");
    let slot_dir = fx.pool_dir().join("ant");
    assert_eq!(names(&slot_dir), vec!["api_key", "slot.json"]);
    let feedback = UpstreamFeedback::CredentialInvalid {
        reason: "401 invalid x-api-key".into(),
    };
    pool.report("ant", &feedback).await.expect("report");
    let meta = pool.show("ant").await.expect("show");
    assert_eq!(meta.status, CredentialStatus::Invalid);
    assert_eq!(meta.error_count, 1);
    assert!(meta
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("401")));
    let named = pool
        .resolve(
            UpstreamKind::Anthropic,
            &CredentialChoice::Ref("ant".into()),
        )
        .await;
    assert!(
        matches!(
            named,
            Err(PoolError::Unavailable {
                status: CredentialStatus::Invalid,
                ..
            })
        ),
        "{named:?}"
    );

    // Survives a restart.
    drop(pool);
    let pool = fx.open();
    assert_eq!(
        pool.show("ant").await.expect("show").status,
        CredentialStatus::Invalid
    );
    let none = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await;
    assert!(matches!(none, Err(PoolError::NoDefault(_))), "{none:?}");

    // Rotation to an OAuth credential: new file, old one gone, active again.
    let doc = MaterialDocument::new(claude_json("at-new", hours(4)));
    let meta = pool.replace_material("ant", doc).await.expect("rotate");
    assert_eq!(meta.status, CredentialStatus::Active);
    assert_eq!(meta.kind, CredentialKind::Oauth);
    assert_eq!(names(&slot_dir), vec![".credentials.json", "slot.json"]);
    assert_eq!(mode(&slot_dir.join(".credentials.json")), 0o600);
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-new");
    drop(pool);
    let pool = fx.open();
    assert_eq!(
        pool.show("ant").await.expect("show").status,
        CredentialStatus::Active
    );
}

#[tokio::test]
async fn rate_limit_backs_off_and_account_errors_are_recorded() {
    let fx = Fixture::new();
    fx.hand_slot("or", "api_key", b"sk-or-1");
    let pool = fx.open();
    let t0 = Utc::now();
    pool.report("or", &UpstreamFeedback::RateLimited { until: None })
        .await
        .expect("report");
    let meta = pool.show("or").await.expect("show");
    assert_eq!(meta.status, CredentialStatus::RateLimited);
    let until = meta.rate_limited_until.expect("until");
    assert!(
        until >= t0 + DEFAULT_RATE_LIMIT_BACKOFF
            && until <= Utc::now() + DEFAULT_RATE_LIMIT_BACKOFF
    );
    let named = pool
        .resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("or".into()),
        )
        .await;
    assert!(
        matches!(
            named,
            Err(PoolError::Unavailable {
                status: CredentialStatus::RateLimited,
                ..
            })
        ),
        "{named:?}"
    );

    let account = UpstreamFeedback::AccountError {
        reason: "credit balance too low".into(),
    };
    pool.report("or", &account).await.expect("report");
    let meta = pool.show("or").await.expect("show");
    assert_eq!(meta.error_count, 2);
    assert!(meta
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("credit balance too low")));

    let ended = UpstreamFeedback::RateLimited {
        until: Some(hours(-1)),
    };
    pool.report("or", &ended).await.expect("report");
    let meta = pool.show("or").await.expect("show");
    assert_eq!(meta.status, CredentialStatus::Active);
    assert_eq!(meta.rate_limited_until, None);

    let unknown = pool.report("ghost", &account).await;
    assert!(
        matches!(unknown, Err(PoolError::NotFound(_))),
        "{unknown:?}"
    );
}

// -------------------------------------------------------- add and remove

#[tokio::test]
async fn add_writes_a_private_slot_that_is_live_at_once() {
    let fx = Fixture::new();
    let pool = fx.open();
    assert_eq!(mode(&fx.pool_dir()), 0o700);
    let meta = pool
        .add(NewCredential {
            upstream: UpstreamKind::Anthropic,
            credential_ref: Some("team-a".into()),
            label: Some("Team A".into()),
            material: MaterialDocument::new(claude_json("at-team", hours(8))),
        })
        .await
        .expect("add");
    assert_eq!(meta.credential_ref, "team-a");
    assert_eq!(meta.kind, CredentialKind::Oauth);
    assert_eq!(meta.label.as_deref(), Some("Team A"));
    assert_eq!(meta.source, CredentialSource::Pool);
    let dir = fx.pool_dir().join("team-a");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(names(&dir), vec![".credentials.json", "slot.json"]);
    for f in names(&dir) {
        assert_eq!(mode(&dir.join(&f)), 0o600, "{f}");
    }
    assert_eq!(names(&fx.pool_dir()), vec!["team-a"], "no scratch left");
    let m = pool
        .resolve(
            UpstreamKind::Anthropic,
            &CredentialChoice::Ref("team-a".into()),
        )
        .await
        .expect("live without reopening");
    assert_eq!(secret_of(&m), "at-team");
    drop(pool);
    let meta = fx.open().show("team-a").await.expect("after restart");
    assert_eq!(meta.label.as_deref(), Some("Team A"));
}

#[tokio::test]
async fn add_generates_a_valid_ref_when_absent() {
    let fx = Fixture::new();
    let pool = fx.open();
    let a = pool
        .add(new_cred(UpstreamKind::OpenRouter, None, key_doc("sk-1")))
        .await
        .expect("add");
    let b = pool
        .add(new_cred(UpstreamKind::OpenRouter, None, key_doc("sk-2")))
        .await
        .expect("add");
    assert_ne!(a.credential_ref, b.credential_ref);
    for r in [&a.credential_ref, &b.credential_ref] {
        assert!(r.starts_with("openrouter-"), "{r}");
        assert_eq!(validate_credential_ref(r), Ok(()));
        assert!(fx.pool_dir().join(r).join("api_key").is_file());
    }
}

#[tokio::test]
async fn add_refuses_taken_refs_bad_material_and_unknown_upstreams() {
    let fx = Fixture::new();
    let configured = vec![env_cred(
        "from-env",
        UpstreamKind::OpenRouter,
        "JANUS_POOL_T_ADD_ENV",
        "sk-env",
    )];
    let pool = fx.try_open(configured).expect("open");
    pool.add(new_cred(
        UpstreamKind::OpenRouter,
        Some("k"),
        key_doc("sk-k"),
    ))
    .await
    .expect("add");
    let dup = pool
        .add(new_cred(
            UpstreamKind::OpenRouter,
            Some("k"),
            key_doc("sk-x"),
        ))
        .await;
    assert!(
        matches!(&dup, Err(PoolError::AlreadyExists(r)) if r == "k"),
        "{dup:?}"
    );
    let dup = pool
        .add(new_cred(
            UpstreamKind::OpenRouter,
            Some("from-env"),
            key_doc("sk-x"),
        ))
        .await;
    assert!(matches!(dup, Err(PoolError::AlreadyExists(_))), "{dup:?}");
    let bad = pool
        .add(new_cred(
            UpstreamKind::OpenRouter,
            Some("bad"),
            MaterialDocument::new(json!({"nonsense": true})),
        ))
        .await;
    assert!(
        matches!(
            bad,
            Err(PoolError::InvalidMaterial {
                upstream: UpstreamKind::OpenRouter,
                ..
            })
        ),
        "{bad:?}"
    );
    assert!(!fx.pool_dir().join("bad").exists());

    let lonely = Fixture::new();
    let only_openrouter = FilePool::open(PoolOptions {
        pool_dir: lonely.pool_dir(),
        upstreams: vec![lonely.openrouter.clone()],
        configured: Vec::new(),
    })
    .expect("open");
    let no_impl = only_openrouter
        .add(new_cred(UpstreamKind::OpenAi, Some("x"), key_doc("sk")))
        .await;
    assert!(
        matches!(no_impl, Err(PoolError::InvalidMaterial { .. })),
        "{no_impl:?}"
    );
    // The existing slot is untouched by the refused duplicate.
    let m = pool
        .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Ref("k".into()))
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-k");
}

#[tokio::test]
async fn delete_removes_the_slot_and_config_credentials_are_not_pool_managed() {
    let fx = Fixture::new();
    let configured = vec![env_cred(
        "from-env",
        UpstreamKind::OpenRouter,
        "JANUS_POOL_T_DELETE_ENV",
        "sk-env",
    )];
    let pool = fx.try_open(configured).expect("open");
    pool.add(new_cred(
        UpstreamKind::OpenRouter,
        Some("gone"),
        key_doc("sk-g"),
    ))
    .await
    .expect("add");
    pool.delete("gone").await.expect("delete");
    assert!(names(&fx.pool_dir()).is_empty(), "slot and scratch removed");
    assert!(matches!(
        pool.show("gone").await,
        Err(PoolError::NotFound(_))
    ));
    assert!(matches!(
        pool.delete("gone").await,
        Err(PoolError::NotFound(_))
    ));
    let r = pool
        .resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("gone".into()),
        )
        .await;
    assert!(matches!(r, Err(PoolError::NotFound(_))), "{r:?}");

    let del = pool.delete("from-env").await;
    assert!(
        matches!(
            del,
            Err(PoolError::NotPoolManaged {
                origin: CredentialSource::Env,
                ..
            })
        ),
        "{del:?}"
    );
    let rot = pool.replace_material("from-env", key_doc("sk-new")).await;
    assert!(
        matches!(rot, Err(PoolError::NotPoolManaged { .. })),
        "{rot:?}"
    );
    // Enable/disable are operator switches, so they do apply.
    assert_eq!(
        pool.disable("from-env").await.expect("disable").status,
        CredentialStatus::Disabled
    );
    drop(pool);
    assert!(fx.open().show("gone").await.is_err(), "stays deleted");
}

// ------------------------------------------------ config env/file sources

#[tokio::test]
async fn env_and_file_sources_are_reported_and_file_refresh_writes_back() {
    let fx = Fixture::new();
    let auth = fx.outside().join("codex-auth.json");
    fs::write(
        &auth,
        serde_json::to_vec(&codex_json("at-file", hours(-1))).expect("json"),
    )
    .expect("write");
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o640)).expect("chmod");
    let keyfile = fx.outside().join("or.key");
    fs::write(&keyfile, b"sk-or-file\n").expect("write");
    let configured = vec![
        env_cred(
            "env-key",
            UpstreamKind::OpenRouter,
            "JANUS_POOL_T_SRC_ENV",
            "sk-or-env",
        ),
        ConfiguredCredential {
            credential_ref: "file-oauth".into(),
            upstream: UpstreamKind::OpenAi,
            label: Some("codex".into()),
            source: ConfiguredSource::File(auth.clone()),
        },
        ConfiguredCredential {
            credential_ref: "file-key".into(),
            upstream: UpstreamKind::OpenRouter,
            label: None,
            source: ConfiguredSource::File(keyfile),
        },
    ];
    let pool = fx.try_open(configured).expect("open");
    let env = pool.show("env-key").await.expect("show");
    assert_eq!(
        (env.source, env.kind),
        (CredentialSource::Env, CredentialKind::ApiKey)
    );
    let file = pool.show("file-oauth").await.expect("show");
    assert_eq!(
        (file.source, file.kind),
        (CredentialSource::File, CredentialKind::Oauth)
    );
    let key = pool.show("file-key").await.expect("show");
    assert_eq!(
        (key.source, key.kind),
        (CredentialSource::File, CredentialKind::ApiKey)
    );

    let m = pool
        .resolve(
            UpstreamKind::OpenAi,
            &CredentialChoice::Ref("file-oauth".into()),
        )
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-file+r1");
    let on_disk: Value = serde_json::from_slice(&fs::read(&auth).expect("read")).expect("json");
    assert_eq!(on_disk["tokens"]["access_token"], "at-file+r1");
    assert_eq!(mode(&auth), 0o640, "the file's own mode is kept");
    let m = pool
        .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(
        (m.credential_ref.as_str(), secret_of(&m).as_str()),
        ("env-key", "sk-or-env")
    );
}

#[tokio::test]
async fn material_changed_on_disk_is_reloaded() {
    let fx = Fixture::new();
    let auth = fx.outside().join("auth.json");
    fs::write(
        &auth,
        serde_json::to_vec(&codex_json("at-1", hours(2))).expect("json"),
    )
    .expect("write");
    let configured = vec![ConfiguredCredential {
        credential_ref: "codex".into(),
        upstream: UpstreamKind::OpenAi,
        label: None,
        source: ConfiguredSource::File(auth.clone()),
    }];
    let slot_dir = fx.hand_slot("or", "api_key", b"sk-or-old");
    let pool = fx.try_open(configured).expect("open");
    // The vendor CLI refreshes its own file; an operator edits a slot.
    fs::write(
        &auth,
        serde_json::to_vec(&codex_json("at-2-from-cli", hours(2))).expect("json"),
    )
    .expect("rewrite");
    fs::write(slot_dir.join("api_key"), b"sk-or-new-and-longer").expect("rewrite");
    let m = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-2-from-cli");
    let m = pool
        .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-or-new-and-longer");
    assert_eq!(fx.openai.refreshes(), 0);
}

#[tokio::test]
async fn open_refuses_bad_config_credentials() {
    let fx = Fixture::new();
    let missing_env = ConfiguredCredential {
        credential_ref: "x".into(),
        upstream: UpstreamKind::OpenRouter,
        label: None,
        source: ConfiguredSource::Env("JANUS_POOL_T_DEFINITELY_UNSET".into()),
    };
    assert!(matches!(
        fx.try_open(vec![missing_env]),
        Err(OpenError::Configured { .. })
    ));
    let twice = vec![
        env_cred("same", UpstreamKind::OpenRouter, "JANUS_POOL_T_TWICE", "sk"),
        env_cred("same", UpstreamKind::OpenRouter, "JANUS_POOL_T_TWICE", "sk"),
    ];
    assert!(matches!(
        fx.try_open(twice),
        Err(OpenError::Configured { .. })
    ));
    let reserved = vec![env_cred(
        "default",
        UpstreamKind::OpenRouter,
        "JANUS_POOL_T_RESERVED",
        "sk",
    )];
    assert!(matches!(
        fx.try_open(reserved),
        Err(OpenError::Configured { .. })
    ));
    fx.hand_slot("clash", "api_key", b"sk-slot");
    let clash = vec![env_cred(
        "clash",
        UpstreamKind::OpenRouter,
        "JANUS_POOL_T_CLASH",
        "sk",
    )];
    assert!(matches!(fx.try_open(clash), Err(OpenError::DuplicateRef(r)) if r == "clash"));
}

// -------------------------------------------- filesystem safety and modes

#[tokio::test]
async fn path_traversal_refs_are_refused_everywhere() {
    let fx = Fixture::new();
    fx.hand_slot("ok", "api_key", b"sk-ok");
    let pool = fx.open();
    for bad in ["..", "../ok", "a/b", ".hidden", "", "default"] {
        assert!(
            matches!(pool.show(bad).await, Err(PoolError::InvalidRef(_))),
            "show {bad:?}"
        );
        assert!(
            matches!(pool.delete(bad).await, Err(PoolError::InvalidRef(_))),
            "delete {bad:?}"
        );
        assert!(
            matches!(pool.enable(bad).await, Err(PoolError::InvalidRef(_))),
            "enable {bad:?}"
        );
        let r = pool
            .resolve(UpstreamKind::OpenRouter, &CredentialChoice::Ref(bad.into()))
            .await;
        assert!(
            matches!(r, Err(PoolError::InvalidRef(_))),
            "resolve {bad:?}"
        );
        let r = pool
            .add(new_cred(UpstreamKind::OpenRouter, Some(bad), key_doc("sk")))
            .await;
        assert!(matches!(r, Err(PoolError::InvalidRef(_))), "add {bad:?}");
    }
    assert_eq!(
        names(&fx.outside()),
        vec!["data"],
        "nothing written outside the pool"
    );
    assert_eq!(names(&fx.pool_dir()), vec!["ok"]);
}

#[tokio::test]
async fn symlinks_in_the_pool_are_refused_not_followed() {
    let fx = Fixture::new();
    let secret_elsewhere = fx.outside().join("elsewhere.key");
    fs::write(&secret_elsewhere, b"sk-not-for-the-pool").expect("write");
    let dir = fx.pool_dir().join("linked");
    fs::create_dir_all(&dir).expect("mkdir");
    std::os::unix::fs::symlink(&secret_elsewhere, dir.join("api_key")).expect("symlink");
    let err = fx
        .try_open(Vec::new())
        .expect_err("symlinked material file");
    assert!(matches!(&err, OpenError::Slot { .. }), "{err:?}");
    fs::remove_dir_all(&dir).expect("rm");

    let real = fx.outside().join("real-slot");
    fs::create_dir_all(&real).expect("mkdir");
    fs::write(real.join("api_key"), b"sk-x").expect("write");
    std::os::unix::fs::symlink(&real, fx.pool_dir().join("dirlink")).expect("symlink");
    let err = fx
        .try_open(Vec::new())
        .expect_err("symlinked slot directory");
    assert!(matches!(&err, OpenError::Slot { .. }), "{err:?}");
}

#[tokio::test]
async fn open_refuses_unreadable_slots_and_sweeps_interrupted_writes() {
    let fx = Fixture::new();
    fx.hand_slot("good", "api_key", b"sk-good");
    // Leftovers of an interrupted add and an interrupted write.
    fs::create_dir_all(fx.pool_dir().join(".add-half-0123456789abcdef")).expect("mkdir");
    fs::write(
        fx.pool_dir()
            .join("good")
            .join(".write-api_key-0123456789abcdef"),
        b"partial",
    )
    .expect("write");
    let pool = fx.open();
    assert_eq!(names(&fx.pool_dir()), vec!["good"]);
    assert_eq!(
        names(&fx.pool_dir().join("good")),
        vec!["api_key", "slot.json"]
    );
    drop(pool);

    let cases: [(&str, &str, &[u8]); 4] = [
        ("broken-json", "auth.json", b"{not json"),
        ("empty-key", "api_key", b"\n"),
        ("unknown-doc", ".credentials.json", b"{\"what\": 1}"),
        ("bad name", "api_key", b"sk"),
    ];
    for (name, file, bytes) in cases {
        let dir = fx.hand_slot(name, file, bytes);
        let err = fx.try_open(Vec::new()).expect_err(name);
        assert!(matches!(&err, OpenError::Slot { .. }), "{name}: {err:?}");
        fs::remove_dir_all(dir).expect("rm");
    }
    let dir = fx.pool_dir().join("empty");
    fs::create_dir_all(&dir).expect("mkdir");
    assert!(matches!(
        fx.try_open(Vec::new()),
        Err(OpenError::Slot { .. })
    ));
    fs::write(dir.join("api_key"), b"sk-1").expect("write");
    fs::write(dir.join("auth.json"), b"{}").expect("write");
    assert!(
        matches!(fx.try_open(Vec::new()), Err(OpenError::Slot { reason, .. }) if reason.contains("more than one")),
    );
    fs::remove_dir_all(&dir).expect("rm");
    let generic = fx.hand_slot("generic", "material.json", b"{\"api_key\": \"sk\"}");
    let err = fx.try_open(Vec::new()).expect_err("no upstream");
    assert!(
        matches!(&err, OpenError::Slot { reason, .. } if reason.contains("identify an upstream")),
        "{err:?}"
    );
    fs::remove_dir_all(generic).expect("rm");
    fx.open();
}

#[tokio::test]
async fn pool_directory_is_created_and_kept_private() {
    let fx = Fixture::new();
    fx.open();
    assert_eq!(mode(&fx.pool_dir()), 0o700);
    fs::set_permissions(fx.pool_dir(), fs::Permissions::from_mode(0o755)).expect("chmod");
    fx.open();
    assert_eq!(mode(&fx.pool_dir()), 0o700);
}

// ----------------------------------------------- refresh failure handling

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_failure_is_single_flight_and_backs_off() {
    let fx = Fixture::with_anthropic(FakeUpstream::with_delay(
        UpstreamKind::Anthropic,
        Duration::from_millis(200),
    ));
    fx.anthropic.fail_refresh.store(true, Ordering::SeqCst);
    let body = serde_json::to_vec(&claude_json("at-0", hours(-1))).expect("json");
    fx.hand_slot("spent", ".credentials.json", &body);
    let pool = Arc::new(fx.open());
    for result in resolve_all_at_once(&pool, 16).await {
        let err = result.expect_err("refresh fails");
        assert!(
            matches!(&err, PoolError::RefreshFailed { credential_ref, reason }
                if credential_ref == "spent" && reason.contains("invalid_grant")),
            "{err:?}"
        );
    }
    assert_eq!(
        fx.anthropic.refreshes(),
        1,
        "waiters share the one failed refresh"
    );
    assert_eq!(pool.show("spent").await.expect("show").error_count, 1);

    // Within the back-off, a new request does not call the endpoint either.
    let again = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await;
    assert!(
        matches!(&again, Err(PoolError::RefreshFailed { reason, .. }) if reason.contains("not retried before")),
        "{again:?}"
    );
    assert_eq!(fx.anthropic.refreshes(), 1);

    // After it, one request tries again.
    end_refresh_backoff(&pool, "spent");
    let retried = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await;
    assert!(matches!(retried, Err(PoolError::RefreshFailed { .. })));
    assert_eq!(fx.anthropic.refreshes(), 2);

    // An operator's forced refresh is not held back, and new material ends
    // the back-off.
    pool.refresh("spent")
        .await
        .expect_err("forced, still failing");
    assert_eq!(fx.anthropic.refreshes(), 3);
    fx.anthropic.fail_refresh.store(false, Ordering::SeqCst);
    pool.replace_material(
        "spent",
        MaterialDocument::new(claude_json("at-new", hours(-1))),
    )
    .await
    .expect("rotate");
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("new material refreshes at once");
    assert_eq!(secret_of(&m), "at-new+r4");
}

#[tokio::test]
async fn refreshed_token_that_is_already_expiring_is_refused() {
    let fx = Fixture::new();
    fx.anthropic.stale_refresh.store(true, Ordering::SeqCst);
    let body = serde_json::to_vec(&claude_json("at-old", hours(-1))).expect("json");
    let dir = fx.hand_slot("claude", ".credentials.json", &body);
    let pool = fx.open();
    let err = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect_err("stale refresh");
    assert!(
        matches!(&err, PoolError::RefreshFailed { reason, .. } if reason.contains("already expiring")),
        "{err:?}"
    );
    assert_eq!(fx.anthropic.refreshes(), 1);
    // Neither memory nor disk took the unusable document.
    let meta = pool.show("claude").await.expect("show");
    assert!(meta.expires_at.is_some_and(|t| t < Utc::now()));
    let on_disk: Value =
        serde_json::from_slice(&fs::read(dir.join(".credentials.json")).expect("read"))
            .expect("json");
    assert_eq!(on_disk["claudeAiOauth"]["accessToken"], "at-old");
}

#[tokio::test]
async fn status_is_checked_again_after_waiting_on_a_refresh() {
    let (_fx, pool) = slow_refresh_pool_a_expired_b_healthy().await;
    let step = || tokio::time::sleep(Duration::from_millis(60));
    let resolve = |choice: CredentialChoice| {
        let pool = pool.clone();
        tokio::spawn(async move { pool.resolve(UpstreamKind::Anthropic, &choice).await })
    };

    // The first request refreshes `a` (holding it for 300ms); the operator
    // disables `a` meanwhile; two more requests queue behind the disable.
    let first = resolve(CredentialChoice::Ref("a".into()));
    step().await;
    let disable = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.disable("a").await })
    };
    step().await;
    let named = resolve(CredentialChoice::Ref("a".into()));
    let default = resolve(CredentialChoice::Default);
    assert_eq!(
        first
            .await
            .expect("join")
            .expect("before the disable")
            .credential_ref,
        "a"
    );
    disable.await.expect("join").expect("disable");
    let named = named.await.expect("join");
    assert!(
        matches!(
            named,
            Err(PoolError::Unavailable {
                status: CredentialStatus::Disabled,
                ..
            })
        ),
        "{named:?}"
    );
    let default = default.await.expect("join").expect("default");
    assert_eq!(default.credential_ref, "b", "the chain picked again");
    assert_eq!(pool.show("a").await.expect("show").request_count, 1);
}

#[tokio::test]
async fn default_pick_removed_while_waiting_moves_to_the_next() {
    let (_fx, pool) = slow_refresh_pool_a_expired_b_healthy().await;
    let resolve = |choice: CredentialChoice| {
        let pool = pool.clone();
        tokio::spawn(async move { pool.resolve(UpstreamKind::Anthropic, &choice).await })
    };
    let first = resolve(CredentialChoice::Default);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let delete = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.delete("a").await })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;
    let named = resolve(CredentialChoice::Ref("a".into()));
    let default = resolve(CredentialChoice::Default);
    first.await.expect("join").expect("first");
    delete.await.expect("join").expect("delete");
    let named = named.await.expect("join");
    assert!(matches!(named, Err(PoolError::NotFound(_))), "{named:?}");
    let default = default.await.expect("join").expect("default");
    assert_eq!(default.credential_ref, "b");
}

// ------------------------------------------------------ material documents

#[tokio::test]
async fn bare_string_keys_are_stored_canonically_and_survive_a_restart() {
    let fx = Fixture::new();
    let pool = fx.open();
    for (r, upstream, material) in [
        ("or", UpstreamKind::OpenRouter, json!("sk-or-v1-x")),
        ("ant", UpstreamKind::Anthropic, json!("  sk-ant-y\n")),
        // A key that cannot be a bare key file is stored as JSON.
        (
            "spaced",
            UpstreamKind::OpenRouter,
            json!({"api_key": "a b"}),
        ),
    ] {
        let meta = pool
            .add(new_cred(upstream, Some(r), MaterialDocument::new(material)))
            .await
            .expect("add");
        assert_eq!(meta.kind, CredentialKind::ApiKey);
    }
    // Rotation accepts the string form too.
    pool.add(new_cred(
        UpstreamKind::OpenAi,
        Some("oai"),
        MaterialDocument::new(codex_json("at", hours(3))),
    ))
    .await
    .expect("add oai");
    pool.replace_material("oai", MaterialDocument::new(json!("sk-proj-z")))
        .await
        .expect("rotate");
    assert_eq!(
        names(&fx.pool_dir().join("or")),
        vec!["api_key", "slot.json"]
    );
    assert_eq!(
        fs::read(fx.pool_dir().join("ant").join("api_key")).expect("read"),
        b"sk-ant-y\n"
    );
    assert_eq!(
        names(&fx.pool_dir().join("spaced")),
        vec!["material.json", "slot.json"]
    );
    assert_eq!(
        names(&fx.pool_dir().join("oai")),
        vec!["api_key", "slot.json"]
    );
    drop(pool);

    let pool = fx.try_open(Vec::new()).expect("restart");
    for (r, upstream, key) in [
        ("or", UpstreamKind::OpenRouter, "sk-or-v1-x"),
        ("ant", UpstreamKind::Anthropic, "sk-ant-y"),
        ("spaced", UpstreamKind::OpenRouter, "a b"),
        ("oai", UpstreamKind::OpenAi, "sk-proj-z"),
    ] {
        let m = pool
            .resolve(upstream, &CredentialChoice::Ref(r.into()))
            .await
            .expect("resolve");
        assert_eq!(secret_of(&m), key, "{r}");
    }
}

#[tokio::test]
async fn slots_holding_a_key_as_a_json_string_open() {
    // What an earlier pool wrote for a bare-string key.
    let fx = Fixture::new();
    let dir = fx.hand_slot("legacy", "material.json", b"\"sk-legacy\"\n");
    let state = SlotState {
        upstream: UpstreamKind::OpenRouter,
        label: None,
        created_at: Utc::now(),
        disabled: false,
        invalid: None,
        rate_limited_until: None,
        last_error: None,
        material_file: None,
    };
    slot::write_state(&dir, &state).expect("slot.json");
    fx.hand_slot("legacy-ant", ".credentials.json", b"\"sk-ant-legacy\"");
    let pool = fx.try_open(Vec::new()).expect("open");
    let m = pool
        .resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("legacy".into()),
        )
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-legacy");
    let m = pool
        .resolve(
            UpstreamKind::Anthropic,
            &CredentialChoice::Ref("legacy-ant".into()),
        )
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-ant-legacy");
}

#[tokio::test]
async fn half_written_config_file_is_an_error_not_a_key() {
    let fx = Fixture::new();
    let auth = fx.outside().join("auth.json");
    let full = serde_json::to_vec(&codex_json("at-1", hours(2))).expect("json");
    fs::write(&auth, &full).expect("write");
    let configured = vec![ConfiguredCredential {
        credential_ref: "codex".into(),
        upstream: UpstreamKind::OpenAi,
        label: None,
        source: ConfiguredSource::File(auth.clone()),
    }];
    let pool = fx.try_open(configured.clone()).expect("open");
    // The vendor CLI is caught mid-rewrite.
    fs::write(
        &auth,
        b"{\"tokens\":{\"access_token\":\"at-2\",\"refresh_token\":\"rt-SECRET\"",
    )
    .expect("truncate");
    let err = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect_err("half-written file");
    assert!(matches!(err, PoolError::InvalidMaterial { .. }), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("SECRET"));
    let meta = pool.show("codex").await.expect("show");
    assert_eq!(meta.kind, CredentialKind::Oauth, "not taken for a key");
    // The rewrite completes: the next request reads it.
    let done = serde_json::to_vec(&codex_json("at-2", hours(2))).expect("json");
    fs::write(&auth, &done).expect("finish");
    let m = pool
        .resolve(UpstreamKind::OpenAi, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-2");
    drop(pool);
    // At startup, too, a half-written file refuses rather than guesses.
    fs::write(&auth, &done[..done.len() - 1]).expect("truncate");
    assert!(matches!(
        fx.try_open(configured),
        Err(OpenError::Configured { .. })
    ));
}

#[tokio::test]
async fn interrupted_rotation_opens_with_the_recorded_material() {
    let fx = Fixture::new();
    let pool = fx.open();
    pool.add(new_cred(
        UpstreamKind::Anthropic,
        Some("rot"),
        key_doc("sk-ant-old"),
    ))
    .await
    .expect("add");
    drop(pool);
    let dir = fx.pool_dir().join("rot");
    let oauth = serde_json::to_vec(&claude_json("at-new", hours(4))).expect("json");
    let read_state = || -> SlotState {
        serde_json::from_slice(&fs::read(dir.join(slot::SLOT_STATE_FILE)).expect("read"))
            .expect("parse")
    };
    assert_eq!(read_state().material_file, Some(MaterialFile::ApiKey));

    // Crash after writing the new file, before recording it.
    fs::write(dir.join(".credentials.json"), &oauth).expect("write");
    let pool = fx.try_open(Vec::new()).expect("open");
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "sk-ant-old");
    assert_eq!(names(&dir), vec!["api_key", "slot.json"]);
    drop(pool);

    // Crash after recording it, before removing the old file.
    fs::write(dir.join(".credentials.json"), &oauth).expect("write");
    let mut state = read_state();
    state.material_file = Some(MaterialFile::ClaudeCredentials);
    slot::write_state(&dir, &state).expect("slot.json");
    let pool = fx.try_open(Vec::new()).expect("open");
    let m = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("resolve");
    assert_eq!(secret_of(&m), "at-new");
    assert_eq!(names(&dir), vec![".credentials.json", "slot.json"]);

    // A completed rotation records its file.
    pool.replace_material("rot", key_doc("sk-ant-3"))
        .await
        .expect("rotate");
    assert_eq!(read_state().material_file, Some(MaterialFile::ApiKey));
    assert_eq!(names(&dir), vec!["api_key", "slot.json"]);
}

// ----------------------------------------------------------------- leakage

/// Every log event and span field, as text.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<String>>);

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }
}

impl CapturedLogs {
    fn text(&self) -> String {
        self.0.lock().expect("lock").clone()
    }

    fn write(&self, record: impl FnOnce(&mut FieldWriter<'_>)) {
        let mut buf = self.0.lock().expect("lock");
        record(&mut FieldWriter(&mut buf));
        buf.push('\n');
    }
}

impl tracing::Subscriber for CapturedLogs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        self.write(|w| span.record(w));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        self.write(|w| values.record(w));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        self.write(|w| event.record(w));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn material_never_in_debug_output() {
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    // With exactly one scoped dispatcher registered, tracing-core decides a
    // callsite's interest from the default of whichever thread registers it
    // first, so a callsite first hit by a concurrently running test (no
    // subscriber) would be cached as uninteresting and never reach `logs`.
    // A second live dispatcher makes it consult every registered one.
    let _second = tracing::Dispatch::new(CapturedLogs::default());

    let fx = Fixture::new();
    let body = serde_json::to_vec(&claude_json("SECRET-access", hours(-1))).expect("json");
    let dir = fx.hand_slot("claude", ".credentials.json", &body);
    let configured = vec![env_cred(
        "env-key",
        UpstreamKind::OpenRouter,
        "JANUS_POOL_T_LEAK_ENV",
        "SECRET-env-key",
    )];
    let pool = fx.try_open(configured).expect("open");
    let mut seen = Vec::new();

    let added = pool
        .add(new_cred(
            UpstreamKind::OpenRouter,
            Some("added"),
            key_doc("SECRET-added"),
        ))
        .await
        .expect("add");
    seen.push(format!("{added:?}"));
    seen.push(serde_json::to_string(&added).expect("json"));
    let refused = pool
        .add(new_cred(
            UpstreamKind::OpenRouter,
            Some("refused"),
            MaterialDocument::new(json!({"bogus": "SECRET-refused"})),
        ))
        .await
        .expect_err("bad material");
    seen.push(format!("{refused} {refused:?}"));
    fx.anthropic.fail_refresh.store(true, Ordering::SeqCst);
    let failed = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect_err("refresh fails");
    seen.push(format!("{failed} {failed:?}"));
    fx.anthropic.fail_refresh.store(false, Ordering::SeqCst);
    end_refresh_backoff(&pool, "claude");
    let material = pool
        .resolve(UpstreamKind::Anthropic, &CredentialChoice::Default)
        .await
        .expect("refreshed");
    assert!(
        secret_of(&material).starts_with("SECRET-access"),
        "real material is handed out"
    );
    seen.push(format!("{material:?}"));
    for m in [
        pool.resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("env-key".into()),
        )
        .await
        .expect("env"),
        pool.resolve(
            UpstreamKind::OpenRouter,
            &CredentialChoice::Ref("added".into()),
        )
        .await
        .expect("added"),
    ] {
        seen.push(format!("{m:?}"));
    }
    pool.report(
        "added",
        &UpstreamFeedback::CredentialInvalid {
            reason: "401".into(),
        },
    )
    .await
    .expect("report");
    pool.replace_material("added", key_doc("SECRET-rotated"))
        .await
        .expect("rotate");
    let list = pool.list(None).await.expect("list");
    seen.push(format!("{list:?}"));
    seen.push(serde_json::to_string(&list).expect("json"));
    seen.push(format!("{pool:?}"));
    seen.push(format!("{pool:#?}"));
    seen.push(fs::read_to_string(dir.join(slot::SLOT_STATE_FILE)).expect("slot.json"));
    seen.push(
        fs::read_to_string(fx.pool_dir().join("added").join(slot::SLOT_STATE_FILE))
            .expect("slot.json"),
    );
    // An open error over a slot with material it cannot use.
    fx.hand_slot("unusable", "auth.json", b"{\"SECRET-in-file\": 1}");
    let open_err = fx.try_open(Vec::new()).expect_err("unusable slot");
    seen.push(format!("{open_err} {open_err:?}"));
    let log_text = logs.text();
    seen.push(log_text.clone());

    // The outputs are about the credentials, so they are not trivially empty.
    let all = seen.join("\n");
    for r in ["claude", "env-key", "added"] {
        assert!(format!("{pool:?}").contains(r), "{r} in Debug");
    }
    assert!(
        log_text.contains("credential added") && log_text.contains("refresh failed"),
        "{log_text}"
    );
    assert!(!all.contains("SECRET"), "material leaked:\n{all}");
}
