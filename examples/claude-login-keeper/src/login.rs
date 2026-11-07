//! One `claude auth login --claudeai` run, isolated from the operator's own
//! Claude Code, as a state machine the keeper can watch, feed a pasted code
//! into, cancel and time out.
//!
//! Isolation (`docs/claude-login-keeper#isolation`): the child gets an empty
//! environment plus `HOME=<state>/home`, `CLAUDE_CONFIG_DIR=<state>/claude`
//! and a `PATH` whose first entry is `<state>/bin`, which holds
//!
//! - `open`, `xdg-open` (also `$BROWSER`): record the URL the CLI asks to
//!   open in `<state>/run/open.url`; no browser is launched;
//! - `security`: exits 44 ("item not found") for every call, so the CLI
//!   never reaches a macOS keychain and stores the new login in
//!   `<state>/claude/.credentials.json`, where the keeper reads it. Each
//!   call appends its subcommand and `-s` service name (never a `-w`
//!   value) to `<state>/run/security.log`.
//!
//! All credentials share that config directory, so a finished login is moved
//! at once to `<state>/pending/<ref>.json`, the file of the credential it was
//! run for.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::output::{authorize_url, last_notice, redact_query, strip_ansi};

/// How much recent output is kept for scanning.
const SCAN_TAIL: usize = 16 * 1024;
/// How often the `open` shim's file is checked.
const OPEN_POLL: Duration = Duration::from_millis(200);
/// How long a terminated child gets before `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long output still buffered at exit is read for.
const DRAIN: Duration = Duration::from_secs(1);

/// Where a login flow is. Serialized for the loopback page and the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginState {
    /// No flow has run since the keeper started.
    Idle,
    /// The `claude` child is being spawned.
    Starting,
    /// The child runs; no authorize URL yet.
    AwaitingUrl,
    /// The URL is known; waiting for the operator to authorize.
    AwaitingBrowser {
        /// The printed URL (manual flow): the authorize page shows a code
        /// that is pasted back (`POST /login/code`). Works from any browser.
        auth_url: String,
        /// The URL the CLI asked to open (redirects to a callback on this
        /// host's `localhost`): completes by itself, but only from a browser
        /// on this host.
        local_url: Option<String>,
        /// The CLI's last complaint, such as an invalid pasted code.
        notice: Option<String>,
    },
    /// Logged in and rotated into the pool.
    Succeeded {
        at: DateTime<Utc>,
    },
    Failed {
        reason: String,
    },
    /// Ended before a verdict: by the operator, because the credential is
    /// usable again, or because the keeper stopped.
    Cancelled {
        reason: String,
    },
}

impl LoginState {
    /// A flow in this state is still running.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            LoginState::Starting | LoginState::AwaitingUrl | LoginState::AwaitingBrowser { .. }
        )
    }
}

/// A new login's OAuth material, `{"claudeAiOauth": {...}}`. Never printed.
#[derive(Clone)]
pub struct Material(Value);

impl std::fmt::Debug for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Material(<redacted>)")
    }
}

impl Material {
    /// Read the `.credentials.json` a login wrote. The error never quotes
    /// the file.
    pub fn read(path: &Path) -> Result<Material, String> {
        let text =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let doc: Value =
            serde_json::from_slice(&text).map_err(|_| format!("{} is not JSON", path.display()))?;
        let oauth = doc
            .get("claudeAiOauth")
            .filter(|o| o.is_object())
            .ok_or_else(|| format!("{} has no claudeAiOauth object", path.display()))?;
        for field in ["accessToken", "refreshToken"] {
            if oauth
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(format!("{} has no claudeAiOauth.{field}", path.display()));
            }
        }
        Ok(Material(serde_json::json!({ "claudeAiOauth": oauth })))
    }

    /// The document for `PUT /v1/credentials/{ref}/material`.
    pub fn document(&self) -> &Value {
        &self.0
    }

    /// `claudeAiOauth.expiresAt` (Unix ms), for logs.
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.0["claudeAiOauth"]["expiresAt"]
            .as_i64()
            .and_then(DateTime::from_timestamp_millis)
    }
}

/// The keeper's directory layout and the child's environment.
#[derive(Debug, Clone)]
pub struct Isolation {
    root: PathBuf,
}

