//! Fixtures for the CLI end-to-end tests: the real daemon (`Daemon::start`,
//! the lifecycle `janus-daemon` itself runs) over a real temporary data
//! directory and config file, driven by the real `janus` binary as a child
//! process with a hermetic environment.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use janus_daemon::{Daemon, DaemonConfig, Embedded};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub const WAIT: Duration = Duration::from_secs(15);

/// The data plane's identity binding and one upstream that nothing serves:
/// requests produce request-log records (rejections, unavailable
/// credentials, upstream errors) without any fake upstream.
const BASE: &str = r#"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }

[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:{dead_port}/api"
"#;

/// A port nothing listens on.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

pub struct TestDaemon {
    pub root: TempDir,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    daemon: Option<Daemon>,
}

impl TestDaemon {
    /// Start the daemon with the base config plus `extra` (`{dir}` is the
    /// temporary root).
    pub async fn start(extra: &str) -> TestDaemon {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let config_path = root.path().join("janus.toml");
        let toml = config_text(root.path(), &data_dir, extra);
        std::fs::write(&config_path, toml).expect("write config");
        let mut d = TestDaemon {
            root,
            data_dir,
            config_path,
            daemon: None,
        };
        d.boot().await;
        d
    }

    /// Start the daemon (again) over this data directory and config.
    pub async fn boot(&mut self) {
        let config = DaemonConfig::load(&self.config_path).expect("config is valid");
        let daemon = Daemon::start(config, Embedded::default())
            .await
            .expect("daemon starts");
        self.daemon = Some(daemon);
    }

    /// Stop the daemon and start it again over the same data directory.
    pub async fn restart(&mut self) {
        self.stop().await;
        self.boot().await;
    }

    pub async fn stop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
    }

    fn daemon(&self) -> &Daemon {
        self.daemon.as_ref().expect("daemon running")
    }

    /// `unix://<data_dir>/admin.sock`.
    pub fn socket(&self) -> String {
        format!("unix://{}", self.data_dir.join("admin.sock").display())
    }

    pub fn data_plane(&self) -> SocketAddr {
        self.daemon().data_plane_addr()
    }

    /// Wait until every request-log record submitted so far is committed.
    pub async fn flush(&self) {
        self.daemon().requests().flush().await;
    }

    /// `janus --admin <socket> args...`, not yet run.
    pub fn cli(&self, args: &[&str]) -> Janus {
        let socket = self.socket();
        let mut all = vec!["--admin", socket.as_str()];
        all.extend_from_slice(args);
        janus(&all)
    }

    /// `janus --admin <socket> args...`, run to completion.
    pub async fn janus(&self, args: &[&str]) -> Run {
        self.cli(args).run().await
    }

    /// One data-plane request to the openrouter upstream, with an opaque
    /// token when given; returns the HTTP status.
    pub async fn call(&self, token: Option<&str>) -> u16 {
        let mut req = reqwest::Client::new()
            .post(format!(
                "http://{}/openrouter/v1/chat/completions",
                self.data_plane()
            ))
            .body("{}")
            .timeout(WAIT);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send()
            .await
            .expect("data plane answers")
            .status()
            .as_u16()
    }

    /// The admin TCP listener's URL, read through `janus status`.
    pub async fn admin_url(&self) -> String {
        let s = self.janus(&["status", "--json"]).await.ok_json();
        s["listeners"]["admin_tcp"]["url"]
            .as_str()
            .expect("admin TCP listener configured")
            .to_owned()
    }

    pub fn admin_token(&self) -> String {
        std::fs::read_to_string(self.data_dir.join("admin-token"))
            .expect("admin token")
            .trim()
            .to_owned()
    }
}

pub fn config_text(root: &Path, data_dir: &Path, extra: &str) -> String {
    let base = BASE.replace("{dead_port}", &free_port().to_string());
    format!(
        "data_dir = \"{}\"\n\n[data_plane]\nbind = \"127.0.0.1:0\"\n\n{}\n{base}",
        data_dir.display(),
        extra.replace("{dir}", &root.display().to_string()),
    )
}

