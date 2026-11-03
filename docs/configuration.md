---
title: Daemon configuration
type: reference
status: draft
tags: [janus, config, daemon]
---

# Daemon configuration

`janus-daemon` reads one TOML file, and that file is its only source of
settings: the command line supplies nothing but the file's path
(`janus-daemon --config <path>`). `janus-daemon check-config --config <path>`
runs the full startup validation, constructs every binding and the plugin
client, and exits without binding a listener. This document is the reference for
every section of the file. The identity bindings are designed in
<ref target="docs/caller-identity#binding">binding an extractor to a request
type</ref> and the plugin block in
<ref target="docs/plugin-contract#plugin-config">configuring the plugin</ref>;
this document states where they sit and how they combine with the rest.

```toml
data_dir = "/var/lib/janus"              # optional

[data_plane]
bind = "127.0.0.1:7600"

[admin]                                  # optional; the admin socket always exists
bind = "127.0.0.1:7601"

[ui]                                     # optional; absent = no UI listener
bind = "127.0.0.1:7602"

[request_log]
enabled = true
retain  = "30d"

[secrets.caller-identity]
file = "/etc/janus/caller-identity.key"  # or env = "VAR"

[[credential]]
ref      = "openrouter-main"
upstream = "openrouter"
env      = "OPENROUTER_API_KEY"          # or file = "/path"

[identity]                               # the daemon default binding
method     = "hmac"
source     = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
# ...

[[upstream]]
name        = "openrouter"
path_prefix = "/openrouter"
base_url    = "https://openrouter.ai/api"
```

## General rules {#rules}

- **Unknown keys are refused**, in every section. A misspelt key is a startup
  error, not a setting that silently does nothing.
- **Settings without effect are refused.** A key that would change nothing where
  it is written — an `[admin]` setting with no `admin.bind`, `[plugin.tls]` on a
  linked plugin, a binding field the binding's method does not use — is a
  startup error that names its location (`admin.tls`,
  `upstream[openai].route[/v1/models].identity`, ...).
- **Every refusal happens before the data-plane listener binds.** A daemon whose
  config fails any rule in this document, or whose named files, secrets, keys or
  JWKS endpoints cannot be used, does not start.

## Data directory {#data-dir}

The data directory holds the daemon's own state: the admin socket
(`admin.sock`), the admin token (`admin-token`), the database (`janus.db`) and
the credential pool's slots (`credentials/<ref>/`). It is, in order:

1. the top-level `data_dir` key;
2. else `$JANUS_DATA_DIR`;
3. else `$HOME/.janus`.

With none of the three available the daemon refuses to start. The directory is
created mode `0700` when absent. The `janus` CLI resolves it the same way to
find the admin socket — see <ref target="docs/admin-surface#cli">the janus
CLI</ref>.

## Listeners {#listeners}

There are three TCP listeners and one unix socket, kept apart for the reasons in
<ref target="docs/architecture#admin-surface">the admin surface is a separate
listener</ref>.

- **`[data_plane]`** (required) — `bind`: the address callers are pointed at.
- **`[admin]`** (optional) — the admin unix socket at `<data_dir>/admin.sock`
  always exists; this section only adds the TCP admin listener.
  - `bind` — the TCP address. Every other `[admin]` key is refused without it.
  - `allow_non_loopback` (default `false`) — required for a non-loopback `bind`.
  - `tls = { cert, key }` — PEM certificate and key files. Required for a
    non-loopback `bind` unless `allow_plaintext = true`.
  - `allow_plaintext` (default `false`) — states that something outside Janus
    encrypts the hop. Refused together with `tls`.
- **`[ui]`** (optional) — the UI listener, which serves the UI and the admin API
  beside it. Without the section the listener does not exist.
  - `bind` (required) — the TCP address.
  - `tls = { cert, key }` — required for a non-loopback `bind` unless
    `allow_plaintext = true`.
  - `allow_plaintext` (default `false`) — refused together with `tls`.
  - `assets_dir` — the directory of the built UI. Defaults to the `ui/dist`
    directory of the source tree the daemon was built from. A directory without
    an `index.html` refuses startup.

The listeners must use distinct addresses. An unspecified address (`0.0.0.0`,
`[::]`) holds its port on every interface, so it collides with any listener on
the same port; port `0` (an OS-assigned port) never collides.

