//! The on-disk layout of the pool directory.
//!
//! ```text
//! <data_dir>/credentials/            0700
//!     <ref>/                         0700, one directory per slot
//!         slot.json                  0600, metadata and verdicts (never material)
//!         .credentials.json          0600, | exactly one material file:
//!         auth.json                  0600, | the vendor CLI's own file, a
//!         api_key                    0600, | plain key, or a generic JSON
//!         material.json              0600, | document
//! ```
//!
//! A slot directory name is its `ref`, so every name that reaches the
//! filesystem has passed [`validate_credential_ref`] first: no `/`, no
//! leading `.`. Names starting with `.` are the pool's own scratch space
//! (in-progress writes, slots being removed) and are never slots.
//!
//! Every write is atomic: a temporary file in the same directory, `fsync`,
//! `rename` over the target, `fsync` of the directory. A slot appears or
//! disappears as a whole by renaming its directory. Slot files are opened
//! with `O_NOFOLLOW`, so a symlink planted in a slot is refused rather than
//! followed out of the pool.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::contract::credential::{validate_credential_ref, MaterialDocument};
use crate::contract::upstream::UpstreamKind;

/// The pool directory's name under the data directory.
pub const POOL_DIR_NAME: &str = "credentials";
/// Per-slot metadata: upstream, label, operator and upstream verdicts.
pub const SLOT_STATE_FILE: &str = "slot.json";
/// A bare API key, as text.
pub const API_KEY_FILE: &str = "api_key";
/// A JSON material document with no vendor file name of its own.
pub const GENERIC_MATERIAL_FILE: &str = "material.json";

/// Mode of the pool directory and of every slot directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode of every file in a slot.
pub const FILE_MODE: u32 = 0o600;

/// Vendor credential files the pool recognises by name, and the upstream a
/// slot holding one belongs to when its `slot.json` does not say
/// (`docs/scope#extractable`: `.credentials.json` for Anthropic, `auth.json`
/// for OpenAI/Codex, a plain API key for OpenRouter).
const DETECTED_UPSTREAMS: [(MaterialFile, UpstreamKind); 3] = [
    (MaterialFile::ClaudeCredentials, UpstreamKind::Anthropic),
    (MaterialFile::CodexAuth, UpstreamKind::OpenAi),
    (MaterialFile::ApiKey, UpstreamKind::OpenRouter),
];

/// `<data_dir>/credentials`.
pub fn pool_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(POOL_DIR_NAME)
}

/// Which material file a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialFile {
    /// `.credentials.json`, the Claude CLI's credential file.
    ClaudeCredentials,
    /// `auth.json`, the Codex CLI's credential file.
    CodexAuth,
    /// `api_key`, a bare key as text.
    ApiKey,
    /// `material.json`, any other JSON material document.
    Generic,
}

impl MaterialFile {
    const ALL: [MaterialFile; 4] = [
        MaterialFile::ClaudeCredentials,
        MaterialFile::CodexAuth,
        MaterialFile::ApiKey,
        MaterialFile::Generic,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            MaterialFile::ClaudeCredentials => ".credentials.json",
            MaterialFile::CodexAuth => "auth.json",
            MaterialFile::ApiKey => API_KEY_FILE,
            MaterialFile::Generic => GENERIC_MATERIAL_FILE,
        }
    }

    /// The upstream a slot holding this file belongs to, when nothing else
    /// says.
    pub fn detected_upstream(self) -> Option<UpstreamKind> {
        DETECTED_UPSTREAMS
            .iter()
            .find(|(file, _)| *file == self)
            .map(|(_, upstream)| *upstream)
    }

    /// The file a document for `upstream` is stored in: a plain-key document
    /// as `api_key`; any other document under the vendor file name that
    /// identifies `upstream`, or `material.json` when none does.
    pub fn for_document(upstream: UpstreamKind, doc: &MaterialDocument) -> MaterialFile {
        if plain_api_key(doc).is_some() {
            return MaterialFile::ApiKey;
        }
        DETECTED_UPSTREAMS
            .iter()
            .find(|(file, up)| *up == upstream && *file != MaterialFile::ApiKey)
            .map_or(MaterialFile::Generic, |(file, _)| *file)
    }

    /// The document stored in a file of this kind.
    pub fn decode(self, bytes: &[u8]) -> Result<MaterialDocument, String> {
        match self {
            MaterialFile::ApiKey => plain_key_document(bytes),
            _ => json_document(bytes),
        }
    }

    /// The bytes a document of this kind is stored as.
    pub fn encode(self, doc: &MaterialDocument) -> Result<Vec<u8>, String> {
        match self {
            MaterialFile::ApiKey => {
                let key = plain_api_key(doc)
                    .ok_or("an api_key file holds only an {\"api_key\": ...} document")?;
                let mut bytes = key.as_bytes().to_vec();
                bytes.push(b'\n');
                Ok(bytes)
            }
            _ => {
                let mut bytes = serde_json::to_vec_pretty(doc.expose())
                    .map_err(|e| format!("cannot serialize material: {e}"))?;
                bytes.push(b'\n');
                Ok(bytes)
            }
        }
    }
}

