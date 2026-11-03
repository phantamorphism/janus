---
title: Status
type: overview
status: draft
tags: [janus, status]
---

# Status

This document is a plain statement of what is built, what is not, and anything
still genuinely undecided — not a place to invent answers to make the picture
look more finished than it is.

## Current build status {#build-status}

Built and running:

- **`janus-core`** — the plugin contract (`janus.proto` and its generated client
  and server), the `JanusPlugin` trait for linked plugins, the identity
  extractors (`hmac`, `jwt`, `opaque`, `trusted_header`, plus `none` and custom
  methods) and their bindings, the `peer::PeerUid` interceptor for socket
  plugins, and the metering primitives with the in-memory and SQLite
  `UsageStore`s and their conformance suite.
- **`janus-daemon`** — the data plane for the Anthropic, OpenAI (including
  ChatGPT-account OAuth credentials, served at the Codex backend) and OpenRouter
  upstreams, through the full
  <ref target="docs/architecture#request-lifecycle">request lifecycle</ref>; the
  credential pool with config credentials, pool slots and refresh; the plugin
  client over a unix socket, mutual TLS or opt-in plaintext, or linked; the
  opaque-identity registry and request log in SQLite; the admin API on the unix
  socket, the TCP admin listener and the UI listener; and
  `janus-daemon check-config`. See <ref target="docs/configuration">daemon
  configuration</ref>.
- **The UI** — the Overview, Credentials, Identities, Requests (with live tail
  and detail drawer) and Plugin views described in
  <ref target="docs/admin-surface#ui">the UI</ref>, behind a token prompt.

Not built:

- **The `janus` CLI.** The binary is a stub that prints "not yet implemented".
  Every command in <ref target="docs/admin-surface#cli">the janus CLI</ref> is
  design; until it exists the admin API is reached with `curl` or the UI. The
  key generation behind `janus plugin tls init` exists as a daemon library
  module, but no command exposes it.

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
- **Plugin endpoint discovery.** A `[plugin]` block in the daemon's config file
  and nothing else; the endpoint URI scheme selects the transport.
  <ref target="docs/plugin-contract#plugin-config">Configuring the plugin</ref>;
  reasoning in <ref target="docs/decisions#plugin-discovery">decisions</ref>.
- **Daemon-to-plugin channel security.** Filesystem permissions on the unix
  socket; mandatory mutual TLS against an operator-named anchor on the network,
  with the daemon refusing a plugin that does not request a client certificate;
  plaintext only by explicit opt-in. No shared-secret mode.
  <ref target="docs/plugin-contract#channel-security">Securing the
  channel</ref>; options and verdicts in
  <ref target="docs/decisions#plugin-channel-auth">decisions</ref>.
- **The metering store.** The `UsageStore` trait is one atomic, idempotent,
  ledgered operation; `janus-core` ships a volatile in-memory store and a
  durable embedded SQLite store, plus a conformance suite. No replicated store
  until a plugin runs replicas.
  <ref target="docs/plugin-contract#usage-stores">The store</ref>; options and
  verdicts in <ref target="docs/decisions#metering-store">decisions</ref>.
- **Policy on an identity-less route.** None. A `method = "none"` route is open;
  what it logs and emits is stated.
  <ref target="docs/caller-identity#binding">Binding an extractor to a request
  type</ref>.
- **The opaque-identity registry.** A daemon-owned SQLite table of hashed,
  daemon-generated tokens with claims, expiry and revocation, managed through
  the admin surface. <ref target="docs/caller-identity#registry">The
  opaque-identity registry</ref>.
- **The `janus` CLI, admin endpoints and UI scope.** One `/v1` HTTP+JSON API —
  status, credentials, identities, requests, plugin — with the CLI as a thin
  client and the UI as a deliberate subset; a metadata-only request log with an
  on/off setting and retention; the admin token on every TCP listener and TLS
  off loopback. <ref target="docs/admin-surface">Admin surface, CLI and
  UI</ref>.
- **Embedded callers of the prior gateway.** Served by the admin endpoints; the
  host's response-bytes tap stays with the host.
  <ref target="docs/scope#inprocess-gap">Scope: embedded callers</ref>.

## Open questions {#open-questions}

Nothing is open. Three things are deliberately not built and have a named
trigger rather than an open decision — a replicated `UsageStore` (the first
plugin that runs replicas), credential entry and rotation in the UI (a user who
needs it), and request/response body capture in the request log (a user who
needs it, and a design of its own) — and each is recorded where it was decided,
in <ref target="docs/decisions">decision records</ref>. When something new
becomes genuinely undecided, it goes here, and an answer inferred from elsewhere
in the corpus is speculation until it does.
