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
//! disappears as a whole by renaming its directory. Rotating to a material
//! file of a different name is three atomic steps — write the new file,
//! record it in `slot.json`, remove the old one — so a crash between them
//! leaves two material files of which `slot.json` names the current one,
//! and the next open removes the other. Slot files are opened with
//! `O_NOFOLLOW`, so a symlink planted in a slot is refused rather than
//! followed out of the pool.
//!
//! # Material documents
//!
//! A plain API key reaches the pool in either form the upstream
//! implementations accept: a JSON string (`"sk-..."`) or `{"api_key":
//! "sk-..."}`. The pool holds and stores only the object form
//! ([`canonical_document`]), and reads either form back from any JSON
//! material file, so a key the admin surface accepted always survives a
//! restart. A plain-key document whose key can be an `api_key` file's whole
//! content ([`bare_key`]) is stored there; any other document as JSON.
//!
//! Text that is not JSON is a bare key only when it is unambiguous: one
//! non-empty token with no whitespace that does not begin with `{`, `[` or
//! `"`. Text beginning with one of those must be a complete JSON document;
//! a truncated or half-written vendor file is an error, never a key.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Which material file a slot holds. Serialized (in `slot.json`) as its
/// file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaterialFile {
    /// `.credentials.json`, the Claude CLI's credential file.
    #[serde(rename = ".credentials.json")]
    ClaudeCredentials,
    /// `auth.json`, the Codex CLI's credential file.
    #[serde(rename = "auth.json")]
    CodexAuth,
    /// `api_key`, a bare key as text.
    #[serde(rename = "api_key")]
    ApiKey,
    /// `material.json`, any other JSON material document.
    #[serde(rename = "material.json")]
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

    /// The file a (canonical) document for `upstream` is stored in: a
    /// plain-key document whose key reads back unchanged from a bare key
    /// file as `api_key`; any other document under the vendor file name
    /// that identifies `upstream`, or `material.json` when none does.
    pub fn for_document(upstream: UpstreamKind, doc: &MaterialDocument) -> MaterialFile {
        if stored_api_key(doc).is_some_and(|key| bare_key(key) == Ok(key)) {
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
                let key = stored_api_key(doc)
                    .ok_or("an api_key file holds only an {\"api_key\": ...} document")?;
                if bare_key(key) != Ok(key) {
                    return Err("this key cannot be stored as a bare key file".into());
                }
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
pub fn stored_api_key(doc: &MaterialDocument) -> Option<&str> {
    let obj = doc.expose().as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get("api_key")?.as_str()
}

/// The plain-key document of `key`, without surrounding whitespace.
fn key_document(key: &str) -> MaterialDocument {
    MaterialDocument::new(serde_json::json!({ "api_key": key.trim() }))
}

/// The form the pool holds and stores a document in: a bare key given as a
/// JSON string, and a plain-key document, become `{"api_key": "<key>"}`
/// without surrounding whitespace; any other document is unchanged.
pub fn canonical_document(doc: MaterialDocument) -> MaterialDocument {
    match doc.expose() {
        Value::String(key) => key_document(key),
        _ => match stored_api_key(&doc) {
            Some(key) if key.trim() != key => key_document(key),
            _ => doc,
        },
    }
}

/// The text of a file or variable, without surrounding whitespace or a
/// leading byte-order mark.
fn document_text(bytes: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
    Ok(text.trim().trim_start_matches('\u{feff}').trim_start())
}

/// Whether text begins as a JSON document (an object, an array or a string)
/// would, and so is never taken for a bare key.
fn looks_like_json(text: &str) -> bool {
    text.starts_with(['{', '[', '"'])
}

/// `text` (already trimmed) as a bare key: one non-empty token, no
/// whitespace, not the start of a JSON document.
pub fn bare_key(text: &str) -> Result<&str, &'static str> {
    if text.is_empty() {
        Err("the API key is empty")
    } else if text.contains(char::is_whitespace) {
        Err("a plain API key file holds one key and nothing else")
    } else if looks_like_json(text) {
        Err("the text begins like a JSON document but is not a plain key; a JSON material file must be complete, well-formed JSON")
    } else {
        Ok(text)
    }
}

/// A bare key read from a file or the environment, as the plain-key
/// document `{"api_key": "<key>"}`. Surrounding whitespace (the trailing
/// newline of `echo key > file`) is not part of the key.
pub fn plain_key_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    let text = document_text(bytes).map_err(|e| format!("the API key is {e}"))?;
    Ok(key_document(bare_key(text)?))
}

/// A JSON material document read from a file: an object, or a bare key as
/// a JSON string (held as its plain-key document). The parser's message
/// names a position, never the content.
pub fn json_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    let text = document_text(bytes).map_err(|e| format!("the material is {e}"))?;
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    match value {
        Value::Object(_) => Ok(MaterialDocument::new(value)),
        Value::String(key) => Ok(key_document(&key)),
        _ => Err("a JSON material document must be an object or a key string".into()),
    }
}