/// The key of a plain-key document: exactly `{"api_key": "<key>"}`.
pub fn plain_api_key(doc: &MaterialDocument) -> Option<&str> {
    let obj = doc.expose().as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get("api_key")?.as_str()
}

/// A bare key read from a file or the environment, as the plain-key
/// document `{"api_key": "<key>"}`. Surrounding whitespace (the trailing
/// newline of `echo key > file`) is not part of the key.
pub fn plain_key_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the API key is not UTF-8".to_owned())?;
    let key = text.trim();
    if key.is_empty() {
        return Err("the API key is empty".into());
    }
    if key.contains(char::is_whitespace) {
        return Err("a plain API key file holds one key and nothing else".into());
    }
    Ok(MaterialDocument::new(serde_json::json!({ "api_key": key })))
}

/// A JSON material document read from a file. The parser's message names a
/// position, never the content.
pub fn json_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("not valid JSON: {e}"))?;
    if !value.is_object() {
        return Err("a JSON material document must be an object".into());
    }
    Ok(MaterialDocument::new(value))
}

/// A config `file` credential's contents: a JSON object is a vendor
/// credential file, anything else a bare key.
pub fn sniff_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) if value.is_object() => Ok(MaterialDocument::new(value)),
        _ => plain_key_document(bytes),
    }
}

/// A slot's `slot.json`. Holds what must survive a restart — the operator's
/// and the upstream's verdicts — and never any material. Use counters
/// (`request_count`, `error_count`, `last_used_at`) are per daemon run and
/// live only in memory, so the request path never writes to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotState {
    pub upstream: UpstreamKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Orders the slot in the default chain: oldest first.
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub disabled: bool,
    /// The upstream's rejection reason while the credential is `invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Identity of a material file's contents on disk, to notice a file that
/// changed underneath the pool (a vendor CLI rewriting its own `auth.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileStamp {
    pub fn of(meta: &fs::Metadata) -> FileStamp {
        FileStamp {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// A slot as read from disk.
pub struct SlotOnDisk {
    pub material_file: MaterialFile,
    pub doc: MaterialDocument,
    pub stamp: FileStamp,
    /// `None` when the slot was made by hand and has no `slot.json` yet.
    pub state: Option<SlotState>,
    /// When the slot directory was last modified: the creation time given to
    /// a hand-made slot.
    pub dir_modified: DateTime<Utc>,
}

/// Why a slot directory could not be read.
#[derive(Debug, thiserror::Error)]
pub enum SlotError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Invalid(String),
}

/// Read a regular file without following a symlink at its final component.
pub fn read_no_follow(path: &Path) -> io::Result<(Vec<u8>, FileStamp)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok((bytes, FileStamp::of(&meta)))
}

/// The stamp of a slot file, without following a symlink.
pub fn stamp_no_follow(path: &Path) -> io::Result<FileStamp> {
    Ok(FileStamp::of(&fs::symlink_metadata(path)?))
}

