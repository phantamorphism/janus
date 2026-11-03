//! The admin token: `<data_dir>/admin-token`, mode `0600`, generated on
//! first start and presented as `Authorization: Bearer <token>` on the TCP
//! admin and UI listeners (`docs/architecture#admin-surface`).
//!
//! It is a separate secret with a separate check from caller identities:
//! nothing here consults the opaque-identity registry, and the registry
//! never sees this token. Only the token's SHA-256 is kept in memory, and a
//! presented token is hashed and compared to it in constant time, so the
//! comparison takes the same time whatever the presented value's length or
//! content.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::AdminError;

/// Random bytes in a generated token (base64url, 43 characters).
const TOKEN_BYTES: usize = 32;
/// The shortest token accepted from an existing `admin-token` file.
pub const MIN_TOKEN_LEN: usize = 32;

/// The loaded admin token, as its hash.
pub(crate) struct AdminToken {
    hash: [u8; 32],
}

impl AdminToken {
    /// Read `path`, or generate it (mode `0600`) when absent.
    pub(crate) fn load_or_create(path: &Path) -> Result<Self, AdminError> {
        match open_existing(path) {
            Ok(file) => return Self::read(path, file),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(token_error(path, format!("cannot open: {e}"))),
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
        let mut file = match created {
            Ok(file) => file,
            // Another process created it between the two opens: use theirs.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let file = open_existing(path)
                    .map_err(|e| token_error(path, format!("cannot open: {e}")))?;
                return Self::read(path, file);
            }
            Err(e) => return Err(token_error(path, format!("cannot create: {e}"))),
        };
        file.write_all(format!("{token}\n").as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| token_error(path, format!("cannot write: {e}")))?;
        tracing::info!(path = %path.display(), "generated the admin token");
        Ok(AdminToken::of(token.as_bytes()))
    }

    /// What [`AdminToken::load_or_create`] would refuse, without creating
    /// anything: an existing file is read and checked; an absent one passes.
    pub(crate) fn check(path: &Path) -> Result<(), AdminError> {
        match open_existing(path) {
            Ok(file) => Self::read(path, file).map(drop),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(token_error(path, format!("cannot open: {e}"))),
        }
    }

    fn read(path: &Path, mut file: File) -> Result<Self, AdminError> {
        let meta = file
            .metadata()
            .map_err(|e| token_error(path, format!("cannot stat: {e}")))?;
        if !meta.is_file() {
            return Err(token_error(path, "is not a regular file"));
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(token_error(
                path,
                format!(
                    "is owned by uid {}, not the daemon's uid {euid}",
                    meta.uid()
                ),
            ));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(token_error(
                path,
                format!("has mode {mode:04o}; it must not be readable or writable by group or others (0600)"),
            ));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| token_error(path, format!("cannot read: {e}")))?;
        let token = text.trim();
        if token.chars().any(char::is_whitespace) {
            return Err(token_error(path, "holds more than one word"));
        }
        if token.len() < MIN_TOKEN_LEN {
            return Err(token_error(
                path,
                format!("holds a token shorter than {MIN_TOKEN_LEN} characters"),
            ));
        }
        Ok(AdminToken::of(token.as_bytes()))
    }

    fn of(token: &[u8]) -> Self {
        AdminToken {
            hash: Sha256::digest(token).into(),
        }
    }

    /// Whether `presented` is the admin token, in constant time.
    pub(crate) fn verify(&self, presented: &[u8]) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented).into();
        presented.ct_eq(&self.hash).into()
    }
}

fn open_existing(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

fn token_error(path: &Path, reason: impl Into<String>) -> AdminError {
    AdminError::Token {
        path: PathBuf::from(path),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn generated_token_is_private_reused_and_verified_in_constant_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin-token");
        let first = AdminToken::load_or_create(&path).expect("generate");
        let meta = std::fs::metadata(&path).expect("stat");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).expect("read");
        let token = text.trim();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(token).expect("b64").len(),
            TOKEN_BYTES
        );
        assert!(first.verify(token.as_bytes()));
        assert!(!first.verify(b"wrong"));
        assert!(!first.verify(format!("{token}x").as_bytes()));
        assert!(!first.verify(b""));

        let again = AdminToken::load_or_create(&path).expect("reuse");
        assert!(again.verify(token.as_bytes()));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), text);
    }

    #[test]
    fn exposed_weak_or_linked_token_files_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin-token");
        std::fs::write(&path, "x".repeat(MIN_TOKEN_LEN)).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = AdminToken::load_or_create(&path).err().expect("refused");
        assert!(err.to_string().contains("0644"), "{err}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(AdminToken::load_or_create(&path).is_ok());

        std::fs::write(&path, "short\n").expect("write");
        let err = AdminToken::load_or_create(&path).err().expect("refused");
        assert!(err.to_string().contains("shorter"), "{err}");

        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "y".repeat(40)).expect("write");
        let link = dir.path().join("linked-token");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(
            AdminToken::load_or_create(&link).is_err(),
            "symlink followed"
        );
    }
}
