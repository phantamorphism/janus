//! `Meter`: quotas over a [`UsageStore`] (`docs/plugin-contract` "Metering
//! primitives" and "Charging before the cost is known").

use std::collections::HashMap;

use super::ledger::unix_now;
use crate::contract::metering::{
    Apply, Bound, Charge, Durability, MeterKey, OpId, Phase, Quota, UsageError, UsageStore, Window,
};

/// What a plugin calls from its `ResolveRoute` and `Notify` handlers: a
/// store plus the quotas configured on each meter.
///
/// - `charge` is one `apply` of a `Reserve` op with every quota's limit on
///   its bound — phase 1, check and increment in one step.
/// - `record` is one `apply` of a `Settle` op with no limits — phase 2,
///   counted unconditionally.
/// - `refund` is `release` of the `Reserve` op; `settle` is `settle` of it.
/// - `check` is `get` per quota.
///
/// Each quota of a meter is one counter per identity, keyed by its period
/// and by the window the clock is in when the op is first applied, so a
/// per-minute and a per-day quota on one meter count separately even where
/// their windows begin together. A meter's quotas with the same window share
/// a counter, so the tightest of their limits is the one that holds.
pub struct Meter<S: UsageStore> {
    store: S,
    quotas: HashMap<String, Vec<Configured>>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

/// A validated quota: its limit, and its window as a period in whole
/// seconds (`0`: `Total`).
#[derive(Debug, Clone, Copy)]
struct Configured {
    limit: u64,
    period_secs: u64,
}

impl<S: UsageStore> Meter<S> {
    /// Refuses a `Window::Total` quota on a store whose `durability()` is
    /// `Volatile` ([`UsageError::VolatileTotal`]): a budget specified as
    /// "never resets" cannot live in memory that does. Refuses a quota no
    /// counter can represent ([`UsageError::InvalidQuota`]): a `Fixed`
    /// period that is not a positive whole number of seconds.
    pub fn new(store: S, quotas: Vec<(String, Quota)>) -> Result<Self, UsageError> {
        if store.durability() == Durability::Volatile {
            if let Some((meter, _)) = quotas.iter().find(|(_, q)| q.window == Window::Total) {
                return Err(UsageError::VolatileTotal {
                    meter: meter.clone(),
                });
            }
        }
        Self::new_allowing_volatile_total(store, quotas)
    }

    /// As `new`, but accepts a `Total` quota on a volatile store: the
    /// explicit opt-in for tests and for plugins that accept the reset. Still
    /// refuses an invalid quota ([`UsageError::InvalidQuota`]).
    pub fn new_allowing_volatile_total(
        store: S,
        quotas: Vec<(String, Quota)>,
    ) -> Result<Self, UsageError> {
        let mut by_meter: HashMap<String, Vec<Configured>> = HashMap::new();
        for (meter, quota) in quotas {
            let period_secs = period_secs(&meter, quota.window)?;
            let configured = by_meter.entry(meter).or_default();
            match configured.iter_mut().find(|q| q.period_secs == period_secs) {
                Some(same_window) => same_window.limit = same_window.limit.min(quota.limit),
                None => configured.push(Configured {
                    limit: quota.limit,
                    period_secs,
                }),
            }
        }
        Ok(Self {
            store,
            quotas: by_meter,
            clock: Box::new(unix_now),
        })
    }

