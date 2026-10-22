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
  back. Payload: `upstream`, `status`, `credential_ref` (what was actually used,
  including when the plugin said `use_default`), `duration_ms`, and `usage` —
  whatever the per-upstream implementation was able to observe about what the
  call consumed. `usage` is the "what in a response is worth observing" slot
  from <ref target="docs/credential-substitution#general-purpose"> not specific
  to LLM providers</ref>, and is the only route by which a real cost figure
  reaches a plugin.
- **`call_abandoned`** — `ResolveRoute` answered, and the call then never
  reached the upstream. Payload: `stage` (which step of
  <ref target="docs/architecture#request-lifecycle">the lifecycle</ref> it died
  at) and `reason`. This is the event that makes a reservation refundable; see
  <ref target="docs/plugin-contract#metering-primitives">metering
  primitives</ref>.
- **`credential_invalid`** — the upstream rejected the credential Janus
  substituted in.
- **`account_error`** — the upstream reported a problem with the account behind
  the credential rather than with the request.

Both of the first two carry the `request_id` of the `ResolveRoute` they follow.
Exactly one of them is emitted per resolved request, so a plugin that charges a
budget on the way in always gets a matching settlement event on the way out.

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
  critical path of every proxied request, and the channel has to be secured. How
  that channel is authenticated is not yet decided — see
  <ref target="docs/status#open-questions">open questions</ref>.

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

Build tooling beyond this — whether the custom crate lives in the Janus
workspace, how it pins a Janus version — is an operator's own packaging choice
and not something Janus specifies.

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
/// What a plugin calls from its ResolveRoute implementation.
pub struct Meter<S: UsageStore> { /* store + configured quotas */ }

impl<S: UsageStore> Meter<S> {
    /// Check and increment in one step, so two concurrent requests from one
    /// identity cannot both pass a limit with one unit left.
    pub async fn charge(&self, identity: &str, meter: &str, amount: u64)
        -> Result<Charge, UsageError>;

    /// Give an amount back — for a charge whose call never happened.
    pub async fn refund(&self, identity: &str, meter: &str, amount: u64)
        -> Result<(), UsageError>;
}

pub enum Charge {
    Allowed  { used: u64, limit: u64 },
    Exceeded { used: u64, limit: u64 },
}

pub struct Quota { pub limit: u64, pub window: Window }
pub enum Window {
    Fixed { period: Duration }, // resets on a period boundary
    Total,                      // never resets
}
```

`charge` returning `Exceeded` is the signal to answer `deny_reason`; `Allowed`
leaves the plugin free to answer `credential_ref` or `use_default` as it would
have anyway. A plugin can hold several meters per identity — `"requests"`,
`"input_tokens"`, whatever it names — and the quota configuration is the
plugin's, not Janus's.

Storage is behind a trait so the primitive doesn't dictate where counts live:

```rust
#[async_trait]
pub trait UsageStore: Send + Sync {
    async fn add(&self, key: &MeterKey, amount: u64) -> Result<u64, UsageError>;
    async fn get(&self, key: &MeterKey) -> Result<u64, UsageError>;
    async fn sub(&self, key: &MeterKey, amount: u64) -> Result<(), UsageError>;
}

pub struct MeterKey {
    pub identity: String,    // caller_identity
    pub meter: String,       // the plugin's own dimension name
    pub window_start: i64,   // derived from the Quota's Window
}
```

`janus-core` ships one implementation, `MemoryUsageStore`, which is correct for
a single plugin process and loses its counts on restart. That is the right
starting point and not the end state; see
<ref target="docs/status#open-questions">open questions</ref> for the shared
backend.

### Charging before the cost is known {#two-phase-metering}

The awkward part of metering a proxy is that `ResolveRoute` has to answer before
the call happens, and the figure worth billing — tokens consumed, bytes
transferred — only exists afterwards. The design does not pretend otherwise. It
runs in two phases, which is what the `request_id` on both messages is for:

1. **At `ResolveRoute`:** `charge(identity, "requests", 1)` on a dimension known
   up front. `Exceeded` denies the request before it costs anything upstream,
   which is the whole reason the check is here rather than in a reconciliation
   job.
2. **On `call_completed`:** the plugin reads the real figures out of `usage` and
   charges them to the identity, matching the event's `request_id` to the
   decision it made in phase 1.
3. **On `call_abandoned`:** the plugin `refund`s the phase-1 charge, because
   `ResolveRoute` said yes and the call never reached the upstream.

The consequence, stated plainly rather than designed away: a cost-based budget
is enforced on the *next* request, not the one that exceeded it. An identity can
overshoot its limit by at most one call's worth. That is inherent to billing a
figure that does not exist until the work is done — the alternatives are
buffering the whole response to price it before relaying (which breaks streaming
and the transparency property) or pre-authorizing a pessimistic estimate (which
under-serves every caller to bound the overshoot of one). One call of overshoot
on a budget that denies the next call is the cheaper trade, and any plugin that
disagrees can pre-authorize an estimate in phase 1 and reconcile in phase 2
using exactly these primitives.

## No plugin configured: pure pass-through {#no-plugin-pass-through}

With no plugin endpoint configured at all, `janus-daemon` makes zero calls
defined in `janus.proto`: it substitutes credentials from its own default chain
and relays responses unmodified. There is no policy, and no way for a missing
plugin to break a working proxy. This is one of the stated project goals (see
<ref target="docs/index#goals">standalone by default</ref>) — the plugin
contract is strictly additive, never a hidden requirement to get basic proxying
working.

How `janus-daemon` discovers a configured plugin endpoint is open — see
<ref target="docs/status#open-questions">open questions</ref>.
