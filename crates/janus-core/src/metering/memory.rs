//! `MemoryUsageStore`: a mutex over hash maps (`docs/plugin-contract`
//! "The store").

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;

use super::ledger::{evaluate, key_expired, ledger_cutoff, unix_now, AutoCompact};
use crate::contract::metering::{
    Apply, Bound, Durability, MeterKey, OpId, Phase, UsageError, UsageStore, DEFAULT_LEDGER_TTL,
};

/// A `Volatile` [`UsageStore`]: correct for one plugin process, loses every
/// count on restart. The right store for `Fixed` windows a restart may
/// legitimately reset; `Meter::new` refuses it a `Total` quota.
///
/// Expired windows of an `(identity, meter)` are swept whenever that pair is
/// written; [`UsageStore::compact`] sweeps every pair and the ledger, and
/// runs by itself from `apply` once per a quarter of the ledger TTL, so the
/// maps stay bounded by recent traffic even if the plugin never calls it.
pub struct MemoryUsageStore {
    state: Mutex<State>,
    ledger_ttl: Duration,
    auto_compact: AutoCompact,
}

#[derive(Default)]
struct State {
    /// identity → meter → (period_secs, window_start) → count.
    counts: HashMap<String, HashMap<String, HashMap<WindowId, Count>>>,
    ledger: HashMap<OpId, Entry>,
}

/// A key's window within its `(identity, meter)`.
type WindowId = (u64, i64);

fn window_id(key: &MeterKey) -> WindowId {
    (key.period_secs, key.window_start)
}

struct Count {
    used: u64,
    retain_until: Option<i64>,
}

struct Entry {
    /// What the op returned, replayed to a repeat.
    result: Apply,
    /// The keys a still-reversible op added `amount` to.
    touched: Vec<MeterKey>,
    amount: u64,
    /// An applied `Reserve` op not yet released or settled.
    reversible: bool,
    created_at: i64,
}

impl MemoryUsageStore {
    /// A store whose ledger entries live for [`DEFAULT_LEDGER_TTL`].
    pub fn new() -> Self {
        Self::with_ledger_ttl(DEFAULT_LEDGER_TTL)
    }

