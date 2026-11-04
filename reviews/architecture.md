---
title: architecture
type: reference
status: draft
---

VERDICT: PASS REVIEWER: independent-architecture-reviewer (claude-sonnet-5-5)

# Architecture review: whole workspace (janus-core, janus-daemon, janus-cli, ui)

No open high or medium findings. Seven low findings remain. The design holds up
as a system: the layering is clean, secrets are redacted, the admin plane is
separate from the data plane, and the daemon fails closed and shuts down in
bounded time.

## Method

- Scratch copy at `/tmp/rv-arch` with `CARGO_TARGET_DIR=/tmp/rv-arch-target`.
  Both directories were deleted afterwards. The repo tree and its `target/` were
  not touched.
- Per-dependency removal test: for every entry in `[dependencies]` of each
  crate, I removed it and ran
  `cargo check -p <crate> --all-targets --all-features --offline`. I repeated
  the test for every `[dev-dependencies]` entry.
- Workspace-level `[workspace.dependencies]` entries not referenced by any
  member manifest.
- UI dependencies checked by grep (`ui/package.json` against `ui/src` and the
  config files).
- `node scripts/gates/dup-scan.mjs crates 10` and `... 6`.
- `cargo clippy --workspace --all-targets` (0 warnings).
  `cargo test -p janus-core` with and without `--all-features`.
- Reading of `run.rs`, `dataplane/serve.rs`, `dataplane/pipeline.rs`,
  `admin/listen.rs`, `admin/mod.rs`, the credential contract, the CLI connect
  and credential code, and the UI token store.
- 15 doc claims sampled (see the table at the end).

## What is sound

- **Layering.** `janus-core` has no dependency on the daemon or the CLI. The
  daemon depends on core. The CLI depends on core and on the daemon library.
  There is no dependency cycle.
- **Secrets.** `Secret`, `MaterialDocument` and `TokenHash` have hand-written
  redacted `Debug`. `CredentialMaterial` is not `Serialize`. The CLI takes no
  secret on argv (stdin or a file only). The UI keeps the token in
  `sessionStorage` only.
- **Fail-closed.** Every plugin failure maps to a rejected request
  (`dataplane/pipeline.rs:114`). A credential that cannot be had abandons the
  request (`pipeline.rs:126`). An identity leak in the rewrite is a hard
  failure.
- **Admin boundary.** The admin plane is a unix socket (`0600`, checked on
  start) or an opt-in TCP listener that requires a token and TLS. The data plane
  has no admin routes, and the token file is `0600`-checked.
- **Lifecycle.**
  - `Daemon::shutdown` cancels the listeners.
  - The data plane drains for 30 s, then aborts the remaining connections.
  - The tracker wait is timed.
  - The request log is flushed, with a timeout, before admin wait.
  - Admin connections are bounded by `SHUTDOWN_GRACE`.
- **Backpressure.**
  - The settlement path uses `try_send` with a drop counter.
  - The Notify queue is bounded.
  - The SSE tail has a bounded buffer.
  - Both listeners have connection caps.
  - All listeners share one connection builder with a header-read timeout.
- **Config surface.** `deny_unknown_fields` appears 12 times. The doc defaults I
  sampled match the code.
- **Dup scan.** Threshold 10 reports OK (100 files). Threshold 6 reports only
  test-code repetition (`cli/tests/cli.rs`, `jwt.rs` tests, `meter.rs` tests,
  `binding.rs` tests). I found no copy-paste in production code.
- **Stubs and TODOs.** None found in production code. There is no
  `allow(dead_code)` or `allow(unused)`.

## Findings

### L1 (low): unused dependency `rustls-pki-types` in janus-daemon
- File: `crates/janus-daemon/Cargo.toml` (the
  `rustls-pki-types.workspace = true` line).
- Evidence: removing the entry leaves
  `cargo check -p janus-daemon --all-targets --all-features` green. There is no
  `rustls_pki_types` reference in `crates/janus-daemon/src` or its tests.
- Context: the workspace manifest says member crates list dependencies "up
  front" for later modules. Several such entries have since gained users. This
  one has not.
- Fix: remove the line. If a later module needs it, add it then.
- All other dependencies in the three crates, all dev-dependencies and all UI
  dependencies are used. `@testing-library/dom` is a peer dependency of
  `@testing-library/react`, and `@types/node` is used for the Vite and tsc
  config. Every `[workspace.dependencies]` entry has a user.

### L2 (low): janus-cli links the whole janus-daemon library
- File: `crates/janus-cli/Cargo.toml` (`janus-daemon.workspace = true`). Uses:
  `connect.rs:20`, `commands/mod.rs:12,43`, `commands/status.rs:12`,
  `commands/log.rs:66,437`.
- Evidence:
  - The CLI imports `DaemonConfig`, `check_config`, `plugin::tls_init`,
    `store::OUTCOMES` and `store::MAX_LIMIT`.
  - `docs/architecture.md` (janus-cli section) says the CLI "contains no proxy
    logic of its own". Its binary nevertheless statically links axum, bundled
    SQLite, tonic, the data plane and the pool.
  - `janus check-config` runs the full daemon `prepare()`, which may create the
    data directory, `janus.db` and the pool directory (see the doc comment on
    `check_config` in `run.rs`). A "check" command with side effects is
    surprising.
- Fix: say so in `architecture.md`, or move `OUTCOMES`, `MAX_LIMIT` and the TLS
  generator into core (or a small config crate) so the CLI does not depend on
  the daemon. Make `check-config` side-effect free, or document `--dry-run`.

### L3 (low): the SQLite UsageStore is feature-gated and runs only with `--all-features`
- File: `crates/janus-core/Cargo.toml` (`[features] sqlite`, `testing`). Gated
  modules: `metering/mod.rs:15,20,29`.
