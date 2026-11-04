use janus_core::metering::*;
use async_trait::async_trait;

mod mem { janus_core::usage_store_conformance_tests!(|| (janus_core::metering::MemoryUsageStore::new(), ()), janus_core::metering::DEFAULT_LEDGER_TTL); }
mod sql { janus_core::usage_store_conformance_tests!(|| { let d = tempfile::tempdir().unwrap(); (janus_core::metering::SqliteUsageStore::open(d.path().join("u.db")).unwrap(), d) }, janus_core::metering::DEFAULT_LEDGER_TTL); }

// a "third-party" store: a newtype that forwards to the memory store, defined outside janus-core
struct Third(MemoryUsageStore);
#[async_trait]
impl UsageStore for Third {
    async fn apply(&self, op: &OpId, bounds: &[Bound<'_>], amount: u64) -> Result<Apply, UsageError> { self.0.apply(op, bounds, amount).await }
    async fn release(&self, op: &OpId) -> Result<bool, UsageError> { self.0.release(op).await }
    async fn settle(&self, op: &OpId) -> Result<(), UsageError> { self.0.settle(op).await }
    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError> { self.0.get(key).await }
    async fn compact(&self, now: i64) -> Result<u64, UsageError> { self.0.compact(now).await }
    fn durability(&self) -> Durability { self.0.durability() }
}
mod third { janus_core::usage_store_conformance_tests!(|| (super::Third(janus_core::metering::MemoryUsageStore::new()), ()), janus_core::metering::DEFAULT_LEDGER_TTL); }
