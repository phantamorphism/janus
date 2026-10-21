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
│   ├── janus-core/      # identity verification, credential pool, substitution, forwarding
│   ├── janus-daemon/    # long-running binary; data plane + admin surface + plugin client
│   └── janus-cli/       # admin client binary ("janus")
├── proto/
│   └── janus.proto      # the plugin contract
└── ui/                  # standalone web UI
```

### janus-core {#janus-core}

The library crate holding everything that is true of the proxy regardless of how
it is deployed. It is linked by `janus-daemon` and by tooling; it owns no
listener and no process lifecycle of its own. Its responsibilities:

- **Caller identity verification.** Minting and verifying the HMAC-signed tokens
  that represent a caller identity, so the daemon can establish who is asking
  before anything else happens.
- **The credential pool.** Storage and lookup of real upstream credentials,
  detection of what kind each one is, and refreshing the ones that expire. This
  is the only component with read access to credential material; nothing above
  it hands a credential back toward a caller.
- **The substitution itself.** Turning an inbound request that carries a caller
  identity into an outbound request that carries a real credential: removing the
  caller's auth material, applying the upstream's own auth scheme, and rewriting
  the target.
- **Forwarding.** Executing the upstream call and relaying the response,
  including streaming responses, without buffering the whole body where the
  upstream streams it.
- **The request log.** The per-request record that makes a call attributable to
  the identity that made it.

Each supported upstream contributes only the narrow, upstream-specific pieces
listed in <ref target="docs/credential-substitution#general-purpose">not
specific to LLM providers</ref>; the rest of the crate is upstream-agnostic.

### janus-daemon {#janus-daemon}

The long-running process. It is where listeners, configuration, and the plugin
connection live. It exposes three distinct surfaces:

- **The data plane.** The proxy endpoint callers point at. This is the hot path
  described in <ref target="docs/architecture#request-lifecycle">the request
  lifecycle</ref>.
- **The admin surface.** The control channel used by
  <ref target="docs/architecture#janus-cli">janus-cli</ref> and
  <ref target="docs/architecture#ui">the UI</ref> for credential management,
  pool health, and log inspection. It is separate from the data plane because it
  is operator-facing, not caller-facing, and must not be reachable by the
  sandboxed callers the data plane serves.
- **The plugin client.** `janus-daemon` is the gRPC *client* of the contract in
  <ref target="docs/plugin-contract">the plugin contract</ref>; the plugin is
  the server. The daemon calls out to policy, never the other way around.

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
management, pool health inspection, and request-log tailing. The full command
surface is not yet decided — see <ref target="docs/status#open-questions">open
questions</ref>.

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

## Request lifecycle {#request-lifecycle}

The path a single proxied call takes, end to end. Steps 2, 3 and 7 are the three
points where a decision actually gets made; everything else is mechanical.

1. **A caller issues a request.** The agent calls the upstream's own API, with
   the base URL pointed at `janus-daemon`'s data plane and its own caller
   identity in place of upstream auth. From the caller's side this is an
   ordinary upstream call.
2. **Janus verifies the identity.** *(Decision: Janus alone.)* The presented
   identity is verified locally. If it does not verify, the request is rejected
   immediately — no plugin is consulted, no credential is touched, nothing is
   forwarded. This happens before policy because policy needs a trustworthy
   identity to reason about.
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
5. **The request is rewritten.** The caller's identity material is stripped, the
   real credential's auth is applied in the form that upstream expects, and the
   target is rewritten to the real upstream. This is the substitution; see
   <ref target="docs/credential-substitution#substitution">the
   substitution</ref>.
6. **The call is forwarded.** The rewritten request goes upstream and the
   response is relayed back toward the caller, streaming if the upstream
   streams.
7. **The response is handled.** *(Decision: configured behavior.)* One of the
   three behaviors in <ref target="docs/architecture#proxy-behaviors">proxy
   behaviors</ref> applies.
8. **Anything worth telling anyone about is emitted.** Outcomes a plugin may
   care about — a credential rejected by the upstream, an upstream error — go
   out over <ref target="docs/plugin-contract#notify">Notify</ref>, which is
   fire-and-forget and off the critical path. The caller's response does not
   wait on it.

Steps 2 through 5 are entirely invisible to the caller when they succeed. That
is the point: the caller's view of a successful call is identical to calling the
upstream directly with a credential it does not have.

## Proxy behaviors {#proxy-behaviors}

Janus does one of three things with a call, in increasing order of involvement.
All three substitute the credential; they differ only in what happens to the
response.

- **Pass through.** The default and the baseline. The response is relayed
  unmodified. This is the lowest-latency path and the only one available with no
  plugin configured.
- **Pass through and observe.** The response is still relayed unmodified, but
  the call is recorded and/or emitted as an event. Observation is strictly off
  the critical path: it must not alter what the caller receives, and it must not
  be able to stall or fail the response. This is the path that makes usage
  attribution and auditing possible without changing behavior.
- **Pass through and augment.** The response is transformed before it reaches
  the caller, transparently — the caller still sees a well-formed response of
  the kind the upstream produces, not a Janus-specific envelope. This is the
  only behavior that can change a caller-visible result, which makes it the one
  that needs the tightest definition. What a plugin may legitimately change, and
  how that interacts with streamed responses, is not yet decided — see
  <ref target="docs/status#open-questions">open questions</ref>.

## What's real vs. scaffolded {#reality-check}

`proto/janus.proto` is a working design — see
<ref target="docs/plugin-contract">the plugin contract</ref> for the two RPCs it
defines and the reasoning behind their shapes. Everything else described above
(the crates, the lifecycle, the UI) is design plus scaffolding: each binary
currently just prints a "not yet implemented" message, `janus-core` has no
logic, and the UI is a placeholder page. No proxying happens yet.
