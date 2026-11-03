//! Opening `janus.db`: file mode, pragmas, and the schema migrations.
//!
//! Migrations live here in code, applied in order and tracked with SQLite's
//! `user_version`. They are append-only: a released migration is never
//! edited, a schema change is a new entry. A database whose `user_version`
//! is newer than this build knows is refused rather than guessed at.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};

use super::StoreError;

/// How long a connection waits on another connection's write lock (the
/// request-log writer, a registry mutation, the retention job) before
/// failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `MIGRATIONS[i]` takes the schema from `user_version = i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    // 1: identities registry and request log
    // (docs/admin-surface#storage). Timestamps are RFC 3339 UTC with nine
    // fractional digits (see `ts_to_sql`), fixed width, so text order is
    // time order and the file stays readable with the sqlite3 shell.
    "CREATE TABLE identities (
        id           TEXT PRIMARY KEY NOT NULL,
        token_hash   TEXT NOT NULL UNIQUE,
        identity     TEXT NOT NULL,
        claims       TEXT NOT NULL,
        label        TEXT,
        created_at   TEXT NOT NULL,
        expires_at   TEXT,
        revoked_at   TEXT,
        last_seen_at TEXT
    );
    CREATE INDEX identities_identity ON identities (identity);
    CREATE TABLE requests (
        seq             INTEGER PRIMARY KEY AUTOINCREMENT,
        request_id      TEXT NOT NULL,
        received_at     TEXT NOT NULL,
        duration_ms     INTEGER NOT NULL,
        upstream        TEXT,
        route           TEXT,
        method          TEXT NOT NULL,
        path            TEXT NOT NULL,
        binding         TEXT,
        caller_identity TEXT NOT NULL,
        claimed_identity TEXT,
        correlation_id  TEXT,
        outcome         TEXT NOT NULL,
        outcome_detail  TEXT,
        credential_ref  TEXT,
        status          INTEGER,
        usage           TEXT,
        error           TEXT
    );
    CREATE INDEX requests_received_at ON requests (received_at);
    CREATE INDEX requests_identity_received_at ON requests (caller_identity, received_at);
    CREATE INDEX requests_request_id ON requests (request_id);",
];

/// The schema version this build writes.
pub(super) fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Open (creating if absent, mode `0600`) the database at `path` in WAL
/// mode and bring its schema up to date.
pub(super) fn open(path: &Path) -> Result<Connection, StoreError> {
    // Create the file ourselves so it never exists with a umask-derived
    // mode; SQLite gives the -wal and -shm files the main file's mode.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut conn = connect(path)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Schema(format!(
            "could not enable WAL journal mode (got {mode:?})"
        )));
    }
    migrate(&mut conn)?;
    Ok(conn)
}

/// Another connection to an already-opened database.
pub(super) fn connect(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL + NORMAL: a commit is durable against a process crash; an OS
    // crash can lose the last few commits but never corrupts the file.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > schema_version() {
        return Err(StoreError::Schema(format!(
            "database schema version {current} is newer than this daemon's {}",
            schema_version()
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// A timestamp's column form.
pub(super) fn ts_to_sql(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Parse a timestamp column written by [`ts_to_sql`].
pub(super) fn ts_from_sql(text: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|e| StoreError::Corrupt(format!("bad timestamp {text:?}: {e}")))
}

/// Parse an optional timestamp column.
pub(super) fn opt_ts_from_sql(text: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    text.as_deref().map(ts_from_sql).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_bring_a_new_file_to_the_current_version_in_wal_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("janus.db");
        let conn = open(&path).expect("open");
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, schema_version());
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal");
        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'requests_%' ORDER BY name")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        names.sort();
        assert_eq!(
            names,
            [
                "requests_identity_received_at",
                "requests_received_at",
                "requests_request_id"
            ]
        );
        let mode = std::fs::metadata(&path).expect("stat");
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        drop(conn);
        // Reopening an up-to-date file runs no migration and keeps data.
        let conn = open(&path).expect("reopen");
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, schema_version());
    }

    #[test]
    fn a_database_from_a_newer_daemon_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("janus.db");
        drop(open(&path).expect("open"));
        let conn = Connection::open(&path).expect("raw open");
        conn.pragma_update(None, "user_version", schema_version() + 1)
            .expect("bump");
        drop(conn);
        match open(&path) {
            Err(StoreError::Schema(msg)) => assert!(msg.contains("newer"), "{msg}"),
            other => panic!("expected schema refusal, got {other:?}"),
        }
    }

    #[test]
    fn timestamps_are_fixed_width_and_round_trip_at_nanosecond_precision() {
        let a = DateTime::from_timestamp(1_700_000_000, 5).expect("ts");
        let b = DateTime::from_timestamp(1_700_000_000, 400_000_000).expect("ts");
        let (sa, sb) = (ts_to_sql(a), ts_to_sql(b));
        assert_eq!(sa.len(), sb.len());
        assert!(sa < sb, "text order must be time order: {sa} {sb}");
        assert_eq!(ts_from_sql(&sa).expect("parse"), a);
        assert!(ts_from_sql("yesterday").is_err());
    }
}
