---
title: Architecture
type: reference
status: draft
tags: [janus, architecture]
---

# Architecture

Janus is a Cargo workspace of three crates plus a standalone web UI. This
document describes what each piece is responsible for and the path a single
request takes through them. For what the system is *for* — callers, identities,
and why the substitution exists at all — see
<ref target="docs/credential-substitution">credential substitution and policy
enforcement</ref>. See <ref target="docs/status#build-status">status</ref> for
how much of this is actually implemented today (as of 2026-10-21: scaffolding
only).

## Crate split {#crates}

```
janus/
├── crates/
│   ├── janus-core/      # identity extraction, credential pool, substitution,
│   │                    # forwarding, metering primitives
│   ├── janus-daemon/    # library + binary; data plane, admin surface,
│   │                    # plugin client. Linked into custom builds.
│   └── janus-cli/       # admin client binary ("janus")
├── proto/
│   └── janus.proto      # the plugin contract
└── ui/                  # standalone web UI
```

### janus-core {#janus-core}

The library crate holding everything that is true of the proxy regardless of how
it is deployed. It is linked by `janus-daemon`, by tooling, and by plugin
implementations that want its metering primitives; it owns no listener and no
process lifecycle of its own. Its responsibilities:

- **Caller identity extraction.** The stage that turns the material a caller
  presented into a verified identity: the `IdentityExtractor` trait, the four
  shipped verification methods (including the HMAC primitives carried over from
  the existing gateway code — see <ref target="docs/scope#extractable">what
  carries over</ref>), and the config-driven bindings that select one per
  request type. Fully designed in <ref target="docs/caller-identity">caller
  identity extraction</ref>.
- **The credential pool.** Storage and lookup of real upstream credentials,
  detection of what kind each one is, and refreshing the ones that expire. This
  is the only component with read access to credential material; nothing above
  it hands a credential back toward a caller.
- **The substitution itself.** Turning an inbound request that carries a caller
  identity into an outbound request that carries a real credential: constructing
  the outbound request from a per-upstream forwarding allowlist so the caller's
  identity material is absent rather than deleted (see
  <ref target="docs/caller-identity#scrub">the presented material never goes
  upstream</ref>), applying the upstream's own auth scheme, and rewriting the
  target.
- **Forwarding.** Executing the upstream call and relaying the response,
  including streaming responses, without buffering the whole body where the
  upstream streams it.
- **The request log.** The per-request record that makes a call attributable to
  the identity that made it.
- **Metering primitives.** Per-identity counters and quota helpers that a policy
  plugin can link rather than reimplement. Janus ships the counting code and
  does none of the counting itself — see
  <ref target="docs/plugin-contract#metering-primitives">metering
  primitives</ref>.

Each supported upstream contributes only the narrow, upstream-specific pieces
listed in <ref target="docs/credential-substitution#general-purpose">not
specific to LLM providers</ref>; the rest of the crate is upstream-agnostic.

### janus-daemon {#janus-daemon}

The long-running process, and a library. Listeners, configuration, and the
plugin connection live here. The library/binary split is what makes a linked
plugin possible: the stock binary is a few lines over `janus_daemon::run`, and a
custom build is another binary calling the same entry point with its own
implementations passed in — see
<ref target="docs/plugin-contract#custom-build">what a custom build looks
like</ref>.

It exposes three distinct surfaces:

- **The data plane.** The proxy endpoint callers point at. This is the hot path
  described in <ref target="docs/architecture#request-lifecycle">the request
  lifecycle</ref>.
- **The admin surface.** The control channel used by
  <ref target="docs/architecture#janus-cli">janus-cli</ref> and
  <ref target="docs/architecture#ui">the UI</ref> for credential management,
  pool health, and log inspection — see
  <ref target="docs/architecture#admin-surface">the admin surface</ref> below
  for how it is kept out of a caller's reach.
- **The plugin client.** `janus-daemon` is the client of the contract in
  <ref target="docs/plugin-contract">the plugin contract</ref>; the plugin
  implements the server side. The daemon calls out to policy, never the other
  way around, and does so through one internal interface regardless of which
  execution model is in use — see
  <ref target="docs/plugin-contract#plugin-client">the daemon codes against one
  client interface</ref>.

With no plugin endpoint configured, `janus-daemon` makes zero calls defined in
`proto/janus.proto` and behaves as a plain credential-substituting pass-through
proxy. This is an explicit design requirement (see
<ref target="docs/index#goals">standalone by default</ref>), not an accident of
the current stub state. How a plugin endpoint gets discovered and authenticated
is not yet decided — see <ref target="docs/status#open-questions">open
questions</ref>.

### janus-cli {#janus-cli}

