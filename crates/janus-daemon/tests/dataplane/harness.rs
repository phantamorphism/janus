//! Test environment: a data directory with the files a config names,
//! config text, hmac caller tokens, and waiting on request-log records.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use janus_core::contract::request_log::RequestRecord;
use janus_daemon::{Daemon, DaemonConfig, Embedded};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::broadcast;

pub const SECRET: &[u8] = b"test-caller-identity-secret";
pub const NAMESPACE: &str = "janus-data-plane";
/// The real upstream keys; none may ever reach a caller.
pub const OR_MAIN_KEY: &str = "sk-or-REAL-KEY-main-0001";
pub const OR_SPARE_KEY: &str = "sk-or-REAL-KEY-spare-0002";
pub const ANT_MAIN_KEY: &str = "sk-ant-REAL-KEY-main-0003";

/// Where the daemon's plugin comes from.
pub enum PluginCfg {
    None,
    /// `endpoint = "unix://<dir>/plugin.sock"`.
    Grpc,
    /// `linked = true`.
    Linked,
}

pub struct Opts<'a> {
    pub plugin: PluginCfg,
    pub timeout_ms: u64,
    pub openrouter: &'a str,
    /// TOML placed right after the openrouter `[[upstream]]` table
    /// (`[upstream.identity]`, `[[upstream.route]]`).
    pub openrouter_extra: &'a str,
    pub anthropic: Option<&'a str>,
    /// TOML keys placed in the anthropic `[[upstream]]` table, before its
    /// `[upstream.identity]` (e.g. `token_url = "..."`).
    pub anthropic_extra: &'a str,
    /// Emit the daemon-wide `[identity]` default.
    pub default_identity: bool,
    pub data_plane: &'a str,
    /// `data_plane.max_connections`; `None` leaves the default.
    pub max_connections: Option<usize>,
}

impl<'a> Opts<'a> {
    pub fn new(openrouter: &'a str) -> Opts<'a> {
        Opts {
            plugin: PluginCfg::None,
            timeout_ms: 2000,
            openrouter,
            openrouter_extra: "",
            anthropic: None,
            anthropic_extra: "",
            default_identity: true,
            data_plane: "127.0.0.1:0",
            max_connections: None,
        }
    }
}

pub struct Env {
    pub dir: TempDir,
}

impl Env {
    pub fn new() -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Env { dir };
        std::fs::write(env.path("caller.secret"), SECRET).expect("secret");
        std::fs::write(env.path("or-main.key"), OR_MAIN_KEY).expect("key");
        std::fs::write(env.path("or-spare.key"), OR_SPARE_KEY).expect("key");
        std::fs::write(env.path("ant-main.key"), ANT_MAIN_KEY).expect("key");
        env
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn socket(&self) -> PathBuf {
        self.path("plugin.sock")
    }

    pub fn toml(&self, o: &Opts<'_>) -> String {
        let p = |name: &str| self.path(name).display().to_string();
        let mut t = format!(
            "data_dir = \"{data}\"\n\n[data_plane]\nbind = \"{bind}\"\n",
            data = p("data"),
            bind = o.data_plane,
        );
        if let Some(max) = o.max_connections {
            t += &format!("max_connections = {max}\n");
        }
        t.push('\n');
        match o.plugin {
            PluginCfg::None => {}
            PluginCfg::Grpc => {
                t += &format!(
                    "[plugin]\nendpoint = \"unix://{}\"\ntimeout_ms = {}\n\n",
                    p("plugin.sock"),
                    o.timeout_ms
                )
            }
            PluginCfg::Linked => {
                t += &format!("[plugin]\nlinked = true\ntimeout_ms = {}\n\n", o.timeout_ms)
            }
        }
        t += &format!(
            "[secrets.caller-identity]\nfile = \"{}\"\n\n",
            p("caller.secret")
        );
        if o.default_identity {
            t += &format!(
                "[identity]\nmethod = \"hmac\"\nsource = {{ header = \"authorization\", scheme = \"Bearer \" }}\nsecret_ref = \"caller-identity\"\nnamespace = \"{NAMESPACE}\"\nidentity_claim = \"subject\"\ncorrelation_claim = \"session\"\n\n"
            );
        }
        t += &format!(
            "[[upstream]]\nname = \"openrouter\"\npath_prefix = \"/openrouter\"\nbase_url = \"{}\"\n{}\n\n",
            o.openrouter, o.openrouter_extra
        );
        if let Some(anthropic) = o.anthropic {
            t += &format!(
                "[[upstream]]\nname = \"anthropic\"\npath_prefix = \"/anthropic\"\nbase_url = \"{anthropic}\"\n{}\n[upstream.identity]\nsource = {{ header = \"x-api-key\" }}\n\n",
                o.anthropic_extra
            );
            t += &format!(
                "[[credential]]\nref = \"ant-main\"\nupstream = \"anthropic\"\nfile = \"{}\"\n\n",
                p("ant-main.key")
            );
        }
        t += &format!(
            "[[credential]]\nref = \"or-main\"\nupstream = \"openrouter\"\nfile = \"{}\"\n\n[[credential]]\nref = \"or-spare\"\nupstream = \"openrouter\"\nfile = \"{}\"\n",
            p("or-main.key"),
            p("or-spare.key")
        );
        t
    }

