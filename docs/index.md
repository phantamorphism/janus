---
title: Janus
type: overview
status: draft
tags: [janus, overview]
---

# Janus

Janus is a standalone, open-source proxy that sits between a caller and the
authenticated services it needs to reach, so the caller never has to hold the
real credentials for those services.

The motivating caller is an agent running in a sandbox. Such an agent needs to
call APIs that are gated by API keys, OAuth tokens, or signed headers — but the
sandbox is exactly the place those secrets should not live. Janus resolves that
tension: an agent is issued its own identity, presents that identity to Janus,
and makes ordinary requests against the upstream's own API shape. Janus
substitutes the real credential on the way out. The agent can authenticate and
call; it can never read the token it authenticated with. See
<ref target="docs/credential-substitution">credential substitution and policy
enforcement</ref> for the full model.

Because every one of those calls crosses Janus, Janus is also the natural place
to enforce policy on them — is this identity allowed to use this credential at
all, has it run out of credit, has it exceeded a rate limit. Those decisions are
made by plugins over a gRPC contract rather than being hardcoded, so operators
supply their own policy without forking Janus. See
<ref target="docs/plugin-contract">the plugin contract</ref>.

Janus is named for the Roman god of doors and thresholds, traditionally depicted
two-faced, looking both directions at once — one face toward the caller, one
toward the upstream, which is exactly the position a credential-substituting
proxy occupies.

## First upstreams {#first-upstreams}

The first upstreams Janus implements are the LLM APIs: Anthropic, OpenAI, and
OpenRouter. That is where the need is concrete today, and there is existing
proxy code for those three that Janus builds on (see
<ref target="docs/scope">scope and non-goals</ref>).

Those three are the first implementation, not the definition. Nothing about
holding a credential on a caller's behalf, substituting it per-request, and
gating the call on policy is specific to LLM providers — the same shape applies
to any authenticated HTTP service an agent is asked to call. Janus is built
around that general point; the LLM upstreams are the first proof of it.

## Goals {#goals}

1. **Callers never hold real credentials.** The credential a request is
   ultimately authenticated with is resolved and injected inside Janus. A
   caller's own identity is the only secret it ever sees, and compromising that
   identity does not yield the upstream credential behind it.
2. **Every call is a policy enforcement point.** Because the proxy is on the
   path, authentication, authorization, and credit/usage checks can be applied
   per-request, and a denial is a first-class outcome of the routing decision
   rather than a bolted-on side channel.
3. **Standalone by default.** A daemon and CLI that work with no external
   dependencies out of the box: a pass-through proxy that needs no configuration
   beyond upstream credentials to get traffic flowing.
4. **Extensible via a gRPC plugin contract, not forks.** Policy and
   observability behavior is reached only through a defined gRPC contract. Janus
   is the gRPC *client*; the plugin is the *server*, and can run in-process, on
   a local socket, or across the network. See
   <ref target="docs/plugin-contract">the plugin contract</ref> for the design
   and reasoning.

## Where to go next {#links}

- <ref target="docs/credential-substitution">Credential substitution and policy
  enforcement</ref> — the core model: actors, identities, what gets substituted,
  and where policy is decided.
- <ref target="docs/architecture">Architecture</ref> — the crate, daemon, CLI,
  and UI split, what each piece is responsible for, and the end-to-end request
  lifecycle.
- <ref target="docs/plugin-contract">Plugin contract</ref> — the `ResolveRoute`
  and `Notify` design, why the two RPCs are shaped differently, and the three
  plugin execution models.
- <ref target="docs/scope">Scope and non-goals</ref> — which existing proxy code
  carries over into Janus, and what is deliberately left out.
- <ref target="docs/status">Status</ref> — current build status and open
  questions, honestly unresolved rather than guessed at.
