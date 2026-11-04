//! `janus log query|tail|show` (`docs/admin-surface#requests-endpoint`).
//!
//! `tail` follows `GET /v1/requests/stream`. The daemon's tail is live only
//! (events carry no SSE id), so when the stream drops — a daemon restart, a
//! network blip — the CLI reconnects with backoff and closes the gap with a
//! `GET /v1/requests?since=` query over the outage, printing only records
//! it has not printed already.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use http::Method;
use janus_core::contract::request_log::{Outcome, RequestRecord};
use serde::Deserialize;
use serde_json::Value;

use super::{upstream_parser, Ctx};
use crate::client::{api_path, decode, Admin};
use crate::error::CliError;
use crate::render::{self, fields, Table};
use crate::sse;

#[derive(Subcommand)]
pub enum LogCommand {
    /// Query the request log, newest first, one page at a time.
    Query {
        #[command(flatten)]
        filter: StreamFilter,
        /// Only records that used this credential ref.
        #[arg(long, value_name = "REF")]
        credential: Option<String>,
        /// Records received at or after this time: RFC 3339, or a duration
        /// meaning that long ago (15m, 2h, 7d).
        #[arg(long, value_name = "TIME", value_parser = parse_time)]
        since: Option<DateTime<Utc>>,
        /// Records received before this time (same forms as --since).
        #[arg(long, value_name = "TIME", value_parser = parse_time)]
        until: Option<DateTime<Utc>>,
        /// Page size (the daemon defaults to 100 and caps at 1000).
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// The cursor printed after the previous page ("next"), for the
        /// next, older page.
        #[arg(long, value_name = "CURSOR")]
        before: Option<String>,
    },
    /// Follow the log live until interrupted, reconnecting if the stream drops.
    Tail {
        #[command(flatten)]
        filter: StreamFilter,
    },
    /// One record.
    Show { request_id: String },
}

/// The filters the live tail supports; `query` takes these and more.
#[derive(Args, Clone)]
pub struct StreamFilter {
    /// Only records whose verified caller identity is this.
    #[arg(long, value_name = "STRING")]
    identity: Option<String>,
    #[arg(long, value_name = "NAME", value_parser = upstream_parser())]
    upstream: Option<String>,
    #[arg(long, value_name = "OUTCOME", value_parser = clap::builder::PossibleValuesParser::new(janus_daemon::store::OUTCOMES))]
    outcome: Option<String>,
}

impl StreamFilter {
    fn query(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("identity", self.identity.clone()),
            ("upstream", self.upstream.clone()),
            ("outcome", self.outcome.clone()),
        ]
    }
}

/// RFC 3339, or a duration back from now.
fn parse_time(text: &str) -> Result<DateTime<Utc>, String> {
    parse_time_at(text, Utc::now())
}

fn parse_time_at(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.with_timezone(&Utc));
    }
    let ago = humantime::parse_duration(text).map_err(|_| {
        format!("{text:?} is neither an RFC 3339 time nor a duration such as 15m or 7d")
    })?;
    chrono::Duration::from_std(ago)
        .ok()
        .and_then(|d| now.checked_sub_signed(d))
        .ok_or_else(|| format!("{text:?} reaches too far back"))
}

#[derive(Deserialize)]
struct Page {
    requests: Vec<RequestRecord>,
    next: Option<String>,
    disabled: bool,
}

const DISABLED: &str = "the request log is disabled (request_log.enabled = false)";