/// Read one slot directory.
pub fn read_slot(dir: &Path) -> Result<SlotOnDisk, SlotError> {
    let dir_meta = fs::symlink_metadata(dir)?;
    if !dir_meta.is_dir() {
        return Err(SlotError::Invalid(
            "is not a directory (symlinks are not followed)".into(),
        ));
    }
    let present: Vec<MaterialFile> = MaterialFile::ALL
        .into_iter()
        .filter(|f| fs::symlink_metadata(dir.join(f.file_name())).is_ok())
        .collect();
    let material_file = match present.as_slice() {
        [one] => *one,
        [] => {
            return Err(SlotError::Invalid(format!(
                "holds no material file (expected one of {})",
                MaterialFile::ALL.map(MaterialFile::file_name).join(", ")
            )))
        }
        many => {
            return Err(SlotError::Invalid(format!(
                "holds more than one material file ({}); keep exactly one",
                many.iter()
                    .map(|f| f.file_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    };
    let (bytes, stamp) = read_no_follow(&dir.join(material_file.file_name())).map_err(|e| {
        SlotError::Invalid(format!("cannot read {}: {e}", material_file.file_name()))
    })?;
    let doc = material_file
        .decode(&bytes)
        .map_err(|e| SlotError::Invalid(format!("{}: {e}", material_file.file_name())))?;
    let state = match read_no_follow(&dir.join(SLOT_STATE_FILE)) {
        Ok((bytes, _)) => Some(
            serde_json::from_slice::<SlotState>(&bytes)
                .map_err(|e| SlotError::Invalid(format!("{SLOT_STATE_FILE}: {e}")))?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(SlotError::Invalid(format!(
                "cannot read {SLOT_STATE_FILE}: {e}"
            )))
        }
    };
    Ok(SlotOnDisk {
        material_file,
        doc,
        stamp,
        state,
        dir_modified: dir_meta
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| DateTime::<Utc>::from(SystemTime::UNIX_EPOCH)),
    })
}

/// Create the pool directory if needed and hold it to [`DIR_MODE`].
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            if meta.permissions().mode() & 0o777 != DIR_MODE {
                tracing::warn!(path = %dir.display(), mode = format_args!("{:o}", meta.permissions().mode() & 0o777), "credential pool directory was not private; setting mode 0700");
                fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a directory (symlinks are not followed)",
                dir.display()
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(dir)?;
            // `recursive` applies the mode before the umask; set it exactly.
            fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
        }
        Err(e) => Err(e),
    }
}

/// Hold a slot read by [`read_slot`] — typically one made by hand — to the
/// pool's modes: the directory [`DIR_MODE`], its files [`FILE_MODE`].
pub fn make_private(dir: &Path, material_file: MaterialFile) -> io::Result<()> {
    let files = [material_file.file_name(), SLOT_STATE_FILE];
    let targets = std::iter::once((dir.to_path_buf(), DIR_MODE))
        .chain(files.iter().map(|f| (dir.join(f), FILE_MODE)));
    for (path, mode) in targets {
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symlink", path.display()),
            ));
        }
        if meta.permissions().mode() & 0o777 != mode {
            tracing::warn!(path = %path.display(), mode = format_args!("{:o}", meta.permissions().mode() & 0o777), "credential pool slot was not private; tightening its mode");
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

/// A name in a pool directory that no slot can have, for scratch work.
fn scratch_name(purpose: &str, base: &str) -> String {
    format!(".{purpose}-{base}-{:016x}", rand::random::<u64>())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Replace `dir/name` with `bytes` atomically, the new file having `mode`.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> io::Result<FileStamp> {
    let tmp = dir.join(scratch_name("write", name));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        // `mode` passes through the umask; set it exactly.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        let target = dir.join(name);
        fs::rename(&tmp, &target)?;
        sync_dir(dir)?;
        stamp_no_follow(&target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Rewrite a config `file` credential in place (after a refresh), keeping
/// the file's own mode. The path may be a symlink (a vendor CLI's file
/// linked into place); the file it resolves to is the one replaced.
pub fn rewrite_external(path: &Path, bytes: &[u8]) -> io::Result<FileStamp> {
    let target = fs::canonicalize(path)?;
    let mode = fs::metadata(&target)?.permissions().mode() & 0o7777;
    let dir = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "credential file has no parent")
    })?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "credential file name"))?;
    write_atomic(dir, name, bytes, mode)?;
    Ok(FileStamp::of(&fs::metadata(path)?))
}

/// Write `state` as the slot's `slot.json`.
pub fn write_state(slot_dir: &Path, state: &SlotState) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(slot_dir, SLOT_STATE_FILE, &bytes, FILE_MODE).map(|_| ())
}

