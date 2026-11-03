---
title: DELIVERABLES
type: reference
status: draft
---

# Janus — user-observable deliverables

## How to read this file

This file is the complete list of behaviours Janus must exhibit, extracted from
the project's design documents and its plugin contract (`proto/janus.proto`). It
is written for a QA tester and an evidence assessor who see **only this file**:
no source code, no design docs. Every deliverable is self-contained.

Glossary used throughout:

- **Janus** — a proxy that sits between a *caller* (typically an agent in a
  sandbox) and an authenticated *upstream* (Anthropic, OpenAI, OpenRouter). The
  caller presents its own *caller identity*; Janus verifies it, picks a real
  upstream *credential*, substitutes it into the outbound request, forwards, and
  relays the response. The caller never sees the real credential.
- **janus-daemon** — the long-running proxy process. It has three kinds of
  listener: the **data plane** (TCP, `data_plane.bind`, what callers hit), the
  **admin surface** (HTTP+JSON under `/v1`, on a unix socket by default and
  optionally TCP), and the optional **UI listener** (`ui.bind`). It is
  configured by a TOML config file whose path is given on the command line.
- **janus** — the admin CLI, a thin client of the admin surface.
- **The UI** — a standalone web app served by the daemon on the UI listener.
- **Plugin** — an optional policy service the daemon calls over the
  `JanusPlugin` gRPC contract (`ResolveRoute`, `Notify`), or a Rust
  implementation compiled ("linked") into a custom daemon build.
- **`<data_dir>`** — the daemon's data directory (holds `admin.sock`,
  `admin-token`, `janus.db`).
- **Default chain** — the daemon's own rule for choosing a credential from its
  pool when no plugin decides.

Each deliverable has this shape:

- a bold heading line `**Dn — Title.**` (n is a unique number);
- **What:** the feature, stated from the outside;
- **Exercise:** how an operator, caller, or plugin author triggers it (config
  TOML, data-plane HTTP requests, admin API calls, `janus` CLI commands, UI
  views, plugin gRPC behaviour);
- **Acceptance:** the concrete observable outcome that decides pass/fail.

Where the spec fixes a value (status code, file mode, field name, default) the
Acceptance line states it exactly. Where the spec does not fix a value (for
example the exact HTTP status of a policy denial), the Acceptance line says what
is fixed and does not invent the rest; an assessor should not fail a deliverable
for a choice the spec leaves open.

Reading of "standalone by default" assumed by these tests: the docs state both
that a standalone daemon needs no configuration beyond upstream credentials to
get traffic flowing, and that every request must resolve an identity binding (or
be rejected). The tests assume the minimal standalone configuration is: the
`[[upstream]]` blocks, their credentials, and one daemon-default `[identity]`
binding — with no plugin and no external service (see D27).

Shape rule enforced by the gate `scripts/gates/deliverables-shape.mjs`: each
deliverable starts on a new line with an optional `- `/`* ` bullet, optional
`**`, then `D` followed by its number; ids are unique; each deliverable's block
contains an `Acceptance:` line before the next deliverable begins. No other line
in this file starts with `D` immediately followed by a digit.

---

## A. Data plane, transparency and upstream configuration

**D1 — Transparent proxy in the upstream's own API shape.** What: a caller uses
the upstream's own API or SDK unchanged except for (a) the base URL, pointed at
the Janus data plane under that upstream's `path_prefix`, and (b) the credential
it presents, which is its caller identity instead of a real key. Janus defines
no API of its own in front of an upstream. Exercise: configure
`[[upstream]] name = "anthropic", path_prefix = "/anthropic", base_url = "https://api.anthropic.com"`
and a valid identity binding; point a stock Anthropic SDK client at
`http://<data_plane.bind>/anthropic` with a caller identity token as its key;
send an ordinary request. Acceptance: the SDK call succeeds and returns the same
status and body the upstream returns for a direct call made with a real
credential; the caller's code needed no change other than base URL and key.

**D2 — The three launch upstreams.** What: Anthropic, OpenAI and OpenRouter are
each supported as upstreams, each configured as an `[[upstream]]` block with
`name`, `path_prefix` and `base_url`; the upstream name is one of `"anthropic"`,
`"openai"`, `"openrouter"`. Exercise: configure all three upstreams with
credentials in the pool and make one ordinary API call through each prefix.
Acceptance: each of the three calls is forwarded to its configured `base_url`
and returns the upstream's real response; `GET /v1/status` lists all three
upstreams.

**D3 — Request target and path rewriting.** What: Janus rewrites the target of
the inbound request to the real upstream: the request path under `path_prefix`
is sent to that upstream's `base_url`. Exercise: send
`POST /anthropic/v1/messages` to the data plane with a valid identity.
Acceptance: the upstream (or a recording test upstream at `base_url`) receives
the request at the upstream path without the Janus `path_prefix`, and the
request-log record shows `upstream = "anthropic"`, `method` and `path`.

**D4 — Per-upstream auth is applied in the form that upstream expects.** What:
the real credential is applied using each upstream's own auth scheme (header
name and scheme, or signing procedure), so the upstream sees a normal, correctly
authenticated request and has no knowledge a proxy or a distinct caller identity
is involved. This file does not state each upstream's header/scheme; the real
upstream is the judge of "expected form". Exercise: with real credentials in the
pool, make one authenticated call through each of the three upstreams to the
real upstream; additionally route each through a recording upstream at
`base_url`. Acceptance: each real upstream accepts the call (it does not answer
with an authentication error); at the recording upstream, each outbound request
contains the pool credential's bytes and does not contain the caller's identity
token anywhere.

**D5 — Outbound request is constructed from a per-upstream forwarding
allowlist.** What: the outbound request is built from scratch; only headers on
that upstream's forwarding allowlist are copied. Anything not on the list —
including whatever header/query/basic-auth field carried the caller identity —
is absent because it was never added. Exercise: send a request carrying the
identity in `authorization`, plus an arbitrary custom header (e.g.
`x-sandbox-secret: abc`) and a correlation header; observe the outbound request
at a recording upstream. Acceptance: the outbound request contains no caller
identity material in any location and contains none of the non-allowlisted
headers; allowlisted headers (e.g. content type) are present.

**D6 — Identity-bearing location is asserted empty on the way out.** What: on
top of the allowlist, the substitution stage checks that the exact location the
identity was read from is empty on the outbound request. A misconfigured
allowlist that wrongly includes the identity-bearing header fails loudly rather
than leaking the caller's token upstream. Exercise: (test build / test
configuration) make an upstream allowlist include the header the identity
binding reads from, then send a request. Acceptance: the request is not
forwarded with the token — it fails with an error (visible in the request log
`error` / daemon log) and the upstream never receives the caller's identity
token.

**D7 — Response relayed unmodified.** What: Janus never transforms responses:
the caller receives the upstream's own response, relayed unmodified. Janus also
does not augment requests beyond the substitution (a plugin cannot inject
headers or edit the body) and supports only HTTP upstreams (no gRPC passthrough
or other protocols). Exercise: call an endpoint through Janus and the same
endpoint directly with a real key (or via a recording upstream returning a known
status and body); compare. Acceptance: response status and body bytes received
by the caller are identical to what the upstream sent; no Janus-specific fields
are added to the body.

