---
title: Development guide
type: reference
status: draft
tags: [janus, development, contributing]
---

# Development guide

How to build, test, verify and extend Janus. The contribution process (issues,
pull requests, conduct) is in `CONTRIBUTING.md`; this document is the technical
reference it points at. For what the system is, start with
<ref target="docs/index">the overview</ref> and
<ref target="docs/architecture">the architecture</ref>.

## Layout {#layout}

| Path | What it is |
|------|------------|
| `crates/janus-core` | The plugin contract (generated from `proto/janus.proto`), identity extractors, credential pool, the per-upstream proxy implementations, forwarding, and metering primitives with their `UsageStore`s. |
| `crates/janus-daemon` | The long-running process: data plane, admin API, UI listener, request log and opaque-identity registry in SQLite, plugin client. A library plus a thin binary, so a custom build can call `janus_daemon::run`. |
| `crates/janus-cli` | The `janus` admin CLI, a thin client of the admin API. |
| `proto/janus.proto` | The plugin contract. |
| `ui/` | The operator UI (React, TypeScript, Vite, Vitest). |
| `docs/` | The design corpus, kept consistent by Schematic. See <ref target="docs/development#docs-workflow">the docs workflow</ref>. |
| `evidence/` | The acceptance deliverables, the blind-QA harness and its recorded results. |
| `reviews/` | Independent review reports for the code. |
| `scripts/gates/` | The oracles the repository's gates run. |

## Building {#building}

Requirements:

- A Rust toolchain at or above the minimum supported version, 1.89. The floor
  comes from the dependency tree (`uuid` declares 1.89) and
  `cargo check --workspace --locked` passes on a 1.89.0 toolchain. Current
  stable is what everything else is developed on.
- `protoc` on `PATH`, or its path in the `PROTOC` environment variable.
  `janus-core`'s `build.rs` compiles `proto/janus.proto` with it
  (`brew install protobuf`, `apt install protobuf-compiler`).
- A C compiler. `rusqlite` is built with its bundled SQLite.
- Node.js 20.19 or newer (Vite 8) and npm, only for the UI and the acceptance
  harness.

```sh
cargo build --workspace            # debug build
cargo build --release --workspace  # target/release/{janus-daemon,janus}

cd ui && npm ci && npm run build   # ui/dist, served by the UI listener
```

The daemon serves the UI from `ui.assets_dir`. When that is unset it looks next
to the executable (`ui`, then `../share/janus/ui`) and, in debug builds, at
`ui/dist` in the source tree; it does not embed the assets. See
<ref target="docs/configuration">daemon configuration</ref>.

To run a daemon locally, write a config file (every key is documented in
<ref target="docs/configuration">daemon configuration</ref>), validate it, then
start it:

```sh
target/release/janus-daemon check-config --config path/to/janus.toml
target/release/janus-daemon --config path/to/janus.toml
target/release/janus status
```

## Tests {#tests}

```sh
cargo test --workspace --all-features   # Rust: unit, integration, conformance
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt --all -- --check

cd ui
npm run typecheck && npm run lint && npm test -- --run && npm run build
```

`--all-features` matters: the `sqlite` and `testing` features of `janus-core`
gate the SQLite `UsageStore` and the conformance suite, and the daemon enables
them.

The gate oracles in `scripts/gates/` are plain Node scripts and can be run by
hand:

```sh
node scripts/gates/no-stubs.mjs crates ui/src       # no TODO, todo!(), allow(dead_code) and similar
node scripts/gates/dup-scan.mjs crates <N>          # no repeated run of N significant lines
node scripts/gates/ui-check.mjs                     # type-check, lint, tests, build
node scripts/gates/cargo-tests.mjs --min <N> -- --workspace --all-features
```

## The verification standard {#verification}

Passing tests is necessary and not sufficient. Janus sits on a security
boundary, and a unit test written alongside the code it tests shares the code's
assumptions. A change that alters behaviour is verified at the level a user sees
it:

- **Run the real thing.** Start a real `janus-daemon` against a mock upstream
  (or the real one, with a throwaway credential) and exercise the change through
  its real interface: the data plane with a real HTTP client, the admin API
  through the `janus` CLI, the UI in a browser. Record what you ran and what you
  saw in the pull request.
- **Test the refusal, not only the success.** For anything on the trust boundary
  (identity extraction, the admin listeners, the plugin channel, forwarding
  allowlists, log content), include the case that must fail: the forged token,
  the data-plane request to an admin path, the downgraded channel, the secret
  that must not appear in the log.
- **A feature is not done at "compiles".** Types and unit tests do not show that
  a flag is wired to anything, that a config key reaches the code that reads it,
  or that a UI control calls the endpoint it should.

