---
title: contract 1.1.1
type: reference
status: draft
---

VERDICT: PASS REVIEWER: independent-reviewer (claude-opus-5-5), contract
scaffold 1.1.1, re-review of revision 2

# Review: contract scaffold (janus-core `contract`, janus-daemon `contract`, run.rs, Cargo manifests)

This is a from-scratch re-review of the current files. I read all of these in
full against `docs/*.md` and `proto/janus.proto`:

- the workspace `Cargo.toml`;
- `crates/janus-core/{Cargo.toml,build.rs,src/lib.rs}`;
- every file under `crates/janus-core/src/contract/`: mod, identity, registry,
  binding, plugin, proto, metering, credential, upstream, request_log and paths;
- `crates/janus-daemon/{Cargo.toml,src/lib.rs,src/run.rs}`;
- `crates/janus-daemon/src/contract/{mod,config,embedded}.rs`.

Unused crate dependencies are excluded from this pass by coordinator direction.
They are retained until the final integration pass.

Commands run:
- `cargo check --workspace --all-targets`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test -p janus-core -p janus-daemon`: 32 + 12 pass.
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p janus-core -p janus-daemon`:
  clean.
- `node scripts/gates/no-stubs.mjs` on the in-scope paths: NO STUBS OK (17
  files).
- `node scripts/gates/dup-scan.mjs crates 8`: DUP SCAN OK.
- No `#[allow]`, TODO, `todo!` or `unimplemented!` in scope.

## What is right (new in this revision)

- **`contract::registry`.**
  - `OpaqueRegistry::lookup` is a sync hash-map probe that touches
    `last_seen_at` in memory.
  - `TokenHash` is SHA-256 of the token, with a test vector.
  - `IdentityRecord::verdict` checks revoked before expired, which keeps the
    log's `revoked` and `expired` kinds distinguishable. The `<= now` boundary
    is tested.
  - This closes the core/daemon gap for the `opaque` method.
- **`ExtractorInput` / `ExtractorFactory`.** These pin construction with
  resolved material:
  - secret bytes for `hmac`;
  - JWKS URL or PEM bytes for `jwt`;
  - the registry for `opaque`.
- **`CredentialPool`.**
  - It covers the data plane's `resolve` and `report` and every
    `/v1/credentials` operation.
  - Its doc pins that a `Ref` never falls through to the default chain
    (lifecycle step 4).
  - `validate_credential_ref` is now the single public ref grammar, and config
    uses it.
- **`UpstreamImpl`.**
  - It covers the `credential-substitution#general-purpose` list item for item,
    including the allowlist-constructed outbound request and the `presented_at`
    cross-check.
  - `classify` produces `UpstreamFeedback`, the same type
    `CredentialPool::report` takes.
- **`contract::request_log`.**
  - The `Outcome` vocabulary and the `RejectionKind` / `PluginFailureClass`
    qualifiers match `admin-surface#request-log`.
  - It is serde-flattened onto `RequestRecord`, and round-trip tests cover every
    outcome.
  - `CallAbandoned.stage` is now the typed `AbandonStage`, shared with the log.
- **`PluginStatus` / `PluginHealth` / `PluginMode`.**
  - These match the `/v1/status` `plugin` object, including `unknown` until the
    first call.
  - `PluginMode` has exactly one definition (core), which config reuses.
- **Binding.**
  - Changing method at the upstream inherits nothing. This is documented as an
    implementation decision, and the tests cover it.
  - Fields the resolved method does not use are refused (`NotApplicable`).
  - `jwks_url` is parsed and must be http(s).
  - Both key fields are refused when empty.
- **Config.**
  - `timeout_ms = 0` is refused.
  - `allow_plaintext` and `[plugin.tls]` are refused where they have no effect.
  - Stray `admin.*` settings without `bind` are refused, and so is TLS together
    with plaintext.
  - Overlapping or `/` prefixes are refused, using whole-segment matching.
  - Route paths are literal, and wildcard characters are refused.
  - Listener collision treats unspecified addresses (`0.0.0.0`, `[::]`) as
    colliding with any address on the same port.
  - Every invented section (`data_dir`, `[secrets]`, `[[credential]]`, `[ui]`)
    is declared in the module docs as an implementation decision.
- **`Secret`.** It holds bytes, compares in constant time via `subtle`, zeroizes
  with volatile writes in `Drop`, and its `Debug`/`Display` are redacted.
  `MaterialDocument` is redacted too.
