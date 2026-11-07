//! The keeper's configuration file (TOML), read once at startup.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// The whole file. See `keeper.example.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The unix socket the gRPC plugin service listens on.
    pub socket: PathBuf,
    /// Refuse gRPC calls from any peer not running as this uid.
    #[serde(default)]
    pub daemon_uid: Option<u32>,
    /// The keeper's own directory. `claude` runs with `HOME=<state_dir>/home`
    /// and `CLAUDE_CONFIG_DIR=<state_dir>/claude`, so its login never shares
    /// a refresh token with the operator's own Claude Code.
    pub state_dir: PathBuf,
    /// The `claude` executable. A bare name is looked up on `PATH` at
    /// startup, because the child runs with a reduced `PATH`.
    #[serde(default = "default_claude")]
    pub claude_bin: PathBuf,
    /// The daemon's admin surface: `unix:<path>` (no token) or
    /// `http://<loopback>:<port>` (bearer token from `admin_token_file`).
    /// Default: `unix:<data dir>/admin.sock`, the data dir resolved as the
    /// daemon does (`$JANUS_DATA_DIR`, else `$HOME/.janus`).
    #[serde(default)]
    pub admin: Option<String>,
    /// The admin token for an `http://` admin address. Default:
    /// `<data dir>/admin-token`.
    #[serde(default)]
    pub admin_token_file: Option<PathBuf>,
    /// The loopback page and its control endpoints.
    #[serde(default = "default_http_bind")]
    pub http_bind: SocketAddr,
    /// How often each managed credential is checked through the admin API.
    #[serde(default = "default_validate_interval", with = "humantime_serde")]
    pub validate_interval: Duration,
    /// An OAuth credential expiring within this window is refreshed by the
    /// periodic check, so an idle credential does not lapse unnoticed.
    #[serde(default = "default_refresh_ahead", with = "humantime_serde")]
    pub refresh_ahead: Duration,
    /// How long one login flow waits for the operator's authorization.
    #[serde(default = "default_login_timeout", with = "humantime_serde")]
    pub login_timeout: Duration,
    /// A problem report waits this long before a flow starts, so a burst of
    /// `credential_invalid` events starts one flow.
    #[serde(default = "default_debounce", with = "humantime_serde")]
    pub debounce: Duration,
    /// The wait after the first failed flow; it doubles per failure.
    #[serde(default = "default_backoff_min", with = "humantime_serde")]
    pub backoff_min: Duration,
    /// The longest wait between automatic flows.
    #[serde(default = "default_backoff_max", with = "humantime_serde")]
    pub backoff_max: Duration,
    /// Deny requests to a managed instance while every managed credential
    /// for it is known to need a login, with a reason that says how to
    /// authorize. `false` answers `use_default` always.
    #[serde(default = "default_true")]
    pub deny_when_unusable: bool,
    /// The credentials this keeper looks after.
    #[serde(rename = "credential")]
    pub credentials: Vec<ManagedCredential>,
}

/// One `[[credential]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCredential {
    /// The pool slot's ref (a `kind = "oauth"` Anthropic credential).
    #[serde(rename = "ref")]
    pub credential_ref: String,
    /// The upstream instances this credential serves. Requests to these
    /// instances are the ones a deny applies to.
    #[serde(default)]
    pub instances: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    /// One line: where in the file, and what is wrong there.
    #[error("{path}: {detail}")]
    Parse { path: String, detail: String },
    #[error("invalid config: {0}")]
    Invalid(String),
}

fn default_claude() -> PathBuf {
    PathBuf::from("claude")
}
fn default_http_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 27711))
}
fn default_validate_interval() -> Duration {
    Duration::from_secs(300)
}
fn default_refresh_ahead() -> Duration {
    Duration::from_secs(900)
}
fn default_login_timeout() -> Duration {
    Duration::from_secs(600)
}
fn default_debounce() -> Duration {
    Duration::from_secs(10)
}
fn default_backoff_min() -> Duration {
    Duration::from_secs(60)
}
fn default_backoff_max() -> Duration {
    Duration::from_secs(3600)
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut config = Config::parse(&text).map_err(|e| match e {
            ConfigError::Parse { detail, .. } => ConfigError::Parse {
                path: path.display().to_string(),
                detail,
            },
            other => other,
        })?;
        config.claude_bin = resolve_bin(&config.claude_bin)?;
        Ok(config)
    }

    /// Parse and validate, without touching the filesystem.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: "config".into(),
            detail: parse_detail(text, &e),
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        if self.credentials.is_empty() {
            return invalid("at least one [[credential]] is required".into());
        }
        for (i, c) in self.credentials.iter().enumerate() {
            if c.credential_ref.is_empty() {
                return invalid(format!("credential #{} has an empty ref", i + 1));
            }
            if self.credentials[..i]
                .iter()
                .any(|o| o.credential_ref == c.credential_ref)
            {
                return invalid(format!("credential {:?} is listed twice", c.credential_ref));
            }
        }
        if !self.http_bind.ip().is_loopback() {
            return invalid(format!(
                "http_bind {} is not a loopback address; the page is unauthenticated",
                self.http_bind
            ));
        }
        if !self.state_dir.is_absolute() || !self.socket.is_absolute() {
            return invalid("state_dir and socket must be absolute paths".into());
        }
        if self.backoff_min.is_zero() || self.backoff_max < self.backoff_min {
            return invalid("backoff_min must be non-zero and at most backoff_max".into());
        }
        if self.login_timeout.is_zero() || self.validate_interval.is_zero() {
            return invalid("login_timeout and validate_interval must be non-zero".into());
        }
        Ok(())
    }

    /// The `[[credential]]` entry for `credential_ref`.
    pub fn credential(&self, credential_ref: &str) -> Option<&ManagedCredential> {
        self.credentials
            .iter()
            .find(|c| c.credential_ref == credential_ref)
    }

    /// The managed credentials that serve `instance`.
    pub fn serving<'a>(
        &'a self,
        instance: &'a str,
    ) -> impl Iterator<Item = &'a ManagedCredential> + 'a {
        self.credentials
            .iter()
            .filter(move |c| c.instances.iter().any(|i| i == instance))
    }
}

