//! `SqliteUsageStore` (feature `sqlite`): an embedded database file
//! (`docs/plugin-contract` "The store").

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use tokio::sync::oneshot;

use super::ledger::{evaluate, ledger_cutoff, unix_now};
use crate::contract::metering::{
    Apply, Bound, Durability, MeterKey, OpId, Phase, UsageError, UsageStore, DEFAULT_LEDGER_TTL,
};

/// A `Durable` [`UsageStore`] in one SQLite file at a path the plugin
/// chooses, so a `Total` budget survives a plugin restart.
///
/// All operations run on one writer thread that owns the connection; each
/// `apply`, `release`, `settle` and `compact` is a single `BEGIN IMMEDIATE`
/// transaction; the file is in WAL mode with `synchronous=FULL`. Opening
/// takes an exclusive advisory lock for the file and fails with
/// [`UsageError::Locked`] while another store — in this or any other process
/// — holds it: two replicas sharing one file is not a supported deployment.
/// The lock is taken on a `<path>-lock` file beside the database, because on
/// some platforms (macOS) a whole-file advisory lock on the database itself
/// conflicts with SQLite's own byte-range locks.
pub struct SqliteUsageStore {
    jobs: mpsc::Sender<Job>,
    writer: Option<JoinHandle<()>>,
    /// Holds the advisory lock until the writer has closed the connection.
    _lock: File,
    ledger_ttl: Duration,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// `PRAGMA user_version` of the schema below. A file at `0` is new (the
/// schema and its version are written in one transaction); any version other
/// than this one is refused, as there are no migrations.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage_counts (
    identity     TEXT    NOT NULL,
    meter        TEXT    NOT NULL,
    period_secs  INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    used         INTEGER NOT NULL,
    retain_until INTEGER,
    PRIMARY KEY (identity, meter, period_secs, window_start)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS usage_counts_retain_until
    ON usage_counts (retain_until) WHERE retain_until IS NOT NULL;
CREATE TABLE IF NOT EXISTS usage_ledger (
    identity       TEXT    NOT NULL,
    meter          TEXT    NOT NULL,
    request_id     TEXT    NOT NULL,
    phase          TEXT    NOT NULL,
    created_at     INTEGER NOT NULL,
    amount         INTEGER NOT NULL,
    reversible     INTEGER NOT NULL,
    rejected_index INTEGER,
    rejected_used  INTEGER,
    PRIMARY KEY (identity, meter, request_id, phase)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS usage_ledger_created_at ON usage_ledger (created_at);
CREATE TABLE IF NOT EXISTS usage_ledger_keys (
    identity     TEXT    NOT NULL,
    meter        TEXT    NOT NULL,
    request_id   TEXT    NOT NULL,
    phase        TEXT    NOT NULL,
    position     INTEGER NOT NULL,
    key_identity TEXT    NOT NULL,
    key_meter    TEXT    NOT NULL,
    period_secs  INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    used_after   INTEGER NOT NULL,
    PRIMARY KEY (identity, meter, request_id, phase, position),
    FOREIGN KEY (identity, meter, request_id, phase)
        REFERENCES usage_ledger (identity, meter, request_id, phase) ON DELETE CASCADE
) WITHOUT ROWID;
";

const OP: &str = "identity = ?1 AND meter = ?2 AND request_id = ?3 AND phase = ?4";

impl SqliteUsageStore {
    /// Open, creating if absent, the store at `path`, with ledger entries
    /// living for [`DEFAULT_LEDGER_TTL`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, UsageError> {
        Self::open_with_ledger_ttl(path, DEFAULT_LEDGER_TTL)
    }

    /// As `open`, with ledger entries dropped by `compact` once older than
    /// `ledger_ttl`.
    pub fn open_with_ledger_ttl(
        path: impl AsRef<Path>,
        ledger_ttl: Duration,
    ) -> Result<Self, UsageError> {
        let path = path.as_ref();
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push("-lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(backend)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(UsageError::Locked(path.display().to_string()))
            }
            Err(TryLockError::Error(err)) => return Err(backend(err)),
        }
        let mut conn = Connection::open(path).map_err(backend)?;
        initialize(&mut conn)?;
        let (jobs, inbox) = mpsc::channel::<Job>();
        let writer = std::thread::Builder::new()
            .name("janus-usage-sqlite".into())
            .spawn(move || {
                let mut conn = conn;
                for job in inbox {
                    job(&mut conn);
                }
            })
            .map_err(backend)?;
        Ok(Self {
            jobs,
            writer: Some(writer),
            _lock: lock,
            ledger_ttl,
        })
    }

