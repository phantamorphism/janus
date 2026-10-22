---
title: Status
type: overview
status: draft
tags: [janus, status]
---

# Status

As of 2026-10-21, both the `janus` code repository and this documentation corpus
are brand new. This document is a plain statement of what's actually built, and
a list of what's still genuinely undecided — not a place to invent answers to
make the picture look more finished than it is.

## Current build status {#build-status}

- `cargo check` passes on the `janus` workspace. `tsc` and `eslint` pass on the
  UI.
- Every binary is a stub: `janus-daemon` and `janus` (the CLI) each just print a
  "not yet implemented" message. `janus-core` is a doc-comment stub with no
  logic. The UI is a single placeholder page.
- No proxying happens yet, and no credential has ever been substituted. The
  forwarding path described in
  <ref target="docs/architecture#request-lifecycle">the request lifecycle</ref>
  is design, not running code. See <ref target="docs/scope#extractable">scope:
  what carries over</ref> for what the first implementation work involves.
- `proto/janus.proto` is the one exception: it's a worked design, not a stub.
  See <ref target="docs/plugin-contract">the plugin contract</ref>.
- This documentation corpus is the first deliverable beyond scaffolding. It
  describes the intended design in full, deliberately, while the code is still a
  scaffold — so read everything outside this section as design unless it says
  otherwise.

## Decided, and where the design lives {#decided}

These were open questions in an earlier revision of this document. They are
answered now, and the answers live in the design docs rather than here:

- **Caller identity.** Janus does not mint identities; it extracts and verifies
  what the deployment's own provisioner issued, via a configured extractor per
  request type. <ref target="docs/caller-identity">Caller identity
  extraction</ref>.
- **The in-process plugin model.** Compile-time linkage, not dynamic loading: a
  Rust trait and a custom build.
  <ref target="docs/plugin-contract#linked">Linked plugins are a custom
  build</ref>.
- **Response augmentation.** Out of scope, deliberately, until a concrete use
  case demands it. <ref target="docs/architecture#no-response-transforms">Out of
  scope: response transformation</ref>.
- **Where metering lives.** The policy is plugin-owned; the counting primitives
  ship in `janus-core` for plugins to link.
  <ref target="docs/plugin-contract#metering-primitives">Metering
  primitives</ref>.
- **The correlation field's name.** `conversation_id` was an LLM-ism and is
  gone. The contract now carries a Janus-assigned `request_id` and a separate,
  possibly-untrusted `correlation_id` — two things the old single field was
  conflating.
  <ref target="docs/plugin-contract#resolve-route">ResolveRoute</ref>.
- **How the admin surface is kept away from callers.** Separate listeners, with
  a unix socket as the default admin address.
  <ref target="docs/architecture#admin-surface">The admin surface is a separate
  listener</ref>.

## Open questions {#open-questions}

These are genuinely undecided. Treat any answer you might infer from context
elsewhere as speculation until one of these is actually resolved and this
section is updated.

- **Plugin endpoint discovery.** How does `janus-daemon` learn where a
  configured plugin is? Environment variable, config file, CLI flag, some
  combination — and how is the out-of-process transport selected alongside it?
  Not decided. The linked model is not affected: it is selected by which binary
  you built (see <ref target="docs/plugin-contract#custom-build">what a custom
  build looks like</ref>). See
  <ref target="docs/plugin-contract#no-plugin-pass-through">the pass-through
  requirement</ref> this has to coexist with.
- **Daemon-to-plugin auth.** Is the gRPC channel between `janus-daemon` and an
  out-of-process plugin secured with mTLS, a shared secret, or is it assumed to
  be localhost-only with no auth? Not decided, and the answer differs between
  the local-socket and network models. (The linked model has no channel to
  secure.)
- **A shared backend for the metering primitives.** `janus-core` ships an
  in-memory `UsageStore`, which is correct for one plugin process and loses its
  counts on restart. A durable or distributed backend — for a plugin with
  replicas, or counts that have to survive a restart — is not designed. The
  trait exists so adding one is additive rather than a redesign, and nothing
  currently needs it. See
  <ref target="docs/plugin-contract#metering-primitives">metering
  primitives</ref>.
- **The opaque-identity registry.** One of the four extraction methods looks
  identities up in a daemon-owned registry, and that registry's storage and
  admin operations are not designed — see
  <ref target="docs/caller-identity#open-registry">open: the opaque-identity
  registry</ref>. It is a piece of the admin command surface below, and the
  other three extraction methods don't depend on it.
- **Full `janus` CLI and admin endpoint surface.**
  <ref target="docs/architecture#janus-cli">janus-cli</ref> is described as an
  admin client for credential management, pool health, and log tailing, and
  <ref target="docs/architecture#admin-surface">where it connects and how it
  authenticates</ref> is now decided — but the actual command set and the
  endpoints behind it don't exist yet.
- **UI feature scope.** <ref target="docs/architecture#ui">The UI</ref> is a
  placeholder page today. What it should actually show or let an operator do is
  undecided. Where it is served from is decided.
- **Embedded callers of the prior gateway.** See
  <ref target="docs/scope#inprocess-gap">scope: known gap</ref> — the gateway
  Janus builds on is called in-process by its host for token minting, credential
  inspection, and log viewing. Minting is no longer a Janus problem (the host
  keeps it), but the daemon admin surface the other two have to move to does not
  exist yet. Recorded here so it isn't forgotten.
