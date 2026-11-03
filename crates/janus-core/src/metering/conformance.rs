//! The `UsageStore` conformance suite (feature `testing`): the behaviour
//! every store must have for `Meter` to be correct over it
//! (`docs/plugin-contract` "The store"). Both shipped stores run it; a third
//! store runs it against itself with
//! [`usage_store_conformance_tests!`](crate::usage_store_conformance_tests).
//!
//! Each check takes a fresh, empty store and panics on the first violation.
//! Checks use windows anchored at the current wall-clock time, so a store
//! that sweeps expired windows on its own may do so freely.

use std::sync::Arc;
use std::time::Duration;

use super::ledger::unix_now;
use crate::contract::metering::{Apply, Bound, MeterKey, OpId, Phase, UsageStore};

/// Generate one `#[tokio::test]` per conformance check against a store.
///
/// `$make` is a closure returning `(store, guard)`: a fresh, empty store, and
/// anything that must outlive it for the duration of one check (a temporary
/// directory, a container handle; `()` if nothing). `$ledger_ttl` is the
/// ledger TTL the store was built with, at least one minute. The calling
/// crate needs `tokio` with the `macros` and `rt-multi-thread` features.
///
/// ```
/// // In the store's crate, e.g. `tests/conformance.rs`:
/// use janus_core::metering::{MemoryUsageStore, DEFAULT_LEDGER_TTL};
///
/// janus_core::usage_store_conformance_tests!(
///     || (MemoryUsageStore::new(), ()),
///     DEFAULT_LEDGER_TTL
/// );
/// # fn main() {}
/// ```
#[macro_export]
macro_rules! usage_store_conformance_tests {
    ($make:expr, $ledger_ttl:expr) => {
        $crate::usage_store_conformance_tests!(@checks $make, $ledger_ttl;
            apply_adds_to_every_bound,
            apply_limit_is_inclusive,
            apply_is_all_or_nothing_across_bounds,
            unlimited_bounds_cross_their_limit,
            apply_is_idempotent_per_request_id,
            release_lands_in_the_window_charged,
            release_returns_false_when_there_is_nothing_to_undo,
            settle_makes_an_op_final,
            keys_are_isolated_per_identity_meter_period_and_window,
            concurrent_applies_never_exceed_limit,
            concurrent_repeats_of_one_op_apply_once,
            concurrent_releases_undo_once,
        );
        $crate::usage_store_conformance_tests!(@ttl_checks $make, $ledger_ttl;
            compact_drops_expired_keys_only,
            compact_drops_ledger_entries_past_the_ttl,
        );
    };
    (@checks $make:expr, $ledger_ttl:expr; $($check:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn $check() {
                let (store, _guard) = ($make)();
                $crate::metering::conformance::$check(store).await;
            }
        )+
    };
    (@ttl_checks $make:expr, $ledger_ttl:expr; $($check:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
            async fn $check() {
                let (store, _guard) = ($make)();
                $crate::metering::conformance::$check(store, $ledger_ttl).await;
            }
        )+
    };
}

const IDENTITY: &str = "conformance-identity";
const METER: &str = "requests";

/// A key of the suite's identity and meter: the `Total` window at
/// `window_start` 0, a per-minute one otherwise. Stores treat every field
/// as opaque key material, so the window starts need not be aligned.
fn key(window_start: i64) -> MeterKey {
    let period_secs = if window_start == 0 { 0 } else { 60 };
    key_for(IDENTITY, METER, period_secs, window_start)
}

fn key_for(identity: &str, meter: &str, period_secs: u64, window_start: i64) -> MeterKey {
    MeterKey {
        identity: identity.into(),
        meter: meter.into(),
        period_secs,
        window_start,
    }
}

fn op(request_id: &str, phase: Phase) -> OpId {
    OpId {
        identity: IDENTITY.into(),
        meter: METER.into(),
        request_id: request_id.into(),
        phase,
    }
}

/// A bound kept for an hour past now.
fn limited(key: &MeterKey, limit: u64) -> Bound<'_> {
    Bound {
        key,
        limit: Some(limit),
        retain_until: Some(unix_now() + 3600),
    }
}

fn unlimited(key: &MeterKey) -> Bound<'_> {
    Bound {
        key,
        limit: None,
        retain_until: None,
    }
}