    /// Run `work` on the writer thread and wait for its result. A caller
    /// that stops waiting does not cancel it.
    async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, UsageError> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Box::new(move |conn| {
                // The caller may have stopped waiting; the work is done either way.
                let _ = reply.send(work(conn));
            }))
            .map_err(|_| writer_stopped())?;
        result.await.map_err(|_| writer_stopped())?.map_err(backend)
    }
}

impl Drop for SqliteUsageStore {
    /// Stop the writer after the jobs already queued and wait for it to close
    /// the connection, so the file is unlocked only once nothing writes it.
    fn drop(&mut self) {
        let (closed, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.jobs, closed));
        if let Some(writer) = self.writer.take() {
            // A writer that panicked has nothing left to flush.
            let _ = writer.join();
        }
    }
}

fn initialize(conn: &mut Connection) -> Result<(), UsageError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(backend)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(UsageError::Backend(
            format!("usage store could not enter WAL mode (journal_mode is {mode})").into(),
        ));
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(backend)?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(backend)?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(backend)?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(UsageError::Backend(
            format!("usage store schema version {version} is not this build's {SCHEMA_VERSION}")
                .into(),
        ));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(backend)?;
    tx.execute_batch(SCHEMA).map_err(backend)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(backend)?;
    tx.commit().map_err(backend)
}

/// SQLite integers are signed; counts are stored bit-for-bit so the whole
/// `u64` range round-trips. Arithmetic on them happens in Rust.
fn to_sql(value: u64) -> i64 {
    value as i64
}

fn from_sql(value: i64) -> u64 {
    value as u64
}

fn backend(err: impl std::error::Error + Send + Sync + 'static) -> UsageError {
    UsageError::Backend(Box::new(err))
}

fn writer_stopped() -> UsageError {
    UsageError::Backend("usage store writer thread has stopped".into())
}

/// A `Bound` the writer thread can own.
struct OwnedBound {
    key: MeterKey,
    limit: Option<u64>,
    retain_until: Option<i64>,
}

fn count(conn: &Connection, key: &MeterKey) -> rusqlite::Result<u64> {
    let used: Option<i64> = conn
        .query_row(
            "SELECT used FROM usage_counts
             WHERE identity = ?1 AND meter = ?2 AND period_secs = ?3 AND window_start = ?4",
            params![
                key.identity,
                key.meter,
                to_sql(key.period_secs),
                key.window_start
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(used.map_or(0, from_sql))
}

/// What a previously applied `op` returned, if the ledger still holds it.
fn recorded_result(conn: &Connection, op: &OpId) -> rusqlite::Result<Option<Apply>> {
    let row: Option<(Option<i64>, Option<i64>)> = conn
        .query_row(
            &format!("SELECT rejected_index, rejected_used FROM usage_ledger WHERE {OP}"),
            op_params(op),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((rejected_index, rejected_used)) = row else {
        return Ok(None);
    };
    if let (Some(index), Some(used)) = (rejected_index, rejected_used) {
        let index = usize::try_from(index).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, err.into())
        })?;
        return Ok(Some(Apply::Rejected {
            index,
            used: from_sql(used),
        }));
    }
    let mut totals = conn.prepare(&format!(
        "SELECT used_after FROM usage_ledger_keys WHERE {OP} ORDER BY position"
    ))?;
    let used = totals
        .query_map(op_params(op), |row| row.get::<_, i64>(0).map(from_sql))?
        .collect::<rusqlite::Result<Vec<u64>>>()?;
    Ok(Some(Apply::Applied { used }))
}

