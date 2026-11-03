//! The request log (`docs/admin-surface#request-log`): the `requests` table,
//! its writer, its queries, the live tail, and retention.
//!
//! Writes go through a queue to one writer thread, so the data plane hands
//! a finished [`RequestRecord`] over with [`RequestLog::submit`] and never
//! waits on SQLite. The writer commits records in batches and publishes
//! each committed record on a broadcast channel — the source of
//! `GET /v1/requests/stream` ([`RequestLog::subscribe`], filtered with
//! [`RequestFilter::matches`]). Reads ([`RequestLog::query`],
//! [`RequestLog::get`]) are short blocking SQLite calls on their own
//! connection.
//!
//! With `request_log.enabled = false` nothing is written or published, and
//! the read side answers with an empty, `disabled` result rather than with
//! rows left over from when it was enabled. Retention keeps running either
//! way, so those leftovers still age out.
//!
//! Pagination is newest first over `(received_at, seq)`, where `seq` is the
//! insert order; the cursor names the last row of a page. A row inserted
//! while a client pages is never returned twice and never displaces a row
//! that was already there.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use janus_core::contract::binding::Method;
use janus_core::contract::request_log::{Outcome, RequestRecord};
use janus_core::contract::upstream::UpstreamKind;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::db::{ts_from_sql, ts_to_sql};
use super::StoreError;

/// `limit` of `GET /v1/requests` when absent.
pub const DEFAULT_LIMIT: u32 = 100;
/// The largest `limit` honoured; larger values are capped to it.
pub const MAX_LIMIT: u32 = 1000;
/// Every `outcome` value (`docs/admin-surface#request-log`), the
/// vocabulary `?outcome=` is checked against.
pub const OUTCOMES: [&str; 7] = [
    "forwarded",
    "rejected_identity",
    "denied",
    "plugin_failed",
    "credential_unavailable",
    "upstream_error",
    "abandoned",
];

/// Records queued for the writer before `submit` waits.
const QUEUE_CAPACITY: usize = 4096;
/// Records committed in one transaction at most.
const BATCH: usize = 256;
/// Live-tail events buffered per subscriber before it lags.
const TAIL_CAPACITY: usize = 1024;

const COLUMNS: &str = "seq, request_id, received_at, duration_ms, upstream, route, method, path, \
     binding, caller_identity, claimed_identity, correlation_id, outcome, outcome_detail, \
     credential_ref, status, usage, error";

/// The filters shared by `GET /v1/requests` and `GET /v1/requests/stream`
/// (which uses only `identity`, `upstream` and `outcome`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestFilter {
    /// Exact match on `caller_identity`.
    pub identity: Option<String>,
    pub upstream: Option<UpstreamKind>,
    /// One of [`OUTCOMES`].
    pub outcome: Option<String>,
    /// Exact match on `credential_ref`.
    pub credential: Option<String>,
    /// `received_at >= since`.
    pub since: Option<DateTime<Utc>>,
    /// `received_at < until`.
    pub until: Option<DateTime<Utc>>,
}

impl RequestFilter {
    /// Refuse an `outcome` outside the vocabulary.
    pub fn validate(&self) -> Result<(), StoreError> {
        match &self.outcome {
            Some(o) if !OUTCOMES.contains(&o.as_str()) => Err(StoreError::Invalid(format!(
                "unknown outcome {o:?}: expected one of {}",
                OUTCOMES.join(", ")
            ))),
            _ => Ok(()),
        }
    }

    /// Whether `record` passes the filter: the live tail's test, and the
    /// same predicate [`RequestLog::query`] evaluates in SQL.
    pub fn matches(&self, record: &RequestRecord) -> bool {
        self.identity
            .as_ref()
            .is_none_or(|i| *i == record.caller_identity)
            && self.upstream.is_none_or(|u| record.upstream == Some(u))
            && self
                .outcome
                .as_ref()
                .is_none_or(|o| o == record.outcome.name())
            && self
                .credential
                .as_ref()
                .is_none_or(|c| record.credential_ref.as_ref() == Some(c))
            && self.since.is_none_or(|s| record.received_at >= s)
            && self.until.is_none_or(|u| record.received_at < u)
    }
}

