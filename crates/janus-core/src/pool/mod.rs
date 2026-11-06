//! The credential pool: directory-per-slot storage of real upstream
//! credentials, kind detection, refresh, and the default chain. The only
//! component with read access to credential material. Codes against
//! [`crate::contract::credential`]. See `docs/scope` "Carries over" and
//! `docs/admin-surface` "Credentials".
//!
//! [`FilePool`] implements [`CredentialPool`] over two kinds of credential:
//!
//! - **Pool slots** — one directory per credential under
//!   `<data_dir>/credentials/` ([`pool_dir`]), named by its `ref`. A slot
//!   (mode `0700`) holds one material file — `.credentials.json`,
//!   `auth.json`, `api_key` (a bare key) or `material.json` — and
//!   `slot.json` (upstream, label, creation time, and the verdicts that
//!   must survive a restart: disabled, invalid, rate-limited until, last
//!   error), all mode `0600`. Use counters live in memory, so the request
//!   path never writes to disk. Slots are discovered at [`FilePool::open`]
//!   and added, rotated and removed through the admin surface, live.
//! - **Config credentials** — the `[[credential]]` entries of the daemon
//!   config ([`ConfiguredCredential`]), read from an environment variable or
//!   a file. Their existence and material belong to the config: they can be
//!   enabled, disabled and refreshed, not rotated or removed.
//!
//! Kind detection: a slot's upstream is what its `slot.json` records, or —
//! for a slot made by hand — what its material file's name says
//! (`.credentials.json` Anthropic, `auth.json` OpenAI/Codex, `api_key`
//! OpenRouter). Its kind
//! ([`CredentialKind`](crate::contract::credential::CredentialKind)) and expiry are whatever the
//! upstream implementation decodes from the material
//! ([`UpstreamImpl::decode_material`]); the pool never interprets material
//! itself.
//!
//! The default chain for an upstream is its `active` credentials in a fixed
//! order — config credentials in declaration order, then pool slots oldest
//! first (ties by `ref`) — and picks the first. "First usable", not
//! rotation, so `GET /v1/credentials/default` names the account traffic is
//! actually on. Disabled, invalid and rate-limited credentials are skipped;
//! a rate-limited one rejoins when its back-off ends.
//!
//! Refresh happens on use when the decoded `expires_at` is within
//! [`REFRESH_MARGIN`], and on demand ([`CredentialPool::refresh`]). It is
//! single-flight per credential: concurrent requests for an expiring
//! credential wait for one refresh and all use its result — success or
//! failure. A failed refresh is not retried on use for
//! [`REFRESH_FAILURE_BACKOFF`]: requests in that window fail at once with
//! the same reason instead of each calling the token endpoint again. An
//! operator's forced refresh, or new material (rotated, or changed on
//! disk), ends the back-off. A refreshed document is written back where it
//! came from (the slot, or a config `file`); a config `env` credential keeps
//! it in memory. A refresh that fails fails the request, and never moves it
//! to another credential.
//!
//! A request checks the credential's status again after waiting on a
//! refresh: one disabled, marked invalid, rate-limited or removed meanwhile
//! is not handed out. A named `ref` then fails; the default chain picks
//! again.
//!
//! Material documents: a plain key arrives as `"<key>"` or `{"api_key":
//! "<key>"}` and is held and stored as the latter; see the `slot` module for
//! which file each document is stored in and what text is taken for a bare
//! key (never a truncated JSON file).
//!
//! All filesystem work on the request and admin paths runs on the blocking
//! pool, never on the async executor. [`FilePool::open`] is synchronous and
//! runs before serving.
//!
//! Material never appears in `Debug` or `Display` output, in an error, or in
//! a log event: [`Secret`](crate::contract::credential::Secret) and
//! [`MaterialDocument`] redact themselves, and nothing here formats them.

mod slot;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::contract::credential::{
    validate_credential_ref, CredentialChoice, CredentialMaterial, CredentialMetadata,
    CredentialPool, CredentialSource, CredentialStatus, MaterialDocument, NewCredential, PoolError,
    UpstreamFeedback,
};
use crate::contract::selection::{sticky_score, validate_tag, Pick, Selection, Strategy};
use crate::contract::upstream::{DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind};

pub use slot::{pool_dir, POOL_DIR_NAME};
use slot::{FileStamp, MaterialFile, SlotState};

/// A credential whose access token expires within this long is refreshed
/// before use, so a request is not sent with a token that dies in flight.
pub const REFRESH_MARGIN: chrono::Duration = chrono::Duration::seconds(60);

/// How long a credential is held out of the default chain after the
/// upstream rate-limited it without saying until when.
pub const DEFAULT_RATE_LIMIT_BACKOFF: chrono::Duration = chrono::Duration::seconds(60);

/// How long a failed refresh is served to requests as their answer before
/// the token endpoint is asked again.
pub const REFRESH_FAILURE_BACKOFF: chrono::Duration = chrono::Duration::seconds(10);

/// A `[[credential]]` entry of the daemon config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredCredential {
    pub credential_ref: String,
    pub upstream: UpstreamKind,
    pub label: Option<String>,
    /// Selection tags, already validated.
    pub tags: Vec<String>,
    pub source: ConfiguredSource,
}

/// Where a config credential's material is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfiguredSource {
    /// An environment variable holding a bare API key.
    Env(String),
    /// A file holding a vendor CLI's credential file (a JSON object) or a
    /// bare API key.
    File(PathBuf),
}

