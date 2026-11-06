//! Upstream instances and credential selection end to end
//! (`docs/credential-substitution#credential-selection`, `#failover`):
//! several `[[upstream]]` instances of one implementation, each with its own
//! filter and strategy, in front of one fake upstream that answers by the
//! key it receives.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use http::{HeaderMap, StatusCode};
use janus_core::contract::request_log::{FailedAttempt, Outcome, Verdict};
use janus_core::contract::upstream::UpstreamKind;
use janus_daemon::{Daemon, DaemonConfig, Embedded, RunError};

use super::harness::*;
use super::plugin::{Linked, Recorder, Reply};
use super::upstream::{Script, Upstream};

/// Three openrouter keys: `or-a` untagged, `or-b` and `or-c` tagged `team`.
const KEYS: [(&str, &str); 3] = [
    ("or-a", "sk-or-SEL-a-0001"),
    ("or-b", "sk-or-SEL-b-0002"),
    ("or-c", "sk-or-SEL-c-0003"),
];

fn key_ref(headers: &HeaderMap) -> &'static str {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    KEYS.iter()
        .find(|(_, key)| auth == format!("Bearer {key}"))
        .map_or("?", |(r, _)| r)
}

/// What the fake upstream answers per credential ref; unlisted refs get a
/// 200 naming the ref.
#[derive(Clone, Default)]
struct Rejections(Arc<Mutex<BTreeMap<&'static str, u16>>>);

impl Rejections {
    fn set(&self, credential_ref: &'static str, status: u16) {
        self.0.lock().unwrap().insert(credential_ref, status);
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn script(&self) -> Script {
        let table = Arc::clone(&self.0);
        Script::ByHeaders(Arc::new(move |headers| {
            let r = key_ref(headers);
            match table.lock().unwrap().get(r).copied() {
                Some(429) => (
                    StatusCode::TOO_MANY_REQUESTS,
                    vec![
                        ("content-type", "application/json".into()),
                        ("retry-after", "600".into()),
                    ],
                    r#"{"error":{"message":"Rate limit exceeded","code":429}}"#.into(),
                ),
                Some(401) => (
                    StatusCode::UNAUTHORIZED,
                    vec![("content-type", "application/json".into())],
                    r#"{"error":{"message":"User not found.","code":401}}"#.into(),
                ),
                Some(other) => (StatusCode::from_u16(other).unwrap(), vec![], "{}".into()),
                None => (
                    StatusCode::OK,
                    vec![("content-type", "application/json".into())],
                    format!(r#"{{"served_by":"{r}"}}"#),
                ),
            }
        }))
    }
}

struct Setup {
    env: Env,
    up: Upstream,
    rejections: Rejections,
}

impl Setup {
    async fn new() -> Setup {
        let env = Env::new();
        for (r, key) in KEYS {
            std::fs::write(env.path(&format!("{r}.key")), key).expect("key");
        }
        let rejections = Rejections::default();
        let up = Upstream::start(rejections.script()).await;
        Setup {
            env,
            up,
            rejections,
        }
    }

    /// A config with `instances` as its `[[upstream]]` tables (each gets the
    /// fake upstream's `base_url` and the hmac binding) and the three keys.
    fn toml(&self, plugin: bool, instances: &str) -> String {
        let p = |name: &str| self.env.path(name).display().to_string();
        let mut t = format!(
            "data_dir = \"{}\"\n\n[data_plane]\nbind = \"127.0.0.1:0\"\n\n",
            p("data")
        );
        if plugin {
            t += "[plugin]\nlinked = true\n\n";
        }
        t += &format!(
            "[secrets.caller-identity]\nfile = \"{}\"\n\n[identity]\nmethod = \"hmac\"\nsource = {{ header = \"authorization\", scheme = \"Bearer \" }}\nsecret_ref = \"caller-identity\"\nnamespace = \"{NAMESPACE}\"\nidentity_claim = \"subject\"\n\n",
            p("caller.secret")
        );
        t += &instances.replace("BASE", &self.up.base_url());
        for (r, _) in KEYS {
            t += &format!(
                "\n[[credential]]\nref = \"{r}\"\nupstream = \"openrouter\"\nfile = \"{}\"\n",
                p(&format!("{r}.key"))
            );
            if r != "or-a" {
                t += "tags = [\"team\"]\n";
            }
        }
        t
    }

    async fn start(&self, instances: &str) -> Running {
        self.start_with(instances, Embedded::default()).await
    }

    async fn start_with(&self, instances: &str, embedded: Embedded) -> Running {
        let plugin = embedded.plugin.is_some();
        let config =
            DaemonConfig::from_toml_str(&self.toml(plugin, instances)).expect("config is valid");
        let daemon = Daemon::start(config, embedded)
            .await
            .expect("daemon starts");
        Running::new(daemon)
    }

    /// The refs the upstream saw, in order.
    fn served(&self) -> Vec<&'static str> {
        self.up.seen().iter().map(|s| key_ref(&s.headers)).collect()
    }
}

/// `alice`'s chat completion on `prefix`; status and body.
async fn chat(rt: &Running, prefix: &str, caller: &str) -> (u16, String) {
    let response = client()
        .post(rt.url(&format!("{prefix}/v1/chat/completions")))
        .bearer_auth(token(caller))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("request reaches the data plane");
    caller_view(response).await
}

const TWO_INSTANCES: &str = r#"
[[upstream]]
name        = "openrouter"
path_prefix = "/openrouter"
base_url    = "BASE"
strategy    = "round_robin"
  [upstream.credentials]
  refs = ["or-a", "or-b"]

[[upstream]]
name           = "team"
implementation = "openrouter"
path_prefix    = "/team"
base_url       = "BASE"
failover       = true
  [upstream.credentials]
  tags = ["team"]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn each_instance_uses_only_its_admitted_credentials_and_round_robin_spreads() {
    let s = Setup::new().await;
    let mut rt = s.start(TWO_INSTANCES).await;

    let mut picked = Vec::new();
    for _ in 0..4 {
        let (status, body) = chat(&rt, "/openrouter", "alice").await;
        assert_eq!(status, 200, "{body}");
        let record = rt.record().await;
        assert_eq!(record.upstream.as_deref(), Some("openrouter"));
        assert_eq!(record.implementation, Some(UpstreamKind::OpenRouter));
        picked.push(record.credential_ref.clone().expect("a credential"));
    }
    assert_eq!(picked, ["or-a", "or-b", "or-a", "or-b"]);

    for _ in 0..3 {
        let (status, body) = chat(&rt, "/team", "alice").await;
        assert_eq!(status, 200, "{body}");
        let record = rt.record().await;
        assert_eq!(record.upstream.as_deref(), Some("team"));
        assert_eq!(record.implementation, Some(UpstreamKind::OpenRouter));
        assert_eq!(record.credential_ref.as_deref(), Some("or-b"), "ordered");
        assert!(record.attempts.is_empty());
    }
    assert_eq!(
        s.served(),
        ["or-a", "or-b", "or-a", "or-b", "or-b", "or-b", "or-b"]
    );
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_on_429_and_401_moves_to_the_next_credential_and_benches_the_first() {
    let s = Setup::new().await;
    let mut rt = s.start(TWO_INSTANCES).await;

    // 429 on or-b: the request is answered by or-c, with the same body.
    s.rejections.set("or-b", 429);
    let (status, body) = chat(&rt, "/team", "alice").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("or-c"), "{body}");
    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::Forwarded);
    assert_eq!(record.status, Some(200));
    assert_eq!(record.credential_ref.as_deref(), Some("or-c"));
    assert_eq!(
        record.attempts,
        [FailedAttempt {
            credential_ref: "or-b".into(),
            status: 429,
            verdict: Verdict::RateLimited,
        }]
    );
    let seen = s.up.seen();
    assert_eq!(s.served(), ["or-b", "or-c"]);
    assert_eq!(seen[0].body, seen[1].body, "the body is sent again whole");
    assert!(!seen[0].body.is_empty());

    // or-b is rate limited now: the next request goes straight to or-c.
    s.rejections.clear();
    chat(&rt, "/team", "alice").await;
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-c"));
    assert!(record.attempts.is_empty());
    assert_eq!(s.served().len(), 3);
    rt.stop().await;

    // 401 on or-b: invalid, failed over, and out of the chain afterwards.
    let s = Setup::new().await;
    let mut rt = s.start(TWO_INSTANCES).await;
    s.rejections.set("or-b", 401);
    let (status, _) = chat(&rt, "/team", "alice").await;
    assert_eq!(status, 200);
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-c"));
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].credential_ref, "or-b");
    assert_eq!(record.attempts[0].status, 401);
    assert_eq!(record.attempts[0].verdict, Verdict::CredentialInvalid);
    s.rejections.clear();
    chat(&rt, "/team", "alice").await;
    assert_eq!(rt.record().await.credential_ref.as_deref(), Some("or-c"));
    // The other instance never shares or-c, and still has or-a.
    chat(&rt, "/openrouter", "alice").await;
    assert_eq!(rt.record().await.credential_ref.as_deref(), Some("or-a"));
    assert_eq!(s.served(), ["or-b", "or-c", "or-c", "or-a"]);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn when_every_admitted_credential_is_rejected_the_last_answer_is_relayed() {
    let s = Setup::new().await;
    let mut rt = s.start(TWO_INSTANCES).await;
    s.rejections.set("or-b", 429);
    s.rejections.set("or-c", 429);
    let (status, body) = chat(&rt, "/team", "alice").await;
    assert_eq!(status, 429);
    assert!(body.contains("Rate limit exceeded"), "{body}");
    let record = rt.record().await;
    assert_eq!(record.status, Some(429));
    assert_eq!(record.credential_ref.as_deref(), Some("or-c"));
    assert_eq!(record.attempts.len(), 1, "or-b failed over; or-c answered");
    assert_eq!(s.served(), ["or-b", "or-c"], "or-a is not the team's");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_failover_without_failover_true_or_with_a_body_over_the_replay_limit() {
    let s = Setup::new().await;
    let mut rt = s.start(TWO_INSTANCES).await;
    // `/openrouter` has no `failover`: the 429 is the caller's.
    s.rejections.set("or-a", 429);
    let (status, _) = chat(&rt, "/openrouter", "alice").await;
    assert_eq!(status, 429);
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-a"));
    assert!(record.attempts.is_empty());

    // `/team` fails over, but not a body it cannot send again.
    s.rejections.set("or-b", 429);
    let big = format!(r#"{{"model":"m","pad":"{}"}}"#, "x".repeat(1024 * 1024 + 1));
    let response = client()
        .post(rt.url("/team/v1/chat/completions"))
        .bearer_auth(token("alice"))
        .body(big)
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 429);
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-b"));
    assert!(record.attempts.is_empty());
    assert_eq!(s.served(), ["or-a", "or-b"]);
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_choice_is_never_failed_over_and_must_be_admitted() {
    let s = Setup::new().await;
    let recorder = Recorder::new(Reply::Credential("or-c".into()));
    let embedded = Embedded {
        plugin: Some(Arc::new(Linked(Arc::clone(&recorder)))),
        extractors: vec![],
    };
    let mut rt = s.start_with(TWO_INSTANCES, embedded).await;

    // The plugin names or-c; it is rejected; the request does not move on.
    s.rejections.set("or-c", 429);
    let (status, _) = chat(&rt, "/team", "alice").await;
    assert_eq!(status, 429);
    let record = rt.record().await;
    assert_eq!(record.credential_ref.as_deref(), Some("or-c"));
    assert!(record.attempts.is_empty());
    let resolved = recorder.resolved();
    assert_eq!(resolved[0].upstream, "team");
    assert_eq!(resolved[0].implementation, "openrouter");
    let completed = recorder
        .wait_for("call_completed", &record.request_id)
        .await;
    assert_eq!(completed.payload["upstream"], "team");
    assert_eq!(completed.payload["implementation"], "openrouter");

    // A ref outside the instance's filter is refused, never substituted.
    recorder.set_reply(Reply::Credential("or-a".into()));
    let (status, body) = chat(&rt, "/team", "alice").await;
    assert_eq!(status, 503);
    assert!(!body.contains("or-a"));
    let record = rt.record().await;
    assert_eq!(record.outcome, Outcome::CredentialUnavailable);
    assert_eq!(s.served(), ["or-c"], "or-a never reached the upstream");
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn least_recently_used_and_sticky_pick_as_documented() {
    let s = Setup::new().await;
    let mut rt = s
        .start(
            r#"
[[upstream]]
name        = "openrouter"
path_prefix = "/lru"
base_url    = "BASE"
strategy    = "least_recently_used"

[[upstream]]
name           = "sticky"
implementation = "openrouter"
path_prefix    = "/sticky"
base_url       = "BASE"
strategy       = "sticky"
"#,
        )
        .await;

    let mut lru = Vec::new();
    for _ in 0..4 {
        chat(&rt, "/lru", "alice").await;
        lru.push(rt.record().await.credential_ref.clone().unwrap());
    }
    assert_eq!(lru, ["or-a", "or-b", "or-c", "or-a"]);

    // One identity always lands on one credential; identities spread.
    let mut by_caller = BTreeMap::new();
    for caller in (0..12).map(|i| format!("caller-{i}")) {
        for _ in 0..2 {
            chat(&rt, "/sticky", &caller).await;
            let picked = rt.record().await.credential_ref.clone().unwrap();
            let first = by_caller.entry(caller.clone()).or_insert(picked.clone());
            assert_eq!(*first, picked, "{caller} moved");
        }
    }
    let distinct: std::collections::BTreeSet<_> = by_caller.values().collect();
    assert!(
        distinct.len() >= 2,
        "12 identities on one key: {by_caller:?}"
    );

    // A credential rate-limited out of the chain moves only its holders.
    let victim = by_caller["caller-0"].clone();
    let victim_ref: &'static str = KEYS.iter().find(|(r, _)| *r == victim).unwrap().0;
    s.rejections.set(victim_ref, 429);
    chat(&rt, "/sticky", "caller-0").await; // benches the victim (no failover)
    rt.record().await;
    s.rejections.clear();
    for (caller, before) in &by_caller {
        chat(&rt, "/sticky", caller).await;
        let now = rt.record().await.credential_ref.clone().unwrap();
        if *before == victim {
            assert_ne!(now, victim, "{caller} still on the benched key");
        } else {
            assert_eq!(&now, before, "{caller} moved though its key is fine");
        }
    }
    rt.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_a_filter_ref_naming_another_implementations_pool_slot() {
    use janus_core::contract::credential::{CredentialPool, NewCredential};
    use janus_core::contract::upstream::UpstreamImpl;
    use janus_core::pool::{pool_dir, FilePool, PoolOptions};
    use janus_core::proxy::{AnthropicUpstream, OpenAiUpstream, OpenRouterUpstream};

    let s = Setup::new().await;
    // An anthropic pool slot, added at runtime on an earlier run.
    let data = s.env.path("data");
    std::fs::create_dir_all(&data).unwrap();
    let upstreams: Vec<Arc<dyn UpstreamImpl>> = vec![
        Arc::new(AnthropicUpstream::new().unwrap()),
        Arc::new(OpenAiUpstream::new().unwrap()),
        Arc::new(OpenRouterUpstream),
    ];
    let pool = FilePool::open(PoolOptions {
        pool_dir: pool_dir(&data),
        upstreams,
        configured: Vec::new(),
    })
    .unwrap();
    let new: NewCredential = serde_json::from_value(serde_json::json!({
        "upstream": "anthropic",
        "ref": "ant-slot",
        "material": { "api_key": "sk-ant-SEL-slot-0009" },
    }))
    .unwrap();
    pool.add(new).await.unwrap();
    drop(pool);

    let filtered = |refs: &str| {
        DaemonConfig::from_toml_str(&s.toml(
            false,
            &format!(
                "[[upstream]]\nname = \"openrouter\"\npath_prefix = \"/openrouter\"\nbase_url = \"BASE\"\n[upstream.credentials]\nrefs = {refs}\n"
            ),
        ))
        .expect("parses and validates without the pool")
    };
    let refused = Daemon::start(filtered(r#"["or-a", "ant-slot"]"#), Embedded::default())
        .await
        .expect_err("refused at startup");
    let RunError::Config(e) = &refused else {
        panic!("not a config refusal: {refused}");
    };
    assert_eq!(e.at(), Some("upstream[openrouter].credentials.refs"));
    assert!(e.to_string().contains("for anthropic"), "{e}");

    // A ref naming no credential at all is only a warning.
    let daemon = Daemon::start(filtered(r#"["or-a", "not-yet"]"#), Embedded::default())
        .await
        .expect("an unknown ref is a warning, not a refusal");
    daemon.shutdown().await;
}
