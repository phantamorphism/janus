//! The metering contract (`docs/plugin-contract#metering-primitives` and
//! `#usage-stores`): the [`UsageStore`] trait, and the data types `Meter`
//! (in [`crate::metering`]) is built from and returns.
//!
//! `Meter<S: UsageStore>` itself is implementation, not contract; its
//! documented signature is `new` / `new_allowing_volatile_total` / `charge` /
//! `check` / `record` / `refund` / `settle` over the types below.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

/// A store's default ledger TTL: an unsettled, unreleased op, and every
/// `Settle`-phase entry, is dropped by `compact` once older than this. Has to
/// exceed the longest metered call plus any delay delivering its settlement
/// event. Store configuration may override it.
pub const DEFAULT_LEDGER_TTL: Duration = Duration::from_secs(60 * 60);

/// The result of `Meter::charge` or `Meter::check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    /// Of the quota nearest exhaustion.
    Allowed { used: u64, limit: u64 },
    /// Of the quota that was exceeded. Answer `deny_reason`.
    Exceeded { used: u64, limit: u64 },
}

impl Charge {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Charge::Allowed { .. })
    }
}

/// One limit on one meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    pub limit: u64,
    pub window: Window,
}

/// How a quota's count resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// Resets on a period boundary.
    Fixed { period: Duration },
    /// Never resets. Refused on a `Volatile` store unless the author opts in.
    Total,
}

/// One counter: an identity's usage of one meter in one window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeterKey {
    /// `caller_identity`.
    pub identity: String,
    /// The plugin's own dimension name (`"requests"`, `"input_tokens"`, ...).
    pub meter: String,
    /// The quota's period in whole seconds (`Window::Fixed { period }`);
    /// `0` for `Total`. Part of the key because two quotas on one meter
    /// (say hourly and daily) can share a `window_start` — every daily
    /// boundary is also an hourly one — and must still count separately.
    pub period_secs: u64,
    /// Unix seconds at which the window this count belongs to began,
    /// derived from the quota's [`Window`] (for `Fixed`, a multiple of the
    /// period); `0` for `Total`.
    pub window_start: i64,
}

/// One key an [`UsageStore::apply`] adds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound<'a> {
    pub key: &'a MeterKey,
    /// `None`: unconditional (a phase-2 record).
    pub limit: Option<u64>,
    /// `None`: keep forever (a `Total` window). Unix seconds otherwise.
    pub retain_until: Option<i64>,
}

/// The idempotency key of a store operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpId {
    pub identity: String,
    pub meter: String,
    pub request_id: String,
    pub phase: Phase,
}

/// Which half of two-phase metering an op belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// `charge`: checked against limits, reversible by `refund`.
    Reserve,
    /// `record`: unconditional.
    Settle,
}

impl Phase {
    /// Stable spelling for stores that persist the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Reserve => "reserve",
            Phase::Settle => "settle",
        }
    }
}

/// The result of [`UsageStore::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Apply {
    /// New totals, one per bound, in bound order.
    Applied { used: Vec<u64> },
    /// The first bound that would exceed its limit; nothing changed.
    Rejected { index: usize, used: u64 },
}

/// Whether a store's counts survive a restart of the process holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Volatile,
    Durable,
}

/// A metering failure.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// `Meter::new` refuses a `Window::Total` quota on a `Volatile` store.
    #[error("meter {meter:?} has a Total quota, which a volatile store cannot hold; use a durable store or Meter::new_allowing_volatile_total")]
    VolatileTotal { meter: String },
    /// `Meter::new` refuses a quota it cannot enforce as written (for
    /// example a `Fixed` period that is not a positive whole number of
    /// seconds); `reason` says which rule it broke.
    #[error("meter {meter:?} has an invalid quota: {reason}")]
    InvalidQuota { meter: String, reason: String },
    /// An operation named a meter with no configured quotas.
    #[error("no quotas configured for meter {0:?}")]
    UnknownMeter(String),
    /// The store's file is held by another process (`SqliteUsageStore`
    /// refuses to share one).
    #[error("usage store is locked by another process: {0}")]
    Locked(String),
    /// Any other failure of the backing store.
    #[error("usage store backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Where counts live: one atomic, idempotent, ledgered operation
