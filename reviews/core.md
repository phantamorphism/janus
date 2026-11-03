---
title: core
type: reference
status: draft
---

VERDICT: FAIL REVIEWER: independent-reviewer (claude-opus-5-5), janus-core
(contract, identity, pool, metering, proxy, peer) + janus-daemon plugin/** and
store/**

# Review: janus-core and daemon plugin/store

## Scope and method

I read every file under `crates/janus-core/src/**`, plus
`crates/janus-daemon/src/plugin/**` and `crates/janus-daemon/src/store/**`. The
spec I checked them against is `docs/*.md` and `proto/janus.proto`. I did not
look at the daemon dataplane/admin code or the UI, which another team is
building.

Mutation testing ran only in scratch copies under `/tmp/janus-review-*`. I made
no edits in the tree and ran no `git` or `sch`.

## Commands run

- `cargo test --workspace --all-features`: passes (206 + 86 + 2 tests, 0
  failed).
- `cargo clippy --workspace --all-features --all-targets -- -D warnings`: clean.
- `node scripts/gates/dup-scan.mjs crates 10`: **exit 1**. The tool reports two
  clusters:
  - `proxy/openrouter.rs:49-54` duplicates `proxy/anthropic.rs:119-124`. Both
    contain the same `forwarded_headers` and `outbound_url -> join_base`
    boilerplate, and `openai.rs` repeats it as well.
  - `metering/meter.rs:844` duplicates `meter.rs:539`. These are copies of a
    test snippet.

## Findings

- [ ] [high] crates/janus-daemon/src/plugin/socket.rs:159 The plugin socket-path
  check can be bypassed with a symlink, which defeats the StrictModes-style
  check that `docs/plugin-contract.md` "securing the channel" requires.
  - **Cause:** `check_full` canonicalizes the configured socket's directory. If
    the socket file itself is a symlink, it replaces `resolved` with the
    symlink's target. It then walks only `socket_dir(target)` and takes the
    allowed owner from that target directory (`socket_dir: Some(meta.uid())`).
    The directory that holds the symlink, `canonical_dir`, is never walked.
  - **Scenario:** the configured endpoint is `unix:///srv/open/p.sock` and
    `/srv/open` is mode 0777. A direct socket there is correctly refused. An
    attacker instead creates `/srv/open/p.sock -> /home/attacker/s/evil.sock`,
    with `/home/attacker/s` owned by the attacker and mode 0750. `check_full`
    returns `Ok`, and the daemon connects to the attacker's socket. The attacker
    then answers ResolveRoute (choosing any credential ref) and receives every
    Notify event. A scratch copy reproduced this.
  - **Fix:** walk `canonical_dir` as well, or refuse a symlink in the final path
    component.
  - No test covers a symlinked socket.

- [ ] [high] crates/janus-core/src/pool/slot.rs:146 After a credential is added
  with its key as a bare JSON string, the daemon cannot restart.
  - **Cause:** two `plain_api_key` helpers disagree.
    - The upstream's helper, `proxy/material.rs:21-29`, accepts `"sk-..."` (a
      JSON string) as well as `{"api_key": ...}`. So `FilePool::add` and
      `replace_material` accept a bare string.
    - The pool's helper, `slot.rs:146`, recognizes only the object form. So
      `MaterialFile::for_document` (`slot.rs:107`) chooses `material.json` and
      writes the string there.
    - On the next open, `json_document` (`slot.rs:171-177`) refuses any
      non-object, `open_slot` fails, and `FilePool::open` refuses to start.
  - **Scenario:**
    `pool.add(NewCredential { upstream: OpenRouter, material: json!("sk-or-v1-x"), .. })`
    returns `Ok(ApiKey)`. Restarting then fails with
    `material.json: a JSON material document must be an object`. One admin
    `POST /v1/credentials` call with a string `material` is enough to make the
    daemon unbootable.
  - **Fix:** use one helper and one notion of what a plain-key document is. Also
    add a test that reopens the pool after the string form.

- [ ] [medium] crates/janus-core/src/proxy/openai.rs:265 Any caller can
  permanently mark a shared credential `invalid`.
  - **Cause:** OpenAI maps every 401 *and 403* to `CredentialInvalid`. Anthropic
    does the same at `proxy/anthropic.rs:238`. The pool then holds the
    credential `invalid` until an operator rotates the material.
  - **Why a caller can trigger it:** the caller controls the path and body, and
    there is no path allowlist.
  - **Scenario:** a pool holds an OpenAI restricted key (or a Codex OAuth
    token). A sandboxed caller requests an endpoint outside that key's scopes,
    e.g. `GET /v1/files`, and OpenAI answers 403 "missing scopes". The
    credential is now `invalid` for every caller.
  - **Other triggers:**
    - A Cloudflare 403 challenge page from chatgpt.com.
    - An Anthropic OAuth 401 caused by the access token being rotated elsewhere.
      This marks the credential invalid without attempting a refresh first.
  - The code already understands the distinction: OpenRouter
    (`openrouter.rs:10-12`) deliberately ignores 403.
  - This is a caller-triggerable denial of service against other identities,
    which contradicts "revoking a single identity costs nothing to any other
    caller" in `credential-substitution.md`.

- [ ] [medium] crates/janus-core/src/pool/mod.rs:374 Refresh single-flight only
  holds when the refresh succeeds.
  - **Cause:** after a failed refresh, `needs_refresh` is still true. Every
    waiter queued on `io` then calls `refresh_locked` again, one after another.
    The module doc (`pool/mod.rs:42`) promises concurrent requests "wait for one
    refresh and all use its result".
  - **Scenario:** an expired OAuth slot whose refresh token is spent
    (`invalid_grant`) receives 16 concurrent requests. That makes 16 sequential
    token-endpoint calls, each with up to the 30s `REFRESH_TIMEOUT`. A
    scratch-copy probe test counted 16 `refresh()` calls.
  - There is no backoff or negative caching, so this repeats for as long as
    traffic continues.

- [ ] [medium] crates/janus-core/src/pool/slot.rs:182 `sniff_document` turns a
  JSON file that is truncated or mid-rewrite into an "API key" made of the JSON
  text.
  - **Cause:** anything that does not parse as an object falls through to
    `plain_key_document`. Minified JSON contains no whitespace, so the
    whitespace check does not reject it.
  - **Where it bites:** `reload_if_changed` (`pool/mod.rs:353`, `:506-511`)
    re-reads a config `file` credential exactly when a vendor CLI rewrites it.
  - **Scenario:** a non-atomic rewrite is observed as
    `{"tokens":{"access_token":"at","refresh_token":"rt"`. The credential
    becomes `ApiKey`, the whole fragment (including the refresh token) is sent
    upstream as the auth header, and the new file stamp is recorded, so the
    corruption persists. A probe in the scratch copy confirmed the conversion.
  - **Fix:** a file that starts with `{` (or otherwise looks like JSON) must
    never be taken as a bare key.

- [ ] [medium] crates/janus-daemon/src/plugin/grpc.rs:147 The gRPC plugin
  channel has no HTTP/2 keepalive and no connect timeout. A ResolveRoute timeout
  drops only that one call and keeps the dead connection.
  - **Scenario:** the plugin's address disappears without a TCP reset (for
    example, a pod rescheduled to a new IP or an expired NAT/LB entry). Every
    proxied request then fails closed as `plugin_timeout` until kernel TCP
    retransmission gives up, roughly 15 minutes on Linux.
  - **Evidence:** in a scratch copy, a black-holing proxy produced 10
    consecutive timeouts on a single connection, with no reconnect.
  - The Notify stream has the same problem: it stalls silently and events are
    dropped once the 1024-entry queue fills.
  - **Fix:** set `http2_keep_alive_interval` / `keep_alive_timeout` /
    `connect_timeout`, or rebuild the channel after N consecutive timeouts.

- [ ] [medium] crates/janus-daemon/src/plugin/tls.rs:329 Pinned-certificate
  verification is untested.
  - **Mutations that survived** (all plugin tests still pass):
    - Deleting the byte comparison against `pinned_cert`. In pinned mode this
      means any certificate naming the host is accepted.
    - Deleting the pinned-certificate expiry check (`tls.rs` near 340).
  - **Gap:** every pinned test presents the correct pin. None presents a
    different self-signed certificate and expects `plugin_tls`.

- [ ] [medium] crates/janus-core/src/proxy/relay.rs:595 The test
  `redirects_are_relayed_not_followed` cannot fail.
  - **Why:** the fake upstream answers 307, and the request body is a
    non-replayable `wrap_stream`, so reqwest would never follow it anyway.
  - **Evidence:** removing `.redirect(Policy::none())` (`proxy/mod.rs:100`)
    leaves the test green. Changing the scripted status to 302 makes it fail.
  - **Why it matters:** on a cross-host redirect, reqwest strips `authorization`
    but not `x-api-key` or `chatgpt-account-id`. That guard is the only thing
    keeping an Anthropic API key from following a `Location` header, and nothing
    pins it.

- [ ] [medium] crates/janus-core/src/proxy/usage.rs:289 The SSE memory bounds
  are unpinned.
  - **Mutations that survived** (the whole `usage::` suite stays green):
    - Removing the line limit (`usage.rs:180`).
    - Removing the data limit (`usage.rs:206`).
  - **Gap:** `oversized_bodies_go_unobserved_rather_than_held` only checks that
    the *next* event parses. It does not check that the oversized one was not
    buffered.
  - **Scenario:** an upstream that streams one endless `data:` line can grow
    daemon memory without bound, and no test would notice.

- [ ] [medium] crates/janus-core/src/contract/binding.rs:521 `jwks_url` silently
  accepts plain `http://`.
  - **Risk:** anyone on the path to the JWKS endpoint can serve their own key
    set and mint any `caller_identity`, which is a full authentication bypass.
  - **Inconsistent with the rest of the design:** the plugin channel requires an
    explicit `allow_plaintext` and warns, and the admin listener refuses
    plaintext on non-loopback. Identity verification deserves at least the same
    opt-in.
  - **Scenario:** `jwks_url = "http://idp.internal/jwks"` is accepted with no
    warning, and an ARP/DNS spoofer forges tokens.

- [ ] [medium] crates/janus-core/src/identity/jwt.rs:48 A key without a `kid`
  never matches a token that carries one.
  - **Cause:** the match arm `(Some(_), None) => false`, and `pem_key`
    (`jwt.rs:385-389`) always sets `kid: None`.
  - **Effect:** with `public_key_file`, every JWT from Auth0, Okta, Entra or
    Keycloak is rejected, because they always set `kid`.
  - **Scenario:** a PEM EdDSA key and a token signed by it with header
    `kid: "k1"` gives
    `Err(Invalid("no EdDSA verification key with kid \"k1\""))`.
  - This fails closed, but it makes the documented "configured public key" mode
    (`caller-identity.md:124`) unusable with real IdPs, and no test covers it.

- [ ] [medium] scripts/gates/dup-scan.mjs The dup-scan gate fails (exit 1) on
  in-scope code:
  - `proxy/openrouter.rs:49-54` against `proxy/anthropic.rs:119-124`. The
    `forwarding_allowlist!` macro could emit `forwarded_headers`, and
    `outbound_url`'s `join_base` could be the trait's default method.
  - `metering/meter.rs:844` against `meter.rs:539`. Fold this into a test
    helper.

- [ ] [low] crates/janus-core/src/pool/slot.rs:492 `replace_material` is not
  atomic when the file name changes, for example when rotating `api_key` to
  `.credentials.json`.
  - It writes the new file, then removes the old one. A crash in between leaves
    two material files.
  - `read_slot` (`slot.rs:298-306`) then refuses the slot ("holds more than one
    material file"), and the daemon will not start until someone repairs it by
    hand.

- [ ] [low] crates/janus-core/src/pool/mod.rs:806 `resolve` checks credential
  status before `make_usable` and does not check it again afterwards (only
  `removed` is re-checked).
  - **Scenario:** a request is waiting on `io` for a refresh while an admin
    disables the credential or a concurrent `report` marks it invalid. That
    request still receives the material.
  - **Test gap:** removing the `removed` re-check at `:819` also survives the
    tests.

- [ ] [low] crates/janus-core/src/pool/mod.rs:369 Blocking filesystem I/O runs
  on the async runtime.
  - `make_usable` runs `disk_stamp` (a stat) on every request.
  - `reload_if_changed` reads whole files synchronously while holding the tokio
    `io` mutex (`:346-361`, `:507-510`).
  - `generate_ref` calls `Path::exists` (`:765`).
  - Everything else uses `spawn_blocking`.

- [ ] [low] crates/janus-core/src/pool/mod.rs:418 The check for "refreshed token
  is already expiring" is untested; deleting it survives every test.

- [ ] [low] crates/janus-core/src/proxy/outbound.rs:83 The pre-auth and
  post-auth identity-leak assertions can each be deleted on its own (mutations
  M2 and M3) and all `identity_material_absent*` tests still pass.
  - No test has an `apply_auth` that copies the caller's value, so the post-auth
    branch is never the one that fires.
  - The comment on
    `identity_material_absent_from_outbound_when_auth_would_copy_it` admits that
    the pre-check is what catches it.

- [ ] [low] crates/janus-core/src/proxy/relay.rs:136 Several relay behaviours
  are unpinned; each of these mutations survives the `relay::` suite:
  - Removing `strip_hop_by_hop` on forwarded responses. Chunked upstream
    responses carry `transfer-encoding`, so this matters.
  - The 16 KiB `CLASSIFY_PREFIX_LIMIT` cap (`:205`).
  - The `expected_len == relayed -> Complete` branch in `Drop` (`:301`).
  - The empty-request "no body" condition (`:126`).

- [ ] [low] crates/janus-core/src/proxy/openai.rs:110 Codex routing uses
  `path.find("/v1/")`, which matches the first `/v1/` anywhere in the path. The
  module doc says paths outside the `/v1` surface are refused, but:
  - `/dashboard/v1/x` maps to `chatgpt.com/backend-api/codex/x`.
  - `/v1x/v1/responses` maps to `.../codex/responses`.
  - The host stays fixed, so this is not SSRF, but it contradicts the doc.

- [ ] [low] crates/janus-core/src/proxy/outbound.rs:236 `join_base` checks the
  base path with a string `starts_with`, not by path segment.
  - Base `/api` with an output path `/apix/y` passes. This is reachable through
    `"/v1/.\t./.\t./apix/y"`, because the url crate strips tabs after the
    dot-segment check.
  - It is not reachable from `http::Uri` today, but it becomes reachable if the
    data plane ever percent-decodes `rest`.
  - The check is also redundant in every tested case: deleting it (mutation M6)
    survives.

- [ ] [low] crates/janus-core/src/proxy/mod.rs:99 The reqwest client never calls
  `.no_proxy()`.
  - `HTTP(S)_PROXY` / `ALL_PROXY` in the daemon's environment silently route
    substituted credentials and token refreshes through that proxy.
  - With an `http://` base URL the credential reaches that proxy in plaintext.

- [ ] [low] crates/janus-core/src/proxy/mod.rs:84 Only `connect_timeout` is set.
  There is no response-head or idle-read timeout.
  - An upstream that completes TLS and then stalls holds the request, and its
    `call_completed` / `call_abandoned`, until the caller gives up.

- [ ] [low] crates/janus-core/src/proxy/mod.rs:106 `UpstreamSet` is dead outside
  its own test.
  - The pool takes `Vec<Arc<dyn UpstreamImpl>>`, so the module doc's claim that
    "the pool uses the same set" is false.
  - Related duplication: `jwt_expiry` (`material.rs:85`) hand-decodes base64url,
    and the test JWT helper is duplicated in `material.rs:153` and
    `openai.rs:303`.

- [ ] [low] crates/janus-core/src/proxy/usage.rs:29 Large SSE events silently
  report `usage: null`.
  - `SSE_EVENT_LIMIT` is 1 MiB, and OpenAI's `response.completed` event carries
    the whole response object. A large response therefore gives metering
    `usage: null` without any signal.
  - CR-only line endings are not handled (`usage.rs:179`).

- [ ] [low] crates/janus-core/src/identity/binding.rs:280 Nothing tests that two
  `hmac` bindings with different namespaces or secrets get separate extractors.
  - Mutating the sharing key to compare only the method survives every test.
    This is the namespace confusion the docs warn about.
  - Also surviving: accepting a non-numeric `nbf` (`jwt.rs:220`), and removing
    the 1 MiB JWKS cap (`jwt.rs:305`).

- [ ] [low] crates/janus-core/src/identity/binding.rs:191 Routes match by
  literal exact path, but the docs say "path pattern" (`caller-identity.md:165`,
  `admin-surface.md:148`).
  - **Bypass:** if a route is stricter than its upstream (upstream `none`, route
    `/v1/messages` = `hmac`), then `/v1/messages/` falls through to the open
    upstream binding.
  - `route()` also accepts paths that can never match (empty, no leading `/`,
    containing `?`) without error.

- [ ] [low] crates/janus-core/src/contract/binding.rs:299 The upstream merge
  treats `jwks_url` and `public_key_file` as independent fields.
  - **Scenario:** the default sets `jwks_url` and an upstream sets
    `public_key_file`. The merge yields `Conflict{jwks_url, public_key_file}`,
    so an upstream block can never change the key source.
  - Correlation in the same merge is already handled as "one logical field"; the
    key source needs the same treatment.

- [ ] [low] crates/janus-core/src/identity/jwt.rs:245 When `max_age` is
  exceeded, JWKS refetches happen on the request path while holding the
  `refetch` mutex.
  - A hanging endpoint stalls all requests on that extractor for up to 10
    seconds.
  - On failure `fetched_at` is not updated, so the stall repeats every
    `min_interval`.

- [ ] [low] crates/janus-core/src/contract/binding.rs:205 The header `scheme`
  value is not validated.
  - `scheme = "Bearer"` (no trailing space) accepts `Authorization: Bearerxyz`
    as token `xyz`.
  - `scheme = ""` accepts `Basic xyz`.

- [ ] [low] crates/janus-core/src/contract/identity.rs:44 `RequestHead` derives
  `Debug`, so any `{:?}` of it prints raw `Authorization`, `x-api-key` and query
  tokens. There is no call site today; this is latent.

- [ ] [low] crates/janus-daemon/src/plugin/tls.rs:363 The 30-day
  certificate-expiry warning is logged only on load or reload, or when the
  plugin's certificate changes.
  - A daemon started 60 days before expiry never warns as the certificate enters
    the 30-day window.

- [ ] [low] crates/janus-daemon/src/plugin/tls.rs:603 `janus plugin check` has
  no connect or handshake timeout; only `PROBE_WAIT` after the handshake is
  bounded.
  - Against a host that drops SYNs, `POST /v1/plugin/check` hangs for the OS
    connect timeout, which is roughly 75–127 seconds, three times over.

- [ ] [low] crates/janus-daemon/src/plugin/tls.rs:1 More TLS checks are
  unpinned:
  - Re-enabling TLS session resumption survives the tests. A resumed TLS 1.3
    handshake has no CertificateRequest, so it would misreport
    `plugin_no_client_auth`.
  - Removing the startup check for a client certificate that is not yet valid
    also survives.

- [ ] [low] crates/janus-daemon/src/store/request_log.rs:415 A failed batch
  insert drops up to 256 request-log records with only an error line and no
  retry.
  - Likely trigger: `SQLITE_BUSY` while the startup retention `DELETE` holds the
    write lock after a long downtime.

- [ ] [low] crates/janus-daemon/src/plugin/transport.rs:120 The failure log is
  shared between the Notify connection and ResolveRoute, and a normal
  end-of-stream is recorded as `Unreachable`.
  - A Notify close at the same moment as a real RPC error can make a
    ResolveRoute failure be classed `plugin_unreachable` instead of
    `plugin_error`.

- [ ] [low] crates/janus-daemon/src/store/db.rs:76 `janus.db` is set to 0600
  only when it is created. A pre-existing file with looser permissions is left
  alone, even though it holds token hashes, identities and claims.

- [ ] [low] crates/janus-daemon/src/contract/config.rs:340 Two undocumented
  config behaviours:
  - A `[plugin.tls]` block next to a `unix://` endpoint is silently ignored
    instead of refused.
  - A linked plugin with no `[plugin]` block is refused (`plugin/mod.rs:73-80`),
    which the docs do not list.
  - Separately, the Notify `dropped_events` counter is not surfaced in
    `PluginStatus`.

- [ ] [low] crates/janus-core/src/metering/memory.rs:29 The memory store's
  ledger and per-identity maps grow without bound unless the plugin calls
  `compact`.
  - The docs' claim "bounded by the traffic of one TTL" only holds under that
    condition, which appears only in a doc comment (`meter.rs:91`).
  - Separately, `SqliteUsageStore::drop` (`sqlite.rs:160`) joins the writer
    thread and so blocks a runtime worker when dropped in async code.

- [ ] [low] crates/janus-core/Cargo.toml:34 Manifest housekeeping:
  - `humantime` is declared but unused in janus-core.
  - The feature comments at lines 11 and 13 still say "Reserved for ... (leaf
    1.2.3)", although both features are implemented.
  - `pool` uses `std::os::unix` and `O_NOFOLLOW` but is not `#[cfg(unix)]`-gated
    like `peer`.

- [ ] [low] crates Repo hygiene: there are empty stray directories with spaces
  in their names: `crates/janus-core identity`, `janus-core metering`,
  `janus-core pool`, `janus-core proxy`, `janus-daemon plugin`,
  `janus-daemon store`. Each contains only an empty `src/`, which looks like a
  botched `mkdir -p`.

## Checked and found sound

- **Request smuggling (raw-TCP probes):**
  - TE+CL is normalized.
  - Hop-by-hop and Connection-listed headers, `host`, `te` and `upgrade` are
    never copied, even when allowlisted.
  - Auth headers are set with `insert` and marked sensitive.
- **Query identity:** scrubbed before `outbound_url`, using the same decoding as
  extraction.
- **No host-changing SSRF:** `//evil.com`, `/@evil.com`, CRLF, `%2e` and `\`
  were all checked.
- **Redirects:** reqwest uses `Policy::none()`.
- **HMAC:** constant-time; the signature is checked before claims; the signature
  format is strict.
- **JWT:** algorithms come from the key; HS*/`oct`/`none` are refused; `exp`,
  `iss` and `aud` are required.
