//! The daemon's SQLite database, `<data_dir>/janus.db`: the opaque-identity
//! registry (`identities`) and the request log (`requests`) with retention.
//! See `docs/admin-surface#storage` and `docs/caller-identity#registry`.
//!
//! [`Store::open`] creates or migrates the file (WAL mode, migrations in
//! [`db`]), loads the registry into memory and starts the request-log
//! writer. What the other daemon modules call:
//!
//! - the data plane: [`Store::registry`] as the `opaque` extractor's
//!   [`OpaqueRegistry`](janus_core::contract::registry::OpaqueRegistry), and
//!   [`RequestLog::submit`] once a response has been relayed;
//! - the admin surface: [`Registry`] create / get / list / revoke / delete
//!   for `/v1/identities`, [`RequestLog::query`] / [`RequestLog::get`] /
//!   [`RequestLog::subscribe`] for `/v1/requests`, and
//!   [`RequestLog::stats`] for `/v1/status`;
//! - the lifecycle: [`Store::spawn_jobs`] for the periodic `last_seen_at`
//!   flush and retention, and its cancellation for the final flush.
//!
//! The store's methods are synchronous and short (one SQLite statement or
//! an in-memory probe), except `submit`/`flush`, which only touch the
//! writer's queue.

mod db;
mod registry;
mod request_log;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use janus_core::contract::paths;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::contract::config::RequestLogConfig;

pub use registry::{CreatedIdentity, Expiry, IdentityFilter, NewIdentity, Registry};
pub use request_log::{
    RequestFilter, RequestLog, RequestLogStats, RequestPage, RequestQuery, DEFAULT_LIMIT,
    MAX_LIMIT, OUTCOMES,
};

/// How often in-memory `last_seen_at` values are written to the table:
/// the bound on how stale the stored value can be.
pub const LAST_SEEN_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// How often the retention job runs at most; it runs more often when
/// `request_log.retain` is shorter than this.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema: {0}")]
    Schema(String),
    /// A row that does not decode; the file was edited or damaged.
    #[error("corrupt database row: {0}")]
    Corrupt(String),
    /// Bad input from the caller of the store (the admin surface maps it to
    /// `400`).
    #[error("{0}")]
    Invalid(String),
    #[error("no identity with id {0:?}")]
    NotFound(String),
}

/// Intervals of the background jobs; [`Default`] is the production setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobIntervals {
    pub last_seen_flush: Duration,
    pub retention: Duration,
}

impl JobIntervals {
    /// The production intervals for a given retention.
    pub fn for_retention(retain: Duration) -> Self {
        JobIntervals {
            last_seen_flush: LAST_SEEN_FLUSH_INTERVAL,
            retention: retain.clamp(Duration::from_secs(1), RETENTION_INTERVAL),
        }
    }
}

/// `janus.db`, opened.
#[derive(Debug, Clone)]
pub struct Store {
    registry: Arc<Registry>,
    requests: Arc<RequestLog>,
}