async fn apply<S: UsageStore + ?Sized>(
    store: &S,
    op: &OpId,
    bounds: &[Bound<'_>],
    amount: u64,
) -> Apply {
    store.apply(op, bounds, amount).await.expect("apply")
}

async fn get<S: UsageStore + ?Sized>(store: &S, key: &MeterKey) -> u64 {
    store.get(key).await.expect("get")
}

async fn release<S: UsageStore + ?Sized>(store: &S, op: &OpId) -> bool {
    store.release(op).await.expect("release")
}

/// An applied op adds `amount` to every bound and reports the new totals in
/// bound order; `get` reads them back; an unwritten key reads 0.
pub async fn apply_adds_to_every_bound<S: UsageStore>(store: S) {
    let now = unix_now();
    let (minute, total) = (key(now - now % 60), key(0));
    assert_eq!(get(&store, &minute).await, 0, "unwritten key reads 0");
    let bounds = [limited(&minute, 10), unlimited(&total)];
    assert_eq!(
        apply(&store, &op("a", Phase::Reserve), &bounds, 3).await,
        Apply::Applied { used: vec![3, 3] }
    );
    assert_eq!(
        apply(&store, &op("b", Phase::Reserve), &bounds, 4).await,
        Apply::Applied { used: vec![7, 7] }
    );
    assert_eq!(get(&store, &minute).await, 7);
    assert_eq!(get(&store, &total).await, 7);
    assert_eq!(
        apply(&store, &op("c", Phase::Reserve), &[], 4).await,
        Apply::Applied { used: vec![] },
        "an op with no bounds applies to nothing"
    );
}

/// Reaching a limit exactly is allowed; one past it is not, and a rejection
/// reports the count as it stands.
pub async fn apply_limit_is_inclusive<S: UsageStore>(store: S) {
    let k = key(0);
    assert_eq!(
        apply(&store, &op("a", Phase::Reserve), &[limited(&k, 5)], 5).await,
        Apply::Applied { used: vec![5] }
    );
    assert_eq!(
        apply(&store, &op("b", Phase::Reserve), &[limited(&k, 5)], 1).await,
        Apply::Rejected { index: 0, used: 5 }
    );
    assert_eq!(
        apply(&store, &op("c", Phase::Reserve), &[limited(&k, 5)], 0).await,
        Apply::Applied { used: vec![5] },
        "a zero amount at the limit does not exceed it"
    );
    assert_eq!(
        apply(
            &store,
            &op("d", Phase::Reserve),
            &[limited(&k, 100)],
            u64::MAX
        )
        .await,
        Apply::Rejected { index: 0, used: 5 },
        "an amount that would overflow exceeds any limit"
    );
    assert_eq!(get(&store, &k).await, 5);
}

/// One bound that would exceed rejects the op, names that bound, and leaves
/// every bound — including those before it — unchanged.
pub async fn apply_is_all_or_nothing_across_bounds<S: UsageStore>(store: S) {
    let now = unix_now();
    let (a, b, c) = (key(now), key(now + 1), key(0));
    let seed = [limited(&b, 100)];
    apply(&store, &op("seed", Phase::Reserve), &seed, 8).await;
    let bounds = [limited(&a, 100), limited(&b, 10), limited(&c, 100)];
    assert_eq!(
        apply(&store, &op("over", Phase::Reserve), &bounds, 3).await,
        Apply::Rejected { index: 1, used: 8 }
    );
    assert_eq!(
        (
            get(&store, &a).await,
            get(&store, &b).await,
            get(&store, &c).await
        ),
        (0, 8, 0)
    );
    assert!(
        !release(&store, &op("over", Phase::Reserve)).await,
        "a rejected op touched nothing to release"
    );
    assert_eq!(
        apply(&store, &op("fits", Phase::Reserve), &bounds, 2).await,
        Apply::Applied {
            used: vec![2, 10, 2]
        }
    );
}

/// A bound with no limit is added to unconditionally, even past a limit
/// other ops checked against the same key.
pub async fn unlimited_bounds_cross_their_limit<S: UsageStore>(store: S) {
    let k = key(0);
    apply(&store, &op("charge", Phase::Reserve), &[limited(&k, 10)], 9).await;
    assert_eq!(
        apply(&store, &op("record", Phase::Settle), &[unlimited(&k)], 50).await,
        Apply::Applied { used: vec![59] }
    );
    assert_eq!(
        apply(&store, &op("next", Phase::Reserve), &[limited(&k, 10)], 1).await,
        Apply::Rejected { index: 0, used: 59 },
        "a crossed limit is visible to the next limited op"
    );
}

/// A repeated op returns its original result and changes nothing, whatever
/// it is repeated with; the same request id in another phase, identity or
/// meter is a different op.
pub async fn apply_is_idempotent_per_request_id<S: UsageStore>(store: S) {
    let k = key(0);
    let first = op("req-1", Phase::Reserve);
    assert_eq!(
        apply(&store, &first, &[limited(&k, 10)], 4).await,
        Apply::Applied { used: vec![4] }
    );
    for amount in [4, 1, 100] {
        assert_eq!(
            apply(&store, &first, &[limited(&k, 10)], amount).await,
            Apply::Applied { used: vec![4] },
            "a repeat replays the original result"
        );
    }
    assert_eq!(get(&store, &k).await, 4);

    let rejected = op("req-2", Phase::Reserve);
    assert_eq!(
        apply(&store, &rejected, &[limited(&k, 10)], 7).await,
        Apply::Rejected { index: 0, used: 4 }
    );
    assert_eq!(
        apply(&store, &rejected, &[limited(&k, 100)], 7).await,
        Apply::Rejected { index: 0, used: 4 },
        "a rejection is replayed too, even when the op would now fit"
    );

    let settle_phase = op("req-1", Phase::Settle);
    assert_eq!(
        apply(&store, &settle_phase, &[unlimited(&k)], 2).await,
        Apply::Applied { used: vec![6] },
        "the Settle phase of a request id is its own op"
    );
    apply(&store, &settle_phase, &[unlimited(&k)], 2).await;
    let other_identity = OpId {
        identity: "someone-else".into(),
        ..first.clone()
    };
    let other_key = key_for("someone-else", METER, 0, 0);
    assert_eq!(
        apply(&store, &other_identity, &[limited(&other_key, 10)], 1).await,
        Apply::Applied { used: vec![1] },
        "the request id of another identity is its own op"
    );
    assert_eq!(get(&store, &k).await, 6);
}

/// `release` undoes exactly the keys the op touched, by the op's amount,
/// even after other ops wrote to those keys and to later windows.
pub async fn release_lands_in_the_window_charged<S: UsageStore>(store: S) {
    let now = unix_now();
    let (charged, later, total) = (key(now - now % 60), key(now - now % 60 + 60), key(0));
    let charge = op("charged", Phase::Reserve);
    apply(
        &store,
        &charge,
        &[limited(&charged, 100), limited(&total, 100)],
        5,
    )
    .await;
    apply(
        &store,
        &op("same-window", Phase::Reserve),
        &[limited(&charged, 100), limited(&total, 100)],
        2,
    )
    .await;
    apply(
        &store,
        &op("next-window", Phase::Reserve),
        &[limited(&later, 100), limited(&total, 100)],
        3,
    )
    .await;
    assert!(release(&store, &charge).await);
    assert_eq!(get(&store, &charged).await, 2, "the charged window");
    assert_eq!(get(&store, &later).await, 3, "a window it did not touch");
    assert_eq!(get(&store, &total).await, 5);
    assert_eq!(
        apply(&store, &charge, &[limited(&charged, 100)], 5).await,
        Apply::Applied { used: vec![5, 5] },
        "a released op still replays its original result and is not re-applied"
    );
    assert_eq!(get(&store, &charged).await, 2);
}

/// `release` is `false`, and changes nothing, for an op that is unknown,
/// already released, already settled, rejected, or of the `Settle` phase.
pub async fn release_returns_false_when_there_is_nothing_to_undo<S: UsageStore>(store: S) {
    let k = key(0);
    assert!(!release(&store, &op("unknown", Phase::Reserve)).await);

    let once = op("once", Phase::Reserve);
    apply(&store, &once, &[limited(&k, 100)], 3).await;
    assert!(release(&store, &once).await);
    assert!(!release(&store, &once).await, "already released");

    let settled = op("settled", Phase::Reserve);
    apply(&store, &settled, &[limited(&k, 100)], 4).await;
    store.settle(&settled).await.expect("settle");
    assert!(!release(&store, &settled).await, "already settled");

    let recorded = op("recorded", Phase::Settle);
    apply(&store, &recorded, &[unlimited(&k)], 5).await;
    assert!(!release(&store, &recorded).await, "a Settle-phase op");

    let rejected = op("rejected", Phase::Reserve);
    apply(&store, &rejected, &[limited(&k, 1)], 1).await;
    assert!(!release(&store, &rejected).await, "a rejected op");

    assert_eq!(get(&store, &k).await, 9);
}

/// `settle` makes an op final: its count stands, it can no longer be
/// released, a repeat still replays; settling twice, or an unknown op, is
/// not an error.
pub async fn settle_makes_an_op_final<S: UsageStore>(store: S) {
    let k = key(0);
    let charge = op("final", Phase::Reserve);
    apply(&store, &charge, &[limited(&k, 10)], 6).await;
    store.settle(&charge).await.expect("settle");
    store.settle(&charge).await.expect("settle twice");
    store
        .settle(&op("never-applied", Phase::Reserve))
        .await
        .expect("settle unknown");
    assert!(!release(&store, &charge).await);
    assert_eq!(
        apply(&store, &charge, &[limited(&k, 10)], 6).await,
        Apply::Applied { used: vec![6] }
    );
    assert_eq!(get(&store, &k).await, 6);
}

/// Counts of different identities, meters, periods and windows never mix —
/// in particular two periods whose windows begin at the same second, as an
/// hourly and a daily window do at every midnight.
pub async fn keys_are_isolated_per_identity_meter_period_and_window<S: UsageStore>(store: S) {
    let keys = [
        key_for("alice", "requests", 0, 0),
        key_for("alice", "requests", 3_600, 86_400),
        key_for("alice", "requests", 86_400, 86_400),
        key_for("alice", "requests", 3_600, 90_000),
        key_for("alice", "tokens", 0, 0),
        key_for("bob", "requests", 0, 0),
    ];
    for (amount, k) in (1u64..).zip(&keys) {
        let op = OpId {
            identity: k.identity.clone(),
            meter: k.meter.clone(),
            request_id: format!("op-{amount}"),
            phase: Phase::Reserve,
        };
        apply(&store, &op, &[unlimited(k)], amount).await;
    }
    for (amount, k) in (1u64..).zip(&keys) {
        assert_eq!(get(&store, k).await, amount, "{k:?}");
    }
}

/// Many concurrent callers racing for the last units of a limit: exactly
/// `limit` of them are applied, every bound of an op moves together, and the
/// final count is the limit.
pub async fn concurrent_applies_never_exceed_limit<S: UsageStore + 'static>(store: S) {
    const TASKS: usize = 32;
    const PER_TASK: usize = 20;
    const LIMIT: u64 = 150;
    let store = Arc::new(store);
    let now = unix_now();
    let tasks: Vec<_> = (0..TASKS)
        .map(|task| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let (loose, tight) = (key(now), key(0));
                let mut applied = 0u64;
                for n in 0..PER_TASK {
                    let op = op(&format!("t{task}-{n}"), Phase::Reserve);
                    let bounds = [limited(&loose, 10_000), limited(&tight, LIMIT)];
                    match apply(&*store, &op, &bounds, 1).await {
                        Apply::Applied { used } => {
                            assert!(used[1] <= LIMIT, "a total past the limit: {used:?}");
                            applied += 1;
                        }
                        Apply::Rejected { index, used } => {
                            assert_eq!((index, used), (1, LIMIT), "rejected below the limit");
                        }
                    }
                }
                applied
            })
        })
        .collect();
    let mut applied = 0;
    for task in tasks {
        applied += task.await.expect("task");
    }
    assert_eq!(applied, LIMIT);
    assert_eq!(get(&*store, &key(0)).await, LIMIT);
    assert_eq!(
        get(&*store, &key(now)).await,
        LIMIT,
        "bounds of one op move together"
    );
}

