//! Socket-path hygiene for the unix-socket plugin transport
//! (`docs/plugin-contract#channel-unix`).
//!
//! The daemon walks every directory on the socket's path and rejects the
//! path if any of them is writable by group or others, or is owned by anyone
//! other than root, the daemon's user, or the owner of the socket's own
//! directory — `sshd`'s `StrictModes` posture. [`check_existing`] runs the
//! walk at startup over the part of the path that exists (the plugin may
//! create its directory later); [`check_full`] runs it over the full path
//! before every connection and reconnection.
//!
//! Symlinks are resolved before the walk, so the directories checked are
//! the ones a connection actually traverses.

use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// One directory the walk examined, deepest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalkedDir {
    pub path: PathBuf,
    pub uid: u32,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
}

/// The directories a successful walk examined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathWalk {
    /// The socket path the walk resolved to (symlinks followed), or the
    /// configured path when its directory does not exist yet.
    pub socket: PathBuf,
    pub dirs: Vec<WalkedDir>,
}

/// Why a socket path was refused. `dir` is the offending directory, which
/// is what `last_error` must name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SocketPathError {
    #[error("plugin socket path is unsafe: {} {reason}", dir.display())]
    Unsafe { dir: PathBuf, reason: String },
    #[error("plugin socket directory {} does not exist", dir.display())]
    MissingDir { dir: PathBuf },
    #[error("cannot inspect plugin socket path at {}: {message}", dir.display())]
    Io { dir: PathBuf, message: String },
}

impl SocketPathError {
    /// The directory the error is about.
    pub fn dir(&self) -> &Path {
        match self {
            SocketPathError::Unsafe { dir, .. }
            | SocketPathError::MissingDir { dir }
            | SocketPathError::Io { dir, .. } => dir,
        }
    }
}

/// Which uids may own a directory on the path.
#[derive(Debug, Clone, Copy)]
struct AllowedOwners {
    daemon: u32,
    /// The owner of the socket's own directory, once it exists.
    socket_dir: Option<u32>,
}

impl AllowedOwners {
    fn allows(&self, uid: u32) -> bool {
        uid == 0 || uid == self.daemon || self.socket_dir == Some(uid)
    }
}

impl fmt::Display for AllowedOwners {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "root (0), the daemon's user ({})", self.daemon)?;
        if let Some(uid) = self.socket_dir {
            write!(f, " or the socket directory's owner ({uid})")?;
        }
        Ok(())
    }
}

/// The rule applied to one directory, separated from the filesystem so it
/// can be tested for owners a test cannot create.
fn violation(uid: u32, mode: u32, owners: AllowedOwners) -> Option<String> {
    if mode & 0o022 != 0 {
        let who = match (mode & 0o020 != 0, mode & 0o002 != 0) {
            (true, true) => "group and others",
            (true, false) => "group",
            _ => "others",
        };
        return Some(format!("is writable by {who} (mode {:04o})", mode & 0o7777));
    }
    if !owners.allows(uid) {
        return Some(format!("is owned by uid {uid}, which is not {owners}"));
    }
    None
}

fn daemon_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn io_error(dir: &Path, err: io::Error) -> SocketPathError {
    SocketPathError::Io {
        dir: dir.to_path_buf(),
        message: err.to_string(),
    }
}

/// Walk `dir` and every ancestor of it, which must already be canonical.
fn walk(dir: &Path, owners: AllowedOwners) -> Result<Vec<WalkedDir>, SocketPathError> {
    let mut walked = Vec::new();
    for ancestor in dir.ancestors() {
        let meta = std::fs::metadata(ancestor).map_err(|e| io_error(ancestor, e))?;
        let mode = meta.mode() & 0o7777;
        if let Some(reason) = violation(meta.uid(), mode, owners) {
            return Err(SocketPathError::Unsafe {
                dir: ancestor.to_path_buf(),
                reason,
            });
        }
        walked.push(WalkedDir {
            path: ancestor.to_path_buf(),
            uid: meta.uid(),
            mode,
        });
    }
    Ok(walked)
}

fn socket_dir(socket: &Path) -> &Path {
    socket.parent().unwrap_or(Path::new("/"))
}

/// The per-connect check: the socket's directory must exist and the whole
/// path must pass the walk. Returns the resolved socket path, which is what
/// the connection should use.
pub fn check_full(socket: &Path) -> Result<PathWalk, SocketPathError> {
    let dir = socket_dir(socket);
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SocketPathError::MissingDir {
                dir: dir.to_path_buf(),
            })
        }
        Err(e) => return Err(io_error(dir, e)),
    };
    let file_name = socket.file_name().ok_or_else(|| SocketPathError::Io {
        dir: dir.to_path_buf(),
        message: "socket path has no file name".to_owned(),
    })?;
    // A socket that is itself a symlink is resolved too: the directory that
    // matters is the one holding the real socket.
    let mut resolved = canonical_dir.join(file_name);
    if std::fs::symlink_metadata(&resolved).is_ok_and(|m| m.file_type().is_symlink()) {
        resolved = std::fs::canonicalize(&resolved).map_err(|e| io_error(&resolved, e))?;
    }
    let real_dir = socket_dir(&resolved).to_path_buf();
    let meta = std::fs::metadata(&real_dir).map_err(|e| io_error(&real_dir, e))?;
    let owners = AllowedOwners {
        daemon: daemon_uid(),
        socket_dir: Some(meta.uid()),
    };
    let dirs = walk(&real_dir, owners)?;
    Ok(PathWalk {
        socket: resolved,
        dirs,
    })
}

