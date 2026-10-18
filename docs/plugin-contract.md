---
title: Plugin contract
type: decision
status: accepted
tags: [janus, plugin, grpc]
---

# Plugin contract

Janus is extended by host applications through a gRPC contract defined in
`proto/janus.proto`, not by forking Janus or baking host-specific code into it.
`janus-daemon` is always the gRPC *client* of this contract; a host application
(for example, Shadow) implements it as the gRPC *server* — see
<ref target="docs/architecture#janus-daemon">janus-daemon</ref>.

The contract defines exactly two RPCs, `ResolveRoute` and `Notify`, and they are
shaped very differently from each other. That's deliberate: the two things they
do behave differently, and the proto mirrors that rather than forcing both into
one shape for consistency's sake.

## ResolveRoute: synchronous, fail-closed {#resolve-route}

```protobuf
rpc ResolveRoute(ResolveRouteRequest) returns (ResolveRouteResponse);

message ResolveRouteRequest {
  string conversation_id = 1;
  string upstream = 2; // "anthropic" | "openai" | "openrouter"
}

message ResolveRouteResponse {
  oneof outcome {
    string credential_ref = 1; // use this specific credential
    bool use_default = 2;      // defer to janus-daemon's default chain
    string deny_reason = 3;    // hard-fail the request
  }
}
```

`ResolveRoute` is a blocking, synchronous call, consulted before `janus-daemon`
forwards a request upstream — but only when a routing plugin is actually
configured. Every proxied request pays for this round trip when the plugin is
enabled.

The default failure posture is **fail-closed**: if the plugin is unreachable,
times out, or returns an error, the request is rejected rather than silently
falling back to a default credential. This was a deliberate choice, not an
oversight — silently billing the wrong account or credential was judged worse
than a failed request that the caller can see and retry.

This mirrors an existing invariant in Shadow's internal gateway: its AC-03
account-routing feature already behaves as "hard error, no fallback" for the
same reason. `ResolveRoute` is the replacement for that subsystem in the Janus
world — see <ref target="docs/scope#left-behind">scope: what's left behind</ref>
for why AC-03 itself isn't portable as-is.

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
channel, not a request/response exchange whose result affects proxy behavior.

It uses one generic envelope (`event_type`, `conversation_id`, `payload_json`)
instead of one RPC per event kind, specifically so that new event types don't
require touching `janus.proto` again. Examples of event types a host might emit
or consume: `credential_invalid`, `account_error`, and — notably — a
context-checkpoint-detected signal. That last one existed in Shadow's *original*
internal gateway module to drive a compaction-preamble feature that is
Shadow-specific; it is **not** being ported to Janus as a built-in. Under this
contract it would just be one more `event_type` a host is free to emit and
another host is free to ignore. See <ref target="docs/scope#left-behind">scope:
what's left behind</ref>.

## No plugin configured: pure pass-through {#no-plugin-pass-through}

With no plugin endpoint configured at all, `janus-daemon` makes zero calls
defined in `janus.proto` and is a plain pass-through proxy. This is one of the
three stated project goals (see <ref target="docs/index#goals">the three
goals</ref>, goal 1) — the plugin contract is strictly additive, never a hidden
requirement to get basic proxying working.

How `janus-daemon` discovers a configured plugin endpoint, and how the
daemon-to-plugin connection is authenticated, are both open — see
<ref target="docs/status#open-questions">open questions</ref>.
