---
title: Caller identity extraction
type: design
status: draft
tags: [janus, identity, config]
---

# Caller identity extraction

Every decision Janus makes is keyed on *which caller is asking*. This document
designs the stage that answers that question: how a caller identity is pulled
out of an inbound request, verified, and handed to the credential-substitution
decision. For what the identity is *for* once it exists, see
<ref target="docs/credential-substitution">credential substitution and policy
enforcement</ref>; for where this stage sits in the request path, see step 2 of
<ref target="docs/architecture#request-lifecycle">the request lifecycle</ref>.

## Janus extracts; it does not mint {#not-minting}

Janus deliberately does not define how a caller identity comes into existence.
It is not an identity provider, it issues nothing, and it has no opinion on a
sandbox's provisioning lifecycle. Its responsibility starts at the moment a
request arrives carrying some identity material and ends when that material has
been turned into a verified identity string.

This is not a gap left for later — it is the boundary that makes Janus
deployable. Every deployment that needs this proxy already has *something* that
creates agent sandboxes, and that something is the only component positioned to
hand an identity to a sandbox at creation time without a secret-distribution
problem. Janus inserting its own minting authority into that flow would mean
every adopter rebuilding sandbox provisioning around Janus. Verifying what the
existing provisioner already issues costs the adopter one config block.

The precedent this generalizes is concrete. The gateway Janus builds on (see
<ref target="docs/scope#extractable">scope: what carries over</ref>) is embedded
in a host that mints an HMAC token per agent session and delivers it by writing
the agent CLI's own credential file and base-URL setting — the agent then
presents that token believing it is an ordinary API key. The minting and the
delivery are the host's, specific to how the host spawns agents. Only the
verification step is generic, and only the verification step becomes Janus.

A host migrating onto Janus therefore keeps its minting code and configures a
matching <ref target="docs/caller-identity#methods">`hmac` method</ref> with the
same shared secret and namespace. Nothing about identity issuance has to move.

## The extractor {#extractor}

An **identity extractor** is the stage that runs before any policy call. It
takes the head of an inbound request and produces either a verified identity or
a rejection. It lives in
<ref target="docs/architecture#janus-core">janus-core</ref>, runs in-process,
and is configured rather than coded for every method listed below.

```rust
#[async_trait]
pub trait IdentityExtractor: Send + Sync {
    /// Stable name, used in config bindings and in the request log.
    fn name(&self) -> &str;

    /// Produce a verified identity, or reject the request. Must not have
    /// side effects: it may be called on requests that are later denied.
    async fn extract(&self, req: &RequestHead)
        -> Result<ExtractedIdentity, ExtractionError>;
}
```

`RequestHead` is method, path, and headers — never the body. Extraction happens
before the body is read so that an unauthenticated request is rejected without
Janus buffering anything a caller sends.

The output carries four things:

