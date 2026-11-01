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
  daemon log with the admin principal (`socket` for the unix socket, `token` for
  the TCP listener) and the resource touched.
- **Errors are JSON.**
  `{ "error": { "code": "<snake_case>", "message": "..." } }` with a
  conventional HTTP status. `code` is the stable contract; `message` is for
  humans.

## Endpoints {#endpoints}

### Status {#status-endpoint}

- `GET /v1/status` — daemon identity and health in one document: `version`,
  `build`, `started_at`, the bound listeners, each configured upstream with its
  identity binding method, the request log's size and retention, and a `plugin`
  object: `mode` (`none` | `linked` | `unix` | `tls` | `plaintext`), `endpoint`,
  `health` (`unknown` before any call has been attempted, then `ok` |
  `unreachable` | `timeout` | `tls_error` | `no_client_auth` |
  `rejected_client_cert` | `error` — the failure classes of
  <ref target="docs/plugin-contract#plugin-config">configuring the plugin</ref>
  without their `plugin_` prefix), `last_error`, `last_ok_at`, and when TLS is
  in use `client_cert_expires_at` and `server_cert_expires_at`. This is what
  `janus status` prints and what the UI overview renders.

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
- `PUT /v1/credentials/{ref}/material` — replace the material and clear an
  `invalid` status. The rotation path.
- `POST /v1/credentials/{ref}/disable`, `POST /v1/credentials/{ref}/enable` —
  take a credential out of, or back into, the default chain and plugin
  resolution. A plugin that names a disabled `ref` gets a failed request, not a
  silent fall-through — the same posture as step 4 of
  <ref target="docs/architecture#request-lifecycle">the request lifecycle</ref>.
- `POST /v1/credentials/{ref}/refresh` — force a refresh of an expiring
  credential now rather than on next use.
- `DELETE /v1/credentials/{ref}` — remove the slot.

### Identities {#identities-endpoint}

The admin half of the `opaque` verification method — see
<ref target="docs/caller-identity#registry">the opaque-identity registry</ref>
for what an entry means and how lookup works. Entries are addressed by a
server-assigned `id`, never by the token.

- `POST /v1/identities` — register. Body: `identity` (the `caller_identity`
  string policy will see), optional `claims` (string map, becomes
  `identity_claims`), optional `label`, optional `expires_in` (duration) or
  `expires_at`. Response: the entry's metadata plus `token`, returned **once**
  and never retrievable again. The caller of this endpoint is normally the
  deployment's provisioner, which hands the token to the sandbox it is creating.
- `GET /v1/identities[?identity=&include_revoked=&include_expired=]` — list.
  Several entries may share one `identity` (a rotation in progress).
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
  page's `next`). `limit` defaults to 100 and is capped at 1000.
- `GET /v1/requests/{request_id}` — one record.
- `GET /v1/requests/stream?identity=&upstream=&outcome=` — a live tail as
  `text/event-stream`: one `request` event per completed record matching the
  filter, plus a periodic `ping`. SSE rather than a WebSocket because `curl`,
  the CLI and a browser `fetch` all consume it with nothing extra. The stream
  takes the admin token in the `Authorization` header like every other endpoint,
  never in the query string, so the UI reads it with `fetch` and a streaming
  body rather than with `EventSource`, which cannot set headers.

### Plugin {#plugin-endpoint}

- `GET /v1/plugin` — the `plugin` object from `/v1/status`, alone.
- `POST /v1/plugin/check` — actively verify the channel: perform a full
  handshake with the configured material, then — on the TLS transport — two
  handshakes the plugin must *reject*, one with a throwaway self-signed client
  certificate and one with none. Returns each result; on the socket transport,
  the path-permission walk and a connection attempt. This is how an operator
  confirms the plugin is actually requiring daemon authentication, which the
  daemon can otherwise only observe passively — see
  <ref target="docs/plugin-contract#channel-security">securing the
  channel</ref>.

## The request log {#request-log}

One record per request that reached the data plane, including the ones rejected
at identity extraction. The record is what makes a call attributable to the
identity that made it, and it is metadata only — Janus does not capture request
or response bodies. Fields:

- `request_id` — Janus-assigned; the same value a plugin sees in `ResolveRoute`
  and `Notify`.
- `received_at`, `duration_ms`.
- `upstream`, `route` (the matched path pattern, if a route binding applied),
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
Retention is a single setting, `request_log.retain` (a duration, default
`"30d"`); a periodic job deletes older rows. The log cannot be disabled —
attribution is the point of the proxy — but it can be kept short.

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
does. For a TCP address the admin token comes from `--token-file <path>`,
`JANUS_ADMIN_TOKEN`, or — when the CLI runs on the daemon's host —
`<data_dir>/admin-token`; for the same process-list reason as credential
material, it is never taken as an argument.

Config validation is the daemon's job, not the CLI's, because only the daemon
links the extractors and upstream implementations that a config has to be
checked against: `janus-daemon check-config --config <path>` runs the full
startup validation described in
<ref target="docs/caller-identity#binding">binding an extractor to a request
type</ref> and exits without binding a listener.

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

Authentication: the UI listener requires the admin token on every API call,
loopback included. A browser is a hostile environment — any page a user visits
can issue requests to a loopback port, and through DNS rebinding can even do so
same-origin — so the only thing that makes such a request fail is a secret the
page does not have. The token travels in an `Authorization` header, which also
means a cross-origin request is preflighted, and the daemon answers no CORS
preflight. The UI prompts for the token once and keeps it in session storage;
the daemon logs the UI URL and the token file's path at startup. A non-loopback
UI bind carries the same TLS requirement as a non-loopback admin bind.

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
filesystem, as described in <ref target="docs/scope#extractable">what carries
over</ref>, so that credential material and operational records have different
permissions and different backup stories.
