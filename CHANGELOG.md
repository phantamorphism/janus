---
title: Changelog
type: reference
status: accepted
---

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic
Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version
may include breaking changes to configuration, the admin API or the plugin
contract; they are called out under **Changed**.

## [Unreleased]

### Added

- Open-source project files: contribution guide, security policy, code of
  conduct, governance, issue and pull-request templates, code owners, and a
  development guide (`docs/development.md`).
- Crate and UI package metadata: description, license, repository, homepage,
  readme, keywords, categories and a declared minimum supported Rust version
  (1.89).

## [0.1.0] - Unreleased

First tagged version. What is built, as described in `docs/status.md`:

### Added

- **`janus-core`**: the plugin contract (`proto/janus.proto`, with generated
  gRPC client and server), the `JanusPlugin` trait for linked plugins, identity
  extractors (`hmac`, `jwt`, `opaque`, `trusted_header`, `none`, and custom
  methods) with per-upstream and per-route bindings, the `PeerUid` interceptor
  for unix-socket plugins, the credential pool (config credentials and
  directory-per-slot pool credentials, with OAuth refresh), and the metering
  primitives with in-memory and SQLite `UsageStore`s plus a conformance suite.
- **`janus-daemon`**: the data plane for the Anthropic, OpenAI (including
  ChatGPT-account OAuth credentials, served at the Codex backend) and OpenRouter
  upstreams, through the full request lifecycle: identity verification,
  credential resolution, substitution from a per-upstream header allowlist,
  forwarding and unmodified relay of streaming responses. The plugin client over
  a unix socket, mutual TLS, opt-in plaintext, or linked, failing closed. The
  opaque-identity registry and a metadata-only request log in SQLite with
  retention. The admin API (`/v1`) on a unix socket by default, with an opt-in
  token-authenticated TCP listener (TLS off loopback), a separate UI listener,
  and `janus-daemon check-config`.
- **`janus` CLI**: status, credentials, opaque identities, request-log query and
  live tail, plugin status and channel check, TLS key generation for the plugin
  channel, and `check-config`. Connects over the admin socket, or over TCP with
  a token and an optional CA file.
- **UI**: Overview, Credentials, Identities, Requests (live tail and detail
  drawer) and Plugin views, behind an admin-token prompt.
- **Documentation**: the design corpus in `docs/` (checked with Schematic), the
  acceptance deliverables in `evidence/DELIVERABLES.md` with a blind-QA harness
  and recorded results, and independent review reports in `reviews/`.

### Not built

- A replicated `UsageStore`, credential entry and rotation in the UI, and
  request or response body capture in the request log are deliberately absent;
  each has a named trigger in `docs/status.md`.
- Response transformation, request augmentation beyond the credential
  substitution, and non-HTTP transports are out of scope
  (`docs/architecture.md`).

[Unreleased]: https://github.com/phantamorphism/janus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/phantamorphism/janus/releases/tag/v0.1.0
