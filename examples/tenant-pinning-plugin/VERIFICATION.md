---
title: Tenant-pinning plugin verification
type: reference
status: draft
tags: [janus, plugin, example, verification]
---

# Tenant-pinning plugin verification

This is a recorded end-to-end run of the
<ref target="examples/tenant-pinning-plugin/README">tenant-pinning example
plugin</ref> against a real `janus-daemon`. The upstream is a local stand-in for
Anthropic that records the credential each request arrives with. All keys are
fakes (`sk-demo-...`). The identity tokens are held in shell variables and are
not printed. `$DEMO` is the run's private directory below `$HOME` (mode `0700`).
The daemon refuses a socket below `/tmp` on macOS, because `/private/tmp` is
world-writable.

## Results {#results}

| Scenario | Seen by the caller | Seen by the upstream / the plugin |
|---|---|---|
| Plugin down at daemon start | 503 `plugin_unreachable` | `call_abandoned` (`stage` `resolve`) arrives after the plugin starts |
| `tenant=acme` | 200 | `x-api-key: sk-demo-acme-1111` (`claude-acme`) |
| `tenant=globex` | 200 | `sk-demo-globex-2222` (`claude-globex`) |
| Unlisted `tenant=initech`, `deny_unlisted = false` | 200 | `sk-demo-shared-0000` (default chain, `use_default`) |
| `tier=blocked` | 403, reason `identity blocked-agent is blocked (tier=blocked)` | no upstream call; `call_abandoned` with `refunded: false` |
| `tenant=hooli`, pinned to a key that the upstream rejects | 401 from upstream | `credential_invalid`, then `call_completed` with status 401 |
| Budget 3: calls 1 and 2 | 200, 200 | charged 1/3, 2/3 |
| Call 3, upstream down | 502 `upstream_error` | charged 3/3, then `call_abandoned` (`stage` `forward`) with `refunded: true` |
| Call 3 again, upstream back | 200 | charged 3/3 again: the refund freed the unit |
| Call 4 | 403, `request budget exhausted: 3/3 requests in the current 3600s window` | no upstream call |
| Plugin killed with SIGKILL | 503 `plugin_unreachable` | `call_abandoned` (`stage` `resolve`) arrives after the restart |
| Plugin restarted with `deny_unlisted = true` | `globex` 200; `initech` 403 `tenant "initech" is not allowed by policy` | the policy change takes effect on restart |
| Socket directory made group-writable | `janus plugin check` fails and names the directory and its mode | |
| SIGTERM | the plugin exits within the grace period and removes the socket | |

## Setup {#setup}

Build from the repository root:

```sh
cargo build --workspace     # janus, janus-daemon, tenant-pinning-plugin
```

`$DEMO/config.toml`:

```toml
data_dir = "$DEMO/data"

[data_plane]
bind = "127.0.0.1:27600"

[identity]
method = "opaque"
source = { header = "x-api-key" }   # what the Anthropic SDK sends

[[credential]]
ref      = "claude-shared"          # first in chain order: the default
upstream = "anthropic"
env      = "DEMO_KEY_SHARED"
tags     = ["claude"]

[[credential]]
ref      = "claude-acme"
upstream = "anthropic"
env      = "DEMO_KEY_ACME"
tags     = ["claude"]

[[credential]]
ref      = "claude-globex"
upstream = "anthropic"
env      = "DEMO_KEY_GLOBEX"
tags     = ["claude"]

[[credential]]
ref      = "claude-hooli"
upstream = "anthropic"
env      = "DEMO_KEY_HOOLI"
tags     = ["claude"]

[[upstream]]
name           = "claude"
implementation = "anthropic"
path_prefix    = "/claude"
base_url       = "http://127.0.0.1:27610"
  [upstream.credentials]
  tags = ["claude"]

[plugin]
endpoint   = "unix://$DEMO/run/policy.sock"
timeout_ms = 250
```

`$DEMO/policy.toml` (`policy-strict.toml` is the same file with
`deny_unlisted = true`):

```toml
deny_unlisted = false

[budget]
requests    = 3
window_secs = 3600

[tenants.acme]
pin = { claude = "claude-acme" }

[tenants.globex]
pin = { claude = "claude-globex" }

[tenants.hooli]
pin = { claude = "claude-hooli" }
```

`$DEMO/mock-upstream.js`:

```js
// Stand-in Anthropic upstream on 127.0.0.1:27610. Records the credential each
// request arrived with; answers 401 for the key ending "-revoked".
const http = require("http");
const fs = require("fs");
http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    const seen = { path: req.url, x_api_key: req.headers["x-api-key"] ?? null, authorization: req.headers.authorization ?? null };
    fs.appendFileSync(__dirname + "/upstream-seen.ndjson", JSON.stringify(seen) + "\n");
    res.setHeader("content-type", "application/json");
    if ((seen.x_api_key || "").endsWith("-revoked")) {
      res.statusCode = 401;
      res.end(JSON.stringify({ type: "error", error: { type: "authentication_error", message: "invalid x-api-key" } }));
      return;
    }
    res.end(JSON.stringify({ id: "msg_demo", type: "message", model: "claude-demo",
      content: [{ type: "text", text: "ok" }], seen,
      usage: { input_tokens: 12, output_tokens: 30 } }));
  });
}).listen(27610, "127.0.0.1");
```

The driver script that produced the transcript below. Each `$` line in the
transcript is a command from this script, followed by its output and exit
status:

```bash
#!/usr/bin/env bash
# Drives the end-to-end verification; writes transcript.txt.
set -u
D="$HOME/.janus-plugin-demo"; cd "$D"
export PATH=/tmp/plugin-target/debug:$PATH JANUS_DATA_DIR="$D/data"
export DEMO_KEY_SHARED=sk-demo-shared-0000 DEMO_KEY_ACME=sk-demo-acme-1111 DEMO_KEY_GLOBEX=sk-demo-globex-2222 DEMO_KEY_HOOLI=sk-demo-hooli-revoked
T="$D/transcript.txt"; : > "$T"
say() { printf '\n## %s\n' "$*" >> "$T"; }
run() { printf '$ %s\n' "$*" >> "$T"; eval "$@" >> "$T" 2>&1; printf '[exit %s]\n' "$?" >> "$T"; }
BODY='{"model":"claude-demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
call() { # call <token-var-name>
  printf '$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $%s" -d "$BODY"\n' "$1" >> "$T"
  curl -s -w '\nHTTP %{http_code}\n' -X POST http://127.0.0.1:27600/claude/v1/messages \
    -H "x-api-key: ${!1}" -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' -d "$BODY" >> "$T" 2>&1
}
wait_port() { for _ in $(seq 100); do nc -z 127.0.0.1 "$1" 2>/dev/null && return; sleep 0.1; done; echo "port $1 never opened" >> "$T"; }
start_plugin() { # start_plugin <policy>
  printf '$ tenant-pinning-plugin --policy %s --socket $DEMO/run/policy.sock --daemon-uid $(id -u) 2>>plugin.log &\n' "$1" >> "$T"
  tenant-pinning-plugin --policy "$1" --socket "$D/run/policy.sock" --daemon-uid "$(id -u)" 2>>plugin.log &
  echo $! > plugin.pid
  for _ in $(seq 100); do [ -S run/policy.sock ] && break; sleep 0.1; done
}
start_mock() { node mock-upstream.js & echo $! > mock.pid; wait_port 27610; }

say "Validate the config"
run janus-daemon check-config --config config.toml

say "Start the stand-in upstream and the daemon; the plugin is NOT running yet"
start_mock
janus-daemon --config config.toml 2>daemon.log & echo $! > daemon.pid
wait_port 27600
run janus plugin status
run janus plugin check

say "Issue identities with claims"
for spec in "acme-agent tenant=acme" "globex-agent tenant=globex" "initech-agent tenant=initech" \
            "blocked-agent tenant=acme tier=blocked" "hooli-agent tenant=hooli" "acme-batch tenant=acme"; do
  set -- $spec; name=$1; shift; claims=""; for c in "$@"; do claims="$claims --claim $c"; done
  var=TOK_$(echo "$name" | tr 'a-z-' 'A-Z_')
  printf '$ %s=$(janus identity create --identity %s%s)\n' "$var" "$name" "$claims" >> "$T"
  eval "$var=\$(janus identity create --identity $name $claims 2>>\"$T\")"
done

say "Plugin down at daemon start: fail closed"
call TOK_ACME_AGENT

say "Start the plugin; check the channel"
start_plugin policy.toml
run ls -ld run run/policy.sock
run janus plugin check
run janus plugin status

say "A socket directory writable by its group is refused"
run chmod g+w run
run janus plugin check
run chmod g-w run

say "Tenant acme -> pinned claude-acme"
call TOK_ACME_AGENT
say "Tenant globex -> pinned claude-globex"
call TOK_GLOBEX_AGENT
say "Unlisted tenant initech, deny_unlisted=false -> default chain (claude-shared)"
call TOK_INITECH_AGENT
say "tier=blocked -> denied, reason reaches the caller"
call TOK_BLOCKED_AGENT
say "Tenant hooli -> pinned claude-hooli, which the upstream rejects (credential_invalid)"
call TOK_HOOLI_AGENT

say "Budget (3 per window) for acme-batch: two calls succeed"
call TOK_ACME_BATCH
call TOK_ACME_BATCH
say "Upstream unreachable: the third call is charged, abandoned, and refunded"
run 'kill $(cat mock.pid); sleep 0.3'
call TOK_ACME_BATCH
run 'sleep 0.5'
start_mock
say "Upstream back: the refunded unit lets a third call through"
call TOK_ACME_BATCH
say "Fourth call: budget exhausted -> denied"
call TOK_ACME_BATCH

say "What the upstream received (only real-credential headers; identity tokens never arrive)"
run cat upstream-seen.ndjson

say "Plugin killed (SIGKILL, stale socket left behind): fail closed"
run 'kill -9 $(cat plugin.pid); sleep 0.3'
call TOK_GLOBEX_AGENT
run janus plugin status

say "Plugin restarted, now with deny_unlisted = true"
start_plugin policy-strict.toml
run 'sleep 0.5'
call TOK_GLOBEX_AGENT
call TOK_INITECH_AGENT

say "Request log"
run 'sleep 1; janus log query --limit 30'

say "janus status"
run janus status

say "Plugin log (one JSON line per decision / event)"
run 'sleep 1; cat plugin.log'

say "Graceful stop: SIGTERM removes the socket"
run 'kill -TERM $(cat plugin.pid); sleep 3; ls -A run; ps -p $(cat plugin.pid) || echo plugin exited'
kill "$(cat daemon.pid)" "$(cat mock.pid)"
```