impl Isolation {
    pub fn new(root: &Path) -> Isolation {
        Isolation {
            root: root.to_path_buf(),
        }
    }
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("claude")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
    fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir().join(".credentials.json")
    }
    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }
    /// A finished login for `credential_ref` that is not in the pool yet.
    pub fn pending_file(&self, credential_ref: &str) -> PathBuf {
        let mut name = String::new();
        for b in credential_ref.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => name.push(b as char),
                _ => name.push_str(&format!("%{b:02X}")),
            }
        }
        self.pending_dir().join(format!("{name}.json"))
    }
    /// Move the login that the CLI just wrote to `credential_ref`'s pending
    /// file, so no other credential's flow or attempt can take it.
    pub fn keep_pending(&self, credential_ref: &str) -> std::io::Result<()> {
        std::fs::rename(self.credentials_file(), self.pending_file(credential_ref))
    }
    fn open_file(&self) -> PathBuf {
        self.run_dir().join("open.url")
    }
    /// One line per call of the `security` shim: `<subcommand> -s <service>`.
    pub fn security_log(&self) -> PathBuf {
        self.run_dir().join("security.log")
    }
    fn pid_file(&self) -> PathBuf {
        self.run_dir().join("login.pid")
    }

    /// Create the directories, make them `0700` (also when they existed),
    /// refuse any that another user owns, and write the shims.
    pub fn prepare(&self) -> std::io::Result<()> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        for dir in [
            self.root.clone(),
            self.home(),
            self.config_dir(),
            self.bin_dir(),
            self.run_dir(),
            self.pending_dir(),
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            let meta = std::fs::symlink_metadata(&dir)?;
            if !meta.is_dir() || meta.uid() != me {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} is not a directory owned by uid {me}", dir.display()),
                ));
            }
            if meta.mode() & 0o777 != 0o700 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let open = self.open_file();
        let tmp = format!("{}.tmp", open.display());
        let opener = format!(
            "#!/bin/sh\n\
             # claude-login-keeper: records the URL claude asks to open; opens nothing.\n\
             u=\nfor a in \"$@\"; do u=$a; done\n\
             printf '%s\\n' \"$u\" > {tmp} && mv {tmp} {open}\n",
            tmp = shell_quote(&tmp),
            open = shell_quote(&open.display().to_string()),
        );
        let keychain = format!(
            "#!/bin/sh\n\
             # claude-login-keeper: no keychain for this claude; it stores its login in\n\
             # $CLAUDE_CONFIG_DIR/.credentials.json instead. 44 is \"item not found\".\n\
             # Logs the subcommand and the -s service name, never a -w value.\n\
             sub=$1\ns=\n\
             while [ $# -gt 0 ]; do\n\
             \x20 if [ \"$1\" = -s ] && [ $# -gt 1 ]; then s=$2; fi\n\
             \x20 shift\n\
             done\n\
             printf '%s -s %s\\n' \"$sub\" \"$s\" >> {log}\n\
             exit 44\n",
            log = shell_quote(&self.security_log().display().to_string()),
        );
        for (name, body) in [
            ("open", opener.as_str()),
            ("xdg-open", opener.as_str()),
            ("security", keychain.as_str()),
        ] {
            let path = self.bin_dir().join(name);
            std::fs::write(&path, body)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// `claude auth login --claudeai` with nothing of this process's
    /// environment but `USER`, `LOGNAME`, `LANG` and `TMPDIR`.
    pub fn command(&self, claude_bin: &Path) -> Command {
        let mut path = vec![self.bin_dir()];
        if let Some(dir) = claude_bin.parent() {
            path.push(dir.to_path_buf());
        }
        path.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
        let mut cmd = Command::new(claude_bin);
        cmd.args(["auth", "login", "--claudeai"])
            .env_clear()
            .env("PATH", std::env::join_paths(path).unwrap_or_default())
            .env("HOME", self.home())
            .env("CLAUDE_CONFIG_DIR", self.config_dir())
            .env("BROWSER", self.bin_dir().join("open"))
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("DISABLE_AUTOUPDATER", "1")
            .current_dir(self.home());
        for var in ["USER", "LOGNAME", "LANG", "TMPDIR"] {
            if let Some(value) = std::env::var_os(var) {
                cmd.env(var, value);
            }
        }
        cmd
    }
}

/// Single-quote `s` for `/bin/sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What a run produced.
#[derive(Debug)]
pub enum Outcome {
    Authorized(Material),
    Failed(String),
    Cancelled,
}