/// The `janus` binary under test, with an environment that names nothing
/// unless the test says so.
pub struct Janus {
    cmd: Command,
    stdin: Option<Vec<u8>>,
}

pub fn janus(args: &[&str]) -> Janus {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_janus"));
    cmd.args(args);
    for var in [
        "JANUS_ADMIN",
        "JANUS_DATA_DIR",
        "JANUS_TOKEN_FILE",
        "JANUS_ADMIN_TOKEN",
        "JANUS_ADMIN_CA_FILE",
    ] {
        cmd.env_remove(var);
    }
    // No real ~/.janus is ever consulted.
    cmd.env("HOME", "/nonexistent-janus-test-home");
    cmd.kill_on_drop(true);
    Janus { cmd, stdin: None }
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    #[track_caller]
    pub fn ok(&self) -> &Self {
        assert_eq!(
            self.code, 0,
            "janus failed\nstdout: {}\nstderr: {}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn ok_json(&self) -> Value {
        self.ok();
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", self.stdout))
    }

    /// The `--json` error envelope printed on stderr.
    #[track_caller]
    pub fn error_json(&self) -> Value {
        serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not a JSON error ({e}): {}", self.stderr))
    }
}

impl Janus {
    pub fn env(mut self, k: &str, v: impl AsRef<std::ffi::OsStr>) -> Self {
        self.cmd.env(k, v);
        self
    }

    pub fn stdin(mut self, bytes: &[u8]) -> Self {
        self.stdin = Some(bytes.to_vec());
        self
    }

    pub async fn run(mut self) -> Run {
        self.cmd
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = self.cmd.spawn().expect("spawn janus");
        if let Some(bytes) = self.stdin.take() {
            let mut stdin = child.stdin.take().expect("stdin");
            stdin.write_all(&bytes).await.expect("write stdin");
            drop(stdin);
        }
        let out = tokio::time::timeout(WAIT * 4, child.wait_with_output())
            .await
            .expect("janus finished in time")
            .expect("janus ran");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Start a long-running command (`log tail`) and read it line by line.
    pub fn spawn(mut self) -> Spawned {
        self.cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = self.cmd.spawn().expect("spawn janus");
        let stdout = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let stderr = BufReader::new(child.stderr.take().expect("stderr")).lines();
        Spawned {
            child,
            stdout,
            stderr,
        }
    }
}

pub struct Spawned {
    pub child: Child,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Lines<BufReader<ChildStderr>>,
}

impl Spawned {
    pub async fn stdout_line(&mut self) -> String {
        tokio::time::timeout(WAIT, self.stdout.next_line())
            .await
            .expect("a stdout line in time")
            .expect("read")
            .expect("stdout still open")
    }

    /// Read stderr until a line containing `needle`; returns it.
    pub async fn stderr_until(&mut self, needle: &str) -> String {
        let read = async {
            loop {
                match self.stderr.next_line().await.expect("read") {
                    Some(line) if line.contains(needle) => return line,
                    Some(_) => {}
                    None => panic!("stderr closed before {needle:?}"),
                }
            }
        };
        tokio::time::timeout(WAIT, read)
            .await
            .unwrap_or_else(|_| panic!("no stderr line containing {needle:?} in time"))
    }

    pub async fn exit(mut self) -> (i32, String) {
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("exited in time")
            .expect("wait");
        let mut rest = String::new();
        while let Ok(Some(line)) = self.stderr.next_line().await {
            rest.push_str(&line);
            rest.push('\n');
        }
        (status.code().unwrap_or(-1), rest)
    }
}

/// Every file under `dir`, recursively, with its contents.
pub fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path, bytes));
                }
            }
        }
    }
    out
}

pub fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}