```rust
pub struct ExtractedIdentity {
    /// The identity policy keys off. Becomes
    /// `ResolveRouteRequest.caller_identity`. Opaque downstream: a stable
    /// string to compare and index, never a structure to parse.
    pub caller_identity: String,

    /// Remaining verified claims, if the method produced any. Becomes
    /// `ResolveRouteRequest.identity_claims`, so a plugin can make a
    /// decision on more than the bare id without parsing `caller_identity`.
    pub claims: BTreeMap<String, String>,

    /// A grouping value lifted from verified material, if the binding asked
    /// for one. Feeds `correlation_id` — see
    /// `docs/caller-identity#correlation` for why its provenance matters.
    pub correlation_id: Option<String>,

    /// Exactly where in the request the identity material was found, so the
    /// substitution stage can guarantee it is gone on the way out. See
    /// `docs/caller-identity#scrub`.
    pub presented_at: PresentedAt,
}
```

The split between `caller_identity` and `claims` is load-bearing. The gateway
Janus builds on carries a session id *and* an agent-type marker in one token,
and its routing logic reads the session id while other code reads the marker.
Collapsing both into one string would force plugins to parse an identifier the
<ref target="docs/plugin-contract#resolve-route">contract</ref> explicitly tells
them to treat as opaque. `claims` is the supported place for the rest.

## Verification methods {#methods}

A method is the "how do I know this material is genuine" half of an extractor.
Four ship in `janus-core`, each constructed from config with no code:

- **`hmac`** — the material is a signed claims blob Janus can verify locally
  with a shared secret: claims JSON, hex-encoded, with an HMAC-SHA256 signature
  over the encoding. Verification checks the signature, that the claims have not
  expired, and that the claim set's **namespace** matches the one this binding
  expects. The namespace check is not decoration: it is what stops a token
  minted for one purpose being replayed against another, and the gateway this
  generalizes relies on it to keep its proxy, host-RPC, and CLI tokens from
  being interchangeable. A Janus binding declares its namespace, and a token
  from any other namespace fails to verify even though the signature is good.
  This is the method a host that already mints its own tokens uses.
- **`jwt`** — the material is a JWT from an external issuer, verified against a
  JWKS endpoint or a configured public key, with issuer and audience checked.
  The method for deployments where an existing identity provider is already the
  minting authority. Janus verifies and projects a claim; it never issues.
- **`opaque`** — the material is a bare random string with no internal
  structure, verified by looking it up in a registry the daemon's
  <ref target="docs/architecture#admin-surface">admin surface</ref> manages. The
  method for deployments that want revocation to be an immediate operator action
  rather than a token-expiry wait, and that will accept a lookup on the critical
  path to get it.
- **`trusted_header`** — the material is taken as the identity with no
  cryptographic check, because a front proxy or service mesh has already
  authenticated the caller and is trusted to set the header. Correct in exactly
  that topology and catastrophic outside it: any caller that can reach the data
  plane directly can then claim any identity. It must be named explicitly in
  config — there is no way to reach it by default or by omission.

Each method is paired with a **source**: where in the request the raw material
sits. `header { name, scheme }` (with an optional prefix such as `Bearer ` to
strip), `query { name }`, or `basic_password` for upstreams whose SDKs only
offer HTTP basic auth.

Separating source from method is what makes one deployment's identity work
across upstreams with different auth conventions. The gateway Janus builds on
has a single hardcoded source — `Authorization: Bearer` — and makes it work by
choosing, per upstream, the specific SDK environment variable that happens to
produce that header (`ANTHROPIC_AUTH_TOKEN` rather than `ANTHROPIC_API_KEY`,
because the latter would make the client send `x-api-key` instead). That works
for a host that controls how its agents are configured, and not at all for a
proxy that has to accept whatever an existing caller already sends. Making the
source configuration moves the accommodation to the operator's config file,
where it costs one line, instead of to the caller's environment, where it may
not be changeable at all.

## Binding an extractor to a request type {#binding}

Extraction is configured per request type, resolved most-specific-first:

1. a **route** binding — one path pattern within an upstream,
2. the **upstream** binding — every path under that upstream's prefix,
3. the **daemon default** — anything not covered above.

If no binding resolves, the request is rejected. There is no implicit
"unauthenticated" tier; a path that should take no identity has to say so with
`method = "none"`, which is a deliberate, greppable statement in config rather
than a consequence of an omission.

```toml
# Daemon default: one identity scheme for every upstream that doesn't
# override it.
[identity]
method          = "hmac"
source          = { header = "authorization", scheme = "Bearer " }
secret_ref      = "caller-identity"
namespace       = "janus-data-plane"
identity_claim  = "subject"
correlation_claim = "session"    # optional; omit for no correlation value

[[upstream]]
name       = "anthropic"
path_prefix = "/anthropic"
base_url   = "https://api.anthropic.com"
# No [upstream.identity] — inherits [identity] wholesale.

[[upstream]]
name       = "openai"
path_prefix = "/openai"
base_url   = "https://api.openai.com"

  # This upstream's callers are SDKs that only send an API key, which lands
  # in a different header. Same secret, same namespace, different source.
  [upstream.identity]
  source = { header = "x-api-key" }

  # Narrower than the upstream: a route that takes no identity at all.
  [[upstream.route]]
  path     = "/v1/models"
  identity = { method = "none" }