/// Many concurrent deliveries of one op are applied once and all see the
/// same result.
pub async fn concurrent_repeats_of_one_op_apply_once<S: UsageStore + 'static>(store: S) {
    let store = Arc::new(store);
    let tasks: Vec<_> = (0..64)
        .map(|_| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                let k = key(0);
                apply(&*store, &op("dup", Phase::Settle), &[unlimited(&k)], 7).await
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.expect("task"), Apply::Applied { used: vec![7] });
    }
    assert_eq!(get(&*store, &key(0)).await, 7);
}

/// Many concurrent releases of one op: exactly one succeeds and the amount
/// comes off once.
pub async fn concurrent_releases_undo_once<S: UsageStore + 'static>(store: S) {
    let store = Arc::new(store);
    let k = key(0);
    apply(
        &*store,
        &op("keep", Phase::Reserve),
        &[limited(&k, 100)],
        10,
    )
    .await;
    apply(&*store, &op("undo", Phase::Reserve), &[limited(&k, 100)], 4).await;
    let tasks: Vec<_> = (0..64)
        .map(|_| {
            let store = Arc::clone(&store);
            tokio::spawn(async move { release(&*store, &op("undo", Phase::Reserve)).await })
        })
        .collect();
    let mut released = 0;
    for task in tasks {
        released += usize::from(task.await.expect("task"));
    }
    assert_eq!(released, 1);
    assert_eq!(get(&*store, &k).await, 10);
}