The admin client binary, named `janus`. It speaks to the daemon's admin surface
and contains no proxy logic of its own — it cannot resolve a credential or
forward a request, only ask the daemon to. Intended scope: credential
management, pool health inspection, and request-log tailing. By default it
connects over the admin unix socket, which means an operator with filesystem
access to the daemon's data directory needs no token and no configuration. The
full command surface is not yet decided — see
<ref target="docs/status#open-questions">open questions</ref>.

### ui {#ui}

A standalone web application, served by Janus on its own port and consuming the
same admin surface as `janus-cli`. It is operator-facing: it exists to make the
daemon's state — which credentials are in the pool, what their health is, what
traffic has flowed through — legible without reading logs by hand.

Architecturally it is a peer of the CLI, not a component of any application that
happens to be calling Janus: it has its own port, its own lifecycle, and it
holds no privileged path to credentials beyond what the admin surface exposes.
Anything an operator can do in the UI is something the admin surface can do, and
therefore something the CLI can do too. Feature scope beyond the current
placeholder page is not yet decided — see
<ref target="docs/status#open-questions">open questions</ref>.

## The admin surface is a separate listener {#admin-surface}

The admin surface manages credentials. The data plane serves sandboxed callers
whose entire reason for existing is that they must not reach those credentials.
Those two cannot share an address.

So they don't share a listener:

- **The data plane** binds a TCP address (`data_plane.bind`) — the one a caller
  is pointed at. Its router has only proxy routes mounted on it.
- **The admin surface** binds a **unix domain socket** by default, at
  `<data_dir>/admin.sock`, mode `0600`. On that socket, filesystem permissions
  *are* the authentication: a process that can open the socket is already
  running as the daemon's user, and a caller holding nothing but a TCP route to
  the data plane has no way to address it at all. This is the default because it
  needs no token, no TLS, and no configuration to be correct.
- **An admin TCP listener** (`admin.bind`) is opt-in, for the cases the socket
  cannot serve: a CLI or UI in a different container, or on a different host.
  When set it must be a loopback address unless `admin.allow_non_loopback` is
  explicitly set, and a non-loopback admin bind additionally requires an admin
  token to be configured — the daemon refuses to start rather than quietly
  exposing credential management. The token is an opaque random value generated
  into `<data_dir>/admin-token` (mode `0600`) on first start and presented as a
  bearer token, compared in constant time. It is deliberately *not* the same
  primitive as a caller identity: identities are minted, scoped and expiring,
  and an operator credential is none of those. Separate secret, separate check
  path, no possibility of one verifying as the other.
- **The UI listener** (`ui.bind`, loopback by default) serves the UI's static
  assets, and — because a browser cannot speak a unix socket — the same admin
  API alongside them. It is a third listener, distinct from the data plane; the
  admin socket stays available either way. Not enabling the UI means that
  listener does not exist.

The admin surface speaks HTTP and JSON on whichever listener it is reached over.
A browser has to be able to talk to it, and the CLI is trivial either way. gRPC
is reserved for the <ref target="docs/plugin-contract">plugin contract</ref>,
where a per-request hot path justifies it.

### Why separate listeners rather than separate paths {#listener-rationale}

Serving admin routes under a reserved path prefix on the data-plane listener
would be less configuration, and it is the wrong trade here for two reasons.

First, there is no prefix available to reserve. The data plane is deliberately a
transparent proxy of each upstream's own API shape (see
<ref target="docs/credential-substitution#substitution">the substitution</ref>),
and Janus rewrites request paths on the way out. Any path can legitimately
appear on the data plane, so "admin lives under `/admin`" makes `/admin` a path
an upstream may not use, and makes every future path-rewriting change a
potential credential-management exposure.

Second, it changes what the boundary rests on. With one listener, the isolation
is a string comparison the routing code must get right on every request and
every refactor. With two listeners, it is a socket the caller cannot address —
and on the default unix-socket configuration, cannot even name. The data-plane
router additionally has no admin routes mounted, so an admin path arriving on
the data-plane port is a 404 regardless of what either listener is bound to.
That is defence in depth on top of the address separation, not the mechanism.

## Request lifecycle {#request-lifecycle}

The path a single proxied call takes, end to end. Steps 2 and 3 are where a
decision actually gets made; everything else is mechanical.

1. **A caller issues a request.** The agent calls the upstream's own API, with
   the base URL pointed at `janus-daemon`'s data plane and its own caller
   identity presented instead of a real upstream credential. From the caller's
   side this is an ordinary upstream call.
2. **Janus extracts and verifies the caller identity.** *(Decision: Janus
   alone.)* The extractor bound to this request type reads the identity material
   from wherever that binding says it sits and verifies it locally. If it does
   not verify, the request is rejected immediately — no plugin is consulted, no
   credential is touched, the body is never buffered, nothing is forwarded. This
   happens before policy because policy needs a trustworthy identity to reason
   about. See <ref target="docs/caller-identity">caller identity
   extraction</ref>.