pub async fn run(ctx: &Ctx, command: &LogCommand) -> Result<(), CliError> {
    match command {
        LogCommand::Query {
            filter,
            credential,
            since,
            until,
            limit,
            before,
        } => {
            let mut query = filter.query();
            query.extend([
                ("credential", credential.clone()),
                ("since", since.map(|t| t.to_rfc3339())),
                ("until", until.map(|t| t.to_rfc3339())),
                ("limit", limit.map(|n| n.to_string())),
                ("before", before.clone()),
            ]);
            let path = api_path(&["v1", "requests"], &query)?;
            let (raw, page): (Value, Page) = ctx.admin()?.json(Method::GET, &path, None).await?;
            if ctx.json {
                return render::json(&raw);
            }
            if page.disabled {
                render::note(DISABLED);
                return Ok(());
            }
            render::out(&table(&page.requests))?;
            if let Some(next) = &page.next {
                render::note(&format!("more: janus log query ... --before {next}"));
            }
            Ok(())
        }
        LogCommand::Show { request_id } => {
            let path = api_path(&["v1", "requests", request_id], &[])?;
            let (raw, record): (Value, RequestRecord) =
                ctx.admin()?.json(Method::GET, &path, None).await?;
            if ctx.json {
                render::json(&raw)
            } else {
                render::out(&detail(&record))
            }
        }
        LogCommand::Tail { filter } => {
            Tail::new(ctx.admin()?, filter.clone(), ctx.json)
                .run()
                .await
        }
    }
}

fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::RejectedIdentity { kind } => format!("{}({})", o.name(), render::name(kind)),
        Outcome::Denied { reason } => format!("{}({reason})", o.name()),
        Outcome::PluginFailed { class } => format!("{}({})", o.name(), render::name(class)),
        Outcome::Abandoned { stage } => format!("{}({stage})", o.name()),
        _ => o.name().to_owned(),
    }
}

const HEADER: [&str; 9] = [
    "RECEIVED",
    "REQUEST ID",
    "UPSTREAM",
    "METHOD",
    "PATH",
    "IDENTITY",
    "OUTCOME",
    "STATUS",
    "MS",
];

fn row(r: &RequestRecord) -> Vec<String> {
    vec![
        render::time(Some(r.received_at)),
        r.request_id.clone(),
        r.upstream.map_or("-".to_owned(), |u| u.to_string()),
        r.method.clone(),
        r.path.clone(),
        render::text(Some(&r.caller_identity)),
        outcome_text(&r.outcome),
        r.status.map_or("-".to_owned(), |s| s.to_string()),
        r.duration_ms.to_string(),
    ]
}

fn table(records: &[RequestRecord]) -> String {
    if records.is_empty() {
        return "no matching requests\n".to_owned();
    }
    let mut t = Table::new(&HEADER);
    for r in records {
        t.row(row(r));
    }
    t.render()
}

fn detail(r: &RequestRecord) -> String {
    fields(&[
        ("request id", r.request_id.clone()),
        ("received", render::time(Some(r.received_at))),
        ("duration", format!("{} ms", r.duration_ms)),
        (
            "upstream",
            r.upstream.map_or("-".to_owned(), |u| u.to_string()),
        ),
        ("route", render::text(r.route.as_deref())),
        ("request", format!("{} {}", r.method, r.path)),
        (
            "binding",
            r.binding
                .as_ref()
                .map_or("-".to_owned(), ToString::to_string),
        ),
        ("identity", render::text(Some(&r.caller_identity))),
        (
            "claimed identity",
            render::text(r.claimed_identity.as_deref()),
        ),
        ("correlation id", render::text(r.correlation_id.as_deref())),
        ("outcome", outcome_text(&r.outcome)),
        ("credential", render::text(r.credential_ref.as_deref())),
        ("status", r.status.map_or("-".to_owned(), |s| s.to_string())),
        (
            "usage",
            r.usage.as_ref().map_or("-".to_owned(), Value::to_string),
        ),
        ("error", render::text(r.error.as_deref())),
    ])
}

/// First wait before reconnecting; doubled per failed attempt up to
/// [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How far before the moment the stream dropped the gap query reaches: a
/// record's `received_at` is when the request arrived, and it is committed
/// only after the response finished, so a long request that was in flight
/// at the drop has an earlier timestamp than the drop itself.
const GAP_LOOKBACK: Duration = Duration::from_secs(300);
/// How long the stream may be silent before it is treated as dead. The
/// daemon pings every 10 seconds, so this is more than three missed pings;
/// it catches a half-open connection that would otherwise hang forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(35);
/// Request ids remembered for de-duplication between the gap query and the
/// stream.
const SEEN_CAP: usize = 10_000;