### The blind-QA acceptance harness {#acceptance}

`evidence/DELIVERABLES.md` lists every user-observable behaviour as a numbered
deliverable (`D1`, `D2`, ...) with an acceptance line, written so a tester who
sees only that file can check it. The scripts in `evidence/acceptance/` do
exactly that, black-box, against the release binaries: they start real daemons
on ports reserved per group, drive them with real clients, and write a result
per deliverable to `evidence/results/<group>.json`.

```sh
# All groups, in parallel (builds target/release first), then merges results:
node evidence/acceptance/run.mjs      # prints ACCEPTANCE PASS only if every deliverable passes

# One group, against binaries you have already built:
cargo build --release --workspace
node evidence/acceptance/a.mjs        # group a: data plane and upstream configuration
```

The groups are `a`, `b`, `cd`, `ef`, `ghn`, `ijk` and `lm`, each
header-commented with the deliverables it covers. Some groups need a built
`ui/dist`, and `cd` needs `@grpc/grpc-js` and `@grpc/proto-loader` installed
under `/tmp/qa-cd` (its header says so). Group scripts use fixed local ports in
disjoint ranges, so do not run two copies of one group at once.

When you add a user-observable behaviour, add a deliverable with an acceptance
line to `evidence/DELIVERABLES.md` and cover it in the matching group script.
`node scripts/gates/evidence-coverage.mjs` checks that every deliverable has a
passing result with non-empty artifacts.

## Code standards {#code-standards}

- **Clippy is clean at `-D warnings`.** Fix the lint rather than silencing it.
- **No dead code and no suppressions.** Do not add `#[allow(dead_code)]` or
  `#[allow(unused...)]`. Unused code is deleted or wired in. The `no-stubs` gate
  rejects the markers.
- **No stubs.** No `todo!()`, `unimplemented!()`, `TODO`/`FIXME` comments, or
  placeholder behaviour. If a behaviour is not built, it is stated as not built
  in <ref target="docs/status">status</ref>, with its trigger, and the code does
  not pretend.
- **No duplicated blocks.** The `dup-scan` gate fails on a repeated run of
  significant lines. Extract the shared code.
- **Fail closed on the trust boundary.** An error in identity verification,
  plugin resolution, TLS setup or config validation refuses the request or the
  startup. It never falls back to something more permissive.
- **Unknown config is an error.** New config keys are validated, and a key that
  would have no effect where it is written is refused, as described in
  <ref target="docs/configuration">daemon configuration</ref>.
- **Secrets stay out of output.** Credential material, tokens and secrets are
  never written to logs, error text, the request log or CLI output. Prefer types
  whose `Debug` redacts.
- **Tests sit with the behaviour they check**, and the test for a bug fix fails
  without the fix.

## Docs workflow with Schematic {#docs-workflow}

`docs/` is a Schematic corpus: Markdown documents that link to each other with
`<ref target="docs/<name>#anchor">` elements, checked by the `sch` CLI against
`schematic.yaml`. Change docs through `sch`, never with plain `git` commands on
the docs.

```sh
sch branch my-change      # start work (a Schematic branch)
# edit docs/*.md
sch check                 # run the rules over main..HEAD
sch fix                   # run the fix phase (rules that rewrite)
sch commit -m "docs: ..." # fix, check, then commit
sch merge my-change       # check, merge, check again, run gates, land
```

Each document needs front matter with `title`, `type` (`design`, `decision`,
`overview`, `reference` or `glossary`) and `status` (`draft`, `review`,
`accepted` or `deprecated`). Write in the present tense, describing what the
system is and does. Do not narrate how it came to be. Link by `<ref>` rather
than by bare path, so `sch mv` and `sch refs` keep working.

`sch check` reports two kinds of finding:

- **Errors** (broken references, missing front matter, bad types) block a
  commit. Fix them.
- **Propagation warnings** (`propagation/may-need-update`) mean a document you
  did not edit links to one you did, and a judge model scored the link as likely
  to be affected by your change. It is a prompt to read the linked document and
  decide whether it still says what is true. Update it if it does not. If it
  does, the warning stands as a record that you looked. It is not proof of a
  defect, and silencing it by editing text for its own sake is the wrong
  response.

A change to behaviour updates the doc that describes it in the same change.
Moved behaviour that leaves a doc describing the old thing is a defect.
Decisions that settle a design question, and the options considered, are added
to <ref target="docs/decisions">the decision records</ref>.

## Commits and pull requests {#commits}

- Commit messages: a short imperative subject in the form `area: what changed`
  (for example `daemon: refuse admin bind without TLS`), and a body that says
  why when it is not obvious.
