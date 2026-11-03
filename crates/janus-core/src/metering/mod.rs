//! Metering primitives a policy plugin links: [`Meter`], [`MemoryUsageStore`],
//! `SqliteUsageStore` (feature `sqlite`) and the `UsageStore` conformance
//! suite (feature `testing`). Codes against [`crate::contract::metering`].
//! See `docs/plugin-contract` "Metering primitives".
//!
//! Counting is mechanical; deciding what an `Exceeded` means is the plugin's
//! policy. A plugin meters in two phases keyed by the contract's
//! `request_id`: at `ResolveRoute` it `check`s the dimensions billed after
//! the fact and `charge`s the ones known up front; on `call_completed` it
//! `record`s the real figures and `settle`s the charge; on `call_abandoned`
//! it `refund`s the charge. Every count-changing operation is idempotent per
//! `request_id` within the store's ledger TTL, and a refund lands in the
//! window the charge landed in.

#[cfg(any(test, feature = "testing"))]
pub mod conformance;
mod ledger;
mod memory;
mod meter;
#[cfg(feature = "sqlite")]
mod sqlite;

pub use crate::contract::metering::{
    Apply, Bound, Charge, Durability, MeterKey, OpId, Phase, Quota, UsageError, UsageStore, Window,
    DEFAULT_LEDGER_TTL,
};
pub use memory::MemoryUsageStore;
pub use meter::Meter;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteUsageStore;