/// A config `file` credential's contents: text that begins like JSON must
/// be a complete JSON document (a vendor credential file, or a key as a
/// JSON string); anything else must be one bare key. A file caught
/// half-written is an error, so it is never sent upstream as a key.
pub fn sniff_document(bytes: &[u8]) -> Result<MaterialDocument, String> {
    if document_text(bytes).is_ok_and(looks_like_json) {
        json_document(bytes)
    } else {
        plain_key_document(bytes)
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
    /// Selection tags; absent in a slot written before tags existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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
    /// Which material file is current. Absent in a slot written by an older
    /// pool or by hand; then the slot must hold exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_file: Option<MaterialFile>,
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
    /// Material files other than the current one: what an interrupted
    /// rotation left behind, to be removed ([`remove_leftovers`]).
    pub leftovers: Vec<MaterialFile>,
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
    let present: Vec<MaterialFile> = MaterialFile::ALL
        .into_iter()
        .filter(|f| fs::symlink_metadata(dir.join(f.file_name())).is_ok())
        .collect();
    let recorded = state
        .as_ref()
        .and_then(|s| s.material_file)
        .filter(|f| present.contains(f));
    let material_file = match (present.as_slice(), recorded) {
        ([one], _) => *one,
        ([], _) => {
            return Err(SlotError::Invalid(format!(
                "holds no material file (expected one of {})",
                MaterialFile::ALL.map(MaterialFile::file_name).join(", ")
            )))
        }
        // An interrupted rotation: `slot.json` names the current file.
        (_, Some(current)) => current,
        (many, None) => {
            return Err(SlotError::Invalid(format!(
                "holds more than one material file ({}); keep exactly one",
                many.iter()
                    .map(|f| f.file_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    };
    let leftovers = present
        .into_iter()
        .filter(|f| *f != material_file)
        .collect();
    let (bytes, stamp) = read_no_follow(&dir.join(material_file.file_name())).map_err(|e| {
        SlotError::Invalid(format!("cannot read {}: {e}", material_file.file_name()))
    })?;
    let doc = material_file
        .decode(&bytes)
        .map_err(|e| SlotError::Invalid(format!("{}: {e}", material_file.file_name())))?;
    Ok(SlotOnDisk {
        material_file,
        doc,
        stamp,
        state,
        leftovers,
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

/// Replace a slot's material and its `slot.json` (`state`, which must name
/// `new`), removing the previous material file when the new document is
/// stored under a different name. Each step is atomic and `slot.json` is
/// written between them, so a crash at any point leaves a slot that opens:
/// before `slot.json` names `new`, the old material is current and `new`
/// a leftover; after, the reverse.
pub fn replace_material(
    slot_dir: &Path,
    old: MaterialFile,
    new: MaterialFile,
    bytes: &[u8],
    state: &SlotState,
) -> io::Result<FileStamp> {
    debug_assert_eq!(state.material_file, Some(new));
    let stamp = write_atomic(slot_dir, new.file_name(), bytes, FILE_MODE)?;
    write_state(slot_dir, state)?;
    if old != new {
        remove_leftovers(slot_dir, &[old])?;
    }
    Ok(stamp)
}

/// Remove material files that are not the slot's current one.
pub fn remove_leftovers(slot_dir: &Path, leftovers: &[MaterialFile]) -> io::Result<()> {
    for file in leftovers {
        match fs::remove_file(slot_dir.join(file.file_name())) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    sync_dir(slot_dir)
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
        assert_eq!(stored_api_key(&extra), None);
    }

    #[test]
    fn plain_key_files_round_trip_and_refuse_junk() {
        let doc = plain_key_document(b"  sk-or-123\n").expect("key");
        assert_eq!(stored_api_key(&doc), Some("sk-or-123"));
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
            stored_api_key(&sniff_document(b"sk-x\n").expect("key")),
            Some("sk-x")
        );
    }

    #[test]
    fn text_that_begins_like_json_is_never_a_bare_key() {
        let key = |doc: Result<MaterialDocument, String>| {
            stored_api_key(&doc.expect("document")).map(str::to_owned)
        };
        // Truncated or malformed JSON, minified (no whitespace to give it
        // away) or not, is an error from every reader of bare keys.
        for junk in [
            &b"{\"tokens\":{\"access_token\":\"at\",\"refresh_token\":\"rt\""[..],
            b"{",
            b"[\"a\"",
            b"\"sk-unterminated",
            b"\xef\xbb\xbf{\"a\":",
        ] {
            assert!(sniff_document(junk).is_err(), "{junk:?}");
            assert!(plain_key_document(junk).is_err(), "{junk:?}");
        }
        // Complete JSON that is not an object or a key string is refused.
        assert!(sniff_document(b"[1]").is_err());
        // A key as a JSON string is a plain-key document, from any reader.
        assert_eq!(key(sniff_document(b"\"sk-q\"\n")).as_deref(), Some("sk-q"));
        assert_eq!(key(json_document(b" \" sk-q \" ")).as_deref(), Some("sk-q"));
        // A byte-order mark does not hide a vendor file.
        let doc = sniff_document(b"\xef\xbb\xbf{\"tokens\":{}}").expect("json");
        assert!(doc.expose().get("tokens").is_some());
        // Bare keys that are not JSON-shaped stay keys, digits included.
        assert_eq!(key(sniff_document(b"123456\n")).as_deref(), Some("123456"));
        assert_eq!(key(sniff_document(b"sk-a{b}")).as_deref(), Some("sk-a{b}"));
    }

    #[test]
    fn every_stored_document_reads_back_as_itself() {
        let docs = [
            serde_json::json!({"api_key": "sk-plain"}),
            serde_json::json!({"api_key": "with space"}),
            serde_json::json!({"api_key": "{looks-like-json"}),
            serde_json::json!({"api_key": "k", "org": "o"}),
            serde_json::json!({"claudeAiOauth": {"accessToken": "a"}}),
        ];
        for value in docs {
            let doc = canonical_document(MaterialDocument::new(value.clone()));
            for up in UpstreamKind::ALL {
                let file = MaterialFile::for_document(up, &doc);
                let bytes = file.encode(&doc).expect("encode");
                let back = file.decode(&bytes).expect("decode");
                assert_eq!(back.expose(), &value, "{file:?}");
            }
        }
        let canonical = canonical_document(MaterialDocument::new(serde_json::json!(" sk-s\n")));
        assert_eq!(canonical.expose(), &serde_json::json!({"api_key": "sk-s"}));
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
