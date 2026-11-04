---
title: Contributing to Janus
type: reference
status: accepted
---

# Contributing to Janus

Thanks for helping. Janus is a credential-substituting proxy: it holds real
upstream credentials so the agents it serves never do. That makes correctness on
the trust boundary matter more than speed or feature count, and it shapes what
we ask of contributions.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Janus
is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), and
your contribution is offered under the same terms unless you state otherwise.

## Security issues

Do not file vulnerabilities as public issues. Follow [SECURITY.md](SECURITY.md).

## Before you start

- **Small fixes** (a bug with an obvious cause, a typo, a test gap) can go
  straight to a pull request.
- **Anything larger** (a new upstream, an identity method, a config key, an
  admin endpoint, a change to `proto/janus.proto`) starts as an issue, so the
  design is agreed before code is written. Janus deliberately resists adding
  surface area without a user who needs it; the reasoning for existing choices
  is in `docs/decisions.md`, and what is deliberately not built is in
  `docs/scope.md` and `docs/status.md`. Read those first, as they often answer
  "why doesn't it already do X".
- Look at `docs/architecture.md` and `docs/credential-substitution.md` for the
  model before touching the data plane, identity extraction or the admin
  surface.

## Setting up

You need a current Rust toolchain (the minimum supported version is 1.89),
`protoc`, and Node.js 20.19 or newer for the UI.

```sh
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings

cd ui && npm ci && npm run typecheck && npm run lint && npm test -- --run
```

The full guide, with the layout of the repository, the extension points and the
release process, is [docs/development.md](docs/development.md).

## What a good change looks like

- **It is verified for real.** Tests are necessary and not sufficient. Run a
  real `janus-daemon` and exercise the behaviour through its real interface,
  including the case that must be refused. The pull request says what you ran
  and what you saw. The blind-QA acceptance harness
  (`node evidence/acceptance/run.mjs`, or one group such as
  `node evidence/acceptance/a.mjs`) checks the user-observable behaviours listed
  in `evidence/DELIVERABLES.md`; behaviour you add gets a deliverable there.
- **It meets the code standards.** Clippy clean at `-D warnings`; no dead-code
  or lint suppressions; no stubs, `todo!()` or `TODO` comments; no duplicated
  blocks (the `dup-scan` gate); failures on the trust boundary fail closed;
  secrets never reach logs or error text.
- **The docs move with the code.** `docs/` is a Schematic corpus. Edit it with
  the `sch` CLI (`sch branch`, `sch check`, `sch fix`, `sch commit`,
  `sch merge`), never with plain `git` commands on the docs. A pull request that
  changes behaviour updates the document that describes it. See the docs
  workflow in [docs/development.md](docs/development.md#docs-workflow).
- **It is one logical change**, with a message of the form `area: what changed`
  and a description that says why.
- **A change to `proto/janus.proto` is flagged** as one and follows the contract
  change process in [docs/development.md](docs/development.md#proto-changes):
  open an issue first, add only, and update `docs/plugin-contract.md`.

## Pull requests

Fork, branch from `main`, and open a pull request using the template. Fill in
the checklist honestly: an unchecked box with an explanation is better than a
checked box that is not true. A maintainer reviews for correctness first, then
for fit with the design. Expect questions about failure paths and about what
happens when the plugin, the upstream or the config is wrong. Review comments
are about the change, not the person, and the Code of Conduct applies in both
directions.

## Reporting bugs and proposing features

Use the issue forms. A good bug report has the Janus version or commit, the
relevant config with secrets removed, the exact request or command, and what you
expected against what happened. For anything involving credentials, tokens or
the request log, check that your report contains no real secret.

## Releases

Maintainers cut releases by tagging `v*` on `main` after updating
[CHANGELOG.md](CHANGELOG.md). User-visible changes in your pull request should
add a line under `Unreleased` there.

## Governance

How decisions are made and who can merge is in [GOVERNANCE.md](GOVERNANCE.md).
