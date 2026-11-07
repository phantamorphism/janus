---
title: Writing a plugin
type: reference
status: draft
tags: [janus, plugin, guide]
---

# Writing a plugin

A policy plugin is a gRPC server for the `JanusPlugin` service in
`proto/janus.proto`. `janus-daemon` is its client. This guide covers what a
plugin does in each of the two RPCs, how it serves the unix socket, and how an
operator checks it. The design and its reasoning are in
<ref target="docs/plugin-contract">the plugin contract</ref>. A complete, tested
plugin that follows this guide is
<ref target="examples/tenant-pinning-plugin/README">the tenant-pinning example
plugin</ref>. A plugin that uses `Notify` to act on credential events, and the
admin API to rotate material, is
<ref target="examples/claude-login-keeper/README">the Claude login keeper</ref>
(<ref target="docs/claude-login-keeper">reference</ref>).

## Choose an execution model {#model}

- **Unix socket**: a separate process on the daemon's host. Use this model for
  most plugins, in any language. The rest of this guide uses it.
- **Network**: the same server on another host, behind mutual TLS. The service
  code is the same; only the listener is different (see
  <ref target="#network">serving over mutual TLS</ref>).
- **Linked**: a Rust `JanusPlugin` trait implementation compiled into a custom
  build of the daemon. See <ref target="docs/plugin-contract#linked">linked
  plugins are a custom build</ref>.

A Rust plugin depends on `janus-core`. That crate supplies the generated server
stubs (`contract::proto::janus_plugin_server`), the Rust contract types and
their conversions (`contract::plugin`), the typed payloads of the settlement
events, the metering primitives (`metering`) and the peer-uid check (`peer`). A
plugin in another language generates its stubs from `proto/janus.proto`.

## Answer ResolveRoute {#resolve-route}

The daemon calls `ResolveRoute` once for each proxied request that has a
verified identity, before it forwards the request. The plugin must answer within
`timeout_ms` (default 250 ms). Each answer has exactly one outcome:

| Outcome | Use it when |
|---|---|
| `credential_ref` | The caller must use a specific credential. The ref must be a credential of `implementation` that the instance's credential filter admits. Any other ref fails the request as `credential_unavailable`. |
| `use_default` (`true`) | The caller is allowed, and the instance's own filter and strategy select the credential. |
| `deny_reason` | The caller is refused for any reason. The text goes to the caller in a 403 response, so write it for the caller. |

Rules that the daemon enforces on the answer:

- An answer with no outcome set, `use_default = false`, an empty
  `credential_ref` or an empty `deny_reason` is a protocol error. The daemon
  rejects the request as `plugin_error`.
- An RPC error, a timeout or an unreachable plugin rejects the request (fail
  closed). The caller gets a 503 with the failure class. The daemon never falls
  back to its default chain.

Rules for the plugin:

- Make policy decisions from `caller_identity` and `identity_claims` only. Both
  are verified. `correlation_id` can come from the caller, so never use it in a
  decision.
- Use `upstream` (the instance name, such as `claude`) to select a pin. Use
  `implementation` (`anthropic`, `openai`, `openrouter`) to know which
  credentials are valid for a ref. If `implementation` is empty, the daemon is
  older than this field; use `upstream` in its place. `janus-core`'s
  `ResolveRoute::try_from` does this for you.
- Deny a request with an empty `caller_identity`. `ResolveRoute::try_from`
  refuses such a request, so a plugin that decodes with it and denies on error
  obeys this rule.

## Consume Notify {#notify}

`Notify` is one client-streaming call. The daemon opens it and keeps it open
while it runs, and opens it again (with backoff) after it drops. The plugin
reads events until the stream ends, then sends `NotifyAck`. Nothing that the
plugin returns changes how the daemon handles a request.

The daemon emits four event types (see
<ref target="docs/plugin-contract#guaranteed-events">events janus-daemon
guarantees to emit</ref>):

| `event_type` | Sent when | Payload in `janus-core` |
|---|---|---|
| `call_completed` | The upstream sent a response, with any status. | `CallCompleted` |
| `call_abandoned` | The request did not get to the upstream after `ResolveRoute` was sent. This includes a request the plugin denied, which has `stage = "resolve"`. | `CallAbandoned` |
| `credential_invalid` | The upstream rejected the substituted credential. | `credential_ref`, `upstream`, `implementation`, `status` and `reason` |
| `account_error` | The upstream reported a problem with the account. | same as `credential_invalid` |

Ignore an event type that you do not know. Event types are additive.

For each request that `ResolveRoute` was sent for, the daemon emits exactly one
of `call_completed` or `call_abandoned`, with the same `request_id`. A
`credential_invalid` or `account_error` event comes before the `call_completed`
event of the same request.

Delivery is asynchronous and best effort. Events can arrive some time after the
response goes to the caller. An event in the daemon's queue goes to a plugin
that restarts. An event is lost if the queue overflows, or if it was in flight
when the plugin stopped.