/// The pure part of a run: output and the `open` shim's URL in, the
/// state out.
#[derive(Debug)]
pub struct Machine {
    tail: Vec<u8>,
    seen_output: bool,
    auth_url: Option<String>,
    local_url: Option<String>,
    notice: Option<String>,
    announced_url: bool,
    announced_notice: Option<String>,
}

/// What a flow tells the log once: the URL when it is first known, a
/// complaint of the CLI when it changes.
#[derive(Debug, PartialEq, Eq)]
pub enum Announce {
    Url {
        auth_url: String,
        local_url: Option<String>,
    },
    Notice(String),
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Machine {
        Machine {
            tail: Vec::new(),
            seen_output: false,
            auth_url: None,
            local_url: None,
            notice: None,
            announced_url: false,
            announced_notice: None,
        }
    }

    /// The next thing to log, at most once each.
    pub fn announce(&mut self) -> Option<Announce> {
        if let LoginState::AwaitingBrowser {
            auth_url,
            local_url,
            ..
        } = self.state()
        {
            if !self.announced_url {
                self.announced_url = true;
                return Some(Announce::Url {
                    auth_url,
                    local_url,
                });
            }
        }
        if self.notice.is_some() && self.notice != self.announced_notice {
            self.announced_notice = self.notice.clone();
            return self.notice.clone().map(Announce::Notice);
        }
        None
    }

    /// Output from the child (stdout or stderr).
    pub fn feed(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.seen_output = true;
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > SCAN_TAIL {
            let cut = self.tail.len() - SCAN_TAIL;
            self.tail.drain(..cut);
        }
        let text = strip_ansi(&self.tail);
        if self.auth_url.is_none() {
            self.auth_url = authorize_url(&text);
        }
        if let Some(notice) = last_notice(&text) {
            self.notice = Some(notice);
        }
    }

    /// The URL the `open` shim recorded.
    pub fn opened(&mut self, url: &str) {
        let url = url.trim();
        if !url.is_empty() {
            self.local_url = Some(url.to_owned());
        }
    }

    pub fn state(&self) -> LoginState {
        match (&self.auth_url, &self.local_url) {
            (Some(auth), local) => LoginState::AwaitingBrowser {
                auth_url: auth.clone(),
                local_url: local.clone(),
                notice: self.notice.clone(),
            },
            // Only the local URL: still useful from a browser on this host.
            (None, Some(local)) => LoginState::AwaitingBrowser {
                auth_url: local.clone(),
                local_url: Some(local.clone()),
                notice: self.notice.clone(),
            },
            (None, None) if self.seen_output => LoginState::AwaitingUrl,
            (None, None) => LoginState::Starting,
        }
    }

    /// The verdict once the child exited: success needs exit 0 *and* a new
    /// `.credentials.json` (the CLI exits 0 too when it could not save).
    pub fn finish(&self, status: ExitStatus, material: Result<Material, String>) -> Outcome {
        if !status.success() {
            return Outcome::Failed(
                self.notice
                    .clone()
                    .unwrap_or_else(|| format!("claude auth login exited with {status}")),
            );
        }
        match material {
            Ok(m) => Outcome::Authorized(m),
            Err(e) => Outcome::Failed(match &self.notice {
                Some(n) => format!("{n} ({e})"),
                None => format!("claude reported success but {e}"),
            }),
        }
    }
}

/// What the keeper sends a running flow.
pub struct Control {
    /// A code pasted from the authorize page.
    pub codes: mpsc::Receiver<String>,
    /// `true` cancels.
    pub cancel: watch::Receiver<bool>,
}

/// Runs logins.
#[derive(Debug, Clone)]
pub struct Login {
    pub claude_bin: PathBuf,
    pub isolation: Isolation,
    pub timeout: Duration,
}

