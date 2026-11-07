---
title: Claude login keeper
type: reference
status: draft
tags: [janus, plugin, oauth, anthropic, example]
---

# Claude login keeper

`claude-login-keeper` (`examples/claude-login-keeper/`) is an example plugin
that keeps a Claude subscription credential usable. Such a credential is an
Anthropic OAuth credential (`kind` `oauth`, a Claude Code `.credentials.json`)
in the pool (<ref target="docs/credential-substitution">credential
substitution</ref>). When Anthropic revokes it, the daemon marks it `invalid`.
When its refresh token is refused, every refresh fails, and the access token
stops working when it expires. Neither recovers without a new login, and only a
person can authorize a login. The keeper notices the problem, runs the login,
shows the authorize URL to the operator, and after the operator authorizes, it
rotates the new material into the pool. The operator's click is the only manual
step.

How to build, configure and run it is in
<ref target="examples/claude-login-keeper/README">the keeper's README</ref>. A
recorded run against a real daemon is in
<ref target="examples/claude-login-keeper/VERIFICATION">the keeper's
verification</ref>.

## What it does {#overview}

The keeper is one process with three parts:

- **The plugin service** is on a unix socket, as in
  <ref target="docs/writing-a-plugin">writing a plugin</ref>.
  - `Notify` reads two event types for the credentials the keeper manages.
    `credential_invalid` marks the credential as needing a login. An
    `account_error` only asks for a check, because a login does not fix a
    billing or organization problem.
  - `call_completed` is ignored. A 2xx shows only that an access token served
    one request, and the request can be older than a `credential_invalid` event.
    Only a check through the admin API clears `needs_login`.
  - `ResolveRoute` is described in <ref target="#deny">deny or default</ref>.
- **A periodic check** runs at startup and then every `validate_interval`. It
  calls `GET /v1/credentials/{ref}` for each managed credential. If an OAuth
  credential expires within `refresh_ahead`, the check also calls
  `POST /v1/credentials/{ref}/refresh`. The result is one of these health
  values:

  | Health | Meaning | Flow | Deny |
  |---|---|---|---|
  | `healthy` | The credential works, or its refresh succeeded. | No | No |
  | `disabled` | An operator disabled the slot. | No | No |
  | `refresh_refused` | The token endpoint refused the grant (`502 refresh_failed` with `invalid_grant`, or a 400, 401 or 403 from the token endpoint). | Yes | Only after `expires_at` |
  | `needs_login` | The pool marks the credential `invalid`. | Yes | Yes |
  | `unknown` | No check has answered yet. | No | No |

  A refused refresh does not set `invalid` in the pool. The pool only records
  `last_error` and backs off, so no `credential_invalid` event comes, and the
  check is the only way the keeper finds it. While the `expires_at` of a
  `refresh_refused` credential is unchanged, later checks do not call the
  token endpoint again. A transport failure, a 5xx from the token endpoint, or
  an admin API that does not answer says nothing about the credential. Such a
  check changes no health value and does not set `checked_at`.

  A check records its answer only if nothing changed the credential's health
  while the check ran. Each `credential_invalid` event, rotation and recorded
  check advances a per-credential epoch. A check that started at an older
  epoch is dropped (`event` `check_superseded`). A slow check therefore cannot
  overwrite a newer event or a rotation.

  A healthy check also deletes a kept login of that credential (see
  <ref target="#isolation">isolation</ref>), because the pool's own material
  works.
