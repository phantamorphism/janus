//! Counts the settlement work still in flight (request-log submits, relay
//! outcome waits) so shutdown can wait for it before the store's final
//! flush.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    live: AtomicUsize,
    idle: Notify,
}

/// Hands out [`InFlight`] tokens and waits for all of them to drop.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tracker {
    inner: Arc<Inner>,
}

/// Held by one piece of settlement work for as long as it runs.
#[derive(Debug)]
pub(crate) struct InFlight {
    inner: Arc<Inner>,
}

impl Tracker {
    pub(crate) fn token(&self) -> InFlight {
        self.inner.live.fetch_add(1, Ordering::AcqRel);
        InFlight {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Resolves once no token is live.
    pub(crate) async fn idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            // Register before checking, so a token dropped in between
            // still wakes this waiter.
            notified.as_mut().enable();
            if self.inner.live.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.inner.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.idle.notify_waiters();
        }
    }
}