/// `GET /v1/requests`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestQuery {
    pub filter: RequestFilter,
    /// [`DEFAULT_LIMIT`] when `None`; capped at [`MAX_LIMIT`]; 0 is refused.
    pub limit: Option<u32>,
    /// The previous page's `next`.
    pub before: Option<String>,
}

/// One page, newest first.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestPage {
    pub records: Vec<RequestRecord>,
    /// Cursor for the following page; `None` on the last one.
    pub next: Option<String>,
    /// `request_log.enabled = false`: the page is empty by configuration,
    /// not because nothing matched.
    pub disabled: bool,
}

/// What `/v1/status` reports about the request log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLogStats {
    pub enabled: bool,
    pub retain: Duration,
    /// Rows currently in the table.
    pub rows: u64,
}

/// Position of a row in newest-first order; the cursor's content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    received_at: String,
    seq: i64,
}

impl Cursor {
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}/{}", self.received_at, self.seq))
    }

    fn decode(text: &str) -> Result<Self, StoreError> {
        let bad = || StoreError::Invalid(format!("invalid cursor {text:?}"));
        let raw = URL_SAFE_NO_PAD.decode(text).map_err(|_| bad())?;
        let raw = String::from_utf8(raw).map_err(|_| bad())?;
        let (at, seq) = raw.rsplit_once('/').ok_or_else(bad)?;
        ts_from_sql(at).map_err(|_| bad())?;
        Ok(Cursor {
            received_at: at.to_owned(),
            seq: seq.parse().map_err(|_| bad())?,
        })
    }
}

enum Command {
    Record(Box<RequestRecord>),
    Flush(oneshot::Sender<()>),
}

/// The request log. Share it behind an `Arc`.
pub struct RequestLog {
    enabled: bool,
    retain: Duration,
    reader: Mutex<Connection>,
    /// `None` when disabled: there is no writer.
    queue: Option<mpsc::Sender<Command>>,
    tail: broadcast::Sender<Arc<RequestRecord>>,
    rows: Arc<AtomicU64>,
}