## Transcript {#transcript}

### Validate the config

```text
$ janus-daemon check-config --config config.toml
2026-11-06T12:18:28.560823Z  INFO janus_core::pool: credential pool opened dir=<tempdir>/credentials credentials=4
config OK
data_dir    $DEMO/data
data_plane  127.0.0.1:27600
plugin      unix
credentials 4
upstream    claude (anthropic) /claude identity=opaque strategy=ordered failover=false
[exit 0]
```

### Start the stand-in upstream and the daemon; the plugin is NOT running yet

```text
$ janus plugin status
plugin      unix
endpoint    unix://$DEMO/run/policy.sock
health      unknown
last ok     -
last error  -
[exit 0]
$ janus plugin check
plugin unix unix://$DEMO/run/policy.sock: FAILED
  FAIL path_permissions [plugin_unreachable]: plugin socket directory $DEMO/run does not exist
  FAIL connect: not attempted: the socket path failed the permission walk
janus: the plugin channel check did not pass
[exit 1]
```

### Issue identities with claims

```text
$ TOK_ACME_AGENT=$(janus identity create --identity acme-agent --claim tenant=acme)
created identity 01a1b0cb-cd96-7037-8ecd-a4a5e7ffab23 for "acme-agent" (expires -); the token below is shown once and cannot be retrieved again
$ TOK_GLOBEX_AGENT=$(janus identity create --identity globex-agent --claim tenant=globex)
created identity 01a1b0cb-cda4-7322-bf1f-a8e63b495775 for "globex-agent" (expires -); the token below is shown once and cannot be retrieved again
$ TOK_INITECH_AGENT=$(janus identity create --identity initech-agent --claim tenant=initech)
created identity 01a1b0cb-cdb2-737a-9130-a6cf358f467f for "initech-agent" (expires -); the token below is shown once and cannot be retrieved again
$ TOK_BLOCKED_AGENT=$(janus identity create --identity blocked-agent --claim tenant=acme --claim tier=blocked)
created identity 01a1b0cb-cdbf-7159-99ed-09bd3e68aae6 for "blocked-agent" (expires -); the token below is shown once and cannot be retrieved again
$ TOK_HOOLI_AGENT=$(janus identity create --identity hooli-agent --claim tenant=hooli)
created identity 01a1b0cb-cdcc-706d-80f5-d287022e8078 for "hooli-agent" (expires -); the token below is shown once and cannot be retrieved again
$ TOK_ACME_BATCH=$(janus identity create --identity acme-batch --claim tenant=acme)
created identity 01a1b0cb-cdda-762d-a3b8-8f3d982c346c for "acme-batch" (expires -); the token below is shown once and cannot be retrieved again
```