/// `compact` drops exactly the keys whose `retain_until` has passed — never a
/// `Total` key, never one still retained — and counts what it dropped.
pub async fn compact_drops_expired_keys_only<S: UsageStore>(store: S, ledger_ttl: Duration) {
    let t0 = unix_now();
    let (short, long, total) = (key(t0), key(t0 + 1), key(0));
    let bounds = [
        Bound {
            key: &short,
            limit: Some(100),
            retain_until: Some(t0 + 5),
        },
        Bound {
            key: &long,
            limit: Some(100),
            retain_until: Some(t0 + 20),
        },
        unlimited(&total),
    ];
    apply(&store, &op("keys", Phase::Reserve), &bounds, 3).await;
    let t1 = unix_now();
    assert_eq!(
        store.compact(t0 + 5).await.expect("compact"),
        0,
        "retained through its retain_until"
    );
    assert_eq!(store.compact(t0 + 6).await.expect("compact"), 1);
    assert_eq!(
        (
            get(&store, &short).await,
            get(&store, &long).await,
            get(&store, &total).await
        ),
        (0, 3, 3)
    );
    let past_ttl = t1 + i64::try_from(ledger_ttl.as_secs()).expect("ttl") + 1;
    assert_eq!(
        store.compact(past_ttl).await.expect("compact"),
        2,
        "the long-window key and the op's ledger entry"
    );
    assert_eq!(get(&store, &total).await, 3, "a Total key is kept forever");
    assert_eq!(
        store.compact(i64::MAX).await.expect("compact"),
        0,
        "nothing left to drop"
    );
}

