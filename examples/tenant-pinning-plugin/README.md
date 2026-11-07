---
title: Tenant-pinning example plugin
type: reference
status: draft
tags: [janus, plugin, example]
---

# Tenant-pinning example plugin

`tenant-pinning-plugin` is a small policy plugin for Janus that you can copy. It
serves the `JanusPlugin` gRPC service on a unix socket and does three things:

1. **Pins by claim.** A caller whose verified `tenant` claim is listed in the
   policy file gets that tenant's credential on each upstream instance that the
   tenant has a pin for. For example, tenant `acme` gets `claude-acme` on
   instance `claude`. Every other caller gets `use_default`.
2. **Denies.** A caller with the claim `tier=blocked` is denied. When
   `deny_unlisted = true`, a caller with no tenant claim, or with a tenant that
   is not in the policy file, is also denied. Each identity has a request
   budget, and a request over that budget is denied. Each denial gives a reason,
   and the caller gets that reason in a 403 response.
3. **Settles.** An allowed request is charged in `ResolveRoute`. The charge is
   settled on `call_completed` and refunded on `call_abandoned`. The plugin
   writes one JSON log line for each `call_completed`, `call_abandoned`,
   `credential_invalid` and `account_error` event, with the tokens observed in
   `call_completed` `usage`. It ignores all other event types.

The general guide is <ref target="docs/writing-a-plugin">writing a plugin</ref>.
The contract is <ref target="docs/plugin-contract">the plugin contract</ref>. A
recorded run against a real daemon is in
<ref target="examples/tenant-pinning-plugin/VERIFICATION">the verification
transcript</ref>.

## Files {#files}

| File | Contents |
|---|---|
| `src/policy.rs` | The policy file and the routing decision. Has no I/O, so you can test it with unit tests. |
| `src/service.rs` | `ResolveRoute` and `Notify`: the decision, then the budget, as the `JanusPlugin` service. |
| `src/lib.rs` | `serve`: binds the socket with the modes that the daemon requires, then serves until shutdown (`janus_core::peer::serve_unix`). |
| `src/main.rs` | The command line. |
| `tests/socket.rs` | Serves the plugin on a real socket and calls it with the gRPC client that is generated from `proto/janus.proto`. |
| `policy.example.toml` | An annotated policy file. |

The crate is a member of the Janus workspace. It depends on `janus-core` for the
generated stubs, the contract types, the metering primitives and the peer-uid
check, and it adds no third-party crate that the workspace does not already use.
Thus the workspace's MSRV, `cargo deny` and clippy gates also apply to this
crate. To use it outside this repository, copy the directory and change the
`.workspace = true` dependencies to versions or paths. The crate is unix-only,
because it serves a unix socket.

## Build and test {#build}

From the repository root (`protoc` must be on `PATH`, as for the rest of the
workspace):

```sh
cargo build -p tenant-pinning-plugin             # target/debug/tenant-pinning-plugin
cargo test  -p tenant-pinning-plugin             # policy unit tests + socket integration test
cargo clippy -p tenant-pinning-plugin --all-targets --all-features -- -D warnings
```

The CI commands for the full workspace (`cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo test --workspace --all-features`, and `cargo check --workspace --locked`
on Rust 1.89.0) also build and test this crate.

## The policy file {#policy}

```toml
deny_unlisted = false            # true: deny a missing or unlisted tenant

[budget]                         # per caller identity
requests    = 5
window_secs = 60                 # fixed window: resets on each boundary

[tenants.acme]
pin = { claude = "claude-acme" } # instance name -> credential ref

[tenants.globex]
pin = { claude = "claude-globex" }
```

The plugin reads the file once, at startup. It refuses unknown keys, a zero
budget and an empty ref. To change the policy, restart the plugin. The daemon
fails requests closed while the plugin is down and connects again by itself.

The decision has this order: `tier=blocked`, then the tenant, then the budget.
Requests denied by policy do not use budget. The ref that a pin names must be a
credential of the instance's implementation, and the instance's credential
filter must admit it. If not, the daemon fails the request as
`credential_unavailable`.

## Run it against a daemon {#run}

The daemon refuses a socket whose directory, or any ancestor, is writable by
group or others, or is owned by a user other than root, the daemon's user or the
socket directory's owner. Use a directory of its own, outside `/tmp`. If the
directory does not exist, the plugin creates it with mode `0750`, and it makes
the socket mode `0660`:

```sh
tenant-pinning-plugin \
  --policy /srv/janus-policy/policy.toml \
  --socket /srv/janus-policy/run/policy.sock \
  --daemon-uid "$(id -u janus)"        # optional: refuse callers that are not the daemon
```

The daemon's configuration for this plugin, with one instance and four API-key
credentials:

```toml
[identity]
method = "opaque"
source = { header = "x-api-key" }

[[credential]]
ref = "claude-shared"                  # first in chain order: the default
upstream = "anthropic"
env = "CLAUDE_SHARED_KEY"
tags = ["claude"]

[[credential]]
ref = "claude-acme"
upstream = "anthropic"
env = "CLAUDE_ACME_KEY"
tags = ["claude"]

[[credential]]
ref = "claude-globex"
upstream = "anthropic"
env = "CLAUDE_GLOBEX_KEY"
tags = ["claude"]

[[upstream]]
name           = "claude"
implementation = "anthropic"
path_prefix    = "/claude"
base_url       = "https://api.anthropic.com"
  [upstream.credentials]
  tags = ["claude"]

[plugin]
endpoint   = "unix:///srv/janus-policy/run/policy.sock"
timeout_ms = 250
```

Give callers identities that have the claims the policy reads, then check the
channel:

```sh
janus identity create --identity acme-agent --claim tenant=acme
janus identity create --identity contractor-7 --claim tenant=acme --claim tier=blocked
janus plugin check
```

The plugin logs to stderr as JSON lines. `RUST_LOG` sets the level, and
`RUST_LOG=tenant_pinning_plugin=debug` also logs the ignored event types.
SIGTERM or Ctrl-C stops the plugin after a grace period of at most 2 seconds and
removes the socket.

To serve the plugin over mutual TLS instead of a unix socket, see
<ref target="docs/writing-a-plugin#network">serving over mutual TLS</ref>. Only
the listener in `serve` changes.

## Limits {#limits}

- **A fixed window, not a rolling one.** The budget uses the `janus-core`
  `Meter` with a `Window::Fixed` quota. Counts reset at each `window_secs`
  boundary, so a caller can make up to twice `requests` calls across one
  boundary. The primitives do not supply a rolling window.
- **Counts are in memory.** The budget uses `MemoryUsageStore`, so a restart
  resets it. Use `SqliteUsageStore` (the `janus-core` feature `sqlite`) for a
  budget that survives a restart.
- **Refunds arrive asynchronously.** A refund takes effect only after its
  `call_abandoned` event arrives. A charge whose settlement event is lost is
  never refunded. See
  <ref target="docs/plugin-contract#guaranteed-events">events janus-daemon
  guarantees to emit</ref>.
- **The token count is for logs only.** The plugin adds up the tokens in `usage`
  (Anthropic `input_tokens` + `output_tokens`, or OpenAI `prompt_tokens` +
  `completion_tokens`) for each identity, and writes the total to its log. It
  does not enforce a limit on tokens. To enforce one, `record` the tokens
  against a second meter and `check` that meter in `ResolveRoute`, as
  <ref target="docs/plugin-contract#two-phase-metering">charging before the cost
  is known</ref> describes.