    /// Replace the clock that places a count in its window: Unix seconds,
    /// the system clock by default. For tests and simulations; a store keeps
    /// its own clock for ledger ages and window sweeps.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// The store, for what `Meter` does not wrap — chiefly `compact`. The
    /// shipped stores also compact themselves from their write path, so a
    /// plugin's own timer is optional.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Phase 1. Check and increment in one step against every quota
    /// configured on `meter`, so two concurrent requests from one identity
    /// cannot both pass a limit with one unit left. `Exceeded` leaves every
    /// counter where it was. Idempotent per `request_id`: a repeat returns
    /// the original answer.
    pub async fn charge(
        &self,
        request_id: &str,
        identity: &str,
        meter: &str,
        amount: u64,
    ) -> Result<Charge, UsageError> {
        let quotas = self.quotas_of(meter)?;
        let windows = self.current_windows(identity, meter, quotas);
        let bounds: Vec<Bound<'_>> = windows
            .iter()
            .zip(quotas)
            .map(|(window, quota)| window.bound(Some(quota.limit)))
            .collect();
        let op = op_id(request_id, identity, meter, Phase::Reserve);
        match self.store.apply(&op, &bounds, amount).await? {
            Apply::Applied { used } => nearest_exhaustion(quotas, &used),
            Apply::Rejected { index, used } => {
                let quota = quotas
                    .get(index)
                    .ok_or_else(|| store_mismatch(index + 1, quotas.len()))?;
                Ok(Charge::Exceeded {
                    used,
                    limit: quota.limit,
                })
            }
        }
    }

    /// Read-only: `Exceeded` if the identity is already at or over any quota
    /// on `meter` (the first such, in configuration order). For dimensions
    /// whose figure is only known after the call.
    pub async fn check(&self, identity: &str, meter: &str) -> Result<Charge, UsageError> {
        let quotas = self.quotas_of(meter)?;
        let mut used = Vec::with_capacity(quotas.len());
        for window in self.current_windows(identity, meter, quotas) {
            used.push(self.store.get(&window.key).await?);
        }
        if let Some((quota, &used)) = quotas.iter().zip(&used).find(|(q, &u)| u >= q.limit) {
            return Ok(Charge::Exceeded {
                used,
                limit: quota.limit,
            });
        }
        nearest_exhaustion(quotas, &used)
    }

    /// Phase 2. Add an amount unconditionally to every quota's current
    /// window — the cost already happened and must be counted even when it
    /// crosses the limit. Idempotent per `request_id`.
    pub async fn record(
        &self,
        request_id: &str,
        identity: &str,
        meter: &str,
        amount: u64,
    ) -> Result<(), UsageError> {
        let quotas = self.quotas_of(meter)?;
        let windows = self.current_windows(identity, meter, quotas);
        let bounds: Vec<Bound<'_>> = windows.iter().map(|window| window.bound(None)).collect();
        let op = op_id(request_id, identity, meter, Phase::Settle);
        match self.store.apply(&op, &bounds, amount).await? {
            Apply::Applied { .. } => Ok(()),
            Apply::Rejected { .. } => Err(UsageError::Backend(
                format!("usage store rejected an unconditional record on meter {meter:?}").into(),
            )),
        }
    }

    /// Reverse the `charge` made under `request_id`, in whichever window it
    /// actually landed. `false` if there was nothing to reverse: no such
    /// charge, already refunded or settled, `Exceeded`, or older than the
    /// store's ledger TTL.
    pub async fn refund(
        &self,
        request_id: &str,
        identity: &str,
        meter: &str,
    ) -> Result<bool, UsageError> {
        self.quotas_of(meter)?;
        let op = op_id(request_id, identity, meter, Phase::Reserve);
        self.store.release(&op).await
    }

    /// Declare the `charge` under `request_id` final, so the store may
    /// forget how to reverse it.
    pub async fn settle(
        &self,
        request_id: &str,
        identity: &str,
        meter: &str,
    ) -> Result<(), UsageError> {
        self.quotas_of(meter)?;
        let op = op_id(request_id, identity, meter, Phase::Reserve);
        self.store.settle(&op).await
    }

    fn quotas_of(&self, meter: &str) -> Result<&[Configured], UsageError> {
        self.quotas
            .get(meter)
            .map(Vec::as_slice)
            .ok_or_else(|| UsageError::UnknownMeter(meter.to_owned()))
    }

