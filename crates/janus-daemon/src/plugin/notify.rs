//! The hand-off queue behind `PluginClient::notify`, shared by both client
//! implementations.
//!
//! `notify` must return immediately and never surface a failure to the
//! request path (`docs/plugin-contract#plugin-client`). Events go into a
//! bounded queue drained by one background task per client, which keeps
//! them in order. When the queue is full — the plugin is slow, stalled or
//! unreachable for longer than the queue absorbs — the *new* event is
//! dropped and counted, and a warning is logged at each power-of-two total.
//! A dropped settlement event leaves its phase-1 charge unrefunded and its
//! phase-2 cost unrecorded: the accepted limitation of a budget settled
//! over an asynchronous channel (`docs/plugin-contract#guaranteed-events`,
//! issue #1).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use janus_core::contract::plugin::NotifyEvent;
use tokio::sync::mpsc;

/// How many events a client buffers for its plugin before dropping new ones.
pub const NOTIFY_QUEUE_CAPACITY: usize = 1024;

/// The sending half: what `notify` pushes into.
#[derive(Debug)]
pub(crate) struct NotifyQueue {
    tx: mpsc::Sender<NotifyEvent>,
    dropped: Arc<AtomicU64>,
}

impl NotifyQueue {
    pub(crate) fn new(capacity: usize) -> (Self, mpsc::Receiver<NotifyEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            NotifyQueue {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Enqueue without waiting. A full or closed queue drops the event.
    pub(crate) fn push(&self, event: NotifyEvent) {
        let reason = match self.tx.try_send(event) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(ev)) => ("queue full", ev),
            Err(mpsc::error::TrySendError::Closed(ev)) => ("delivery task stopped", ev),
        };
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if total.is_power_of_two() {
            tracing::warn!(
                event_type = %reason.1.event_type,
                request_id = reason.1.request_id.as_deref().unwrap_or(""),
                dropped_total = total,
                "plugin Notify event dropped: {}",
                reason.0
            );
        }
    }

    /// Events dropped since the client was built.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(n: usize) -> NotifyEvent {
        NotifyEvent {
            event_type: "test".into(),
            request_id: Some(format!("req-{n}")),
            correlation_id: None,
            payload_json: "{}".into(),
        }
    }

    #[test]
    fn full_queue_drops_newest_and_counts() {
        let (queue, mut rx) = NotifyQueue::new(2);
        for n in 0..5 {
            queue.push(event(n));
        }
        assert_eq!(queue.dropped(), 3);
        assert_eq!(
            rx.try_recv().expect("first").request_id.as_deref(),
            Some("req-0")
        );
        assert_eq!(
            rx.try_recv().expect("second").request_id.as_deref(),
            Some("req-1")
        );
        assert!(rx.try_recv().is_err());
        drop(rx);
        queue.push(event(9));
        assert_eq!(queue.dropped(), 4, "a closed queue drops too");
    }
}
