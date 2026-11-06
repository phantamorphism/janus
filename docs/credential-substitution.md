---
title: Credential substitution and policy enforcement
type: design
status: draft
tags: [janus, credentials, identity, policy]
---

# Credential substitution and policy enforcement

This document describes the model Janus is built around. The
<ref target="docs/architecture">architecture</ref> describes the modules that
implement it and the path a single request takes through them; this document
describes what those modules are for.

## The problem {#problem}

An agent runs in a sandbox and is asked to do work that requires calling an
authenticated service — an LLM API, a SaaS API, an internal service. The
conventional way to enable that is to put a credential in the sandbox: an API
key in an environment variable, an OAuth token in a config file on disk.

That has three consequences that get worse as the number of agents grows:

- **Blast radius.** The credential is usually long-lived and broadly scoped. Any
  code the agent runs, any dependency it pulls, any prompt-injected instruction
  it follows, can read the credential and use it for something other than the
  task — including exfiltrating it.
- **No attribution.** Every agent sharing a credential is indistinguishable at
  the upstream. Usage, abuse, and cost all land on one undifferentiated
  identity.
- **Revocation is coarse.** Cutting off one misbehaving agent means rotating a
  credential that every other agent is also using.

The underlying mistake is co-locating *the right to make a call* with *the
secret that authorizes the call*. Janus separates them.

## Actors {#actors}

- **Caller.** The agent, in its sandbox. It holds its own identity and nothing
  else. It is configured to reach the upstream through Janus rather than
  directly, which in practice means a different base URL.
- **Caller identity.** An identity belonging to that specific caller, presented
  on every request it makes through Janus. It is not an upstream credential and
  has no value at the upstream. Janus does not mint it — whatever creates the
  sandbox is the only thing positioned to hand an identity to it, so Janus's
  responsibility begins at *extracting and verifying* what the caller presents.
  See <ref target="docs/caller-identity">caller identity extraction</ref>.
- **janus-daemon.** The proxy process. It extracts and verifies the caller
  identity, determines which real credential the call should be made with,
  performs the substitution, and forwards. See
  <ref target="docs/architecture#janus-daemon">janus-daemon</ref>.
- **Credential pool.** Where the real upstream credentials live, on the Janus
  side of the boundary. Callers have no read path to it.
- **Policy plugin.** An optional implementation of the Janus plugin contract
  that makes the per-request decision: which credential this identity gets, or
  that the call is denied. It can be compiled into a custom build of the daemon
  or reached over gRPC on a local socket or across the network; Janus is always
  the caller of it either way. See <ref target="docs/plugin-contract">the plugin
  contract</ref>.
- **Upstream.** The authenticated service being called. It sees a normal,
  correctly-authenticated request and has no knowledge that a proxy or a
  distinct caller identity is involved.

## The substitution {#substitution}

A request arrives carrying the caller's identity instead of an upstream
credential. Where on the wire that identity sits is configuration, not a fixed
convention — a binding per request type says which header or parameter to read
it from and how to verify it, because different upstreams' SDKs present a
credential in different places and Janus has to accept what the caller already
sends (see <ref target="docs/caller-identity#binding">binding an extractor to a
request type</ref>). Janus verifies the identity, decides which real credential
applies, builds an outbound request that does not carry the caller's identity
material, applies the real credential's auth in whatever form that upstream
expects, and forwards the result upstream. The response comes back along the
same path, relayed unmodified.

Two properties of that exchange are load-bearing:

- **The caller is never told which credential was used.** The response is the
  upstream's own response. There is no handshake in which Janus returns a
  credential to the caller for it to use directly, because that would
  reintroduce the secret into the sandbox. The only thing that crosses back is
  the upstream's answer.
- **The proxy is transparent in request shape.** Janus does not define its own
  API surface in front of each upstream. A caller uses the upstream's own API or
  SDK, unchanged except for the base URL and the credential it presents. This is
  what makes the model adoptable — existing agent code doesn't have to be
  rewritten against a Janus-specific protocol — and it constrains Janus: the
  substitution has to be invisible to a caller that knows only the upstream's
  documented contract.

What this buys, against the three problems above: the sandbox has nothing worth
stealing, every call is attributable to one identity, and revoking a single
identity costs nothing to any other caller.

## Why the proxy is the policy enforcement point {#policy-point}

Credential substitution forces every call through one process. That chokepoint
is worth more than the substitution alone, because it is the one place where a
decision can be made with full knowledge of both sides: who is asking, and what
they are asking for.

Three kinds of check belong there:

- **Authentication.** Is this caller identity real, current, and not revoked?
  Janus answers this one itself, before any plugin is consulted — see
  <ref target="docs/caller-identity">caller identity extraction</ref>. A plugin
  is never handed an unverified identity, and never has to authenticate a caller
  on policy's behalf.
- **Authorization.** May this identity reach this upstream at all, and with
  which credential? Two agents hitting the same upstream can legitimately be
  routed to different accounts.
- **Metering.** Has this identity exhausted its credit, quota, or rate budget?
  Because the check happens before the call is forwarded, an over-budget caller
  can be stopped before it costs anything upstream rather than reconciled after
  the fact. The *policy* — what a limit is and what exceeding it means — is the
  plugin's; the counting is a mechanical problem every such plugin shares, so
  Janus ships primitives for it rather than each plugin reinventing them. See
  <ref target="docs/plugin-contract#metering-primitives">metering
  primitives</ref>.

