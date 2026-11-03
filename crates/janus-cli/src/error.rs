//! What can go wrong, and the exit code each failure maps to.
//!
//! Exit codes (also in `janus --help`):
//!
//! - `0` — success;
//! - `1` — the daemon refused or failed the operation (an admin API error),
//!   or a check the command ran did not pass (`plugin check`,
//!   `check-config`);
//! - `2` — the command line or local input is wrong (bad flags, no material
//!   on stdin, an unreadable file);
//! - `3` — the daemon could not be reached (no socket, connection refused,
//!   TLS failure, timeout).

use std::fmt;

pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_UNREACHABLE: u8 = 3;

#[derive(Debug)]
pub enum CliError {
    /// The command line or local input is wrong.
    Usage(String),
    /// The daemon could not be reached.
    Unreachable(String),
    /// The admin API answered with its error envelope.
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// The operation ran and did not succeed.
    Failed { code: String, message: String },
    /// Standard output went away (`janus log tail | head`): stop quietly.
    StdoutClosed,
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        CliError::Usage(message.into())
    }

    pub fn failed(code: &str, message: impl Into<String>) -> Self {
        CliError::Failed {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Usage(_) => EXIT_USAGE,
            CliError::Unreachable(_) => EXIT_UNREACHABLE,
            CliError::Api { .. } | CliError::Failed { .. } => EXIT_FAILED,
            CliError::StdoutClosed => 0,
        }
    }

    /// The stable code, as `--json` reports it.
    pub fn code(&self) -> &str {
        match self {
            CliError::Usage(_) => "usage",
            CliError::Unreachable(_) => "unreachable",
            CliError::Api { code, .. } | CliError::Failed { code, .. } => code,
            CliError::StdoutClosed => "stdout_closed",
        }
    }

    /// The error as `--json` prints it on stderr: the admin API's envelope,
    /// plus the HTTP status when there was one.
    pub fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::json!({ "code": self.code(), "message": self.message() });
        if let CliError::Api { status, .. } = self {
            error["status"] = (*status).into();
        }
        serde_json::json!({ "error": error })
    }

    fn message(&self) -> String {
        match self {
            CliError::Usage(m) | CliError::Unreachable(m) => m.clone(),
            CliError::Api { message, .. } | CliError::Failed { message, .. } => message.clone(),
            CliError::StdoutClosed => "standard output was closed".to_owned(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Api {
                status,
                code,
                message,
            } => write!(f, "{message} ({code}, HTTP {status})"),
            other => f.write_str(&other.message()),
        }
    }
}

/// `e` and its sources, joined: reqwest and hyper put the useful part
/// (connection refused, the TLS alert) several levels down.
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = s.source();
    }
    text
}