- Evidence:
  - `cargo test -p janus-core` runs 231 tests. With `--all-features` it runs 254
    plus 1 more doctest.
  - Nothing in the workspace enables the `sqlite` or `testing` features.
    `metering/sqlite.rs` (808 lines) and the conformance suite compile and run
    only under an explicit flag.
  - The metering subsystem (`Meter`, memory store, sqlite store, conformance,
    about 3k lines) has no consumer in the daemon or CLI. It is plugin-facing
    library surface, as the spec says. In-repo it is exercised only by its own
    tests.
  - `scripts/gates/cargo-tests.mjs` takes its cargo arguments from the caller,
    so whether the gate covers these tests depends on how it is invoked.
- Fix: invoke the test gate with `--all-features`, or add a
  `[dev-dependencies] janus-core = { features = ["sqlite","testing"] }`
  self-entry (or a workspace CI step). That way the shipped store cannot go
  untested.

### L4 (low): single-implementation traits and adapter indirection
- Evidence:
  - `ExtractorFactory` has one implementation (`identity/factory.rs`).
  - `CredentialPool` has one implementation (`pool/mod.rs`, `FilePool`).
  - `MaterialResolver` has two implementations (`identity/binding.rs` and
    `Materials<'_>` in `run.rs`), but the second is an adapter around the first.
  - `OpaqueRegistry` has one real implementation and a test fake.
  - `Arc<dyn CredentialPool>` and `Arc<dyn OpaqueRegistry>` are threaded through
    admin and the data plane.
  - Genuine multiplicity exists for `IdentityExtractor`, `UpstreamImpl` (3 real
    implementations), `PluginClient` (gRPC and linked) and `UsageStore`.
- Context: the traits sit at the documented embedding seams, and the test fakes
  use them. This is not harmful. It is mild over-abstraction.
- Fix: none required. Do not add more single-implementation traits. If
  `ExtractorFactory` stays internal, consider making it a concrete type.

### L5 (low): doc drift on where the request log lives
- Files: `docs/architecture.md`, janus-core responsibilities ("The request log.
  The per-request record...") against
  `crates/janus-daemon/src/store/request_log.rs` (1223 lines) and
  `crates/janus-core/src/contract/request_log.rs`.
- Evidence: core holds only the record and outcome types. The writer, queue,
  retention and tail all live in the daemon (`RequestLog`, `store/`). The doc
  lists the request log as a core responsibility.
- Fix: reword to "the request-log record types (the store is in the daemon)".

### L6 (low): no hardening headers or inbound slow-body bound
- Files: `crates/janus-daemon/src/admin/ui.rs`, `admin/listen.rs:308`.
- Evidence:
  - There are no `Content-Security-Policy`, `X-Content-Type-Options` or
    `frame-ancestors` response headers anywhere in the daemon. A grep for
    `Content-Security`, `nosniff` and `X-Frame` found nothing. The UI listener
    serves a token-bearing app from `ServeDir`.
  - There is no limit on repeated bad admin-token attempts.
  - `connection_builder` bounds header reads only (`HEADER_READ_TIMEOUT`). There
    is no idle timeout on an inbound request body, and HTTP/2 has a timer but no
    keep-alive or idle settings. A client that trickles a body holds one of the
    `max_connections` slots until the upstream read-idle timeout acts. That
    timeout covers the upstream side, not the caller side.
- Fix: add CSP, nosniff and `frame-ancestors 'none'` on the UI listener. Add a
  body-read idle timeout on the data plane. Consider a small backoff on repeated
  token failures.

### L7 (low): UI wire types are hand-copied from Rust
- File: `ui/src/api/types.ts` (about 280 lines) against core and daemon serde
  types (`CredentialMetadata`, `IdentityRecord`, `RequestRecord`,
  `PluginStatus`, `DaemonStatus` and others).
- Evidence: shapes such as `OutcomeName`, `PluginFailureClass` and
  `AbandonStage` are re-declared by hand. Nothing checks them against the Rust
  enums. A new Rust outcome compiles on both sides, and the UI silently
  mis-renders it.
- Fix: add a gate that compares the TS unions with the Rust enum variants, or
  generate the types.

## Doc claims sampled (15)

| Claim | Result |
|---|---|
| Admin socket is mode `0600` | Confirmed (`admin/listen.rs:190,208`) |
| Admin token file is `0600` and checked | Confirmed (`admin/token.rs:49,98`) |
| Non-loopback admin bind requires TLS or `allow_plaintext` | Confirmed (config validation) |
| Token compared in constant time | Confirmed (`admin/token.rs`, `subtle`) |
| Data plane has no admin routes | Confirmed (separate routers) |
| `max_connections` default 1024, must be above zero | Confirmed (`config.rs:91,193`) |
| `request_log.retain` default 30d | Confirmed (`config.rs:93`) |
| Plugin `timeout_ms` default 250, never 0 | Confirmed (`config.rs:89`) |
| Plugin failure fails the request closed | Confirmed (`pipeline.rs:114`) |
| No plugin configured means zero plugin calls | Confirmed (plugin handle is `Option`) |
| Admin token stored in `sessionStorage` only | Confirmed (`ui/src/auth/token.ts`) |
| No CLI flag takes a secret | Confirmed (`main.rs`, `credential.rs`) |
| Library/binary split: the stock binary is a few lines over `run` | Confirmed (`main.rs`, `run.rs`) |
| Graceful shutdown with a grace period | Confirmed (`serve.rs`, `run.rs`) |
| "The request log" is a janus-core responsibility | **Drift** (see L5) |

The doc also says the CLI "contains no proxy logic", but its dependency graph
includes all of it (see L2).