- **`run.rs` / `lib.rs`.** They no longer claim an entry point that does not
  exist, and the orphaned `RunError` is gone.
- **Tests that previously could not fail now can:**
  - The metering test checks `Arc` forwarding order and results.
  - The failure-class and rejection-kind tests assert lengths.
  - The `Embedded` tests drive `check_against`, including route-level custom
    extractors.
  - The `trusted_header` + `correlation_claim` refusal and the empty
    `correlation_claim` case are covered.
- **Mental mutations.** Each of these breaks a test:
  - removing the method-change reset in `merged_over`;
  - dropping a field from `refuse_inapplicable`'s `set` table;
  - flipping `is_unspecified` in the collision check;
  - removing `validate()` from either `TryFrom`.

## Findings

### Prior findings, verified

- [x] [high] crates/janus-core/src/contract/mod.rs:26 There was no
  opaque-identity registry contract. Resolved: `contract::registry` now provides
  `OpaqueRegistry`, `IdentityRecord` and `TokenHash`, and
  `ExtractorInput::Opaque` carries the registry.
- [x] [high] crates/janus-core/src/contract/credential.rs:100 There was no
  credential-pool interface, and the ref grammar was private to config.
  Resolved: `CredentialPool`, `PoolError`, `CredentialChoice` and
  `UpstreamFeedback` now exist, and `validate_credential_ref` is public and used
  by config.
- [x] [medium] crates/janus-core/src/contract/upstream.rs:12 There was no
  upstream-implementation contract. Resolved: `UpstreamImpl` and `UsageObserver`
  now exist.
- [x] [medium] crates/janus-core/src/contract/plugin.rs:98 The request-log
  record, outcome vocabulary and status object were missing, and `stage` was a
  `String`. Resolved: `contract::request_log` and `PluginStatus` now exist, and
  `stage` is an `AbandonStage`.
- [x] [medium] crates/janus-core/Cargo.toml:10 Unused deps and empty features.
  Waived by coordinator direction: they are retained until the final integration
  pass. The features now carry "reserved for leaf 1.2.3" comments.
- [x] [medium] crates/janus-daemon/src/run.rs:1 The entry point was
  half-finished. Resolved: the docs now say `run` is delivered with the data
  plane, and `RunError` was removed.
- [x] [low] crates/janus-core/src/contract/binding.rs:253 A merge could not
  clear an inherited field. Resolved: a method change inherits nothing, this is
  documented, and `switching_method_at_the_upstream_inherits_nothing` tests it.
- [x] [low] crates/janus-core/src/contract/binding.rs:221 Irrelevant fields were
  silently ignored. Resolved for bindings (`refuse_inapplicable`) and for
  `admin`/`ui`/`plugin.allow_plaintext`. See the new `[plugin.tls]`-on-`unix://`
  finding below for the one remaining case.
- [x] [low] crates/janus-daemon/src/contract/config.rs:685 Prefix, route and
  listener checks were too narrow. Resolved: overlap and `/` are refused, route
  paths are literal, and unspecified-address collisions are caught.
- [x] [low] crates/janus-daemon/src/contract/config.rs:240 `timeout_ms = 0` was
  accepted. Resolved.
- [x] [low] crates/janus-core/src/contract/binding.rs:314 An empty
  `public_key_file` was accepted and `jwks_url` was not parsed. Resolved.
- [x] [low] crates/janus-core/src/contract/credential.rs:75 `Secret` was
  String-only, compared in non-constant time and was not zeroized. Resolved.
- [x] [low] crates/janus-core/src/contract/plugin.rs:266 An empty `upstream` and
  an empty deny reason were accepted. Resolved: `ResolveRoute::validate` and
  `RouteOutcome::validate` run on decode and on linked answers.
- [x] [low] crates/janus-core/src/contract/plugin.rs:361 There was no plugin
  health surface. Resolved: `PluginStatus::record_ok` / `record_failure` are fed
  by the data plane.
- [x] [low] crates/janus-core/src/contract/metering.rs:198 Some tests were weak
  or could not fail. Resolved (see above).
- [x] [low] crates/janus-core/src/contract/metering.rs:52 There was no ledger
  TTL constant and the `window_start` units were unstated. Resolved:
  `DEFAULT_LEDGER_TTL` is one hour, matching `plugin-contract#usage-stores`, and
  `window_start` is documented in unix seconds.