- One logical change per pull request. Keep drive-by reformatting out.
- The pull request description says what changed, how it was verified (the
  commands run and what was observed against a real daemon), which docs changed,
  and whether `proto/janus.proto` changed.
- Run the Rust and UI checks above, and `sch check` if you touched `docs/`,
  before opening the pull request.

## Changing the plugin contract {#proto-changes}

`proto/janus.proto` is a public contract: plugin authors in other languages
generate code from it. Treat a change to it as an API change.

1. Open an issue first (the feature-request form has a field for it) that states
   the need, why the existing `ResolveRoute` and `Notify` shapes cannot carry it
   (the generic `NotifyEvent` envelope exists so new event kinds do not need a
   proto change), and the compatibility impact.
2. Only add. New fields take new field numbers, new RPCs and messages are
   additive, and existing numbers, names and meanings do not change. A removal
   or a semantic change needs an explicit decision recorded in
   <ref target="docs/decisions">the decision records</ref> and a new contract
   version.
3. Update <ref target="docs/plugin-contract">the plugin contract</ref> in the
   same change, and the daemon's handling of old plugins that do not set the new
   field.
4. Mark the pull request as a proto change so it is reviewed as one. The
   workspace must still build (`janus-core`'s `build.rs` regenerates the Rust
   types), and the daemon's tests against both a linked and a gRPC test plugin
   must pass.

## Extension points {#extension-points}

Janus has real seams for the things people most often want to add. Each is a
trait in `janus-core`'s `contract` module.

- **An upstream.** Implement `UpstreamImpl`
  (`crates/janus-core/src/contract/upstream.rs`): how auth is applied, base URL
  and path handling, the forwarding header allowlist, credential refresh, and
  how usage is read from a response through `UsageObserver`.
  `crates/janus-core/src/proxy/anthropic.rs`, `openai.rs` and `openrouter.rs`
  are the three reference implementations, and `proxy/testkit.rs` has a probe
  upstream for tests. An upstream is selected by name in config through
  `UpstreamKind` in `contract/upstream.rs`, so a new built-in upstream also
  touches that enum, the config validation in
  `crates/janus-daemon/src/contract/config.rs`, and the wiring in
  `crates/janus-daemon/src/run.rs`. Say so in the issue before starting: it is a
  change to the supported upstream set. The model is described in
  <ref target="docs/credential-substitution#general-purpose">not specific to LLM
  providers</ref>.
- **An identity extractor.** Implement `IdentityExtractor`
  (`crates/janus-core/src/contract/identity.rs`). The shipped methods are in
  `crates/janus-core/src/identity/` (`hmac.rs`, `jwt.rs`, `opaque.rs`,
  `trusted_header.rs`), and `identity/testkit.rs` supports tests. A method that
  belongs in the stock daemon is registered in the `ExtractorFactory`
  (`identity/factory.rs`). A deployment-specific one is a custom extractor: a
  custom build passes it in `Embedded::extractors` and references it from config
  as `method = "custom:<name>"`. See
  <ref target="docs/caller-identity#custom">custom extractors</ref>.
- **A `UsageStore`.** Implement `UsageStore` (`contract/metering.rs`) and run
  the conformance suite from `janus-core`'s `testing` feature against it, as
  `MemoryUsageStore` (`metering/memory.rs`) and `SqliteUsageStore`
  (`metering/sqlite.rs`) do through the `usage_store_conformance_tests!` macro.
  A store that passes the suite satisfies the atomic, idempotent, ledgered
  contract that <ref target="docs/plugin-contract#usage-stores">the plugin
  contract</ref> specifies.
- **A policy plugin.** Implement `JanusPlugin` for a linked plugin, or serve
  `janus.proto` over gRPC for an out-of-process one. See
  <ref target="docs/plugin-contract">the plugin contract</ref>.

## Releases {#releases}

Releases are cut from `main` by a maintainer:

1. `CHANGELOG.md` has the changes moved from `Unreleased` into a dated section
   for the new version.
2. The workspace `version` in `Cargo.toml` is set to match.
3. The Rust and UI checks, and `node evidence/acceptance/run.mjs`, pass on the
   release commit.
4. The commit is tagged `vX.Y.Z` (tags matching `v*` are release tags) and
   pushed. Security fixes are released as a patch version with a GitHub Security
   Advisory.

The crates are not published to crates.io. All three set `publish = false`:
`janus-daemon` and `janus-cli` are applications whose UI assets are not packaged
inside the crate, and `janus-core`'s public API is not yet stable enough to
promise to plugin authors who would depend on it from crates.io. Publishing
`janus-core` is the likely first step once its API settles, and requires a
`version` on the path dependencies.