fn op_params(op: &OpId) -> (&str, &str, &str, &'static str) {
    (&op.identity, &op.meter, &op.request_id, op.phase.as_str())
}

fn apply_op(
    conn: &mut Connection,
    op: &OpId,
    bounds: &[OwnedBound],
    amount: u64,
) -> rusqlite::Result<Apply> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(result) = recorded_result(&tx, op)? {
        return Ok(result);
    }
    let current = bounds
        .iter()
        .map(|bound| count(&tx, &bound.key))
        .collect::<rusqlite::Result<Vec<u64>>>()?;
    let result = evaluate(&current, bounds.iter().map(|bound| bound.limit), amount);
    let (rejected_index, rejected_used) = match &result {
        Apply::Rejected { index, used } => (Some(*index as i64), Some(to_sql(*used))),
        Apply::Applied { .. } => (None, None),
    };
    let reversible = op.phase == Phase::Reserve && rejected_index.is_none();
    tx.execute(
        "INSERT INTO usage_ledger (identity, meter, request_id, phase, created_at, amount,
                                   reversible, rejected_index, rejected_used)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            op.identity,
            op.meter,
            op.request_id,
            op.phase.as_str(),
            unix_now(),
            to_sql(amount),
            reversible,
            rejected_index,
            rejected_used
        ],
    )?;
    if let Apply::Applied { used } = &result {
        for (position, (bound, &total)) in bounds.iter().zip(used).enumerate() {
            tx.execute(
                "INSERT INTO usage_counts (identity, meter, period_secs, window_start, used,
                                           retain_until)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (identity, meter, period_secs, window_start)
                 DO UPDATE SET used = excluded.used, retain_until = excluded.retain_until",
                params![
                    bound.key.identity,
                    bound.key.meter,
                    to_sql(bound.key.period_secs),
                    bound.key.window_start,
                    to_sql(total),
                    bound.retain_until
                ],
            )?;
            tx.execute(
                "INSERT INTO usage_ledger_keys (identity, meter, request_id, phase, position,
                                                key_identity, key_meter, period_secs,
                                                window_start, used_after)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    op.identity,
                    op.meter,
                    op.request_id,
                    op.phase.as_str(),
                    position as i64,
                    bound.key.identity,
                    bound.key.meter,
                    to_sql(bound.key.period_secs),
                    bound.key.window_start,
                    to_sql(total)
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(result)
}