/// `line L, column C: message` for a TOML error, without its source excerpt.
fn parse_detail(text: &str, e: &toml::de::Error) -> String {
    let message = e.message().trim().replace('\n', " ");
    let Some(span) = e.span() else {
        return message;
    };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    format!("line {line}, column {column}: {message}")
}

/// An absolute path for `bin`: as given when it has a directory part, else
/// the first match on this process's `PATH`.
fn resolve_bin(bin: &Path) -> Result<PathBuf, ConfigError> {
    if bin.components().count() > 1 {
        if !bin.is_absolute() || !bin.is_file() {
            return Err(ConfigError::Invalid(format!(
                "claude_bin {} is not an existing file at an absolute path",
                bin.display()
            )));
        }
        return Ok(bin.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "claude_bin {:?} is not on PATH; give an absolute path",
                bin.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
socket = "/home/op/.keeper/run/keeper.sock"
state_dir = "/home/op/.keeper/state"
[[credential]]
ref = "claude-sub"
instances = ["claude"]
"#;

    #[test]
    fn defaults_apply_to_a_minimal_file() {
        let c = Config::parse(MINIMAL).expect("parses");
        assert_eq!(c.http_bind.to_string(), "127.0.0.1:27711");
        assert_eq!(c.login_timeout, Duration::from_secs(600));
        assert!(c.deny_when_unusable);
        assert_eq!(c.serving("claude").count(), 1);
        assert_eq!(c.serving("other").count(), 0);
    }

    #[test]
    fn a_non_loopback_page_and_duplicate_refs_are_refused() {
        let public = format!("http_bind = \"0.0.0.0:8080\"\n{MINIMAL}");
        assert!(Config::parse(&public)
            .unwrap_err()
            .to_string()
            .contains("not a loopback"));
        let twice = format!("{MINIMAL}[[credential]]\nref = \"claude-sub\"\n");
        assert!(Config::parse(&twice)
            .unwrap_err()
            .to_string()
            .contains("listed twice"));
    }

    #[test]
    fn the_example_file_parses() {
        let c = Config::parse(include_str!("../keeper.example.toml")).expect("example parses");
        assert_eq!(c.credentials[0].credential_ref, "claude-sub");
        assert_eq!(c.backoff_max, Duration::from_secs(3600));
    }

    #[test]
    fn a_missing_claude_bin_is_refused_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("keeper.toml");
        for bin in ["/nonexistent/claude", "relative/claude"] {
            std::fs::write(&file, format!("claude_bin = {bin:?}\n{MINIMAL}")).expect("write");
            let err = Config::load(&file).unwrap_err().to_string();
            assert!(err.contains("not an existing file"), "{err}");
        }
        std::fs::write(&file, format!("claude_bin = \"/bin/sh\"\n{MINIMAL}")).expect("write");
        assert_eq!(
            Config::load(&file).expect("loads").claude_bin,
            Path::new("/bin/sh")
        );
    }

    #[test]
    fn a_parse_error_is_one_line_with_its_position() {
        let err = Config::parse("socket = 3\n").unwrap_err().to_string();
        assert_eq!(
            err,
            "config: line 1, column 10: invalid type: integer `3`, expected path string"
        );
    }

    #[test]
    fn durations_are_humantime() {
        let text = format!("backoff_min = \"30s\"\nbackoff_max = \"2h\"\n{MINIMAL}");
        let c = Config::parse(&text).expect("parses");
        assert_eq!(c.backoff_min, Duration::from_secs(30));
        assert_eq!(c.backoff_max, Duration::from_secs(7200));
        let bad = format!("backoff_min = \"2h\"\nbackoff_max = \"1m\"\n{MINIMAL}");
        assert!(Config::parse(&bad).is_err());
    }
}
