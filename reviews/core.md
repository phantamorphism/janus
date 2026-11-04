---
title: core
type: reference
status: draft
---

VERDICT: PASS REVIEWER: independent-reviewer (claude-sonnet-5-5)

# Review: janus-core and daemon plugin/store (round 3, after the round-2 fixes)

Scope is unchanged: `crates/janus-core/src/**`,
`crates/janus-daemon/src/plugin/**`, `crates/janus-daemon/src/store/**`. This
round re-verifies the fixes for the previous open medium and its lows.

## Commands run

All ran in a scratch copy under `/tmp` with `CARGO_TARGET_DIR=/tmp/rv2-target`.
The scratch copy and target have been removed. The repo tree and its `target/`
were not touched.

- `cargo test --workspace --all-features`: all pass (janus-core 254, daemon 115
  and 38, others green).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean.
- `node scripts/gates/dup-scan.mjs crates 10`: `DUP SCAN OK (100 files)`.

## Previous open findings

- **URL leak in `error_chain` (medium): fixed.** `relay.rs` now has one
  `transport_error` that calls `without_url()`. It is shared with `material.rs`,
  and the duplicate is gone. The forwarder and `refresh_grant` both use it.
  - Mutation: deleting the `without_url()` call fails two tests,
    `the_upstream_error_text_never_carries_the_request_query` (the output showed
    `?key=sk-secret-query` leaking) and
    `a_second_transport_failure_is_reported_with_its_cause`. The test can fail.
- **Retry classifier unpinned (low): fixed.** Deleting
  `&& is_dead_connection(&e)` from the forwarder's retry condition now fails
  `an_error_that_is_not_a_dead_connection_is_not_sent_again`.
- **`refresh_grant` retry too broad (low): code fixed.** It now retries only
  `is_dead_connection` errors (`material.rs:126`). The test side is open, see
  the first low finding below.
- **Replay capture pinning shared buffers (low): code fixed.** Capture uses
  `Bytes::copy_from_slice`. The test side is open, see the second low finding.

## Open findings

- [ ] [low] crates/janus-core/src/proxy/material.rs:126 The `refresh_grant`
  retry guard is unpinned. Widening `is_dead_connection(&e)` to
  `!e.is_timeout()` leaves all 62 `proxy::` tests green. Every test failure that
  reaches the guard is a dead connection, so no test shows a non-dead-connection
  error being refused a retry on the grant path.
- [ ] [low] crates/janus-core/src/proxy/relay.rs:142 Replacing
  `Bytes::copy_from_slice(chunk)` with `chunk.clone()` survives. This is hard to
  observe in a test and the memory effect is minor, since capture is still
  capped at 1 MiB of chunk lengths.

## Carried over and sound

All earlier fixes remain in place (socket symlink walk, pool key
canonicalization, 403 without a verdict, refresh failure backoff, truncated-JSON
refusal, gRPC keepalive, pinned-cert tests, redirect tests, https-only
`jwks_url` and `token_url` except loopback IP literals).

Forwarder retry guards `reused`, `replayable` and `REPLAY_LIMIT` were each
confirmed pinned by a failing test when removed (round 2). The retry fires only
on a reused connection, with no response byte, and a replayable body. The
residual duplicate-send risk is the documented HTTP/1.1 one.

No open high or medium findings.