/// Recently printed request ids, bounded.
struct Seen {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Seen {
    fn new() -> Self {
        Seen {
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    /// `true` the first time `id` is offered.
    fn insert(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// How one connection of the tail ended.
enum Ended {
    /// The stream was lost after it had been established; reconnect.
    Dropped(String),
    /// The stream could not be opened.
    Refused(CliError),
}

/// A page of the gap query, records kept as the daemon sent them.
#[derive(Deserialize)]
struct RawPage {
    requests: Vec<Value>,
    next: Option<String>,
    disabled: bool,
}

struct Tail {
    admin: Admin,
    filter: StreamFilter,
    json: bool,
    started_at: DateTime<Utc>,
    seen: Seen,
    /// Whether a stream has ever been established.
    connected_once: bool,
    /// When the stream first dropped, while the gap since is not closed.
    gap_from: Option<DateTime<Utc>>,
    /// Silence after which the stream counts as lost.
    idle_timeout: Duration,
}

impl Tail {
    fn new(admin: Admin, filter: StreamFilter, json: bool) -> Self {
        Tail {
            admin,
            filter,
            json,
            started_at: Utc::now(),
            seen: Seen::new(),
            connected_once: false,
            gap_from: None,
            idle_timeout: IDLE_TIMEOUT,
        }
    }

    async fn run(mut self) -> Result<(), CliError> {
        let path = api_path(&["v1", "requests", "stream"], &self.filter.query())?;
        let mut backoff = FIRST_BACKOFF;
        loop {
            match self.follow(&path).await? {
                // The first attempt failing is an error to report, not an
                // outage to wait out.
                Ended::Refused(e) if !self.connected_once => return Err(e),
                // An API refusal (a bad filter, a bad token) will not fix
                // itself; anything else is the outage still going on.
                Ended::Refused(
                    e @ CliError::Api {
                        status: 400..=499, ..
                    },
                ) => return Err(e),
                Ended::Refused(e) => render::note(&format!("tail: still disconnected: {e}")),
                Ended::Dropped(why) => {
                    self.gap_from.get_or_insert_with(Utc::now);
                    backoff = FIRST_BACKOFF;
                    render::note(&format!("tail: stream lost ({why}); reconnecting"));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// One connection: open, wait for the opening ping, close any gap, and
    /// print until the stream ends.
    async fn follow(&mut self, path: &str) -> Result<Ended, CliError> {
        let mut body = match self.admin.stream(path).await {
            Ok(body) => body,
            Err(e) => return Ok(Ended::Refused(e)),
        };
        let mut decoder = sse::Decoder::default();
        let mut subscribed = false;
        loop {
            let chunk = match tokio::time::timeout(self.idle_timeout, body.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => {
                    return Ok(Ended::Dropped("the daemon closed the stream".to_owned()))
                }
                Ok(Err(e)) => return Ok(Ended::Dropped(e)),
                Err(_) => {
                    return Ok(Ended::Dropped(format!(
                        "nothing received for {}s",
                        self.idle_timeout.as_secs()
                    )))
                }
            };
            for event in decoder.feed(&chunk) {
                match event.event.as_str() {
                    "ping" if !subscribed => {
                        // The daemon sends its first ping once the
                        // subscription is in place: from here on nothing
                        // committed is missed.
                        subscribed = true;
                        if self.connected_once {
                            render::note("tail: reconnected");
                        } else {
                            render::note("tail: following the request log (Ctrl-C to stop)");
                        }
                        self.connected_once = true;
                        if let Some(from) = self.gap_from {
                            match self.close_gap(from).await {
                                Ok(()) => self.gap_from = None,
                                Err(CliError::Unreachable(why)) => return Ok(Ended::Dropped(why)),
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    "ping" => {}
                    "request" => {
                        let value: Value = serde_json::from_str(&event.data).map_err(|e| {
                            CliError::failed(
                                "bad_response",
                                format!("the tail sent a request event that is not JSON: {e}"),
                            )
                        })?;
                        self.print(&value)?;
                    }
                    "lagged" => {
                        let missed = serde_json::from_str::<Value>(&event.data)
                            .ok()
                            .and_then(|v| v.get("missed").and_then(Value::as_u64));
                        render::note(&format!(
                            "tail: fell behind; the daemon dropped {} record(s) for this tail (see janus log query)",
                            missed.map_or("some".to_owned(), |n| n.to_string())
                        ));
                    }
                    "disabled" => return Err(CliError::failed("request_log_disabled", DISABLED)),
                    // An event type this CLI does not know: a newer daemon.
                    _ => {}
                }
            }
        }
    }

    /// Print what was committed while the stream was down, oldest first.
    async fn close_gap(&mut self, dropped_at: DateTime<Utc>) -> Result<(), CliError> {
        let lookback = chrono::Duration::from_std(GAP_LOOKBACK).unwrap_or(chrono::Duration::zero());
        let since = (dropped_at - lookback).max(self.started_at);
        let mut records = Vec::new();
        let mut before = None;
        loop {
            let mut query = self.filter.query();
            query.extend([
                ("since", Some(since.to_rfc3339())),
                ("limit", Some(janus_daemon::store::MAX_LIMIT.to_string())),
                ("before", before.take()),
            ]);
            let path = api_path(&["v1", "requests"], &query)?;
            let (_, page): (Value, RawPage) = self.admin.json(Method::GET, &path, None).await?;
            if page.disabled {
                return Err(CliError::failed("request_log_disabled", DISABLED));
            }
            records.extend(page.requests);
            match page.next {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        // Pages are newest first.
        for record in records.iter().rev() {
            self.print(record)?;
        }
        Ok(())
    }

    /// Print a record not printed before: one JSON line, or one line of
    /// the query table's columns.
    fn print(&mut self, value: &Value) -> Result<(), CliError> {
        let record: RequestRecord = decode("the request log tail", value)?;
        if !self.seen.insert(&record.request_id) {
            return Ok(());
        }
        if self.json {
            render::json_line(value)
        } else {
            render::out(&format!("{}\n", row(&record).join("  ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_rfc3339_or_a_duration_ago() {
        let now = DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_time_at("2026-11-01T00:00:00+01:00", now).unwrap(),
            DateTime::parse_from_rfc3339("2026-10-31T23:00:00Z").unwrap()
        );
        assert_eq!(
            parse_time_at("90m", now).unwrap(),
            DateTime::parse_from_rfc3339("2026-11-03T10:30:00Z").unwrap()
        );
        assert!(parse_time_at("yesterday", now).is_err());
    }

    #[test]
    fn seen_is_bounded_and_deduplicates() {
        let mut seen = Seen::new();
        assert!(seen.insert("a"));
        assert!(!seen.insert("a"));
        for i in 0..SEEN_CAP {
            seen.insert(&i.to_string());
        }
        assert!(seen.insert("a"), "the oldest id was forgotten");
        assert_eq!(seen.order.len(), SEEN_CAP);
    }

    #[tokio::test]
    async fn a_silent_stream_is_dropped_after_the_idle_timeout() {
        use crate::client::{Address, HttpOptions};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A server that opens the event stream, sends the opening ping, and
        // then goes silent without closing: a half-open connection.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            let ping = b"event: ping\ndata: {}\n\n";
            socket
                .write_all(format!("{:x}\r\n", ping.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(ping).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });

        let admin = Admin::new(
            Address::parse(&format!("http://{addr}")).unwrap(),
            HttpOptions {
                ca_file: None,
                token: None,
            },
        )
        .unwrap();
        let mut tail = Tail::new(
            admin,
            StreamFilter {
                identity: None,
                upstream: None,
                outcome: None,
            },
            true,
        );
        tail.idle_timeout = Duration::from_millis(200);
        let ended =
            tokio::time::timeout(Duration::from_secs(10), tail.follow("/v1/requests/stream"))
                .await
                .expect("follow returns instead of hanging")
                .expect("not a hard error");
        match ended {
            Ended::Dropped(why) => assert!(why.contains("nothing received"), "{why}"),
            Ended::Refused(e) => panic!("expected a dropped stream, got {e}"),
        }
        assert!(tail.connected_once);
    }
}
