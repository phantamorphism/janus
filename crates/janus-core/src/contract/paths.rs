//! The daemon's data directory and the fixed files inside it.
//!
//! Shared by the daemon (which creates them) and the `janus` CLI (which
//! resolves the admin socket and token "the same way the daemon does",
//! `docs/admin-surface#cli`).

use std::path::{Path, PathBuf};

/// The admin unix socket, mode `0600` (`docs/architecture#admin-surface`).
pub const ADMIN_SOCKET_FILE: &str = "admin.sock";
/// The admin token for TCP admin and UI listeners, mode `0600`.
pub const ADMIN_TOKEN_FILE: &str = "admin-token";
/// The daemon's SQLite database: identities registry and request log
/// (`docs/admin-surface#storage`).
pub const DATABASE_FILE: &str = "janus.db";
/// Environment variable naming the data directory when the daemon config
/// does not set `data_dir`.
pub const DATA_DIR_ENV: &str = "JANUS_DATA_DIR";
/// Directory name under `$HOME` used when neither config nor
/// [`DATA_DIR_ENV`] names a data directory.
pub const DEFAULT_DATA_DIR_NAME: &str = ".janus";

/// Resolve the data directory: an explicit `configured` value (the daemon
/// config's `data_dir`) wins, then `$JANUS_DATA_DIR`, then `$HOME/.janus`.
/// `None` only when none of the three is available.
pub fn resolve_data_dir(configured: Option<&Path>) -> Option<PathBuf> {
    resolve_data_dir_from(
        configured,
        std::env::var_os(DATA_DIR_ENV).map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve_data_dir_from(
    configured: Option<&Path>,
    env_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return Some(dir.to_path_buf());
    }
    if let Some(dir) = env_dir.filter(|d| !d.as_os_str().is_empty()) {
        return Some(dir);
    }
    home.filter(|h| !h.as_os_str().is_empty())
        .map(|h| h.join(DEFAULT_DATA_DIR_NAME))
}

/// `<data_dir>/admin.sock`.
pub fn admin_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ADMIN_SOCKET_FILE)
}

/// `<data_dir>/admin-token`.
pub fn admin_token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ADMIN_TOKEN_FILE)
}

/// `<data_dir>/janus.db`.
pub fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DATABASE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_precedence_is_config_then_env_then_home() {
        let cfg = PathBuf::from("/etc/janus-data");
        let env = PathBuf::from("/var/lib/janus");
        let home = PathBuf::from("/home/op");
        assert_eq!(
            resolve_data_dir_from(Some(&cfg), Some(env.clone()), Some(home.clone())),
            Some(cfg)
        );
        assert_eq!(
            resolve_data_dir_from(None, Some(env.clone()), Some(home.clone())),
            Some(env)
        );
        assert_eq!(
            resolve_data_dir_from(None, Some(PathBuf::new()), Some(home.clone())),
            Some(home.join(".janus"))
        );
        assert_eq!(resolve_data_dir_from(None, None, None), None);
        let dir = Path::new("/d");
        assert_eq!(admin_socket_path(dir), Path::new("/d/admin.sock"));
        assert_eq!(admin_token_path(dir), Path::new("/d/admin-token"));
        assert_eq!(database_path(dir), Path::new("/d/janus.db"));
    }
}