impl Store {
    /// Open `<data_dir>/janus.db`, creating it if absent. `data_dir` must
    /// exist.
    pub fn open(data_dir: &Path, log: &RequestLogConfig) -> Result<Self, StoreError> {
        let path = paths::database_path(data_dir);
        let registry_conn = db::open(&path)?;
        let registry = Registry::load(registry_conn)?;
        let requests = RequestLog::start(
            log.enabled,
            log.retain,
            db::connect(&path)?,
            db::connect(&path)?,
        )?;
        Ok(Store {
            registry: Arc::new(registry),
            requests: Arc::new(requests),
        })
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn requests(&self) -> &Arc<RequestLog> {
        &self.requests
    }

    /// Run the `last_seen_at` flush and the retention job until `cancel`
    /// fires, then flush `last_seen_at` and the request-log queue one last
    /// time. Retention runs once immediately, so rows that aged out while
    /// the daemon was down go at startup.
    pub fn spawn_jobs(&self, intervals: JobIntervals, cancel: CancellationToken) -> JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            let mut flush = tokio::time::interval(intervals.last_seen_flush);
            let mut retention = tokio::time::interval(intervals.retention);
            flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = flush.tick() => store.flush_last_seen().await,
                    _ = retention.tick() => {
                        let log = Arc::clone(&store.requests);
                        match tokio::task::spawn_blocking(move || log.prune(Utc::now())).await {
                            Ok(Ok(0)) => {}
                            Ok(Ok(n)) => tracing::info!(deleted = n, "request-log retention"),
                            Ok(Err(e)) => tracing::error!(error = %e, "request-log retention failed"),
                            Err(e) => tracing::error!(error = %e, "request-log retention task failed"),
                        }
                    }
                }
            }
            store.flush_last_seen().await;
            store.requests.flush().await;
        })
    }

    async fn flush_last_seen(&self) {
        let registry = Arc::clone(&self.registry);
        match tokio::task::spawn_blocking(move || registry.flush_last_seen()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "last_seen_at flush failed"),
            Err(e) => tracing::error!(error = %e, "last_seen_at flush task failed"),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use chrono::{DateTime, Utc};

    use super::Store;

    pub fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("valid timestamp")
    }

    pub fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = reopen(&dir);
        (dir, store)
    }

    pub fn reopen(dir: &tempfile::TempDir) -> Store {
        Store::open(dir.path(), &Default::default()).expect("open store")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use janus_core::contract::binding::Method;
    use janus_core::contract::registry::{OpaqueRegistry, TokenHash};
    use janus_core::contract::request_log::{Outcome, RequestRecord};
    use rusqlite::Connection;

    use super::test_support::{at, open_store};
    use super::*;

    fn rec(id: &str, received_at: chrono::DateTime<Utc>) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            received_at,
            duration_ms: 1,
            upstream: None,
            implementation: None,
            attempts: Vec::new(),
            route: None,
            method: "GET".into(),
            path: "/nowhere".into(),
            binding: Some(Method::None),
            caller_identity: String::new(),
            claimed_identity: None,
            correlation_id: None,
            outcome: Outcome::Forwarded,
            credential_ref: None,
            status: Some(204),
            usage: None,
            error: None,
        }
    }

    fn raw(dir: &tempfile::TempDir) -> Connection {
        Connection::open(dir.path().join(paths::DATABASE_FILE)).expect("raw open")
    }

    fn last_seen_on_disk(dir: &tempfile::TempDir, id: &str) -> Option<String> {
        raw(dir)
            .query_row(
                "SELECT last_seen_at FROM identities WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .expect("row")
    }

    fn request_ids_on_disk(dir: &tempfile::TempDir) -> Vec<String> {
        raw(dir)
            .prepare("SELECT request_id FROM requests ORDER BY seq")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn jobs_flush_last_seen_and_apply_retention_periodically() {
        let (dir, store) = open_store();
        let created = store
            .registry()
            .create(
                NewIdentity {
                    identity: "sandbox-7".into(),
                    ..Default::default()
                },
                Utc::now(),
            )
            .expect("create");
        let old = Utc::now() - chrono::Duration::days(40);
        store.requests().submit(rec("stale", old)).await;
        store.requests().submit(rec("fresh", Utc::now())).await;
        store.requests().flush().await;

        let cancel = CancellationToken::new();
        let jobs = store.spawn_jobs(
            JobIntervals {
                last_seen_flush: Duration::from_millis(50),
                retention: Duration::from_millis(50),
            },
            cancel.clone(),
        );
        eventually("retention", || request_ids_on_disk(&dir) == ["fresh"]).await;
        assert_eq!(store.requests().stats().rows, 1);

        // A lookup after the jobs started reaches the file without a write
        // of its own, on the next periodic flush.
        let seen = Utc::now();
        store
            .registry()
            .lookup(&TokenHash::of(created.token.as_bytes()), seen);
        eventually("last_seen flush", || {
            last_seen_on_disk(&dir, &created.record.id) == Some(db::ts_to_sql(seen))
        })
        .await;
        cancel.cancel();
        jobs.await.expect("jobs exit cleanly");
    }

    #[tokio::test]
    async fn cancelling_jobs_flushes_what_is_pending() {
        let (dir, store) = open_store();
        let created = store
            .registry()
            .create(
                NewIdentity {
                    identity: "sandbox-7".into(),
                    ..Default::default()
                },
                at(1_000),
            )
            .expect("create");
        let cancel = CancellationToken::new();
        // Intervals far longer than the test: only the shutdown path can
        // write what follows.
        let jobs = store.spawn_jobs(
            JobIntervals {
                last_seen_flush: Duration::from_secs(3_600),
                retention: Duration::from_secs(3_600),
            },
            cancel.clone(),
        );
        // Let the immediate first ticks pass.
        tokio::time::sleep(Duration::from_millis(50)).await;
        store
            .registry()
            .lookup(&TokenHash::of(created.token.as_bytes()), at(1_234));
        store.requests().submit(rec("queued", Utc::now())).await;
        assert_eq!(last_seen_on_disk(&dir, &created.record.id), None);

        cancel.cancel();
        jobs.await.expect("jobs exit cleanly");
        assert_eq!(
            last_seen_on_disk(&dir, &created.record.id),
            Some(db::ts_to_sql(at(1_234)))
        );
        assert_eq!(request_ids_on_disk(&dir), ["queued"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_submitters_all_land_once() {
        let (dir, store) = open_store();
        let mut tasks = Vec::new();
        for t in 0..8 {
            let log = Arc::clone(store.requests());
            tasks.push(tokio::spawn(async move {
                for i in 0..250 {
                    log.submit(rec(&format!("t{t}-{i}"), at(1_000 + i))).await;
                }
            }));
        }
        for t in tasks {
            t.await.expect("submitter");
        }
        store.requests().flush().await;
        let mut ids = request_ids_on_disk(&dir);
        assert_eq!(ids.len(), 2_000);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2_000);
        assert_eq!(store.requests().stats().rows, 2_000);
    }

    #[test]
    fn job_intervals_follow_retention() {
        let hour = Duration::from_secs(3_600);
        let default = JobIntervals::for_retention(Duration::from_secs(30 * 86_400));
        assert_eq!(default.retention, RETENTION_INTERVAL);
        assert_eq!(default.last_seen_flush, LAST_SEEN_FLUSH_INTERVAL);
        assert_eq!(
            JobIntervals::for_retention(hour / 4).retention,
            hour / 4,
            "a short retention is enforced at its own granularity"
        );
        assert_eq!(
            JobIntervals::for_retention(Duration::ZERO).retention,
            Duration::from_secs(1)
        );
    }

    #[test]
    fn open_requires_an_existing_data_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        match Store::open(&missing, &RequestLogConfig::default()) {
            Err(StoreError::Io { path, .. }) => {
                assert_eq!(path, paths::database_path(&missing));
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
