---
title: Claude login keeper verification
type: reference
status: draft
tags: [janus, plugin, example, verification, oauth]
---

# Claude login keeper verification

This page records two runs of the
<ref target="examples/claude-login-keeper/README">Claude login keeper</ref>:

- **Run (a)** uses a real `janus-daemon`, a stand-in Anthropic upstream, a
  stand-in token endpoint, and `tests/fake-claude.sh` in place of `claude`.
- **Run (b)** uses the real `claude` binary (Claude Code 2.1.319, macOS) in the
  keeper's isolated directories. The flow reaches the authorize step and is then
  cancelled. No real login was completed.

All tokens are FAKE (`sk-ant-oat01-FAKE-…`). The stand-ins log only the first 21
characters of a token. The keeper's log has no `sk-ant-` string at all. `$DEMO`
is the run's private directory below `$HOME` (mode `0700`). On macOS the daemon
refuses a plugin socket below `/tmp`, because `/private/tmp` is world-writable.
The ports are 27700 (data plane), 27701 (upstream), 27702 (token endpoint),
27711 and 27712 (the keeper pages).

## Results {#results}

| Scenario | Seen |
|---|---|
| Revoked token | The first request gets the upstream's 401, and the daemon marks the credential `invalid`. Requests sent after the keeper has the `credential_invalid` event (0.5 s later here) get 403 `claude login needs re-authorization: …`. |
| Debounce (2 s) | No flow 1 s after the burst. One flow after 4 s. The stub ran once. |
| URL exposed | The flow is `awaiting_browser` with both URLs. The URL appears in one `login_url` JSON log line, on the page, in the deny reason, and from the CLI. |
| Page headers | `Content-Security-Policy` with `frame-ancestors 'none'`, `X-Frame-Options: DENY`, `nosniff`, `no-store`, `no-referrer`. |
| Foreign `Host` (DNS rebinding) | 403 `unexpected Host header`. |
| Wrong code | Re-prompted. `notice` is `Invalid code. Please make sure the full code was copied.` and the flow keeps waiting. |
| Refused login | `failed` with the CLI's reason. Backoff 5 s. A request inside the backoff starts no flow. The second flow starts after the backoff. |
| Keeper SIGKILLed mid-flow | Requests get 503 `plugin_unreachable` (fail-closed). After the restart, the startup check finds `invalid`, and a new flow starts. |
| CLI `login` | With flow 1 already running for the credential, the CLI follows that flow: it prints the URL, takes the code on stdin, prints `Logged in; credential claude-sub rotated.` and exits 0. Flow 1 then reads `succeeded`; no other flow was started. |
| Rotation | The credential is `active` with the new expiry. `keeper/claude/` and `keeper/pending/` are empty. The next request gets 200, and the upstream saw `sk-ant-oat01-FAKE-NEW`. |
| Expiring credential, refresh refused (400 `invalid_grant`) | The pool keeps `active` and only records `last_error`. The periodic check refreshes once, records `refresh_refused`, and starts a flow. Requests are still served (200) with the old access token. A check 10 s later does not call the token endpoint again (one call in total). After `expires_at`, requests get 403 with the authorize URL. |
| Login while the daemon is down | `failed: logged in, but the rotation failed: admin surface unreachable…`, and `keeper/pending/claude-sub.json` is kept. With the daemon back, the next attempt finds the kept login, rotates it and serves 200, without a new flow. The flow then reads `succeeded`. |
| Credential fixed by hand during a flow | The next check finds it `active`: the flow ends as `cancelled` with `the credential is usable again` (`event` `flow_superseded`), `failed_flows` stays 0, and requests get 200. |
| Page: foreign `Origin` | 403 `cross-origin request refused`. |
| Operator start on a healthy credential, then cancel | The flow runs (no health gate) and is `cancelled` with `cancelled by the operator`. `failed_flows` stays 0, no attempt is scheduled, and automatic flows are held for 20 s (`backoff_max`). Requests keep getting 200. |
| SIGTERM | The keeper exits after about 1.7 s. The socket is removed. |
| Stub: keychain shim | `security` on the stub's `PATH` is `$DEMO/keeper/bin/security`. The stub asks it for the unsuffixed `Claude Code-credentials` on each run; the shim answers 44 and logs each of the 6 lookups. The system binary never gets the name: the stub calls `security` only if `PATH` resolves it to the shim. |
| Real `claude`: start | `awaiting_browser`. The printed URL is `https://claude.com/cai/oauth/authorize` with `redirect_uri` `https://platform.claude.com/oauth/code/callback`. The local URL has the same path, with `redirect_uri` `http://localhost:<port>/callback`. |
| Real `claude`: wrong code, cancel | `notice` `Invalid code…`, and the flow keeps waiting. Cancel gives `cancelled`, and the process is gone. |
| Real `claude`: keeper SIGKILLed | The CLI survives its parent. The restarted keeper matches the pid file's process (its own group leader, `claude_bin` and `auth login --claudeai` on its command line), logs `orphan_flow_ended` (`killed` `false`: `SIGTERM` sufficed) and the process is gone. |
| Real `claude`: keychain | The shim logged every `security` call that the CLI made through `PATH`: one lookup each of `Claude Code-credentials-6fb6de07` and `Claude Code-6fb6de07`, and none of `Claude Code-credentials` (`grep -c` 0). `/usr/bin/security find-generic-password` for the keeper's suffixed item exits 44: it does not exist. |

## Setup (a) {#setup}

Build from the repository root with
`CARGO_TARGET_DIR=/tmp/keeper-target cargo build --workspace`. The binaries are
`janus`, `janus-daemon` and `claude-login-keeper`.

`$DEMO/config.toml` (the daemon):

