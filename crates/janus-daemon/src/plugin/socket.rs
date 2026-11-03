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
//! The path is resolved one component at a time and every directory a
//! name is looked up in is checked — the directories a connection actually
//! traverses, including each one that holds a symlink on the way (whoever
//! can write there can repoint the link). After connecting, the uid of the
//! process serving the socket is held to the same owners, which closes the
//! window between the walk and the connect.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use janus_core::contract::plugin::{PluginError, PluginFailureClass};
use serde::Serialize;
use tokio::net::UnixStream;

use super::transport::within;

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
    /// Owner of the socket's directory, once it exists: who may serve it.
    #[serde(skip)]
    socket_dir_owner: Option<u32>,
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

/// Symlinks followed while resolving one path before it is refused, as
/// the kernel's own limit (`MAXSYMLINKS`).
const MAX_SYMLINKS: usize = 40;

/// One step still to take while resolving a path.
enum Step {
    Root,
    Parent,
    Name(OsString),
}

fn steps(path: &Path) -> impl DoubleEndedIterator<Item = Step> + '_ {
    path.components().filter_map(|c| match c {
        Component::RootDir | Component::Prefix(_) => Some(Step::Root),
        Component::ParentDir => Some(Step::Parent),
        Component::Normal(name) => Some(Step::Name(name.to_owned())),
        Component::CurDir => None,
    })
}

/// Where resolution ended.
enum Outcome {
    /// The socket's resolved path; its directory exists and was looked in
    /// (the socket file itself may not exist yet).
    Socket(PathBuf),
    /// A directory on the way does not exist yet; this is the deepest one
    /// that does.
    Missing(PathBuf),
}

/// Every directory a lookup of the path read, in the order first read,
/// and where the lookup ended.
struct Resolution {
    looked_in: Vec<WalkedDir>,
    outcome: Outcome,
}

fn record(looked_in: &mut Vec<WalkedDir>, dir: &Path) -> Result<(), SocketPathError> {
    if looked_in.iter().any(|d| d.path == dir) {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| io_error(dir, e))?;
    looked_in.push(WalkedDir {
        path: dir.to_path_buf(),
        uid: meta.uid(),
        mode: meta.mode() & 0o7777,
    });
    Ok(())
}

/// Resolve `socket` one component at a time, the way the kernel does,
/// recording every directory a name is looked up in — including the
/// directory holding each symlink, which `canonicalize` would hide: whoever
/// can write there can repoint the link.
fn resolve(socket: &Path) -> Result<Resolution, SocketPathError> {
    let absolute = if socket.is_absolute() {
        socket.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| io_error(socket, e))?
            .join(socket)
    };
    let mut pending: VecDeque<Step> = steps(&absolute).collect();
    let mut cur = PathBuf::from("/");
    let mut looked_in = Vec::new();
    let mut links = 0;
    while let Some(step) = pending.pop_front() {
        let name = match step {
            Step::Root => {
                cur = PathBuf::from("/");
                continue;
            }
            Step::Parent => {
                cur.pop();
                continue;
            }
            Step::Name(name) => name,
        };
        record(&mut looked_in, &cur)?;
        let next = cur.join(&name);
        let last = pending.is_empty();
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let outcome = if last {
                    Outcome::Socket(next)
                } else {
                    Outcome::Missing(cur)
                };
                return Ok(Resolution { looked_in, outcome });
            }
            Err(e) => return Err(io_error(&next, e)),
        };
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_SYMLINKS {
                return Err(SocketPathError::Io {
                    dir: next,
                    message: format!("more than {MAX_SYMLINKS} symlinks on the path"),
                });
            }
            let target = std::fs::read_link(&next).map_err(|e| io_error(&next, e))?;
            for step in steps(&target).rev() {
                pending.push_front(step);
            }
        } else if last {
            return Ok(Resolution {
                looked_in,
                outcome: Outcome::Socket(next),
            });
        } else if meta.is_dir() {
            cur = next;
        } else {
            return Err(SocketPathError::Io {
                dir: next,
                message: "is not a directory".to_owned(),
            });
        }
    }
    Err(SocketPathError::Io {
        dir: socket.to_path_buf(),
        message: "socket path has no file name".to_owned(),
    })
}