```

An upstream's `[upstream.identity]` block is merged over the daemon default
field by field, so overriding only `source` keeps the default method, secret and
claim mapping. A route's `identity` block replaces the resolved upstream binding
outright — a route that differs at all usually differs completely.

Config is validated at startup, not per request: every binding is resolved into
a constructed extractor before the data-plane listener binds, and a binding that
names a missing secret, an unreachable JWKS endpoint, or an unregistered custom
extractor is a startup failure. A daemon that cannot establish who is calling
must not start and serve traffic anyway.

## Correlation values are not identities {#correlation}

`caller_identity` and `claims` come out of verified material and are
trustworthy. `correlation_id` may not be: besides `correlation_claim` above, a
binding can lift it from an unverified passthrough header
(`correlation.from_header = "x-request-id"`) so that a caller's own tracing id
survives the hop.

That makes it caller-controlled input, and it must never be used to key a policy
decision — a caller that can choose its own correlation value can choose another
caller's. It is for grouping and for logs. The
<ref target="docs/plugin-contract#resolve-route">contract</ref> says the same
thing from the plugin's side.

## The presented material never goes upstream {#scrub}

A caller's identity has no value at the upstream and must not reach it. Janus
guarantees that structurally rather than by remembering to delete it: the
outbound request is **constructed**, not copied-and-edited. Each upstream
implementation declares which inbound headers are forwarded, and anything not on
that list — including whatever carried the identity — is absent from the
outbound request because it was never added to it.

`ExtractedIdentity.presented_at` exists on top of that as an assertion, not as
the mechanism: the substitution stage checks that the location the extractor
read from is empty on the outbound request, so a per-upstream forwarding list
that wrongly includes an identity-bearing header fails loudly instead of leaking
a caller's token to a third party.

This is the same discipline the gateway Janus builds on already follows — it
captures a small allowlist of inbound headers for its request log and builds the
upstream call from scratch — stated as a requirement rather than left as an
implementation habit.

## Failure posture {#failure}

Extraction failure is a **401 to the caller, and nothing else happens**: no
plugin is called, no credential is read from the pool, no upstream connection is
opened, and the request body is never buffered. This is why extraction is step 2
of <ref target="docs/architecture#request-lifecycle">the request lifecycle</ref>
rather than part of step 3 — policy is entitled to assume the identity it is
handed is real.

`ExtractionError` distinguishes *no material presented*, *material present but
invalid*, and *material valid but for a different namespace or issuer*, because
those are three different operator problems (a caller that was never configured,
a caller whose token expired, a caller pointed at the wrong Janus binding) and
collapsing them into "401" makes the second and third undiagnosable.

For the request log only, Janus may decode identity material **without verifying
it** in order to record which caller a rejected request claimed to be from. The
gateway this is taken from has exactly this facility, with a warning attached,
and the same rule applies here: an unverified decode may be written to a log and
may never be returned from `extract` or used to authorize anything.

## Custom extractors {#custom}

Some deployments hold identity material that cannot be verified locally at all —
an opaque token belonging to an external authority that only answers an
introspection call. That is a custom extractor, not a new Janus feature: an
implementation of the `IdentityExtractor` trait above, registered by name in a
custom build, and referenced from config as `method = "custom:<name>"`.

```rust
// In a custom build's main.rs, alongside a linked plugin:
janus_daemon::run(config, Embedded {
    plugin: Some(Arc::new(MyPolicy::new())),
    extractors: vec![Arc::new(MyIntrospectionExtractor::new(client))],
}).await
```

This is the same compile-time linkage that
<ref target="docs/plugin-contract#linked">linked plugins</ref> use, and it is
deliberately the answer here instead of a third RPC on the plugin contract. An
extraction RPC would put a second blocking network round trip in front of the
one `ResolveRoute` already costs, on every request, to learn something a
locally-verifiable token supplies for free — and it would have to exist even in
the no-plugin pass-through mode, where Janus is specified to make zero calls
defined in `janus.proto`. A custom extractor that genuinely needs a network call
can make one; it just isn't the common case paying for it.

## Open: the opaque-identity registry {#open-registry}

The `opaque` method above looks identities up in a registry the daemon owns, and
that registry's shape is not designed here: what it stores beyond the identity
string, whether entries carry an expiry as well as being revocable, and the
admin-surface operations that create and revoke them. It belongs with the rest
of the admin command surface, which is still open — see
<ref target="docs/status#open-questions">open questions</ref>. The other three
methods need no such store and are unblocked by it.