    /// A store whose ledger entries are dropped by `compact` once older than
    /// `ledger_ttl`.
    pub fn with_ledger_ttl(ledger_ttl: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            ledger_ttl,
            auto_compact: AutoCompact::new(ledger_ttl, unix_now()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, UsageError> {
        self.state
            .lock()
            .map_err(|_| UsageError::Backend("memory usage store lock poisoned".into()))
    }
}

impl Default for MemoryUsageStore {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    fn count(&self, key: &MeterKey) -> u64 {
        self.counts
            .get(&key.identity)
            .and_then(|meters| meters.get(&key.meter))
            .and_then(|windows| windows.get(&window_id(key)))
            .map_or(0, |count| count.used)
    }

    fn windows_mut(&mut self, key: &MeterKey) -> &mut HashMap<WindowId, Count> {
        self.counts
            .entry(key.identity.clone())
            .or_default()
            .entry(key.meter.clone())
            .or_default()
    }

    /// The windows of `key`'s `(identity, meter)`, if any were ever written.
    fn existing_windows_mut(&mut self, key: &MeterKey) -> Option<&mut HashMap<WindowId, Count>> {
        self.counts
            .get_mut(&key.identity)
            .and_then(|meters| meters.get_mut(&key.meter))
    }

    /// Drop every expired window and every ledger entry stamped before
    /// `ledger_cutoff`. Returns how many were dropped.
    fn compact(&mut self, now: i64, ledger_cutoff: i64) -> u64 {
        let mut dropped = 0u64;
        self.counts.retain(|_, meters| {
            meters.retain(|_, windows| {
                let before = windows.len();
                windows.retain(|_, count| !key_expired(count.retain_until, now));
                dropped += (before - windows.len()) as u64;
                !windows.is_empty()
            });
            !meters.is_empty()
        });
        let before = self.ledger.len();
        self.ledger
            .retain(|_, entry| entry.created_at >= ledger_cutoff);
        dropped + (before - self.ledger.len()) as u64
    }

    /// Drop the expired windows of `key`'s `(identity, meter)`.
    fn sweep(&mut self, key: &MeterKey, now: i64) {
        if let Some(windows) = self.existing_windows_mut(key) {
            windows.retain(|_, count| !key_expired(count.retain_until, now));
        }
    }
}

#[async_trait]
impl UsageStore for MemoryUsageStore {
    async fn apply(
        &self,
        op: &OpId,
        bounds: &[Bound<'_>],
        amount: u64,
    ) -> Result<Apply, UsageError> {
        let mut state = self.lock()?;
        if let Some(entry) = state.ledger.get(op) {
            return Ok(entry.result.clone());
        }
        let now = unix_now();
        if self.auto_compact.due(now) {
            state.compact(now, ledger_cutoff(now, self.ledger_ttl));
        } else {
            for bound in bounds {
                state.sweep(bound.key, now);
            }
        }
        let current: Vec<u64> = bounds.iter().map(|bound| state.count(bound.key)).collect();
        let result = evaluate(&current, bounds.iter().map(|bound| bound.limit), amount);
        let mut touched = Vec::new();
        if let Apply::Applied { used } = &result {
            for (bound, &total) in bounds.iter().zip(used) {
                state.windows_mut(bound.key).insert(
                    window_id(bound.key),
                    Count {
                        used: total,
                        retain_until: bound.retain_until,
                    },
                );
                touched.push(bound.key.clone());
            }
        }
        let reversible = op.phase == Phase::Reserve && matches!(result, Apply::Applied { .. });
        if !reversible {
            touched.clear();
        }
        state.ledger.insert(
            op.clone(),
            Entry {
                result: result.clone(),
                touched,
                amount,
                reversible,
                created_at: now,
            },
        );
        Ok(result)
    }

    async fn release(&self, op: &OpId) -> Result<bool, UsageError> {
        let mut state = self.lock()?;
        let (touched, amount) = match state.ledger.get_mut(op) {
            Some(entry) if entry.reversible => {
                entry.reversible = false;
                (std::mem::take(&mut entry.touched), entry.amount)
            }
            _ => return Ok(false),
        };
        for key in &touched {
            if let Some(count) = state
                .existing_windows_mut(key)
                .and_then(|windows| windows.get_mut(&window_id(key)))
            {
                count.used = count.used.saturating_sub(amount);
            }
        }
        Ok(true)
    }

    async fn settle(&self, op: &OpId) -> Result<(), UsageError> {
        let mut state = self.lock()?;
        if let Some(entry) = state.ledger.get_mut(op) {
            entry.reversible = false;
            entry.touched = Vec::new();
        }
        Ok(())
    }

    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError> {
        Ok(self.lock()?.count(key))
    }

    async fn compact(&self, now: i64) -> Result<u64, UsageError> {
        let cutoff = ledger_cutoff(now, self.ledger_ttl);
        Ok(self.lock()?.compact(now, cutoff))
    }

    fn durability(&self) -> Durability {
        Durability::Volatile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::usage_store_conformance_tests!(|| (MemoryUsageStore::new(), ()), DEFAULT_LEDGER_TTL);

    fn key(meter: &str, window_start: i64) -> MeterKey {
        MeterKey {
            identity: "alice".into(),
            meter: meter.into(),
            period_secs: 60,
            window_start,
        }
    }

    fn reserve(meter: &str, request_id: &str) -> OpId {
        OpId {
            identity: "alice".into(),
            meter: meter.into(),
            request_id: request_id.into(),
            phase: Phase::Reserve,
        }
    }

    #[tokio::test]
    async fn memory_store_is_volatile() {
        assert_eq!(MemoryUsageStore::new().durability(), Durability::Volatile);
        assert_eq!(MemoryUsageStore::default().ledger_ttl, DEFAULT_LEDGER_TTL);
    }

    #[tokio::test]
    async fn memory_store_sweeps_expired_windows_of_the_written_pair_on_write() {
        let store = MemoryUsageStore::new();
        let now = unix_now();
        let stale = key("requests", now - 120);
        let other_meter_stale = key("tokens", now - 120);
        let fresh = key("requests", now);
        for (request_id, key) in [("r1", &stale), ("r2", &other_meter_stale)] {
            let bound = Bound {
                key,
                limit: None,
                retain_until: Some(now - 60),
            };
            store
                .apply(&reserve(&key.meter, request_id), &[bound], 4)
                .await
                .expect("apply");
        }
        // Written already expired, so still present until the next write.
        assert_eq!(store.get(&stale).await.expect("get"), 4);
        let bound = Bound {
            key: &fresh,
            limit: None,
            retain_until: Some(now + 60),
        };
        store
            .apply(&reserve("requests", "r3"), &[bound], 1)
            .await
            .expect("apply");
        assert_eq!(store.get(&stale).await.expect("get"), 0);
        assert_eq!(store.get(&fresh).await.expect("get"), 1);
        // A different (identity, meter) is not swept by this write.
        assert_eq!(store.get(&other_meter_stale).await.expect("get"), 4);
    }

    #[tokio::test]
    async fn memory_store_compacts_itself_without_a_compact_call() {
        let store = MemoryUsageStore::new();
        let now = unix_now();
        let stale = key("tokens", now - 120);
        let bound = Bound {
            key: &stale,
            limit: None,
            retain_until: Some(now - 60),
        };
        let old = reserve("tokens", "old");
        store.apply(&old, &[bound], 4).await.expect("apply");
        // Age the entry past the ledger TTL, as an hour of wall clock would.
        store
            .lock()
            .expect("lock")
            .ledger
            .get_mut(&old)
            .expect("entry")
            .created_at = now - DEFAULT_LEDGER_TTL.as_secs() as i64 - 1;
        let fresh = key("requests", now);
        let write = |request_id: &'static str| {
            let store = &store;
            let fresh = &fresh;
            async move {
                let bound = Bound {
                    key: fresh,
                    limit: None,
                    retain_until: Some(now + 60),
                };
                store
                    .apply(&reserve("requests", request_id), &[bound], 1)
                    .await
                    .expect("apply");
            }
        };
        write("r1").await;
        // Within the interval a write sweeps only its own pair.
        assert_eq!(store.lock().expect("lock").ledger.len(), 2);
        assert_eq!(store.get(&stale).await.expect("get"), 4);
        store.auto_compact.expire();
        write("r2").await;
        let state = store.lock().expect("lock");
        assert!(!state.ledger.contains_key(&old), "aged entry dropped");
        assert_eq!(state.ledger.len(), 2);
        assert!(
            !state.counts["alice"].contains_key("tokens"),
            "another pair's expired window swept"
        );
    }
}
