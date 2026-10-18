---
title: Janus
type: overview
status: draft
tags: [janus, overview]
---

# Janus

Janus is a standalone, open-source, dual-upstream LLM proxy. It proxies traffic
to Anthropic and OpenAI (OpenRouter too, as a third upstream), and is named for
the Roman god of doors and thresholds — traditionally depicted two-faced,
looking both directions at once. That fits a proxy that sits at the threshold
between a client application and multiple upstream providers.

## Origin {#origin}

Janus began as an extraction. [Shadow](https://github.com/Soft-In-Science-Ltd)
(a personal AI assistant with a Rust/axum backend) has a gateway module bundled
inside its `shadow-server` binary crate that proxies LLM traffic, handling
credential pooling, HMAC auth, and request logging. That module was pulled out
into its own project — Janus — so it can be used by applications other than
Shadow, with no dependency on Shadow at all.

## The three goals {#goals}

1. **Standalone by default.** A daemon and CLI that work with zero external
   dependencies out of the box: a pure pass-through proxy, no configuration
   required to get traffic flowing.
2. **Extensible via a gRPC plugin contract, not forks.** Host-specific behavior
   — routing decisions, event notifications — is reached only through a defined
   gRPC contract. Janus is the gRPC *client*; a host application (such as
   Shadow) implements the contract as the gRPC *server*. See
   <ref target="docs/plugin-contract">the plugin contract</ref> for the design
   and reasoning.
3. **Its own project.** Janus lives in the `phantamorphism` GitHub org, is dual
   MIT/Apache-2.0 licensed, and is public. It is not scoped to Shadow's use
   cases — see <ref target="docs/scope">scope and non-goals</ref> for what that
   means concretely.

## Where to go next {#links}

- <ref target="docs/architecture">Architecture</ref> — the crate, daemon, CLI,
  and UI split, and what each piece is responsible for.
- <ref target="docs/plugin-contract">Plugin contract</ref> — the `ResolveRoute`
  and `Notify` design, and why the two RPCs are shaped differently.
- <ref target="docs/scope">Scope and non-goals</ref> — what's cleanly
  extractable from Shadow's internal gateway, and what's deliberately left
  behind.
- <ref target="docs/status">Status</ref> — current build status and open
  questions, honestly unresolved rather than guessed at.