    pub fn write_config(&self, o: &Opts<'_>) -> PathBuf {
        let path = self.path("janus.toml");
        std::fs::write(&path, self.toml(o)).expect("write config");
        path
    }

    pub async fn start(&self, o: &Opts<'_>, embedded: Embedded) -> Running {
        let config = DaemonConfig::from_toml_str(&self.toml(o)).expect("config is valid");
        let daemon = Daemon::start(config, embedded)
            .await
            .expect("daemon starts");
        Running::new(daemon)
    }
}

pub struct Running {
    pub daemon: Daemon,
    /// `http://<data plane>`.
    pub base: String,
    records: broadcast::Receiver<Arc<RequestRecord>>,
}

impl Running {
    pub fn new(daemon: Daemon) -> Running {
        let records = daemon.requests().subscribe();
        Running {
            base: format!("http://{}", daemon.data_plane_addr()),
            daemon,
            records,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The next request-log record committed.
    pub async fn record(&mut self) -> Arc<RequestRecord> {
        tokio::time::timeout(Duration::from_secs(10), self.records.recv())
            .await
            .expect("a request-log record within 10s")
            .expect("request-log tail open")
    }

    pub async fn stop(self) {
        self.daemon.shutdown().await;
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// An hmac caller token as the docs define it:
/// `<claims-hex>.<hex(hmac_sha256(secret, claims-hex))>`.
pub fn token_with(
    secret: &[u8],
    ns: &str,
    subject: &str,
    exp: i64,
    session: Option<&str>,
) -> String {
    let mut claims = serde_json::json!({ "ns": ns, "exp": exp, "subject": subject, "team": "red" });
    if let Some(session) = session {
        claims["session"] = session.into();
    }
    let claims_hex = hex::encode(claims.to_string());
    let signature = hex::encode(hmac_sha256(secret, claims_hex.as_bytes()));
    format!("{claims_hex}.{signature}")
}

/// A valid token for `subject` in the data-plane namespace.
pub fn token(subject: &str) -> String {
    token_with(SECRET, NAMESPACE, subject, now() + 3600, Some("sess-1"))
}

pub fn expired_token(subject: &str) -> String {
    token_with(SECRET, NAMESPACE, subject, now() - 3600, None)
}

/// Poll `cond` for up to 10 s.
pub async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Every byte a caller received, headers included, as text.
pub async fn caller_view(response: reqwest::Response) -> (u16, String) {
    let status = response.status().as_u16();
    let mut text = String::new();
    for (name, value) in response.headers() {
        text += &format!("{name}: {}\n", String::from_utf8_lossy(value.as_bytes()));
    }
    text += &response.text().await.expect("body");
    (status, text)
}

/// Fail if `text` carries any real key or credential ref.
pub fn assert_no_credential(text: &str) {
    for secret in [
        OR_MAIN_KEY,
        OR_SPARE_KEY,
        ANT_MAIN_KEY,
        "or-main",
        "or-spare",
        "ant-main",
    ] {
        assert!(!text.contains(secret), "caller saw {secret:?} in:\n{text}");
    }
}