### Plugin down at daemon start: fail closed

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_AGENT" -d "$BODY"
{"error":{"code":"plugin_unreachable","message":"the policy plugin could not be consulted (plugin_unreachable)","type":"janus_error"},"type":"error"}
HTTP 503
```

### Start the plugin; check the channel

```text
$ tenant-pinning-plugin --policy policy.toml --socket $DEMO/run/policy.sock --daemon-uid $(id -u) 2>>plugin.log &
$ ls -ld run run/policy.sock
drwxr-x---@ 3 demo  staff  96 Nov  6 14:18 run
srw-rw----@ 1 demo  staff   0 Nov  6 14:18 run/policy.sock
[exit 0]
$ janus plugin check
plugin unix unix://$DEMO/run/policy.sock: PASSED
  ok   path_permissions: 5 directories checked: none writable by group or others, all owned by root, the daemon's user or the socket directory's owner
         0750 uid=502 $DEMO/run
         0700 uid=502 $DEMO
         0750 uid=502 $HOME
         0755 uid=0 /Users
         0755 uid=0 /
  ok   connect: connected to $DEMO/run/policy.sock, served by an allowed uid
[exit 0]
$ janus plugin status
plugin      unix
endpoint    unix://$DEMO/run/policy.sock
health      unreachable
last ok     -
last error  plugin socket directory $DEMO/run does not exist
[exit 0]
```

### A socket directory writable by its group is refused

```text
$ chmod g+w run
[exit 0]
$ janus plugin check
plugin unix unix://$DEMO/run/policy.sock: FAILED
  FAIL path_permissions [plugin_unreachable]: plugin socket path is unsafe: $DEMO/run is writable by group (mode 0770)
  FAIL connect: not attempted: the socket path failed the permission walk
janus: the plugin channel check did not pass
[exit 1]
$ chmod g-w run
[exit 0]
```

### Tenant acme -> pinned claude-acme

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_AGENT" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
```

### Tenant globex -> pinned claude-globex

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_GLOBEX_AGENT" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-globex-2222","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
```

### Unlisted tenant initech, deny_unlisted=false -> default chain (claude-shared)

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_INITECH_AGENT" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-shared-0000","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
```

### tier=blocked -> denied, reason reaches the caller

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_BLOCKED_AGENT" -d "$BODY"
{"error":{"code":"denied","message":"identity blocked-agent is blocked (tier=blocked)","type":"janus_error"},"type":"error"}
HTTP 403
```

### Tenant hooli -> pinned claude-hooli, which the upstream rejects (credential_invalid)

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_HOOLI_AGENT" -d "$BODY"
{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}
HTTP 401
```

### Budget (3 per window) for acme-batch: two calls succeed

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_BATCH" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_BATCH" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
```

### Upstream unreachable: the third call is charged, abandoned, and refunded

```text
$ kill $(cat mock.pid); sleep 0.3
[exit 0]
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_BATCH" -d "$BODY"
{"error":{"code":"upstream_error","message":"the upstream could not be reached","type":"janus_error"},"type":"error"}
HTTP 502
$ sleep 0.5
[exit 0]
```

### Upstream back: the refunded unit lets a third call through

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_BATCH" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
```

### Fourth call: budget exhausted -> denied

```text
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_ACME_BATCH" -d "$BODY"
{"error":{"code":"denied","message":"request budget exhausted: 3/3 requests in the current 3600s window","type":"janus_error"},"type":"error"}
HTTP 403
```

### What the upstream received (only real-credential headers; identity tokens never arrive)

```text
$ cat upstream-seen.ndjson
{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-globex-2222","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-shared-0000","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-hooli-revoked","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null}
{"path":"/v1/messages","x_api_key":"sk-demo-acme-1111","authorization":null}
[exit 0]
```

### Plugin killed (SIGKILL, stale socket left behind): fail closed