```toml
data_dir = "$DEMO/data"

[data_plane]
bind = "127.0.0.1:27700"

[identity]
method = "opaque"
source = { header = "x-api-key" }

[[upstream]]
name           = "claude"
implementation = "anthropic"
path_prefix    = "/claude"
base_url       = "http://127.0.0.1:27701"
token_url      = "http://127.0.0.1:27702/v1/oauth/token"
  [upstream.credentials]
  kind = "oauth"

[plugin]
endpoint   = "unix://$DEMO/run/keeper.sock"
timeout_ms = 250
```

`$DEMO/keeper.toml`:

```toml
socket            = "$DEMO/run/keeper.sock"
state_dir         = "$DEMO/keeper"
claude_bin        = "/tmp/janus-keeper/examples/claude-login-keeper/tests/fake-claude.sh"
admin             = "unix:$DEMO/data/admin.sock"
http_bind         = "127.0.0.1:27711"
validate_interval = "10s"
refresh_ahead     = "15m"
login_timeout     = "3m"
debounce          = "2s"
backoff_min       = "5s"
backoff_max       = "20s"

[[credential]]
ref       = "claude-sub"
instances = ["claude"]
```

`$DEMO/old-credentials.json` is hand-written FAKE material. Its `expiresAt` is a
day ahead, so only the stand-in's 401 makes it invalid:

```json
{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE-OLD-0000","refreshToken":"sk-ant-ort01-FAKE-OLD-0000","expiresAt":1794147805000,"scopes":["user:inference"],"subscriptionType":"max"}}
```

`$DEMO/mock.js`:

```js
// 27701: stand-in Anthropic. Records the token prefix each request carried;
// a token containing "FAKE-OLD" is answered 401 "OAuth token has been revoked.".
// 27702: stand-in token endpoint; refuses every refresh with 400 invalid_grant.
const http = require("http");
const fs = require("fs");
const log = (f, o) => fs.appendFileSync(__dirname + "/" + f, JSON.stringify(o) + "\n");
http.createServer((req, res) => {
  req.resume();
  req.on("end", () => {
    const auth = req.headers.authorization || "";
    // Record only a prefix: enough to tell old from new, never the token.
    log("upstream-seen.ndjson", { path: req.url, token: auth.replace(/^Bearer /, "").slice(0, 21) });
    res.setHeader("content-type", "application/json");
    if (auth.includes("FAKE-OLD")) {
      res.statusCode = 401;
      return res.end(JSON.stringify({ type: "error", error: { type: "authentication_error", message: "OAuth token has been revoked." } }));
    }
    res.end(JSON.stringify({ id: "msg_demo", type: "message", model: "claude-demo", content: [{ type: "text", text: "ok" }], usage: { input_tokens: 3, output_tokens: 2 } }));
  });
}).listen(27701, "127.0.0.1");
http.createServer((req, res) => {
  req.resume();
  req.on("end", () => {
    log("token-seen.ndjson", { path: req.url, at: new Date().toISOString() });
    res.statusCode = 400;
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ error: "invalid_grant", error_description: "Refresh token revoked" }));
  });
}).listen(27702, "127.0.0.1");
```

`$DEMO/driver.sh`:

```bash
#!/usr/bin/env bash
# Drives verification (a); writes transcript.txt. Run from $DEMO with bash.
set -u
D="$HOME/.kp-demo"; cd "$D"
export PATH=/tmp/keeper-target/debug:$PATH JANUS_DATA_DIR="$D/data" NO_COLOR=1
T="$D/transcript.txt"; : > "$T"
say() { printf '\n## %s\n' "$*" >> "$T"; }
run() { printf '$ %s\n' "$*" >> "$T"; eval "$@" >> "$T" 2>&1; printf '[exit %s]\n' "$?" >> "$T"; }
BODY='{"model":"claude-demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
post() {
  curl -s -w '\nHTTP %{http_code}\n' -X POST http://127.0.0.1:27700/claude/v1/messages \
    -H "x-api-key: $TOK" -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' -d "$BODY"
}
# The deny reason carries the authorize URL: its query values are redacted.
call() {
  printf '$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"\n' >> "$T"
  post 2>&1 | sed -E 's/=([^&" ]*)/=…/g' >> "$T"
}
wait_port() { for _ in $(seq 100); do nc -z 127.0.0.1 "$1" 2>/dev/null && return; sleep 0.1; done; echo "port $1 never opened" >> "$T"; }
# The keeper's state, URL query values redacted.
flow() { curl -s http://127.0.0.1:27711/status | python3 -c 'import json,sys,re
s=json.load(sys.stdin); f=s["flow"] or {}; c=s["credentials"][0]
red=lambda u: re.sub(r"=([^&]*)", "=…", u) if u else u
print(json.dumps({"flow":f.get("id"),"state":f.get("state"),"auth_url":red(f.get("auth_url")),"local_url":red(f.get("local_url")),"notice":f.get("notice"),"reason":f.get("reason"),"health":c["health"],"health_reason":c.get("reason"),"failed_flows":c["failed_flows"],"next_flow_in_secs":c["next_flow_in_secs"]}, ensure_ascii=False))'; }
events() { grep -E "\"event\":\"($1)\"" keeper.log | sed -E 's/"timestamp":"[^"]*",//; s/,"target":"[^"]*"//; s/"claude":"[^"]*",//; s/=([^&" ]*)/=…/g'; }
runs() { grep -c '^run' keeper/fake-claude/runs; }
start_keeper() {
  printf '$ claude-login-keeper serve --config keeper.toml 2>>keeper.log &\n' >> "$T"
  claude-login-keeper serve --config keeper.toml 2>>keeper.log & echo $! > keeper.pid
  wait_port 27711
}

say "Validate the config; start the stand-ins, the daemon and the keeper"
run janus-daemon check-config --config config.toml '2>/dev/null'
node mock.js & echo $! > mock.pid; wait_port 27701
janus-daemon --config config.toml 2>daemon.log & echo $! > daemon.pid; wait_port 27700
start_keeper
sleep 1
run janus plugin check

say "Import the oauth credential (hand-written FAKE material); create an identity"
run janus credential add --upstream anthropic --ref claude-sub --from-file old-credentials.json
printf '$ TOK=$(janus identity create --identity agent-1)\n' >> "$T"
TOK=$(janus identity create --identity agent-1 2>/dev/null)

say "Anthropic revokes the token: one request, then two more after the keeper has the event"
call; sleep 0.5; call; call
run janus credential show claude-sub
say "1s later (debounce 2s): no flow yet. 3s later: one flow, waiting for authorization"
sleep 1; run flow
sleep 3; run flow
run runs
say "The URL as a structured log line (query values redacted here)"
run "events login_url | tail -1"
say "The loopback page shows it, with headers that forbid framing, sniffing and caching"
run "curl -s http://127.0.0.1:27711/ | grep -o 'Open <a href=\"https://claude.com/cai/oauth/authorize?[^\"]*' | sed -E 's/=([^&]*)/=…/g'"
run "curl -s -D - -o /dev/null http://127.0.0.1:27711/ | grep -iE '^(content-security-policy|x-frame-options|x-content-type-options|cache-control|referrer-policy):'"
say "A DNS-rebound name is refused on every route"
run "curl -s -H 'Host: attacker.example:27711' http://127.0.0.1:27711/status -w 'HTTP %{http_code}\n'"
say "A request now is denied with how to re-authorize"
call

say "A wrong code is re-prompted; a refused one fails the flow"
run "curl -s -X POST http://127.0.0.1:27711/login/code -d credential=claude-sub -d code=not-a-code"
sleep 1; run flow
run "curl -s -X POST http://127.0.0.1:27711/login/code -d credential=claude-sub -d code=fail-on-purpose"
sleep 1.5; run flow
say "Backoff: requests inside it start nothing; the next flow starts after 5s"
call
sleep 1; run runs
sleep 5; run flow
run runs
run "events 'login_started|login_failed|backoff'"

say "Keeper killed (SIGKILL) mid-flow, then restarted"
STUB=$(tail -1 keeper/fake-claude/runs | cut -d' ' -f2)
run 'kill -9 $(cat keeper.pid)'
sleep 0.5
run "ps -o pid= -p $STUB || echo 'stub $STUB gone'"
call
run janus plugin status
start_keeper
sleep 4; run flow
run runs

say "The CLI attaches to the running flow and prints the URL; the code is pasted on its stdin"
printf '$ (sleep 2; echo "good-code#state") | claude-login-keeper login --config keeper.toml --credential claude-sub\n' >> "$T"
(sleep 2; echo "good-code#state") | claude-login-keeper login --config keeper.toml --credential claude-sub 2>cli.err > cli.out
echo "[exit $?]" >> cli.out
sed -E 's/=([^& )]*)/=…/g' cli.out >> "$T"; grep -v '^{' cli.err >> "$T"
run janus credential show claude-sub
run 'ls -A keeper/claude/ keeper/pending/'
run "events 'login_authorized|credential_rotated'"
say "Requests are served again, with the new token"
call
run 'tail -2 upstream-seen.ndjson'
run flow

say "An idle credential about to expire whose refresh the vendor refuses (token endpoint: 400 invalid_grant)"
SOON_S=$(( $(date +%s) + 100 ))
printf '{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE-SOON-0000","refreshToken":"sk-ant-ort01-FAKE-SOON-0000","expiresAt":%s000}}\n' "$SOON_S" > soon-credentials.json
run janus credential rotate claude-sub --from-file soon-credentials.json
sleep 12
run 'wc -l < token-seen.ndjson'
run "events credential_checked | tail -1"
run janus credential show claude-sub
run flow
say "The access token still works: requests are served; later checks do not ask the token endpoint again"
call
sleep 10
run 'wc -l < token-seen.ndjson'
say "Past expires_at the credential is unusable: requests are denied (about 75s later)"
sleep $(( SOON_S - $(date +%s) + 1 ))
call

say "Login succeeds while the daemon is down: the rotation fails, the login is kept"
run 'kill -TERM $(cat daemon.pid); sleep 1'
run "curl -s -X POST http://127.0.0.1:27711/login/code -H 'Origin: http://127.0.0.1:27711' -d credential=claude-sub -d 'code=good-code%23state' -o /dev/null -w 'HTTP %{http_code}\n'"
sleep 1.5; run flow
run 'ls -A keeper/claude/ keeper/pending/'
say "Daemon back: the next attempt rotates the kept login in, without a new flow"
printf '$ janus-daemon --config config.toml 2>>daemon.log &\n' >> "$T"
janus-daemon --config config.toml 2>>daemon.log & echo $! > daemon.pid; wait_port 27700
sleep 12
run runs
run "events 'rotation_failed|pending_login_found|credential_rotated' | tail -3"
run 'ls -A keeper/pending/'
call
run 'tail -1 upstream-seen.ndjson'
run flow

say "Revoked again; the operator rotates good material by hand during the flow: the next check ends the flow"
run janus credential rotate claude-sub --from-file old-credentials.json '>/dev/null'
call; sleep 3; run flow
printf '{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE-HAND-0000","refreshToken":"sk-ant-ort01-FAKE-HAND-0000","expiresAt":%s}}\n' "$(( ($(date +%s) + 28800) * 1000 ))" > hand-credentials.json
run janus credential rotate claude-sub --from-file hand-credentials.json '>/dev/null'
sleep 11; run flow
run "events flow_superseded"
call

say "Cross-origin POSTs are refused; an operator start on a healthy credential runs; cancel"
run "curl -s -X POST http://127.0.0.1:27711/login/start -H 'Origin: https://evil.example' -d credential=claude-sub -w 'HTTP %{http_code}\n'"
run "curl -s -X POST http://127.0.0.1:27711/login/start -d credential=claude-sub -w 'HTTP %{http_code}\n'"
sleep 1.5; run flow
run "curl -s -X POST http://127.0.0.1:27711/login/cancel -d credential=claude-sub -w 'HTTP %{http_code}\n'"
sleep 2.5; run flow
run "events automatic_flows_paused"
call

say "SIGTERM: the keeper exits within the grace period and removes its socket"
run 'kill -TERM $(cat keeper.pid); for i in $(seq 50); do kill -0 $(cat keeper.pid) 2>/dev/null || break; sleep 0.1; done; echo "exited after about ${i}00ms"'
run 'ls -A run/'
kill -TERM "$(cat daemon.pid)" "$(cat mock.pid)"
say "Every keychain call of the stub went to the keeper's security shim"
run 'cat keeper/fake-claude/security.path keeper/fake-claude/security.rc; sort keeper/run/security.log | uniq -c'
say "Token values in the keeper's log"
run "grep -c 'sk-ant-' keeper.log"
```

