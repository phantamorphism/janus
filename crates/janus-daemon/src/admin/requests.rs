//! `/v1/requests` (`docs/admin-surface#requests-endpoint`): the request log,
//! queried and tailed.
//!
//! The live tail is `text/event-stream`: a `ping` as soon as the
//! subscription is in place and every [`PING_INTERVAL`] after, one
//! `request` event (the record as JSON) per committed record matching the
//! filter, and a `lagged` event (`{"missed": n}`) if this subscriber fell so
//! far behind that records were dropped for it. Events carry no `id`: the
//! tail is live only, and a client that reconnects closes the gap with a
//! query. With the request log disabled the stream is a single `disabled`
//! event. The stream ends when the daemon shuts down.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::Json;
use chrono::{DateTime, Utc};
use janus_core::contract::request_log::RequestRecord;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::api::{blocking, AdminState};
use super::error::{ApiError, PathParam, Query};
use crate::store::{RequestFilter, RequestQuery};

/// How often the live tail sends a `ping`.
pub const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Events buffered for a slow SSE client before the tail waits for it.
const SSE_BUFFER: usize = 64;
/// Live tails open at once, across every listener; one more is refused with
/// `503 too_many_streams` until one closes.
pub const MAX_TAILS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryParams {
    identity: Option<String>,
    upstream: Option<String>,
    outcome: Option<String>,
    credential: Option<String>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    limit: Option<u32>,
    before: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Page {
    requests: Vec<RequestRecord>,
    /// The `before` cursor of the next (older) page; `null` on the last.
    next: Option<String>,
    /// `request_log.enabled = false`.
    disabled: bool,
}

pub(crate) async fn query(
    State(state): State<AdminState>,
    Query(p): Query<QueryParams>,
) -> Result<Json<Page>, ApiError> {
    let q = RequestQuery {
        filter: RequestFilter {
            identity: p.identity,
            upstream: p.upstream,
            outcome: p.outcome,
            credential: p.credential,
            since: p.since,
            until: p.until,
        },
        limit: p.limit,
        before: p.before,
    };
    let log = state.deps.requests.clone();
    let page = blocking(move || log.query(&q)).await?;
    Ok(Json(Page {
        requests: page.records,
        next: page.next,
        disabled: page.disabled,
    }))
}

pub(crate) async fn show(
    State(state): State<AdminState>,
    PathParam(request_id): PathParam,
) -> Result<Json<RequestRecord>, ApiError> {
    let log = state.deps.requests.clone();
    let id = request_id.clone();
    match blocking(move || log.get(&id)).await? {
        Some(record) => Ok(Json(record)),
        None if !state.deps.requests.enabled() => Err(ApiError::not_found(
            "request_log_disabled",
            "the request log is disabled (request_log.enabled = false)",
        )),
        None => Err(ApiError::not_found(
            "request_not_found",
            format!("no request {request_id:?} in the log"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamParams {
    identity: Option<String>,
    upstream: Option<String>,
    outcome: Option<String>,
}

pub(crate) async fn stream(
    State(state): State<AdminState>,
    Query(p): Query<StreamParams>,
) -> Result<Sse<ReceiverStream<Result<Event, Infallible>>>, ApiError> {
    let filter = RequestFilter {
        identity: p.identity,
        upstream: p.upstream,
        outcome: p.outcome,
        ..RequestFilter::default()
    };
    filter.validate()?;
    let (tx, rx) = mpsc::channel(SSE_BUFFER);
    if state.deps.requests.enabled() {
        let permit = Arc::clone(&state.tails).try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too_many_streams",
                format!("{MAX_TAILS} live tails are already open; close one and retry"),
            )
        })?;
        let records = state.deps.requests.subscribe();
        tokio::spawn(async move {
            tail(records, filter, tx, state.cancel.clone()).await;
            drop(permit);
        });
    } else {
        // Capacity is at least one, so this cannot fail.
        let _ = tx.try_send(Ok(Event::default()
            .event("disabled")
            .data(r#"{"disabled":true}"#)));
    }
    Ok(Sse::new(ReceiverStream::new(rx)))
}

/// Feed one SSE client until it goes away or the daemon stops.
async fn tail(
    mut records: broadcast::Receiver<Arc<RequestRecord>>,
    filter: RequestFilter,
    tx: mpsc::Sender<Result<Event, Infallible>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    // The first tick is immediate: the opening ping.
    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => return,
            () = tx.closed() => return,
            _ = ping.tick() => ping_event(),
            received = records.recv() => match received {
                Ok(record) if filter.matches(&record) => match Event::default().event("request").json_data(&*record) {
                    Ok(event) => event,
                    Err(e) => {
                        tracing::error!(error = %e, "request record does not serialize");
                        continue;
                    }
                },
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(missed)) => Event::default()
                    .event("lagged")
                    .data(format!(r#"{{"missed":{missed}}}"#)),
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        // A client that stops reading stalls only its own tail, and only
        // until it goes away or the daemon stops.
        tokio::select! {
            () = cancel.cancelled() => return,
            sent = tx.send(Ok(event)) => if sent.is_err() { return },
        }
    }
}

fn ping_event() -> Event {
    Event::default()
        .event("ping")
        .data(format!(r#"{{"at":"{}"}}"#, Utc::now().to_rfc3339()))
}
