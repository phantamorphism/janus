---
title: Plugin contract
type: decision
status: accepted
tags: [janus, plugin, grpc]
---

# Plugin contract

Policy is not hardcoded into Janus. The decisions that depend on an operator's
own rules — which credential a given caller may have substituted in, whether
that caller is still within its budget, whether the call is allowed at all — are
delegated to a plugin over a contract defined in `proto/janus.proto`.
`janus-daemon` is always the *client* of this contract; the plugin always
implements the server side — see
<ref target="docs/architecture#janus-daemon">janus-daemon</ref>.

The contract defines exactly two operations, `ResolveRoute` and `Notify`, and
they are shaped very differently from each other. That's deliberate: one is on
the critical path of every proxied request and its answer changes what Janus
does; the other is an after-the-fact announcement that changes nothing. The
proto mirrors that rather than forcing both into one shape for consistency's
sake.

## ResolveRoute: the substitution decision {#resolve-route}

```protobuf
rpc ResolveRoute(ResolveRouteRequest) returns (ResolveRouteResponse);

message ResolveRouteRequest {
  string request_id = 1;      // Janus-assigned, unique to this request
  string upstream = 2;        // "anthropic" | "openai" | "openrouter"
  string caller_identity = 3; // the verified identity that is asking
  string correlation_id = 4;  // optional grouping hint; may be untrusted
  map<string, string> identity_claims = 5; // other verified claims, if any
}

message ResolveRouteResponse {
  oneof outcome {
    string credential_ref = 1; // substitute this credential
    bool use_default = 2;      // defer to janus-daemon's default chain
    string deny_reason = 3;    // refuse the request
  }
}
```

This is the call that decides what
<ref target="docs/credential-substitution#substitution">the substitution</ref>
substitutes. It is consulted after Janus has verified the caller's identity and
before the request is forwarded upstream — step 3 of
<ref target="docs/architecture#request-lifecycle">the request lifecycle</ref> —
and only when a plugin is actually configured.

The request carries **who** is asking and **what** they want to reach, plus two
pieces of bookkeeping:

- `caller_identity` is the verified output of
  <ref target="docs/caller-identity#extractor">identity extraction</ref>. A
  plugin never authenticates the caller itself, and should treat the value as an
  opaque key rather than a structure to parse.
- `identity_claims` carries the other claims extraction verified alongside the
  identity — an agent-type or tenant marker, for instance. It is the supported
  way to make a decision on more than the bare id, and exists precisely so that
  "treat `caller_identity` as opaque" is advice a plugin can actually follow.
- `upstream` names the destination. New upstreams are new values, not a proto
  change.
- `correlation_id` is a grouping hint and may be empty. Depending on the
  deployment's binding it may have come from a verified claim or from an
  unverified caller-supplied header, so it must never key a policy decision —
  see <ref target="docs/caller-identity#correlation">correlation values are not
  identities</ref>.
- `request_id` is assigned by Janus, unique per proxied request, and never
  caller-supplied. Its job is to make the decision made here matchable to the
  `Notify` events that follow it, which is what
  <ref target="docs/plugin-contract#metering-primitives">post-call
  accounting</ref> is built on.

The three response outcomes cover the three things policy can usefully say:

- `credential_ref` — use this specific credential. The reference is opaque to
  `janus-daemon`; the plugin and the credential pool agree on what it names.
  This is how per-caller account separation works: two identities calling the
  same upstream can be routed to different credentials.
- `use_default` — this caller is fine, Janus should pick from its own default
  chain. The explicit "yes, and I don't care which" answer, distinct from a
  denial and from naming a credential.
- `deny_reason` — refuse the request. This is the single policy-enforcement
  hook, and deliberately not subdivided: a revoked identity, a caller not
  authorized for this upstream, and an exhausted credit or rate budget all come
  back as a denial with a reason. `janus-daemon` does not interpret the
  difference; the request fails and the reason reaches the caller.

`ResolveRoute` is blocking by design. Every proxied request pays for this round
trip when a plugin is enabled, which is why the execution model matters — see
<ref target="docs/plugin-contract#transports">execution and transport
models</ref>.

The default failure posture is **fail-closed**: if the plugin is unreachable,
times out, or returns an error, the request is rejected rather than silently
falling back to a default credential. This was a deliberate choice, not an
oversight. A proxy whose whole purpose is to control which credential gets used
must not resolve an outage by guessing — acting as, or billing, the wrong
account silently is worse than a failed request the caller can see and retry.

## Notify: asynchronous, generic envelope {#notify}

