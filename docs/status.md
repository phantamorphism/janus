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
- No proxy logic has been ported from Shadow yet. See
  <ref target="docs/scope#extractable">scope: what's extractable</ref> for what
  that porting work will eventually involve.
- `proto/janus.proto` is the one exception: it's a finished design, not a stub.
  See <ref target="docs/plugin-contract">the plugin contract</ref>.
- This documentation corpus is the first deliverable beyond scaffolding.

## Open questions {#open-questions}

These are genuinely undecided. Treat any answer you might infer from context
elsewhere as speculation until one of these is actually resolved and this
section is updated.

- **Plugin endpoint discovery.** How does `janus-daemon` learn where a
  configured plugin server is? Environment variable, config file, CLI flag, some
  combination? Not decided. See
  <ref target="docs/architecture#janus-daemon">janus-daemon</ref> and
  <ref target="docs/plugin-contract#no-plugin-pass-through">the pass-through
  requirement</ref> this has to coexist with.
- **Daemon-to-plugin auth.** Is the gRPC channel between `janus-daemon` and a
  plugin server secured with mTLS, a shared secret, or is it assumed to be
  localhost-only with no auth? Not decided.
- **Full `janus` CLI command surface.**
  <ref target="docs/architecture#janus-cli"> janus-cli</ref> is described as an
  admin client for credential management, pool health, and log tailing, but the
  actual command set beyond that description doesn't exist yet.
- **UI feature scope.** <ref target="docs/architecture#ui">The UI</ref> is a
  placeholder page today. What it should actually show or let an operator do is
  undecided.
- **Shadow's in-process gateway callers.** See
  <ref target="docs/scope#inprocess-gap">scope: known gap</ref> — Shadow's
  backend calls its internal gateway in-process for token minting, a
  credential-peek UI, and request-log viewing, none of which have an HTTP/RPC
  equivalent against Janus yet. Explicitly out of scope for now, recorded here
  so it isn't forgotten.
