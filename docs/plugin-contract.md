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
delegated to a plugin over a gRPC contract defined in `proto/janus.proto`.
`janus-daemon` is always the gRPC *client* of this contract; the plugin is the
*server* — see <ref target="docs/architecture#janus-daemon">janus-daemon</ref>.

The contract defines exactly two RPCs, `ResolveRoute` and `Notify`, and they are
shaped very differently from each other. That's deliberate: one is on the
critical path of every proxied request and its answer changes what Janus does;
the other is an after-the-fact announcement that changes nothing. The proto
mirrors that rather than forcing both into one shape for consistency's sake.

## ResolveRoute: the substitution decision {#resolve-route}

```protobuf
rpc ResolveRoute(ResolveRouteRequest) returns (ResolveRouteResponse);

message ResolveRouteRequest {
  string conversation_id = 1; // optional correlation hint; may be empty
  string upstream = 2;        // "anthropic" | "openai" | "openrouter"
  string caller_identity = 3; // the verified identity that is asking
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

The request carries the two things a policy decision needs: **who** is asking
(`caller_identity`, already verified by the daemon — a plugin never has to
authenticate the caller itself) and **what** they are asking to reach
(`upstream`). `conversation_id` is a correlation hint only, useful for grouping
related calls, and may be empty.

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
  string conversation_id = 2;
  string payload_json = 3;   // event-specific data, kept loose on purpose
}

message NotifyAck {}
```

`Notify` is fire-and-forget. `janus-daemon` never blocks on it and barely
inspects the response — it's a "something happened, tell whoever's listening"
channel, not an exchange whose result affects proxy behavior. It is how the
observe behavior in <ref target="docs/architecture#proxy-behaviors">proxy
behaviors</ref> reaches anything outside the daemon, and because it is off the
critical path, a slow or absent listener cannot stall a caller's response.

It uses one generic envelope (`event_type`, `conversation_id`, `payload_json`)
instead of one RPC per event kind, specifically so that new event types don't
require touching `janus.proto` again. The events worth emitting are the ones a
policy implementation cannot discover on its own because they happen after
`ResolveRoute` has already answered: a credential the upstream rejected
(`credential_invalid`), an upstream-side account error (`account_error`), the
completion of a call whose cost needs recording. A plugin consumes the event
types it cares about and ignores the rest; neither side needs the other to agree
on a fixed list.

## Execution and transport models {#transports}

The same gRPC interface is used no matter where the plugin runs. What changes is
how far the call has to travel, and that is a deployment choice an operator
makes based on the latency it can afford and the isolation it wants:

- **Bundled (in-process).** The plugin is loaded into `janus-daemon`'s own
  process and implements the same interface without a socket in between. Fastest
  — no socket, no context switch, no network round trip — and the right choice
  when the policy logic is trusted and the per-request budget is tight. It is
  also the weakest isolation: the plugin shares an address space with the
  process that holds real credential material. "Bundled" means loaded and
  conforming to the same interface, not compiled in as a fork of Janus; the
  mechanism by which that is achieved is not yet decided — see
  <ref target="docs/status#open-questions">open questions</ref>.
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

Two consequences of holding the interface constant across all three. First, the
model is meant to be a deployment choice rather than a plugin author's: the
intent is that one implementation can be bundled in one deployment and run
remotely in another, though how completely that holds depends on the unresolved
bundled ABI — see <ref target="docs/status#open-questions">open questions</ref>.
Second, the fail-closed posture above is not a remote-only concern — a bundled
plugin that panics or a local socket that is not listening are handled the same
way as an unreachable network service.

gRPC is the contract shape precisely because of the first model's existence
alongside the third. The interface has to be cheap enough to sit on the
synchronous path of every request while still being implementable out-of-process
and in any language: a schema-defined binary protocol with a single multiplexed
connection and streaming support is the cheapest thing that satisfies both.

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