```protobuf
rpc Notify(stream NotifyEvent) returns (NotifyAck);

message NotifyEvent {
  string event_type = 1;     // freeform, extensible
  string request_id = 2;     // the ResolveRoute this is about; may be empty
  string correlation_id = 3;
  string payload_json = 4;   // event-specific data, kept loose on purpose
}

message NotifyAck {}
```

`Notify` is fire-and-forget. `janus-daemon` never blocks on it and barely
inspects the response — it's a "something happened, tell whoever's listening"
channel, not an exchange whose result affects proxy behavior. It is how the
observe behavior in <ref target="docs/architecture#proxy-behaviors">proxy
behaviors</ref> reaches anything outside the daemon, and because it is off the
critical path, a slow or absent listener cannot stall a caller's response.

It uses one generic envelope instead of one RPC per event kind, specifically so
that new event types don't require touching `janus.proto` again. A plugin
consumes the event types it cares about and ignores the rest; neither side needs
the other to agree on a fixed list.

### Events janus-daemon guarantees to emit {#guaranteed-events}

"Freeform" applies to the envelope, not to the daemon's own behavior. Four event
types are emitted by `janus-daemon` itself, with `payload_json` shapes a plugin
can rely on. They are the ones a policy implementation cannot discover for
itself, because they describe what happened *after* `ResolveRoute` answered:

- **`call_completed`** — the request reached the upstream and a response came
  back. Payload: `caller_identity` (the same value `ResolveRoute` was given),
  `upstream`, `status`, `credential_ref` (what was actually used, including when
  the plugin said `use_default`), `duration_ms`, and `usage` — whatever the
  per-upstream implementation was able to observe about what the call consumed.
  `usage` is the "what in a response is worth observing" slot from
  <ref target="docs/credential-substitution#general-purpose"> not specific to
  LLM providers</ref>, and is the only route by which a real cost figure reaches
  a plugin.
- **`call_abandoned`** — `ResolveRoute` answered, and the call then never
  reached the upstream. Payload: `caller_identity`, `stage` (which step of
  <ref target="docs/architecture#request-lifecycle">the lifecycle</ref> it died
  at) and `reason`. This is the event that makes a reservation refundable; see
  <ref target="docs/plugin-contract#metering-primitives">metering
  primitives</ref>. It is also emitted, with `stage` naming the resolve step,
  when `ResolveRoute` was sent but failed — timed out, errored, or the plugin
  became unreachable mid-call — because the plugin may have charged before the
  daemon gave up on its answer, and a refund of a charge that was never made is
  a no-op.
- **`credential_invalid`** — the upstream rejected the credential Janus
  substituted in.
- **`account_error`** — the upstream reported a problem with the account behind
  the credential rather than with the request.

Both of the first two carry the `request_id` of the `ResolveRoute` they follow.
Exactly one of them is emitted for every request `ResolveRoute` was sent for, so
a plugin that charges a budget on the way in is always sent a matching
settlement event on the way out.