/// What [`FilePool::open`] is given.
pub struct PoolOptions {
    /// The pool directory, normally [`pool_dir`] of the data directory.
    /// Created (mode `0700`) when absent.
    pub pool_dir: PathBuf,
    /// One implementation per upstream credentials may belong to.
    pub upstreams: Vec<Arc<dyn UpstreamImpl>>,
    /// The config's `[[credential]]` entries, in declaration order.
    pub configured: Vec<ConfiguredCredential>,
}

/// Why the pool could not be opened. The daemon refuses to start on any of
/// these rather than run with part of its pool silently missing.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("credential pool directory {path}: {source}")]
    PoolDir { path: PathBuf, source: io::Error },
    #[error("credential pool slot {path}: {reason}")]
    Slot { path: PathBuf, reason: String },
    #[error("configured credential {credential_ref:?}: {reason}")]
    Configured {
        credential_ref: String,
        reason: String,
    },
    #[error("credential {0:?} is both a config credential and a pool slot")]
    DuplicateRef(String),
    #[error("more than one upstream implementation for {0}")]
    DuplicateUpstream(UpstreamKind),
}

/// The credential pool over a pool directory and the config's credentials.
pub struct FilePool {
    dir: PathBuf,
    upstreams: HashMap<UpstreamKind, Arc<dyn UpstreamImpl>>,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    /// Serialises adds and removals, which change the set of slots.
    membership: tokio::sync::Mutex<()>,
    /// Serialises default-chain picks, and counts them: a pick reads every
    /// candidate's `last_pick` and stamps the chosen one, so two concurrent
    /// picks never both take the same least-recently-used credential.
    picks: Mutex<u64>,
}

impl fmt::Debug for FilePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = read_lock(&self.entries);
        f.debug_struct("FilePool")
            .field("dir", &self.dir)
            .field("credentials", &entries.values().collect::<Vec<_>>())
            .finish()
    }
}

/// Where one credential lives, and so how it is persisted.
enum Origin {
    Pool { dir: PathBuf },
    Env,
    File { path: PathBuf },
}

impl Origin {
    fn source(&self) -> CredentialSource {
        match self {
            Origin::Pool { .. } => CredentialSource::Pool,
            Origin::Env => CredentialSource::Env,
            Origin::File { .. } => CredentialSource::File,
        }
    }
}

/// The default-chain position: config credentials first, in declaration
/// order, then pool slots oldest first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ChainKey {
    Configured(usize),
    Pool(DateTime<Utc>, String),
}

struct Entry {
    credential_ref: String,
    upstream: Arc<dyn UpstreamImpl>,
    origin: Origin,
    chain: ChainKey,
    created_at: DateTime<Utc>,
    /// Held across every read-modify-write of the material or of
    /// `slot.json`, including the whole of a refresh: what makes refresh
    /// single-flight, and keeps disk and memory in step.
    io: tokio::sync::Mutex<()>,
    state: Mutex<EntryState>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.lock();
        f.debug_struct("Credential")
            .field("ref", &self.credential_ref)
            .field("upstream", &self.upstream.kind())
            .field("source", &self.origin.source())
            .field("kind", &st.material.decoded.kind)
            .field("status", &st.status(Utc::now()))
            .finish()
    }
}

/// The credential's material as last read or written.
struct Material {
    doc: MaterialDocument,
    decoded: DecodedMaterial,
    /// The slot file holding it; `None` off the pool.
    file: Option<MaterialFile>,
    /// What the backing file looked like when read; `None` for `env`.
    stamp: Option<FileStamp>,
}

struct EntryState {
    label: Option<String>,
    tags: Vec<String>,
    /// The pool-wide pick sequence number of the last time a default-chain
    /// pick chose this credential; `0` for never. What
    /// `least_recently_used` orders by.
    last_pick: u64,
    material: Material,
    disabled: bool,
    invalid: Option<String>,
    rate_limited_until: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    request_count: u64,
    error_count: u64,
    last_error: Option<String>,
    /// Deleted while someone still held the entry.
    removed: bool,
    /// The last refresh's failure, until a refresh succeeds or the material
    /// is replaced. Its back-off is what makes requests that waited on the
    /// failed refresh share its answer: they reach `io` within it.
    refresh_failure: Option<RefreshFailure>,
}

/// A failed refresh, shared with the requests that waited on it and served
/// to those that arrive before `retry_at`.
struct RefreshFailure {
    reason: String,
    retry_at: DateTime<Utc>,
}

impl EntryState {
    fn status(&self, now: DateTime<Utc>) -> CredentialStatus {
        if self.disabled {
            CredentialStatus::Disabled
        } else if self.invalid.is_some() {
            CredentialStatus::Invalid
        } else if self.rate_limited_until.is_some_and(|until| until > now) {
            CredentialStatus::RateLimited
        } else {
            CredentialStatus::Active
        }
    }

    fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.material
            .decoded
            .expires_at
            .is_some_and(|at| at - now <= REFRESH_MARGIN)
    }

    fn record_error(&mut self, message: String) {
        self.error_count += 1;
        self.last_error = Some(message);
    }
}

/// Tags as stored: sorted, without duplicates.
pub fn normalize_tags(mut tags: Vec<String>) -> Vec<String> {
    tags.sort();
    tags.dedup();
    tags
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// Run blocking filesystem work off the async executor.
async fn blocking<T: Send + 'static, E: From<io::Error> + Send + 'static>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| E::from(io::Error::other(e)))?
}

