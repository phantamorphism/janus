---
title: daemon
type: reference
status: draft
---

VERDICT: PASS

REVIEWER: independent-reviewer (claude-sonnet-5-5)

Scope:
`crates/janus-daemon/src/{dataplane,admin,run.rs,lib.rs,main.rs,contract/config.rs}`
and `tests/{dataplane,admin}*`. This is round 3, covering the round-2 fixes. I
also read the CLI status changes. janus-core `relay.rs` was read only for the
stale-connection retry.

Verification, run in a scratch copy under /tmp with its own `CARGO_TARGET_DIR`.
The repo's `target/` was not used, and the scratch files are removed:
- Unmutated `cargo test -p janus-daemon -p janus-cli` is fully green (daemon
  admin 38, dataplane 41, lib 115, cli 12 and 17).
- `cargo clippy -p janus-daemon -p janus-cli --all-targets --all-features -- -D warnings`
  is clean.
- I did not re-run the workspace-wide test and clippy commands this round. The
  earlier round ran them clean, and janus-core/plugin/store were reviewed
  elsewhere.

## Round-2 items, verified

| Item | Verdict |
|---|---|
| `data_plane.max_connections` | Done. It is parsed with default 1024 and `0` is refused at validation, with a config test. The admin cap stays at 256. Both are documented in `docs/configuration.md` and `docs/admin-surface.md`. |
| Refusal counting and visibility | Done. `dp.refused` is an atomic shared into `/v1/status` (`data_plane.refused_connections`) and `Daemon::refused_connections()`. The warning fires only at power-of-two totals, so there is no log flood. |
| CLI | Done. `janus status` prints `refused conns` and alerts when `dropped_records` is non-zero. |
| Cap test | Real. `data_plane_refuses_connections_beyond_its_cap_and_recovers` fills the slots with idle connections, asserts refusal and the counter, then frees the slots and asserts recovery. |
| Shutdown-bound test | Real. `shutdown_returns_within_its_bound_when_the_log_writer_is_stalled` stalls the writer with a DB write lock, queues 3000 requests, and asserts shutdown returns within 45 s. |

## Mutation results

| Mutant | Result |
|---|---|
| Remove the data-plane connection cap (`if false`) | killed by the cap test |
| `FLUSH_TIMEOUT` 10 s to 3600 s | killed by the shutdown-bound test (45 s timeout hit) |
| `FLUSH_TIMEOUT` and `IDLE_TIMEOUT` both 3600 s | killed by the shutdown-bound test |

Earlier-round mutants stay killed: spawn-instead-of-`try_submit`, dropped
counter not incrementing, no header-read timeout, and the admin token check
always true.

## Remaining findings (all low, none blocking)

1. **[low] Cap semantics.** 1024 is a sound default for streaming agent load.
   HTTP/1.1 streams hold a connection each, HTTP/2 multiplexes, and idle
   HTTP/1.1 keep-alives are reaped by the 10 s header-read timeout. A refused
   connection still gets no HTTP response (a reset, not `503 Retry-After`). That
   is acceptable at this limit and is observable through the counter and the
   CLI.
2. **[low] Dropped-record logging.** `try_submit` logs an error only on the
   first drop of the process lifetime. The counter, `/v1/status` and the CLI
   alert make later drops visible, but a second stall after recovery produces no
   new log line. Logging at power-of-two totals, as done for refused
   connections, would be consistent.
3. **[low] Stale-connection retry (core `relay.rs`, informational).** It can
   resend a POST the upstream already read. It is conservative (reused
   connection only, no response byte, body at most 1 MiB, one retry) and
   documented as the browser and Go `net/http` trade-off. Duplicate LLM calls
   are possible and rare. The core review covers it in depth.
4. **[low] Unauthenticated 404 probes** still produce request-log rows. They are
   bounded by `try_submit` and retention.

## Open summary
- High: none.
- Medium: none.
- Low: 1 to 4, none blocking.

PASS: no open high or medium findings.
