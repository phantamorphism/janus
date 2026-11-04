---
title: Quickstart
type: reference
status: draft
tags: [janus, quickstart]
---

# Quickstart

This walkthrough builds Janus, runs the daemon against a stand-in upstream on
localhost, stores a credential, issues a caller an identity, and shows a call
that reaches the upstream carrying the real credential the caller never saw.
Every command here was run as written against the release binaries. It needs a
Rust toolchain (1.89 or newer), `curl` and, for the stand-in upstream and the
UI, Node.js.

The pieces it exercises are described in
<ref target="docs/credential-substitution">credential substitution</ref>,
<ref target="docs/caller-identity">caller identity extraction</ref> and
<ref target="docs/configuration">daemon configuration</ref>.

## Build {#build}

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"     # janus and janus-daemon
```

This produces two binaries: `janus-daemon` (the proxy) and `janus` (the admin
client). To serve the web UI as well, build it and place it where the daemon
looks: a `ui` directory beside the executable, or `../share/janus/ui`, or
whatever `ui.assets_dir` names:

```sh
npm --prefix ui ci && npm --prefix ui run build
cp -R ui/dist target/release/ui
```

Skip this step if you only want the CLI; omit the `[ui]` block below and the
daemon does not need the assets.

## A stand-in upstream {#upstream}

Real upstreams need real keys. To see the substitution without one, run a server
that reports the `Authorization` header it receives. Save this as
`mock-upstream.js`:

```js
// A stand-in upstream: reports the Authorization header it received.
require("http")
  .createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ path: req.url, authorization: req.headers.authorization ?? null }) + "\n");
  })
  .listen(27390, "127.0.0.1");
```

```sh
node mock-upstream.js &
```

## Configure and start the daemon {#daemon}

Save this as `config.toml`. The daemon's config file is its only source of
settings; the full reference is <ref target="docs/configuration">daemon
configuration</ref>.

```toml
[data_plane]
bind = "127.0.0.1:27300"            # where callers are pointed

[ui]                                # optional; omit for no UI listener
bind = "127.0.0.1:27302"

[identity]                          # how a caller's identity is read and verified
method = "opaque"                   # looked up in the daemon's own registry
source = { header = "authorization", scheme = "Bearer " }

[[upstream]]
name        = "openrouter"
path_prefix = "/openrouter"
base_url    = "http://127.0.0.1:27390"   # the real one is https://openrouter.ai/api
```

Choose where the daemon keeps its state (the credential pool, the SQLite
database, the admin socket and the admin token). It defaults to `~/.janus`; here
it is a local directory. The `janus` CLI resolves the directory the same way, so
exporting the variable once covers both:

```sh
export JANUS_DATA_DIR="$PWD/data"
janus-daemon check-config --config config.toml    # validate without binding anything
janus-daemon --config config.toml &
```

`check-config` prints the listeners and upstream bindings it would start with
and exits 0. The daemon logs the admin socket path, the data-plane address and
(with `[ui]`) the UI address and the path of the admin token.

```sh
janus status
```

This prints the version, the listeners, the upstream and its binding, the
request-log state, `credentials 0` and `plugin none`.

## Add a credential {#credential}

The real key goes into the credential pool, read from standard input so it never
appears in shell history or the process list:

```sh
printf '%s' 'sk-or-REAL-SECRET-KEY' \
  | janus credential add --upstream openrouter --ref openrouter-main --api-key-stdin
```

The daemon replies `added credential openrouter-main` with its metadata (kind
`api_key`, status `active`), and the credential is live at once; no restart. The
CLI reaches the daemon over its admin unix socket (`$JANUS_DATA_DIR/admin.sock`,
mode `0600`), so no token is needed.

## Create an identity for a caller {#identity}

```sh
TOKEN=$(janus identity create --identity agent-1 --expires-in 1h)
```

With the `opaque` method the daemon generates the token and shows it exactly
once; on success it is the only thing written to stdout, so the assignment above
captures it (the metadata goes to stderr). This token is what a sandboxed agent
would be given in place of an upstream key. The daemon stores only its hash.

## Call through Janus {#call}

```sh
curl -s http://127.0.0.1:27300/openrouter/v1/models \
  -H "Authorization: Bearer $TOKEN"
```

Output:

```json
{"path":"/v1/models","authorization":"Bearer sk-or-REAL-SECRET-KEY"}
```

The upstream received the real key; the caller sent only its identity token. The
`/openrouter` prefix was removed and the rest of the path appended to
`base_url`. A call with no identity is refused before anything is forwarded:

```sh
curl -s -w ' %{http_code}\n' http://127.0.0.1:27300/openrouter/v1/models
```

```
{"error":{"code":"no_material","message":"caller identity rejected: no_material","type":"janus_error"},"type":"error"} 401
```

A token the registry does not know is refused the same way, with code `invalid`.

## Look at what happened {#observe}

```sh
janus log query --limit 5
```

Both calls are in the request log, which holds metadata only (no bodies, no
credential material): the forwarded call shows `agent-1`, outcome `forwarded`
and status `200`; the other shows outcome `rejected_identity(no_material)`.
`janus log tail` follows the log live; `janus credential list` shows the
credential's request counter.

If you built the UI, open <http://127.0.0.1:27302/>. It asks once for the admin
token, which is in `$JANUS_DATA_DIR/admin-token`:

```sh
cat "$JANUS_DATA_DIR/admin-token"
```

## Revoke {#revoke}

Revocation takes effect on the very next request:

```sh
janus identity list                       # note the ID column
janus identity revoke <id>
curl -s -w ' %{http_code}\n' http://127.0.0.1:27300/openrouter/v1/models \
  -H "Authorization: Bearer $TOKEN"
```

The last call now returns code `revoked` and status 401.

## Point a real client at Janus {#real}

Change the upstream to the real service and give the SDK Janus's address and an
identity token instead of a key. For an OpenAI-compatible client against
OpenRouter that is, in the config:

```toml
base_url = "https://openrouter.ai/api"
```

and, in the agent's environment, a base URL of
`http://127.0.0.1:27300/openrouter/v1` and the identity token as its API key.
The SDK sends the token as `Authorization: Bearer`, which the binding above
reads. For an upstream whose SDK sends a different header, change `source` in
the binding; a binding can be set per upstream or per path — see
<ref target="docs/caller-identity#binding">binding an extractor to a request
type</ref>.

## Stop {#stop}

```sh
kill %1 %2        # the daemon and the stand-in upstream, in this shell
```

## Where next {#next}

- Per-agent attribution beyond a name: `janus identity create --claim key=value`
  attaches claims that policy sees.
- Other identity methods (`hmac`, `jwt`, `trusted_header`) for deployments that
  already mint tokens: <ref target="docs/caller-identity#methods">verification
  methods</ref>.
- A policy plugin for authorization and metering:
  <ref target="docs/plugin-contract">the plugin contract</ref>.
- Exposing the admin surface off the host:
  <ref target="docs/admin-surface">admin surface, CLI and UI</ref>.
