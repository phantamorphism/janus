---
title: Admin surface, CLI and UI
type: design
status: draft
tags: [janus, admin, cli, ui, registry, logging]
---

# Admin surface, CLI and UI

The admin surface is the control channel of `janus-daemon`: what an operator
uses to manage the credential pool, the opaque-identity registry, and to read
the request log. Where it listens and how it is kept away from callers is
decided in <ref target="docs/architecture#admin-surface">the admin surface is a
separate listener</ref>. This document is the surface itself — the endpoints,
the `janus` command set built on them, what the UI shows, and the storage behind
them. The reasoning behind the scope is recorded in
<ref target="docs/decisions#admin-surface">decision records</ref>.

## Principles {#principles}

- **One API, three clients.** `janus`, the UI, and any host application that
  embeds Janus all speak the same HTTP+JSON API, versioned under `/v1`. Anything
  one of them can do, the others can do. There is no privileged in-process path.
- **Credential material never leaves the daemon.** No endpoint returns a secret,
  a token, or a refresh token. Endpoints that *accept* material
  (`POST /v1/credentials`, `PUT /v1/credentials/{ref}/material`) return only
  metadata. The one value the surface hands out exactly once is an
  opaque-identity token at creation, which is a caller's lookup key and not an
  upstream credential.
- **Mutations are explicit and logged.** Every mutating call is recorded in the
  daemon log with the admin principal (`socket` for the unix socket, `admin_tcp`
  for the TCP admin listener, `ui` for the UI listener) and the resource
  touched.
- **Errors are JSON.**
  `{ "error": { "code": "<snake_case>", "message": "..." } }` with a
  conventional HTTP status. `code` is the stable contract; `message` is for
  humans. A malformed body, query or path parameter gets the same envelope
  (`invalid_request`, 400), and so do unknown endpoints (`not_found`, 404) and
  methods (`method_not_allowed`, 405). See
  <ref target="docs/admin-surface#errors">error codes</ref>.
- **Responses are objects.** A list is wrapped in an envelope named for its
  resource (`{ "credentials": [...] }`, `{ "identities": [...] }`,
  `{ "requests": [...], "next": ..., "disabled": ... }`), so a list can gain
  fields without a breaking change. Creation answers `201` with the created
  resource, other mutations `200` with the resource's new state, and deletion
  `204` with no body. Unknown body and query fields are refused.
- **Authentication is per listener.** On the unix socket, filesystem permissions
  authenticate the caller and no token is needed. On the TCP admin listener and
  on the UI listener, every API request carries
  `Authorization: Bearer <admin token>`, loopback included. The token is taken
  from that header and from nowhere else — never the query string, never a
  cookie. A missing or wrong token is `401 unauthorized`, with
  `WWW-Authenticate: Bearer`. No CORS preflight is answered: an `OPTIONS`
  request carries no token, so it is refused like any other request.

## Endpoints {#endpoints}

### Status {#status-endpoint}

`GET /v1/status` returns daemon identity and health in one document. This is
what `janus status` prints and what the UI overview renders. Its fields:

- `version`, `build`, `started_at`.
- `listeners` — `admin_socket` (its path); `admin_tcp` and `ui`, each `null`
  when not configured, otherwise `{ address, tls, url }`; and `data_plane` (its
  address).