/// (`docs/plugin-contract#usage-stores`).
#[async_trait]
pub trait UsageStore: Send + Sync {
    /// Add `amount` to every key in `bounds`, all or nothing, provided no
    /// bound with a `limit` would exceed it. All bounds in one call share one
    /// (identity, meter) and differ only in window. Idempotent on `op`: a
    /// repeat returns the original result and changes nothing. The store
    /// remembers which keys `op` touched so that `release` can undo exactly
    /// those.
    async fn apply(
        &self,
        op: &OpId,
        bounds: &[Bound<'_>],
        amount: u64,
    ) -> Result<Apply, UsageError>;

    /// Undo `op` against the keys it touched. `false` if `op` is unknown,
    /// already released, or already settled.
    async fn release(&self, op: &OpId) -> Result<bool, UsageError>;

    /// `op` is final; the store may drop its ledger entry.
    async fn settle(&self, op: &OpId) -> Result<(), UsageError>;

    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError>;

    /// Drop keys whose `retain_until` has passed and ledger entries older
    /// than the ledger TTL. Returns how many were dropped.
    async fn compact(&self, now: i64) -> Result<u64, UsageError>;

    fn durability(&self) -> Durability;
}

#[async_trait]
impl<S: UsageStore + ?Sized> UsageStore for Arc<S> {
    async fn apply(
        &self,
        op: &OpId,
        bounds: &[Bound<'_>],
        amount: u64,
    ) -> Result<Apply, UsageError> {
        (**self).apply(op, bounds, amount).await
    }

    async fn release(&self, op: &OpId) -> Result<bool, UsageError> {
        (**self).release(op).await
    }

    async fn settle(&self, op: &OpId) -> Result<(), UsageError> {
        (**self).settle(op).await
    }

    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError> {
        (**self).get(key).await
    }

    async fn compact(&self, now: i64) -> Result<u64, UsageError> {
        (**self).compact(now).await
    }

    fn durability(&self) -> Durability {
        (**self).durability()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records which trait method was reached, to prove the `Arc` impl
    /// forwards each call to the store it wraps.
    #[derive(Default)]
    struct Recording(Mutex<Vec<&'static str>>);

    impl Recording {
        fn hit(&self, name: &'static str) {
            self.0.lock().expect("lock").push(name);
        }
    }

    #[async_trait]
    impl UsageStore for Recording {
        async fn apply(
            &self,
            _op: &OpId,
            bounds: &[Bound<'_>],
            amount: u64,
        ) -> Result<Apply, UsageError> {
            self.hit("apply");
            Ok(Apply::Applied {
                used: bounds.iter().map(|_| amount).collect(),
            })
        }

        async fn release(&self, _op: &OpId) -> Result<bool, UsageError> {
            self.hit("release");
            Ok(true)
        }

        async fn settle(&self, _op: &OpId) -> Result<(), UsageError> {
            self.hit("settle");
            Ok(())
        }

        async fn get(&self, key: &MeterKey) -> Result<u64, UsageError> {
            self.hit("get");
            Err(UsageError::UnknownMeter(key.meter.clone()))
        }

        async fn compact(&self, now: i64) -> Result<u64, UsageError> {
            self.hit("compact");
            Ok(u64::try_from(now).unwrap_or(0))
        }

        fn durability(&self) -> Durability {
            Durability::Durable
        }
    }

    #[tokio::test]
    async fn arc_store_forwards_every_operation_and_its_result() {
        let inner = Arc::new(Recording::default());
        let store: Arc<dyn UsageStore> = inner.clone();
        let key = MeterKey {
            identity: "session-42".into(),
            meter: "requests".into(),
            period_secs: 3600,
            window_start: 1_700_000_000,
        };
        let op = OpId {
            identity: key.identity.clone(),
            meter: key.meter.clone(),
            request_id: "req-1".into(),
            phase: Phase::Reserve,
        };
        let bounds = [
            Bound {
                key: &key,
                limit: Some(10),
                retain_until: Some(1_700_003_600),
            },
            Bound {
                key: &key,
                limit: None,
                retain_until: None,
            },
        ];
        assert_eq!(
            UsageStore::apply(&store, &op, &bounds, 3)
                .await
                .expect("apply"),
            Apply::Applied { used: vec![3, 3] }
        );
        assert!(UsageStore::release(&store, &op).await.expect("release"));
        UsageStore::settle(&store, &op).await.expect("settle");
        assert!(matches!(
            UsageStore::get(&store, &key).await,
            Err(UsageError::UnknownMeter(m)) if m == "requests"
        ));
        assert_eq!(UsageStore::compact(&store, 7).await.expect("compact"), 7);
        assert_eq!(UsageStore::durability(&store), Durability::Durable);
        assert_eq!(
            *inner.0.lock().expect("lock"),
            ["apply", "release", "settle", "get", "compact"]
        );
    }

    #[test]
    fn ledger_ttl_and_phase_spellings_match_the_docs() {
        assert_eq!(DEFAULT_LEDGER_TTL, Duration::from_secs(3600));
        assert_eq!(Phase::Reserve.as_str(), "reserve");
        assert_eq!(Phase::Settle.as_str(), "settle");
        assert!(Charge::Allowed { used: 1, limit: 10 }.is_allowed());
        assert!(!Charge::Exceeded {
            used: 10,
            limit: 10
        }
        .is_allowed());
    }

    #[test]
    fn keys_differing_only_in_period_are_distinct_counters() {
        let hourly = MeterKey {
            identity: "session-42".into(),
            meter: "requests".into(),
            period_secs: 3600,
            window_start: 1_699_920_000,
        };
        let daily = MeterKey {
            period_secs: 86_400,
            ..hourly.clone()
        };
        assert_ne!(hourly, daily);
        let set: std::collections::BTreeSet<_> = [hourly, daily].into_iter().collect();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn invalid_quota_names_meter_and_reason() {
        let err = UsageError::InvalidQuota {
            meter: "requests".into(),
            reason: "period must be a positive whole number of seconds".into(),
        };
        assert_eq!(
            err.to_string(),
            "meter \"requests\" has an invalid quota: period must be a positive whole number of seconds"
        );
    }
}
