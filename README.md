---
title: Janus
type: overview
status: accepted
---

# Janus

A standalone, dual-upstream (Anthropic + OpenAI) LLM proxy. Named for the Roman
god of doors and thresholds, depicted two-faced, looking both directions at once
— fitting for something that sits at the threshold between a client and two
upstream providers.

**Status: early scaffold. Nothing is implemented yet.**

## Goals

- **Standalone by default.** A daemon + CLI you can run with zero dependencies
  beyond upstream credentials. No external calls, no configuration required to
  get a working pass-through proxy.
- **Extensible via plugins, not forks.** Optional behavior — routing decisions,
  event notifications — is reached only through a gRPC contract
  (`proto/janus.proto`), never by depending on a specific host application's
  internals. Janus is the gRPC *client*; a host app (e.g.
  [Shadow](https://github.com/Soft-In-Science-Ltd)) is the plugin *server*.
- **Its own project.** This started as an extraction from Shadow's internal
  gateway module, but it does not depend on Shadow and is not scoped to Shadow's
  use cases.

## Layout

```
janus/
├── crates/
│   ├── janus-core/      # proxy logic, credential pool, HMAC auth
│   ├── janus-daemon/    # long-running binary; hosts the gRPC plugin server
│   └── janus-cli/       # admin client binary ("janus"), talks to the daemon
├── proto/
│   └── janus.proto      # the plugin contract: ResolveRoute (sync), Notify (async)
├── ui/                  # standalone web UI (React/Vite/Tailwind), own port
├── docs/                # design docs, kept consistent by Schematic (`sch`)
└── README.md
```

## Documentation

Design docs live in `docs/` and are kept internally consistent by
[Schematic](https://github.com/phantamorphism/schematic) (the `sch` CLI) — this
repo is itself a Schematic corpus (`schematic.yaml` at the root). Start at
<ref target="docs/index">docs/index.md</ref>. When editing docs, use the
`sch branch` / `sch check` / `sch fix` / `sch commit` / `sch merge` loop instead
of plain git — see the `schematic` skill or `skills/schematic/SKILL.md` in the
Schematic repo for the full workflow.

## Plugin contract

Two extension points, shaped differently because they behave differently:

- **`ResolveRoute`** — a synchronous routing decision, consulted before a
  request is forwarded upstream. Blocking by design. Fails **closed**: if a
  configured plugin is unreachable or errors, the request is rejected rather
  than silently falling back to a default credential.
- **`Notify`** — a fire-and-forget event stream using one generic envelope
  (`{event_type, payload}`) rather than one RPC per event kind, so new event
  types don't require a proto change.

With no plugin configured, Janus makes zero calls defined in `janus.proto` and
behaves as a plain pass-through proxy.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your
option.