impl Login {
    /// End a flow left behind by a previous keeper process, and forget it.
    /// The pid file must name a live process that leads its own process
    /// group and runs `<claude_bin> auth login --claudeai`; anything else is
    /// left alone. `SIGTERM` to the group, `SIGKILL` after [`KILL_GRACE`].
    pub async fn reap_orphan(&self) {
        let pid_file = self.isolation.pid_file();
        let pid = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 1);
        let _ = std::fs::remove_file(&pid_file);
        let Some(pid) = pid else { return };
        let Some((pgid, command)) = ps(pid, "pgid=,command=").and_then(|out| {
            let out = out.trim_start();
            let (pgid, command) = out.split_once(char::is_whitespace)?;
            Some((pgid.parse::<i32>().ok()?, command.to_owned()))
        }) else {
            return;
        };
        let ours = pgid == pid
            && command.contains(&*self.claude_bin.to_string_lossy())
            && command.contains("auth login --claudeai");
        if !ours {
            tracing::info!(
                event = "orphan_pid_ignored",
                pid,
                "the pid file names another process"
            );
            return;
        }
        // SAFETY: plain syscall on the group the process just identified leads.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = tokio::time::Instant::now() + KILL_GRACE;
        while alive(pid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let killed = alive(pid);
        if killed {
            // SAFETY: as above.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        tracing::warn!(event = "orphan_flow_ended", pid, killed);
    }

    /// One run, start to verdict; every state change goes to `state`.
    pub async fn run(
        &self,
        credential_ref: &str,
        state: &watch::Sender<LoginState>,
        control: Control,
    ) -> Outcome {
        state.send_replace(LoginState::Starting);
        let iso = &self.isolation;
        if let Err(e) = iso.prepare() {
            return Outcome::Failed(format!("cannot prepare {}: {e}", iso.root.display()));
        }
        // A login starts logged out: whatever an earlier run left is not
        // this run's result.
        for stale in [iso.credentials_file(), iso.open_file()] {
            let _ = std::fs::remove_file(stale);
        }
        let mut cmd = iso.command(&self.claude_bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Outcome::Failed(format!("cannot run {}: {e}", self.claude_bin.display()))
            }
        };
        let pid = child.id();
        if let Some(pid) = pid {
            let _ = std::fs::write(iso.pid_file(), pid.to_string());
        }
        tracing::info!(event = "login_started", credential_ref, pid, claude = %self.claude_bin.display(),
            config_dir = %iso.config_dir().display());
        let outcome = self.drive(credential_ref, &mut child, state, control).await;
        let _ = std::fs::remove_file(iso.pid_file());
        outcome
    }

    async fn drive(
        &self,
        credential_ref: &str,
        child: &mut Child,
        state: &watch::Sender<LoginState>,
        mut control: Control,
    ) -> Outcome {
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut machine = Machine::new();
        let mut published = LoginState::Starting;
        let mut poll = tokio::time::interval(OPEN_POLL);
        let deadline = tokio::time::sleep(self.timeout);
        tokio::pin!(deadline);
        let (mut out_buf, mut err_buf) = ([0u8; 4096], [0u8; 4096]);
        let status = loop {
            tokio::select! {
                n = read_some(&mut stdout, &mut out_buf) => machine.feed(&out_buf[..n]),
                n = read_some(&mut stderr, &mut err_buf) => machine.feed(&err_buf[..n]),
                _ = poll.tick() => {
                    if let Ok(url) = std::fs::read_to_string(self.isolation.open_file()) {
                        machine.opened(&url);
                    }
                }
                code = control.codes.recv() => {
                    let Some(code) = code else { continue };
                    match stdin.as_mut() {
                        Some(pipe) => {
                            let line = format!("{}\n", code.trim());
                            let written = pipe.write_all(line.as_bytes()).await.and(pipe.flush().await);
                            tracing::info!(event = "login_code_submitted", credential_ref, written = written.is_ok());
                        }
                        None => tracing::warn!(event = "login_code_dropped", credential_ref),
                    }
                }
                changed = control.cancel.changed() => {
                    if changed.is_err() || *control.cancel.borrow() {
                        terminate(child).await;
                        return Outcome::Cancelled;
                    }
                }
                () = &mut deadline => {
                    terminate(child).await;
                    return Outcome::Failed(format!(
                        "no authorization within {}",
                        humantime::format_duration(self.timeout)
                    ));
                }
                status = child.wait() => break status,
            }
            while let Some(news) = machine.announce() {
                match news {
                    Announce::Url {
                        auth_url,
                        local_url,
                    } => {
                        tracing::info!(event = "login_url", credential_ref, auth_url = %auth_url,
                            local_url = local_url.as_deref().map(redact_query),
                            "authorize this login: open auth_url, then paste the code it shows");
                    }
                    Announce::Notice(notice) => {
                        tracing::info!(event = "login_notice", credential_ref, notice = %notice);
                    }
                }
            }
            let now = machine.state();
            if now != published {
                state.send_replace(now.clone());
                published = now;
            }
        };
        drop(stdin);
        // Output written just before exit.
        let _ = tokio::time::timeout(DRAIN, async {
            while stdout.is_some() || stderr.is_some() {
                tokio::select! {
                    n = read_some(&mut stdout, &mut out_buf) => machine.feed(&out_buf[..n]),
                    n = read_some(&mut stderr, &mut err_buf) => machine.feed(&err_buf[..n]),
                }
            }
        })
        .await;
        match status {
            Ok(status) => {
                machine.finish(status, Material::read(&self.isolation.credentials_file()))
            }
            Err(e) => Outcome::Failed(format!("cannot wait for claude: {e}")),
        }
    }
}

