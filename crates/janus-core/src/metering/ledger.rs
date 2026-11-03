//! The rules every shipped [`UsageStore`](crate::contract::metering::UsageStore)
//! applies the same way, so `MemoryUsageStore` and `SqliteUsageStore` differ
//! only in where counts and ledger entries live.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::contract::metering::Apply;

/// Decide one `apply` from the current count of each bound's key and the
/// bound's limit, in bound order: the first limited bound that `amount` would
/// take past its limit rejects the whole op with that bound's current count;
/// otherwise every bound gets `amount` added. A count that would overflow
/// `u64` exceeds any limit, and saturates where there is none.
pub(crate) fn evaluate(
    current: &[u64],
    limits: impl IntoIterator<Item = Option<u64>>,
    amount: u64,
) -> Apply {
    for (index, (&used, limit)) in current.iter().zip(limits).enumerate() {
        if let Some(limit) = limit {
            if used.checked_add(amount).is_none_or(|next| next > limit) {
                return Apply::Rejected { index, used };
            }
        }
    }
    Apply::Applied {
        used: current
            .iter()
            .map(|used| used.saturating_add(amount))
            .collect(),
    }
}

/// The wall clock in Unix seconds: when a store stamps a ledger entry and
/// sweeps expired windows.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Ledger entries stamped before this instant are older than `ttl` at `now`
/// and are dropped by `compact`.
pub(crate) fn ledger_cutoff(now: i64, ttl: Duration) -> i64 {
    now.saturating_sub(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX))
}

/// Whether a key retained until `retain_until` has expired at `now`.
pub(crate) fn key_expired(retain_until: Option<i64>, now: i64) -> bool {
    retain_until.is_some_and(|until| until < now)
}

/// When a store compacts itself. `compact` is what keeps the ledger
/// bounded by the traffic of one TTL (`docs/plugin-contract` "Metering
/// primitives"); a store that waited for its plugin to call it would grow
/// without bound in a plugin that never does. So every shipped store runs a
/// full compaction from its own write path once per a quarter of its ledger
/// TTL (at least a second apart): the ledger then never holds entries much
/// older than 1¼ TTLs. A plugin may still call `compact` on its own timer.
pub(crate) struct AutoCompact {
    /// The earliest Unix second the next compaction may run.
    next: AtomicI64,
    interval: i64,
}

impl AutoCompact {
    pub(crate) fn new(ledger_ttl: Duration, now: i64) -> Self {
        let interval = i64::try_from(ledger_ttl.as_secs() / 4)
            .unwrap_or(i64::MAX)
            .max(1);
        Self {
            next: AtomicI64::new(now.saturating_add(interval)),
            interval,
        }
    }

    /// Whether the caller should compact at `now`. At most one caller per
    /// interval is told yes, so concurrent writers never compact together.
    pub(crate) fn due(&self, now: i64) -> bool {
        let next = self.next.load(Ordering::Relaxed);
        now >= next
            && self
                .next
                .compare_exchange(
                    next,
                    now.saturating_add(self.interval),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
    }

    /// Make the next write compact. For tests that cannot wait an interval.
    #[cfg(test)]
    pub(crate) fn expire(&self) {
        self.next.store(i64::MIN, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_rejects_the_first_bound_that_would_exceed() {
        assert_eq!(
            evaluate(&[2, 9, 9], [Some(10), Some(9), Some(5)], 1),
            Apply::Rejected { index: 1, used: 9 }
        );
        assert_eq!(
            evaluate(&[2, 8], [Some(10), Some(9)], 1),
            Apply::Applied { used: vec![3, 9] }
        );
    }

    #[test]
    fn evaluate_unlimited_bounds_never_reject_and_overflow_saturates() {
        assert_eq!(
            evaluate(&[u64::MAX - 1, 7], [None, None], 5),
            Apply::Applied {
                used: vec![u64::MAX, 12]
            }
        );
        assert_eq!(
            evaluate(&[u64::MAX - 1], [Some(u64::MAX)], 5),
            Apply::Rejected {
                index: 0,
                used: u64::MAX - 1
            }
        );
    }

    #[test]
    fn ledger_cutoff_and_key_expiry() {
        assert_eq!(ledger_cutoff(10_000, Duration::from_secs(3600)), 6_400);
        assert_eq!(ledger_cutoff(i64::MIN, Duration::from_secs(1)), i64::MIN);
        assert!(key_expired(Some(99), 100));
        assert!(!key_expired(Some(100), 100));
        assert!(!key_expired(None, i64::MAX));
        assert!(unix_now() > 1_700_000_000);
    }

    #[test]
    fn auto_compact_is_due_once_per_quarter_ttl() {
        let auto = AutoCompact::new(Duration::from_secs(3600), 1_000);
        assert!(!auto.due(1_000));
        assert!(!auto.due(1_899));
        assert!(auto.due(1_900));
        assert!(!auto.due(1_900), "one caller per interval");
        assert!(!auto.due(2_799));
        assert!(auto.due(2_800));
        let tiny = AutoCompact::new(Duration::ZERO, 0);
        assert!(!tiny.due(0));
        assert!(tiny.due(1), "at least a second apart");
    }
}