3. **Janus resolves which credential to use.** *(Decision: the plugin, or the
   default chain.)* If a plugin is configured, `janus-daemon` calls
   <ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref> with the
   verified caller identity and the intended upstream, and gets back one of
   three answers: use this specific credential, defer to the daemon's default
   chain, or deny with a reason. A denial ends the request here. If no plugin is
   configured, the default chain decides and this round trip does not happen at
   all.
4. **The credential is fetched from the pool.** The pool resolves the selected
   reference to live credential material, refreshing it first if it has expired.
   A credential that cannot be made usable is a failure of the request, not a
   silent fall-through to a different credential.
5. **The request is rewritten.** The outbound request is constructed from the
   upstream's forwarding allowlist, so the caller's identity material is not
   carried over; the real credential's auth is applied in the form that upstream
   expects, and the target is rewritten to the real upstream. This is the
   substitution; see <ref target="docs/credential-substitution#substitution">the
   substitution</ref>.
6. **The call is forwarded and the response relayed unmodified.** The rewritten
   request goes upstream and the response comes back to the caller as the
   upstream produced it, streaming if the upstream streams. Janus does not
   transform responses — see
   <ref target="docs/architecture#proxy-behaviors">proxy behaviors</ref>.
7. **Settlement and observation are emitted.** Outcomes a plugin may care about
   — how the call ended, a credential the upstream rejected — go out over
   <ref target="docs/plugin-contract#notify">Notify</ref>, which is
   fire-and-forget and off the critical path. Exactly one of `call_completed` or
   `call_abandoned` is emitted for every request that reached step 3, so a
   plugin that charged a budget on the way in always learns how the call
   actually ended. The caller's response does not wait on any of it.

Steps 2 through 5 are entirely invisible to the caller when they succeed. That
is the point: the caller's view of a successful call is identical to calling the
upstream directly with a credential it does not have.

## Proxy behaviors {#proxy-behaviors}

Janus does one of two things with a call. Both relay the response unmodified
(step 6 of <ref target="docs/architecture#request-lifecycle">the request
lifecycle</ref>); they differ only in whether the call is reported outward
afterwards.

- **Pass through.** The default and the baseline. Nothing about the call leaves
  the daemon beyond its own request log. This is the lowest-latency path and the
  only one available with no plugin configured.
- **Pass through and observe.** The call is additionally reported outward as an
  event — step 7 of the lifecycle — on top of the request log the daemon keeps
  either way. Observation is strictly off the critical path: it happens after
  the response has been relayed, it must not alter what the caller receives, and
  it must not be able to stall or fail the response. This is the path that makes
  usage attribution and auditing possible without changing behavior.

In both, the only mutation Janus performs is on the **request**, and it is the
credential substitution itself — strip what the caller presented, apply what the
upstream expects, rewrite the target. That is not one option among several; it
is the mechanism the whole system exists to provide.

### Out of scope: response transformation {#no-response-transforms}

Janus does not transform responses, and does not define an API for doing so.
This is a scope boundary, not an unanswered question.

The reasoning is that there is nothing concrete to design against. Both launch
upstreams are HTTP APIs that stream, and for both of them every behavior Janus
needs — substitute the credential, decide whether the call is allowed, report
what it cost — is satisfied on the request side plus
<ref target="docs/plugin-contract#notify">Notify</ref>. No second use case
exists yet that a transform API would serve, and a transform API invented
without one would be the worst of both: speculative surface area on the hot
path, and still probably the wrong shape when a real requirement arrives.

Specifically out of scope until a concrete use case demands it:

- **Response-body rewriting**, in any form — arbitrary, constrained, or
  schema-aware per upstream.
- **Streaming-aware transforms.** Relaying a stream as it arrives and
  transforming a body that must be whole are in direct tension, and resolving
  that tension is work with no current payer.
- **Request augmentation beyond the substitution.** A plugin cannot inject
  headers or edit a caller's body. `ResolveRoute` answers *which credential*; it
  does not get to reshape the call.
- **Non-HTTP transports.** gRPC passthrough and other protocols are not
  supported. Both launch upstreams are HTTP.

This is what the <ref target="docs/plugin-contract">plugin contract</ref> is
for. Each of these can be added later as a new extension point without
redesigning anything above, which is precisely why it is safe to leave them out
now instead of guessing at them.

## What's real vs. scaffolded {#reality-check}

`proto/janus.proto` is a working design — see
<ref target="docs/plugin-contract">the plugin contract</ref> for the two RPCs it
defines and the reasoning behind their shapes. Everything else described above
(the crates, the lifecycle, the UI) is design plus scaffolding: each binary
currently just prints a "not yet implemented" message, `janus-core` has no
logic, and the UI is a placeholder page. No proxying happens yet.
