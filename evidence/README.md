---
title: Evidence
type: reference
status: accepted
---

# Evidence

This directory is the acceptance evidence for Janus: a black-box record that the
built binaries do what the project says they do, kept in the repository so
anyone can inspect it and re-run it.

| Path | What it is |
| --- | --- |
| `DELIVERABLES.md` | The complete list of user-observable behaviours (`D1` to `D110`), each with an acceptance line. Written so a tester who sees only this file can check every item. |
| `acceptance/` | The acceptance scripts, one per group of deliverables (`a`, `b`, `cd`, `ef`, `ghn`, `ijk`, `lm`), plus their shared helpers and small scratch crates. They run the release binaries against mock upstreams and plugins; they read no source code. |
| `results/` | One `<group>.json` and `<group>.log` per group, written by the scripts. |
| `results.json` | All per-deliverable results merged: `PASS`, `FAIL` or `BLOCKED`, with a note and the artifacts that back it. |
| `artifacts/` | The raw observations behind each result: request and response transcripts, admin API output, daemon logs, screenshots. |

The acceptance scripts were written blind: the people (and agents) who wrote
them worked from `DELIVERABLES.md` alone, independently of the implementation,
so a result reflects behaviour a user can observe rather than what the code was
expected to do. Independent review reports on the work are in
<ref target="reviews/README">reviews</ref>.

## Re-running

Everything runs from the repository root. You need Node 20 or later, a Rust
toolchain, and a built UI (`cd ui && npm install && npm run build`) for the
groups that exercise it. The `cd` and `ghn` groups install `@grpc/grpc-js` and
`@grpc/proto-loader` into a scratch directory under `/tmp` on first use
(`GRPC_NODE_MODULES` points `ghn` at an existing install instead).

```sh
# Every group, in parallel (builds target/release first), then merges results:
node evidence/acceptance/run.mjs      # prints ACCEPTANCE PASS only if all 110 pass

# One group, against binaries you have already built (optionally elsewhere):
cargo build --release --workspace
JANUS_BIN_DIR=$PWD/target/release node evidence/acceptance/a.mjs
```

Groups use fixed local ports in disjoint ranges and scratch directories under
`/tmp/qa-*`, so do not run two copies of one group at once. Re-running rewrites
`results/`, `results.json` and `artifacts/` with the new observations; the
artifacts contain only throwaway test credentials and `/tmp` paths.