## Transcript (a) {#transcript}

Query values of every authorize URL are replaced by `…`. The URLs are the
stand-in's, but the redaction is the same as for the real binary.

```text
## Validate the config; start the stand-ins, the daemon and the keeper
$ janus-daemon check-config --config config.toml 2>/dev/null
config OK
data_dir    $DEMO/data
data_plane  127.0.0.1:27700
plugin      unix
credentials 0
upstream    claude (anthropic) /claude identity=opaque strategy=ordered failover=false
[exit 0]
$ claude-login-keeper serve --config keeper.toml 2>>keeper.log &
$ janus plugin check
plugin unix unix://$DEMO/run/keeper.sock: PASSED
  ok   path_permissions: 5 directories checked: none writable by group or others, all owned by root, the daemon's user or the socket directory's owner
         0750 uid=502 $DEMO/run
         0700 uid=502 $DEMO
         0750 uid=502 $HOME
         0755 uid=0 /Users
         0755 uid=0 /
  ok   connect: connected to $DEMO/run/keeper.sock, served by an allowed uid
[exit 0]

## Import the oauth credential (hand-written FAKE material); create an identity
$ janus credential add --upstream anthropic --ref claude-sub --from-file old-credentials.json
added credential claude-sub
ref                 claude-sub
upstream            anthropic
kind                oauth
label               -
tags                -
source              pool
status              active
rate limited until  -
expires             2026-11-08T14:23:25Z
last used           -
requests            0
errors              0
last error          -
[exit 0]
$ TOK=$(janus identity create --identity agent-1)

## Anthropic revokes the token: one request, then two more after the keeper has the event
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"type":"error","error":{"type":"authentication_error","message":"OAuth token has been revoked."}}
HTTP 401
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"denied","message":"claude login needs re-authorization: open http://127.0.0.1:27711/ on the keeper host, or run `claude-login-keeper login --credential claude-sub` there","type":"janus_error"},"type":"error"}
HTTP 403
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"denied","message":"claude login needs re-authorization: open http://127.0.0.1:27711/ on the keeper host, or run `claude-login-keeper login --credential claude-sub` there","type":"janus_error"},"type":"error"}
HTTP 403
$ janus credential show claude-sub
ref                 claude-sub
upstream            anthropic
kind                oauth
label               -
tags                -
source              pool
status              invalid
rate limited until  -
expires             2026-11-08T14:23:25Z
last used           2026-11-07T15:56:55Z
requests            1
errors              1
last error          rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.
[exit 0]

## 1s later (debounce 2s): no flow yet. 3s later: one flow, waiting for authorization
$ flow
{"flow": null, "state": null, "auth_url": null, "local_url": null, "notice": null, "reason": null, "health": "needs_login", "health_reason": "401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 0, "next_flow_in_secs": 0}
[exit 0]
$ flow
{"flow": 1, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ runs
1
[exit 0]

## The URL as a structured log line (query values redacted here)
$ events login_url | tail -1
{"level":"INFO","fields":{"message":"authorize this login: open auth_url, then paste the code it shows","event":"login_url","credential_ref":"claude-sub","auth_url":"https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…"}}
[exit 0]

## The loopback page shows it, with headers that forbid framing, sniffing and caching
$ curl -s http://127.0.0.1:27711/ | grep -o 'Open <a href="https://claude.com/cai/oauth/authorize?[^"]*' | sed -E 's/=([^&]*)/=…/g'
Open <a href=…&amp;client_id=…&amp;redirect_uri=…&amp;state=…
[exit 0]
$ curl -s -D - -o /dev/null http://127.0.0.1:27711/ | grep -iE '^(content-security-policy|x-frame-options|x-content-type-options|cache-control|referrer-policy):'
content-security-policy: default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'
x-frame-options: DENY
x-content-type-options: nosniff
cache-control: no-store
referrer-policy: no-referrer
[exit 0]

## A DNS-rebound name is refused on every route
$ curl -s -H 'Host: attacker.example:27711' http://127.0.0.1:27711/status -w 'HTTP %{http_code}\n'
unexpected Host header
HTTP 403
[exit 0]

## A request now is denied with how to re-authorize
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"denied","message":"claude login needs re-authorization: open https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=… , sign in, and paste the code it shows at http://127.0.0.1:27711/ (credential claude-sub)","type":"janus_error"},"type":"error"}
HTTP 403

## A wrong code is re-prompted; a refused one fails the flow
$ curl -s -X POST http://127.0.0.1:27711/login/code -d credential=claude-sub -d code=not-a-code
[exit 0]
$ flow
{"flow": 1, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": "Invalid code. Please make sure the full code was copied.", "reason": null, "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ curl -s -X POST http://127.0.0.1:27711/login/code -d credential=claude-sub -d code=fail-on-purpose
[exit 0]
$ flow
{"flow": 1, "state": "failed", "auth_url": null, "local_url": null, "notice": null, "reason": "Login failed: Request failed with status code 400", "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 1, "next_flow_in_secs": 3}
[exit 0]

## Backoff: requests inside it start nothing; the next flow starts after 5s
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"denied","message":"claude login needs re-authorization: open http://127.0.0.1:27711/ on the keeper host, or run `claude-login-keeper login --credential claude-sub` there","type":"janus_error"},"type":"error"}
HTTP 403
$ runs
1
[exit 0]
$ flow
{"flow": 2, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 1, "next_flow_in_secs": null}
[exit 0]
$ runs
2
[exit 0]
$ events 'login_started|login_failed|backoff'
{"level":"INFO","fields":{"event":"login_started","credential_ref":"claude-sub","pid":81052,"config_dir":"$DEMO/keeper/claude"}}
{"level":"WARN","fields":{"event":"login_failed","credential_ref":"claude-sub","reason":"Login failed: Request failed with status code 400"}}
{"level":"INFO","fields":{"event":"backoff","credential_ref":"claude-sub","wait_secs":5,"failed_flows":1,"retry":true}}
{"level":"INFO","fields":{"event":"login_started","credential_ref":"claude-sub","pid":81107,"config_dir":"$DEMO/keeper/claude"}}
[exit 0]

## Keeper killed (SIGKILL) mid-flow, then restarted
$ kill -9 $(cat keeper.pid)
[exit 0]
$ ps -o pid= -p 81107 || echo 'stub 81107 gone'
stub 81107 gone
[exit 0]
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"plugin_unreachable","message":"the policy plugin could not be consulted (plugin_unreachable)","type":"janus_error"},"type":"error"}
HTTP 503
$ janus plugin status
plugin      unix
endpoint    unix://$DEMO/run/keeper.sock
health      unreachable
last ok     2026-11-07T15:57:03Z
last error  cannot connect to plugin socket $DEMO/run/keeper.sock: Connection refused (os error 61)
[exit 0]
$ claude-login-keeper serve --config keeper.toml 2>>keeper.log &
$ flow
{"flow": 1, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ runs
3
[exit 0]

## The CLI attaches to the running flow and prints the URL; the code is pasted on its stdin
$ (sleep 2; echo "good-code#state") | claude-login-keeper login --config keeper.toml --credential claude-sub
Open this URL, sign in, then paste the code it shows here:

  https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…

(From a browser on this host this one completes by itself:
  https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…)

Paste code: 
Logged in; credential claude-sub rotated.
[exit 0]
code submitted
$ janus credential show claude-sub
ref                 claude-sub
upstream            anthropic
kind                oauth
label               -
tags                -
source              pool
status              active
rate limited until  -
expires             2026-11-07T23:57:15Z
last used           2026-11-07T15:56:55Z
requests            1
errors              1
last error          rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.
[exit 0]
$ ls -A keeper/claude/ keeper/pending/
keeper/claude/:

keeper/pending/:
[exit 0]
$ events 'login_authorized|credential_rotated'
{"level":"INFO","fields":{"event":"login_authorized","credential_ref":"claude-sub","expires_at":"2026-11-07T23:57:15+00:00"}}
{"level":"INFO","fields":{"event":"credential_rotated","credential_ref":"claude-sub","status":"Active","expires_at":"2026-11-07T23:57:15+00:00"}}
[exit 0]

## Requests are served again, with the new token
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2}}
HTTP 200
$ tail -2 upstream-seen.ndjson
{"path":"/v1/messages","token":"sk-ant-oat01-FAKE-OLD"}
{"path":"/v1/messages","token":"sk-ant-oat01-FAKE-NEW"}
[exit 0]
$ flow
{"flow": 1, "state": "succeeded", "auth_url": null, "local_url": null, "notice": null, "reason": null, "health": "healthy", "health_reason": null, "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]

## An idle credential about to expire whose refresh the vendor refuses (token endpoint: 400 invalid_grant)
$ janus credential rotate claude-sub --from-file soon-credentials.json
rotated credential claude-sub
ref                 claude-sub
upstream            anthropic
kind                oauth
label               -
tags                -
source              pool
status              active
rate limited until  -
expires             2026-11-07T15:58:56Z
last used           2026-11-07T15:57:16Z
requests            2
errors              1
last error          rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.
[exit 0]
$ wc -l < token-seen.ndjson
       1
[exit 0]
$ events credential_checked | tail -1
{"level":"WARN","fields":{"event":"credential_checked","credential_ref":"claude-sub","outcome":"refresh_refused","expires_at":"2026-11-07T15:58:56+00:00","reason":"admin API answered 502 refresh_failed: refreshing credential \"claude-sub\" failed: credential refresh failed: token endpoint answered 400 Bad Request: invalid_grant"}}
[exit 0]
$ janus credential show claude-sub
ref                 claude-sub
upstream            anthropic
kind                oauth
label               -
tags                -
source              pool
status              active
rate limited until  -
expires             2026-11-07T15:58:56Z
last used           2026-11-07T15:57:16Z
requests            2
errors              2
last error          refresh failed: credential refresh failed: token endpoint answered 400 Bad Request: invalid_grant
[exit 0]
$ flow
{"flow": 2, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "refresh_refused", "health_reason": "admin API answered 502 refresh_failed: refreshing credential \"claude-sub\" failed: credential refresh failed: token endpoint answered 400 Bad Request: invalid_grant", "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]

## The access token still works: requests are served; later checks do not ask the token endpoint again
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2}}
HTTP 200
$ wc -l < token-seen.ndjson
       1
[exit 0]

## Past expires_at the credential is unusable: requests are denied (about 75s later)
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"error":{"code":"denied","message":"claude login needs re-authorization: open https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=… , sign in, and paste the code it shows at http://127.0.0.1:27711/ (credential claude-sub)","type":"janus_error"},"type":"error"}
HTTP 403

## Login succeeds while the daemon is down: the rotation fails, the login is kept
$ kill -TERM $(cat daemon.pid); sleep 1
[exit 0]
$ curl -s -X POST http://127.0.0.1:27711/login/code -H 'Origin: http://127.0.0.1:27711' -d credential=claude-sub -d 'code=good-code%23state' -o /dev/null -w 'HTTP %{http_code}\n'
HTTP 303
[exit 0]
$ flow
{"flow": 2, "state": "failed", "auth_url": null, "local_url": null, "notice": null, "reason": "logged in, but the rotation failed: admin surface unreachable at unix:$DEMO/data/admin.sock: No such file or directory (os error 2)", "health": "refresh_refused", "health_reason": "admin API answered 502 refresh_failed: refreshing credential \"claude-sub\" failed: credential refresh failed: token endpoint answered 400 Bad Request: invalid_grant", "failed_flows": 1, "next_flow_in_secs": 3}
[exit 0]
$ ls -A keeper/claude/ keeper/pending/
keeper/claude/:

keeper/pending/:
claude-sub.json
[exit 0]

## Daemon back: the next attempt rotates the kept login in, without a new flow
$ janus-daemon --config config.toml 2>>daemon.log &
$ runs
4
[exit 0]
$ events 'rotation_failed|pending_login_found|credential_rotated' | tail -3
{"level":"ERROR","fields":{"event":"rotation_failed","credential_ref":"claude-sub","error":"admin surface unreachable at unix:$DEMO/data/admin.sock: No such file or directory (os error 2)"}}
{"level":"INFO","fields":{"event":"pending_login_found","credential_ref":"claude-sub"}}
{"level":"INFO","fields":{"event":"credential_rotated","credential_ref":"claude-sub","status":"Active","expires_at":"2026-11-07T23:58:58+00:00"}}
[exit 0]
$ ls -A keeper/pending/
[exit 0]
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2}}
HTTP 200
$ tail -1 upstream-seen.ndjson
{"path":"/v1/messages","token":"sk-ant-oat01-FAKE-NEW"}
[exit 0]
$ flow
{"flow": 2, "state": "succeeded", "auth_url": null, "local_url": null, "notice": null, "reason": null, "health": "healthy", "health_reason": null, "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]

## Revoked again; the operator rotates good material by hand during the flow: the next check ends the flow
$ janus credential rotate claude-sub --from-file old-credentials.json >/dev/null
[exit 0]
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"type":"error","error":{"type":"authentication_error","message":"OAuth token has been revoked."}}
HTTP 401
$ flow
{"flow": 3, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "needs_login", "health_reason": "rejected by the upstream: 401 Unauthorized: authentication_error: OAuth token has been revoked.", "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ janus credential rotate claude-sub --from-file hand-credentials.json >/dev/null
[exit 0]
$ flow
{"flow": 3, "state": "cancelled", "auth_url": null, "local_url": null, "notice": null, "reason": "the credential is usable again", "health": "healthy", "health_reason": null, "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ events flow_superseded
{"level":"INFO","fields":{"message":"the credential is usable again","event":"flow_superseded","credential_ref":"claude-sub"}}
[exit 0]
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2}}
HTTP 200

## Cross-origin POSTs are refused; an operator start on a healthy credential runs; cancel
$ curl -s -X POST http://127.0.0.1:27711/login/start -H 'Origin: https://evil.example' -d credential=claude-sub -w 'HTTP %{http_code}\n'
cross-origin request refused
HTTP 403
[exit 0]
$ curl -s -X POST http://127.0.0.1:27711/login/start -d credential=claude-sub -w 'HTTP %{http_code}\n'
HTTP 303
[exit 0]
$ flow
{"flow": 4, "state": "awaiting_browser", "auth_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "local_url": "https://claude.com/cai/oauth/authorize?code=…&client_id=…&redirect_uri=…&state=…", "notice": null, "reason": null, "health": "healthy", "health_reason": null, "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ curl -s -X POST http://127.0.0.1:27711/login/cancel -d credential=claude-sub -w 'HTTP %{http_code}\n'
HTTP 303
[exit 0]
$ flow
{"flow": 4, "state": "cancelled", "auth_url": null, "local_url": null, "notice": null, "reason": "cancelled by the operator", "health": "healthy", "health_reason": null, "failed_flows": 0, "next_flow_in_secs": null}
[exit 0]
$ events automatic_flows_paused
{"level":"INFO","fields":{"event":"automatic_flows_paused","credential_ref":"claude-sub","pause_secs":20}}
[exit 0]
$ curl -s -X POST http://127.0.0.1:27700/claude/v1/messages -H "x-api-key: $TOK" -d "$BODY"
{"id":"msg_demo","type":"message","model":"claude-demo","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2}}
HTTP 200

## SIGTERM: the keeper exits within the grace period and removes its socket
$ kill -TERM $(cat keeper.pid); for i in $(seq 50); do kill -0 $(cat keeper.pid) 2>/dev/null || break; sleep 0.1; done; echo "exited after about ${i}00ms"
exited after about 1700ms
[exit 0]
$ ls -A run/
[exit 0]

## Every keychain call of the stub went to the keeper's security shim
$ cat keeper/fake-claude/security.path keeper/fake-claude/security.rc; sort keeper/run/security.log | uniq -c
$DEMO/keeper/bin/security
44
   6 find-generic-password -s Claude Code-credentials
[exit 0]

## Token values in the keeper's log
$ grep -c 'sk-ant-' keeper.log
0
[exit 1]
```

