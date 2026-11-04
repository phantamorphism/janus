---
title: Scope and non-goals
type: decision
status: accepted
tags: [janus, scope]
---

# Scope and non-goals

Janus is a credential-substituting proxy with a narrow, upstream-agnostic core.
This document states what that core contains, what is deliberately outside it,
and how a deployment that previously embedded a gateway in its own process
reaches the same functions through Janus's admin surface.

The first upstreams are Anthropic, OpenAI and OpenRouter because the need for
them is concrete. Janus depends on no application-specific type and is not
scoped to one deployment's use cases.

## What janus-core provides {#extractable}

These pieces are generic, with no dependency on any particular host application,
and live in <ref target="docs/architecture#janus-core">janus-core</ref>:

- **HMAC token verification.** The `hmac` verification method of the identity
  extractor, including its namespace separation, which keeps a token minted for
  one purpose from being replayable against another. See
  <ref target="docs/caller-identity#methods">verification methods</ref>. Janus
  verifies identities; it does not issue them (see
  <ref target="docs/caller-identity#not-minting">Janus extracts; it does not
  mint</ref>), so the minting half is always the deployment's own.
- **The credential pool.** Filesystem-only, directory-per-slot, with no database
  dependency. It detects `.credentials.json` for Anthropic, `auth.json` for
  OpenAI/Codex, and plain API keys for OpenRouter.
- **OAuth refresh.** Keychain-backed for Anthropic, `auth.json`-backed for
  OpenAI (Codex): the "how a credential of this kind is refreshed" piece of an
  upstream implementation.
- **The three HTTP proxy routers.** Anthropic, OpenAI and OpenRouter, the first
  instances of the narrow per-upstream surface described in
  <ref target="docs/credential-substitution#general-purpose">not specific to LLM
  providers</ref>.

The current build state is in
<ref target="docs/status#build-status">status</ref>.

## What is deliberately not in Janus {#left-behind}

Two things that a host application often wants are left out because they depend
on that application's own types, and Janus has no business knowing about them.
Both are served by the <ref target="docs/plugin-contract">plugin contract</ref>
instead:

- **Account routing.** Choosing which of several accounts serves a given caller
  depends on the host's account store, event bus, secret backend and environment
  model. The Janus equivalent is
  <ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>: a host
  that wants per-caller account routing implements it as a plugin, so Janus does
  not depend on one application's account model. The fail-closed "hard error, no
  fallback" posture is the same reasoning behind `ResolveRoute`.
- **Application-specific event kinds.** An app-wide notification enum, or a
  feature such as context-checkpoint persistence, is not a Janus built-in. Janus
  provides <ref target="docs/plugin-contract#notify">Notify</ref>'s generic
  event envelope, and a host can emit or consume any signal as an arbitrary
  `event_type`.

The test for "does this belong in janus-core": would an operator with no
relationship to the host application ever need it? If the answer depends on a
host-internal type, it belongs behind the plugin contract, in a plugin's own
implementation.

## Migrating from an embedded gateway {#inprocess-gap}

A gateway that runs inside its host's process is usually called in-process for
more than proxying: minting tokens from other parts of the host, a credential
inspector, and a request-log viewer. Moving to Janus, a separate process,
changes each of these as follows.

Token minting is not a migration cost. Janus does not mint identities, so the
host keeps that code where it is and configures a matching `hmac` extractor
binding with the same secret and namespace; Janus verifies what the host already
issues.

Credential inspection and log viewing become calls against
<ref target="docs/architecture#admin-surface">the daemon's admin surface</ref>:
the credential peek is `GET /v1/credentials/default?upstream=` and the list is
`GET /v1/credentials` (metadata only; material is never returned), and the log
viewer is `GET /v1/requests` with its filters, plus `GET /v1/requests/stream`
for a live tail. See <ref target="docs/admin-surface#endpoints">endpoints</ref>.
A host on the same machine reaches them over the admin socket with no token; a
host elsewhere uses the TLS admin listener with the admin token.

Two differences from an embedded gateway are real costs and are stated here
rather than discovered later. The request log is metadata-only, so a viewer that
showed request and response bodies loses them. And a host's tap on upstream
response bytes, if its own runtime consumes them, is response inspection and
stays on the host's side of Janus; see
<ref target="docs/architecture#no-response-transforms">out of scope: response
transformation</ref>.