### Still open

- [ ] [low] crates/janus-core/src/contract/paths.rs:18 `JANUS_DATA_DIR`,
  `$HOME/.janus`, `[secrets.<name>]`, `[[credential]] env/file`,
  `[data_plane] bind` and `[ui]` are still absent from `docs/`. They are now
  honestly labelled as implementation decisions in the code, but the spec of
  record should eventually state them, so that the CLI, operators and the docs
  agree.
- [ ] [low] crates/janus-daemon/src/main.rs:12 This file is outside this leaf's
  scope. It still prints "not yet implemented", which `no-stubs.mjs` will flag
  if the daemon crate is ever scanned whole. The later leaf that owns `main.rs`
  needs to replace it.

### New in this revision

- [ ] [low] crates/janus-core/src/contract/registry.rs:103 `RegistryEntry` is
  orphaned. No trait method, function or test in the contract takes or returns
  it, and `OpaqueRegistry` returns a bare `IdentityRecord`.
  - The write side of the registry also has no contract:
    `POST/GET/DELETE /v1/identities` and revoke.
  - This is tolerable if the store and admin leaves share a concrete daemon
    type. But it is asymmetric with `CredentialPool`, which pins the full admin
    surface.
  - Either delete `RegistryEntry` and let the store leaf define its row type, or
    give it a user (for example, a management trait the admin surface codes
    against).
- [ ] [low] crates/janus-core/src/contract/identity.rs:270 `ExtractorFactory` is
  an `async_trait` with exactly one intended implementation
  (`janus_core::identity`) and no second implementation in sight. Custom
  extractors arrive pre-built via `Embedded`, and `ExtractorInput` has no
  `Custom` arm.
  - A plain `async fn build(input) -> Result<Arc<dyn IdentityExtractor>, _>` in
    `janus_core::identity` would serve the daemon equally well.
  - Keep the trait only if the data plane needs to inject a fake factory in
    tests, and say so in its doc comment.
- [ ] [low] crates/janus-core/src/contract/upstream.rs:86 There is mild overlap
  between old and new types:
  - `DecodedMaterial` (`kind`, `secret`, `account_id`, `expires_at`) and
    `CredentialMaterial` (`credential_ref`, `upstream`, `kind`, `secret`,
    `account_id`) repeat three fields.
  - `PoolError::{InvalidMaterial, RefreshFailed}` (`credential.rs:313`) restate
    `UpstreamError::{InvalidMaterial, RefreshFailed}` as `reason: String` rather
    than wrapping them as `#[source] UpstreamError`, which drops the error chain
    the pool's caller could log.
  - This is not blocking. Consider embedding the upstream error as a source.
- [ ] [low] crates/janus-core/src/contract/request_log.rs:113 There are three
  problems around the record's `upstream`, `binding` and `abandoned` outcome:
  - **Unloggable record.** `upstream: Option` documents "None when the path
    matched no upstream". That behaviour is invented, and no `Outcome` value
    fits such a request: `docs/architecture` treats unknown data-plane paths as
    plain 404s, and the outcome vocabulary has no "unrouted" entry. A record
    with `upstream: None` therefore cannot be written truthfully. Either don't
    log unrouted requests (and make `upstream` non-optional), or define the
    outcome.
  - **Unstated kind for an unresolved binding.** `binding: None` "when no
    binding resolved" (line 120) leaves open which `RejectionKind` such a
    rejection carries.
  - **Overlapping outcomes.** `AbandonStage::{Resolve, Credential, Forward}`
    overlap the `plugin_failed`, `credential_unavailable` and `upstream_error`
    outcomes. The contract does not say when the data plane writes `abandoned`
    instead of one of those. State the mapping, so the data plane and the admin
    `?outcome=` filter agree.
- [ ] [low] crates/janus-daemon/src/contract/config.rs:356 `[plugin.tls]` next
  to a `unix://` endpoint is accepted and ignored.
  - That contradicts the module's own "refuse settings without effect" rule.
  - The stated justification is that the documented example combines them, but
    the example's own comment says the block is "Required when the endpoint
    scheme is https://". The example is showing both alternatives on one line,
    not prescribing TLS over a unix socket.
  - Refusing it, as is already done for linked and `http://` endpoints, would be
    consistent.