fn release_op(conn: &mut Connection, op: &OpId) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let amount: Option<i64> = tx
        .query_row(
            &format!("SELECT amount FROM usage_ledger WHERE {OP} AND reversible = 1"),
            op_params(op),
            |row| row.get(0),
        )
        .optional()?;
    let Some(amount) = amount.map(from_sql) else {
        return Ok(false);
    };
    tx.execute(
        &format!("UPDATE usage_ledger SET reversible = 0 WHERE {OP}"),
        op_params(op),
    )?;
    let touched = {
        let mut keys = tx.prepare(&format!(
            "SELECT key_identity, key_meter, period_secs, window_start
             FROM usage_ledger_keys WHERE {OP}"
        ))?;
        let rows = keys.query_map(op_params(op), |row| {
            Ok(MeterKey {
                identity: row.get(0)?,
                meter: row.get(1)?,
                period_secs: from_sql(row.get(2)?),
                window_start: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<MeterKey>>>()?
    };
    for key in &touched {
        tx.execute(
            "UPDATE usage_counts SET used = ?5
             WHERE identity = ?1 AND meter = ?2 AND period_secs = ?3 AND window_start = ?4",
            params![
                key.identity,
                key.meter,
                to_sql(key.period_secs),
                key.window_start,
                to_sql(count(&tx, key)?.saturating_sub(amount))
            ],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

#[async_trait]
impl UsageStore for SqliteUsageStore {
    async fn apply(
        &self,
        op: &OpId,
        bounds: &[Bound<'_>],
        amount: u64,
    ) -> Result<Apply, UsageError> {
        let op = op.clone();
        let bounds: Vec<OwnedBound> = bounds
            .iter()
            .map(|bound| OwnedBound {
                key: bound.key.clone(),
                limit: bound.limit,
                retain_until: bound.retain_until,
            })
            .collect();
        self.call(move |conn| apply_op(conn, &op, &bounds, amount))
            .await
    }

    async fn release(&self, op: &OpId) -> Result<bool, UsageError> {
        let op = op.clone();
        self.call(move |conn| release_op(conn, &op)).await
    }

    async fn settle(&self, op: &OpId) -> Result<(), UsageError> {
        let op = op.clone();
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                &format!("UPDATE usage_ledger SET reversible = 0 WHERE {OP}"),
                op_params(&op),
            )?;
            tx.commit()
        })
        .await
    }

    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError> {
        let key = key.clone();
        self.call(move |conn| count(conn, &key)).await
    }

    async fn compact(&self, now: i64) -> Result<u64, UsageError> {
        let cutoff = ledger_cutoff(now, self.ledger_ttl);
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let keys = tx.execute(
                "DELETE FROM usage_counts WHERE retain_until IS NOT NULL AND retain_until < ?1",
                [now],
            )?;
            // Cascades to `usage_ledger_keys`, which is not counted.
            let entries = tx.execute("DELETE FROM usage_ledger WHERE created_at < ?1", [cutoff])?;
            tx.commit()?;
            Ok((keys + entries) as u64)
        })
        .await
    }

    fn durability(&self) -> Durability {
        Durability::Durable
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::contract::metering::{Charge, Quota, Window};
    use crate::metering::Meter;

    fn fresh() -> (SqliteUsageStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteUsageStore::open(dir.path().join("usage.db")).expect("open");
        (store, dir)
    }

    mod conformance {
        crate::usage_store_conformance_tests!(super::fresh, super::DEFAULT_LEDGER_TTL);
    }

    fn quotas() -> Vec<(String, Quota)> {
        vec![
            (
                "requests".to_owned(),
                Quota {
                    limit: 10,
                    window: Window::Fixed {
                        period: Duration::from_secs(3600),
                    },
                },
            ),
            (
                "credits".to_owned(),
                Quota {
                    limit: 100,
                    window: Window::Total,
                },
            ),
        ]
    }

    fn credits() -> MeterKey {
        MeterKey {
            identity: "alice".into(),
            meter: "credits".into(),
            period_secs: 0,
            window_start: 0,
        }
    }

    #[tokio::test]
    async fn sqlite_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        {
            let meter = Meter::new(SqliteUsageStore::open(&path).expect("open"), quotas())
                .expect("a durable store accepts a Total quota");
            meter
                .charge("r1", "alice", "credits", 30)
                .await
                .expect("charge");
            meter
                .charge("r2", "alice", "credits", 20)
                .await
                .expect("charge");
            meter
                .record("r1", "alice", "credits", 5)
                .await
                .expect("record");
            meter
                .charge("r3", "alice", "requests", 4)
                .await
                .expect("charge");
        }
        let meter =
            Meter::new(SqliteUsageStore::open(&path).expect("reopen"), quotas()).expect("meter");
        assert_eq!(meter.store().get(&credits()).await.expect("get"), 55);
        assert_eq!(
            meter
                .charge("r1", "alice", "credits", 30)
                .await
                .expect("charge"),
            Charge::Allowed {
                used: 30,
                limit: 100
            },
            "the ledger survives too: a redelivered charge replays"
        );
        meter
            .record("r1", "alice", "credits", 5)
            .await
            .expect("record");
        assert!(
            meter
                .refund("r2", "alice", "credits")
                .await
                .expect("refund"),
            "a charge made before the restart is refundable after it"
        );
        assert_eq!(meter.store().get(&credits()).await.expect("get"), 35);
        assert_eq!(
            meter.check("alice", "requests").await.expect("check"),
            Charge::Allowed { used: 4, limit: 10 }
        );
    }

    #[tokio::test]
    async fn sqlite_store_is_durable_and_accepts_total() {
        let (store, _dir) = fresh();
        assert_eq!(store.durability(), Durability::Durable);
        assert_eq!(store.ledger_ttl, DEFAULT_LEDGER_TTL);
        assert!(Meter::new(store, quotas()).is_ok());
    }

    #[test]
    fn sqlite_refuses_a_file_another_store_holds_until_it_is_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        let held = SqliteUsageStore::open(&path).expect("open");
        match SqliteUsageStore::open(&path) {
            Err(UsageError::Locked(locked)) => assert_eq!(locked, path.display().to_string()),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a second store opened a held file"),
        }
        drop(held);
        SqliteUsageStore::open(&path).expect("reopen once released");
    }

    #[tokio::test]
    async fn sqlite_runs_in_wal_mode_with_full_sync() {
        let (store, _dir) = fresh();
        let (journal, synchronous, foreign_keys, version) = store
            .call(|conn| {
                let int = |name: &str| {
                    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
                };
                let journal: String =
                    conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                Ok((
                    journal,
                    int("synchronous")?,
                    int("foreign_keys")?,
                    int("user_version")?,
                ))
            })
            .await
            .expect("pragmas");
        assert_eq!(journal, "wal");
        assert_eq!(synchronous, 2, "FULL");
        assert_eq!(foreign_keys, 1);
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn sqlite_refuses_another_schema_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        drop(SqliteUsageStore::open(&path).expect("open"));
        Connection::open(&path)
            .and_then(|conn| conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1))
            .expect("bump schema");
        assert!(matches!(
            SqliteUsageStore::open(&path),
            Err(UsageError::Backend(err)) if err.to_string().contains("schema version 2")
        ));
    }

    #[tokio::test]
    async fn sqlite_counts_round_trip_the_whole_u64_range() {
        let (store, _dir) = fresh();
        let op = OpId {
            identity: "alice".into(),
            meter: "credits".into(),
            request_id: "big".into(),
            phase: Phase::Settle,
        };
        let key = credits();
        let bound = Bound {
            key: &key,
            limit: None,
            retain_until: None,
        };
        store
            .apply(&op, &[bound], u64::MAX - 1)
            .await
            .expect("apply");
        assert_eq!(store.get(&key).await.expect("get"), u64::MAX - 1);
        assert_eq!(
            store.apply(&op, &[bound], 1).await.expect("replay"),
            Apply::Applied {
                used: vec![u64::MAX - 1]
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sqlite_concurrent_charges_cannot_exceed_limit() {
        let (store, _dir) = fresh();
        let meter = Arc::new(Meter::new(store, quotas()).expect("meter"));
        let tasks: Vec<_> = (0..24)
            .map(|task| {
                let meter = Arc::clone(&meter);
                tokio::spawn(async move {
                    let mut allowed = 0u64;
                    for n in 0..10 {
                        let request = format!("t{task}-r{n}");
                        let charge = meter.charge(&request, "alice", "credits", 1);
                        allowed += u64::from(charge.await.expect("charge").is_allowed());
                    }
                    allowed
                })
            })
            .collect();
        let mut allowed = 0;
        for task in tasks {
            allowed += task.await.expect("task");
        }
        assert_eq!(allowed, 100);
        assert_eq!(meter.store().get(&credits()).await.expect("get"), 100);
    }
}