/// Write a whole new slot under `pool`, appearing at once as `pool/<ref>`.
/// Fails with `AlreadyExists` when that name is taken.
pub fn create_slot(
    pool: &Path,
    credential_ref: &str,
    material_file: MaterialFile,
    material: &[u8],
    state: &SlotState,
) -> io::Result<FileStamp> {
    validate_credential_ref(credential_ref)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let target = pool.join(credential_ref);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let tmp = pool.join(scratch_name("add", credential_ref));
    let result = (|| {
        fs::DirBuilder::new().mode(DIR_MODE).create(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(DIR_MODE))?;
        write_atomic(&tmp, material_file.file_name(), material, FILE_MODE)?;
        write_state(&tmp, state)?;
        // `rename` onto an existing empty directory would succeed, so the
        // existence check above is repeated as close to the rename as the
        // filesystem allows; the pool serialises adds, so only an outside
        // writer could race this.
        if fs::symlink_metadata(&target).is_ok() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        fs::rename(&tmp, &target)?;
        sync_dir(pool)?;
        stamp_no_follow(&target.join(material_file.file_name()))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

/// Replace a slot's material, removing the previous material file when the
/// new document is stored under a different name.
pub fn replace_material(
    slot_dir: &Path,
    old: MaterialFile,
    new: MaterialFile,
    bytes: &[u8],
) -> io::Result<FileStamp> {
    let stamp = write_atomic(slot_dir, new.file_name(), bytes, FILE_MODE)?;
    if old != new {
        fs::remove_file(slot_dir.join(old.file_name()))?;
        sync_dir(slot_dir)?;
    }
    Ok(stamp)
}

/// Remove a slot: it disappears at once (renamed to a scratch name), then
/// its contents are deleted.
pub fn remove_slot(pool: &Path, credential_ref: &str) -> io::Result<()> {
    validate_credential_ref(credential_ref)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let doomed = pool.join(scratch_name("delete", credential_ref));
    fs::rename(pool.join(credential_ref), &doomed)?;
    sync_dir(pool)?;
    fs::remove_dir_all(&doomed)
}

/// Remove the leftovers of interrupted writes and removals: every
/// scratch-named entry of the pool directory and of its slots.
pub fn sweep_scratch(pool: &Path) -> io::Result<()> {
    for entry in fs::read_dir(pool)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let file_type = entry.file_type()?;
        if is_scratch(name) {
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        } else if file_type.is_dir() {
            for inner in fs::read_dir(entry.path())? {
                let inner = inner?;
                if inner.file_name().to_str().is_some_and(is_scratch) {
                    fs::remove_file(inner.path())?;
                }
            }
        }
    }
    Ok(())
}

fn is_scratch(name: &str) -> bool {
    ["write", "add", "delete"]
        .iter()
        .any(|p| name.starts_with(&format!(".{p}-")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_file_choice_follows_document_and_upstream() {
        let key = MaterialDocument::new(serde_json::json!({"api_key": "k"}));
        let oauth = MaterialDocument::new(serde_json::json!({"claudeAiOauth": {}}));
        for up in UpstreamKind::ALL {
            assert_eq!(MaterialFile::for_document(up, &key), MaterialFile::ApiKey);
        }
        assert_eq!(
            MaterialFile::for_document(UpstreamKind::Anthropic, &oauth),
            MaterialFile::ClaudeCredentials
        );
        assert_eq!(
            MaterialFile::for_document(UpstreamKind::OpenAi, &oauth),
            MaterialFile::CodexAuth
        );
        assert_eq!(
            MaterialFile::for_document(UpstreamKind::OpenRouter, &oauth),
            MaterialFile::Generic
        );
        // A document with more than the key is not a plain key.
        let extra = MaterialDocument::new(serde_json::json!({"api_key": "k", "org": "o"}));
        assert_eq!(plain_api_key(&extra), None);
    }

    #[test]
    fn plain_key_files_round_trip_and_refuse_junk() {
        let doc = plain_key_document(b"  sk-or-123\n").expect("key");
        assert_eq!(plain_api_key(&doc), Some("sk-or-123"));
        let bytes = MaterialFile::ApiKey.encode(&doc).expect("encode");
        assert_eq!(bytes, b"sk-or-123\n");
        assert!(plain_key_document(b"\n \n").is_err());
        assert!(plain_key_document(b"two keys").is_err());
        assert!(plain_key_document(&[0xff, 0xfe]).is_err());
        assert!(json_document(b"[1]").is_err());
        assert!(sniff_document(b"{\"tokens\":{}}")
            .expect("json")
            .expose()
            .get("tokens")
            .is_some());
        assert_eq!(
            plain_api_key(&sniff_document(b"sk-x\n").expect("key")),
            Some("sk-x")
        );
    }

    #[test]
    fn atomic_write_replaces_with_exact_mode_and_leaves_no_scratch() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_atomic(dir.path(), "f", b"one", 0o600).expect("first");
        write_atomic(dir.path(), "f", b"two", 0o600).expect("second");
        assert_eq!(fs::read(dir.path().join("f")).expect("read"), b"two");
        let mode = fs::metadata(dir.path().join("f"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("ls")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f")]);
    }
}