- **Checks continue during a flow.** A check that finds an automatic flow's
  credential usable again ends that flow (`cancelled`, "the credential is
  usable again"), resets its backoff and lifts the deny. A flow started by the
  operator is not ended this way.
- **The loopback page** (`http_bind`, loopback only) shows the state and the
  authorize URL. It also takes the pasted code and starts or cancels a flow.
  Cancel (`POST /login/cancel`) and code (`POST /login/code`) name a
  `credential`. They reach only the attempt for that credential, and answer
  `409 no_flow` if another credential's attempt runs. With one configured
  credential, the field can be omitted. A client that sends
  `Accept: application/json` (the `login` subcommand) gets
  `{"ok": message, "flow_id"}`, or
  `{"error": {"code", "message", "credential_ref", "flow_id"}}` with the code
  `busy`, `unknown_credential`, `bad_request`, `no_flow` or `shutting_down`.
  `GET /status` lists the latest flows in `flows` (up to 8, each with its
  `id`), and the latest one in `flow`.

## Login flows {#flows}

A flow is one run of `claude auth login --claudeai`. This is the CLI's
non-interactive login subcommand, so it needs no terminal UI. The flow has these
states:

| State | Meaning |
|---|---|
| `starting` | The child process is being spawned. |
| `awaiting_url` | The child runs; it has printed no authorize URL yet. |
| `awaiting_browser` | The URL is known. The page, the log (`event` `login_url`, once per flow), the CLI and the deny reason show it. |
| `succeeded` | The login was rotated into the pool. |
| `failed` | The CLI failed, the flow timed out (`login_timeout`), or the rotation failed. |
| `cancelled` | The operator cancelled the flow, the credential became usable again, or the keeper shut down. The `reason` says which. |

The CLI prints two URLs, and both are surfaced:

- The **printed URL** redirects to
  `https://platform.claude.com/oauth/code/callback`. That page shows a code, and
  the operator pastes the code back (on the page, or on the CLI's stdin). This
  URL works from a browser on any machine.
- The **local URL** is the URL that the CLI asks the system to open. It
  redirects to `http://localhost:<port>/callback` on the keeper's host. It
  completes by itself, but only from a browser on that host.

A login succeeds only if the CLI exits 0 **and** it wrote a `.credentials.json`
that has `claudeAiOauth.accessToken` and `refreshToken`. The CLI also exits 0
when it logged in but could not save the credentials.

Scheduling rules:

- **One attempt at a time** for all credentials, because they share one config
  directory. An attempt holds the flow slot from its check to its rotation. The
  slot is freed when the attempt ends, also if the attempt panics; a flow left
  running by a panic ends as `failed`.
- **Debounce.** The first problem report sets the time a flow may start
  (`debounce` later). Later reports do not move it. A burst of events therefore
  starts one flow.
- **Check first.** Every automatic attempt asks the admin API before it does
  anything. A credential that is usable again starts no flow. If the admin API
  does not answer, the attempt counts as a failure and starts no flow. A
  `credential_invalid` that comes while the check runs stands over the check's
  answer, so the attempt goes on to a flow.
- **A kept login first.** If the credential has a kept login (see
  <ref target="#isolation">isolation</ref>), the attempt rotates it and starts
  no flow.
- **Backoff.** A failed or timed-out flow waits `backoff_min * 2^(n-1)`, capped
  at `backoff_max`, and the next attempt is scheduled for when the wait ends.
  While the credential stays unusable, the keeper tries again at a slowing rate
  and never in a storm.
- **A look after each attempt.** When an attempt ends and its credential still
  wants a login (`needs_login` or `refresh_refused`) with no attempt scheduled,
  for example after a report that came during the attempt, the next attempt is
  scheduled. The debounce, the backoff and a cancel's hold apply.
- **Operator cancel.** A cancel is not a failure: it does not increase the
  backoff. It holds automatic flows for that credential for `backoff_max`; the
  next automatic attempt waits for the hold to end. A cancel names its
  credential, so it never ends or holds another credential's flow. An operator
  start is not held.
- **Operator start** (`POST /login/start`, or the `login` subcommand) skips the
  debounce, the backoff and the health gate. The start takes the flow slot and
  publishes its flow, with a new `id`, before it answers. While any attempt
  runs, a start for any credential gets `409 busy`, with the running attempt's
  `credential_ref` and, once that attempt has a flow, its `flow_id`. The `login`
  subcommand follows only the flow whose `id` its start returned. On `busy` for
  its own credential it follows the running flow by its `flow_id`; while that
  attempt is still checking and has no flow, it asks again.

## Isolation from the operator's Claude Code {#isolation}

Two parties that use one refresh-token family (Janus, and the operator's own
Claude Code logged in with the same grant) invalidate each other's tokens. The
keeper therefore makes its own login, a separate OAuth grant. The keeper's grant
never touches the operator's `~/.claude`, and never touches the operator's
keychain item.

The child process gets an empty environment plus these values:

- `HOME=<state_dir>/home`.
- `CLAUDE_CONFIG_DIR=<state_dir>/claude`.
- `PATH`, starting with `<state_dir>/bin`, then the directory of `claude_bin`
  and the system directories.
- `BROWSER`, `TERM=dumb`, `NO_COLOR`, `DISABLE_AUTOUPDATER`.
- `USER`, `LOGNAME`, `LANG` and `TMPDIR`, from the keeper's own environment.

Variables such as `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` do not reach
the child. `<state_dir>/bin` holds three shims:

- `open` and `xdg-open` (also `$BROWSER`) record the URL in
  `<state_dir>/run/open.url` and open nothing.
- `security` exits 44 ("item not found") for every call. It appends the
  subcommand and the `-s` service name of each call (never a `-w` value) to
  `<state_dir>/run/security.log`.

These facts about where Claude Code 2.1.319 stores a login were verified on
macOS. The sources are the strings of the binary and the `security.log` of a run
with the real binary
(<ref target="examples/claude-login-keeper/VERIFICATION#real">the
verification</ref>):

- **Without `CLAUDE_CONFIG_DIR`**: the login keychain item
  `Claude Code-credentials` (account `$USER`), which is the operator's own item.
  If the keychain fails, the CLI falls back to `~/.claude/.credentials.json`.
- **With `CLAUDE_CONFIG_DIR=<dir>`**: the keychain service name gets a suffix,
  `Claude Code-credentials-<first 8 hex of sha256(<dir>)>`. The plaintext
  fallback is `<dir>/.credentials.json`. With the override set, the shim log
  shows queries only for suffixed names. The unsuffixed item is never named.
- The CLI runs `security` by its bare name, so the shim answers all of its
  keychain calls. A lookup gets "not found". In the binary's code, an add that
  fails with a non-transient error makes the CLI write the plaintext file, and
  the keeper reads that file. This write has not been observed with the real
  binary, because no real login was completed. If the CLI does not write the
  file, the flow fails with
  `claude reported success but ... has no claudeAiOauth` (or `cannot read`), and
  nothing is rotated. After runs with the real binary,
  `/usr/bin/security find-generic-password` for the keeper's suffixed item exits
  44: nothing was written to the login keychain. On Linux the CLI writes the
  plaintext file in any case.

When a flow succeeds, the keeper moves the CLI's file to
`<state_dir>/pending/<ref>.json` (the ref percent-encoded), so a kept login
belongs to exactly one credential. It then rotates that file and deletes it when
the rotation succeeds. The pool is then the only holder of the new refresh
token. The keeper never runs `claude auth logout`, because a logout could revoke
the grant that the pool now uses.

- If the rotation fails in any way other than `400 invalid_material` (a 401, a
  409, a 5xx, no answer), the file stays. The next automatic attempt for that
  credential, after the backoff, rotates it and starts no flow. This also
  recovers a login that finished just before the keeper stopped: the first check
  after the restart finds the credential still unusable and schedules an
  attempt, which rotates the file.
- If the daemon answers `400 invalid_material`, no retry can help: the file is
  deleted (`event` `pending_login_discarded`), and the next attempt runs a flow.
- If a check finds the credential healthy, the file is deleted (`event`
  `pending_login_discarded`). The pool's own material works, and it is not
  overwritten with an older login.
- An attempt for another credential never reads the file.

The keeper creates `<state_dir>` and its subdirectories with mode `0700`. It
sets that mode on a directory that already exists, and it refuses one that
another user owns.

A keeper killed during a flow leaves its child running: the real CLI does not
exit when its stdin closes. The keeper records the child's pid in
`<state_dir>/run/login.pid`, and the child leads its own process group. At
startup, the keeper reads and removes that file. It ends the process only if all
of these are true:

- The process leads its own process group (its pgid is its pid).
- Its command line contains `claude_bin` and `auth login --claudeai`.

It sends `SIGTERM` to the group, and `SIGKILL` if the group is still alive after
the grace period. A pid that names any other process is left alone (`event`
`orphan_pid_ignored`).

## Deny or default {#deny}

`ResolveRoute` answers `use_default` with one exception. If `deny_when_unusable`
is set (the default), and every managed credential that lists the request's
instance in `instances` is unusable, the keeper answers `deny_reason`. A
credential is unusable when its health is `needs_login`, or `refresh_refused`
and its `expires_at` has passed:

```
claude login needs re-authorization: open <printed URL> , sign in, and paste the code it shows at http://127.0.0.1:27711/ (credential claude-sub)
```

Before a flow is waiting, the reason names the page and the `login` subcommand
instead of the URL.

Why deny: `use_default` would send the request to a credential that is known to
be dead, and the caller would get the upstream's 401, or a refresh failure. The
deny returns a 403 with a reason that tells the caller what to do. The URL in
that reason is safe to show. It carries a PKCE challenge, not a verifier, and a
code from it is useless without the keeper's CLI process.

The deny only applies when the operator has listed the instance in `instances`.
That list says that the managed credentials are the instance's only usable
source. An instance served by other credentials too must not be listed.
Otherwise use `deny_when_unusable = false`. Before the first answered check the
health is `unknown`, which never denies. A failed check changes no health value:
a credential that a `credential_invalid` event marked `needs_login` keeps
denying while the admin surface is unreachable, and one that a check found
healthy keeps serving. The log records the answer once each time it changes
(`event` `route_answer_changed`).

## Admin client {#admin-client}

The keeper calls the admin API directly. It does not run the `janus` CLI, and it
does not depend on `janus-cli`. It uses HTTP/1.1 over the daemon's `admin.sock`,
which needs no token. An `http://` address with a bearer token from
`admin_token_file` also works. The keeper makes three calls:

- `GET /v1/credentials/{ref}`.
- `POST /v1/credentials/{ref}/refresh`.
- `PUT /v1/credentials/{ref}/material` with
  `{"material": {"claudeAiOauth": {...}}}`. This replaces the material and
  clears `invalid`. It works only on a pool slot: a config-sourced credential
  gets `409 not_pool_managed`, and the keeper logs a warning for it at the first
  check.

Only the `claudeAiOauth` object of the file is pushed. The keeper uses only
workspace dependencies and adds no new third-party crate. Pipes replace a
pseudo-terminal, because `claude auth login` works over plain pipes.

## Secrets {#secrets}

- Token values are never logged. The log records `expires_at`, refs and states.
- The material type's `Debug` output is redacted.
- A pasted code is passed to the child and is not logged.
- Errors about the credentials file never quote the file.
- The authorize URL is logged in full, because the operator needs it.

## Limits {#limits}

- The page has no authentication. It binds only to loopback. It refuses a
  request whose `Host` is not the bind address or `localhost:<port>`, so a
  DNS-rebound name gets a 403. It refuses a `POST` that has a foreign `Origin`.
  Every answer sets a `Content-Security-Policy` with `frame-ancestors 'none'`,
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`,
  `Cache-Control: no-store` and `Referrer-Policy: no-referrer`. A local user on
  a shared host can still start or cancel a flow, or paste a code from their own
  account, which would log the keeper in as that account.
- The CLI's output strings (the authorize path, the failure phrases) are matched
  as Claude Code 2.1.319 prints them. A later CLI that changes them makes flows
  fail or time out with a reason. It does not cause a wrong rotation.
- The keeper holds its state in memory. A restart forgets the backoff and the
  flows. At startup it ends an orphaned flow and checks every credential. A kept
  login is rotated by the attempt that this check schedules for a credential
  that is still unusable, and deleted if the check finds the credential healthy.
- A real login and the token exchange that follows it have not run in tests.
  <ref target="examples/claude-login-keeper/VERIFICATION#gaps">The
  verification</ref> lists what is proven and what still needs the operator's
  click.
