//! When a credential's next login flow may start: a debounce on problem
//! reports and an exponential backoff on failed flows. Pure; the keeper
//! passes the clock in.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Schedule {
    debounce: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// When the first unanswered problem report arrived.
    pending_since: Option<Instant>,
    /// Failed flows since the last success.
    failures: u32,
    /// No automatic flow before this.
    not_before: Option<Instant>,
}

impl Schedule {
    pub fn new(debounce: Duration, backoff_min: Duration, backoff_max: Duration) -> Self {
        Schedule {
            debounce,
            backoff_min,
            backoff_max,
            pending_since: None,
            failures: 0,
            not_before: None,
        }
    }

    /// A problem was reported. Reports while one is pending do not move
    /// the debounce: the first report sets when the flow may start.
    pub fn report(&mut self, now: Instant) {
        self.pending_since.get_or_insert(now);
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// When the pending flow may start; `None` when nothing is pending.
    pub fn due(&self) -> Option<Instant> {
        let since = self.pending_since?;
        let debounced = since + self.debounce;
        Some(match self.not_before {
            Some(at) if at > debounced => at,
            _ => debounced,
        })
    }

    /// The flow that answers the pending report is starting.
    pub fn started(&mut self) {
        self.pending_since = None;
    }

    /// The credential is usable again (a flow succeeded, or a check found
    /// it healthy): forget failures and any pending report.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.not_before = None;
        self.pending_since = None;
    }

    /// The operator cancelled a flow: no automatic flow for `pause`, and
    /// none at all until the next problem report. The cancel is not a
    /// failure, so the backoff does not grow.
    pub fn paused(&mut self, now: Instant, pause: Duration) {
        self.pending_since = None;
        self.not_before = Some(now + pause);
    }

    /// A flow failed or timed out: the next automatic one
    /// waits `backoff_min * 2^(failures-1)`, capped at `backoff_max`.
    /// Returns that wait.
    pub fn failed(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32
            .checked_shl(self.failures.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let wait = self
            .backoff_min
            .checked_mul(factor)
            .map_or(self.backoff_max, |w| w.min(self.backoff_max));
        self.not_before = Some(now + wait);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn schedule() -> Schedule {
        Schedule::new(10 * S, 60 * S, 600 * S)
    }

    #[test]
    fn a_burst_of_reports_is_debounced_from_the_first() {
        let t0 = Instant::now();
        let mut s = schedule();
        assert_eq!(s.due(), None);
        s.report(t0);
        s.report(t0 + 5 * S);
        s.report(t0 + 9 * S);
        assert_eq!(s.due(), Some(t0 + 10 * S));
        s.started();
        assert_eq!(s.due(), None);
    }

    #[test]
    fn failures_back_off_exponentially_up_to_the_cap() {
        let t0 = Instant::now();
        let mut s = schedule();
        let waits: Vec<u64> = (0..6).map(|_| s.failed(t0).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 480, 600, 600]);
        // A report after the last failure waits out the backoff, not the
        // debounce.
        s.report(t0);
        assert_eq!(s.due(), Some(t0 + 600 * S));
        for _ in 0..100 {
            s.failed(t0);
        }
        assert_eq!(s.failed(t0), 600 * S);
    }

    #[test]
    fn success_resets_the_backoff() {
        let t0 = Instant::now();
        let mut s = schedule();
        s.failed(t0);
        s.started();
        s.succeeded();
        assert_eq!(s.failures(), 0);
        s.report(t0 + 2 * S);
        assert_eq!(s.due(), Some(t0 + 12 * S));
    }

    #[test]
    fn a_pause_drops_the_pending_report_and_holds_the_next_one() {
        let t0 = Instant::now();
        let mut s = schedule();
        s.failed(t0);
        s.report(t0);
        s.paused(t0, 3600 * S);
        assert_eq!(s.due(), None, "no automatic restart after a cancel");
        assert_eq!(s.failures(), 1, "a cancel is not a failure");
        s.report(t0 + S);
        assert_eq!(s.due(), Some(t0 + 3600 * S));
    }
}
