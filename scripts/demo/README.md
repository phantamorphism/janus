---
title: Demo and screenshots
type: reference
status: accepted
---

# Janus demo and README screenshots

`run.mjs` starts a real `janus-daemon` against a local mock of the Anthropic,
OpenAI and OpenRouter APIs (including SSE streaming and an OAuth `token_url`),
adds fake credentials and identities through the `janus` CLI, replays a spread
of traffic (successes, upstream 429/401/400/529, revoked/expired/unknown
identities), and then keeps running until Ctrl-C. Only Node (stdlib) is needed.

```sh
cargo build --release --workspace && (cd ui && npm run build)
node scripts/demo/run.mjs            # prints the UI URL (default http://127.0.0.1:27210/)
```

Flags: `--seed-only` (credentials and identities, no traffic), `--verbose`
(daemon log), `--keep` (keep the temp data dir), `--ui-port N`.

The admin token is the fixed fake `demo-admin-token-not-a-secret-0000000` (file
path printed at startup). All keys are `sk-demo-...`; ports are within
27200-27299; the data dir is `/tmp/janus-demo-*` and is removed on exit. No
plugin is configured, so the Plugin view shows "no plugin".

## Regenerating `docs/images/ui-*.png`

With the demo running, drive the UI with `agent-browser` at 1440x900, scale 2
(`agent-browser set viewport 1440 900 2`): open the UI URL, capture the login
gate (`ui-login.png`) before entering the token, paste the demo token, then
screenshot each view. Overview uses a 1440x1300 viewport and Credentials
1440x1100 so the whole page fits; `ui-request-detail.png` is the Requests view
with the drawer open on a successful row.
