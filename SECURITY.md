---
title: Security policy
type: reference
status: accepted
---

# Security policy

Janus exists to keep real upstream credentials out of the hands of the callers
it serves, so security reports are the most valuable kind of report it can get.
This document says how to send one, what counts, and what Janus does and does
not promise.

## Supported versions

Janus is pre-1.0. Security fixes are made on `main` and released as a new patch
version of the latest `0.x` minor line. Older minor lines are not patched; an
upgrade is the fix. Until a `0.1.0` tag is published, `main` is the only
supported version.

## Reporting a vulnerability

Report privately. Do not open a public issue, pull request or discussion for a
suspected vulnerability.

1. Go to the repository's **Security** tab on GitHub
   (<https://github.com/phantamorphism/janus/security>).
2. Choose **Report a vulnerability**. This opens a private GitHub Security
   Advisory visible only to you and the maintainers.
3. Include the Janus version or commit, the configuration involved (redact real
   credentials and tokens), what you expected, what happened, and the smallest
   reproduction you can. A failing test, or a short script against a real
   `janus-daemon`, is the best reproduction.

If the button is not available to you, open a public issue that says only that
you need a private channel for a security report, with no details, and a
maintainer will arrange one.

## What to expect

These are targets from a small volunteer-run project, not contractual
guarantees.

| Step | Target |
|------|--------|
| Acknowledgement of your report | 3 business days |
| Initial assessment (accepted, needs more information, or declined, with reasons) | 7 days |
| Fix or mitigation for an accepted report | 90 days at the outside, sooner for anything that exposes credential material |
| Public disclosure | When a fix is released, or at 90 days, whichever is first; earlier by agreement |

We publish a GitHub Security Advisory (with a CVE where warranted) and credit
you in it and in the changelog unless you ask not to be credited. If you plan to
publish your own write-up, tell us the date and we will coordinate.

## What counts as a vulnerability

Janus is a credential-substituting proxy. The properties it is built to hold are
the ones a report should attack:

- **A caller obtains real upstream credential material.** Anything that lets a
  data-plane caller read an upstream API key, OAuth access token or refresh
  token: in a response body or header, an error message, a redirect, a timing or
  ordering oracle, or by making Janus send the credential somewhere the caller
  chose.
- **Identity bypass.** A request accepted without valid identity material on a
  route bound to a verifying extractor (`hmac`, `jwt`, `opaque`); forging,
  replaying across namespaces, or extending the lifetime of a token; a revoked
  or expired opaque identity still accepted; one identity verifying as another;
  a caller identity verifying as the admin token or the reverse.
- **The caller's identity material reaching the upstream.** The outbound request
  is constructed from a per-upstream header allowlist; any path by which the
  presented identity is forwarded is a bug.
- **Admin surface exposure to the data plane.** Any way for a request arriving
  on the data-plane listener to reach `/v1` admin routes, or for the admin TCP
  or UI listeners to serve a `/v1` call without the admin token, or to start in
  a cleartext non-loopback configuration without the explicit opt-in.
- **Credential or secret leakage into logs and surfaces.** Credential material,
  admin tokens, opaque tokens, HMAC/JWT secrets or refresh tokens appearing in
  the request log, tracing output, admin API responses, the CLI output, error
  text or the UI. The request log is metadata-only by design; a body or secret
  in it is a bug.
- **SSRF and redirection through configuration or upstream behaviour.**
  Attacker-influenced values that cause Janus to send credentials to a host the
  operator did not configure: for example, a data-plane request path that
  escapes `base_url`, a redirect from an upstream or a JWKS endpoint that
  downgrades to `http://` or crosses to another host, or a `token_url` that
  validation should have refused.
- **Plugin channel downgrade.** A way to make the daemon talk to a network
  plugin without mutual TLS when the operator configured it, to accept a plugin
  that does not present or request a client certificate, to trust system roots
  in place of the operator-named anchor, or to treat an unreachable plugin as
  anything other than a denial (fail-closed is the contract).
- **Memory-safety or denial-of-service bugs reachable by an unauthenticated
  data-plane caller** that go beyond ordinary resource exhaustion, such as a
  panic that takes down the daemon from one request, or unbounded buffering
  before identity is verified.
- **Vulnerable dependencies** where Janus is demonstrably reachable by the
  vulnerable code path.

## Trust model and known limits

Reports that restate the following are not vulnerabilities, because the project
documents them as the boundary. They are written down here so you do not have to
discover them. The design lives in `docs/`: start with
`docs/credential-substitution.md`, `docs/caller-identity.md`,
`docs/admin-surface.md` and `docs/plugin-contract.md`.

- **The operator and the host are trusted.** Anyone who can read the daemon's
  data directory, open `admin.sock` (mode `0600`), read the admin token, or read
  the credential pool directories has the credentials. Janus does not defend
  against a malicious operator, a compromised host account running the daemon,
  or root on the machine. Credentials are stored on disk in the pool, not in an
  encrypted vault.
- **A caller keeps the power of its own identity.** Janus stops a caller from
  holding the upstream credential; it does not stop a caller from using its own
  identity for anything that identity is allowed to do, or from leaking that
  identity. Containing a leaked identity is what expiry, revocation and policy
  are for. A stolen `hmac` or `jwt` token is valid until it expires; `opaque`
  identities can be revoked immediately.
- **Some routes are open on purpose.** A route configured with `method = "none"`
  authenticates nothing, and a `trusted_header` binding believes whatever sets
  the header. Both are only correct behind infrastructure that already
  authenticated the caller, and the data plane must not be reachable by anyone
  who could forge the header.
- **Identity extraction is local.** An extractor either verifies or rejects. The
  request log may record an unverified decode of a rejected request's material
  for diagnosis; that value is never used to authorize anything.
- **Policy is a plugin's job.** With no plugin configured there is no policy and
  Janus is a pass-through. A plugin is trusted with the verified identity and
  the upstream. A linked (compiled-in) plugin shares the daemon's process and
  memory and is not isolated from it.
- **Plugin channel guarantees depend on the transport.** The unix-socket
  transport has no application-layer authentication: the filesystem permissions
  on the socket path are the boundary. Network transport requires mutual TLS
  unless the operator explicitly sets `allow_plaintext = true`, in which case
  the operator is asserting that infrastructure such as a service mesh secures
  the hop.
- **A plugin that goes away mid-call is handled by contract, not hidden.**
  `ResolveRoute` is fail-closed: an unreachable, slow or erroring plugin denies
  the request. The plugin may already have charged before the daemon gave up,
  which is why the daemon emits `call_abandoned` for a failed resolve. `Notify`
  delivery is best-effort, so a plugin can miss a settlement event. A budget
  plugin must treat its own ledger as the source of truth.
- **One retry on a stale connection can deliver a request upstream twice.** If a
  reused pooled connection fails before any response byte, the daemon resends
  once on a fresh connection when the body is at most 1 MiB. Because HTTP/1.1
  cannot tell an upstream that never read the request from one that read it and
  closed, that request may be processed twice. OAuth refresh grants are retried
  only on the same dead-connection signature, so a rotating refresh token is not
  spent twice on any other failure. Non-idempotent upstream calls can therefore
  be duplicated in this rare case.
- **Responses are relayed unmodified.** Janus does not inspect or scrub response
  bodies. An upstream that echoes a credential back in a response is outside
  Janus's control, and the request log does not capture bodies.
- **Admin and UI exposure is the operator's configuration.** Binding the admin
  TCP or UI listener to a non-loopback address with TLS and a strong token is a
  supported setup; exposing it to untrusted networks, or sharing the admin
  token, is not something Janus can compensate for. On loopback the admin token
  is still required, because loopback is not an authentication boundary.
- **Ordinary resource exhaustion** (a caller with a valid identity sending
  expensive requests, many connections, large bodies up to the documented
  limits) is not a vulnerability. Use a policy plugin or a rate limiter in
  front.
- **Janus does not mint identities.** The strength of the tokens a provisioner
  issues, and the secrecy of the shared secrets configured in Janus, are the
  deployment's responsibility.

## Safe harbour

Good-faith research is welcome. Test against your own deployment, do not access
data that is not yours, do not degrade other people's systems, and stop and
report as soon as you can demonstrate the issue. We will not pursue or support
legal action against research that follows this policy.
