//! The contract every other Janus module codes against.
//!
//! Types, traits and config shapes only — the behavior behind them lives in
//! [`crate::identity`], [`crate::pool`], [`crate::metering`],
//! [`crate::proxy`] and in `janus-daemon`. Everything here is derived from
//! `docs/*.md` and `proto/janus.proto`, which remain the spec of record.
//!
//! - [`identity`] — `IdentityExtractor`, `RequestHead`, `ExtractedIdentity`,
//!   `PresentedAt`, `ExtractionError`/`RejectionKind`
//!   (`docs/caller-identity#extractor`), and the construction contract:
//!   `ExtractorInput` handed to an `ExtractorFactory`.
//! - [`registry`] — the opaque-identity registry the `opaque` method probes
//!   and the daemon's store owns: `OpaqueRegistry`, `RegistryEntry`,
//!   `IdentityRecord`, `TokenHash` (`docs/caller-identity#registry`).
//! - [`binding`] — extractor binding config (`[identity]`,
//!   `[upstream.identity]`, route `identity`), the merge rule, and its
//!   resolution into a typed `BindingSpec` (`docs/caller-identity#binding`).
//! - [`plugin`] — the plugin contract in Rust: `ResolveRoute`, `RouteOutcome`,
//!   `NotifyEvent`, `PluginError`/`PluginFailureClass`, the `JanusPlugin`
//!   (linked) and `PluginClient` (daemon-side) traits, the conversions to and
//!   from the wire types (`docs/plugin-contract`), and the admin surface's
//!   `plugin` status object (`PluginStatus`, `PluginMode`, `PluginHealth`).
//! - [`proto`] — the generated wire types and gRPC client/server for
//!   `proto/janus.proto`.
//! - [`metering`] — `UsageStore` and the data types `Meter` is built from
//!   (`docs/plugin-contract#usage-stores`).
//! - [`credential`] — credential metadata, live material and `Secret`, the
//!   `ref` grammar, and the `CredentialPool` interface the data plane and
//!   admin surface share (`docs/admin-surface#credentials-endpoint`).
//! - [`upstream`] — the upstream names, and `UpstreamImpl`, the narrow
//!   per-upstream surface (`docs/credential-substitution#general-purpose`).
//! - [`request_log`] — `RequestRecord` and its outcome and stage vocabulary
//!   (`docs/admin-surface#request-log`).
//! - [`paths`] — the daemon data directory and the fixed files inside it.

pub mod binding;
pub mod credential;
pub mod identity;
pub mod metering;
pub mod paths;
pub mod plugin;
pub mod proto;
pub mod registry;
pub mod request_log;
pub mod selection;
pub mod upstream;
