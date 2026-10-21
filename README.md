---
title: Janus
type: overview
status: accepted
---

# Janus

A standalone proxy that stands between a caller and the authenticated services
it needs to reach, so the caller never holds the real credentials.

The motivating caller is an agent in a sandbox: it needs to call APIs gated by
API keys and OAuth tokens, but the sandbox is the last place those secrets
should live. With Janus, the agent is issued its own identity, presents that
identity, and calls the upstream's own API as usual. Janus substitutes the real
credential on the way out. The agent can authenticate; it can never read the
token it authenticated with.

And because every such call crosses Janus, Janus is where policy on those calls
gets enforced — is this identity allowed this credential, is it out of credit,
is it over its rate budget — via plugins rather than hardcoded rules.

Named for the Roman god of doors and thresholds, depicted two-faced, looking
both directions at once: one face toward the caller, one toward the upstream.

**Status: early scaffold. Nothing is implemented yet.**

## First upstreams

Anthropic, OpenAI and OpenRouter — the LLM APIs — are the first upstreams Janus
targets, because that is where the need is concrete and there is existing proxy
code to build on. They are the first implementation of the model, not the
definition of it: holding a credential on a caller's behalf, substituting it
per-request, and gating the call on policy applies to any authenticated HTTP
service an agent is pointed at.

## Goals

- **Callers never hold real credentials.** The credential a request is
  authenticated with is resolved and injected inside Janus. A caller's own
  identity is the only secret it sees, and compromising it does not yield the
  credential behind it.
- **Every call is a policy enforcement point.** Authentication, authorization,
  and credit/usage checks apply per-request, before anything is forwarded, and a
  denial is a first-class outcome.
- **Standalone by default.** A daemon + CLI you can run with no dependencies
  beyond upstream credentials. No external calls, no configuration required to
  get a working pass-through proxy.
- **Extensible via plugins, not forks.** Policy and observability behavior is
  reached only through a gRPC contract (`proto/janus.proto`). Janus is the gRPC
  *client*; the plugin is the *server*, and can run in-process, on a local unix
  socket, or across the network.

## Layout

```
janus/
├── crates/
│   ├── janus-core/      # identity verification, credential pool, substitution, forwarding
│   ├── janus-daemon/    # long-running binary; data plane + admin surface + plugin client
│   └── janus-cli/       # admin client binary ("janus"), talks to the daemon
├── proto/
│   └── janus.proto      # the plugin contract: ResolveRoute (sync), Notify (async)
├── ui/                  # standalone operator web UI, served on its own port
├── docs/                # design docs, kept consistent by Schematic (`sch`)
└── README.md
```

## Documentation

Design docs live in `docs/` and are kept internally consistent by
[Schematic](https://github.com/phantamorphism/schematic) (the `sch` CLI) — this
repo is itself a Schematic corpus (`schematic.yaml` at the root). Start at
<ref target="docs/index">docs/index.md</ref>; the core model is in
<ref target="docs/credential-substitution">docs/credential-substitution.md</ref>.
When editing docs, use the `sch branch` / `sch check` / `sch fix` / `sch commit`
/ `sch merge` loop instead of plain git — see the `schematic` skill or
`skills/schematic/SKILL.md` in the Schematic repo for the full workflow.

## Plugin contract

Two extension points, shaped differently because they behave differently:

- **`ResolveRoute`** — the synchronous credential-substitution decision,
  consulted after the caller's identity is verified and before the request is
  forwarded. Given a verified caller identity and the target upstream, a plugin
  answers with a credential to use, a deferral to the default chain, or a denial
  — the single hook for auth and credit/usage enforcement. Blocking by design.
  Fails **closed**: if a configured plugin is unreachable or errors, the request
  is rejected rather than silently falling back to a default credential.
- **`Notify`** — a fire-and-forget event stream using one generic envelope
  (`{event_type, conversation_id, payload_json}`) rather than one RPC per event
  kind, so new event types don't require a proto change.

With no plugin configured, Janus makes zero calls defined in `janus.proto` and
behaves as a plain credential-substituting pass-through proxy.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your
option.