- `upstreams` — one entry per `[[upstream]]`: `name`, `path_prefix`, `base_url`,
  `binding` (the upstream-wide binding's method, or `null` when none resolves),
  and `routes`, a list of `{ path, binding }`.
- `data_plane` — `refused_connections`: the connections the data plane closed
  unanswered since start for being over `data_plane.max_connections`; `0` in
  normal operation. `janus status` prints it, flagged when non-zero.
- `request_log` — `enabled`, `retain` (human-readable), `retain_seconds`, and
  `rows` (the rows currently stored), and `dropped_records` (the records dropped
  since start because the writer's queue was full; `0` in normal operation;
  `janus status` prints it, flagged when non-zero).
- `credentials` — `total`, and `by_status`, which counts every status (`active`,
  `invalid`, `disabled`, `rate_limited`), zeros included.
- `plugin`:
  - `mode` — `none`, `linked`, `unix`, `tls` or `plaintext`.
  - `endpoint` — `null` for `none` and `linked`.
  - `health` — `unknown` before any call has been attempted, then `ok`,
    `unreachable`, `timeout`, `tls_error`, `no_client_auth`,
    `rejected_client_cert` or `error`. These are the failure classes of
    <ref target="docs/plugin-contract#plugin-config">configuring the
    plugin</ref> without their `plugin_` prefix.
  - `last_error`, `last_ok_at`.
  - `dropped_events` — the `Notify` events dropped since start because the
    bounded delivery queue was full. `Notify` never blocks the request path, so
    a non-zero count is the operator's only sign that settlement events a plugin
    relies on went missing.
  - `client_cert_expires_at` and `server_cert_expires_at` — present on the TLS
    transport only. The server certificate's expiry appears once a handshake has
    seen it.

  With no `[plugin]` block, `mode` is `none`, `endpoint` is `null` and `health`
  stays `unknown`.

### Credentials {#credentials-endpoint}

A credential is addressed by its `ref` — the same opaque reference a plugin
returns in `credential_ref`, and the pool slot's name. Every list and show
response carries metadata only: `ref`, `upstream`, `kind` (how auth is carried
for that upstream), `label`, `source` (`pool` | `env` | `file`), `status`
(`active` | `invalid` | `disabled` | `rate_limited`), `rate_limited_until`,
`expires_at`, `last_used_at`, `request_count`, `error_count`, `last_error`.

- `GET /v1/credentials[?upstream=]` — list.
- `GET /v1/credentials/{ref}` — one credential.
- `GET /v1/credentials/default?upstream=` — the credential the default chain
  would pick for that upstream *right now*, as metadata. This is the "peek" a
  host needs to show which account its traffic is on; it never touches the
  material.
- `POST /v1/credentials` — add a credential to the pool. Body: `upstream`,
  optional `ref` (generated if absent), optional `label`, and `material` — a
  kind-specific object the upstream implementation knows how to store (an API
  key; the contents of a vendor CLI's credential file). The daemon writes the
  pool slot and the credential is live without a restart.
- `PUT /v1/credentials/{ref}/material` — body `{ "material": { ... } }`; replace
  the material and clear an `invalid` status. The rotation path.
- `POST /v1/credentials/{ref}/disable`, `POST /v1/credentials/{ref}/enable` —
  take a credential out of, or back into, the default chain and plugin
  resolution. A plugin that names a disabled `ref` gets a failed request, not a
  silent fall-through — the same posture as step 4 of
  <ref target="docs/architecture#request-lifecycle">the request lifecycle</ref>.
- `POST /v1/credentials/{ref}/refresh` — force a refresh of an expiring
  credential now rather than on next use. A credential with nothing to refresh
  (an API key) is `409 not_refreshable`; a refresh the vendor refuses is
  `502 refresh_failed`. A forced refresh is never held back by the back-off that
  follows a failed refresh on use. Janus sends a refresh grant once more when it
  fails with a transport error other than a timeout or a refused connection (a
  keep-alive connection the token endpoint had closed); the `refresh_failed`
  reason names the transport cause.
- `DELETE /v1/credentials/{ref}` — remove the slot.

A credential declared in config (`source` `env` or `file`, see
<ref target="docs/configuration#credentials">config credentials</ref>) belongs
to the config: it can be enabled, disabled and refreshed here, but replacing its
material or deleting it is `409 not_pool_managed`.

### Identities {#identities-endpoint}

The admin half of the `opaque` verification method — see
<ref target="docs/caller-identity#registry">the opaque-identity registry</ref>
for what an entry means and how lookup works. Entries are addressed by a
server-assigned `id`, never by the token.

- `POST /v1/identities` — register. Body: `identity` (the `caller_identity`
  string policy will see), optional `claims` (string map, becomes
  `identity_claims`), optional `label`, optional `expires_in` (duration) or
  `expires_at` (not both). The token is always generated by the daemon; a body
  that tries to supply one is refused. Response: `201` with the entry's metadata
  plus `token`, returned **once** and never retrievable again. The caller of
  this endpoint is normally the deployment's provisioner, which hands the token
  to the sandbox it is creating.
- `GET /v1/identities[?identity=&include_revoked=&include_expired=]` — list.
  Several entries may share one `identity` (a rotation in progress). Revoked and
  expired entries are left out unless asked for.
- `GET /v1/identities/{id}` — one entry: `id`, `identity`, `claims`, `label`,
  `created_at`, `expires_at`, `revoked_at`, `last_seen_at`.
- `POST /v1/identities/{id}/revoke` — immediate: the next request presenting
  that token is rejected. Idempotent.
- `DELETE /v1/identities/{id}` — remove the entry outright (housekeeping of
  long-revoked or expired rows).

### Requests {#requests-endpoint}

The request log, queryable and tailable — see
<ref target="docs/admin-surface#request-log">the request log</ref> for the
record shape.

- `GET /v1/requests?identity=&upstream=&outcome=&credential=&since=&until=&limit=&before=`
  — newest first, cursor-paginated (`before` is the cursor from the previous
  page's `next`). `limit` defaults to 100 and is capped at 1000; `limit=0` and
  an unknown `outcome` are refused.
- `GET /v1/requests/{request_id}` — one record.
- `GET /v1/requests/stream?identity=&upstream=&outcome=` — a live tail as
  `text/event-stream`. SSE rather than a WebSocket because `curl`, the CLI and a
  browser `fetch` all consume it with nothing extra. Events:
  - `ping` — first on connect, then every 10 seconds; data `{ "at": ... }`.
  - `request` — one per completed record matching the filter; data is the
    record.
  - `lagged` — the subscriber fell behind and records were skipped; data
    `{ "missed": n }`.
  - `disabled` — sent once, alone, when the request log is disabled; data
    `{ "disabled": true }`.

  The stream is live only: events carry no `id` and there is no replay, so a
  reconnecting client — or one that saw `lagged` — fills the gap with
  `GET /v1/requests?since=`. The stream ends when the daemon shuts down. It
  takes the admin token in the `Authorization` header like every other
  endpoint, so the UI reads it with `fetch` and a streaming body rather than
  with `EventSource`, which cannot set headers.

With the request log disabled, the query endpoints return an empty list with
`"disabled": true`, and `GET /v1/requests/{request_id}` for a record that is not
stored is `404 request_log_disabled` rather than `request_not_found`.

### Plugin {#plugin-endpoint}

- `GET /v1/plugin` — the `plugin` object from `/v1/status`, alone.
- `POST /v1/plugin/check` — actively verify the channel: perform a full
  handshake with the configured material, then — on the TLS transport — two
  handshakes the plugin must *reject*, one with a throwaway self-signed client
  certificate and one with none. Returns
  `{ mode, endpoint, passed, steps: [...] }`, where each step names what ran
  (`path_permissions`, `connect`, `handshake`, `throwaway_cert_rejected`,
  `no_cert_rejected`) with `passed` and a `detail`, plus the failure `class`
  when it failed. On the socket transport the steps are the path-permission walk
  and a connection attempt; on the plaintext transport, a connection attempt; a
  linked plugin has no channel and returns no steps. With no `[plugin]` block
  the call is `409 no_plugin`. This is how an operator confirms the plugin is
  actually requiring daemon authentication, which the daemon can otherwise only
  observe passively — see
  <ref target="docs/plugin-contract#channel-security">securing the
  channel</ref>.

### Error codes {#errors}

| Code | Status | When |
|---|---|---|
| `invalid_request` | 400 | malformed body, query or path parameter; unknown field |
| `invalid_ref` | 400 | a `ref` that breaks the naming rule |
| `invalid_material` | 400 | material the upstream implementation cannot store |
| `unauthorized` | 401 | missing or wrong admin token |
| `not_found` | 404 | unknown endpoint |
| `credential_not_found`, `identity_not_found`, `request_not_found` | 404 | no such resource |
| `no_default_credential` | 404 | the default chain has no `active` credential |
| `request_log_disabled` | 404 | a record lookup while the log is disabled |
| `method_not_allowed` | 405 | known endpoint, wrong method |
| `credential_exists` | 409 | `POST /v1/credentials` with a `ref` already in use |
| `wrong_upstream` | 409 | the `ref` belongs to a different upstream |
| `credential_unavailable` | 409 | the credential cannot be used in its current state |
| `not_pool_managed` | 409 | replacing or deleting a config credential |
| `not_refreshable` | 409 | refreshing a credential that has nothing to refresh |
| `no_plugin` | 409 | `POST /v1/plugin/check` with no `[plugin]` block |
| `payload_too_large` | 413 | request body over the limit |
| `unsupported_media_type` | 415 | a body that is not JSON |
| `internal` | 500 | anything else |
| `refresh_failed` | 502 | the vendor refused a refresh |

## The request log {#request-log}

One record per request that reached the data plane, including the ones rejected
at identity extraction. Records are written through a bounded queue; if the
writer falls behind the queue, further records are dropped rather than held in
memory, and `request_log.dropped_records` in the status endpoint counts them.
The record is what makes a call attributable to the identity that made it, and
it is metadata only — Janus does not capture request or response bodies. Fields:

- `request_id` — Janus-assigned; the same value a plugin sees in `ResolveRoute`
  and `Notify`.
- `received_at`, `duration_ms`.
- `upstream`, `route` (the matched route path, if a route binding applied),
  `method`, `path`.
- `binding` — the identity method that ran (`hmac`, `jwt`, `opaque`,
  `trusted_header`, `custom:<name>`, or `none`).
- `caller_identity` — the verified identity; empty on a `none` route and on a
  rejected request.
- `claimed_identity` — on a rejected request only, the identity the material
  *claimed* to carry, decoded without verification. Log-only, per
  <ref target="docs/caller-identity#failure">failure posture</ref>.
- `correlation_id`.
- `outcome` — one of `forwarded`, `rejected_identity` (with `kind`:
  `no_material` | `invalid` | `wrong_namespace` | `expired` | `revoked`),
  `denied` (with the plugin's `reason`), `plugin_failed` (with the failure
  class), `credential_unavailable`, `upstream_error`, `abandoned` (with
  `stage`).
- `credential_ref` — what was actually used, when one was.
- `status` — the upstream's HTTP status, when the call was forwarded.
- `usage` — the upstream-specific observation object, identical to what
  `call_completed` carries.
- `error` — a short diagnostic, when there is one.

Storage is a `requests` table in the daemon's database (see
<ref target="docs/admin-surface#storage">storage</ref>), written after the
response has been relayed so the write is never on the caller's critical path.
Two settings control it: `request_log.enabled` (default `true`) and
`request_log.retain` (a duration, default `"30d"`); a periodic job deletes older
rows. With `enabled = false` the daemon writes no records, and the
`/v1/requests` endpoints return an empty result with a `disabled` marker. The
default is on because attribution of a call to the identity that made it is the
point of the proxy; an operator who does not want a record can turn it off.

## The janus CLI {#cli}

`janus` is a thin client of the endpoints above; each subcommand maps to one or
two calls, and every command takes `--json` for machine-readable output.

```
janus status

janus credential list [--upstream <name>]
janus credential show <ref>
janus credential default --upstream <name>
janus credential add --upstream <name> [--ref <ref>] [--label <text>]
                     (--api-key-stdin | --from-file <path>)
janus credential rotate <ref> (--api-key-stdin | --from-file <path>)
janus credential enable|disable|refresh|rm <ref>

janus identity create --identity <string> [--claim k=v]... [--label <text>]
                      [--expires-in <duration>]        # prints the token once
janus identity list [--identity <string>] [--include-revoked]
                    [--include-expired]
janus identity show|revoke|rm <id>

janus log query [--identity ...] [--upstream ...] [--outcome ...]
                [--credential ...] [--since ...] [--until ...] [--limit N]
janus log tail  [--identity ...] [--upstream ...] [--outcome ...]
janus log show <request_id>

janus plugin status
janus plugin check
janus plugin tls init --plugin-host <name> [--out <dir>] [--days N]
```

`--from-file` reads credential material from a file on the operator's host — the
vendor CLI's own credential file, typically — and sends its contents in
`material`. `--api-key-stdin` reads a bare API key from standard input. There is
deliberately no flag that takes a key as an argument, because an argument lands
in shell history and in every other local user's view of the process list.
`janus plugin tls init` is the bootstrap for the TLS plugin transport and is
described with it in
<ref target="docs/plugin-contract#channel-security">securing the channel</ref>.

Connection: `--admin <address>` or `JANUS_ADMIN`, where the address is
`unix://<path>` or `http(s)://host:port`; the default is the daemon's admin
socket, `<data_dir>/admin.sock`, resolving `data_dir` the same way the daemon
does. For a TCP address the admin token comes from `--token-file <path>` (or
`JANUS_TOKEN_FILE`), `JANUS_ADMIN_TOKEN`, or — only for a loopback address
(`localhost`, `127.0.0.0/8`, `::1`) — `<data_dir>/admin-token`; a remote address
always needs an explicit token, and the CLI warns when it would send one over
plain `http` to a remote host. For the same process-list reason as credential
material, it is never taken as an argument. For an `https` address
`--ca-file <path>` names the only certificate(s) to trust — a private CA or the
listener's own self-signed certificate — and system roots apply otherwise.

### Exit codes {#cli-exit-codes}

`0` success; `1` the operation failed (the daemon answered with an error); `2`
usage error; `3` the daemon could not be reached.

Config validation is the daemon's job, not the CLI's, because only the daemon
links the extractors and upstream implementations that a config has to be
checked against: `janus-daemon check-config --config <path>` runs the full
startup validation described in
<ref target="docs/caller-identity#binding">binding an extractor to a request
type</ref> and exits without binding a listener. It has no side effects: the
database and pool it builds live in a scratch directory that is removed
afterwards, so the configured data directory is neither created nor touched, and
its credential count covers the `[[credential]]` entries the config declares,
not pool slots already on disk.

## The UI {#ui}

The UI makes the daemon's state legible; it is the admin surface rendered, and
it is deliberately a subset of it. Five views:

- **Overview** — `/v1/status`: listeners, upstreams and their bindings, plugin
  mode and health with certificate expiry, credential health at a glance,
  request and denial rates from the log.
- **Credentials** — the list with status and counters; actions: enable, disable,
  refresh, remove.
- **Identities** — the registry with filters; action: revoke.
- **Requests** — a filterable table over `/v1/requests` with a live-tail toggle
  on `/v1/requests/stream`, and a detail drawer for one record.
- **Plugin** — the channel's health, and a button that runs `/v1/plugin/check`.

Two operations present on the API are not in the UI: adding a credential and
rotating its material. Material enters Janus from a file on the operator's host
or from a provisioner, and a browser form would be a second code path for the
most sensitive input the surface accepts, with nothing yet asking for it. The
API supports both, so adding them later is a UI-only change. Identity *creation*
is likewise absent: it is the provisioner's call, made by a program at sandbox
creation, not an operator's.

Authentication: the UI listener serves its static assets without a token and
requires the admin token on every `/v1` call, loopback included. A browser is a
hostile environment — any page a user visits can issue requests to a loopback
port, and through DNS rebinding can even do so same-origin — so the only thing
that makes such a request fail is a secret the page does not have. The token
travels in an `Authorization` header, which also means a cross-origin request is
preflighted, and the daemon answers no CORS preflight. The UI prompts for the
token once and keeps it in session storage; the daemon logs the UI URL and the
token file's path at startup. A non-loopback UI bind carries the same TLS
requirement as a non-loopback admin bind.

Every response from the UI listener — static assets, the single-page fallback,
and the `/v1` API with its errors — carries `X-Content-Type-Options: nosniff`,
`Content-Security-Policy: default-src 'self'; frame-ancestors 'none'` and
`Referrer-Policy: no-referrer`. The built UI is one same-origin script, one
stylesheet and same-origin `fetch` calls, with no inline script or style, so
`default-src 'self'` needs no `unsafe-inline` allowance;
`frame-ancestors 'none'` is set explicitly because `default-src` does not cover
it, and keeps the token-bearing page out of any frame.

## Storage {#storage}

The daemon owns one SQLite database, `<data_dir>/janus.db`, with two tables:

- `identities` — the opaque-identity registry: `id`, `token_hash` (SHA-256 of
  the token; the token itself is never stored), `identity`, `claims` (JSON),
  `label`, `created_at`, `expires_at`, `revoked_at`, `last_seen_at`. Loaded into
  an in-memory index at startup and written through on every mutation, so an
  `opaque` lookup on the hot path is a hash-map probe and never a disk read;
  `last_seen_at` is flushed periodically rather than written per lookup.
- `requests` — the request log, as described above, indexed on `(received_at)`,
  `(caller_identity, received_at)` and `(request_id)`.

SQLite because the daemon is standalone by default and must carry no external
service; because both tables want indexed queries and the request log wants
retention by deletion, which a flat file does badly; and because the file is
inspectable with stock tooling if the admin surface is ever not enough. The
credential pool is *not* in this database — it stays directory-per-slot on the
filesystem, as described in <ref target="docs/scope#extractable">what janus-core
provides</ref>, so that credential material and operational records have
different permissions and different backup stories.