## Meter in two phases {#metering}

A budget that `ResolveRoute` enforces uses the
<ref target="docs/plugin-contract#metering-primitives">metering primitives</ref>
in two phases, keyed by `request_id`:

1. In `ResolveRoute`, make the policy decision first. Then `charge` one unit for
   an allowed request. If `charge` returns `Exceeded`, answer `deny_reason`. Do
   not charge a request that policy denies.
2. On `call_completed`, `settle` the charge. `record` the cost from `usage` if
   you meter cost.
3. On `call_abandoned`, `refund` the charge. A refund of a request that was
   never charged (a denial) returns `false` and has no effect. A refund after
   `settle` also has no effect.

The settlement events are asynchronous, so a refund takes effect only after its
`call_abandoned` event arrives. A caller that retries immediately after an
abandoned call can still find its budget at the limit.

`usage` in `call_completed` has the shape of the implementation's response. For
`anthropic` it is
`{"model": ..., "usage": {"input_tokens": N, "output_tokens": N, ...}}`. It is
`null` when the daemon observed nothing.

## Serve the unix socket {#socket}

The daemon walks the socket path before it connects. The checks are in
<ref target="docs/plugin-contract#channel-unix">unix socket</ref>. To pass them:

- Put the socket in a directory of its own, owned by the plugin's user, mode
  `0750`. Make the daemon's user a member of the directory's group if the two
  users are different.
- Make the socket mode `0660`.
- Make sure that no ancestor directory is writable by group or others. Make sure
  that each ancestor is owned by root, by the daemon's user or by the socket
  directory's owner. On macOS `/tmp` is a symlink to `/private/tmp` (mode
  `1777`), so a socket below `/tmp` is refused.
- Before you bind, remove a stale socket from a previous run. After a crash the
  file stays and the daemon gets "connection refused".
- To check the daemon's side of the connection, install
  `janus_core::peer::PeerUid` with the daemon's uid.

A Rust plugin can call `janus_core::peer::serve_unix`, which does all of this:
it creates the directory with mode `0750`, removes a stale socket, sets the
socket mode `0660`, installs `PeerUid` when a uid is given, and stops as
described in <ref target="#shutdown">stop cleanly</ref>. Both example plugins
use it.

Configure the daemon with the absolute path:

```toml
[plugin]
endpoint   = "unix:///srv/janus-policy/run/policy.sock"
timeout_ms = 250
```

The daemon can start before the plugin. Until the plugin is listening, each
request fails closed as `plugin_unreachable`.

### Stop cleanly {#shutdown}

The daemon's `Notify` stream does not close. Thus a graceful shutdown that waits
for every connection to drain (tonic's `serve_with_incoming_shutdown`, for
example) does not complete while the daemon runs. Give in-flight calls a fixed
grace period after the shutdown signal and then exit.
`janus_core::peer::serve_unix` waits `janus_core::peer::SHUTDOWN_GRACE` (2
seconds).

## Serve over mutual TLS {#network}

For the network model, the daemon must verify the plugin and the plugin must
require and verify the daemon's client certificate (see
<ref target="docs/plugin-contract#channel-network">network</ref>). Without a
PKI:

```sh
janus plugin tls init --plugin-host policy.internal --out /etc/janus/plugin-tls
```

This writes `daemon.pem`, `daemon-key.pem`, `plugin.pem` and `plugin-key.pem`,
and prints the `[plugin.tls]` block for the daemon:

```toml
[plugin]
endpoint = "https://policy.internal:7443"

  [plugin.tls]
  pinned_cert = "/etc/janus/plugin-tls/plugin.pem"
  client_cert = "/etc/janus/plugin-tls/daemon.pem"
  client_key  = "/etc/janus/plugin-tls/daemon-key.pem"
```

The plugin serves TLS 1.3 with `plugin.pem` and `plugin-key.pem`, and it accepts
only the client certificate `daemon.pem`. In tonic (with its `tls-ring` feature)
that is
`ServerTlsConfig::new().identity(Identity::from_pem(plugin_pem, plugin_key)).client_ca_root(Certificate::from_pem(daemon_pem))`
on a TCP listener, with the same service. `janus plugin check` then makes sure
that the plugin rejects a throwaway client certificate and a handshake with no
certificate. A server that does not request a client certificate is refused as
`plugin_no_client_auth`.

## Check the channel {#check}

```sh
janus plugin check     # path walk + connection (socket), or three handshakes (TLS)
janus plugin status    # health, last success, last error
janus log query        # outcome per request: forwarded, denied(...), plugin_failed(...)
```

`janus plugin check` exits 1 if a step fails, and it names the directory or
handshake that failed. It does not send a `ResolveRoute`. `janus plugin status`
shows the health from the most recent real call. A passing `check` does not
change that health.