`caller_identity` is repeated on the two settlement events so that a plugin can
settle a charge without holding a `request_id → identity` map in memory — a
plugin that restarted between `ResolveRoute` and `call_completed` can still
account for the call when the event reaches it after the restart. Delivery is
best-effort (see <ref target="docs/plugin-contract#plugin-client">the daemon
codes against one client interface</ref>): an event lost while the plugin was
unreachable leaves its phase-1 charge in place (never refunded) and its phase-2
cost unrecorded, which is the residual inaccuracy of a budget whose settlement
rides an asynchronous channel. This is an accepted limitation, tracked as [issue
#1](https://github.com/phantamorphism/janus/issues/1).

## Execution and transport models {#transports}

The same logical contract is used no matter where the plugin runs. What changes
is how far the call has to travel, and that is a deployment choice based on the
latency an operator can afford and the isolation they want:

- **Linked (in-process).** The plugin is compiled into the daemon binary. No
  socket, no serialization, no round trip — a direct Rust call. Fastest, and the
  weakest isolation: the plugin shares an address space with the process holding
  real credential material. See <ref target="docs/plugin-contract#linked">linked
  plugins</ref>.
- **Local socket.** The plugin is a separate process on the same host, reached
  over gRPC on a unix domain socket. Real process isolation and no network
  exposure, at the cost of serialization and a context switch per request. The
  middle option, and the one that keeps the plugin's failure domain separate
  from the daemon's without leaving the machine.
- **Network.** The plugin is a gRPC service on another host. The most
  operationally flexible — one policy service can serve many daemons, and it can
  be deployed and scaled independently — but it puts a network round trip on the
  critical path of every proxied request, and the channel has to be secured in
  both directions — see <ref target="docs/plugin-contract#channel-security">
  securing the channel</ref>.

Which model is in use is a matter of the daemon's configuration — see
<ref target="docs/plugin-contract#plugin-config">configuring the plugin</ref>.

The fail-closed posture above is not a remote-only concern: a linked plugin that
panics and a local socket that is not listening are handled the same way as an
unreachable network service.

gRPC is the wire shape for the two out-of-process models because the contract
has to be implementable out-of-process and in any language while staying cheap
enough to sit on the synchronous path of every request: a schema-defined binary
protocol with a single multiplexed connection and streaming support is the
cheapest thing that satisfies both.

## Linked plugins are a custom build {#linked}

A linked plugin is **compiled in**, not loaded. There is no dynamic-loading
mechanism, no shared-object plugin ABI, and no plugin-versioning problem across
a dylib boundary — none of which Janus will grow, because the alternative is
cheaper: an operator who wants a plugin in-process implements a Rust trait and
builds their own `janus-daemon`.

The trait mirrors the proto one-to-one:

```rust
#[async_trait]
pub trait JanusPlugin: Send + Sync + 'static {
    async fn resolve_route(&self, req: ResolveRoute)
        -> Result<RouteOutcome, PluginError>;

    async fn notify(&self, event: NotifyEvent);
}

pub enum RouteOutcome {
    Credential(String),      // credential_ref
    UseDefault,
    Deny { reason: String },
}
```

Two shape differences from the proto are worth naming, because they are where
the correspondence is deliberately not literal:

- `RouteOutcome` is a Rust enum rather than a `oneof`. A `oneof` is
  representable-but-unset on the wire, so the gRPC path has to treat "no outcome
  set" as a protocol error; the enum makes that case unconstructible for a
  linked plugin.
- `notify` takes one event and returns nothing. `Notify` is a client-streaming
  RPC because batching many events onto one connection is a transport concern —
  so it is the gRPC client's concern, not the contract's. A linked plugin is
  simply called once per event, and neither form can fail in a way the daemon
  acts on.

### The daemon codes against one client interface {#plugin-client}

So that the three execution models are genuinely interchangeable from the
daemon's side, nothing in `janus-daemon` outside its construction path knows
which one is in use. Everything calls:

```rust
#[async_trait]
pub trait PluginClient: Send + Sync {
    async fn resolve_route(&self, req: ResolveRoute)
        -> Result<RouteOutcome, PluginError>;

    /// Hand off an event. Returns immediately; delivery is best-effort and a
    /// failure is logged, never surfaced to the request path.
    fn notify(&self, event: NotifyEvent);
}
```

There are exactly two implementations:

- **`GrpcPluginClient`** — a thin `tonic` client. It serves *both* the local
  socket and the network model, because those differ only in the endpoint the
  channel is built from. It owns the `Notify` stream, reconnection, and the
  request timeout that turns a hung plugin into a fail-closed denial.
- **`LinkedPluginClient<P: JanusPlugin>`** — holds the compiled-in
  implementation and calls it directly. No proto types are constructed, nothing
  is serialized. It wraps each `resolve_route` call so that a panic in the
  plugin becomes a `PluginError` — which the fail-closed path turns into a
  denied request — rather than unwinding into the request task. That is a
  guardrail, not isolation: a linked plugin can still corrupt shared state or
  read credential material, which is the cost of the address space it was given.

The pass-through mode is deliberately **not** a third implementation. With no
plugin configured the daemon holds `None` rather than a client that answers
`UseDefault` locally, and its resolve step reads:

```rust
match &self.plugin {
    None => RouteOutcome::UseDefault,
    Some(client) => client.resolve_route(req).await?,
}
```

A sentinel implementation would work, but it would make
<ref target="docs/plugin-contract#no-plugin-pass-through">"zero calls defined in
`janus.proto`"</ref> a property you have to read an implementation to confirm.
As an `Option`, it is a property you can see.

### What a custom build looks like {#custom-build}

`janus-daemon` is a library plus a thin binary. The stock binary passes nothing:

```rust
// crates/janus-daemon/src/main.rs — the stock binary
janus_daemon::run(config, Embedded::default()).await
```

A custom build is a separate crate that depends on `janus-daemon` and on its own
policy crate, and passes implementations in:

```rust
pub struct Embedded {
    pub plugin: Option<Arc<dyn JanusPlugin>>,
    pub extractors: Vec<Arc<dyn IdentityExtractor>>,
}

// a custom build's main.rs
janus_daemon::run(config, Embedded {
    plugin: Some(Arc::new(MyPolicy::new(&config)?)),
    extractors: vec![],
}).await
```

One struct rather than a widening argument list, so that the next thing a custom
build can supply — a <ref target="docs/caller-identity#custom">custom identity
extractor</ref> is already the second — doesn't change the signature.
`Embedded::default()` is empty, which is what makes the stock binary's behavior
obvious: it cannot have a plugin linked.

Config and linkage have to agree, and disagreement is a **startup failure**, not
a silent degradation:

- config selects the linked model but `Embedded::plugin` is `None` → refuse to
  start. The operator asked for policy; starting as an open pass-through proxy
  instead is the worst possible interpretation.
- config names a gRPC endpoint *and* a plugin is linked → refuse to start.
  Ambiguous, and guessing either way surprises someone.
- a plugin is linked but config has no `[plugin]` block → refuse to start. A
  build that carries policy does not run without it because a config file forgot
  to say so.

Build tooling beyond this — whether the custom crate lives in the Janus
workspace, how it pins a Janus version — is an operator's own packaging choice
and not something Janus specifies.

## Configuring the plugin {#plugin-config}

The daemon learns about its plugin from one place: a `[plugin]` block in its
configuration file. There is no environment variable and no command-line flag
for any individual plugin setting (the config file's own path is the only thing
the command line supplies). Upstreams, identity bindings and listeners are
already config-file concerns validated together at startup, and the contract
requires config and linkage to agree; a second source of plugin settings would
add nothing but a way for them to disagree. A container that needs to inject the
endpoint templates the file, as it already must for the rest.

```toml
# A plugin on a local socket.
[plugin]
endpoint   = "unix:///run/janus/policy.sock"
timeout_ms = 250                               # ResolveRoute deadline; default
```

```toml
# A plugin across the network, over mutual TLS. See "Securing the channel".
[plugin]
endpoint = "https://policy.internal:7443"

  [plugin.tls]
  ca_file     = "/etc/janus/plugin-ca.pem"     # exactly one of ca_file / pinned_cert
  client_cert = "/etc/janus/daemon.pem"
  client_key  = "/etc/janus/daemon-key.pem"    # must be mode 0600
  # server_name  = "policy.internal"           # SNI / hostname override
  # expected_san = "spiffe://example.org/policy"  # replaces the hostname check
```

```toml
# A custom build with a linked plugin says so, and names no endpoint.
[plugin]
linked = true
```

The rules:

- **No `[plugin]` block means pass-through.** The daemon holds `None` for its
  plugin client and makes zero calls defined in `janus.proto`.
- **Exactly one of `endpoint` or `linked = true`.** Both, or neither inside a
  present block, is a startup failure.
- **The `endpoint` scheme selects the transport.** `unix:///absolute/path` (no
  host) is the local-socket model; `https://` is the network model with mutual
  TLS and requires `[plugin.tls]`; `http://` is refused unless
  `allow_plaintext = true` is set alongside it. No other scheme is accepted.
  `[plugin.tls]` belongs to `https://` only, and `allow_plaintext` to `http://`
  only.
- **`timeout_ms`** (default 250, never 0) is the deadline on each
  `ResolveRoute`, linked plugins included. A plugin that does not answer within
  it has, for that request, failed closed. `Notify` has no deadline because
  nothing waits on it.

Over gRPC, every dial and TLS handshake is bounded by a 3-second connect
timeout, `plugin check` probes included. Connections carry HTTP/2 keepalive
pings, idle or not, so a plugin that vanishes is detected within about ten
seconds and redialed rather than leaving requests to time out one by one.
`Notify` runs on its own connection, so a backed-up event stream never delays a
`ResolveRoute`; a dropped stream is reopened with backoff (100 ms rising to 5
s). Events wait in a bounded queue of 1024; when it is full, new events are
dropped rather than blocking the request path, counted in the plugin's
`dropped_events` on the admin surface, and logged as a warning.

Configuration is validated at startup — scheme, that every named file exists and
parses, that the client key matches the client certificate and the certificate
is not expired, that the key file is mode `0600`, `ca_file`/`pinned_cert`
exclusivity, and the socket-path hygiene described below — and a daemon that
fails any of it does not start. Reachability is deliberately *not* checked at
startup, because a plugin is allowed to come up after the daemon does; the
consequence of an unreachable plugin is per-request and fail-closed. Each such
failure is classified as `plugin_unreachable`, `plugin_timeout`,
`plugin_tls_error`, `plugin_no_client_auth`, `plugin_rejected_client_cert` or
`plugin_error` (an RPC error from a plugin that was reached), and the class
appears both on the request-log record (`outcome = plugin_failed`) and as the
plugin's `health` on the admin surface — see
<ref target="docs/admin-surface#status-endpoint"> status</ref>.

## Securing the channel {#channel-security}

A plugin's `ResolveRoute` answer selects which real credential a call is billed
to, and its `Notify` stream is what budget enforcement settles against. Both
directions of the channel therefore have to be authenticated: the daemon must be
talking to the real plugin, and the plugin must be sure the events and questions
are coming from the real daemon. How that is achieved differs per transport, and
the decision and the alternatives it was weighed against are recorded in
<ref target="docs/decisions#plugin-channel-auth">the decision record</ref>.

### Linked {#channel-linked}

There is no channel. The plugin is a function call inside the daemon's process.

### Unix socket {#channel-unix}

Filesystem permissions are the boundary, exactly as they are for the daemon's
own admin socket (<ref target="docs/architecture#admin-surface">the admin
surface is a separate listener</ref>). No application-layer secret is layered on
top, because on a correctly permissioned socket it would authenticate nothing
the kernel has not already.

The plugin binds its socket mode `0660` inside a directory owned by the plugin's
user with mode `0750`, and the daemon's user is in that directory's group. The
daemon resolves the socket's path one component at a time and checks every
directory a name is looked up in — including the directory holding each symlink
it follows, up to 40 — rejecting the path if any of them is writable by group or
others, or is owned by anyone other than root, the daemon's user, or the owner
of the socket's own directory. This is the same posture as `sshd`'s
`StrictModes`, for the same reason: an attacker who owns or can write to a
parent directory, or to the directory a symlink points through, can substitute a
socket of their own. It does this at startup over the part of the path that
exists, refusing to start on a failure; while the socket's directory does not
exist yet, every existing ancestor must be owned by root or the daemon's user.
It walks the full path again before every connection and reconnection, because
the plugin may create its directory after the daemon starts and a path that was
sound at startup can be changed later. After connecting, the daemon reads the
uid of the process serving the socket, which must be in the same owner set. A
failure of either fails the request closed as `plugin_unreachable`, with the
offending directory or uid in `last_error`.

A plugin that wants to verify the daemon's side reads the connecting peer's uid
from the socket (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS). For Rust
plugins `janus-core` ships `peer::PeerUid`, a `tonic` interceptor installed with
`JanusPluginServer::with_interceptor(plugin, PeerUid::new(daemon_uid))`. It
fails closed: a call with no peer credentials, or from any other uid, is refused
with `PERMISSION_DENIED`. In any other language's gRPC stack the check is a few
lines.

### Network {#channel-network}

Mutual TLS, and nothing else. The daemon:

- **verifies the plugin** against exactly one operator-named trust anchor —
  `ca_file`, a private CA, or `pinned_cert`, the plugin's own certificate.
  System roots are never consulted: a public CA will issue a certificate for a
  name to whoever controls that name's DNS, and that is not the authority an
  operator means when they say "my policy service". The plugin's identity is
  checked against the endpoint's hostname, or `server_name` when set; a
  deployment whose certificates carry a URI SAN rather than a hostname (SPIFFE)
  sets `expected_san` instead, and that exact SAN replaces the hostname check.
- **presents a client certificate** (`client_cert`, `client_key`) so the plugin
  can verify the daemon in turn. That verification is the plugin's — it is
  configuration of the author's gRPC server, not Janus-specific code, which is
  why this scheme costs a plugin author in any language nothing to implement.
- **refuses a plugin that did not ask.** A TLS server that does not request a
  client certificate is visible in the handshake, and the daemon treats it as a
  misconfigured plugin: the request fails closed with class
  `plugin_no_client_auth`. This is the one part of the plugin's side Janus can
  enforce, and it is enforced so that a plugin author who forgot cannot be
  silently accepting forged `Notify` events from anything that can reach their
  port.

TLS 1.3 only. Certificate and key files are re-read when they change on disk and
on `SIGHUP`, with the last good material kept if the new files fail to parse;
both certificates' expiry is reported on the admin surface and a warning is
logged when either is within 30 days of it.

**Plaintext is an explicit opt-in**, `allow_plaintext = true` next to an
`http://` endpoint, for the single topology where it is correct: a service mesh
that already authenticates and encrypts both ends of the hop, where TLS inside
TLS buys nothing. The daemon logs a prominent warning at startup when it is set.
There is no loopback exception — a plaintext TCP port on the same host is not
equivalent to a unix socket, because whichever local process bound the port
first owns it; the same-host answer is the socket. The design of this flag
follows `admin.allow_non_loopback` and the `trusted_header` method: correct in
exactly one deployment, named loudly in config, never reached by default.

### Bootstrapping without a PKI {#channel-bootstrap}

An operator running one daemon and one plugin should not have to stand up a
certificate authority.
`janus plugin tls init --plugin-host <name> [--out <dir>] [--days N]` generates
two self-signed key pairs — one for the daemon, one for the plugin, the plugin's
carrying `<name>` as its SAN — writes `daemon.pem`, `daemon-key.pem` (mode
`0600`), `plugin.pem` and `plugin-key.pem`, and prints the `[plugin.tls]` block
that pins `plugin.pem` plus the two facts the plugin's own configuration needs:
serve with `plugin.pem`/`plugin-key.pem`, and accept only clients whose
certificate is `daemon.pem`. No CA, no chain; each side pins the other. Rotating
means running it again and restarting both. This is the same stance as the admin
token the daemon generates for itself — a channel credential Janus can mint
because it is Janus's own, unlike a caller identity.

### Verifying the channel {#channel-check}

`janus plugin check` (`POST /v1/plugin/check` on the admin surface) tests the
channel actively rather than waiting for traffic to. On the network transport it
performs a full handshake with the configured material, then two that the plugin
must *reject*: one presenting a throwaway self-signed client certificate, and
one presenting none. A plugin that accepts the first has client verification
misconfigured; a plugin that accepts the second requests a certificate without
requiring one, which the passive check during traffic cannot tell apart from
correct configuration. The check says which. Under TLS 1.3 a server's rejection
of a client certificate arrives after the client's side of the handshake has
finished, so each probe reads from the connection to observe it, and none of
them issues a `ResolveRoute`. On the socket transport it reports the
path-permission walk and a connection attempt, on the plaintext transport a
connection attempt; a linked plugin has no channel to check. It is the
operator's answer to "is this actually secured, or does it just work?"

## Metering primitives {#metering-primitives}

Deciding whether a caller is over budget is **plugin-owned policy**: it comes
back as a `deny_reason` like any other refusal, and Janus neither sets limits
nor knows what a limit means. But counting is the same mechanical problem for
every plugin that does it, so `janus-core` ships primitives for it rather than
letting each plugin author reinvent them.

These are a **library a plugin links**, not a service the daemon exposes. A
plugin running over a local socket or across the network depends on `janus-core`
and the counters live in the plugin's own process, where its policy already
lives. Janus supplies the counting code; it does not do the counting.

```rust
/// What a plugin calls from its ResolveRoute and Notify handlers.
pub struct Meter<S: UsageStore> { /* store + configured quotas */ }

impl<S: UsageStore> Meter<S> {
    /// Refuses a `Window::Total` quota on a store whose `durability()` is
    /// `Volatile`: a budget specified as "never resets" cannot live in memory
    /// that does. A quota whose period is not a positive whole number of
    /// seconds is `UsageError::InvalidQuota`.
    pub fn new(store: S, quotas: Vec<(String, Quota)>) -> Result<Self, UsageError>;

    /// As `new`, but accepts a `Total` quota on a volatile store: the explicit
    /// opt-in for tests and for plugins that accept the reset.
    pub fn new_allowing_volatile_total(store: S, quotas: Vec<(String, Quota)>)
        -> Result<Self, UsageError>;

    /// Phase 1. Check and increment in one step against every quota
    /// configured on `meter`, so two concurrent requests from one identity
    /// cannot both pass a limit with one unit left. `Exceeded` leaves every
    /// counter where it was: a denied request consumed nothing upstream, so
    /// charging for it would inflate a `Fixed` window and permanently wedge
    /// a `Total` one. Idempotent per `request_id`.
    pub async fn charge(&self, request_id: &str, identity: &str, meter: &str,
                        amount: u64) -> Result<Charge, UsageError>;

    /// Read-only: `Exceeded` if the identity is already at or over any quota
    /// on `meter`. For dimensions whose figure is only known after the call.
    pub async fn check(&self, identity: &str, meter: &str)
        -> Result<Charge, UsageError>;

    /// Phase 2. Add an amount unconditionally — the cost already happened
    /// and must be counted even when it crosses the limit. Idempotent per
    /// `request_id`.
    pub async fn record(&self, request_id: &str, identity: &str, meter: &str,
                        amount: u64) -> Result<(), UsageError>;

    /// Reverse the `charge` made under `request_id`, in whichever window it
    /// actually landed. `false` if there was nothing to reverse.
    pub async fn refund(&self, request_id: &str, identity: &str, meter: &str)
        -> Result<bool, UsageError>;

    /// Declare the `charge` under `request_id` final, so the store may
    /// forget how to reverse it.
    pub async fn settle(&self, request_id: &str, identity: &str, meter: &str)
        -> Result<(), UsageError>;
}

pub enum Charge {
    Allowed  { used: u64, limit: u64 },   // of the quota nearest exhaustion
    Exceeded { used: u64, limit: u64 },   // of the quota that was exceeded
}

pub struct Quota { pub limit: u64, pub window: Window }
pub enum Window {
    Fixed { period: Duration }, // resets on a period boundary
    Total,                      // never resets
}
```

`charge` or `check` returning `Exceeded` is the signal to answer `deny_reason`;
`Allowed` leaves the plugin free to answer `credential_ref` or `use_default` as
it would have anyway. A plugin can hold several meters per identity —
`"requests"`, `"input_tokens"`, whatever it names — each with one or more quotas
(a per-minute and a per-day limit on the same meter, say), and the quota
configuration is the plugin's, not Janus's.

Every operation that changes a count takes the `request_id` from the contract.
That is what makes the primitives safe against the two things that go wrong in
practice: a `Notify` event delivered twice is counted once, and a refund lands
in the window the charge was made in rather than the window the clock is in now.

### The store {#usage-stores}

Storage is behind a trait so the primitive doesn't dictate where counts live.
The trait is shaped around one atomic, idempotent, ledgered operation rather
than separate add/get/subtract calls, because a check-and-increment composed
from separate calls is only atomic under a single in-process lock, and the trait
exists precisely so that counts can live somewhere else:

```rust
#[async_trait]
pub trait UsageStore: Send + Sync {
    /// Add `amount` to every key in `bounds`, all or nothing, provided no
    /// bound with a `limit` would exceed it. All bounds in one call share one
    /// (identity, meter) and differ only in window, so a store can keep them
    /// in one partition. Idempotent on `op`: a repeat returns the original
    /// result and changes nothing. The store remembers which keys `op`
    /// touched so that `release` can undo exactly those.
    async fn apply(&self, op: &OpId, bounds: &[Bound<'_>], amount: u64)
        -> Result<Apply, UsageError>;

    /// Undo `op` against the keys it touched. `false` if `op` is unknown,
    /// already released, or already settled.
    async fn release(&self, op: &OpId) -> Result<bool, UsageError>;

    /// `op` is final; the store may drop its ledger entry.
    async fn settle(&self, op: &OpId) -> Result<(), UsageError>;

    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError>;

    /// Drop keys whose `retain_until` has passed and ledger entries older
    /// than the ledger TTL. Returns how many were dropped.
    async fn compact(&self, now: i64) -> Result<u64, UsageError>;

    fn durability(&self) -> Durability;
}

pub struct MeterKey {
    pub identity: String,    // caller_identity
    pub meter: String,       // the plugin's own dimension name
    pub period_secs: u64,    // the Window's length in seconds; 0 for Total
    pub window_start: i64,   // derived from the Quota's Window; 0 for Total
}

pub struct Bound<'a> {
    pub key: &'a MeterKey,
    pub limit: Option<u64>,         // None: unconditional (a phase-2 record)
    pub retain_until: Option<i64>,  // None: keep forever (a Total window)
}

pub struct OpId {
    pub identity: String,
    pub meter: String,
    pub request_id: String,
    pub phase: Phase,               // Reserve (charge) or Settle (record)
}
pub enum Phase { Reserve, Settle }

pub enum Apply {
    Applied  { used: Vec<u64> },           // new totals, one per bound
    Rejected { index: usize, used: u64 },  // the first bound that would exceed
}

pub enum Durability { Volatile, Durable }
```

`Meter` is a thin layer over this: `charge` is one `apply` with a `Reserve` op
and a limit on every bound; `record` is one `apply` with a `Settle` op and no
limits; `refund` is `release`; `check` is `get` per quota. A ledger entry that
is never settled or released — the plugin crashed between phases, or the
settlement event was lost — is dropped by `compact` after a TTL, as is every
`Settle`-phase entry once its redelivery window has passed, so the ledger is
bounded by the traffic of one TTL, not by history. The TTL is the store's
configuration (default one hour) and has to exceed the longest call a plugin
meters plus any delay in delivering its settlement event: past it, a late
`refund` returns `false` and the charge stands, and a redelivered `record` is
counted again.

`janus-core` ships two stores and a test suite:

- **`MemoryUsageStore`** — `Volatile`. A mutex over hash maps, with expired
  windows swept on write. Correct for one plugin process, loses everything on
  restart, and is the right store for `Fixed` windows a restart may legitimately
  reset.
- **`SqliteUsageStore`** — `Durable`, behind the `sqlite` feature. An embedded
  database file at a path the plugin chooses, so a `Total` budget survives a
  plugin restart with no external service, in keeping with
  <ref target="docs/index#goals">standalone by default</ref>. All writes go
  through one writer thread; each `apply` is a single `BEGIN IMMEDIATE`
  transaction; the file is in WAL mode with `synchronous=FULL`. The store takes
  an exclusive advisory lock at open, on a `<path>-lock` file beside the
  database, and refuses to open a store another process holds. Two plugin
  replicas sharing one SQLite file is not a supported deployment of this store:
  it serializes every `apply` across processes through SQLite's file lock, and
  on a network filesystem — the only way replicas on different hosts could share
  it — that lock is not reliable. Failing at open is better than counts that are
  silently wrong.
- **A conformance suite**, behind the `testing` feature, that any `UsageStore`
  implementation runs against itself: atomicity of `apply` under concurrent
  callers, idempotency on a repeated `op`, all-or-nothing across bounds,
  `release` landing in the window that was charged, and `compact`. Both shipped
  stores pass it, and it is what makes a third store a plugin author's own work
  rather than a Janus release.

A replicated backend is deliberately not shipped. The trait is designed so one
can be — every `apply` touches keys for one `(identity, meter)`, which in Redis
is one hash-tagged slot and one Lua script, with the ledger as a hash under a
TTL — and that sketch is the reference for whoever runs the first plugin with
replicas. Until that plugin exists, shipping and testing a store against a
service no deployment runs would be code with no user behind it. Why the trait
was redesigned, and what was weighed, is in
<ref target="docs/decisions#metering-store">the decision record</ref>.

### Charging before the cost is known {#two-phase-metering}

The awkward part of metering a proxy is that `ResolveRoute` has to answer before
the call happens, and the figure worth billing — tokens consumed, bytes
transferred — only exists afterwards. The design does not pretend otherwise. It
runs in two phases, which is what the `request_id` on both messages is for:

1. **At `ResolveRoute`:** `check(identity, "cost")` on every dimension whose
   figure arrives after the fact — `Exceeded` denies a caller already over its
   cost budget — then `charge(request_id, identity, "requests", 1)` on the
   dimension known up front. `Exceeded` denies the request before it costs
   anything upstream, which is the whole reason the check is here rather than in
   a reconciliation job.
2. **On `call_completed`:** the plugin reads the real figures out of `usage` and
   `record`s them under the event's `request_id` — unconditionally, because the
   cost has already been incurred and a limit crossed here must be visible to
   the next `check` — then `settle`s the phase-1 charge.
3. **On `call_abandoned`:** the plugin `refund`s the phase-1 charge, because the
   call never reached the upstream — whether `ResolveRoute` said yes or the
   daemon gave up waiting for its answer.

The consequence, stated plainly rather than designed away: a cost-based budget
is enforced on the *next* request, not the one that exceeded it. An identity can
overshoot its limit by the cost of the calls it has in flight when it crosses it
— one call's worth for a sequential caller, more for a concurrent one. That is
inherent to billing a figure that does not exist until the work is done — the
alternatives are buffering the whole response to price it before relaying (which
breaks streaming and the transparency property) or pre-authorizing a pessimistic
estimate (which under-serves every caller to bound the overshoot of one). An
in-flight overshoot on a budget that denies the next call is the cheaper trade,
and any plugin that disagrees can `charge` an estimate against the cost meter in
phase 1 and `record` the difference in phase 2 using exactly these primitives.

## No plugin configured: pure pass-through {#no-plugin-pass-through}

With no plugin endpoint configured at all, `janus-daemon` makes zero calls
defined in `janus.proto`: it substitutes credentials from its own default chain
and relays responses unmodified. There is no policy, and no way for a missing
plugin to break a working proxy. This is one of the stated project goals (see
<ref target="docs/index#goals">standalone by default</ref>) — the plugin
contract is strictly additive, never a hidden requirement to get basic proxying
working.

"Configured" means a `[plugin]` block is present in the daemon's config file,
and nothing else — see <ref target="docs/plugin-contract#plugin-config">
configuring the plugin</ref>.