fn invalid_material(upstream: UpstreamKind, e: impl fmt::Display) -> PoolError {
    PoolError::InvalidMaterial {
        upstream,
        reason: e.to_string(),
    }
}

impl Entry {
    fn lock(&self) -> MutexGuard<'_, EntryState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kind(&self) -> UpstreamKind {
        self.upstream.kind()
    }

    fn metadata(&self) -> CredentialMetadata {
        let now = Utc::now();
        let st = self.lock();
        CredentialMetadata {
            credential_ref: self.credential_ref.clone(),
            upstream: self.kind(),
            kind: st.material.decoded.kind,
            label: st.label.clone(),
            source: self.origin.source(),
            status: st.status(now),
            rate_limited_until: st.rate_limited_until.filter(|until| *until > now),
            expires_at: st.material.decoded.expires_at,
            last_used_at: st.last_used_at,
            request_count: st.request_count,
            error_count: st.error_count,
            last_error: st.last_error.clone(),
            tags: st.tags.clone(),
        }
    }

    fn slot_state(&self, st: &EntryState) -> SlotState {
        SlotState {
            upstream: self.kind(),
            label: st.label.clone(),
            tags: st.tags.clone(),
            created_at: self.created_at,
            disabled: st.disabled,
            invalid: st.invalid.clone(),
            rate_limited_until: st.rate_limited_until,
            last_error: st.last_error.clone(),
            material_file: st.material.file,
        }
    }

    /// Write `slot.json` from the current state. Callers hold `io`, so
    /// writes are ordered and the last one carries the latest state.
    async fn persist_state(&self) -> Result<(), PoolError> {
        let Origin::Pool { dir } = &self.origin else {
            return Ok(());
        };
        let state = self.slot_state(&self.lock());
        let dir = dir.clone();
        blocking(move || slot::write_state(&dir, &state)).await?;
        Ok(())
    }

    /// The backing file, and whether a symlink at its final component is
    /// followed (a config `file` may be linked into place; a slot file may
    /// not); `None` for `env`.
    fn backing_file(&self) -> Option<(PathBuf, bool)> {
        match &self.origin {
            Origin::Env => None,
            Origin::File { path } => Some((path.clone(), true)),
            Origin::Pool { dir } => {
                let file = self.lock().material.file?;
                Some((dir.join(file.file_name()), false))
            }
        }
    }

    /// How the backing file looks on disk now; `None` for `env`.
    async fn disk_stamp(&self) -> io::Result<Option<FileStamp>> {
        let Some((path, follow)) = self.backing_file() else {
            return Ok(None);
        };
        blocking(move || {
            let meta = if follow {
                std::fs::metadata(&path)?
            } else {
                std::fs::symlink_metadata(&path)?
            };
            Ok(Some(FileStamp::of(&meta)))
        })
        .await
    }

    /// Reread the backing file if it changed since it was read: a config
    /// `file` rewritten by its vendor CLI, a slot edited by hand. Callers
    /// hold `io`.
    async fn reload_if_changed(&self) -> Result<(), PoolError> {
        let stamp = self.disk_stamp().await?;
        if self.lock().material.stamp == stamp {
            return Ok(());
        }
        let upstream = self.upstream.clone();
        let material = match &self.origin {
            Origin::Env => return Ok(()),
            Origin::File { path } => {
                let path = path.clone();
                blocking(move || read_file_source(upstream.as_ref(), &path)).await?
            }
            Origin::Pool { dir } => {
                let file = self.lock().material.file.unwrap_or(MaterialFile::Generic);
                let path = dir.join(file.file_name());
                blocking(move || read_slot_material(upstream.as_ref(), &path, file)).await?
            }
        };
        tracing::info!(credential = %self.credential_ref, "credential material changed on disk; reloaded");
        let mut st = self.lock();
        st.material = material;
        st.refresh_failure = None;
        Ok(())
    }

    /// The error a request gets for a refresh failure it did not run.
    fn shared_failure(&self, failure: &RefreshFailure) -> PoolError {
        PoolError::RefreshFailed {
            credential_ref: self.credential_ref.clone(),
            reason: format!(
                "{} (not retried before {})",
                failure.reason,
                failure.retry_at.to_rfc3339()
            ),
        }
    }

    /// Make the material usable: reload it if its file changed, and refresh
    /// it if it is about to expire (or unconditionally, when `force`).
    async fn make_usable(&self, force: bool) -> Result<(), PoolError> {
        // The request path: nothing to do, so take no lock.
        if !force {
            let on_disk = self.disk_stamp().await;
            let current = matches!(on_disk, Ok(s) if s == self.lock().material.stamp);
            if current && !self.lock().needs_refresh(Utc::now()) {
                return Ok(());
            }
        }
        let _io = self.io.lock().await;
        if self.lock().removed {
            return Err(PoolError::NotFound(self.credential_ref.clone()));
        }
        if let Err(e) = self.reload_if_changed().await {
            self.lock()
                .record_error(format!("cannot reload material: {e}"));
            return Err(e);
        }
        if !force {
            let now = Utc::now();
            let st = self.lock();
            // Waiting on `io` may have been waiting on another refresh.
            if !st.needs_refresh(now) {
                return Ok(());
            }
            // That refresh failed, or one failed recently: share its answer.
            if let Some(failure) = st.refresh_failure.as_ref().filter(|f| f.retry_at > now) {
                return Err(self.shared_failure(failure));
            }
        }
        self.refresh_locked().await
    }

    /// One refresh. Callers hold `io`.
    async fn refresh_locked(&self) -> Result<(), PoolError> {
        let doc = self.lock().material.doc.clone();
        let fail = |reason: String| {
            tracing::warn!(credential = %self.credential_ref, upstream = %self.kind(), %reason, "credential refresh failed");
            let mut st = self.lock();
            st.record_error(format!("refresh failed: {reason}"));
            st.refresh_failure = Some(RefreshFailure {
                reason: reason.clone(),
                retry_at: Utc::now() + REFRESH_FAILURE_BACKOFF,
            });
            PoolError::RefreshFailed {
                credential_ref: self.credential_ref.clone(),
                reason,
            }
        };
        let new_doc = match self.upstream.refresh(&doc).await {
            Ok(doc) => doc,
            // Asking to refresh an API key is the operator's mistake, not a
            // fault of the credential.
            Err(e @ UpstreamError::NotRefreshable(_)) => {
                return Err(PoolError::RefreshFailed {
                    credential_ref: self.credential_ref.clone(),
                    reason: e.to_string(),
                })
            }
            Err(e) => return Err(fail(e.to_string())),
        };
        let decoded = self
            .upstream
            .decode_material(&new_doc)
            .map_err(|e| fail(format!("refreshed material is unusable: {e}")))?;
        if decoded
            .expires_at
            .is_some_and(|at| at - Utc::now() <= REFRESH_MARGIN)
        {
            return Err(fail("the refreshed token is already expiring".into()));
        }

        let file = self.lock().material.file;
        // Memory first: the refreshed token is usable even if writing it
        // back fails, and the old refresh token may already be spent.
        {
            let mut st = self.lock();
            st.material.doc = new_doc.clone();
            st.material.decoded = decoded;
            st.refresh_failure = None;
        }
        let written = match &self.origin {
            Origin::Env => Ok(None),
            Origin::Pool { dir } => {
                let file = file.unwrap_or(MaterialFile::Generic);
                let dir = dir.clone();
                let doc = new_doc;
                blocking(move || {
                    let bytes = file.encode(&doc).map_err(io::Error::other)?;
                    slot::write_atomic(&dir, file.file_name(), &bytes, slot::FILE_MODE)
                })
                .await
                .map(Some)
            }
            Origin::File { path } => {
                let path = path.clone();
                let doc = new_doc;
                blocking(move || {
                    let bytes = MaterialFile::Generic
                        .encode(&doc)
                        .map_err(io::Error::other)?;
                    slot::rewrite_external(&path, &bytes)
                })
                .await
                .map(Some)
            }
        };
        match written {
            Ok(stamp) => {
                if stamp.is_some() {
                    self.lock().material.stamp = stamp;
                }
                tracing::info!(credential = %self.credential_ref, upstream = %self.kind(), "credential refreshed");
            }
            Err(e) => {
                tracing::error!(credential = %self.credential_ref, error = %e, "credential refreshed but writing it back failed; the refreshed material is held in memory only");
                self.lock()
                    .record_error(format!("refreshed material not written back: {e}"));
            }
        }
        Ok(())
    }
}