/// Apply the rule to every directory looked in: `deepest` (the socket's own
/// directory, or the deepest that exists yet) and its ancestors first,
/// deepest first, then any other directory a symlink on the path led
/// through.
fn walk(
    looked_in: Vec<WalkedDir>,
    deepest: &Path,
    owners: AllowedOwners,
) -> Result<Vec<WalkedDir>, SocketPathError> {
    let chain: Vec<&Path> = deepest.ancestors().collect();
    let rank = |d: &WalkedDir| {
        chain
            .iter()
            .position(|a| *a == d.path)
            .unwrap_or(chain.len())
    };
    let mut ordered = looked_in;
    // Stable: directories off the chain keep their traversal order.
    ordered.sort_by_key(rank);
    for dir in &ordered {
        if let Some(reason) = violation(dir.uid, dir.mode, owners) {
            return Err(SocketPathError::Unsafe {
                dir: dir.path.clone(),
                reason,
            });
        }
    }
    Ok(ordered)
}

fn socket_dir(socket: &Path) -> &Path {
    socket.parent().unwrap_or(Path::new("/"))
}

/// The walk over a resolution that reached the socket's directory.
fn check_resolved(
    looked_in: Vec<WalkedDir>,
    resolved: PathBuf,
) -> Result<PathWalk, SocketPathError> {
    let real_dir = socket_dir(&resolved).to_path_buf();
    let owner = looked_in
        .iter()
        .find(|d| d.path == real_dir)
        .map(|d| d.uid)
        .ok_or_else(|| SocketPathError::Io {
            dir: real_dir.clone(),
            message: "the socket's directory was not examined".to_owned(),
        })?;
    let owners = AllowedOwners {
        daemon: daemon_uid(),
        socket_dir: Some(owner),
    };
    let dirs = walk(looked_in, &real_dir, owners)?;
    Ok(PathWalk {
        socket: resolved,
        dirs,
        socket_dir_owner: Some(owner),
    })
}

/// The per-connect check: the socket's directory must exist and every
/// directory the path's resolution reads must pass the walk. Returns the
/// resolved socket path, which is what the connection should use, and
/// [`PathWalk::check_peer`] should then be run on the connected socket.
pub fn check_full(socket: &Path) -> Result<PathWalk, SocketPathError> {
    let resolution = resolve(socket)?;
    match resolution.outcome {
        Outcome::Socket(resolved) => check_resolved(resolution.looked_in, resolved),
        Outcome::Missing(_) => Err(SocketPathError::MissingDir {
            dir: socket_dir(socket).to_path_buf(),
        }),
    }
}

/// The startup check: the walk over the part of the path that exists.
/// When the socket's directory exists this is the full check; when it does
/// not, its eventual owner is unknown, so every directory must be owned by
/// root or the daemon's user.
pub fn check_existing(socket: &Path) -> Result<PathWalk, SocketPathError> {
    let resolution = resolve(socket)?;
    match resolution.outcome {
        Outcome::Socket(resolved) => check_resolved(resolution.looked_in, resolved),
        Outcome::Missing(deepest) => {
            let owners = AllowedOwners {
                daemon: daemon_uid(),
                socket_dir: None,
            };
            let dirs = walk(resolution.looked_in, &deepest, owners)?;
            Ok(PathWalk {
                socket: socket.to_path_buf(),
                dirs,
                socket_dir_owner: None,
            })
        }
    }
}

/// Whether the process serving the socket may: the same owners as the
/// directories. A socket can be swapped between the walk and the connect;
/// the peer's credentials are what the connection actually reached.
fn peer_violation(uid: u32, owners: AllowedOwners) -> Option<String> {
    (!owners.allows(uid)).then(|| format!("is served by uid {uid}, which is not {owners}"))
}

impl PathWalk {
    /// Check the uid of the process at the other end of a connection made
    /// to [`PathWalk::socket`].
    pub fn check_peer(&self, uid: u32) -> Result<(), SocketPathError> {
        let owners = AllowedOwners {
            daemon: daemon_uid(),
            socket_dir: self.socket_dir_owner,
        };
        match peer_violation(uid, owners) {
            None => Ok(()),
            Some(reason) => Err(SocketPathError::Unsafe {
                dir: self.socket.clone(),
                reason,
            }),
        }
    }

    /// Connect to the resolved socket, bounded by `CONNECT_TIMEOUT`, and
    /// check who answered. Fails as `plugin_unreachable`.
    pub(crate) async fn connect(&self) -> Result<UnixStream, PluginError> {
        let unreachable =
            |message: String| PluginError::new(PluginFailureClass::Unreachable, message);
        let socket = self.socket.display();
        let connect = async {
            UnixStream::connect(&self.socket)
                .await
                .map_err(|e| unreachable(format!("cannot connect to plugin socket {socket}: {e}")))
        };
        let stream = within(
            PluginFailureClass::Unreachable,
            format!("connecting to plugin socket {socket}"),
            connect,
        )
        .await?;
        let cred = stream.peer_cred().map_err(|e| {
            unreachable(format!(
                "cannot read the peer credentials of plugin socket {socket}: {e}"
            ))
        })?;
        self.check_peer(cred.uid())
            .map_err(|e| unreachable(e.to_string()))?;
        Ok(stream)
    }
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