**D8 — Streaming responses are relayed as they arrive.** What: when an upstream
streams (e.g. server-sent events from Anthropic `stream: true`, OpenAI or
OpenRouter streaming completions), Janus relays the stream incrementally without
buffering the whole body. Exercise: issue a streaming request through each of
the three upstreams (or against a test upstream that emits events with delays
between them). Acceptance: the caller receives the first events before the
upstream has finished the response (time-to-first-byte through Janus is close to
the upstream's, not the full-response time), and the concatenated stream is
byte-identical to the upstream's.

**D9 — The caller is never told which credential was used.** What: nothing that
crosses back to the caller identifies or contains the real credential; there is
no handshake that returns a credential to the caller. Exercise: make successful,
denied, and failed calls; inspect every response header and body the caller
receives. Acceptance: no response to a caller contains the credential material,
a refresh token, or the `credential_ref`; a successful response is
indistinguishable from calling the upstream directly.

---

## B. Caller identity extraction and verification

**D10 — `hmac` method: wire format and a valid token accepted.** What: a binding
with `method = "hmac"` verifies a signed claims blob with a shared secret named
by `secret_ref`, and projects the claim named by `identity_claim` into
`caller_identity`. Wire format (fixed by the spec): the presented material is
`<claims-hex>.<signature-hex>`, where `claims-hex` is the hex encoding of the
claims JSON object, and `signature-hex` is the lowercase-hex HMAC-SHA256, keyed
with the shared secret, of the ASCII bytes of `claims-hex`. The claims object
carries `ns` (namespace, a string), `exp` (expiry, Unix seconds), and the claim
named by `identity_claim`. Because the format is fixed, a token minted by any
implementation of it — including a migrating host's existing minting code —
verifies; Janus itself does not mint. Exercise: configure
`[identity] method = "hmac", source = { header = "authorization", scheme = "Bearer " }, secret_ref = "caller-identity", namespace = "janus-data-plane", identity_claim = "subject"`
with secret `S`. Independently of Janus (e.g. a few lines of Python, or
`xxd`+`openssl dgst -sha256 -hmac S`), build claims
`{"ns":"janus-data-plane","exp":<now+3600>,"subject":"agent-1"}`, hex-encode it
to `H`, compute `SIG = lowercase hex HMAC-SHA256(S, ASCII(H))`, and send
`Authorization: Bearer H.SIG`. Acceptance: request is forwarded; request-log
record has `binding = "hmac"`, `outcome = "forwarded"`, and
`caller_identity = "agent-1"`.

**D11 — `hmac` method: bad signature, expiry and namespace separation.** What: a
token whose signature does not verify under the binding's secret is rejected;
correctly signed claims whose `exp` has passed are rejected; and a binding
declares its namespace, so a token whose `ns` differs fails to verify even
though its signature is good (this stops a token minted for one purpose being
replayed against another). Exercise: using the D10 format, send (a) a token
signed with a different secret, and one whose `claims-hex` was altered after
signing, (b) a correctly signed token with `exp` in the past, (c) a correctly
signed token with `"ns":"other"` to a binding with
`namespace = "janus-data-plane"`. Acceptance: each gets HTTP 401 and is not
forwarded; request-log `outcome = "rejected_identity"` with `kind` = (a)
`invalid`, (b) `expired` or `invalid` (the spec does not pin which for hmac),
(c) `wrong_namespace`.

**D12 — `jwt` method: JWKS or public key, issuer and audience enforced.** What:
`method = "jwt"` verifies a JWT from an external issuer against a JWKS endpoint
or a configured public key, checking issuer and audience, and projects a claim
into `caller_identity`. Janus never issues JWTs. Exercise: configure a jwt
binding with a JWKS URL (and separately with a static public key), issuer and
audience; send (a) a valid token, (b) a token with wrong `aud`, (c) wrong `iss`,
(d) signed by another key. Acceptance: (a) forwarded with log `binding = "jwt"`
and `caller_identity` equal to the projected claim, for both key sources;
(b)–(d) HTTP 401, not forwarded, `outcome = "rejected_identity"` with a `kind`
other than `no_material`; (c) has `kind = "wrong_namespace"` (the "different
namespace or issuer" class); the spec does not pin the kind for (b) beyond that,
and (d) is `invalid`.

**D13 — `opaque` method: registered token accepted with registry identity and
claims.** What: `method = "opaque"` looks a bare random token up in the daemon's
opaque-identity registry; the entry's `identity` becomes `caller_identity` and
its `claims` become `identity_claims`. Exercise:
`janus identity create --identity agent-7 --claim tenant=acme` (capture the
printed token); configure an opaque binding; send the token. Acceptance:
forwarded; log `binding = "opaque"`, `caller_identity = "agent-7"`; with a
plugin configured, `ResolveRoute` receives
`identity_claims = {"tenant": "acme"}`.

**D14 — `opaque` method: revocation and expiry.** What: revoking a registry
entry makes the very next request presenting that token fail (no restart, no
cache wait). An entry past its `expires_at` fails exactly as a revoked one does,
but the request log keeps the distinction. An unknown token is invalid.
Exercise: make a successful call with an opaque token,
`janus identity revoke <id>`, immediately repeat; create an identity with
`--expires-in 1s`, wait, use it; send a random unregistered token. Acceptance:
all three get HTTP 401; log `kind` = `revoked`, `expired`, `invalid`
respectively.

**D15 — `trusted_header` method: explicit-only, unverified.** What:
`method = "trusted_header"` takes the configured header's value as the identity
with no cryptographic check (for topologies where a front proxy already
authenticated the caller). It is never reached by default or by omission; it
must be named explicitly in config. Exercise: (a) configure
`method = "trusted_header"` with a header source `x-caller`; send a request with
`x-caller: alice`. (b) a binding with a `source = { header = "x-caller" }` but
`method` omitted entirely; (c) an `[upstream.identity]` override that sets only
`source = { header = "x-caller" }` over an hmac daemon default. In (b) and (c)
send `x-caller: alice` with nothing else. Acceptance: (a) forwarded with
`caller_identity = "alice"` and `binding = "trusted_header"`; in (b) and (c) the
request is never forwarded as `alice` and no record shows
`binding = "trusted_header"` (the daemon either refuses the config at startup or
rejects the request — the spec does not pin which).

**D16 — Identity sources: header (with optional scheme), query, basic
password.** What: each method is paired with a source:
`{ header = "<name>", scheme = "<prefix>" }` (optional prefix such as
`"Bearer "` stripped), `{ query = "<name>" }`, or `basic_password` (the password
of HTTP basic auth, for SDKs that only offer basic auth). Exercise: bind each
source in turn and present the same token as `Authorization: Bearer <tok>`,
`x-api-key: <tok>` (header without scheme), `?key=<tok>`, and
`Authorization: Basic base64(user:<tok>)`; also send the token in a location the
binding does not read. Acceptance: every correctly placed token verifies and
forwards with the same `caller_identity`; the outbound request carries none of
these locations' identity material; the misplaced token gets 401
`kind = "no_material"`.

**D17 — Binding precedence: route, then upstream, then daemon default.** What:
extraction is configured per request type and resolved most-specific-first: a
`[[upstream.route]]` binding (one path pattern), then the `[upstream.identity]`
binding, then the daemon-wide `[identity]` default. Exercise: configure a daemon
default hmac binding, an `openai` upstream override, and an `openai` route
`/v1/models` with its own identity block; send requests to the route path,
another openai path, and an anthropic path. Acceptance: each request is verified
by the expected binding (request-log `binding` and, for the route, `route` show
which applied).

**D18 — Upstream block merges field by field; route block replaces outright.**
What: an `[upstream.identity]` block is merged over the daemon default field by
field (overriding only `source` keeps the default `method`, `secret_ref`,
`namespace` and claim mapping). A route's `identity` block does not merge; it
replaces the resolved upstream binding entirely. Exercise: daemon default hmac
via `authorization: Bearer`; openai upstream sets only
`source = { header = "x-api-key" }`; present the same hmac token as `x-api-key`
and as `authorization` to openai. Then give an openai route
`identity = { method = "opaque", source = { header = "x-api-key" } }` and
present an opaque and an hmac token there. Acceptance: on openai the `x-api-key`
hmac token verifies (`binding = "hmac"`) and the `authorization` one is 401
`no_material`; on the route the opaque token verifies and the hmac token does
not.

**D19 — No resolving binding means rejection; no implicit unauthenticated
tier.** What: if no binding resolves for a request (e.g. no daemon default and
no upstream/route binding), the request is rejected. Omission never yields an
open route. Exercise: config with no `[identity]` default and an upstream with
no identity block; send a request to it. Acceptance: the request is rejected
(not forwarded, no credential spent); it never behaves as `method = "none"`.

**D20 — `method = "none"`: explicit identity-less route and what it emits.**
What: a route with `identity = { method = "none" }` is open: no identity is
extracted, the plugin's `ResolveRoute` is **not** called (not even with an empty
identity), and the default chain chooses the credential, exactly as in no-plugin
mode. There is no config-level allow/deny on such a route. Such a call produces
a request-log record, may produce `credential_invalid`/`account_error` Notify
events if the upstream triggers them, and produces **no** `call_completed` or
`call_abandoned`. Exercise: with a plugin configured, add
`[[upstream.route]] path = "/v1/models", identity = { method = "none" }` and
call `GET /openai/v1/models` with no credential; then make the upstream reject
the default credential on that route. Acceptance: the call is forwarded using a
default-chain credential; the plugin records zero `ResolveRoute` calls and no
`call_completed`/`call_abandoned` for it, but does receive `credential_invalid`
in the rejection case; the request-log record has empty `caller_identity` and
`binding = "none"`.

**D21 — Identity rejection happens before anything else.** What: extraction
failure is an HTTP 401 to the caller and nothing else happens: no plugin call,
no credential read from the pool, no upstream connection, and the request body
is never buffered (extraction reads only method, path and headers). Exercise:
with a plugin and a recording upstream, send a request with no/invalid identity
and a large (or never-ending) body. Acceptance: caller gets 401 promptly without
Janus reading the body; plugin sees zero `ResolveRoute` and zero Notify events
for it; the upstream sees no connection; the credential's
`last_used_at`/`request_count` are unchanged.

**D22 — Rejection kinds are distinguishable.** What: identity rejections
distinguish no material presented, material present but invalid, material valid
but for a different namespace or issuer, and (for opaque entries) expired versus
revoked. Exercise: produce each case: misplaced token (D16), tampered hmac
(D11a), wrong hmac namespace (D11c), expired and revoked opaque entries (D14).
Acceptance: request-log `outcome = "rejected_identity"` with `kind` respectively
`no_material`, `invalid`, `wrong_namespace`, `expired`, `revoked`;
`janus log query --outcome rejected_identity` shows them.

**D23 — Claimed identity on rejected requests is log-only.** What: for a
rejected request, the log records `claimed_identity` — the identity the material
claimed, decoded without verification. It is never used to authorize anything.
Exercise: send an expired hmac token whose subject is `bob`. Acceptance: record
has `outcome = "rejected_identity"`, empty `caller_identity`,
`claimed_identity = "bob"`; the request was not forwarded and no plugin call
carried `bob`.

**D24 — Verified claims reach the plugin separately from the identity.** What:
the extractor's remaining verified claims become
`ResolveRouteRequest.identity_claims` (a string map), so policy can use e.g. an
agent-type or tenant marker without parsing `caller_identity`. Empty when the
method yields only an identity. Exercise: hmac/jwt token carrying `subject = s1`
and `agent_type = coder`; plugin logs its `ResolveRoute` input. Acceptance:
plugin receives `caller_identity = "s1"` and `identity_claims` containing
`agent_type = "coder"`.

**D25 — Correlation values: from a verified claim or an unverified header, never
trusted.** What: `correlation_id` may be lifted from a verified claim
(`correlation_claim = "session"`) or from an unverified passthrough header
(`correlation.from_header = "x-request-id"`). It appears in `ResolveRoute`,
Notify events and the request log, and is a grouping hint only; omitting both
settings yields an empty value. Exercise: configure each variant; send requests
with a `session` claim / an `x-request-id` header. Acceptance: `correlation_id`
in `ResolveRoute`, Notify and the log equals the claim or header value; with
neither configured it is empty; changing the header alone never changes
`caller_identity` or the routing decision Janus makes.

**D26 — Custom identity extractors in a custom build.** What: an operator can
implement the `IdentityExtractor` trait in a custom build, register it via
`Embedded { extractors: vec![...] }`, and reference it as
`method = "custom:<name>"`. The log's `binding` shows `custom:<name>`. Exercise:
build a custom daemon registering extractor `introspect`; bind a route to
`method = "custom:introspect"`; send a request. Acceptance: the request is
accepted/rejected according to the custom extractor; log
`binding = "custom:introspect"`; a config naming `custom:missing` makes the
daemon refuse to start (see D84).

---

## C. Plugin: ResolveRoute

**D27 — No `[plugin]` block means zero contract calls; minimal standalone
config.** What: with no `[plugin]` block in the config, the daemon makes zero
calls defined in `janus.proto` (no `ResolveRoute`, no `Notify`) and acts as a
plain credential-substituting pass-through proxy using its default chain. A
missing plugin cannot break a working proxy. "Configured" means only that a
`[plugin]` block is present. No external service is required. Exercise: run with
a config containing only `[[upstream]]` blocks, pool credentials and a
daemon-default `[identity]` binding (no `[plugin]`), and with a gRPC listener
(or socket) on the address a plugin would use; send traffic through all
upstreams. Acceptance: all traffic forwards; the listener receives zero
connections/calls; `GET /v1/status` shows `plugin.mode = "none"`.

**D28 — ResolveRoute request contents.** What: for each request with a verified
identity when a plugin is configured, the daemon calls `ResolveRoute` with
`request_id` (Janus-assigned, unique, always populated, never caller-supplied),
`upstream` (`"anthropic" | "openai" | "openrouter"`), `caller_identity`
(verified), `correlation_id` (may be empty), `identity_claims`. Exercise: run a
recording plugin; send several requests, including one where the caller sends
its own `x-request-id`. Acceptance: each call has a distinct non-empty
`request_id` unrelated to any caller-supplied value, the correct `upstream`, and
a non-empty verified `caller_identity`; the same `request_id` appears on the
request-log record and on that request's Notify events.

**D29 — Outcome `credential_ref`: use this credential.** What: when the plugin
answers `credential_ref = "<ref>"`, the daemon substitutes exactly that pool
credential. Exercise: plugin returns `credential_ref = "acct-b"` for identity B
and `"acct-a"` for identity A on the same upstream. Acceptance: log records show
`credential_ref = "acct-b"` and `"acct-a"` respectively; the upstream sees each
account's credential; per-caller account separation works.

**D30 — Outcome `use_default`: defer to the default chain.** What:
`use_default = true` means "allowed, Janus picks" from its default chain.
Exercise: plugin returns `use_default`. Acceptance: request forwarded with the
credential `GET /v1/credentials/default?upstream=` reports;
`call_completed.credential_ref` and the log record name the credential actually
used.

**D31 — Outcome `deny_reason`: the single policy-denial channel.** What:
`deny_reason = "<text>"` refuses the request. Auth/authorization/credit/rate
denials all come back this way and the daemon does not distinguish them; the
request fails and the reason reaches the caller. Exercise: plugin returns
`deny_reason = "budget exhausted"`. Acceptance: the caller gets a non-success
error response whose body contains `budget exhausted`; nothing is forwarded
upstream; log `outcome = "denied"` with `reason = "budget exhausted"`. (The
exact HTTP status is not fixed by the spec.)

**D32 — Fail-closed: plugin unreachable.** What: if the plugin cannot be
reached, the request is rejected; the daemon never silently falls back to a
default credential. Exercise: configure
`endpoint = "unix:///run/janus/policy.sock"` with no plugin listening (or a
stopped network plugin); send a request. Acceptance: request fails, not
forwarded; log `outcome = "plugin_failed"` with class `plugin_unreachable`;
`GET /v1/plugin` `health = "unreachable"` with `last_error` set.

**D33 — Fail-closed: plugin timeout (`timeout_ms`, default 250).** What:
`[plugin] timeout_ms` (default 250 ms) is the deadline on each `ResolveRoute`;
no answer within it fails the request closed. `Notify` has no deadline.
Exercise: (a) set `timeout_ms = 1000` and have the plugin sleep 2 s before
answering; (b) omit `timeout_ms` and have the plugin sleep 1 s; (c) omit
`timeout_ms` and have the plugin answer after 100 ms. Acceptance: (a) the
request fails after roughly 1 s; (b) the request fails after roughly 250 ms
(well under 1 s); (c) the request succeeds; failures have log
`outcome = "plugin_failed"` class `plugin_timeout` and `/v1/plugin` health
`timeout`.

**D34 — Fail-closed: plugin RPC error or missing outcome.** What: an RPC error
returned by a reached plugin fails the request closed; a `ResolveRouteResponse`
whose `oneof outcome` is unset is a protocol error and also fails closed (never
interpreted as `use_default`). Exercise: plugin returns (a) a gRPC error status,
(b) an empty `ResolveRouteResponse {}`. Acceptance: both requests fail and are
not forwarded upstream; (a) has log `outcome = "plugin_failed"` with class
`plugin_error` and `/v1/plugin` health `error`; for (b) the spec fixes only the
failed, unforwarded request (no specific class or health value).

**D35 — Fail-closed: TLS failures classified.** What: on the network transport,
handshake failures fail requests closed with a specific class:
`plugin_tls_error` (e.g. plugin cert not signed by the configured anchor / wrong
name), `plugin_no_client_auth` (plugin did not request a client certificate),
`plugin_rejected_client_cert` (plugin rejected the daemon's client cert).
Exercise: run three TLS plugins: one with an untrusted cert, one that requests
no client cert, one that trusts a different client CA. Acceptance: requests fail
with log classes `plugin_tls_error`, `plugin_no_client_auth`,
`plugin_rejected_client_cert` respectively; `/v1/plugin` health shows
`tls_error`, `no_client_auth`, `rejected_client_cert`.

**D36 — Fail-closed: linked plugin panic.** What: a panic inside a linked
plugin's `resolve_route` is caught and turned into a plugin error, which fails
that request closed; it does not unwind into the request task or crash the
daemon. Exercise: custom build with a plugin that panics for identity `crash`;
send a request as `crash`, then as another identity. Acceptance: the `crash`
request fails and is not forwarded upstream; the daemon process keeps running
and the next request is served normally. (The spec does not pin the log
outcome/class for this case.)

**D37 — ResolveRoute is never called without a verified identity.** What: the
only paths to `ResolveRoute` carry a verified non-empty `caller_identity`;
rejected requests and `none` routes never reach it. Exercise: send rejected
requests and `none`-route requests with a recording plugin. Acceptance: the
plugin records no `ResolveRoute` with empty `caller_identity`, and none for
these requests.

---

## D. Plugin: Notify

**D38 — `call_completed` event.** What: when a request that was sent to
`ResolveRoute` reaches the upstream and a response comes back, the daemon emits
`event_type = "call_completed"` with the request's `request_id`, its
`correlation_id`, and `payload_json` containing `caller_identity`, `upstream`,
`status`, `credential_ref` (what was actually used, including under
`use_default`), `duration_ms`, and `usage` (the per-upstream observation of what
the call consumed). Exercise: plugin returns `use_default`; make a successful
Anthropic call. Acceptance: the plugin receives exactly one `call_completed`
with matching `request_id`, the upstream HTTP status, the resolved
`credential_ref`, and a `usage` object identical to the request-log record's
`usage`.

**D39 — `call_abandoned` after a resolve that never reached the upstream.**
What: if `ResolveRoute` answered and the call then never reached the upstream
(e.g. the credential could not be made usable, or the request was denied), the
daemon emits `call_abandoned` with `request_id` and `payload_json` containing
`caller_identity`, `stage` (which lifecycle step it died at) and `reason`.
Exercise: plugin returns `credential_ref` naming a disabled credential (or one
whose refresh fails). Acceptance: plugin receives exactly one `call_abandoned`
with that `request_id`, a `stage` naming the credential step, and a `reason`; no
`call_completed` for it.

**D40 — `call_abandoned` when ResolveRoute itself failed.** What: if
`ResolveRoute` was sent but timed out, errored, or the plugin became unreachable
mid-call, the daemon still emits `call_abandoned` with `stage` naming the
resolve step, so a plugin that charged before the daemon gave up can refund.
Exercise: plugin charges then sleeps past `timeout_ms`; once it is responsive
again, observe Notify. Acceptance: plugin receives a `call_abandoned` with the
timed-out request's `request_id` and a resolve-step `stage`.

**D41 — Exactly one settlement event per resolved request.** What: for every
request `ResolveRoute` was sent for, exactly one of `call_completed` or
`call_abandoned` is emitted (subject to best-effort delivery, D46); none are
emitted for requests never sent to `ResolveRoute` (no plugin, `none` routes,
identity rejections). Exercise: drive a mix of successful, denied,
credential-failed, timed-out, rejected-identity and `none`-route requests
against a healthy recording plugin. Acceptance: for each `request_id` the plugin
saw in `ResolveRoute` it receives exactly one settlement event (never zero,
never both, never two); it receives none for request_ids it never resolved.

**D42 — `credential_invalid` event.** What: when the upstream rejects the
credential Janus substituted, the daemon emits `credential_invalid`. Exercise:
put a revoked/garbage API key in the pool and route a request to it. Acceptance:
plugin receives a `credential_invalid` event; the credential's admin `status`
becomes `invalid` (visible in `janus credential show <ref>`).

**D43 — `account_error` event.** What: when the upstream reports a problem with
the account behind the credential (rather than with the request), the daemon
emits `account_error`. The spec defines no payload for it. Exercise: use a test
upstream that returns an account-level error (e.g. billing/account disabled) for
a specific key; route a request to that key. Acceptance: plugin receives a
Notify event with `event_type = "account_error"`.

**D44 — Notify is off the critical path.** What: Notify is fire-and-forget: the
caller's response never waits on it; a slow, blocking or absent Notify listener
cannot stall or fail a response; failures are logged, never surfaced to the
request path; the daemon does not inspect the reply beyond acknowledging
delivery. Exercise: plugin answers `ResolveRoute` instantly but blocks forever
(or errors) on its `Notify` stream; send requests. Acceptance: callers'
responses arrive with normal latency and content; no request fails because of
Notify.

**D45 — Notify envelope is generic and extensible.** What: one
`Notify(stream NotifyEvent) returns (NotifyAck)` RPC carries every event kind as
`event_type` (freeform), `request_id` (empty for events not about one request),
`correlation_id`, `payload_json` (JSON string). A linked plugin's `notify` is
called once per event and returns nothing. Exercise: gRPC plugin consumes the
client stream; linked plugin counts `notify` invocations. Acceptance: all events
arrive on the single client-streaming RPC with `payload_json` parseable as JSON;
the linked plugin's call count equals the number of events.

**D46 — Notify reconnection, and the accepted limitation on Notify loss.** What:
the gRPC plugin client owns the `Notify` stream and its reconnection: after a
plugin restart, events emitted afterwards are delivered on a re-established
stream. Delivery is best-effort: an event lost while the plugin was unreachable
is not guaranteed to arrive; the plugin's phase-1 charge for it stays in place
(never refunded) and its phase-2 cost is unrecorded. This is an accepted,
documented limitation (tracked as janus issue #1). Settlement events repeat
`caller_identity` so a plugin that restarted between `ResolveRoute` and
settlement can still account for the call when the event does arrive. Exercise:
with a gRPC plugin, make requests; stop the plugin; restart it; make more
requests (no daemon restart). Acceptance: the request results during the outage
are fail-closed as in D32 and otherwise unaffected by Notify; after the restart,
the plugin receives `ResolveRoute` and the settlement events for the new
requests over a new `Notify` stream, each settlement event carrying
`caller_identity`. Loss of events emitted during the outage is not a failure of
this deliverable.

---

## E. Plugin configuration, transport and channel security

**D47 — `[plugin]` block: exactly one of `endpoint` or `linked = true`.** What:
the config file is the only source of plugin settings (no environment variable,
no command-line flag for individual plugin settings). Inside a present
`[plugin]` block exactly one of `endpoint` or `linked = true` must appear.
Exercise: start the daemon with (a) both set, (b) a `[plugin]` block with
neither, (c) a `JANUS_PLUGIN`-style env var / CLI flag attempting to set the
endpoint. Acceptance: (a) and (b) refuse to start with an error; (c) has no
effect on the plugin configuration.

**D48 — Endpoint scheme selects the transport.** What: `unix://` = local socket;
`https://` = network with mutual TLS, requires `[plugin.tls]`; `http://` refused
unless `allow_plaintext = true` is set alongside it (then a prominent warning is
logged at startup); there is no loopback exception; any other scheme refused.
Exercise: start with `https://` and no `[plugin.tls]`; `http://policy:7000`
without and with `allow_plaintext = true`; `http://127.0.0.1:7000` without
`allow_plaintext`; `ftp://x`. Then a valid `unix://`, `https://` and linked
config. Acceptance: `https://` without tls block, both `http://` endpoints
without `allow_plaintext` (including the loopback one), and `ftp://` refuse to
start; `http://` + `allow_plaintext = true` starts, logs a warning, and
`/v1/plugin` shows `mode = "plaintext"`. `unix://` shows `mode = "unix"`,
`https://` shows `mode = "tls"`, linked shows `mode = "linked"`.

**D49 — Plugin TLS material validated at startup.** What: at startup the daemon
checks that every named file exists and parses, the client key matches the
client cert, the cert is not expired, `client_key` is mode `0600`, and exactly
one of `ca_file`/`pinned_cert` is given. Failure refuses startup. Exercise:
start with (a) missing ca_file, (b) mismatched key, (c) expired client cert, (d)
key mode `0644`, (e) both `ca_file` and `pinned_cert`, (f) neither. Acceptance:
each case refuses to start with a diagnostic; a correct config starts.

**D50 — Plugin reachability is not a startup requirement.** What: a plugin may
come up after the daemon; the daemon starts with the plugin down, and
unreachability is handled per request (fail-closed). Exercise: configure a valid
endpoint with no plugin running; start the daemon; send a request; then start
the plugin and send another. Acceptance: daemon starts; first request fails
`plugin_unreachable`; `/v1/plugin` `health` is `unknown` before any call,
`unreachable` after; second request succeeds and `health = "ok"` with
`last_ok_at` set.

**D51 — Unix-socket path hygiene and peer-credential check.** What: for a
`unix://` plugin, the daemon walks every directory on the socket path and
rejects it if any directory is writable by group or others, or owned by anyone
other than root, the daemon's user, or the owner of the socket's own directory.
At startup over the existing part of the path (refuse to start on failure);
again over the full path before every connection and reconnection (a failure
then fails the request as `plugin_unreachable` with the offending directory in
`last_error`). For plugin authors, `janus-core` ships a `tonic` interceptor that
reads the connecting peer's uid (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on
macOS). Exercise: (a) start with a parent directory mode `0777`; (b) start
correctly, then `chmod o+w` a parent directory and send a request; (c) a Rust
plugin uses the interceptor and is connected to by the daemon user and another
user. Acceptance: (a) refuses to start; (b) request fails `plugin_unreachable`
and `/v1/plugin` `last_error` names the offending directory; a correctly
permissioned layout (socket `0660`, dir `0750`, daemon user in the group) works;
(c) the interceptor exposes the peer uid so the plugin can reject the non-daemon
user.

**D52 — Network transport: mutual TLS against an operator-named anchor only.**
What: the daemon verifies the plugin against exactly one trust anchor (`ca_file`
private CA, or `pinned_cert`), never system roots; checks the plugin identity
against the endpoint hostname, or `server_name` when set, or the exact URI SAN
`expected_san` (replacing the hostname check); presents
`client_cert`/`client_key`; TLS 1.3 only. Exercise: plugin with a publicly
trusted cert not under the configured anchor; plugin with correct anchor but a
cert for a different hostname; the same plugin with `server_name` set to the
cert's name; SPIFFE cert with `expected_san` set; plugin offering only TLS 1.2.
Acceptance: public-CA plugin and wrong-host plugin fail `plugin_tls_error`; with
`server_name` matching the cert the same plugin succeeds; SPIFFE plugin with
matching `expected_san` succeeds; TLS 1.2-only plugin fails; the plugin observes
a client certificate on successful handshakes.

**D53 — Refuse a plugin that does not request a client certificate.** What: a
TLS plugin server that does not request a client cert is treated as
misconfigured; requests fail closed with `plugin_no_client_auth`. Exercise: run
a TLS plugin with client auth disabled. Acceptance: every request fails with
class `plugin_no_client_auth`; none are forwarded.

**D54 — Certificate reload and expiry reporting.** What: cert and key files are
re-read when they change on disk and on `SIGHUP`, keeping the last good material
if new files fail to parse; both certificates' expiry is reported on the admin
surface (`client_cert_expires_at`, `server_cert_expires_at`); a warning is
logged when either is within 30 days of expiry. Exercise: replace the client
cert files on disk / send `SIGHUP`; then write a corrupt file; use a cert
expiring in 10 days. Acceptance: new material is used without restart; corrupt
files leave traffic working on the old material; `/v1/status` shows both expiry
timestamps; a warning about the near-expiry cert appears in the daemon log.

**D55 — `janus plugin tls init` bootstraps (and rotates) a two-party PKI.**
What: `janus plugin tls init --plugin-host <name> [--out <dir>] [--days N]`
generates two self-signed key pairs, writes `daemon.pem`, `daemon-key.pem` (mode
`0600`), `plugin.pem`, `plugin-key.pem` (plugin cert carries `<name>` as SAN),
and prints the `[plugin.tls]` block pinning `plugin.pem` plus instructions that
the plugin must serve with `plugin.pem`/`plugin-key.pem` and accept only
`daemon.pem` as client. Rotation is running it again and restarting both sides.
Exercise: run it with `--plugin-host policy.internal --out ./pki --days 30`;
configure both sides as printed; then run it again into `./pki` and restart
daemon and plugin. Acceptance: the four files exist in `./pki`, `daemon-key.pem`
is mode `0600`, `plugin.pem` has SAN `policy.internal` and ~30-day validity;
with the printed config requests succeed and `health = "ok"`; after the re-run
the files contain new keys/certs (different fingerprints) and after restarting
both sides requests succeed again.

**D56 — Active channel check: `janus plugin check` / `POST /v1/plugin/check`.**
What: on TLS, performs a full handshake with configured material, then two
handshakes the plugin must reject: one with a throwaway self-signed client cert,
one with no client cert; each probe reads from the connection to observe a TLS
1.3 post-handshake rejection, and none issues a `ResolveRoute`. On a unix
socket, reports the path-permission walk and a connection attempt. Returns each
result. Exercise: run against (a) a correctly configured mTLS plugin, (b) a
plugin that accepts any client cert, (c) a plugin that requests but does not
require a cert, (d) a unix-socket plugin. Acceptance: (a) reports the full
handshake ok and both probes rejected; (b) reports the self-signed probe
accepted (misconfigured); (c) reports the no-cert probe accepted; (d) reports
the walk result and connection attempt; the plugin records zero `ResolveRoute`
calls from any check.

---

## F. Linked plugins and custom builds

**D57 — Linked plugin via custom build.** What: a linked plugin is compiled in
(no dynamic loading): a separate crate depends on `janus-daemon`, implements the
`JanusPlugin` Rust trait (`resolve_route` →
`RouteOutcome::{Credential, UseDefault, Deny{reason}}`, `notify`), and calls
`janus_daemon::run(config, Embedded { plugin: Some(...), extractors: vec![] })`
with `[plugin] linked = true`. Exercise: build such a binary with a plugin that
denies identity `x` and defers otherwise. Acceptance: `x` is denied (log
`denied`), others forwarded via the default chain; `/v1/status`
`plugin.mode = "linked"`; settlement events reach the linked `notify`.

**D58 — Config/linkage disagreement is a startup failure; stock binary has no
linked plugin.** What: the stock `janus-daemon` passes `Embedded::default()`
(empty), so it cannot have a linked plugin. `linked = true` with
`Embedded::plugin = None` refuses to start; a configured `endpoint` while a
plugin is linked also refuses to start. Exercise: run the stock binary with
`[plugin] linked = true`; run a custom build with a linked plugin and
`[plugin] endpoint = "unix://..."`. Acceptance: both exit non-zero with an error
before binding the data plane; in neither case does the daemon start as an open
pass-through.

---

## G. Credential pool

**D59 — Pool is directory-per-slot with kind auto-detection and source
reporting.** What: real credentials live on the filesystem, one directory per
slot (slot name = `ref`), outside `janus.db`. A slot's kind is auto-detected
from its contents: `.credentials.json` → Anthropic (Claude CLI OAuth),
`auth.json` → OpenAI/Codex, a plain API key → OpenRouter. Each credential
reports `source` of `pool`, `env` or `file`. The `kind` value vocabulary ("how
auth is carried for that upstream") is not fixed by the spec. Exercise: create
slots containing each kind (plus a credential from env/file if configured);
`janus credential list --json`. Acceptance: each is listed with `ref` equal to
its slot name, the correct `upstream` (`anthropic`, `openai`, `openrouter`
respectively), the correct `source`, and a non-empty `kind`; `janus.db` contains
no credential material.

**D60 — OAuth refresh before use.** What: when the selected credential has
expired, the pool refreshes it before use (Anthropic Keychain-backed OAuth;
OpenAI/Codex `auth.json`-backed OAuth). Exercise: use an OAuth credential whose
access token has expired but whose refresh token is valid; send a request.
Acceptance: request succeeds; `expires_at` moves into the future; the caller
never sees the refreshed token.

**D61 — Unusable credential fails the request, never falls through.** What: if
the selected credential cannot be made usable (refresh fails, disabled,
missing), the request fails; Janus does not silently pick a different
credential. Exercise: plugin names a ref whose refresh fails (or a disabled ref,
or a nonexistent ref) while other healthy credentials exist. Acceptance: the
request fails and is not forwarded; no other credential's `request_count`
increments; log `outcome` is `credential_unavailable` or `abandoned` (the spec
does not pin which for this case); the plugin receives `call_abandoned` for it.

**D62 — Default chain selection.** What: with no plugin, on a `none` route, or
on `use_default`, the default chain picks a credential for the upstream;
disabled credentials are excluded. Exercise: two credentials for openai; disable
one; send requests; query `GET /v1/credentials/default?upstream=openai`.
Acceptance: requests use only the enabled credential; the default endpoint
reports that same credential's metadata.

**D63 — Upstream credential rejection updates credential health.** What:
credential `status` reflects reality (`active` | `invalid` | `disabled` |
`rate_limited`, with `rate_limited_until`), and `request_count`, `error_count`,
`last_used_at`, `last_error` are maintained. Exercise: make calls with a good
credential, a rejected one, and one the upstream rate-limits. Acceptance:
counters increment; the rejected one shows `invalid` with `last_error`; the
rate-limited one shows `rate_limited` and a `rate_limited_until`.

**D64 — Credential material never leaks.** What: no data-plane response, admin
endpoint, CLI output, UI view or request-log record contains credential
material, a token or a refresh token. Endpoints that accept material return
metadata only. Exercise: add, show, list, rotate credentials with `--json`;
browse the UI; `janus log show`; grep all output for the key bytes. Acceptance:
the secret string appears nowhere in any output or response.

---

## H. Metering primitives (plugin-author library in `janus-core`)

**D65 — Meter refuses a `Total` quota on a volatile store.** What:
`Meter::new(store, quotas)` returns an error if any quota has `Window::Total`
and the store's `durability()` is `Volatile`;
`Meter::new_allowing_volatile_total` is the explicit opt-in. Exercise:
`Meter::new(MemoryUsageStore, [("requests", Quota{limit: 10, window: Total})])`;
then the same with `new_allowing_volatile_total`; then `Meter::new` with
`SqliteUsageStore`. Acceptance: first returns `Err(UsageError)`; second and
third construct successfully.

**D66 — `charge` is an atomic check-and-increment across all quotas.** What:
phase 1 `charge(request_id, identity, meter, amount)` checks and increments
against every quota on the meter in one step, so two concurrent requests cannot
both pass a limit with one unit left. Returns `Allowed{used, limit}` (quota
nearest exhaustion) or `Exceeded{used, limit}` (the exceeded quota). Exercise:
limit 10, used 9; fire two concurrent `charge(.., 1)` with different
request_ids. Acceptance: exactly one `Allowed`, one `Exceeded`; final count 10.

**D67 — `Exceeded` consumes nothing; `charge` is idempotent per request_id.**
What: an `Exceeded` charge leaves every counter where it was; repeating a
`charge` with the same `request_id` returns the original result and changes
nothing. Exercise: charge to exceed; read counts; repeat a successful charge
with the same id. Acceptance: counts unchanged after `Exceeded`; repeat charge
returns the same result and count does not increase.

**D68 — `check` is read-only.** What: `check(identity, meter)` returns
`Exceeded` if the identity is already at or over any quota on the meter, without
changing counts; for dimensions known only after the call. Exercise: record cost
to reach the limit; call `check` repeatedly. Acceptance: `Exceeded` each time;
counts never change.

**D69 — `record` adds unconditionally and idempotently.** What: phase 2
`record(request_id, identity, meter, amount)` adds even when it crosses the
limit; idempotent per `request_id` (within the ledger TTL). Exercise: limit 100,
used 90; `record(r1, .., 50)` twice. Acceptance: count is 140 (not rejected, not
190); next `check` returns `Exceeded`.

**D70 — `refund` reverses the charge in the window it landed in.** What:
`refund(request_id, identity, meter)` reverses the phase-1 charge in whichever
window it was charged, even after a period boundary; returns `false` if there
was nothing to reverse (unknown, already refunded, or settled). Exercise:
`Fixed{period: 1 min}`; charge in window W1; wait into W2; refund; refund again.
Acceptance: W1's count decrements, W2's is untouched; first refund `true`,
second `false`.

**D71 — `settle`, `compact` and ledger TTL (default one hour).** What: `settle`
marks a charge final (the store may forget how to reverse it; a later refund
returns `false`). `compact(now)` drops keys whose `retain_until` passed and
ledger entries older than the ledger TTL (store configuration, default one
hour), returning how many were dropped. Past the TTL a late `refund` returns
`false` and a redelivered `record` counts again. Exercise: (a) charge then
settle then refund; (b)–(d) run on a meter whose only quota is
`Fixed { period: 24 h }` (so no counter key reaches its `retain_until` during
the test and only ledger-TTL expiry is exercised), with a store at its default
configuration: (b) `charge(r1, ..)` at time T, `compact(T + 59 min)`, then
`refund(r1)`; (c) `charge(r2, ..)` at T, `compact(T + 61 min)`, then
`refund(r2)`; (d) `record(r3, ..)` at T, `compact(T + 61 min)`, `record(r3, ..)`
again. Acceptance: (a) refund is `false`; (b) refund is `true` (entry survived
under the default TTL); (c) compact returns a positive count and refund is
`false`, the charge stands; (d) the redelivered record increments the count
again.

**D72 — Multiple meters and quotas per identity.** What: a plugin can hold
several named meters per identity (e.g. `"requests"`, `"input_tokens"`), each
with one or more quotas (e.g. per-minute and per-day), configured by the plugin.
Exercise: meter `requests` with per-minute limit 2 and per-day limit 100; charge
3 times in a minute. Acceptance: third charge `Exceeded` reporting the
per-minute quota's `limit = 2`; other meters unaffected.

**D73 — `UsageStore` trait semantics.** What: the store trait has
`apply(op, bounds, amount)` (all-or-nothing across bounds of one
`(identity, meter)`, idempotent on `op`, returning `Applied{used}` or
`Rejected{index, used}` for the first bound that would exceed), `release(op)`,
`settle(op)`, `get(key)`, `compact(now)`, `durability()`. Exercise: plugin
author calls `apply` with two bounds where the second would exceed. Acceptance:
returns `Rejected{index: 1, ..}` and neither key changes; repeating an applied
op returns the original `Applied` result.

**D74 — `MemoryUsageStore` is volatile.** What: in-memory store,
`durability() = Volatile`, expired windows swept on write, loses everything on
restart. Exercise: count, drop and recreate the store. Acceptance:
`durability()` is `Volatile`; counts are zero after recreation.

**D75 — `SqliteUsageStore` is durable and single-process.** What: behind the
`sqlite` feature; `durability() = Durable`; file at a plugin-chosen path in WAL
mode with `synchronous=FULL`; each `apply` one `BEGIN IMMEDIATE` transaction via
one writer thread; takes an exclusive advisory lock at open and refuses a file
another process holds. Exercise: charge a `Total` quota, restart the plugin
process, read; open the same file from a second process; after closing the
store, run `sqlite3 <file> 'PRAGMA journal_mode;'`. Acceptance: `durability()`
is `Durable`; count survives restart; second process's open fails with an error;
`journal_mode` reports `wal`.

**D76 — Store conformance suite.** What: behind the `testing` feature, a suite
any `UsageStore` runs against itself: apply atomicity under concurrency,
idempotency on repeated op, all-or-nothing across bounds, release landing in the
charged window, and compact. Both shipped stores pass. Exercise:
`cargo test --features testing,sqlite` in `janus-core`; run the suite against a
third-party store. Acceptance: the suite runs and passes for `MemoryUsageStore`
and `SqliteUsageStore`.

**D77 — Metering is a library, not a daemon service.** What: Janus sets no
limits and does no counting itself; counters live in the plugin's process. A
cost budget is enforced on the next request (in-flight overshoot is expected).
Exercise: run Janus with no plugin and with a plugin that does not use `Meter`.
Acceptance: the daemon exposes no metering endpoint or counter and never denies
a request on its own budget grounds; with a metering plugin, a call that crosses
a cost limit completes and the next call is denied.

---

## I. Request log

**D78 — One metadata-only record per data-plane request.** What: every request
that reached the data plane, including identity rejections, gets one record with
`request_id`, `received_at`, `duration_ms`, `upstream`, `route`, `method`,
`path`, `binding`, `caller_identity`, `claimed_identity` (rejected only),
`correlation_id`, `outcome`, `credential_ref`, `status`, `usage`, `error`. No
request or response bodies are captured. Exercise: send one forwarded, one
rejected, one denied request with distinctive body text;
`janus log show <request_id>` each. Acceptance: three records with the listed
fields; the distinctive body text appears in none.

**D79 — Outcome vocabulary.** What: `outcome` is one of `forwarded`,
`rejected_identity` (with `kind`), `denied` (with `reason`), `plugin_failed`
(with failure class), `credential_unavailable`, `upstream_error`, `abandoned`
(with `stage`). Exercise: produce the outcomes whose triggers this file fixes: a
successful call (`forwarded`), an identity rejection (D11), a plugin denial
(D31), a plugin failure (D32/D33). Then run a broad mix of traffic including
credential failures (D61) and upstream errors. Acceptance: the four fixed
triggers produce `forwarded`, `rejected_identity` + `kind`, `denied` + `reason`,
`plugin_failed` + class respectively, each found by
`janus log query --outcome <x>`; every record in the log, across all traffic,
has an `outcome` from the seven-value list, and any `abandoned` record carries a
`stage`. (The spec does not fix the triggers that distinguish `upstream_error`,
`abandoned` and `credential_unavailable`.)

**D80 — Log write is off the caller's critical path.** What: records are written
to the `requests` table after the response has been relayed. Exercise: make the
database slow/locked during a request. Acceptance: the caller's response is not
delayed by the log write; the record appears afterwards.

**D81 — `request_log.enabled` and `request_log.retain`.** What:
`request_log.enabled` (default `true`) and `request_log.retain` (duration,
default `"30d"`); a periodic job deletes older rows. With `enabled = false` no
records are written and `/v1/requests` endpoints return an empty result with a
`disabled` marker. Exercise: run with defaults; run with `retain = "1m"` and
wait; run with `enabled = false` and send traffic. Acceptance: default writes
records and `/v1/status` shows retention `30d`; old rows disappear after the
retain period; disabled mode writes none and `GET /v1/requests` returns empty
with `disabled` set.

---

## J. Admin listeners and access control

**D82 — Admin unix socket by default, mode 0600, no token.** What: the admin
surface binds `<data_dir>/admin.sock` with mode `0600` by default; filesystem
permission is the authentication; no token needed. Exercise: start with no admin
config; `stat` the socket;
`curl --unix-socket <data_dir>/admin.sock http://x/v1/status`; try as another OS
user. Acceptance: socket mode is `0600`; the curl succeeds with HTTP 200 and no
token; another user cannot open it.

**D83 — Data plane has no admin routes.** What: the data-plane router mounts
only proxy routes; an admin path on the data-plane port returns 404 regardless
of how listeners are bound. Exercise: `curl http://<data_plane.bind>/v1/status`,
`/v1/credentials`, `/v1/requests/stream`, with and without the admin token.
Acceptance: each returns HTTP 404 and no admin data.

**D84 — Startup refusals.** What: the daemon refuses to start (before the
data-plane listener binds) on: an identity binding naming a missing secret, an
unreachable JWKS endpoint, or an unregistered custom extractor; invalid
`[plugin]` configuration (D47–D49, D51, D58); a non-loopback `admin.bind`
without `admin.allow_non_loopback`; a non-loopback admin or UI bind without
`admin.tls = { cert, key }` (or the UI equivalent) unless
`admin.allow_plaintext = true`. Exercise: start once per bad config. Acceptance:
each start exits non-zero with a diagnostic and nothing listens on the
data-plane port.

**D85 — Admin TCP listener: opt-in, loopback by default, token always, TLS off
loopback.** What: `admin.bind` is opt-in. It must be loopback unless
`admin.allow_non_loopback` is set. Every request on it, loopback included,
requires the admin token as `Authorization: Bearer <token>`, compared in
constant time. A non-loopback bind also requires `admin.tls = { cert, key }`
unless `admin.allow_plaintext = true`. Exercise: set
`admin.bind = "127.0.0.1:7070"` and `curl /v1/status` with no token, a wrong
token, the correct token. Then bind `0.0.0.0:7070` with `allow_non_loopback` and
(a) no tls, (b) tls, (c) `allow_plaintext = true`; and `0.0.0.0:7070` without
`allow_non_loopback`. Acceptance: no/wrong token refused with a 4xx and JSON
error body `{ "error": { "code", "message" } }`, correct token 200; (a) and the
no-`allow_non_loopback` case refuse to start; (b) serves HTTPS requiring the
token; (c) serves plaintext requiring the token.

**D86 — Admin token generation and separation from caller identities.** What: on
first start the daemon generates an opaque random admin token into
`<data_dir>/admin-token` (mode `0600`). It is a separate secret with a separate
check path from caller identities: neither verifies as the other. Exercise:
first start; `stat` the file; present the admin token to the data plane as a
caller identity; present a valid caller identity as the admin bearer.
Acceptance: file exists, mode `0600`, reused across restarts; data plane rejects
the admin token with 401; the admin listener refuses the caller identity.

**D87 — UI listener.** What: `ui.bind` (loopback by default) is a third listener
serving the UI's static assets plus the same admin API; it does not exist when
the UI is not enabled; the admin socket stays available either way. Its API
requires the admin token on every call, loopback included; the daemon answers no
CORS preflight; a non-loopback UI bind carries the same TLS rule as a
non-loopback admin bind. At startup the daemon logs the UI URL and the token
file's path. Exercise: (a) enable the UI without setting a `ui.bind` address;
(b) `curl` the UI root; `curl <ui>/v1/status` without/with token; send an
`OPTIONS` preflight with `Origin` and
`Access-Control-Request-Headers: authorization`; (c) set a non-loopback
`ui.bind` with no TLS and no `allow_plaintext`; (d) disable the UI and probe the
port, and query the admin socket. Acceptance: (a) the UI listener reported in
`/v1/status` and in the logged UI URL is a loopback address (`127.0.0.1` or
`::1`); (b) assets served; API refused without token and 200 with it; the
preflight response carries no `Access-Control-Allow-*` headers; (c) refuses to
start; (d) the port refuses connections and the admin socket still answers; the
daemon log contains the UI URL and the admin-token path.

**D88 — Admin errors and mutation audit.** What: all admin errors are JSON
`{ "error": { "code": "<snake_case>", "message": "..." } }` with a conventional
HTTP status (`code` is the stable contract). Every mutating admin call is
recorded in the daemon log with the principal (`socket` or `token`) and the
resource touched. Exercise: `GET /v1/credentials/does-not-exist`; disable a
credential over the socket and over TCP. Acceptance: the 404-class error has the
JSON shape with a snake_case `code`; daemon log shows two mutation entries
naming the credential, with principals `socket` and `token`.

---

## K. Admin endpoints

**D89 — `GET /v1/status` and `GET /v1/plugin`.** What: status returns `version`,
`build`, `started_at`, bound listeners, each upstream with its identity binding
method, request log size and retention, and a `plugin` object with `mode`
(`none`|`linked`|`unix`|`tls`|`plaintext`), `endpoint`, `health`
(`unknown`|`ok`|`unreachable`|`timeout`|`tls_error`|`no_client_auth`|`rejected_client_cert`|`error`),
`last_error`, `last_ok_at`, and with TLS `client_cert_expires_at`,
`server_cert_expires_at`. `GET /v1/plugin` returns that `plugin` object alone.
Exercise: call both endpoints with no plugin and with a TLS plugin. Acceptance:
all listed fields present with the listed vocabularies; `/v1/plugin` body equals
`status.plugin`.

**D90 — Credentials list, show and default peek (metadata only).** What:
`GET /v1/credentials[?upstream=]`, `GET /v1/credentials/{ref}`,
`GET /v1/credentials/default?upstream=` return metadata only: `ref`, `upstream`,
`kind`, `label`, `source`, `status`, `rate_limited_until`, `expires_at`,
`last_used_at`, `request_count`, `error_count`, `last_error`. The default peek
returns what the default chain would pick right now without touching material.
Exercise: call each; filter by upstream. Acceptance: fields present, filter
applied, no material in any response; default peek matches the credential a
following no-plugin request uses.

**D91 — Add a credential live: `POST /v1/credentials`.** What: body `upstream`,
optional `ref` (generated if absent), optional `label`, and kind-specific
`material`; the daemon writes the pool slot and the credential is usable without
a restart; response is metadata only. Exercise: POST an OpenRouter API key
without `ref`; immediately route traffic to it. Acceptance: response contains a
generated `ref` and no material; the next request uses it without restart; a
pool slot directory exists for it.

**D92 — Rotate, enable/disable, refresh, delete a credential.** What:
`PUT /v1/credentials/{ref}/material` replaces material and clears `invalid`;
`POST .../disable` and `.../enable` take a credential out of / back into the
default chain and plugin resolution (a plugin naming a disabled ref gets a
failed request); `POST .../refresh` forces a refresh now;
`DELETE /v1/credentials/{ref}` removes the slot. Exercise: rotate an invalid
credential; disable then enable; refresh an OAuth credential; delete.
Acceptance: after rotate `status = "active"`; while disabled the default chain
skips it and a request whose plugin names it fails without being forwarded (no
fall-through to another credential); refresh updates `expires_at`; after delete
`GET` returns a 404-class JSON error and the slot directory is gone.

**D93 — Identities: create returns a daemon-generated token once.** What:
`POST /v1/identities` with `identity`, optional `claims`, `label`, `expires_in`
or `expires_at` returns entry metadata plus `token` (32 random bytes,
base64url-encoded, generated by the daemon — it never accepts a token from the
provisioner) exactly once; entries are addressed by server-assigned `id`, never
by token. Only the SHA-256 of the token is stored in `janus.db`. Exercise:
create; `GET /v1/identities/{id}`; inspect `janus.db` `identities` table; send a
create whose body also includes `"token": "guessable123"`. Acceptance: create
response has a `token` that base64url-decodes to 32 bytes; no later response
contains it; the table holds `token_hash` and not the token; for the create with
a supplied token, either the request is refused or the returned token is not
`guessable123`, and `guessable123` never verifies on the data plane.

**D94 — Identities: list, show, revoke, delete.** What:
`GET /v1/identities[?identity=&include_revoked=&include_expired=]` lists
(several entries may share one identity, for rotation);
`GET /v1/identities/{id}` returns `id`, `identity`, `claims`, `label`,
`created_at`, `expires_at`, `revoked_at`, `last_seen_at`;
`POST /v1/identities/{id}/revoke` is immediate and idempotent;
`DELETE /v1/identities/{id}` removes the entry. Exercise: create two entries for
one identity (both tokens work), revoke one twice, create a third with
`expires_in` of 1 s and wait; list with and without `include_revoked` and
`include_expired`; filter by `identity`; delete. Acceptance: both tokens verify
before revoke; revoke twice succeeds both times; the revoked entry is hidden by
default and shown with `include_revoked=true`; the expired entry is hidden by
default and shown with `include_expired=true`; the `identity` filter restricts
the list; the deleted entry 404s; `last_seen_at` advances (approximately, within
a flush interval) after use.

**D95 — Requests query and show.** What:
`GET /v1/requests?identity=&upstream=&outcome=&credential=&since=&until=&limit=&before=`
returns newest first, cursor-paginated (`before` = previous page's `next`),
`limit` default 100, cap 1000; `GET /v1/requests/{request_id}` returns one
record. Exercise: generate >1000 records; page through; request `limit=5000`;
filter by each field. Acceptance: default page has 100 items newest-first;
`limit=5000` returns at most 1000; following `next` yields the next older page
with no duplicates; filters restrict correctly.

**D96 — Requests live tail over SSE.** What:
`GET /v1/requests/stream?identity=&upstream=&outcome=` is `text/event-stream`:
one `request` event per completed matching record plus a periodic `ping`. The
token goes in the `Authorization` header, never the query string. Exercise:
`curl -N -H "Authorization: Bearer $T" <admin>/v1/requests/stream?upstream=openai`
while sending traffic to two upstreams; try `?token=...` without the header.
Acceptance: `Content-Type: text/event-stream`; only openai records arrive as
`request` events; `ping` events arrive periodically; the query-string token is
refused.

---

## L. The `janus` CLI

**D97 — CLI connection and token resolution.** What: `janus` connects to
`--admin <address>` or `JANUS_ADMIN` (`unix://<path>` or `http(s)://host:port`),
defaulting to `<data_dir>/admin.sock` resolved the same way the daemon resolves
`data_dir`. For TCP, the token comes from `--token-file <path>`,
`JANUS_ADMIN_TOKEN`, or (on the daemon host) `<data_dir>/admin-token`; there is
no flag taking the token as an argument. The CLI has no proxy logic of its own.
Exercise: run `janus status` with no flags on the daemon host; with
`--admin http://127.0.0.1:7070` and each token source; look for a
`--token <value>` option in `janus --help`. Acceptance: all succeed; no
token-as-argument option exists; with no reachable daemon the CLI fails with a
non-zero exit rather than doing anything locally.

**D98 — `janus status` and `--json` on every command.** What: every subcommand
maps to one or two admin calls and takes `--json` for machine-readable output;
`janus status` prints the `/v1/status` document. Exercise: `janus status`,
`janus status --json`; run every command in D99–D102 with `--json`. Acceptance:
status shows version, listeners, upstreams with binding methods, log
size/retention, plugin mode/health; every `--json` invocation prints valid JSON
and exits 0 on success; failures exit non-zero.

**D99 — `janus credential` commands; secrets never on argv.** What:
`credential list [--upstream]`, `show <ref>`, `default --upstream <name>`,
`add --upstream <name> [--ref] [--label] (--api-key-stdin | --from-file <path>)`,
`rotate <ref> (--api-key-stdin | --from-file <path>)`,
`enable|disable|refresh|rm <ref>`. Material is read from stdin or a file (e.g. a
vendor CLI credential file); no flag accepts a key as an argument. Exercise:
`echo $KEY | janus credential add --upstream openrouter --api-key-stdin`;
`janus credential add --upstream anthropic --from-file ~/.claude/.credentials.json`;
rotate; enable/disable/refresh/rm; inspect `ps` during the add. Acceptance: each
command performs its admin call; the key never appears in argv/`ps` or in
output; `janus credential add --help` lists no key-value flag.

**D100 — `janus identity` commands.** What:
`identity create --identity <s> [--claim k=v]... [--label] [--expires-in <dur>]`
prints the token once;
`identity list [--identity] [--include-revoked] [--include-expired]`;
`identity show|revoke|rm <id>`. Exercise: run each, including `list` with and
without `--include-revoked` and `--include-expired` after revoking one entry and
letting another expire. Acceptance: create prints a token; `show`/`list` never
print it; revoked and expired entries appear only with their respective flag;
revoke takes effect on the next data-plane request; rm removes the entry.

**D101 — `janus log` commands.** What:
`log query [--identity] [--upstream] [--outcome] [--credential] [--since] [--until] [--limit N]`,
`log tail [--identity] [--upstream] [--outcome]` (live, via the SSE stream),
`log show <request_id>`. Exercise: run query with filters; run tail while
sending traffic; show one record. Acceptance: query returns filtered records;
tail prints new matching records as they complete until interrupted; show prints
one full record.

**D102 — `janus plugin` commands.** What: `plugin status` (the plugin object),
`plugin check` (active channel check, D56), `plugin tls init` (D55). Exercise:
run each against a configured TLS plugin. Acceptance: status shows
mode/health/expiry; check prints each probe result; init writes the files.

**D103 — `janus-daemon check-config`.** What:
`janus-daemon check-config --config <path>` runs the full startup validation
(identity bindings, upstreams, plugin config, listeners) and exits without
binding any listener. Exercise: run on a valid config and on a config with a
binding naming a missing secret, while another process holds the data-plane
port. Acceptance: valid config exits 0; invalid exits non-zero with a
diagnostic; in neither case is any port bound or socket created.

---

## M. The UI

**D104 — UI token prompt and session storage.** What: the UI prompts for the
admin token once, keeps it in session storage, and sends it in an
`Authorization` header on every API call. Exercise: open the UI URL from the
daemon log; enter the token; reload; inspect the page's storage and network
requests in browser devtools. Acceptance: the prompt appears on first visit;
reload does not prompt again; the token is present in `sessionStorage` (not in
`localStorage`, cookies, or any URL); every API request carries `Authorization`.

**D105 — UI Overview view.** What: renders `/v1/status` — listeners, upstreams
and their bindings, plugin mode and health with certificate expiry — plus
credential health at a glance and request and denial rates from the log.
Exercise: open Overview with traffic flowing (including some denials) and a TLS
plugin. Acceptance: listeners, upstreams/bindings, plugin mode/health and
certificate expiry match `janus status --json`; the credential health shown
matches the `status` values in `janus credential list --json`; request and
denial rates are shown and are consistent with `janus log query` (counts of all
records and of `--outcome denied`) over the same period.

**D106 — UI Credentials view.** What: credential list with status and counters;
actions enable, disable, refresh, remove. No add or rotate (material entry stays
in the CLI/API). Exercise: open Credentials; perform each action. Acceptance:
each action changes state as confirmed by `janus credential show`; no form for
entering material exists; no material displayed.

**D107 — UI Identities view.** What: registry list with filters; action revoke.
No identity creation. Exercise: filter by identity and include-revoked; revoke
an entry. Acceptance: filters work; revoke makes the token fail on the next
data-plane request; no create control and no token displayed.

**D108 — UI Requests view with live tail and detail drawer.** What: a filterable
table over `/v1/requests`, a live-tail toggle on `/v1/requests/stream` (read
with `fetch` and a streaming body so the token stays in a header), and a detail
drawer for one record. Exercise: filter; toggle live tail while sending traffic;
open a record. Acceptance: table matches `janus log query`; new requests appear
live without reload while the toggle is on and stop when off; the stream request
carries the token in `Authorization`, not the URL; drawer shows the record's
fields.

**D109 — UI Plugin view.** What: shows the channel's health and a button that
runs `/v1/plugin/check`. Exercise: open Plugin; press the check button with a
healthy and a misconfigured plugin. Acceptance: health matches `/v1/plugin`;
check results for each probe are displayed and match `janus plugin check`.

---

## N. Daemon storage

**D110 — One SQLite database with two tables; credentials kept separate.** What:
`<data_dir>/janus.db` holds `identities` (`id`, `token_hash`, `identity`,
`claims` JSON, `label`, `created_at`, `expires_at`, `revoked_at`,
`last_seen_at`) and `requests` (indexed on `received_at`,
`(caller_identity, received_at)`, `request_id`). The registry is loaded into
memory at startup and written through on mutation (no disk read on the hot path;
revocation visible to the next request); `last_seen_at` is flushed periodically.
The credential pool is not in this database. Exercise: inspect `janus.db` with
`sqlite3` after creating identities and sending traffic; restart the daemon and
reuse an opaque token. Acceptance: two tables with the listed columns and
indexes; no credential material and no raw identity token in the database;
opaque tokens still verify after restart.