## Run (b): the real claude binary {#real}

`$DEMO/keeper-real.toml`. Its `instances` names no real instance, so the keeper
never denies. Its own state directory is `$DEMO/real`:

```toml
socket            = "$DEMO/run/real.sock"
state_dir         = "$DEMO/real"
claude_bin        = "$HOME/.local/bin/claude"
admin             = "unix:$DEMO/data/admin.sock"
http_bind         = "127.0.0.1:27712"
validate_interval = "1h"
login_timeout     = "3m"

[[credential]]
ref       = "claude-sub"
instances = ["claude-real-unused"]
```

`$DEMO/real.sh`:

```bash
#!/usr/bin/env bash
# Verification (b): the real claude binary in the keeper's isolated dirs.
# Starts a flow, shows the authorize URLs (host and path only), sends a
# malformed code, cancels, then checks orphan handling. Never authorizes.
set -u
D="$HOME/.kp-demo"; cd "$D"
export PATH=/tmp/keeper-target/debug:$PATH
T="$D/transcript-real.txt"; : > "$T"
say() { printf '\n## %s\n' "$*" >> "$T"; }
run() { printf '$ %s\n' "$*" >> "$T"; eval "$@" >> "$T" 2>&1; printf '[exit %s]\n' "$?" >> "$T"; }
wait_port() { for _ in $(seq 100); do nc -z 127.0.0.1 "$1" 2>/dev/null && return; sleep 0.1; done; }
urls() { curl -s http://127.0.0.1:27712/status | python3 -c 'import json,sys,urllib.parse as u
f=json.load(sys.stdin)["flow"]; print("state:", f["state"], "| notice:", f.get("notice"))
for k in ("auth_url","local_url"):
    if f.get(k):
        x=u.urlsplit(f[k]); q=u.parse_qs(x.query)
        print(k+":", x.scheme+"://"+x.netloc+x.path, "| params:", ",".join(sorted(q)), "| redirect_uri:", q["redirect_uri"][0].split("?")[0])'; }
state() { curl -s http://127.0.0.1:27712/status | python3 -c 'import json,sys; print(json.load(sys.stdin)["flow"]["state"])'; }
start() {
  printf '$ claude-login-keeper serve --config keeper-real.toml 2>>real.log &\n' >> "$T"
  claude-login-keeper serve --config keeper-real.toml 2>>real.log & echo $! > real.pid; wait_port 27712
}

say "The binary"
run 'readlink ~/.local/bin/claude; ~/.local/bin/claude --version'
say "Start the keeper with the real claude; start a flow from the page"
start
run "curl -s -X POST http://127.0.0.1:27712/login/start -d credential=claude-sub -w 'HTTP %{http_code}\n'"
for _ in $(seq 40); do [ "$(state)" = awaiting_browser ] && break; sleep 0.5; done
run urls
run 'ps -o pid=,pgid=,command= -p $(cat real/run/login.pid)'
run 'ls -A real/claude real/home'
say "The keeper's own keychain item (sha256 of its config dir) was not created"
H=$(printf '%s' "$D/real/claude" | shasum -a 256 | cut -c1-8)
run "/usr/bin/security find-generic-password -s 'Claude Code-credentials-$H' > /dev/null 2>&1"
say "A malformed code is refused by the CLI itself; then cancel"
run "curl -s -X POST http://127.0.0.1:27712/login/code -d credential=claude-sub -d code=not-a-code"
sleep 2; run urls
P=$(cat real/run/login.pid)
run "curl -s -X POST http://127.0.0.1:27712/login/cancel -d credential=claude-sub"
sleep 3; run state
run "ps -o pid= -p $P || echo 'claude $P gone'"
say "Every keychain call of the real CLI, as the security shim logged it (service names only)"
run 'sort real/run/security.log | uniq -c'
say "None named the unsuffixed item of a default config, Claude Code-credentials"
run "grep -c -- '-s Claude Code-credentials\$' real/run/security.log"
say "Keeper SIGKILLed mid-flow: the CLI survives it; the restarted keeper ends it"
run "curl -s -X POST http://127.0.0.1:27712/login/start -d credential=claude-sub"
for _ in $(seq 40); do [ "$(state)" = awaiting_browser ] && break; sleep 0.5; done
P=$(cat real/run/login.pid)
run 'kill -9 $(cat real.pid)'
sleep 2
run "ps -o pid=,command= -p $P"
start
sleep 2
run "ps -o pid= -p $P || echo 'claude $P gone'"
run "grep -o '\"event\":\"orphan_flow_ended\"[^}]*' real.log"
run 'kill -TERM $(cat real.pid); sleep 3; ls -A real/claude'
run "grep -c 'sk-ant-' real.log"
```

