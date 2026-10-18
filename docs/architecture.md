---
title: Architecture
type: reference
status: draft
tags: [janus, architecture]
---

# Architecture

Janus is a Cargo workspace of three crates plus a standalone web UI. This
document describes what each piece is responsible for. See
<ref target="docs/status#build-status">status</ref> for how much of this is
actually implemented today (as of 2026-10-18: scaffolding only).

## Crate split {#crates}

```
janus/
├── crates/
│   ├── janus-core/      # proxy logic, credential pool, HMAC auth
│   ├── janus-daemon/    # long-running binary; hosts the gRPC plugin server
│   └── janus-cli/       # admin client binary ("janus")
├── proto/
│   └── janus.proto      # the plugin contract
└── ui/                  # standalone web UI
```

### janus-core {#janus-core}

The proxy brain: credential pool, HMAC token minting/verification, and the HTTP
forwarding logic for each upstream (Anthropic, OpenAI, OpenRouter). This is a
library crate, not a binary — both `janus-daemon` and (eventually) tooling link
against it.

It is intended to be lifted and generalized from Shadow's internal
`shadow-server/src/gateway/` and `shadow-core/src/gateway/token.rs`. See
<ref target="docs/scope#extractable">scope: what's extractable</ref> for the
specific pieces of Shadow's gateway this crate is meant to absorb.

### janus-daemon {#janus-daemon}

The long-running process. Runs the dual-upstream proxy and hosts the gRPC plugin
server defined in <ref target="docs/plugin-contract">the plugin contract</ref> —
meaning `janus-daemon` is the gRPC *client* that calls out to a host-implemented
plugin, not the server implementing it.

With no plugin endpoint configured, `janus-daemon` makes zero calls defined in
`proto/janus.proto` and behaves as a plain pass-through proxy. This is an
explicit design requirement (goal 1 in <ref target="docs/index#goals">the three
goals</ref>), not an accident of the current stub state. How a plugin endpoint
gets discovered and authenticated is not yet decided — see
<ref target="docs/status#open-questions">open questions</ref>.

### janus-cli {#janus-cli}

The admin client binary, named `janus`. Talks to `janus-daemon` over its control
channel and contains no proxy logic of its own. Intended scope: credential
management, pool health inspection, and request-log tailing. The full command
surface is not yet decided — see <ref target="docs/status#open-questions">open
questions</ref>.

### ui {#ui}

A standalone web UI: React 19, Vite 8, Tailwind v4, shadcn. This stack
deliberately matches Shadow's own frontend — not because Janus depends on
Shadow, but because it's the tooling the author already knows and wants
consistency with. The UI is served on its own port, separate from any host
application's UI. Feature scope beyond the current placeholder page is not yet
decided — see <ref target="docs/status#open-questions">open questions</ref>.

## What's real vs. scaffolded {#reality-check}

`proto/janus.proto` is a finished design — see
<ref target="docs/plugin-contract">the plugin contract</ref> for the two RPCs it
defines and the reasoning behind their shapes. Everything else described above
(the crates and the UI) is scaffolding: each binary currently just prints a "not
yet implemented" message, and the UI is a placeholder page. No proxy logic has
been ported from Shadow yet.