/// `compact` keeps ledger entries younger than the TTL and drops older ones
/// in every state; past it an op is forgotten: `release` is `false` and a
/// redelivery is applied again.
pub async fn compact_drops_ledger_entries_past_the_ttl<S: UsageStore>(
    store: S,
    ledger_ttl: Duration,
) {
    let ttl = i64::try_from(ledger_ttl.as_secs()).expect("ttl");
    assert!(
        ttl >= 60,
        "the suite needs a ledger TTL of at least a minute"
    );
    let k = key(0);
    let t0 = unix_now();
    let open = op("open", Phase::Reserve);
    let settled = op("settled", Phase::Reserve);
    let released = op("released", Phase::Reserve);
    let recorded = op("recorded", Phase::Settle);
    for charge in [&open, &settled, &released] {
        apply(&store, charge, &[limited(&k, 100)], 2).await;
    }
    apply(&store, &recorded, &[unlimited(&k)], 10).await;
    store.settle(&settled).await.expect("settle");
    assert!(release(&store, &released).await);
    let t1 = unix_now();

    assert_eq!(
        store.compact(t0 + ttl - 30).await.expect("compact"),
        0,
        "every entry is younger than the TTL"
    );
    assert_eq!(
        apply(&store, &recorded, &[unlimited(&k)], 10).await,
        Apply::Applied { used: vec![16] },
        "still deduplicated within the TTL"
    );
    assert_eq!(get(&store, &k).await, 14);

    assert_eq!(
        store.compact(t1 + ttl + 1).await.expect("compact"),
        4,
        "open, settled, released and Settle-phase entries all expire"
    );
    assert!(
        !release(&store, &open).await,
        "a late refund past the TTL finds nothing; the charge stands"
    );
    assert_eq!(get(&store, &k).await, 14);
    assert_eq!(
        apply(&store, &recorded, &[unlimited(&k)], 10).await,
        Apply::Applied { used: vec![24] },
        "a redelivery past the TTL is counted again"
    );
}