The URLs are reduced to scheme, host and path. The output lists only the
parameter names, and the `redirect_uri` without its query.

```text
## The binary
$ readlink ~/.local/bin/claude; ~/.local/bin/claude --version
$HOME/.local/share/claude/versions/2.1.319
2.1.319 (Claude Code)
[exit 0]

## Start the keeper with the real claude; start a flow from the page
$ claude-login-keeper serve --config keeper-real.toml 2>>real.log &
$ curl -s -X POST http://127.0.0.1:27712/login/start -d credential=claude-sub -w 'HTTP %{http_code}\n'
HTTP 303
[exit 0]
$ urls
state: awaiting_browser | notice: None
auth_url: https://claude.com/cai/oauth/authorize | params: client_id,code,code_challenge,code_challenge_method,redirect_uri,response_type,scope,state | redirect_uri: https://platform.claude.com/oauth/code/callback
[exit 0]
$ ps -o pid=,pgid=,command= -p $(cat real/run/login.pid)
81709 81709 $HOME/.local/bin/claude auth login --claudeai
[exit 0]
$ ls -A real/claude real/home
real/claude:
.claude.json
backups

real/home:
[exit 0]

## The keeper's own keychain item (sha256 of its config dir) was not created
$ /usr/bin/security find-generic-password -s 'Claude Code-credentials-6fb6de07' > /dev/null 2>&1
[exit 44]

## A malformed code is refused by the CLI itself; then cancel
$ curl -s -X POST http://127.0.0.1:27712/login/code -d credential=claude-sub -d code=not-a-code
[exit 0]
$ urls
state: awaiting_browser | notice: Invalid code. Please make sure the full code was copied.
auth_url: https://claude.com/cai/oauth/authorize | params: client_id,code,code_challenge,code_challenge_method,redirect_uri,response_type,scope,state | redirect_uri: https://platform.claude.com/oauth/code/callback
local_url: https://claude.com/cai/oauth/authorize | params: client_id,code,code_challenge,code_challenge_method,redirect_uri,response_type,scope,state | redirect_uri: http://localhost:65051/callback
[exit 0]
$ curl -s -X POST http://127.0.0.1:27712/login/cancel -d credential=claude-sub
[exit 0]
$ state
cancelled
[exit 0]
$ ps -o pid= -p 81709 || echo 'claude 81709 gone'
claude 81709 gone
[exit 0]

## Every keychain call of the real CLI, as the security shim logged it (service names only)
$ sort real/run/security.log | uniq -c
   1 find-generic-password -s Claude Code-6fb6de07
   1 find-generic-password -s Claude Code-credentials-6fb6de07
[exit 0]

## None named the unsuffixed item of a default config, Claude Code-credentials
$ grep -c -- '-s Claude Code-credentials$' real/run/security.log
0
[exit 1]

## Keeper SIGKILLed mid-flow: the CLI survives it; the restarted keeper ends it
$ curl -s -X POST http://127.0.0.1:27712/login/start -d credential=claude-sub
[exit 0]
$ kill -9 $(cat real.pid)
[exit 0]
$ ps -o pid=,command= -p 81795
81795 $HOME/.local/bin/claude auth login --claudeai
[exit 0]
$ claude-login-keeper serve --config keeper-real.toml 2>>real.log &
$ ps -o pid= -p 81795 || echo 'claude 81795 gone'
claude 81795 gone
[exit 0]
$ grep -o '"event":"orphan_flow_ended"[^}]*' real.log
"event":"orphan_flow_ended","pid":81795,"killed":false
[exit 0]
$ kill -TERM $(cat real.pid); sleep 3; ls -A real/claude
.claude.json
backups
[exit 0]
$ grep -c 'sk-ant-' real.log
0
[exit 1]
```