Authorization and metering collapse into the same contract call. The plugin is
handed the verified caller identity and the intended upstream, and answers with
either a credential to use or a reason the request is refused — see
<ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>. Janus does
not care whether a particular denial was an unauthorized upstream or an
exhausted credit balance; both are policy saying no, and both reach the caller
as a refused request rather than a silently-degraded one. A failed
authentication never gets that far: it is a rejection at the door.

The symmetric point: with no plugin configured there is no policy, and Janus is
a pass-through credential-substituting proxy and nothing more. Policy is
additive.

## Credential selection {#credential-selection}

"Which real credential applies" is answered in two layers. Credentials belong to
an upstream implementation; each `[[upstream]]` is an **instance** of one
implementation, mounted at its own path prefix with its own **credential
filter** (by kind, ref or tag) and **strategy** — see
<ref target="docs/configuration#upstream-instances">upstream instances</ref>.
One vendor can so be split by account type: `/anthropic` for API keys only,
`/claude` for subscriptions only, with no credential reachable through the other
prefix.

- **The plugin chooses**, when one is configured and answers with a
  `credential_ref` (see <ref target="docs/plugin-contract#resolve-route">
  ResolveRoute</ref>). The ref must be one the instance's filter admits; a ref
  outside it fails the request as `credential_unavailable`, exactly like a ref
  that does not exist. The filter is a hard boundary that policy cannot widen.
- **The default chain chooses** otherwise (no plugin, `use_default`, or a `none`
  route): among the admitted credentials that are usable now (active, not
  rate-limited, not disabled or invalid), in the implementation's chain order,
  the instance's strategy picks one: `ordered`, `round_robin`,
  `least_recently_used`, or `sticky` by verified caller identity.

Built in are the deterministic strategies that need nothing but the pool's own
state and the verified identity. Selection that depends on who the caller is
beyond that — pinning a team to an account, spending a budget per account,
balancing by observed usage — is policy, and belongs to the plugin, which
already receives the verified identity, its claims and the instance and
implementation names. `sticky` is the one caller-aware built-in, because it
costs nothing a plugin would do better: it is a pure function of the verified
identity and the admitted refs, keeps no state, never reads the caller's
unverified correlation id, and cannot reach a credential the filter does not
admit. It exists for subscription credentials, whose vendors limit per account
and favour one caller staying on one account, so a deployment without a plugin
gets that behaviour without writing one. The reasoning is recorded in
<ref target="docs/decisions#upstream-instances">upstream instances and built-in
selection</ref>.

### Failover {#failover}

An instance with `failover = true` lets a default-chain request survive a
rejected credential. When the upstream answers a `4xx` that the implementation
classifies as a rate limit, an invalid credential or an account error, Janus:

1. reports the verdict to the pool (a rate limit benches the credential until
   its reset, an invalid credential leaves the chain) and to the plugin
   (`credential_invalid` / `account_error`), as for any relayed rejection;
2. picks again with the strategy, excluding every credential this request has
   tried;
3. sends the same request, with the new credential substituted, and records the
   rejected attempt in the request's `attempts`.

When no admitted credential is left, the last rejection is relayed to the caller
as it came. The rules that keep this safe:

- **Only the default chain fails over.** A credential the plugin named is
  policy's choice; its rejection is relayed and reported, never replaced by
  another.
- **Only a body that can be sent again whole.** The caller's body is buffered
  for replay up to the same 1 MiB limit as the retry on a dead pooled upstream
  connection, and is sent again only when Janus has read it to its end within
  that limit. A larger or still-streaming body is never failed over; its
  rejection is relayed.
- **Nothing reaches the caller before the decision.** Only the response head and
  at most the first 16 KiB of a `4xx` body are read to classify it; if the
  request does not fail over, they are relayed first and the rest follows
  unchanged. A `2xx`, `3xx` or `5xx` is never held back.

Failover never widens access: each attempt uses a credential the instance's
filter admits, and the caller sees only the response of the last attempt.

## Not specific to LLM providers {#general-purpose}

The first upstreams Janus implements are Anthropic, OpenAI, and OpenRouter,
because that is the concrete need (see <ref target="docs/scope">scope and
non-goals</ref>). Those are the first implementation of the model, not the model
itself.

Nothing in the preceding sections is LLM-shaped. "A caller that must not hold
the credential," "a credential resolved per-identity," and "a policy decision
before the call is forwarded" describe any authenticated HTTP service an agent
might be pointed at. The parts that are genuinely upstream-specific are narrow
and enumerable:

- how auth is carried on the wire for that upstream (header name and scheme, or
  a signing procedure),
- the base URL and any path rewriting,
- which inbound headers are forwarded to it — the allowlist the outbound request
  is constructed from, which is also what keeps a caller's identity material
  from leaking upstream (see <ref target="docs/caller-identity#scrub">the
  presented material never goes upstream</ref>),
- how a credential of that kind is refreshed when it expires,
- what in a response is worth observing, for upstreams where observation is
  wanted — this is what populates the `usage` field a metering plugin reads off
  <ref target="docs/plugin-contract#guaranteed-events">`call_completed`</ref>.

Everything else — identity extraction and verification, the credential pool, the
policy call, the forwarding path — is shared. Adding an upstream should mean
supplying that narrow list, not extending the model.