    /// Each quota's window at the clock's now.
    fn current_windows(
        &self,
        identity: &str,
        meter: &str,
        quotas: &[Configured],
    ) -> Vec<CurrentWindow> {
        let now = (self.clock)();
        quotas
            .iter()
            .map(|quota| {
                let (window_start, retain_until) = window_at(quota.period_secs, now);
                CurrentWindow {
                    key: MeterKey {
                        identity: identity.to_owned(),
                        meter: meter.to_owned(),
                        period_secs: quota.period_secs,
                        window_start,
                    },
                    retain_until,
                }
            })
            .collect()
    }
}

/// One quota's counter for the window the clock is in, and how long the
/// store has to keep it.
struct CurrentWindow {
    key: MeterKey,
    retain_until: Option<i64>,
}

impl CurrentWindow {
    fn bound(&self, limit: Option<u64>) -> Bound<'_> {
        Bound {
            key: &self.key,
            limit,
            retain_until: self.retain_until,
        }
    }
}

/// The window a quota of `period_secs` (`0`: `Total`) is in at `now`: when
/// it began, and until when its count must be kept (`None`: forever). A
/// `Fixed` window begins at a multiple of its period and is kept until the
/// next one begins.
fn window_at(period_secs: u64, now: i64) -> (i64, Option<i64>) {
    if period_secs == 0 {
        return (0, None);
    }
    // Validated at construction to fit an `i64`.
    let period = period_secs as i64;
    let start = now - now.rem_euclid(period);
    (start, Some(start.saturating_add(period)))
}

/// A quota's window as the period its key carries: whole seconds for
/// `Fixed`, `0` for `Total`.
fn period_secs(meter: &str, window: Window) -> Result<u64, UsageError> {
    let Window::Fixed { period } = window else {
        return Ok(0);
    };
    let secs = period.as_secs();
    if secs == 0 || period.subsec_nanos() != 0 || i64::try_from(secs).is_err() {
        return Err(UsageError::InvalidQuota {
            meter: meter.to_owned(),
            reason: format!(
                "a Fixed period must be a positive whole number of seconds, not {period:?}"
            ),
        });
    }
    Ok(secs)
}

fn op_id(request_id: &str, identity: &str, meter: &str, phase: Phase) -> OpId {
    OpId {
        identity: identity.to_owned(),
        meter: meter.to_owned(),
        request_id: request_id.to_owned(),
        phase,
    }
}

/// `Allowed` of the quota with the fewest units left (the first, on a tie).
fn nearest_exhaustion(quotas: &[Configured], used: &[u64]) -> Result<Charge, UsageError> {
    if used.len() != quotas.len() {
        return Err(store_mismatch(used.len(), quotas.len()));
    }
    let (quota, &used) = quotas
        .iter()
        .zip(used)
        .min_by_key(|(quota, &used)| quota.limit.saturating_sub(used))
        .expect("a configured meter has at least one quota");
    Ok(Charge::Allowed {
        used,
        limit: quota.limit,
    })
}

