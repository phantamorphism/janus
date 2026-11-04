---
title: Decision records
type: decision
status: accepted
tags: [janus, decisions]
---

# Decision records

This document records the main design choices in Janus and *why* they were made:
the decision, its rationale, and for the contested ones, the options that were
considered and why each was kept or set aside. The designs themselves live in
the design docs; it exists so a later reader can tell a settled choice from an
accident. What is built is in <ref target="docs/status">status</ref>.

## How decisions are made {#method}

Every question is judged against the same two things: the project goal in
<ref target="docs/index#goals">goals</ref>, and what the two users of Janus
would expect — the **end user** (the operator deploying the gateway, and the
callers whose traffic crosses it) and the **plugin author** (someone
implementing the <ref target="docs/plugin-contract">plugin contract</ref> to
integrate their own policy). The criterion is correctness — well fitted to the
goal and the users — over ease of implementation, with the counterweight that
nothing is added without a user need behind it.

Where one answer is clearly right, it is taken and justified. Where credible
alternatives exist, each is developed into a full proposal (mechanism,
configuration, failure modes, operator and author experience, prior art, and its
own weaknesses) and weighed against the goal and the users; a hybrid is taken
when it is more correct than any one alternative.

## Plugin endpoint discovery {#plugin-discovery}

**Decision.** The daemon's configuration file is the only source of plugin
settings: a `[plugin]` block with exactly one of `endpoint` or `linked = true`,
where the `endpoint` URI scheme selects the transport. No environment-variable
or command-line source for individual plugin settings. Design:
<ref target="docs/plugin-contract#plugin-config">configuring the plugin</ref>.

**Clear-cut.** Everything else the daemon needs — upstreams, identity bindings,
listeners — is already a config-file concern validated at startup, and the
contract already requires that config and linkage agree. A second source would
only add a way for them to disagree. Containers that want to inject the endpoint
template the file, which they must already do for the rest of it.

## Daemon-to-plugin channel security {#plugin-channel-auth}

**Decision.** Transport-tiered, certificate-based. The unix-socket transport
carries no application-layer authentication: filesystem permissions are the
boundary, as for the admin socket, with the daemon refusing a socket path whose
ancestry is writable by others. The network transport requires mutual TLS: the
daemon verifies the plugin against an operator-named trust anchor (a private CA
or a pinned certificate — never system roots) and presents a client certificate;
the daemon additionally refuses any plugin that does not request one. Plaintext
is reachable only through an explicit `allow_plaintext = true` for deployments
whose infrastructure (a service mesh) authenticates both ends.
`janus plugin tls init` bootstraps the certificates for an operator with no PKI.
There is no shared-secret or bearer mode. Design:
<ref target="docs/plugin-contract#channel-security">securing the channel</ref>.

**Contested.** Three alternatives were weighed:

- **Mutual TLS, transport-tiered** — the design adopted.
- **Bearer token in gRPC metadata plus server-only TLS** — a static secret from
  a file, checked by the plugin in constant time; TLS with server verification
  required off-loopback; plaintext loopback TCP permitted.
- **Infrastructure-delegated** — server-verified TLS only; authenticating the
  daemon to the plugin left to a mesh, network policy, or an optional client
  certificate, with no requirement that it happen.

The infrastructure-delegated alternative ranks last: it leaves the plugin's
`Notify` stream and `ResolveRoute` endpoint open to forgery unless the operator
opts in, and the metering primitives in `janus-core` make budget enforcement
depend on exactly those messages. A design whose safety rests on documentation
rather than enforcement fails goal 2. The other two are close, and each has a
case for a hybrid. The points they share decide it:

- Both directions must be authenticated, and Janus must enforce as much of it as
  it can. Mutual TLS is the one mechanism where the daemon can *detect* a plugin
  that skipped the check — a server that does not request a client certificate
  is visible in the handshake — whereas a bearer check that a plugin author
  forgot is invisible to the daemon forever.
- For a plugin author in any language, mutual TLS is configuration in their gRPC
  stack, not Janus-specific code. A bearer is per-language interceptor code,
  shipped by Janus only for Rust.
- One mechanism, not a choice of two. Letting operators pick bearer *or*
  client-cert would mean a plugin written against one does not work with a
  daemon configured for the other, splitting the ecosystem the contract exists
  to unify.
- A plaintext loopback mode, which the bearer proposal relied on for containers,
  hands the bearer to whichever local process bound the port while the plugin
  was down. The same-host answer is the socket.

The two costs charged against mutual TLS are each answered with a specific
piece: the PKI burden on a small operator by `janus plugin tls init` (following
the precedent of Janus generating its own admin token — a channel credential,
not a caller identity), and the mesh operator's double encryption by the
explicit `allow_plaintext` opt-in, modelled on `admin.allow_non_loopback` and on
the `trusted_header` method: correct in exactly one topology, named loudly in
config, never reached by default.

## The metering store {#metering-store}

**Decision.** The `UsageStore` trait is redesigned around one atomic,
idempotent, ledgered operation, because the documented `add`/`get`/`sub` shape
could not deliver the atomic check-and-increment `charge` promises on any store
other than a single in-process mutex. `janus-core` ships two stores: the
in-memory one, and a durable embedded SQLite store behind a `sqlite` feature, so
that a `Window::Total` budget survives a plugin restart without an external
service. `Meter` refuses to start with a `Total` quota on a volatile store
unless the author opts in. A replicated backend is not shipped; the trait, a
conformance test suite, and a documented Redis sketch make one writable by the
first plugin that needs it. Design:
<ref target="docs/plugin-contract#metering-primitives">metering
primitives</ref>.

**Contested.** Three alternatives were weighed:

- **Memory only; the trait is the deliverable** — fix the trait, ship no second
  backend, let each author implement durability or replication over their own
  store.
- **Memory plus an embedded SQLite store** — the design adopted, with
  refinements from the other two.
- **Memory plus a Redis store** — one shared backend covering both restart
  durability and replicas.

SQLite ranks first and Redis last. The deciding argument is that the settled
design already contains a durability requirement: `Total` is specified as *never
resets*, and goal 2 names credit checks. A library whose only shipped store
resets every `Total` budget on deploy ships a type that is untrue, so "nothing
currently needs it" was wrong for single-process durability — the design's own
semantics need it. Redis answers that need only by asking a single-host operator
to run and correctly configure a stateful service, against the
standalone-by-default goal; memory-only answers it by handing every author the
hardest part (atomicity) that the primitives exist to remove. The original
two-phase design also had two defects — a phase-2 cost sent through `charge` is
dropped when it crosses the limit, and a refund computed from the current time
lands in the wrong window after a period boundary — which the redesigned trait
fixes with an unconditional `record` and a store-held ledger keyed by
`request_id`.

The adopted design is the SQLite alternative's trait and store with three things
taken from the others: the store's self-declared `durability()` and the startup
refusal of `Total` on a volatile store; a non-atomic `check` for cost meters
whose figure is only known after the call; and batches scoped to one
`(identity, meter)` so a store can keep a batch in one partition or slot.

## Policy on an identity-less route {#identity-less-routes}

**Decision.** No. A route bound `method = "none"` is open: `ResolveRoute` is not
called, the default chain decides, and there is no config-level allow/deny for
such a route. The operator's controls are declaring the route or not, and the
default chain. Design: <ref target="docs/caller-identity#binding">binding an
extractor to a request type</ref>.

**Clear-cut.** Janus has one policy mechanism, the plugin, and it is keyed on a
verified identity by construction. A per-route rule set inside Janus would be a
second policy engine — hardcoded rules, which goal 4 rules out — and nothing
asks for it: the only stated use of `none` is cheap unauthenticated metadata
endpoints. The cost is stated rather than hidden: an open route is the one place
goal 2 — every call a policy enforcement point — does not hold, and it does not
hold only because the operator wrote `method = "none"` for that path. What *was*
under-specified is observability, and that is now stated: such calls appear in
the request log with an empty identity, the credential- centric `Notify` events
still fire, and the two settlement events do not, because there was no decision
to settle.

## The opaque-identity registry {#opaque-registry}

**Decision.** A daemon-owned table in `<data_dir>/janus.db`, storing the SHA-256
of a daemon-generated random token alongside the `identity` string, claims,
label, expiry and revocation time, loaded into memory for lookup. Entries are
created, listed, revoked and deleted through the admin surface; the token is
returned once at creation. Design:
<ref target="docs/caller-identity#registry">the opaque-identity registry</ref>
and <ref target="docs/admin-surface#identities-endpoint">identities</ref>.

**Clear-cut**, with one framing point worth recording. Janus does not mint
identities; with `opaque`, the provisioner's issuance step is a call to the
admin surface instead of a signing operation with a shared secret, and the
provisioner still decides who the identity is, what it claims, and how long it
lives. The random string Janus generates is a lookup key, not an identity, and
Janus generates it (rather than accepting one) so that no deployment can
register a guessable token. Hashing it at rest means the registry is not a
credential store if the database leaks.

## Admin surface, CLI and UI {#admin-surface}

**Decision.** One HTTP+JSON API under `/v1` on the admin listener — status,
credentials, identities, requests, plugin — with `janus` as a thin client of it
and the UI as a deliberate subset. The surface never returns credential
material. The request log is metadata-only, stored in SQLite with an on/off
setting and a retention setting, queryable and tailable over SSE. Every TCP
admin and UI listener requires the admin token, loopback included, and a
non-loopback one requires TLS unless plaintext is explicitly allowed. The UI
omits the two operations that accept credential material and identity creation.
Design: <ref target="docs/admin-surface">admin surface, CLI and UI</ref>.

**Clear-cut.** The shape follows from decisions already made (HTTP+JSON, a
separate listener, the socket as default) and from the users: an operator needs
to see pool health and traffic and to act on both; a host embedding Janus needs
the peek and the log; the opaque method needs its registry operations. Three
scope lines within it were judgement calls rather than consequences. They were
taken on the reasoning below without the multi-alternative comparison the
contested questions had, because each leaves the smaller surface and can be
widened later without changing anything already built:

- The UI leaves material entry to the CLI, because material comes from a file on
  the operator's host and a browser form would be a second path for the most
  sensitive input with nothing asking for it. It leaves identity creation to the
  provisioner, a program acting at sandbox creation rather than an operator.
- The request log does not capture bodies, because Janus does not inspect
  responses beyond the `usage` observation, and body capture is a
  privacy-sensitive feature that would need its own design if a user asks.
- The request log is on by default and can be turned off with
  `request_log.enabled = false`, because attribution of a call to the identity
  that made it is the purpose of the proxy but not every operator wants a record
  kept.

## Embedded callers {#embedded-callers}

**Decision.** Served by the admin surface: credential inspection becomes
`GET /v1/credentials` and `GET /v1/credentials/default`, log viewing becomes
`GET /v1/requests` and its stream, reached over the admin socket on the same
host or the TLS admin listener from another. One further embedded use — a host's
own tap on upstream response bytes — stays with the host, because it is response
inspection, which Janus does not do. Recorded in
<ref target="docs/scope#inprocess-gap">scope: embedded callers</ref>.

**Clear-cut.** These are consumers of endpoints the surface needs for its own
users anyway; the admin surface provides them.
