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
- **Caller identity.** An identity minted for that specific caller, presented on
  every request it makes through Janus. It is not an upstream credential and has
  no value at the upstream. How a caller obtains and presents it is not yet
  decided — see <ref target="docs/status#open-questions">open questions</ref>.
- **janus-daemon.** The proxy process. It verifies the caller identity,
  determines which real credential the call should be made with, performs the
  substitution, and forwards. See
  <ref target="docs/architecture#janus-daemon">janus-daemon</ref>.
- **Credential pool.** Where the real upstream credentials live, on the Janus
  side of the boundary. Callers have no read path to it.
- **Policy plugin.** An optional gRPC service that makes the per-request
  decision: which credential this identity gets, or that the call is denied. See
  <ref target="docs/plugin-contract">the plugin contract</ref>.
- **Upstream.** The authenticated service being called. It sees a normal,
  correctly-authenticated request and has no knowledge that a proxy or a
  distinct caller identity is involved.

## The substitution {#substitution}

A request arrives carrying the caller's identity instead of an upstream
credential — where exactly that identity sits on the wire is not yet decided,
see <ref target="docs/status#open-questions">open questions</ref>. Janus
verifies the identity, decides which real credential applies, strips the
caller's identity material out of the request, applies the real credential's
auth in whatever form that upstream expects, and forwards the result upstream.
The response comes back along the same path.

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
- **Authorization.** May this identity reach this upstream at all, and with
  which credential? Two agents hitting the same upstream can legitimately be
  routed to different accounts.
- **Metering.** Has this identity exhausted its credit, quota, or rate budget?
  Because the check happens before the call is forwarded, an over-budget caller
  can be stopped before it costs anything upstream rather than reconciled after
  the fact. Where the counting that answers the question actually lives — in the
  plugin, or in Janus itself — is not yet decided; see
  <ref target="docs/status#open-questions">open questions</ref>.

All three collapse into the same contract call. The plugin is handed the caller
identity and the intended upstream, and answers with either a credential to use
or a reason the request is refused — see
<ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>. Janus does
not care whether a particular denial was an authentication failure or an
exhausted credit balance; both are policy saying no, and both reach the caller
as a refused request rather than a silently-degraded one.

The symmetric point: with no plugin configured there is no policy, and Janus is
a pass-through credential-substituting proxy and nothing more. Policy is
additive.

## Not specific to LLM providers {#general-purpose}

The first upstreams Janus implements are Anthropic, OpenAI, and OpenRouter,
because that is the concrete need and there is existing proxy code for them to
build on (see <ref target="docs/scope">scope and non-goals</ref>). Those are the
first implementation of the model, not the model itself.

Nothing in the preceding sections is LLM-shaped. "A caller that must not hold
the credential," "a credential resolved per-identity," and "a policy decision
before the call is forwarded" describe any authenticated HTTP service an agent
might be pointed at. The parts that are genuinely upstream-specific are narrow
and enumerable:

- how auth is carried on the wire for that upstream (header name and scheme, or
  a signing procedure),
- the base URL and any path rewriting,
- how a credential of that kind is refreshed when it expires,
- what in a response is worth observing, for upstreams where observation is
  wanted.

Everything else — identity verification, the credential pool, the policy call,
the forwarding path — is shared. Adding an upstream should mean supplying that
narrow list, not extending the model.