```text
$ kill -9 $(cat plugin.pid); sleep 0.3
[exit 0]
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_GLOBEX_AGENT" -d "$BODY"
{"error":{"code":"plugin_unreachable","message":"the policy plugin could not be consulted (plugin_unreachable)","type":"janus_error"},"type":"error"}
HTTP 503
$ janus plugin status
plugin      unix
endpoint    unix://$DEMO/run/policy.sock
health      unreachable
last ok     2026-11-06T12:18:31Z
last error  cannot connect to plugin socket $DEMO/run/policy.sock: Connection refused (os error 61)
[exit 0]
```

### Plugin restarted, now with deny_unlisted = true

```text
$ tenant-pinning-plugin --policy policy-strict.toml --socket $DEMO/run/policy.sock --daemon-uid $(id -u) 2>>plugin.log &
$ sleep 0.5
[exit 0]
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_GLOBEX_AGENT" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"seen":{"path":"/v1/messages","x_api_key":"sk-demo-globex-2222","authorization":null},"usage":{"input_tokens":12,"output_tokens":30}}
HTTP 200
$ curl -s -X POST http://127.0.0.1:27600/claude/v1/messages -H "x-api-key: $TOK_INITECH_AGENT" -d "$BODY"
{"error":{"code":"denied","message":"tenant \"initech\" is not allowed by policy","type":"janus_error"},"type":"error"}
HTTP 403
```

### Request log

```text
$ sleep 1; janus log query --limit 30
RECEIVED              REQUEST ID                            UPSTREAM            METHOD  PATH                 IDENTITY       OUTCOME                                                                     STATUS  MS
2026-11-06T12:18:32Z  01a1b0cb-d9c5-73f3-8fe5-487fbdebbcd4  claude (anthropic)  POST    /claude/v1/messages  initech-agent  denied(tenant "initech" is not allowed by policy)                           -       1
2026-11-06T12:18:31Z  01a1b0cb-d9b1-7378-9894-0b9813f6f007  claude (anthropic)  POST    /claude/v1/messages  globex-agent   forwarded                                                                   200     4
2026-11-06T12:18:31Z  01a1b0cb-d77a-7216-8fbf-6e859b76810b  claude (anthropic)  POST    /claude/v1/messages  globex-agent   plugin_failed(plugin_unreachable)                                           -       0
2026-11-06T12:18:31Z  01a1b0cb-d61b-7312-8aff-c981ca056ec5  claude (anthropic)  POST    /claude/v1/messages  acme-batch     denied(request budget exhausted: 3/3 requests in the current 3600s window)  -       0
2026-11-06T12:18:31Z  01a1b0cb-d60c-74aa-83fa-5accc8159cfb  claude (anthropic)  POST    /claude/v1/messages  acme-batch     forwarded                                                                   200     5
2026-11-06T12:18:30Z  01a1b0cb-d365-711f-ae5d-07cf1beedcc7  claude (anthropic)  POST    /claude/v1/messages  acme-batch     upstream_error                                                              -       2
2026-11-06T12:18:30Z  01a1b0cb-d20e-7730-8e97-57c3ee3ab41f  claude (anthropic)  POST    /claude/v1/messages  acme-batch     forwarded                                                                   200     1
2026-11-06T12:18:30Z  01a1b0cb-d202-7387-9308-8025de480bd3  claude (anthropic)  POST    /claude/v1/messages  acme-batch     forwarded                                                                   200     1
2026-11-06T12:18:30Z  01a1b0cb-d1f5-738e-b0da-f538ddf8590b  claude (anthropic)  POST    /claude/v1/messages  hooli-agent    forwarded                                                                   401     1
2026-11-06T12:18:29Z  01a1b0cb-d1e8-70ec-bdb0-173014a5e0d6  claude (anthropic)  POST    /claude/v1/messages  blocked-agent  denied(identity blocked-agent is blocked (tier=blocked))                    -       0
2026-11-06T12:18:29Z  01a1b0cb-d1dc-74a5-8c19-b3a5455477f7  claude (anthropic)  POST    /claude/v1/messages  initech-agent  forwarded                                                                   200     1
2026-11-06T12:18:29Z  01a1b0cb-d1cf-7539-a09b-d6995b17926e  claude (anthropic)  POST    /claude/v1/messages  globex-agent   forwarded                                                                   200     2
2026-11-06T12:18:29Z  01a1b0cb-d1ba-74db-8f85-d86e6f8c4670  claude (anthropic)  POST    /claude/v1/messages  acme-agent     forwarded                                                                   200     8
2026-11-06T12:18:28Z  01a1b0cb-cde7-744b-aa4e-ef15575913a2  claude (anthropic)  POST    /claude/v1/messages  acme-agent     plugin_failed(plugin_unreachable)                                           -       0
[exit 0]
```

