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
//!   `<data_dir>/admin-token` when it exists and the address is a loopback
//!   one (the CLI is on the daemon's host); a remote address always needs
//!   an explicit token, so the local token never goes to another host.
//!   Never an argument: an argument is in shell history and in every
//!   local user's view of the process list.

use std::path::{Path, PathBuf};

use janus_core::contract::paths;
use janus_daemon::DaemonConfig;
use url::{Host, Url};

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
            Address::Http(url) => {
                let local = is_loopback(url);
                let token = self.token(local)?;
                if token.is_some() && !local && url.scheme() == "http" {
                    crate::render::note(&format!(
                        "warning: sending the admin token over plain http to {}; use https",
                        url.host_str().unwrap_or_default()
                    ));
                }
                token
            }
        };
        Admin::new(
            address,
            HttpOptions {
                ca_file: self.ca_file.clone(),
                token,
            },
        )
    }

    fn token(&self, local: bool) -> Result<Option<String>, CliError> {
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
        if local {
            if let Some(dir) = self.data_dir()? {
                let path = paths::admin_token_path(&dir);
                if path.exists() {
                    return read_token_file(&path).map(Some);
                }
            }
        }
        Ok(None)
    }
}

/// Whether `url` names this machine: `localhost` or a loopback IP.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback(text: &str) -> bool {
        is_loopback(&Url::parse(text).expect("url"))
    }

    #[test]
    fn only_local_hosts_are_loopback() {
        for local in [
            "http://127.0.0.1:1",
            "http://127.9.9.9:1",
            "http://[::1]:1",
            "http://localhost:1",
            "https://LOCALHOST:1",
        ] {
            assert!(loopback(local), "{local}");
        }
        for remote in [
            "http://10.0.0.5:1",
            "http://example.com",
            "http://localhost.example.com",
            "http://0.0.0.0:1",
        ] {
            assert!(!loopback(remote), "{remote}");
        }
    }

    #[test]
    fn the_local_token_file_is_not_auto_loaded_for_a_remote_address() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(paths::admin_token_path(dir.path()), "local-secret\n").expect("token");
        let config = dir.path().join("janus.toml");
        std::fs::write(
            &config,
            format!(
                "data_dir = {:?}\n[data_plane]\nbind = \"127.0.0.1:0\"\n",
                dir.path().display().to_string()
            ),
        )
        .expect("config");
        let conn = Connection {
            admin: None,
            token_file: None,
            ca_file: None,
            config: Some(config),
        };
        // Only the explicit sources can supply a token otherwise.
        assert!(std::env::var_os(TOKEN_ENV).is_none());
        assert_eq!(
            conn.token(true).expect("ok").as_deref(),
            Some("local-secret")
        );
        assert_eq!(conn.token(false).expect("ok"), None);
    }
}