    /// `<root>/open`, mode 0777: a directory anyone can write.
    fn open_dir(root: &Path) -> PathBuf {
        let open = root.join("open");
        std::fs::create_dir(&open).expect("mkdir");
        chmod(&open, 0o777);
        open
    }

    #[test]
    fn unix_socket_symlink_in_a_writable_directory_is_refused() {
        // The configured socket is a symlink in a world-writable directory
        // to a socket in a sound one: whoever can write the first directory
        // chose the target, so the link's directory is what is refused.
        let tmp = tempfile::tempdir().expect("tempdir");
        let target_dir = plugin_dir(tmp.path());
        let open = open_dir(tmp.path());
        let link = open.join("p.sock");
        std::os::unix::fs::symlink(target_dir.join("evil.sock"), &link).expect("symlink");
        let err = check_full(&link).expect_err("link lives in a writable directory");
        let canonical_open = std::fs::canonicalize(&open).expect("canonical");
        assert_eq!(err.dir(), canonical_open.as_path(), "{err}");
        assert!(
            err.to_string().contains("writable by group and others"),
            "{err}"
        );
        assert!(check_existing(&link).is_err(), "startup refuses it too");

        // The same with the symlink as a directory component.
        let dir_link = open.join("plugin");
        std::os::unix::fs::symlink(&target_dir, &dir_link).expect("symlink");
        let err = check_full(&dir_link.join("p.sock")).expect_err("component link");
        assert_eq!(err.dir(), canonical_open.as_path(), "{err}");
        let err = check_existing(&dir_link.join("later").join("p.sock"))
            .expect_err("startup walk follows the link too");
        assert_eq!(err.dir(), canonical_open.as_path(), "{err}");
        chmod(&open, 0o755);
    }

    #[test]
    fn unix_socket_symlink_in_a_sound_directory_resolves_to_its_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target_dir = plugin_dir(tmp.path());
        let links = plugin_dir(&tmp.path().join("plugin"));
        let link = links.join("p.sock");
        // Relative, through `..`: resolved against the link's directory.
        std::os::unix::fs::symlink("../real.sock", &link).expect("symlink");
        let walk = check_full(&link).expect("every directory is sound");
        let canonical = std::fs::canonicalize(&target_dir).expect("canonical");
        assert_eq!(walk.socket, canonical.join("real.sock"));
        let looked_in: Vec<_> = walk.dirs.iter().map(|d| d.path.clone()).collect();
        assert_eq!(looked_in[0], canonical, "the socket's own directory first");
        assert!(
            looked_in.contains(&std::fs::canonicalize(&links).expect("canonical")),
            "the link's directory was walked: {looked_in:?}"
        );
    }

    #[test]
    fn unix_socket_symlink_loop_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = plugin_dir(tmp.path());
        std::os::unix::fs::symlink(dir.join("b"), dir.join("a")).expect("symlink");
        std::os::unix::fs::symlink(dir.join("a"), dir.join("b")).expect("symlink");
        let err = check_full(&dir.join("a")).expect_err("loop");
        assert!(err.to_string().contains("symlinks"), "{err}");
    }

    #[tokio::test]
    async fn unix_socket_peer_is_checked_after_connect() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = plugin_dir(tmp.path());
        let path = dir.join("p.sock");
        let _listener = tokio::net::UnixListener::bind(&path).expect("bind");
        let walk = check_full(&path).expect("sound");
        walk.connect().await.expect("served by our own uid");
        drop(_listener);
        let err = walk.connect().await.expect_err("nothing listening");
        assert_eq!(err.class, PluginFailureClass::Unreachable);
        let foreign = daemon_uid().wrapping_add(4242);
        let err = walk.check_peer(foreign).expect_err("foreign peer");
        assert!(err.to_string().contains(&format!("uid {foreign}")), "{err}");
        assert_eq!(err.dir(), walk.socket.as_path());
    }

    #[test]
    fn unix_socket_peer_rule_allows_root_daemon_and_socket_dir_owner_only() {
        let owners = AllowedOwners {
            daemon: 501,
            socket_dir: Some(777),
        };
        assert_eq!(peer_violation(0, owners), None);
        assert_eq!(peer_violation(501, owners), None);
        assert_eq!(peer_violation(777, owners), None);
        let reason = peer_violation(1234, owners).expect("foreign peer");
        assert!(reason.contains("uid 1234"), "{reason}");
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
