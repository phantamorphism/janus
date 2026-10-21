---
title: Status
type: overview
status: draft
tags: [janus, status]
---

# Status

As of 2026-10-18, both the `janus` code repository and this documentation corpus
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

## Open questions {#open-questions}

These are genuinely undecided. Treat any answer you might infer from context
elsewhere as speculation until one of these is actually resolved and this
section is updated.

- **Caller identity minting and presentation.** Janus's whole model depends on a
  caller holding an identity of its own instead of a real credential (see
  <ref target="docs/credential-substitution#actors">actors</ref>), and
  `ResolveRouteRequest.caller_identity` carries the verified result of that —
  but *how* a caller gets one and presents it is not decided. Who mints it, at
  what point in a sandbox's lifecycle, how it reaches the sandbox without
  reintroducing a secret-distribution problem, how it is presented on the wire
  (an `Authorization` header in the upstream's own scheme, a separate header,
  something else), how long it lives, and how it is revoked — all open. The
  contract has a place to carry the identity; the mechanism behind it does not
  exist.
- **The bundled plugin ABI.** The in-process execution model in
  <ref target="docs/plugin-contract#transports">execution and transport
  models</ref> says a plugin can be loaded into the daemon and conform to the
  same interface without going over a socket. How that is actually achieved is
  not decided: a dynamically loaded shared object, a Rust trait with the plugin
  statically linked at build time, an in-process gRPC channel that skips the
  network but keeps the serialization, or something else. The trade-offs differ
  sharply (who can ship a plugin, whether a plugin can be changed without
  rebuilding Janus, what a panicking plugin does to the daemon), so this is a
  real decision and not a detail.
- **What response augmentation is allowed to do.** The augment behavior in
  <ref target="docs/architecture#proxy-behaviors">proxy behaviors</ref> is
  described as transparently transforming a response before it reaches the
  caller, but its actual permissions are unspecified: arbitrary rewriting of
  response bytes, a constrained set of transforms, header-only changes,
  something schema-aware per upstream? Streaming makes it harder still — a
  transform that needs the whole body contradicts relaying a stream as it
  arrives. Undecided, and the narrowest useful definition has not been found
  yet.
- **Where usage and credit metering actually lives.** Denying an over-budget
  caller works today only in the sense that a plugin can return a `deny_reason`.
  Whether that is the whole story — Janus asks, the plugin counts — or whether
  Janus needs its own metering primitives (per-identity counters, cost
  extraction from upstream responses, a quota store, something a plugin can
  query rather than reimplement) is not decided. Related: a plugin that counts
  for itself needs post-call cost data, which only arrives over
  <ref target="docs/plugin-contract#notify">Notify</ref> after the fact; nobody
  has worked through whether that is sufficient.
- **Plugin endpoint discovery.** How does `janus-daemon` learn where a
  configured plugin is? Environment variable, config file, CLI flag, some
  combination — and how is the execution model selected alongside it? Not
  decided. See <ref target="docs/plugin-contract#no-plugin-pass-through">the
  pass-through requirement</ref> this has to coexist with.
- **Daemon-to-plugin auth.** Is the gRPC channel between `janus-daemon` and a
  plugin secured with mTLS, a shared secret, or is it assumed to be
  localhost-only with no auth? Not decided, and the answer differs per execution
  model.
- **Whether `conversation_id` is the right correlation key.** It is carried on
  both `ResolveRouteRequest` and `NotifyEvent` and is currently documented as an
  optional, opaque correlation hint. The name comes from the LLM upstreams that
  Janus implements first, and reads oddly for a general-purpose proxy. Left as
  it is for now rather than renamed on speculation; worth revisiting once there
  is a non-LLM upstream to test it against.
- **Full `janus` CLI command surface.**
  <ref target="docs/architecture#janus-cli">janus-cli</ref> is described as an
  admin client for credential management, pool health, and log tailing, but the
  actual command set beyond that description doesn't exist yet.
- **UI feature scope.** <ref target="docs/architecture#ui">The UI</ref> is a
  placeholder page today. What it should actually show or let an operator do is
  undecided.
- **Embedded callers of the prior gateway.** See
  <ref target="docs/scope#inprocess-gap">scope: known gap</ref> — the gateway
  Janus builds on is called in-process by its host for token minting, credential
  inspection, and log viewing, and the daemon admin surface those would have to
  move to does not exist yet. Recorded here so it isn't forgotten.