/// Decode a document read from disk into the credential's material.
fn decode(
    upstream: &dyn UpstreamImpl,
    doc: Result<MaterialDocument, String>,
    file: Option<MaterialFile>,
    stamp: FileStamp,
) -> Result<Material, PoolError> {
    let doc = doc.map_err(|e| invalid_material(upstream.kind(), e))?;
    let decoded = upstream
        .decode_material(&doc)
        .map_err(|e| invalid_material(upstream.kind(), e))?;
    Ok(Material {
        doc,
        decoded,
        file,
        stamp: Some(stamp),
    })
}

/// Decode a slot's material file.
fn read_slot_material(
    upstream: &dyn UpstreamImpl,
    path: &Path,
    file: MaterialFile,
) -> Result<Material, PoolError> {
    let (bytes, stamp) = slot::read_no_follow(path)?;
    decode(upstream, file.decode(&bytes), Some(file), stamp)
}

/// Decode a config `file` credential.
fn read_file_source(upstream: &dyn UpstreamImpl, path: &Path) -> Result<Material, PoolError> {
    let mut file = std::fs::File::open(path)?;
    let stamp = FileStamp::of(&file.metadata()?);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    decode(upstream, slot::sniff_document(&bytes), None, stamp)
}

impl FilePool {
    /// Open the pool: create the pool directory if needed, remove leftovers
    /// of interrupted writes, read every slot and every config credential.
    /// Any slot or config credential that cannot be read is an error.
    pub fn open(options: PoolOptions) -> Result<FilePool, OpenError> {
        let PoolOptions {
            pool_dir,
            upstreams: impls,
            configured,
        } = options;
        let mut upstreams: HashMap<UpstreamKind, Arc<dyn UpstreamImpl>> = HashMap::new();
        for imp in impls {
            let kind = imp.kind();
            if upstreams.insert(kind, imp).is_some() {
                return Err(OpenError::DuplicateUpstream(kind));
            }
        }
        let dir_err = |source| OpenError::PoolDir {
            path: pool_dir.clone(),
            source,
        };
        slot::ensure_private_dir(&pool_dir).map_err(dir_err)?;
        slot::sweep_scratch(&pool_dir).map_err(dir_err)?;

        let mut entries = BTreeMap::new();
        for (index, cfg) in configured.into_iter().enumerate() {
            if entries.contains_key(&cfg.credential_ref) {
                return Err(OpenError::Configured {
                    credential_ref: cfg.credential_ref,
                    reason: "declared more than once".into(),
                });
            }
            let entry = Self::open_configured(&upstreams, index, cfg)?;
            entries.insert(entry.credential_ref.clone(), Arc::new(entry));
        }
        let listing = std::fs::read_dir(&pool_dir).map_err(dir_err)?;
        for item in listing {
            let item = item.map_err(dir_err)?;
            let path = item.path();
            let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                return Err(OpenError::Slot {
                    path,
                    reason: "name is not UTF-8".into(),
                });
            };
            if name.starts_with('.') {
                continue;
            }
            let entry = Self::open_slot(&upstreams, &name, &path)?;
            if entries.contains_key(&name) {
                return Err(OpenError::DuplicateRef(name));
            }
            entries.insert(name, Arc::new(entry));
        }
        tracing::info!(dir = %pool_dir.display(), credentials = entries.len(), "credential pool opened");
        Ok(FilePool {
            dir: pool_dir,
            upstreams,
            entries: RwLock::new(entries),
            membership: tokio::sync::Mutex::new(()),
            picks: Mutex::new(0),
        })
    }

    fn open_configured(
        upstreams: &HashMap<UpstreamKind, Arc<dyn UpstreamImpl>>,
        index: usize,
        cfg: ConfiguredCredential,
    ) -> Result<Entry, OpenError> {
        let err = |reason: String| OpenError::Configured {
            credential_ref: cfg.credential_ref.clone(),
            reason,
        };
        validate_credential_ref(&cfg.credential_ref).map_err(|e| err(e.to_string()))?;
        let upstream = upstreams
            .get(&cfg.upstream)
            .cloned()
            .ok_or_else(|| err(format!("no implementation for upstream {}", cfg.upstream)))?;
        let (origin, material) = match &cfg.source {
            ConfiguredSource::Env(var) => {
                let value = std::env::var_os(var)
                    .ok_or_else(|| err(format!("environment variable {var} is not set")))?;
                let doc = slot::plain_key_document(value.as_encoded_bytes())
                    .map_err(|e| err(format!("environment variable {var}: {e}")))?;
                let decoded = upstream
                    .decode_material(&doc)
                    .map_err(|e| err(format!("environment variable {var}: {e}")))?;
                let material = Material {
                    doc,
                    decoded,
                    file: None,
                    stamp: None,
                };
                (Origin::Env, material)
            }
            ConfiguredSource::File(path) => {
                let material = read_file_source(upstream.as_ref(), path)
                    .map_err(|e| err(format!("{}: {e}", path.display())))?;
                (Origin::File { path: path.clone() }, material)
            }
        };
        Ok(Entry {
            credential_ref: cfg.credential_ref.clone(),
            upstream,
            origin,
            chain: ChainKey::Configured(index),
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            io: tokio::sync::Mutex::new(()),
            state: Mutex::new(EntryState::new(
                cfg.label.clone(),
                cfg.tags.clone(),
                material,
            )),
        })
    }

    fn open_slot(
        upstreams: &HashMap<UpstreamKind, Arc<dyn UpstreamImpl>>,
        name: &str,
        path: &Path,
    ) -> Result<Entry, OpenError> {
        let err = |reason: String| OpenError::Slot {
            path: path.to_path_buf(),
            reason,
        };
        validate_credential_ref(name).map_err(|e| err(format!("not a valid slot name: {e}")))?;
        let disk = slot::read_slot(path).map_err(|e| err(e.to_string()))?;
        if !disk.leftovers.is_empty() {
            tracing::warn!(credential = %name, current = disk.material_file.file_name(), "removing material left by an interrupted rotation");
            slot::remove_leftovers(path, &disk.leftovers).map_err(|e| err(e.to_string()))?;
        }
        slot::make_private(path, disk.material_file).map_err(|e| err(e.to_string()))?;
        let upstream_kind = match &disk.state {
            Some(state) => state.upstream,
            None => disk.material_file.detected_upstream().ok_or_else(|| {
                err(format!(
                    "{} does not identify an upstream and the slot has no {}",
                    disk.material_file.file_name(),
                    slot::SLOT_STATE_FILE
                ))
            })?,
        };
        let upstream = upstreams
            .get(&upstream_kind)
            .cloned()
            .ok_or_else(|| err(format!("no implementation for upstream {upstream_kind}")))?;
        let decoded = upstream
            .decode_material(&disk.doc)
            .map_err(|e| err(format!("{}: {e}", disk.material_file.file_name())))?;
        let material = Material {
            doc: disk.doc,
            decoded,
            file: Some(disk.material_file),
            stamp: Some(disk.stamp),
        };
        let state = match disk.state {
            Some(state) => state,
            None => {
                // A hand-made slot: record what was detected, and fix its
                // place in the default chain at the directory's age.
                let state = SlotState {
                    upstream: upstream_kind,
                    label: None,
                    tags: Vec::new(),
                    created_at: disk.dir_modified,
                    disabled: false,
                    invalid: None,
                    rate_limited_until: None,
                    last_error: None,
                    material_file: Some(disk.material_file),
                };
                slot::write_state(path, &state).map_err(|e| err(e.to_string()))?;
                tracing::info!(credential = %name, upstream = %upstream_kind, "adopted hand-made credential pool slot");
                state
            }
        };
        for tag in &state.tags {
            validate_tag(tag).map_err(|e| err(format!("{}: {e}", slot::SLOT_STATE_FILE)))?;
        }
        let mut entry_state = EntryState::new(state.label, state.tags, material);
        entry_state.disabled = state.disabled;
        entry_state.invalid = state.invalid;
        entry_state.rate_limited_until = state.rate_limited_until;
        entry_state.last_error = state.last_error;
        Ok(Entry {
            credential_ref: name.to_owned(),
            upstream,
            origin: Origin::Pool {
                dir: path.to_path_buf(),
            },
            chain: ChainKey::Pool(state.created_at, name.to_owned()),
            created_at: state.created_at,
            io: tokio::sync::Mutex::new(()),
            state: Mutex::new(entry_state),
        })
    }

    fn upstream(&self, kind: UpstreamKind) -> Result<&Arc<dyn UpstreamImpl>, PoolError> {
        self.upstreams
            .get(&kind)
            .ok_or_else(|| invalid_material(kind, "no implementation for this upstream"))
    }

    fn get(&self, credential_ref: &str) -> Result<Arc<Entry>, PoolError> {
        validate_credential_ref(credential_ref)?;
        read_lock(&self.entries)
            .get(credential_ref)
            .cloned()
            .ok_or_else(|| PoolError::NotFound(credential_ref.to_owned()))
    }

    fn pool_slot(&self, credential_ref: &str) -> Result<(Arc<Entry>, PathBuf), PoolError> {
        let entry = self.get(credential_ref)?;
        match &entry.origin {
            Origin::Pool { dir } => {
                let dir = dir.clone();
                Ok((entry, dir))
            }
            other => Err(PoolError::NotPoolManaged {
                credential_ref: credential_ref.to_owned(),
                origin: other.source(),
            }),
        }
    }

    /// Every credential for `upstream` (or all), in default-chain order.
    fn ordered(&self, upstream: Option<UpstreamKind>) -> Vec<Arc<Entry>> {
        let mut list: Vec<Arc<Entry>> = read_lock(&self.entries)
            .values()
            .filter(|e| upstream.is_none_or(|u| e.kind() == u))
            .cloned()
            .collect();
        list.sort_by(|a, b| a.chain.cmp(&b.chain));
        list
    }

    /// The credentials `selection` may pick now: active, admitted by its
    /// filter, not in `exclude`, in chain order.
    fn candidates(&self, selection: &Selection, exclude: &[String]) -> Vec<Arc<Entry>> {
        let now = Utc::now();
        self.ordered(Some(selection.implementation()))
            .into_iter()
            .filter(|e| !exclude.contains(&e.credential_ref))
            .filter(|e| {
                let st = e.lock();
                st.status(now) == CredentialStatus::Active
                    && selection.filter().admits(
                        &e.credential_ref,
                        st.material.decoded.kind,
                        &st.tags,
                    )
            })
            .collect()
    }

    /// The default chain's pick for `selection` right now. `commit` makes
    /// it a pick: the round-robin cursor advances and the chosen credential
    /// is stamped for `least_recently_used`; a peek does neither.
    fn default_entry(
        &self,
        selection: &Selection,
        pick: Pick<'_>,
        commit: bool,
    ) -> Result<Arc<Entry>, PoolError> {
        let mut picks = self.picks.lock().unwrap_or_else(|e| e.into_inner());
        let mut candidates = self.candidates(selection, pick.exclude);
        if candidates.is_empty() {
            return Err(PoolError::NoDefault(selection.instance().to_owned()));
        }
        let index = match (selection.strategy(), pick.identity) {
            (Strategy::Ordered, _) | (Strategy::Sticky, None) => 0,
            (Strategy::RoundRobin, _) => {
                let position = if commit {
                    selection.advance()
                } else {
                    selection.position()
                };
                // `len` is at most the number of credentials: no overflow.
                (position % candidates.len() as u64) as usize
            }
            (Strategy::LeastRecentlyUsed, _) => candidates
                .iter()
                .enumerate()
                .min_by_key(|(i, e)| (e.lock().last_pick, *i))
                .map_or(0, |(i, _)| i),
            (Strategy::Sticky, Some(identity)) => candidates
                .iter()
                .enumerate()
                .max_by(|(i, a), (j, b)| {
                    sticky_score(identity, &a.credential_ref)
                        .cmp(&sticky_score(identity, &b.credential_ref))
                        // Equal scores cannot happen for distinct refs; if
                        // they did, the earlier in chain order wins.
                        .then(j.cmp(i))
                })
                .map_or(0, |(i, _)| i),
        };
        let chosen = candidates.swap_remove(index);
        if commit {
            *picks += 1;
            chosen.lock().last_pick = *picks;
        }
        Ok(chosen)
    }

    /// Apply `change` to the state, persist it, and return the metadata.
    async fn mutate(
        &self,
        credential_ref: &str,
        change: impl FnOnce(&mut EntryState),
    ) -> Result<CredentialMetadata, PoolError> {
        let entry = self.get(credential_ref)?;
        let _io = entry.io.lock().await;
        if entry.lock().removed {
            return Err(PoolError::NotFound(credential_ref.to_owned()));
        }
        change(&mut entry.lock());
        entry.persist_state().await?;
        Ok(entry.metadata())
    }

    /// A ref no credential has. A directory made by hand since open may
    /// still hold the name; [`slot::create_slot`] refuses it and `add`
    /// picks again.
    fn generate_ref(&self, upstream: UpstreamKind) -> String {
        let entries = read_lock(&self.entries);
        loop {
            let candidate = format!("{upstream}-{:08x}", rand::random::<u32>());
            if !entries.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

impl EntryState {
    fn new(label: Option<String>, tags: Vec<String>, material: Material) -> EntryState {
        EntryState {
            label,
            tags,
            last_pick: 0,
            material,
            disabled: false,
            invalid: None,
            rate_limited_until: None,
            last_used_at: None,
            request_count: 0,
            error_count: 0,
            last_error: None,
            removed: false,
            refresh_failure: None,
        }
    }
}

#[async_trait]
impl CredentialPool for FilePool {
    async fn resolve(
        &self,
        selection: &Selection,
        choice: &CredentialChoice,
        pick: Pick<'_>,
    ) -> Result<CredentialMaterial, PoolError> {
        let upstream = selection.implementation();
        let unavailable = |r: &str, status| PoolError::Unavailable {
            credential_ref: r.to_owned(),
            status,
        };
        loop {
            let entry = match choice {
                CredentialChoice::Ref(r) => {
                    let entry = self.get(r)?;
                    if entry.kind() != upstream {
                        return Err(PoolError::WrongUpstream {
                            credential_ref: r.clone(),
                            expected: upstream,
                            actual: entry.kind(),
                        });
                    }
                    let st = entry.lock();
                    if !selection
                        .filter()
                        .admits(r, st.material.decoded.kind, &st.tags)
                    {
                        return Err(PoolError::NotEligible {
                            credential_ref: r.clone(),
                            instance: selection.instance().to_owned(),
                        });
                    }
                    let status = st.status(Utc::now());
                    if status != CredentialStatus::Active {
                        return Err(unavailable(r, status));
                    }
                    drop(st);
                    entry
                }
                CredentialChoice::Default => self.default_entry(selection, pick, true)?,
            };
            let usable = entry.make_usable(false).await;
            let default = matches!(choice, CredentialChoice::Default);
            match usable {
                // Removed while this request waited: the chain picks again.
                Err(PoolError::NotFound(_)) if default => continue,
                other => other?,
            }
            // Waiting on a refresh, the credential may have been removed,
            // disabled, or marked invalid or rate-limited.
            let mut st = entry.lock();
            let status = st.status(Utc::now());
            if st.removed || status != CredentialStatus::Active {
                if default {
                    continue;
                }
                return Err(if st.removed {
                    PoolError::NotFound(entry.credential_ref.clone())
                } else {
                    unavailable(&entry.credential_ref, status)
                });
            }
            st.request_count += 1;
            st.last_used_at = Some(Utc::now());
            let decoded = &st.material.decoded;
            return Ok(CredentialMaterial {
                credential_ref: entry.credential_ref.clone(),
                upstream,
                kind: decoded.kind,
                secret: decoded.secret.clone(),
                account_id: decoded.account_id.clone(),
            });
        }
    }

    async fn report(
        &self,
        credential_ref: &str,
        feedback: &UpstreamFeedback,
    ) -> Result<(), PoolError> {
        let now = Utc::now();
        let meta = self
            .mutate(credential_ref, |st| match feedback {
                UpstreamFeedback::CredentialInvalid { reason } => {
                    st.invalid = Some(reason.clone());
                    st.record_error(format!("rejected by the upstream: {reason}"));
                }
                UpstreamFeedback::RateLimited { until } => {
                    let until = until.unwrap_or(now + DEFAULT_RATE_LIMIT_BACKOFF);
                    st.rate_limited_until = Some(until);
                    st.record_error(format!("rate limited until {}", until.to_rfc3339()));
                }
                UpstreamFeedback::AccountError { reason } => {
                    st.record_error(format!("account error: {reason}"));
                }
            })
            .await?;
        tracing::warn!(credential = %credential_ref, upstream = %meta.upstream, status = ?meta.status, last_error = meta.last_error.as_deref().unwrap_or(""), "upstream reported a credential problem");
        Ok(())
    }

    async fn list(
        &self,
        upstream: Option<UpstreamKind>,
    ) -> Result<Vec<CredentialMetadata>, PoolError> {
        Ok(self
            .ordered(upstream)
            .iter()
            .map(|e| e.metadata())
            .collect())
    }

    async fn show(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError> {
        Ok(self.get(credential_ref)?.metadata())
    }

    async fn peek_default(
        &self,
        selection: &Selection,
        identity: Option<&str>,
    ) -> Result<CredentialMetadata, PoolError> {
        let pick = Pick {
            identity,
            exclude: &[],
        };
        Ok(self.default_entry(selection, pick, false)?.metadata())
    }

    async fn set_tags(
        &self,
        credential_ref: &str,
        tags: Vec<String>,
    ) -> Result<CredentialMetadata, PoolError> {
        for tag in &tags {
            validate_tag(tag)?;
        }
        let tags = normalize_tags(tags);
        self.pool_slot(credential_ref)?;
        let meta = self.mutate(credential_ref, |st| st.tags = tags).await?;
        tracing::info!(credential = %credential_ref, tags = ?meta.tags, "credential tags set");
        Ok(meta)
    }

    async fn add(&self, new: NewCredential) -> Result<CredentialMetadata, PoolError> {
        let NewCredential {
            upstream: kind,
            credential_ref,
            label,
            tags,
            material: doc,
        } = new;
        let doc = slot::canonical_document(doc);
        if let Some(r) = &credential_ref {
            validate_credential_ref(r)?;
        }
        for tag in &tags {
            validate_tag(tag)?;
        }
        let tags = normalize_tags(tags);
        let upstream = self.upstream(kind)?.clone();
        let decoded = upstream
            .decode_material(&doc)
            .map_err(|e| invalid_material(kind, e))?;
        let file = MaterialFile::for_document(kind, &doc);
        let bytes = file.encode(&doc).map_err(|e| invalid_material(kind, e))?;

        let _membership = self.membership.lock().await;
        let created_at = Utc::now();
        let state = SlotState {
            upstream: kind,
            label: label.clone(),
            tags: tags.clone(),
            created_at,
            disabled: false,
            invalid: None,
            rate_limited_until: None,
            last_error: None,
            material_file: Some(file),
        };
        let (credential_ref, stamp) = loop {
            let r = match &credential_ref {
                Some(r) => r.clone(),
                None => self.generate_ref(kind),
            };
            if read_lock(&self.entries).contains_key(&r) {
                return Err(PoolError::AlreadyExists(r));
            }
            let (pool, name, bytes, state) =
                (self.dir.clone(), r.clone(), bytes.clone(), state.clone());
            match blocking(move || slot::create_slot(&pool, &name, file, &bytes, &state)).await {
                Ok(stamp) => break (r, stamp),
                // A generated name taken by a directory made by hand.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && credential_ref.is_none() => {
                    continue
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(PoolError::AlreadyExists(r))
                }
                Err(e) => return Err(PoolError::Io(e)),
            }
        };
        let material = Material {
            doc,
            decoded,
            file: Some(file),
            stamp: Some(stamp),
        };
        let entry = Arc::new(Entry {
            credential_ref: credential_ref.clone(),
            upstream,
            origin: Origin::Pool {
                dir: self.dir.join(&credential_ref),
            },
            chain: ChainKey::Pool(created_at, credential_ref.clone()),
            created_at,
            io: tokio::sync::Mutex::new(()),
            state: Mutex::new(EntryState::new(label, tags, material)),
        });
        write_lock(&self.entries).insert(credential_ref.clone(), entry.clone());
        tracing::info!(credential = %credential_ref, upstream = %kind, "credential added to the pool");
        Ok(entry.metadata())
    }

    async fn replace_material(
        &self,
        credential_ref: &str,
        doc: MaterialDocument,
    ) -> Result<CredentialMetadata, PoolError> {
        let (entry, dir) = self.pool_slot(credential_ref)?;
        let doc = slot::canonical_document(doc);
        let kind = entry.kind();
        let decoded = entry
            .upstream
            .decode_material(&doc)
            .map_err(|e| invalid_material(kind, e))?;
        let new_file = MaterialFile::for_document(kind, &doc);
        let bytes = new_file
            .encode(&doc)
            .map_err(|e| invalid_material(kind, e))?;

        let _io = entry.io.lock().await;
        if entry.lock().removed {
            return Err(PoolError::NotFound(credential_ref.to_owned()));
        }
        let (old_file, state) = {
            let st = entry.lock();
            let mut state = entry.slot_state(&st);
            state.invalid = None;
            state.material_file = Some(new_file);
            (st.material.file.unwrap_or(new_file), state)
        };
        let stamp =
            blocking(move || slot::replace_material(&dir, old_file, new_file, &bytes, &state))
                .await?;
        {
            let mut st = entry.lock();
            st.material = Material {
                doc,
                decoded,
                file: Some(new_file),
                stamp: Some(stamp),
            };
            st.invalid = None;
            st.refresh_failure = None;
        }
        tracing::info!(credential = %credential_ref, upstream = %kind, "credential material replaced");
        Ok(entry.metadata())
    }

    async fn enable(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError> {
        let meta = self
            .mutate(credential_ref, |st| st.disabled = false)
            .await?;
        tracing::info!(credential = %credential_ref, "credential enabled");
        Ok(meta)
    }

    async fn disable(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError> {
        let meta = self.mutate(credential_ref, |st| st.disabled = true).await?;
        tracing::info!(credential = %credential_ref, "credential disabled");
        Ok(meta)
    }

    async fn refresh(&self, credential_ref: &str) -> Result<CredentialMetadata, PoolError> {
        let entry = self.get(credential_ref)?;
        let result = entry.make_usable(true).await;
        // A failed refresh is recorded on the credential; persist that too.
        let _io = entry.io.lock().await;
        if !entry.lock().removed {
            entry.persist_state().await?;
        }
        result?;
        Ok(entry.metadata())
    }

    async fn delete(&self, credential_ref: &str) -> Result<(), PoolError> {
        let (entry, _dir) = self.pool_slot(credential_ref)?;
        let _membership = self.membership.lock().await;
        let _io = entry.io.lock().await;
        if entry.lock().removed {
            return Err(PoolError::NotFound(credential_ref.to_owned()));
        }
        let pool = self.dir.clone();
        let r = credential_ref.to_owned();
        blocking(move || slot::remove_slot(&pool, &r)).await?;
        entry.lock().removed = true;
        write_lock(&self.entries).remove(credential_ref);
        tracing::info!(credential = %credential_ref, "credential removed from the pool");
        Ok(())
    }
}