/// A store answered about more bounds than it was given.
fn store_mismatch(answered: usize, bounds: usize) -> UsageError {
    UsageError::Backend(
        format!("usage store answered for {answered} bounds of an op with {bounds}").into(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::metering::MemoryUsageStore;

    const MINUTE: Duration = Duration::from_secs(60);

    fn per_minute(limit: u64) -> Quota {
        Quota {
            limit,
            window: Window::Fixed { period: MINUTE },
        }
    }

    fn total(limit: u64) -> Quota {
        Quota {
            limit,
            window: Window::Total,
        }
    }

    /// A settable clock starting at the next minute boundary of the wall
    /// clock, so the store's own sweep of expired windows never fires early.
    fn clock() -> Arc<AtomicI64> {
        let now = unix_now();
        Arc::new(AtomicI64::new(now - now % 60 + 60))
    }

    /// A memory-backed meter on `clock`, `Total` quotas allowed.
    fn memory_meter(quotas: &[(&str, Quota)], clock: &Arc<AtomicI64>) -> Meter<MemoryUsageStore> {
        let clock = Arc::clone(clock);
        let quotas = quotas.iter().map(|(m, q)| ((*m).to_owned(), *q)).collect();
        Meter::new_allowing_volatile_total(MemoryUsageStore::new(), quotas)
            .expect("valid quotas")
            .with_clock(move || clock.load(Ordering::SeqCst))
    }

    async fn count<S: UsageStore>(
        meter: &Meter<S>,
        name: &str,
        period_secs: u64,
        window_start: i64,
    ) -> u64 {
        let key = MeterKey {
            identity: "alice".into(),
            meter: name.into(),
            period_secs,
            window_start,
        };
        meter.store().get(&key).await.expect("get")
    }

    /// Alice's count on `name`: the `Total` window at `window_start` 0, the
    /// per-minute window starting there otherwise.
    async fn used<S: UsageStore>(meter: &Meter<S>, name: &str, window_start: i64) -> u64 {
        let period_secs = if window_start == 0 { 0 } else { 60 };
        count(meter, name, period_secs, window_start).await
    }

    #[tokio::test]
    async fn charge_allows_up_to_the_limit_then_exceeded() {
        let meter = memory_meter(&[("requests", per_minute(3))], &clock());
        for (n, request) in ["r1", "r2", "r3"].into_iter().enumerate() {
            assert_eq!(
                meter
                    .charge(request, "alice", "requests", 1)
                    .await
                    .expect("charge"),
                Charge::Allowed {
                    used: n as u64 + 1,
                    limit: 3
                }
            );
        }
        assert_eq!(
            meter
                .charge("r4", "alice", "requests", 1)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 3, limit: 3 }
        );
    }

    #[tokio::test]
    async fn charge_exceeded_does_not_increment() {
        let clock = clock();
        let now = clock.load(Ordering::SeqCst);
        let meter = memory_meter(
            &[("requests", per_minute(5)), ("requests", total(100))],
            &clock,
        );
        meter
            .charge("r1", "alice", "requests", 4)
            .await
            .expect("charge");
        for request in ["r2", "r3", "r4"] {
            assert_eq!(
                meter
                    .charge(request, "alice", "requests", 2)
                    .await
                    .expect("charge"),
                Charge::Exceeded { used: 4, limit: 5 }
            );
        }
        assert_eq!(used(&meter, "requests", now).await, 4);
        assert_eq!(
            used(&meter, "requests", 0).await,
            4,
            "the quota that passed is not charged either"
        );
        assert!(
            !meter
                .refund("r2", "alice", "requests")
                .await
                .expect("refund"),
            "an Exceeded charge has nothing to refund"
        );
        assert!(meter
            .charge("r5", "alice", "requests", 1)
            .await
            .expect("charge")
            .is_allowed());
    }

    #[tokio::test]
    async fn charge_is_idempotent_per_request_id() {
        let clock = clock();
        let meter = memory_meter(&[("requests", per_minute(10))], &clock);
        let first = meter
            .charge("r1", "alice", "requests", 4)
            .await
            .expect("charge");
        clock.fetch_add(60, Ordering::SeqCst);
        assert_eq!(
            meter
                .charge("r1", "alice", "requests", 4)
                .await
                .expect("charge"),
            first,
            "a redelivery in a later window replays the original answer"
        );
        assert_eq!(
            used(&meter, "requests", clock.load(Ordering::SeqCst)).await,
            0
        );
        assert_eq!(
            used(&meter, "requests", clock.load(Ordering::SeqCst) - 60).await,
            4
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_charges_cannot_exceed_limit() {
        let clock = clock();
        let now = clock.load(Ordering::SeqCst);
        let meter = Arc::new(memory_meter(
            &[("requests", per_minute(1_000)), ("requests", total(97))],
            &clock,
        ));
        let tasks: Vec<_> = (0..48)
            .map(|task| {
                let meter = Arc::clone(&meter);
                tokio::spawn(async move {
                    let mut allowed = 0u64;
                    for n in 0..10 {
                        let request = format!("t{task}-r{n}");
                        let charge = meter.charge(&request, "alice", "requests", 1);
                        match charge.await.expect("charge") {
                            Charge::Allowed { used, limit } => {
                                assert!(used <= limit);
                                allowed += 1;
                            }
                            Charge::Exceeded { used, limit } => assert_eq!((used, limit), (97, 97)),
                        }
                    }
                    allowed
                })
            })
            .collect();
        let mut allowed = 0;
        for task in tasks {
            allowed += task.await.expect("task");
        }
        assert_eq!(allowed, 97);
        assert_eq!(used(&meter, "requests", 0).await, 97);
        assert_eq!(used(&meter, "requests", now).await, 97);
    }

    #[tokio::test]
    async fn charge_reports_the_quota_nearest_exhaustion() {
        let meter = memory_meter(
            &[("requests", per_minute(10)), ("requests", total(5))],
            &clock(),
        );
        assert_eq!(
            meter
                .charge("r1", "alice", "requests", 2)
                .await
                .expect("charge"),
            Charge::Allowed { used: 2, limit: 5 },
            "3 left of the total beats 8 left of the minute"
        );
        let meter = meter_with_tight_minute();
        assert_eq!(
            meter
                .charge("r1", "alice", "requests", 2)
                .await
                .expect("charge"),
            Charge::Allowed { used: 2, limit: 3 }
        );
    }

    /// Charging 3 `requests` fills the effective limit, so the next unit is
    /// refused against a limit of 3.
    async fn assert_three_fill_a_limit_of_three(meter: &Meter<MemoryUsageStore>) {
        meter
            .charge("r1", "alice", "requests", 3)
            .await
            .expect("charge");
        assert_eq!(
            meter
                .charge("r2", "alice", "requests", 1)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 3, limit: 3 }
        );
    }

    fn meter_with_tight_minute() -> Meter<MemoryUsageStore> {
        memory_meter(
            &[("requests", total(50)), ("requests", per_minute(3))],
            &clock(),
        )
    }

    #[tokio::test]
    async fn charge_exceeded_reports_the_quota_that_was_exceeded() {
        let meter = meter_with_tight_minute();
        assert_three_fill_a_limit_of_three(&meter).await;
    }

    #[tokio::test]
    async fn check_is_read_only_and_exceeded_at_the_limit() {
        let meter = memory_meter(
            &[("cost", per_minute(100)), ("cost", total(1_000))],
            &clock(),
        );
        assert_eq!(
            meter.check("alice", "cost").await.expect("check"),
            Charge::Allowed {
                used: 0,
                limit: 100
            }
        );
        meter
            .record("r1", "alice", "cost", 99)
            .await
            .expect("record");
        for _ in 0..3 {
            assert_eq!(
                meter.check("alice", "cost").await.expect("check"),
                Charge::Allowed {
                    used: 99,
                    limit: 100
                },
                "check does not count"
            );
        }
        meter
            .record("r2", "alice", "cost", 1)
            .await
            .expect("record");
        assert_eq!(
            meter.check("alice", "cost").await.expect("check"),
            Charge::Exceeded {
                used: 100,
                limit: 100
            }
        );
    }

    #[tokio::test]
    async fn record_is_unconditional_and_visible_to_the_next_check() {
        let clock = clock();
        let meter = memory_meter(&[("cost", per_minute(100))], &clock);
        meter
            .record("r1", "alice", "cost", 90)
            .await
            .expect("record");
        meter
            .record("r2", "alice", "cost", 500)
            .await
            .expect("record");
        assert_eq!(
            used(&meter, "cost", clock.load(Ordering::SeqCst)).await,
            590
        );
        assert_eq!(
            meter.check("alice", "cost").await.expect("check"),
            Charge::Exceeded {
                used: 590,
                limit: 100
            }
        );
        assert!(
            !meter.refund("r2", "alice", "cost").await.expect("refund"),
            "a record is not a charge and is not refundable"
        );
    }

    #[tokio::test]
    async fn record_is_idempotent_per_request_id() {
        let clock = clock();
        let meter = memory_meter(&[("cost", total(1_000))], &clock);
        for _ in 0..3 {
            meter
                .record("r1", "alice", "cost", 40)
                .await
                .expect("record");
        }
        assert_eq!(used(&meter, "cost", 0).await, 40);
        meter
            .charge("r1", "alice", "cost", 5)
            .await
            .expect("charge");
        assert_eq!(
            used(&meter, "cost", 0).await,
            45,
            "a charge and a record under one request_id are separate ops"
        );
    }

    #[tokio::test]
    async fn refund_reverses_a_charge_once() {
        let meter = memory_meter(&[("requests", total(10))], &clock());
        meter
            .charge("r1", "alice", "requests", 3)
            .await
            .expect("charge");
        meter
            .charge("r2", "alice", "requests", 2)
            .await
            .expect("charge");
        assert!(meter
            .refund("r1", "alice", "requests")
            .await
            .expect("refund"));
        assert!(!meter
            .refund("r1", "alice", "requests")
            .await
            .expect("refund"));
        assert!(!meter
            .refund("never", "alice", "requests")
            .await
            .expect("refund"));
        assert!(
            !meter.refund("r2", "bob", "requests").await.expect("refund"),
            "a request id is scoped to its identity"
        );
        assert_eq!(used(&meter, "requests", 0).await, 2);
    }

    #[tokio::test]
    async fn refund_lands_in_original_window() {
        let clock = clock();
        let charged_at = clock.load(Ordering::SeqCst);
        let meter = memory_meter(&[("requests", per_minute(10))], &clock);
        meter
            .charge("r1", "alice", "requests", 4)
            .await
            .expect("charge");
        clock.store(charged_at + 75, Ordering::SeqCst);
        meter
            .charge("r2", "alice", "requests", 6)
            .await
            .expect("charge");
        assert!(meter
            .refund("r1", "alice", "requests")
            .await
            .expect("refund"));
        assert_eq!(
            used(&meter, "requests", charged_at).await,
            0,
            "charged window"
        );
        assert_eq!(
            used(&meter, "requests", charged_at + 60).await,
            6,
            "the window the clock is in now is untouched"
        );
        assert_eq!(
            meter
                .charge("r3", "alice", "requests", 5)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 6, limit: 10 }
        );
    }

    #[tokio::test]
    async fn settle_makes_a_charge_final() {
        let meter = memory_meter(&[("requests", total(10))], &clock());
        meter
            .charge("r1", "alice", "requests", 3)
            .await
            .expect("charge");
        meter
            .settle("r1", "alice", "requests")
            .await
            .expect("settle");
        meter
            .settle("unknown", "alice", "requests")
            .await
            .expect("settle");
        assert!(!meter
            .refund("r1", "alice", "requests")
            .await
            .expect("refund"));
        assert_eq!(used(&meter, "requests", 0).await, 3);
    }

    #[tokio::test]
    async fn fixed_window_resets_on_period_boundary() {
        let clock = clock();
        let start = clock.load(Ordering::SeqCst);
        let meter = memory_meter(&[("requests", per_minute(2))], &clock);
        clock.store(start + 59, Ordering::SeqCst);
        meter
            .charge("r1", "alice", "requests", 2)
            .await
            .expect("charge");
        assert!(!meter
            .charge("r2", "alice", "requests", 1)
            .await
            .expect("charge")
            .is_allowed());
        clock.store(start + 60, Ordering::SeqCst);
        assert_eq!(
            meter
                .charge("r3", "alice", "requests", 1)
                .await
                .expect("charge"),
            Charge::Allowed { used: 1, limit: 2 }
        );
        assert_eq!(used(&meter, "requests", start).await, 2);
        assert_eq!(used(&meter, "requests", start + 60).await, 1);
    }

    #[tokio::test]
    async fn total_window_never_resets() {
        let clock = clock();
        let meter = memory_meter(&[("credits", total(5))], &clock);
        meter
            .charge("r1", "alice", "credits", 5)
            .await
            .expect("charge");
        clock.fetch_add(10 * 365 * 86_400, Ordering::SeqCst);
        assert_eq!(
            meter
                .charge("r2", "alice", "credits", 1)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 5, limit: 5 }
        );
    }

    #[test]
    fn window_start_is_a_multiple_of_the_period_and_kept_until_the_next() {
        assert_eq!(window_at(3600, 7_250), (7_200, Some(10_800)));
        assert_eq!(window_at(3600, 7_200), (7_200, Some(10_800)));
        assert_eq!(window_at(3600, -1), (-3_600, Some(0)));
        assert_eq!(window_at(0, 7_250), (0, None));
    }

    #[test]
    fn total_on_volatile_store_refused() {
        let quotas = vec![
            ("requests".to_owned(), per_minute(10)),
            ("credits".to_owned(), total(100)),
        ];
        match Meter::new(MemoryUsageStore::new(), quotas) {
            Err(UsageError::VolatileTotal { meter }) => assert_eq!(meter, "credits"),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a Total quota on a volatile store was accepted"),
        }
        assert!(Meter::new(
            MemoryUsageStore::new(),
            vec![("requests".to_owned(), per_minute(10))]
        )
        .is_ok());
    }

    #[tokio::test]
    async fn new_allowing_volatile_total_is_the_explicit_opt_in() {
        let meter = Meter::new_allowing_volatile_total(
            MemoryUsageStore::new(),
            vec![("credits".to_owned(), total(2))],
        )
        .expect("the opt-in accepts a Total quota on a volatile store");
        meter
            .charge("r1", "alice", "credits", 2)
            .await
            .expect("charge");
        assert!(!meter
            .charge("r2", "alice", "credits", 1)
            .await
            .expect("charge")
            .is_allowed());
    }

    #[tokio::test]
    async fn unknown_meter_is_an_error_for_every_operation() {
        let meter = memory_meter(&[("requests", per_minute(1))], &clock());
        let unknown = |result: Result<(), UsageError>| {
            assert!(matches!(result, Err(UsageError::UnknownMeter(m)) if m == "tokens"));
        };
        unknown(meter.charge("r", "alice", "tokens", 1).await.map(drop));
        unknown(meter.check("alice", "tokens").await.map(drop));
        unknown(meter.record("r", "alice", "tokens", 1).await);
        unknown(meter.refund("r", "alice", "tokens").await.map(drop));
        unknown(meter.settle("r", "alice", "tokens").await);
    }

    #[tokio::test]
    async fn quotas_sharing_a_window_keep_the_tighter_limit() {
        let meter = memory_meter(
            &[
                ("requests", per_minute(10)),
                ("requests", per_minute(3)),
                ("requests", per_minute(7)),
            ],
            &clock(),
        );
        assert_three_fill_a_limit_of_three(&meter).await;
    }

    /// The docs' example: a per-minute and a per-day limit on the same
    /// meter. At a day boundary both windows begin at the same second; their
    /// counters must still be separate.
    #[tokio::test]
    async fn per_minute_and_per_day_quotas_on_one_meter_count_separately() {
        let now = unix_now();
        let midnight = now - now.rem_euclid(86_400) + 86_400;
        let clock = Arc::new(AtomicI64::new(midnight));
        let day = Quota {
            limit: 5,
            window: Window::Fixed {
                period: Duration::from_secs(86_400),
            },
        };
        let meter = memory_meter(&[("requests", per_minute(3)), ("requests", day)], &clock);
        for request in ["r1", "r2", "r3"] {
            assert!(meter
                .charge(request, "alice", "requests", 1)
                .await
                .expect("charge")
                .is_allowed());
        }
        assert_eq!(
            meter
                .charge("r4", "alice", "requests", 1)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 3, limit: 3 },
            "the minute is spent, not doubled by the day sharing its start"
        );
        assert_eq!(count(&meter, "requests", 60, midnight).await, 3);
        assert_eq!(count(&meter, "requests", 86_400, midnight).await, 3);

        clock.store(midnight + 60, Ordering::SeqCst);
        assert_eq!(
            meter
                .charge("r5", "alice", "requests", 2)
                .await
                .expect("charge"),
            Charge::Allowed { used: 5, limit: 5 },
            "a new minute, the same day: the day is now nearest exhaustion"
        );
        assert_eq!(
            meter
                .charge("r6", "alice", "requests", 1)
                .await
                .expect("charge"),
            Charge::Exceeded { used: 5, limit: 5 },
            "the day refuses although the minute has room"
        );
        assert_eq!(count(&meter, "requests", 60, midnight + 60).await, 2);
        assert_eq!(count(&meter, "requests", 86_400, midnight).await, 5);
        assert!(meter
            .refund("r1", "alice", "requests")
            .await
            .expect("refund"));
        assert_eq!(count(&meter, "requests", 60, midnight).await, 2);
        assert_eq!(count(&meter, "requests", 86_400, midnight).await, 4);
        assert_eq!(count(&meter, "requests", 60, midnight + 60).await, 2);
    }

    fn fixed(period: Duration) -> Vec<(String, Quota)> {
        let quota = Quota {
            limit: 1,
            window: Window::Fixed { period },
        };
        vec![("requests".to_owned(), quota)]
    }

    #[test]
    fn periods_that_are_not_positive_whole_seconds_are_invalid_quotas() {
        for period in [Duration::ZERO, Duration::from_millis(1_500)] {
            for result in [
                Meter::new(MemoryUsageStore::new(), fixed(period)).map(drop),
                Meter::new_allowing_volatile_total(MemoryUsageStore::new(), fixed(period))
                    .map(drop),
            ] {
                match result {
                    Err(UsageError::InvalidQuota { meter, reason }) => {
                        assert_eq!(meter, "requests");
                        assert!(
                            reason.contains("positive whole number of seconds"),
                            "{reason}"
                        );
                    }
                    other => panic!("{period:?} accepted or wrong error: {other:?}"),
                }
            }
        }
        let too_long = Duration::from_secs(u64::MAX);
        assert!(matches!(
            Meter::new(MemoryUsageStore::new(), fixed(too_long)),
            Err(UsageError::InvalidQuota { .. })
        ));
        assert!(Meter::new(MemoryUsageStore::new(), fixed(Duration::from_secs(1))).is_ok());
    }

    #[tokio::test]
    async fn identities_and_meters_count_separately() {
        let meter = memory_meter(&[("requests", total(1)), ("tokens", total(1))], &clock());
        for (identity, name) in [
            ("alice", "requests"),
            ("bob", "requests"),
            ("alice", "tokens"),
        ] {
            assert!(meter
                .charge("same-request", identity, name, 1)
                .await
                .expect("charge")
                .is_allowed());
        }
    }

    /// The flow `docs/plugin-contract` "Charging before the cost is known"
    /// describes, across both settlement events.
    #[tokio::test]
    async fn two_phase_metering_denies_the_next_request_after_a_cost_overshoot() {
        let meter = memory_meter(
            &[("requests", per_minute(100)), ("cost", total(1_000))],
            &clock(),
        );
        let resolve = |request: &'static str| {
            let meter = &meter;
            async move {
                if !meter
                    .check("alice", "cost")
                    .await
                    .expect("check")
                    .is_allowed()
                {
                    return false;
                }
                meter
                    .charge(request, "alice", "requests", 1)
                    .await
                    .expect("charge")
                    .is_allowed()
            }
        };
        assert!(resolve("r1").await);
        // call_completed: the call cost more than the whole budget.
        meter
            .record("r1", "alice", "cost", 1_500)
            .await
            .expect("record");
        meter
            .settle("r1", "alice", "requests")
            .await
            .expect("settle");
        // Redelivered call_completed is counted once.
        meter
            .record("r1", "alice", "cost", 1_500)
            .await
            .expect("record");
        assert_eq!(used(&meter, "cost", 0).await, 1_500);
        assert!(!resolve("r2").await, "the next request is denied");
        // call_abandoned for a request that was allowed: refund it.
        let other = memory_meter(&[("requests", per_minute(1))], &clock());
        other
            .charge("r3", "alice", "requests", 1)
            .await
            .expect("charge");
        assert!(other
            .refund("r3", "alice", "requests")
            .await
            .expect("refund"));
        assert!(other
            .charge("r4", "alice", "requests", 1)
            .await
            .expect("charge")
            .is_allowed());
    }

    #[tokio::test]
    async fn compact_through_the_store_accessor_drops_expired_windows() {
        let clock = clock();
        let start = clock.load(Ordering::SeqCst);
        let meter = memory_meter(&[("requests", per_minute(5))], &clock);
        meter
            .charge("r1", "alice", "requests", 1)
            .await
            .expect("charge");
        let dropped = meter.store().compact(start + 61).await.expect("compact");
        assert_eq!(dropped, 1);
        assert_eq!(used(&meter, "requests", start).await, 0);
    }
}