/// `ps -o <fields> -p <pid>`, or `None` when no such process is listed.
fn ps(pid: i32, fields: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", fields, "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The process exists and is not a zombie.
fn alive(pid: i32) -> bool {
    ps(pid, "stat=").is_some_and(|stat| !stat.starts_with('Z'))
}

/// Read from a pipe that may already be closed: a closed pipe is set to
/// `None` and never resolves again.
async fn read_some<R: AsyncReadExt + Unpin>(pipe: &mut Option<R>, buf: &mut [u8]) -> usize {
    let Some(reader) = pipe.as_mut() else {
        return std::future::pending().await;
    };
    match reader.read(buf).await {
        Ok(0) | Err(_) => {
            *pipe = None;
            0
        }
        Ok(n) => n,
    }
}

/// `SIGTERM` to the child's process group, `SIGKILL` after the grace.
async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        // SAFETY: plain syscall on the group our own child leads.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    const PRINTED: &[u8] = b"Opening browser to sign in\xe2\x80\xa6\n\
        If the browser didn't open, visit: \x1b]8;;https://claude.com/cai/oauth/authorize?code=true&state=s\x07\
        https://claude.com/cai/oauth/authorize?code=true&state=s\x1b]8;;\x07\n\
        Paste code here if prompted > ";

    #[test]
    fn states_follow_the_output() {
        let mut m = Machine::new();
        assert_eq!(m.state(), LoginState::Starting);
        m.feed(&PRINTED[..30]);
        assert_eq!(m.state(), LoginState::AwaitingUrl);
        m.feed(&PRINTED[30..]);
        let LoginState::AwaitingBrowser {
            auth_url,
            local_url,
            notice,
        } = m.state()
        else {
            panic!("expected AwaitingBrowser, got {:?}", m.state());
        };
        assert_eq!(
            auth_url,
            "https://claude.com/cai/oauth/authorize?code=true&state=s"
        );
        assert_eq!(local_url, None);
        assert_eq!(notice, None);
        m.opened("https://claude.com/cai/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback\n");
        m.feed(b"Invalid code. Please make sure the full code was copied.\nPaste code here if prompted > ");
        let LoginState::AwaitingBrowser {
            local_url, notice, ..
        } = m.state()
        else {
            panic!("expected AwaitingBrowser");
        };
        assert!(local_url.is_some_and(|u| u.contains("localhost")));
        assert!(notice.is_some_and(|n| n.starts_with("Invalid code")));
    }

    #[test]
    fn a_url_split_across_reads_is_taken_whole() {
        let mut m = Machine::new();
        m.feed(b"visit: https://claude.com/cai/oauth/auth");
        assert_eq!(m.state(), LoginState::AwaitingUrl);
        m.feed(b"orize?state=abc\n");
        assert!(
            matches!(m.state(), LoginState::AwaitingBrowser { auth_url, .. } if auth_url.ends_with("state=abc"))
        );
    }

    #[test]
    fn verdicts_need_exit_zero_and_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(".credentials.json");
        std::fs::write(
            &file,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-T","refreshToken":"sk-ant-ort01-T","expiresAt":1900000000000},"other":1}"#,
        )
        .expect("write");
        let m = Machine::new();
        let Outcome::Authorized(material) = m.finish(exit(0), Material::read(&file)) else {
            panic!("expected Authorized");
        };
        assert!(material.document().get("other").is_none());
        assert_eq!(format!("{material:?}"), "Material(<redacted>)");
        assert!(material.expires_at().is_some());

        let missing = Material::read(&dir.path().join("absent"));
        assert!(
            matches!(m.finish(exit(0), missing), Outcome::Failed(r) if r.starts_with("claude reported success"))
        );

        let mut failed = Machine::new();
        failed.feed(b"Login failed: code expired\n");
        assert!(
            matches!(failed.finish(exit(1), Material::read(&file)), Outcome::Failed(r) if r == "Login failed: code expired")
        );
        assert!(
            matches!(Machine::new().finish(exit(3), Material::read(&file)), Outcome::Failed(r) if r.contains("exited"))
        );
    }

    #[test]
    fn material_without_a_refresh_token_is_refused_without_quoting_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(".credentials.json");
        std::fs::write(
            &file,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-SECRET"}}"#,
        )
        .expect("write");
        let err = Material::read(&file).unwrap_err();
        assert!(
            err.contains("refreshToken") && !err.contains("SECRET"),
            "{err}"
        );
    }

    #[test]
    fn shims_and_environment_isolate_the_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let iso = Isolation::new(dir.path());
        iso.prepare().expect("prepare");
        for shim in ["open", "xdg-open", "security"] {
            let mode = std::fs::metadata(iso.bin_dir().join(shim))
                .expect("shim")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{shim}");
        }
        let status = std::process::Command::new(iso.bin_dir().join("security"))
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials-ab12cd34",
            ])
            .status()
            .expect("run shim");
        assert_eq!(status.code(), Some(44));
        let status = std::process::Command::new(iso.bin_dir().join("security"))
            .args([
                "add-generic-password",
                "-U",
                "-a",
                "me",
                "-s",
                "svc",
                "-w",
                "SECRET",
            ])
            .status()
            .expect("run shim");
        assert_eq!(status.code(), Some(44));
        assert_eq!(
            std::fs::read_to_string(iso.security_log()).expect("security log"),
            "find-generic-password -s Claude Code-credentials-ab12cd34\n\
             add-generic-password -s svc\n",
            "names only, no secret"
        );
        let opened = std::process::Command::new(iso.bin_dir().join("open"))
            .arg("https://claude.com/cai/oauth/authorize?x=1")
            .status()
            .expect("run shim");
        assert!(opened.success());
        assert_eq!(
            std::fs::read_to_string(iso.open_file()).expect("url file"),
            "https://claude.com/cai/oauth/authorize?x=1\n"
        );

        let cmd = iso.command(Path::new("/opt/claude/bin/claude"));
        let env: std::collections::HashMap<_, _> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(
            env["CLAUDE_CONFIG_DIR"],
            iso.config_dir().display().to_string()
        );
        assert_eq!(env["HOME"], iso.home().display().to_string());
        assert!(env["PATH"].starts_with(&iso.bin_dir().display().to_string()));
        assert!(env["PATH"].contains("/opt/claude/bin"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn the_url_is_announced_once_and_each_new_notice_once() {
        let mut m = Machine::new();
        m.feed(PRINTED);
        assert!(matches!(
            m.announce(),
            Some(Announce::Url {
                local_url: None,
                ..
            })
        ));
        m.opened("https://claude.com/cai/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback");
        assert_eq!(
            m.announce(),
            None,
            "the local URL does not repeat the announcement"
        );
        m.feed(b"Invalid code. Please make sure the full code was copied.\n");
        assert!(matches!(m.announce(), Some(Announce::Notice(n)) if n.starts_with("Invalid code")));
        m.feed(b"Paste code here if prompted > ");
        assert_eq!(m.announce(), None);
    }

    #[test]
    fn prepare_tightens_existing_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let iso = Isolation::new(dir.path());
        std::fs::create_dir_all(iso.config_dir()).expect("mkdir");
        std::fs::set_permissions(iso.config_dir(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        iso.prepare().expect("prepare");
        for d in [iso.config_dir(), iso.pending_dir(), iso.home()] {
            let mode = std::fs::metadata(&d).expect("dir").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", d.display());
        }
    }

    #[test]
    fn pending_files_are_per_credential_and_safe_names() {
        let iso = Isolation::new(Path::new("/k"));
        assert_eq!(
            iso.pending_file("claude-sub"),
            Path::new("/k/pending/claude-sub.json")
        );
        assert_eq!(
            iso.pending_file("a/../b"),
            Path::new("/k/pending/a%2F%2E%2E%2Fb.json")
        );
        assert_ne!(iso.pending_file("sub-a"), iso.pending_file("sub-b"));
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
