## What and why

<!-- What changes, and why. Link the issue it closes or follows from. -->

## How it was verified

<!--
Tests are necessary and not sufficient. State what you ran against a real
janus-daemon (or the acceptance group) and what you observed, including the case
that must be refused. Paste commands and relevant output with all secrets
removed.
-->

## Checklist

- [ ] `cargo test --workspace --all-features` passes
- [ ] `cargo clippy --workspace --all-features --all-targets -- -D warnings` is clean, with no new `allow` suppressions, dead code, stubs or `TODO`s
- [ ] UI changes: `npm run typecheck`, `npm run lint`, `npm test -- --run` and `npm run build` pass in `ui/`
- [ ] Verified against a real running daemon (or the relevant `evidence/acceptance` group), not tests alone, and the evidence is above
- [ ] Failure and refusal paths are tested (bad identity, bad config, plugin down), and failures on the trust boundary fail closed
- [ ] No secret, token or credential can reach logs, errors, the request log or CLI output because of this change
- [ ] Docs updated through `sch` (`sch check` is clean), or no docs are affected
- [ ] User-visible behaviour has an entry under `Unreleased` in `CHANGELOG.md`
- [ ] New observable behaviour has a deliverable in `evidence/DELIVERABLES.md`, or none is added

## Contract change

- [ ] This PR changes `proto/janus.proto`. If checked: an issue approved the change, it only adds (no renumbering, removal or changed meaning), `docs/plugin-contract.md` is updated, and old plugins are still handled.

## Notes for the reviewer

<!-- Anything risky, anything you are unsure of, anything deliberately left out. -->