- **Duplicate headers and query parameters:** rejected.
- **Opaque registry:** tokens are stored as SHA-256 hashes.
- **Pool locking:** no std lock is held across `.await`, and lock order is
  consistent.
- **Pool file writes:**
  - Writes are atomic (`O_EXCL|O_NOFOLLOW`, fsync, rename, directory fsync).
  - Files are 0600 and directories 0700.
  - The ref grammar blocks traversal.
- **Refresh failure:** fails the request; it never falls through to another
  credential.
- **Metering:**
  - `charge` is an atomic check-and-increment.
  - Request-id idempotency and replay work as documented.
  - Overflow on a limited bound is treated as exceeded.
  - The SQLite store uses `BEGIN IMMEDIATE`, WAL and `synchronous=FULL`, and all
    SQL is parameterized.
  - The conformance suite runs against both stores.
- **Plugin:**
  - Fails closed by default with a 250 ms timeout; linked-plugin panics also
    fail closed.
  - The Notify queue is bounded at 1024 and uses `try_send`, so it never blocks
    the request path.
  - TLS 1.3 only, with exactly one trust anchor and no system roots.
  - `tls init` writes keys 0600 with exclusive create.
- **Store:** migrations are append-only via `user_version`.
- **Test-only code:** `testutil.rs` is test-only.

### Mutations killed (sample)

Query scrub, Connection-listed hop-by-hop, dot-segment check, JSON 4 MiB cap,
HMAC namespace/expiry/case, JWT aud/nbf/iss/HS refusal, `kid` matching,
`use=enc`, refetch throttle, duplicate-header, scheme case, opaque
revoked/expired, peer uid, refresh single-flight (success path), ledger replay,
evaluate `>`/`>=`, sqlite release/settle, client-auth enforcement, Notify bound,
hostname check, CA chain, key mode, socket ownership, ancestor walk, fail-open
timeout/panic, db mode, pagination cursor, retention.