### janus status

```text
$ janus status
version          0.1.0 (build dev)
started          2026-11-06T12:18:28Z
data plane       127.0.0.1:27600
admin socket     $DEMO/data/admin.sock
admin tcp        -
ui               -
request log      on, 14 rows, retained 30days
dropped records  0
refused conns    0
credentials      4 (3 active, 1 invalid)

UPSTREAM  IMPLEMENTATION  PREFIX   IDENTITY  STRATEGY  CREDENTIALS                                           BASE URL
claude    anthropic       /claude  opaque    ordered   claude-shared,claude-acme,claude-globex,claude-hooli  http://127.0.0.1:27610/

plugin      unix
endpoint    unix://$DEMO/run/policy.sock
health      ok
last ok     2026-11-06T12:18:32Z
last error  cannot connect to plugin socket $DEMO/run/policy.sock: Connection refused (os error 61)
[exit 0]
```

### Plugin log (one JSON line per decision / event)

```text
$ sleep 1; cat plugin.log
{"timestamp":"2026-11-06T12:18:29.783986Z","level":"INFO","fields":{"event":"policy_loaded","path":"policy.toml","tenants":3,"deny_unlisted":false,"budget_requests":3,"budget_window_secs":3600},"target":"tenant_pinning_plugin"}
{"timestamp":"2026-11-06T12:18:29.785293Z","level":"INFO","fields":{"event":"listening","socket":"$DEMO/run/policy.sock","peer_uid":"502"},"target":"tenant_pinning_plugin"}
{"timestamp":"2026-11-06T12:18:29.948991Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d1ba-74db-8f85-d86e6f8c4670","caller_identity":"acme-agent","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-acme","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:29.967879Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d1cf-7539-a09b-d6995b17926e","caller_identity":"globex-agent","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-globex","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:29.980795Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d1dc-74a5-8c19-b3a5455477f7","caller_identity":"initech-agent","upstream":"claude","outcome":"use_default","credential_ref":"","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:29.993265Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d1e8-70ec-bdb0-173014a5e0d6","caller_identity":"blocked-agent","upstream":"claude","outcome":"deny_reason","reason":"identity blocked-agent is blocked (tier=blocked)"},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.006137Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d1f5-738e-b0da-f538ddf8590b","caller_identity":"hooli-agent","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-hooli","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.018922Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d202-7387-9308-8025de480bd3","caller_identity":"acme-batch","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-acme","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.031255Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d20e-7730-8e97-57c3ee3ab41f","caller_identity":"acme-batch","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-acme","budget_used":2,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.374749Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d365-711f-ae5d-07cf1beedcc7","caller_identity":"acme-batch","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-acme","budget_used":3,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.480971Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-cde7-744b-aa4e-ef15575913a2","caller_identity":"acme-agent","stage":"resolve","reason":"plugin_unreachable: plugin socket directory $DEMO/run does not exist","refunded":false},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481179Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d1ba-74db-8f85-d86e6f8c4670","caller_identity":"acme-agent","upstream":"claude","status":200,"credential_ref":"claude-acme","duration_ms":8,"tokens":42,"tokens_total":42,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481310Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d1cf-7539-a09b-d6995b17926e","caller_identity":"globex-agent","upstream":"claude","status":200,"credential_ref":"claude-globex","duration_ms":2,"tokens":42,"tokens_total":42,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481406Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d1dc-74a5-8c19-b3a5455477f7","caller_identity":"initech-agent","upstream":"claude","status":200,"credential_ref":"claude-shared","duration_ms":1,"tokens":42,"tokens_total":42,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481470Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-d1e8-70ec-bdb0-173014a5e0d6","caller_identity":"blocked-agent","stage":"resolve","reason":"denied by the plugin: identity blocked-agent is blocked (tier=blocked)","refunded":false},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481527Z","level":"WARN","fields":{"event":"credential_invalid","request_id":"01a1b0cb-d1f5-738e-b0da-f538ddf8590b","credential_ref":"claude-hooli","upstream":"claude","status":401,"reason":"401 Unauthorized: authentication_error: invalid x-api-key"},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481633Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d1f5-738e-b0da-f538ddf8590b","caller_identity":"hooli-agent","upstream":"claude","status":401,"credential_ref":"claude-hooli","duration_ms":1,"tokens":0,"tokens_total":0,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481705Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d202-7387-9308-8025de480bd3","caller_identity":"acme-batch","upstream":"claude","status":200,"credential_ref":"claude-acme","duration_ms":1,"tokens":42,"tokens_total":42,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481777Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d20e-7730-8e97-57c3ee3ab41f","caller_identity":"acme-batch","upstream":"claude","status":200,"credential_ref":"claude-acme","duration_ms":1,"tokens":42,"tokens_total":84,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:30.481844Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-d365-711f-ae5d-07cf1beedcc7","caller_identity":"acme-batch","stage":"forward","reason":"upstream request failed: error sending request: client error (Connect): tcp connect error: Connection refused (os error 61)","refunded":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:31.052752Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d60c-74aa-83fa-5accc8159cfb","caller_identity":"acme-batch","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-acme","budget_used":3,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:31.057345Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d60c-74aa-83fa-5accc8159cfb","caller_identity":"acme-batch","upstream":"claude","status":200,"credential_ref":"claude-acme","duration_ms":5,"tokens":42,"tokens_total":126,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:31.067959Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d61b-7312-8aff-c981ca056ec5","caller_identity":"acme-batch","upstream":"claude","outcome":"deny_reason","reason":"request budget exhausted: 3/3 requests in the current 3600s window"},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:31.068369Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-d61b-7312-8aff-c981ca056ec5","caller_identity":"acme-batch","stage":"resolve","reason":"denied by the plugin: request budget exhausted: 3/3 requests in the current 3600s window","refunded":false},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:31.442985Z","level":"INFO","fields":{"event":"policy_loaded","path":"policy-strict.toml","tenants":3,"deny_unlisted":true,"budget_requests":3,"budget_window_secs":3600},"target":"tenant_pinning_plugin"}
{"timestamp":"2026-11-06T12:18:31.443257Z","level":"INFO","fields":{"event":"listening","socket":"$DEMO/run/policy.sock","peer_uid":"502"},"target":"tenant_pinning_plugin"}
{"timestamp":"2026-11-06T12:18:31.987330Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d9b1-7378-9894-0b9813f6f007","caller_identity":"globex-agent","upstream":"claude","outcome":"credential_ref","credential_ref":"claude-globex","budget_used":1,"budget_limit":3},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:32.006598Z","level":"INFO","fields":{"event":"resolve_route","request_id":"01a1b0cb-d9c5-73f3-8fe5-487fbdebbcd4","caller_identity":"initech-agent","upstream":"claude","outcome":"deny_reason","reason":"tenant \"initech\" is not allowed by policy"},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:32.680786Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-d77a-7216-8fbf-6e859b76810b","caller_identity":"globex-agent","stage":"resolve","reason":"plugin_unreachable: cannot connect to plugin socket $DEMO/run/policy.sock: Connection refused (os error 61)","refunded":false},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:32.681069Z","level":"INFO","fields":{"event":"call_completed","request_id":"01a1b0cb-d9b1-7378-9894-0b9813f6f007","caller_identity":"globex-agent","upstream":"claude","status":200,"credential_ref":"claude-globex","duration_ms":4,"tokens":42,"tokens_total":42,"failed_over":0,"settled":true},"target":"tenant_pinning_plugin::service"}
{"timestamp":"2026-11-06T12:18:32.681245Z","level":"INFO","fields":{"event":"call_abandoned","request_id":"01a1b0cb-d9c5-73f3-8fe5-487fbdebbcd4","caller_identity":"initech-agent","stage":"resolve","reason":"denied by the plugin: tenant \"initech\" is not allowed by policy","refunded":false},"target":"tenant_pinning_plugin::service"}
[exit 0]
```

### Graceful stop: SIGTERM removes the socket

```text
$ kill -TERM $(cat plugin.pid); sleep 3; ls -A run; ps -p $(cat plugin.pid) || echo plugin exited
  PID TTY           TIME CMD
plugin exited
[exit 0]
```