## What is proven, and what needs the click {#gaps}

Proven by run (a) and `tests/flow.rs`:

- Detection, debounce, one attempt at a time, backoff and retry.
- A refused refresh token: a flow starts, requests are served until
  `expires_at`, and the token endpoint is not asked again.
- Checks during the debounce and during a flow: a credential that is usable
  again starts no flow, or ends the automatic flow.
- A check that the admin API does not answer changes nothing.
- A late check does not overwrite a newer rotation or a newer
  `credential_invalid`, also during an attempt's own check.
- A 2xx `call_completed` after a `credential_invalid` does not lift the deny.
- Failed and timed-out flows, an operator's cancel (no backoff, a hold of
  `backoff_max`, the next attempt scheduled after the hold), and shutdown.
- An operator start takes the slot at once: a second start, for the same or
  another credential, gets `409 busy` with the running flow's id. Cancel and
  code reach only the named credential's flow.
- A panic in an attempt frees the slot and ends its flow as `failed`.
- A restart mid-flow, and orphan matching (a pid file that names another process
  is left alone).
- The deny reason.
- Rotation through the real admin API, and serving again with the new material.
- Recovery of a login whose rotation failed, per credential: with two
  credentials, each login goes only into its own credential. A 401, 409 or 5xx
  keeps the login; only `400 invalid_material` discards it. A healthy check
  deletes it.