/// The startup check: the walk over the deepest existing ancestor of the
/// socket's directory. When the directory itself exists this is the full
/// check; when it does not, its eventual owner is unknown, so ancestors must
/// be owned by root or the daemon's user.
pub fn check_existing(socket: &Path) -> Result<PathWalk, SocketPathError> {
    let dir = socket_dir(socket);
    if dir.exists() {
        return check_full(socket);
    }
    let existing = dir
        .ancestors()
        .find(|a| a.exists())
        .unwrap_or(Path::new("/"));
    let canonical = std::fs::canonicalize(existing).map_err(|e| io_error(existing, e))?;
    let owners = AllowedOwners {
        daemon: daemon_uid(),
        socket_dir: None,
    };
    let dirs = walk(&canonical, owners)?;
    Ok(PathWalk {
        socket: socket.to_path_buf(),
        dirs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    fn plugin_dir(root: &Path) -> PathBuf {
        let dir = root.join("plugin");
        std::fs::create_dir(&dir).expect("mkdir");
        chmod(&dir, 0o750);
        dir
    }

    #[test]
    fn unix_socket_rejects_writable_parent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = plugin_dir(tmp.path());
        let socket = dir.join("policy.sock");
        check_full(&socket).expect("0750 directory is sound");

        chmod(&dir, 0o770);
        let err = check_full(&socket).expect_err("group-writable parent must be refused");
        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        assert_eq!(err.dir(), canonical.as_path());
        assert!(err.to_string().contains("writable by group"), "{err}");
        assert!(err.to_string().contains("0770"), "{err}");

        chmod(&dir, 0o757);
        let err = check_full(&socket).expect_err("other-writable parent must be refused");
        assert!(err.to_string().contains("writable by others"), "{err}");
    }

    #[test]
    fn unix_socket_rejects_writable_ancestor_further_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = tmp.path().join("outer");
        std::fs::create_dir(&outer).expect("mkdir");
        let dir = plugin_dir(&outer);
        chmod(&outer, 0o777);
        let err = check_full(&dir.join("p.sock")).expect_err("world-writable ancestor");
        assert_eq!(
            err.dir(),
            std::fs::canonicalize(&outer).expect("canonical").as_path()
        );
        // The same ancestor is caught by the startup walk even before the
        // plugin has created its own directory.
        let not_yet = outer.join("later").join("p.sock");
        let err = check_existing(&not_yet).expect_err("startup walk covers existing part");
        assert!(matches!(err, SocketPathError::Unsafe { .. }), "{err:?}");
        chmod(&outer, 0o755);
    }

    #[test]
    fn unix_socket_walk_lists_every_directory_up_to_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = plugin_dir(tmp.path());
        let walk = check_full(&dir.join("p.sock")).expect("sound");
        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        assert_eq!(
            walk.dirs.first().map(|d| d.path.clone()),
            Some(canonical.clone())
        );
        assert_eq!(
            walk.dirs.last().map(|d| d.path.clone()),
            Some(PathBuf::from("/"))
        );
        assert_eq!(walk.dirs.len(), canonical.ancestors().count());
        assert_eq!(walk.dirs[0].mode, 0o750);
        assert_eq!(walk.socket, canonical.join("p.sock"));
    }

    #[test]
    fn unix_socket_startup_walk_tolerates_missing_dir_but_connect_walk_does_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("not-yet").join("p.sock");
        let walk = check_existing(&socket).expect("missing dir is fine at startup");
        assert_eq!(
            walk.dirs[0].path,
            std::fs::canonicalize(tmp.path()).expect("canonical")
        );
        let err = check_full(&socket).expect_err("connect needs the directory");
        assert!(matches!(err, SocketPathError::MissingDir { .. }), "{err:?}");
    }

    #[test]
    fn unix_socket_symlinked_directory_is_walked_at_its_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let dir = plugin_dir(&real);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).expect("symlink");
        chmod(&real, 0o777);
        let err = check_full(&link.join("p.sock")).expect_err("target's ancestry is checked");
        assert_eq!(
            err.dir(),
            std::fs::canonicalize(&real).expect("canonical").as_path()
        );
        chmod(&real, 0o755);
    }

    #[test]
    fn unix_socket_ownership_rule_allows_root_daemon_and_socket_dir_owner_only() {
        let owners = AllowedOwners {
            daemon: 501,
            socket_dir: Some(777),
        };
        assert_eq!(violation(0, 0o755, owners), None);
        assert_eq!(violation(501, 0o700, owners), None);
        assert_eq!(violation(777, 0o750, owners), None);
        let reason = violation(1234, 0o755, owners).expect("foreign owner");
        assert!(reason.contains("uid 1234"), "{reason}");
        let startup = AllowedOwners {
            daemon: 501,
            socket_dir: None,
        };
        assert!(violation(777, 0o755, startup).is_some());
        // Root owning a world-writable directory (/tmp) is still refused:
        // anyone can substitute a socket there.
        assert!(violation(0, 0o1777, owners).is_some());
    }
}