## Request log {#request-log}

`[request_log]` (optional):

- `enabled` (default `true`) — with `false` no record is written.
- `retain` (default `"30d"`) — a duration (`"7d"`, `"12h"`); rows older than
  this are deleted by a periodic job, which keeps running when the log is
  disabled so earlier rows still age out.

What a record holds and how it is queried is in
<ref target="docs/admin-surface#request-log">the request log</ref>.

## Secrets {#secrets}

`[secrets.<name>]` declares a named secret that an `hmac` binding's `secret_ref`
refers to. Each holds exactly one of:

- `env = "VAR"` — the value of an environment variable;
- `file = "/path"` — the contents of a file.

The material is the exact bytes, with nothing trimmed: a trailing newline in the
file is part of the secret. A `secret_ref` naming an undeclared secret, an unset
variable, an unreadable file or an empty secret refuses startup.

## Config credentials {#credentials}

The credential pool holds two kinds of credential. **Pool slots** live in
`<data_dir>/credentials/`, one directory per credential, and are added, rotated
and removed through the <ref target="docs/admin-surface#credentials-endpoint">
credentials endpoints</ref> without a restart; they are never listed in config.
**Config credentials** are declared with `[[credential]]`:

- `ref` (required) — the credential's name, as plugins and the admin surface
  address it. Non-empty, only `[A-Za-z0-9._-]`, no leading `.`, not `default`
  (reserved for the default-chain peek), and unique.
- `upstream` (required) — `anthropic`, `openai` or `openrouter`.
- `label` (optional) — for operators.
- exactly one of:
  - `env = "VAR"` — a bare API key in an environment variable (surrounding
    whitespace is not part of the key);
  - `file = "/path"` — a vendor CLI's credential file (a JSON document) or a
    bare API key. The file is re-read when it changes on disk, and a refreshed
    credential is written back to it.

A config credential's existence and material belong to the config: the admin
surface can enable, disable and refresh it, but not replace its material or
delete it. The default chain for an upstream tries config credentials in
declaration order, then pool slots oldest first, and picks the first `active`
one.

## Upstreams, bindings and routes {#upstreams}

Each `[[upstream]]` mounts one upstream implementation on the data plane:

- `name` (required) — `anthropic`, `openai` or `openrouter`; each at most once.
  It is also the `upstream` value of the plugin contract and the request log.
- `path_prefix` (required) — the data-plane path prefix routed to this upstream,
  such as `/anthropic`. It starts with `/`, does not end with `/`, and is not
  `/` alone.
- `base_url` (required) — an `http` or `https` URL with a host and no query or
  fragment. The request path, with the prefix removed, is appended to its path.
  (One exception: a ChatGPT-account OAuth credential for `openai` is valid only
  at the ChatGPT Codex backend, so its requests are targeted there.)
- `[upstream.identity]` (optional) — this upstream's binding.
- `[[upstream.route]]` (optional, repeatable) — `path` and `identity`: a
  path-specific binding.

A request belongs to the upstream whose `path_prefix` is a whole-segment prefix
of its path: `/openai` matches `/openai/v1/models` but not `/openai2/v1/models`.
Prefixes that are equal or nested (`/openai` and `/openai/v2`) are refused, so
at most one upstream matches. A path under no prefix is a 404.

Identity bindings are declared at three levels, each with the same block shape:

1. `[identity]` — the daemon default.
2. `[upstream.identity]` — merged over `[identity]` field by field. The
   correlation setting (`correlation_claim` / `correlation`) is one logical
   field, and so is the JWT key source (`jwks_url` / `public_key_file`): setting
   either half replaces both. An upstream block that names a different `method`
   from the default inherits nothing.
3. A route's `identity` — replaces the resolved upstream binding outright; it is
   never merged.

An upstream with neither `[upstream.identity]` nor `[identity]` has no
upstream-level binding, so only its routes accept requests. The fields each
method takes, and how a route path is matched, are in
<ref target="docs/caller-identity#binding">binding an extractor to a request
type</ref>.

## Plugin {#plugin}

`[plugin]` is optional; without it the daemon is a pass-through proxy that makes
no plugin call. Its keys and rules are in
<ref target="docs/plugin-contract#plugin-config">configuring the plugin</ref>.
