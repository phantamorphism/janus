---
title: Scope and non-goals
type: decision
status: accepted
tags: [janus, scope, shadow]
---

# Scope and non-goals

Janus was extracted from Shadow's internal gateway module, but it is not scoped
to Shadow's use cases and has no dependency on any Shadow-specific type (see
<ref target="docs/index#origin">origin</ref>). This document records what
carries over cleanly and what's deliberately being left behind, so future
contributors don't try to drag Shadow-specific concerns back in.

## Extractable into janus-core {#extractable}

These pieces of Shadow's internal gateway are generic enough to lift and
generalize into <ref target="docs/architecture#janus-core">janus-core</ref>,
with no Shadow-internal dependencies:

- **HMAC token minting and verification** — already isolated in Shadow's
  `shadow-core` crate, so it has no entanglement with Shadow's database or
  app-wide types to begin with.
- **The credential pool / store** — pure filesystem, directory-per-slot. It
  auto-detects `.credentials.json` for Anthropic, `auth.json` for OpenAI/Codex,
  and plain API keys for OpenRouter. No database dependency.
- **OAuth refresh logic** — Keychain-backed for Anthropic, `auth.json`-backed
  for OpenAI (codex).
- **The three HTTP proxy routers** — Anthropic, OpenAI, and OpenRouter.

None of this has been ported yet — see
<ref target="docs/status#build-status">the current build status</ref>.

## Deliberately left behind {#left-behind}

Two pieces of Shadow's internal gateway are **not** being ported, because they
depend on Shadow-internal types that Janus has no business knowing about. Both
are replaced by the <ref target="docs/plugin-contract">plugin contract</ref>
instead:

- **Shadow's AC-03 account-routing subsystem.** It depends on Shadow's SQLite
  database, `AiAccountStore`, `ConversationBus`, `SecretBackend`, and
  `EnvironmentStore` — all Shadow-internal types with no generic equivalent. It
  is replaced by
  <ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>: a host
  that wants account-routing behavior implements it as a plugin server, instead
  of Janus depending on Shadow's account model directly.
- **Shadow's app-wide `NotificationKind` enum and context-checkpoint persistence
  feature.** Replaced by
  <ref target="docs/plugin-contract#notify">Notify</ref>'s generic event
  envelope. The context-checkpoint-detected signal specifically is
  Shadow-specific (it drives a compaction-preamble feature) and will not be a
  Janus built-in; a host can still choose to emit or consume it as an arbitrary
  `event_type` over `Notify`.

The test for "does this belong in janus-core": would a user of Janus with no
relationship to Shadow ever need it? If the answer depends on a Shadow-internal
type, it belongs behind the plugin contract instead, in a host's own
implementation.

## Known gap: Shadow's in-process callers {#inprocess-gap}

Shadow's own backend is today a heavy *in-process* Rust caller of its internal
gateway — not just for proxying LLM traffic, but for things like token minting
calls from other Shadow code, a credential-peek UI, and a request-log viewer.
None of those have an HTTP/RPC-shimmed equivalent in Janus yet. This is out of
scope for the current extraction but worth tracking — see
<ref target="docs/status#open-questions">open questions</ref>.
