---
title: clients
type: reference
status: draft
---

VERDICT: PASS

REVIEWER: independent-reviewer (Sonnet 5.5, claude-sonnet-5-5)

Scope: `crates/janus-cli/**`, `ui/**`, re-reviewed against
`docs/admin-surface.md` and the daemon's `admin/*` (read-only). This is a second
pass after the fixes. No high or medium findings remain open.

## Verification run

- `cargo test -p janus-cli` passed (11 unit and 17 integration tests), and
  clippy was clean. Both used a scratch `CARGO_TARGET_DIR`, since removed.
- In `ui/`: `npm run build` (including `tsc -b`), `npm test` (56 tests) and
  `npm run lint` passed.
- **UI against a real daemon.** I ran a scratch `janus-daemon` with a UI
  listener and drove the UI with agent-browser. This was the main check, because
  the first review's high finding was a fixture-versus-daemon mismatch.
  - The token gate accepted the real admin token. `sessionStorage` held it, and
    `localStorage`, cookies and the URL were empty.
  - Overview rendered listeners and upstreams from the real `/v1/status`.
  - Requests rendered the real rejected-identity records. This was the
    previously broken `requests` key.
  - The live tail showed "Live: new requests appear as they complete". A request
    sent through the data plane appeared in the table immediately.
  - Credentials and Identities showed an `or-main` credential and an `sb1`
    identity created through the CLI, with Refresh, Remove and Revoke actions.
  - Plugin rendered with mode none.
- The CLI also ran against the same real daemon over the unix socket (credential
  add, identity create, log query).

## Mutation checks, in scratch copies under /tmp

The scratch copies and the scratch target dir have been removed, and the repo
tree was never edited. Each row is a change to a copy and the test result it
produced.

| Mutation | Result |
|---|---|
| UI reads `records` instead of `requests` | 15 tests fail |
| UI renames the `lagged` handling | 1 test fails |
| UI moves the token header to `X-Auth` | 5 tests fail |
| UI switches `sessionStorage` to `localStorage` | 4 tests fail |
| UI breaks the tail gap-fill `since` | 1 test fails |
| CLI auto-loads the local token file for a remote address | `the_local_token_file_is_not_auto_loaded_for_a_remote_address` fails |

## Resolved from the previous review

- The `records` versus `requests` key mismatch is fixed. The client now parses
  strictly, and I confirmed it against a real daemon.
- The strict status parser now reads `dropped_events`, routes and listener URLs.
  It is covered by `Overview.test.tsx`.
- The tail backfills after a reconnect or `lagged`, and it handles `disabled`.
- The unused dependencies (shadcn, cva, clsx, tailwind-merge) are removed from
  `package.json`.
- The CLI loads `<data_dir>/admin-token` only for a unix socket or loopback
  address. It warns on stderr before sending the token over plain http to a
  remote host.
- `log tail` has a 35 s idle timeout and drops and reconnects the stream when it
  is silent. A unit test covers the drop.

## Remaining findings

1. LOW - `ui/src/test/fixtures.ts` is still hand-written. The shapes now match
   the daemon (verified live), but nothing automatically ties them to the
   daemon's serde output, so drift is caught only by review or a browser run.
   Consider a captured-response contract test.
2. LOW - `crates/janus-cli/src/commands/log.rs` (`IDLE_TIMEOUT`): the
   idle-timeout test overrides the field, so the default value of 35 s itself is
   untested. Changing it to 3600 s passes the tests. This is cosmetic.
3. LOW - `crates/janus-cli/src/connect.rs` (`is_loopback`): `localhost` is
   trusted by name, as is conventional. An explicit `--token-file` or
   `JANUS_ADMIN_TOKEN` is still sent to any remote host, only with the
   plain-http warning. This is reasonable, since the operator chose it.
4. LOW - `ui/package.json` has no `typecheck` script. `build` runs `tsc -b`, so
   type errors still fail the build.