- The page's `Host` check, `Origin` check, headers and JSON answers.
- The `login` CLI: it follows the flow its start names, not an earlier flow of
  the same credential; Ctrl-C ends the flow and its child; an unknown credential
  and a bad config file give exit 2 and one line; a failed login prints its
  reason once.
- `serve` that cannot bind its socket exits 2 at once.
- The child's environment, and the `security` shim: it is first on `PATH`,
  answers 44, and logs service names only.

Proven by run (b) with the real binary:

- `claude auth login --claudeai` runs over pipes in the isolated directories and
  reaches `awaiting_browser`, with a real authorize URL on `claude.com`.
- The CLI answers a malformed code with a notice and keeps waiting.
- Cancel and orphan handling end the real process.
- Every `security` call that the CLI made through `PATH` reached the shim and
  named a suffixed item; none named `Claude Code-credentials`. The keeper's own
  suffixed item does not exist in the login keychain.

Not proven by these runs:

- That the CLI never reaches the keychain other than through `security` on
  `PATH` (by an absolute path, or through the Security framework). Such a call
  would not appear in the shim's log. The claim that it does not rests on the
  binary's code (see
  <ref target="docs/claude-login-keeper#isolation">isolation</ref>).

These need the operator's click:

- That the real CLI, after a real authorization, writes
  `<state_dir>/claude/.credentials.json`, the plaintext fallback that the
  `security` shim forces. This comes from the binary's code (see
  <ref target="docs/claude-login-keeper#isolation">isolation</ref>). If the CLI
  does not write the file, the flow fails with a reason and nothing is rotated.
- That a pasted real `code#state` completes the token exchange, and that the
  pool then serves real requests with the new grant.
- That the new grant stays valid while the operator's own Claude Code keeps
  using its own grant. This is the reason the keeper exists, and only time shows
  it.

## Completing a real login {#procedure}

1. Make sure that the daemon's pool holds the Claude credential as a pool slot.
   Check with `janus credential show claude-sub`. `source` must be `pool`.
2. Write `keeper.toml` from `keeper.example.toml`:
   - `state_dir`: a new directory below `$HOME`.
   - `claude_bin`: the absolute path of `claude`.
   - `[[credential]] ref`: the slot.
   - `instances`: the instances that the slot serves.
3. Run `claude-login-keeper login --config keeper.toml --credential claude-sub`.
   When no keeper runs, the command runs one in its own process.
4. Open the printed URL in a browser, sign in with the subscription account, and
   click **Authorize**. Paste the code that the page shows into the terminal,
   and press Enter.
5. Expect `Logged in; credential claude-sub rotated.` and exit 0.
6. Check the result:
   - `janus credential show claude-sub` shows `status active` and a new
     `expires`.
   - A request through Janus gets 200.
   - `<state_dir>/claude/` does not hold `.credentials.json`, and
     `<state_dir>/pending/` is empty.
   - The operator's item is unchanged. Compare the output of
     `security find-generic-password -s "Claude Code-credentials"` before and
     after (it prints attributes, not the secret).
7. If step 5 prints `claude reported success but …`, the CLI stored the login in
   a location other than the plaintext file. Report that, with the CLI version.
   Nothing was rotated.