impl fmt::Debug for RequestLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestLog")
            .field("enabled", &self.enabled)
            .field("retain", &self.retain)
            .field("rows", &self.rows.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl RequestLog {
    /// `reader` serves queries and retention; `writer`, when enabled, moves
    /// to the writer thread.
    pub(super) fn start(
        enabled: bool,
        retain: Duration,
        reader: Connection,
        writer: Connection,
    ) -> Result<Self, StoreError> {
        let count: i64 = reader.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))?;
        let rows = Arc::new(AtomicU64::new(count.max(0) as u64));
        let (tail, _) = broadcast::channel(TAIL_CAPACITY);
        let queue = if enabled {
            let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
            let (tail, rows) = (tail.clone(), Arc::clone(&rows));
            std::thread::Builder::new()
                .name("janus-request-log".into())
                .spawn(move || run_writer(writer, rx, tail, rows))
                .map_err(|source| StoreError::Io {
                    path: "request-log writer thread".into(),
                    source,
                })?;
            Some(tx)
        } else {
            None
        };
        Ok(RequestLog {
            enabled,
            retain,
            reader: Mutex::new(reader),
            queue,
            tail,
            rows,
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn retain(&self) -> Duration {
        self.retain
    }

    /// Queue a finished request's record. Waits only if the writer is
    /// [`QUEUE_CAPACITY`] records behind; a no-op when disabled.
    pub async fn submit(&self, record: RequestRecord) {
        let Some(queue) = &self.queue else { return };
        if queue.send(Command::Record(Box::new(record))).await.is_err() {
            tracing::error!("request-log writer has stopped; record dropped");
        }
    }

    /// Wait until every record submitted before this call is committed (or
    /// has failed to commit). Used at shutdown.
    pub async fn flush(&self) {
        if let Some(queue) = &self.queue {
            let (done, wait) = oneshot::channel();
            if queue.send(Command::Flush(done)).await.is_ok() {
                // An error means the writer exited; nothing is left to wait for.
                let _ = wait.await;
            }
        }
    }

    /// Every record committed from now on. Never yields when disabled.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<RequestRecord>> {
        self.tail.subscribe()
    }

    /// `GET /v1/requests`.
    pub fn query(&self, q: &RequestQuery) -> Result<RequestPage, StoreError> {
        q.filter.validate()?;
        let limit = match q.limit {
            None => DEFAULT_LIMIT,
            Some(0) => return Err(StoreError::Invalid("limit must be at least 1".into())),
            Some(n) => n.min(MAX_LIMIT),
        };
        let cursor = q.before.as_deref().map(Cursor::decode).transpose()?;
        if !self.enabled {
            return Ok(RequestPage {
                disabled: true,
                ..RequestPage::default()
            });
        }
        let f = &q.filter;
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        let mut push = |clause: &'static str, value: Option<String>| {
            if let Some(v) = value {
                clauses.push(clause);
                args.push(Value::Text(v));
            }
        };
        push("caller_identity = ?", f.identity.clone());
        push("upstream = ?", f.upstream.map(|u| u.as_str().to_owned()));
        push("outcome = ?", f.outcome.clone());
        push("credential_ref = ?", f.credential.clone());
        push("received_at >= ?", f.since.map(ts_to_sql));
        push("received_at < ?", f.until.map(ts_to_sql));
        if let Some(c) = &cursor {
            clauses.push("(received_at < ? OR (received_at = ? AND seq < ?))");
            args.push(Value::Text(c.received_at.clone()));
            args.push(Value::Text(c.received_at.clone()));
            args.push(Value::Integer(c.seq));
        }
        let mut sql = format!("SELECT {COLUMNS} FROM requests");
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY received_at DESC, seq DESC LIMIT ?");
        args.push(Value::Integer(i64::from(limit) + 1));

        let conn = self.lock_reader();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut page: Vec<(Cursor, RequestRecord)> = Vec::new();
        while let Some(row) = rows.next()? {
            page.push(from_row(row)?);
        }
        let next = if page.len() > limit as usize {
            page.truncate(limit as usize);
            page.last().map(|(c, _)| c.encode())
        } else {
            None
        };
        Ok(RequestPage {
            records: page.into_iter().map(|(_, r)| r).collect(),
            next,
            disabled: false,
        })
    }

    /// `GET /v1/requests/{request_id}`. `None` when absent or disabled.
    pub fn get(&self, request_id: &str) -> Result<Option<RequestRecord>, StoreError> {
        if !self.enabled {
            return Ok(None);
        }
        let conn = self.lock_reader();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM requests WHERE request_id = ?1 ORDER BY seq DESC LIMIT 1"
        ))?;
        let row = stmt
            .query_row([request_id], |row| Ok(from_row(row)))
            .optional()?;
        row.transpose().map(|r| r.map(|(_, record)| record))
    }

    /// Delete rows received more than `retain` before `now`. Returns how
    /// many were deleted.
    pub fn prune(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let Some(cutoff) = chrono::Duration::from_std(self.retain)
            .ok()
            .and_then(|d| now.checked_sub_signed(d))
        else {
            // A retention longer than the representable past keeps everything.
            return Ok(0);
        };
        let deleted = self.lock_reader().execute(
            "DELETE FROM requests WHERE received_at < ?1",
            params![ts_to_sql(cutoff)],
        )? as u64;
        // Saturating: never wrap if the count was ever off.
        let _ = self
            .rows
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(deleted))
            });
        Ok(deleted)
    }

    pub fn stats(&self) -> RequestLogStats {
        RequestLogStats {
            enabled: self.enabled,
            retain: self.retain,
            rows: self.rows.load(Ordering::Relaxed),
        }
    }

    fn lock_reader(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.reader.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn run_writer(
    mut conn: Connection,
    mut queue: mpsc::Receiver<Command>,
    tail: broadcast::Sender<Arc<RequestRecord>>,
    rows: Arc<AtomicU64>,
) {
    while let Some(first) = queue.blocking_recv() {
        let mut batch = Vec::new();
        let mut flushes = Vec::new();
        let mut next = Some(first);
        while let Some(cmd) = next.take() {
            match cmd {
                Command::Record(r) => batch.push(*r),
                // A flush ends the batch: it covers what came before it.
                Command::Flush(done) => {
                    flushes.push(done);
                    break;
                }
            }
            if batch.len() < BATCH {
                next = queue.try_recv().ok();
            }
        }
        if !batch.is_empty() {
            match insert_batch(&mut conn, &batch) {
                Ok(()) => {
                    rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    for record in batch {
                        // No subscribers is not an error.
                        let _ = tail.send(Arc::new(record));
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, records = batch.len(), "request-log write failed")
                }
            }
        }
        for done in flushes {
            let _ = done.send(());
        }
    }
}

fn insert_batch(conn: &mut Connection, batch: &[RequestRecord]) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO requests (request_id, received_at, duration_ms, upstream, route, method, \
             path, binding, caller_identity, claimed_identity, correlation_id, outcome, \
             outcome_detail, credential_ref, status, usage, error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )?;
        for r in batch {
            stmt.execute(params![
                r.request_id,
                ts_to_sql(r.received_at),
                i64::try_from(r.duration_ms).unwrap_or(i64::MAX),
                r.upstream.map(UpstreamKind::as_str),
                r.route,
                r.method,
                r.path,
                r.binding.as_ref().map(Method::name),
                r.caller_identity,
                r.claimed_identity,
                r.correlation_id,
                r.outcome.name(),
                outcome_detail(&r.outcome)?,
                r.credential_ref,
                r.status,
                r.usage.as_ref().map(serde_json::Value::to_string),
                r.error,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The outcome's qualifier (`kind`, `reason`, `class`, `stage`) as a JSON
/// object, or `None` for an outcome without one. Kept as JSON so the
/// column is derived from the contract's serde shape, not a second copy of
/// the vocabulary.
fn outcome_detail(outcome: &Outcome) -> Result<Option<String>, StoreError> {
    let mut value = serde_json::to_value(outcome)
        .map_err(|e| StoreError::Invalid(format!("outcome does not serialize: {e}")))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("outcome is not an object".into()))?;
    obj.remove("outcome");
    Ok((!obj.is_empty()).then(|| value.to_string()))
}

fn from_row(row: &Row<'_>) -> Result<(Cursor, RequestRecord), StoreError> {
    let seq: i64 = row.get("seq")?;
    let request_id: String = row.get("request_id")?;
    let corrupt = |what: &str, e: &dyn fmt::Display| {
        StoreError::Corrupt(format!("request {request_id}: bad {what}: {e}"))
    };
    let received_text: String = row.get("received_at")?;
    let outcome = {
        let name: String = row.get("outcome")?;
        let mut value = match row.get::<_, Option<String>>("outcome_detail")? {
            Some(detail) => serde_json::from_str(&detail).map_err(|e| corrupt("outcome", &e))?,
            None => serde_json::Value::Object(Default::default()),
        };
        value
            .as_object_mut()
            .ok_or_else(|| corrupt("outcome", &"detail is not an object"))?
            .insert("outcome".into(), name.into());
        serde_json::from_value::<Outcome>(value).map_err(|e| corrupt("outcome", &e))?
    };
    let record = RequestRecord {
        received_at: ts_from_sql(&received_text)?,
        duration_ms: row.get::<_, i64>("duration_ms")?.max(0) as u64,
        upstream: row
            .get::<_, Option<String>>("upstream")?
            .map(|u| u.parse::<UpstreamKind>())
            .transpose()
            .map_err(|e| corrupt("upstream", &e))?,
        route: row.get("route")?,
        method: row.get("method")?,
        path: row.get("path")?,
        binding: row
            .get::<_, Option<String>>("binding")?
            .map(|b| b.parse::<Method>())
            .transpose()
            .map_err(|e| corrupt("binding", &e))?,
        caller_identity: row.get("caller_identity")?,
        claimed_identity: row.get("claimed_identity")?,
        correlation_id: row.get("correlation_id")?,
        outcome,
        credential_ref: row.get("credential_ref")?,
        status: row.get("status")?,
        usage: row
            .get::<_, Option<String>>("usage")?
            .map(|u| serde_json::from_str(&u))
            .transpose()
            .map_err(|e| corrupt("usage", &e))?,
        error: row.get("error")?,
        request_id,
    };
    // The cursor keeps the column's own text so comparisons are exact.
    Ok((
        Cursor {
            received_at: received_text,
            seq,
        },
        record,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use janus_core::contract::identity::RejectionKind;
    use janus_core::contract::plugin::PluginFailureClass;
    use janus_core::contract::request_log::AbandonStage;

    use super::*;
    use crate::contract::config::RequestLogConfig;
    use crate::store::test_support::{at, open_store, reopen};
    use crate::store::Store;

    fn rec(id: &str, secs: i64, outcome: Outcome) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            received_at: at(secs),
            duration_ms: 7,
            upstream: Some(UpstreamKind::OpenAi),
            route: None,
            method: "POST".into(),
            path: "/openai/v1/chat/completions".into(),
            binding: Some(Method::Opaque),
            caller_identity: "sandbox-7".into(),
            claimed_identity: None,
            correlation_id: None,
            outcome,
            credential_ref: Some("openai-main".into()),
            status: Some(200),
            usage: None,
            error: None,
        }
    }

    fn every_outcome() -> Vec<Outcome> {
        vec![
            Outcome::Forwarded,
            Outcome::RejectedIdentity {
                kind: RejectionKind::Revoked,
            },
            Outcome::Denied {
                reason: "budget exhausted".into(),
            },
            Outcome::PluginFailed {
                class: PluginFailureClass::Timeout,
            },
            Outcome::CredentialUnavailable,
            Outcome::UpstreamError,
            Outcome::Abandoned {
                stage: AbandonStage::Forward,
            },
        ]
    }

    async fn write(log: &RequestLog, records: impl IntoIterator<Item = RequestRecord>) {
        for r in records {
            log.submit(r).await;
        }
        log.flush().await;
    }

    fn raw_count(dir: &tempfile::TempDir) -> i64 {
        Connection::open(dir.path().join("janus.db"))
            .expect("raw open")
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count")
    }

    fn ids(page: &RequestPage) -> Vec<&str> {
        page.records.iter().map(|r| r.request_id.as_str()).collect()
    }

    fn query(log: &RequestLog, filter: RequestFilter) -> Vec<String> {
        let page = log
            .query(&RequestQuery {
                filter,
                ..Default::default()
            })
            .expect("query");
        ids(&page).into_iter().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn every_outcome_and_field_round_trips_and_survives_reopen() {
        let (dir, store) = open_store();
        let mut records: Vec<RequestRecord> = every_outcome()
            .into_iter()
            .enumerate()
            .map(|(i, o)| rec(&format!("r{i}"), 1_000 + i as i64, o))
            .collect();
        let full = RequestRecord {
            received_at: DateTime::from_timestamp(2_000, 123_456_789).expect("ts"),
            duration_ms: u64::from(u32::MAX) + 5,
            upstream: None,
            route: Some("/v1/messages".into()),
            binding: Some(Method::Custom("sandbox-sig".into())),
            caller_identity: String::new(),
            claimed_identity: Some("session-9".into()),
            correlation_id: Some("corr-1".into()),
            status: None,
            usage: Some(serde_json::json!({"input_tokens": 12, "nested": {"cached": true}})),
            error: Some("signature mismatch".into()),
            ..rec(
                "full",
                0,
                Outcome::RejectedIdentity {
                    kind: RejectionKind::WrongNamespace,
                },
            )
        };
        records.push(full);
        write(store.requests(), records.clone()).await;
        drop(store);

        let store = reopen(&dir);
        let log = store.requests();
        for r in &records {
            assert_eq!(log.get(&r.request_id).expect("get").as_ref(), Some(r));
        }
        let page = log.query(&RequestQuery::default()).expect("query");
        let mut newest_first = records.clone();
        newest_first.reverse();
        assert_eq!(page.records, newest_first);
        assert_eq!(page.next, None);
        assert!(!page.disabled);
        assert_eq!(log.stats().rows, records.len() as u64);
        assert_eq!(log.get("absent").expect("get"), None);
    }

    #[test]
    fn outcome_vocabulary_is_exactly_the_contracts() {
        let names: Vec<&str> = every_outcome().iter().map(Outcome::name).collect();
        assert_eq!(names, OUTCOMES);
        assert!(RequestFilter {
            outcome: Some("rejected".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        for name in OUTCOMES {
            RequestFilter {
                outcome: Some(name.into()),
                ..Default::default()
            }
            .validate()
            .expect(name);
        }
    }

    #[test]
    fn requests_table_holds_metadata_columns_only() {
        let (dir, _store) = open_store();
        let conn = Connection::open(dir.path().join("janus.db")).expect("raw open");
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('requests') ORDER BY cid")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        // One column per documented record field (outcome split into name
        // and qualifier), plus the insert order: nothing that could hold a
        // body, a credential, or presented identity material.
        assert_eq!(cols.join(","), COLUMNS.replace(' ', ""));
        let json = serde_json::to_value(rec("r", 1, Outcome::Forwarded)).expect("ser");
        for field in json.as_object().expect("object").keys() {
            assert!(cols.contains(field), "record field {field} has no column");
        }
    }

    #[tokio::test]
    async fn request_log_disabled_writes_nothing() {
        let (dir, store) = open_store();
        // Rows from an earlier, enabled run.
        write(store.requests(), [rec("old", 1_000, Outcome::Forwarded)]).await;
        drop(store);

        let disabled = RequestLogConfig {
            enabled: false,
            ..Default::default()
        };
        let store = Store::open(dir.path(), &disabled).expect("open");
        let log = store.requests();
        let mut tail = log.subscribe();
        write(log, [rec("new", 2_000, Outcome::Forwarded)]).await;
        assert_eq!(raw_count(&dir), 1, "nothing written while disabled");
        assert!(matches!(
            tail.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let page = log.query(&RequestQuery::default()).expect("query");
        assert!(page.disabled);
        assert!(page.records.is_empty() && page.next.is_none());
        assert_eq!(log.get("old").expect("get"), None);
        assert_eq!(log.get("new").expect("get"), None);
        // Bad input is still bad input.
        assert!(log
            .query(&RequestQuery {
                limit: Some(0),
                ..Default::default()
            })
            .is_err());
        let stats = log.stats();
        assert!(!stats.enabled);
        assert_eq!(stats.rows, 1);
        // Retention still ages the leftovers out.
        assert_eq!(log.prune(at(1_000 + 31 * 86_400)).expect("prune"), 1);
        assert_eq!(raw_count(&dir), 0);
    }

    #[tokio::test]
    async fn request_log_retention_deletes_old_rows() {
        let (dir, _) = open_store();
        let day = 86_400;
        let cfg = RequestLogConfig {
            enabled: true,
            retain: Duration::from_secs(day as u64),
        };
        let store = Store::open(dir.path(), &cfg).expect("open");
        let log = store.requests();
        let now = 10 * day;
        write(
            log,
            [
                rec("two-days", now - 2 * day, Outcome::Forwarded),
                rec("just-over", now - day - 1, Outcome::Forwarded),
                rec("exactly", now - day, Outcome::Forwarded),
                rec("hour", now - 3_600, Outcome::Forwarded),
            ],
        )
        .await;
        assert_eq!(log.stats().rows, 4);
        assert_eq!(log.prune(at(now)).expect("prune"), 2);
        assert_eq!(log.stats().rows, 2);
        assert_eq!(raw_count(&dir), 2);
        assert_eq!(query(log, RequestFilter::default()), ["hour", "exactly"]);
        assert_eq!(log.prune(at(now)).expect("prune"), 0, "idempotent");

        // The default retention is 30 days.
        let (dir, store) = open_store();
        assert_eq!(
            store.requests().retain(),
            Duration::from_secs(30 * day as u64)
        );
        write(
            store.requests(),
            [
                rec("31d", now - 31 * day, Outcome::Forwarded),
                rec("29d", now - 29 * day, Outcome::Forwarded),
            ],
        )
        .await;
        assert_eq!(store.requests().prune(at(now)).expect("prune"), 1);
        assert_eq!(raw_count(&dir), 1);
    }

    #[tokio::test]
    async fn cursor_pagination_stable() {
        let (_dir, store) = open_store();
        let log = store.requests();
        // Ten rows, several sharing a timestamp so insert order breaks ties.
        let original: Vec<RequestRecord> = (0..10)
            .map(|i| rec(&format!("o{i}"), 1_000 + i / 3, Outcome::Forwarded))
            .collect();
        write(log, original.clone()).await;

        let mut seen: Vec<String> = Vec::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            let page = log
                .query(&RequestQuery {
                    limit: Some(3),
                    before: before.clone(),
                    ..Default::default()
                })
                .expect("query");
            seen.extend(ids(&page).into_iter().map(str::to_owned));
            pages += 1;
            // Concurrent traffic between pages: newer rows, rows sharing
            // the cursor's timestamp, and late-written older rows.
            write(
                log,
                [
                    rec(&format!("new{pages}"), 5_000 + pages, Outcome::Forwarded),
                    rec(&format!("same{pages}"), 1_003, Outcome::Forwarded),
                    rec(&format!("late{pages}"), 500 + pages, Outcome::Forwarded),
                ],
            )
            .await;
            match page.next {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        let unique: HashSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "no row twice: {seen:?}");
        let originals: Vec<&str> = seen
            .iter()
            .map(String::as_str)
            .filter(|id| id.starts_with('o'))
            .collect();
        let expected: Vec<String> = original
            .iter()
            .rev()
            .map(|r| r.request_id.clone())
            .collect();
        assert_eq!(originals, expected, "every original row, in order");
        assert!(
            seen.iter().all(|id| !id.starts_with("new")),
            "rows newer than the first page never appear behind a cursor"
        );
        // Late rows older than the cursor are picked up, still in order.
        assert!(seen.iter().any(|id| id.starts_with("late")));
    }

    #[tokio::test]
    async fn filters_select_by_identity_upstream_outcome_credential_and_time() {
        let (_dir, store) = open_store();
        let log = store.requests();
        let records = vec![
            rec("a", 1_000, Outcome::Forwarded),
            RequestRecord {
                caller_identity: "sandbox-8".into(),
                upstream: Some(UpstreamKind::Anthropic),
                ..rec("b", 1_001, Outcome::Forwarded)
            },
            RequestRecord {
                credential_ref: Some("openai-backup".into()),
                ..rec(
                    "c",
                    1_002,
                    Outcome::Denied {
                        reason: "budget".into(),
                    },
                )
            },
            RequestRecord {
                upstream: None,
                caller_identity: String::new(),
                credential_ref: None,
                ..rec(
                    "d",
                    1_003,
                    Outcome::RejectedIdentity {
                        kind: RejectionKind::NoMaterial,
                    },
                )
            },
        ];
        write(log, records.clone()).await;
        let cases: Vec<(RequestFilter, Vec<&str>)> = vec![
            (RequestFilter::default(), vec!["d", "c", "b", "a"]),
            (
                RequestFilter {
                    identity: Some("sandbox-7".into()),
                    ..Default::default()
                },
                vec!["c", "a"],
            ),
            (
                RequestFilter {
                    upstream: Some(UpstreamKind::Anthropic),
                    ..Default::default()
                },
                vec!["b"],
            ),
            (
                RequestFilter {
                    outcome: Some("rejected_identity".into()),
                    ..Default::default()
                },
                vec!["d"],
            ),
            (
                RequestFilter {
                    credential: Some("openai-backup".into()),
                    ..Default::default()
                },
                vec!["c"],
            ),
            (
                RequestFilter {
                    since: Some(at(1_001)),
                    until: Some(at(1_003)),
                    ..Default::default()
                },
                vec!["c", "b"],
            ),
            (
                RequestFilter {
                    identity: Some("sandbox-7".into()),
                    outcome: Some("forwarded".into()),
                    ..Default::default()
                },
                vec!["a"],
            ),
        ];
        for (filter, expected) in cases {
            assert_eq!(query(log, filter.clone()), expected, "{filter:?}");
            // The live tail's predicate agrees with the SQL.
            let mut by_matches: Vec<&str> = records
                .iter()
                .filter(|r| filter.matches(r))
                .map(|r| r.request_id.as_str())
                .collect();
            by_matches.reverse();
            assert_eq!(by_matches, expected, "matches(): {filter:?}");
        }
    }

    #[tokio::test]
    async fn limit_defaults_to_100_and_is_capped_at_1000() {
        let (_dir, store) = open_store();
        let log = store.requests();
        write(
            log,
            (0..1_005).map(|i| rec(&format!("r{i}"), i, Outcome::Forwarded)),
        )
        .await;
        let page = log.query(&RequestQuery::default()).expect("query");
        assert_eq!(page.records.len(), DEFAULT_LIMIT as usize);
        assert_eq!(page.records[0].request_id, "r1004");
        assert!(page.next.is_some());
        let page = log
            .query(&RequestQuery {
                limit: Some(5_000),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(page.records.len(), MAX_LIMIT as usize);
        let rest = log
            .query(&RequestQuery {
                limit: Some(5_000),
                before: page.next,
                ..Default::default()
            })
            .expect("query");
        assert_eq!(ids(&rest), ["r4", "r3", "r2", "r1", "r0"]);
        assert_eq!(rest.next, None, "an exactly-full last page has no next");
    }

    #[tokio::test]
    async fn bad_cursor_limit_and_outcome_are_refused() {
        let (_dir, store) = open_store();
        let log = store.requests();
        write(log, [rec("a", 1_000, Outcome::Forwarded)]).await;
        let bad = [
            RequestQuery {
                before: Some("not a cursor!".into()),
                ..Default::default()
            },
            RequestQuery {
                before: Some(URL_SAFE_NO_PAD.encode("yesterday/3")),
                ..Default::default()
            },
            RequestQuery {
                before: Some(URL_SAFE_NO_PAD.encode(format!("{}/x", ts_to_sql(at(5))))),
                ..Default::default()
            },
            RequestQuery {
                limit: Some(0),
                ..Default::default()
            },
            RequestQuery {
                filter: RequestFilter {
                    outcome: Some("ok".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        for q in bad {
            assert!(
                matches!(log.query(&q), Err(StoreError::Invalid(_))),
                "{q:?}"
            );
        }
    }

    #[tokio::test]
    async fn tail_publishes_each_committed_record() {
        let (_dir, store) = open_store();
        let log = store.requests();
        let mut tail = log.subscribe();
        let records: Vec<RequestRecord> = every_outcome()
            .into_iter()
            .enumerate()
            .map(|(i, o)| rec(&format!("t{i}"), 1_000 + i as i64, o))
            .collect();
        write(log, records.clone()).await;
        for expected in &records {
            let got = tail.recv().await.expect("event");
            assert_eq!(&*got, expected);
        }
        assert!(matches!(
            tail.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        // A subscriber that joins later sees only what comes after.
        let mut late = log.subscribe();
        write(log, [rec("after", 3_000, Outcome::Forwarded)]).await;
        assert_eq!(late.recv().await.expect("event").request_id, "after");
    }
}
