//! Resolving where the admin surface is and how to authenticate to it
//! (`docs/admin-surface#cli`, "Connection").
//!
//! - Address: `--admin` / `JANUS_ADMIN`, else the daemon's admin socket
//!   `<data_dir>/admin.sock`, with `data_dir` resolved the way the daemon
//!   resolves it: the `data_dir` of the daemon's config (`--config`), else
//!   `$JANUS_DATA_DIR`, else `$HOME/.janus`
//!   (`janus_core::contract::paths::resolve_data_dir`).
//! - Token, for an `http(s)` address only: `--token-file` /
//!   `JANUS_TOKEN_FILE`, else the token itself in `JANUS_ADMIN_TOKEN`, else
//!   `<data_dir>/admin-token` when it exists (the CLI is on the daemon's
//!   host). Never an argument: an argument is in shell history and in every
//!   local user's view of the process list.

use std::path::{Path, PathBuf};

use janus_core::contract::paths;
use janus_daemon::DaemonConfig;

use crate::client::{Address, Admin, HttpOptions};
use crate::error::CliError;

/// The environment variable that may hold the admin token itself. Read
/// directly rather than through clap, so `--help` never displays it.
pub const TOKEN_ENV: &str = "JANUS_ADMIN_TOKEN";

/// The connection flags shared by every command.
pub struct Connection {
    pub admin: Option<String>,
    pub token_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

impl Connection {
    /// The data directory, resolved as the daemon resolves it.
    pub fn data_dir(&self) -> Result<Option<PathBuf>, CliError> {
        let configured = match &self.config {
            Some(path) => {
                DaemonConfig::load(path)
                    .map_err(|e| {
                        CliError::usage(format!("cannot use --config {}: {e}", path.display()))
                    })?
                    .data_dir
            }
            None => None,
        };
        Ok(paths::resolve_data_dir(configured.as_deref()))
    }

    pub fn address(&self) -> Result<Address, CliError> {
        if let Some(text) = &self.admin {
            return Address::parse(text);
        }
        match self.data_dir()? {
            Some(dir) => Ok(Address::default_socket(&dir)),
            None => Err(CliError::usage(
                "cannot locate the daemon's admin socket: neither JANUS_DATA_DIR nor HOME is set; \
                 pass --admin <address>, set JANUS_ADMIN, or name the daemon's config with --config",
            )),
        }
    }

    /// A client for the resolved address.
    pub fn admin(&self) -> Result<Admin, CliError> {
        let address = self.address()?;
        let token = match &address {
            Address::Unix(_) => None,
            Address::Http(_) => self.token()?,
        };
        Admin::new(
            address,
            HttpOptions {
                ca_file: self.ca_file.clone(),
                token,
            },
        )
    }

    fn token(&self) -> Result<Option<String>, CliError> {
        if let Some(path) = &self.token_file {
            return read_token_file(path).map(Some);
        }
        if let Some(value) = std::env::var_os(TOKEN_ENV) {
            let value = value
                .into_string()
                .map_err(|_| CliError::usage(format!("{TOKEN_ENV} is not valid UTF-8")))?;
            let value = value.trim();
            if value.is_empty() {
                return Err(CliError::usage(format!("{TOKEN_ENV} is set but empty")));
            }
            return Ok(Some(value.to_owned()));
        }
        if let Some(dir) = self.data_dir()? {
            let path = paths::admin_token_path(&dir);
            if path.exists() {
                return read_token_file(&path).map(Some);
            }
        }
        Ok(None)
    }
}

fn read_token_file(path: &Path) -> Result<String, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::usage(format!(
            "cannot read the admin token file {}: {e}",
            path.display()
        ))
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(CliError::usage(format!(
            "the admin token file {} is empty",
            path.display()
        )));
    }
    Ok(token.to_owned())
}
