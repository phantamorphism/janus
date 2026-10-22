---
title: Scope and non-goals
type: decision
status: accepted
tags: [janus, scope]
---

# Scope and non-goals

Janus's first upstream implementations build on existing code. Shadow, a
separate application with a Rust backend, has a gateway module bundled inside
its server binary that proxies Anthropic, OpenAI and OpenRouter traffic with
credential pooling, HMAC auth and request logging. That working code is the
starting point for Janus's forwarding path, which is why those three are the
first upstreams Janus will support.

Janus depends on no type from that application and is not scoped to its use
cases. This document records which of that code carries over cleanly, and which
parts are deliberately left out, so the host-specific concerns don't get dragged
back in later.

## Carries over into janus-core {#extractable}

These pieces are generic enough to lift and generalize into
<ref target="docs/architecture#janus-core">janus-core</ref>, with no
host-internal dependencies:

- **HMAC token verification** — already isolated in its own crate there, with no
  entanglement with a database or app-wide types. This becomes the `hmac`
  verification method of Janus's identity extractor, including its
  namespace-separation discipline, which is what keeps a token minted for one
  purpose from being replayable against another. See
  <ref target="docs/caller-identity#methods">verification methods</ref>. The
  *minting* half does not carry over: Janus verifies identities, it does not
  issue them (see <ref target="docs/caller-identity#not-minting">Janus extracts;
  it does not mint</ref>), so the host keeps its own minting code.
- **The credential pool / store** — pure filesystem, directory-per-slot. It
  auto-detects `.credentials.json` for Anthropic, `auth.json` for OpenAI/Codex,
  and plain API keys for OpenRouter. No database dependency.
- **OAuth refresh logic** — Keychain-backed for Anthropic, `auth.json`-backed
  for OpenAI (codex). This is the "how a credential of this kind is refreshed"
  piece of an upstream implementation.
- **The three HTTP proxy routers** — Anthropic, OpenAI, and OpenRouter. These
  are the first instances of the narrow per-upstream surface described in
  <ref target="docs/credential-substitution#general-purpose">not specific to LLM
  providers</ref>.

None of this has been ported yet — see
<ref target="docs/status#build-status">the current build status</ref>.

## Deliberately left out {#left-behind}

Two pieces of that gateway are **not** being ported, because they depend on
host-internal types Janus has no business knowing about. Both are replaced by
the <ref target="docs/plugin-contract">plugin contract</ref> instead:

- **Its account-routing subsystem (AC-03).** It depends on that application's
  SQLite database, `AiAccountStore`, `ConversationBus`, `SecretBackend`, and
  `EnvironmentStore` — all host-internal types with no generic equivalent. The
  Janus equivalent is
  <ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>: a host
  that wants per-caller account routing implements it as a plugin, instead of
  Janus depending on one application's account model. Its "hard error, no
  fallback" invariant does carry over — that is the same reasoning behind
  `ResolveRoute`'s fail-closed posture.
- **Its app-wide `NotificationKind` enum and context-checkpoint persistence
  feature.** Replaced by
  <ref target="docs/plugin-contract#notify">Notify</ref>'s generic event
  envelope. The context-checkpoint signal in particular drives a
  compaction-preamble feature specific to that application and will not be a
  Janus built-in; a host can still emit or consume it as an arbitrary
  `event_type`.

The test for "does this belong in janus-core": would an operator with no
relationship to that application ever need it? If the answer depends on a
host-internal type, it belongs behind the plugin contract, in a plugin's own
implementation.

## Known gap: embedded callers {#inprocess-gap}

That gateway runs embedded in its host's process, and the host calls it
in-process for more than proxying: minting tokens from other parts of its own
code, a credential-peek UI, and a request-log viewer.

Token minting turns out not to be a migration cost at all. Janus does not mint
identities, so the host keeps that code exactly where it is and configures a
matching `hmac` extractor binding with the same secret and namespace — Janus
verifies what the host already issues. Nothing moves.

The other two do move. Janus is a separate process, so credential inspection and
log viewing become calls against
<ref target="docs/architecture#admin-surface">the daemon's admin surface</ref> —
both already in its intended scope, but the endpoints don't exist yet (see
<ref target="docs/status#open-questions">open questions</ref>). Recorded here so
the migration cost isn't discovered late.
