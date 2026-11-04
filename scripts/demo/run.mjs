#!/usr/bin/env node
// Janus demo: a real daemon, a mock upstream and believable traffic.
//
//   node scripts/demo/run.mjs [--seed-only] [--keep] [--ui-port N]
//
// Starts a local mock of the anthropic / openai / openrouter APIs, a real
// janus-daemon in front of it (opaque identity binding, UI listener), adds
// demo credentials and identities through the `janus` CLI, replays a spread of
// traffic through the data plane, then keeps running until Ctrl-C.
//
//   --seed-only   add credentials and identities but send no traffic
//   --verbose     show the daemon's log
//   --keep        keep the temporary data directory on exit
//
// Everything is fake: the API keys are `sk-demo-...` and the upstream is local.

import { spawn, execFile, execFileSync } from 'node:child_process'
import http from 'node:http'
import net from 'node:net'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const DAEMON = process.env.JANUS_DAEMON_BIN ?? path.join(ROOT, 'target/release/janus-daemon')
const CLI = process.env.JANUS_BIN ?? path.join(ROOT, 'target/release/janus')
const args = new Set(process.argv.slice(2))
const uiPortArg = process.argv.indexOf('--ui-port')

const DEMO_TOKEN = 'demo-admin-token-not-a-secret-0000000'
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

for (const bin of [DAEMON, CLI]) {
  if (!fs.existsSync(bin)) {
    console.error(`missing ${bin}; run: cargo build --release --workspace`)
    process.exit(2)
  }
}

// ---- ports (27200-27299) ---------------------------------------------------
async function freePort(from = 27200, to = 27299) {
  for (let p = from; p <= to; p++) {
    const ok = await new Promise((res) => {
      const s = net.createServer()
      s.once('error', () => res(false))
      s.listen(p, '127.0.0.1', () => s.close(() => res(true)))
    })
    if (ok) return p
  }
  throw new Error('no free port in 27200-27299')
}
const claimed = new Set()
async function port(preferred) {
  let p = preferred ?? 27200
  for (;;) {
    p = await freePort(p, 27299)
    if (!claimed.has(p)) {
      claimed.add(p)
      return p
    }
    p++
  }
}

// ---- mock upstream ---------------------------------------------------------
const LATENCY = {
  'claude-haiku-4-5': 90,
  'claude-sonnet-4-5': 320,
  'claude-opus-4-1': 780,
  'gpt-4o-mini': 120,
  'gpt-4.1': 410,
  'o3-mini': 950,
  'meta-llama/llama-3.3-70b-instruct': 260,
  'deepseek/deepseek-chat': 380,
}
const TEXT = [
  'Here is a short summary of the three failing integration tests and the likely cause.',
  'The migration is safe to run: it only adds a nullable column and an index.',
  'Done. I grouped the 14 papers into four themes and listed open questions for each.',
  'The flaky test depends on wall-clock ordering; pinning the fixture clock fixes it.',
]
const mock = { tokenCalls: 0 }

function readBody(req) {
  return new Promise((res) => {
    const chunks = []
    req.on('data', (c) => chunks.push(c))
    req.on('end', () => res(Buffer.concat(chunks).toString()))
  })
}

function json(res, status, body, headers = {}) {
  res.writeHead(status, { 'content-type': 'application/json', ...headers })
  res.end(JSON.stringify(body))
}

const mockServer = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x')
  const raw = await readBody(req)
  let body = {}
  try {
    body = raw ? JSON.parse(raw) : {}
  } catch {
    /* not JSON */
  }

  if (url.pathname === '/oauth/token') {
    mock.tokenCalls++
    return json(res, 200, {
      access_token: 'sk-demo-oauth-access-' + mock.tokenCalls,
      refresh_token: 'sk-demo-oauth-refresh-' + mock.tokenCalls,
      expires_in: 8 * 3600,
      token_type: 'Bearer',
    })
  }

  const [, , vendor, ...rest] = url.pathname.split('/') // /mock/<vendor>/...
  const key = (req.headers['x-api-key'] ?? String(req.headers.authorization ?? '').replace(/^Bearer /, '')) || ''
  const model = body.model ?? 'unknown'
  await sleep(LATENCY[model] ?? 200)

  if (key.includes('throttled'))
    return json(res, 429, { type: 'error', error: { type: 'rate_limit_error', message: 'Number of request tokens has exceeded your per-minute rate limit' } }, { 'retry-after': '3600' })
  if (key.includes('revoked'))
    return json(res, 401, { error: { message: 'Incorrect API key provided: sk-demo-...', type: 'invalid_request_error', code: 'invalid_api_key' } })
  if (model === 'demo-overloaded')
    return json(res, 529, { type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } })
  if (model === 'demo-bad-request')
    return json(res, 400, { type: 'error', error: { type: 'invalid_request_error', message: 'max_tokens: must be greater than 0' } })

  const text = TEXT[Math.abs(hash(raw + key)) % TEXT.length]
  const inTok = 40 + (Math.abs(hash(raw)) % 900)
  const outTok = 20 + (Math.abs(hash(text + model)) % 400)
  const path_ = '/' + rest.join('/')

  if (vendor === 'anthropic' && path_ === '/v1/messages') {
    if (body.stream) {
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' })
      const ev = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`)
      ev('message_start', { type: 'message_start', message: { id: 'msg_demo', type: 'message', role: 'assistant', model, content: [], usage: { input_tokens: inTok, output_tokens: 1 } } })
      ev('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } })
      for (const w of text.split(' ')) {
        ev('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: w + ' ' } })
        await sleep(25)
      }
      ev('content_block_stop', { type: 'content_block_stop', index: 0 })
      ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: outTok } })
      ev('message_stop', { type: 'message_stop' })
      return res.end()
    }
    return json(res, 200, { id: 'msg_demo', type: 'message', role: 'assistant', model, content: [{ type: 'text', text }], stop_reason: 'end_turn', usage: { input_tokens: inTok, output_tokens: outTok } })
  }
  if ((vendor === 'openai' || vendor === 'openrouter') && /chat\/completions$/.test(path_)) {
    const usage = { prompt_tokens: inTok, completion_tokens: outTok, total_tokens: inTok + outTok }
    if (body.stream) {
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' })
      for (const w of text.split(' ')) {
        res.write(`data: ${JSON.stringify({ id: 'chatcmpl-demo', object: 'chat.completion.chunk', model, choices: [{ index: 0, delta: { content: w + ' ' } }] })}\n\n`)
        await sleep(25)
      }
      res.write(`data: ${JSON.stringify({ id: 'chatcmpl-demo', object: 'chat.completion.chunk', model, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage })}\n\n`)
      res.write('data: [DONE]\n\n')
      return res.end()
    }
    return json(res, 200, { id: 'chatcmpl-demo', object: 'chat.completion', model, choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }], usage })
  }
  if (path_ === '/v1/models' || path_ === '/models')
    return json(res, 200, { object: 'list', data: Object.keys(LATENCY).map((id) => ({ id, object: 'model' })) })
  return json(res, 404, { error: { message: 'not found', type: 'not_found_error' } })
})

function hash(s) {
  let h = 0
  for (let i = 0; i < s.length; i++) h = (Math.imul(31, h) + s.charCodeAt(i)) | 0
  return h
}

// ---- lifecycle -------------------------------------------------------------
const tmp = fs.mkdtempSync('/tmp/janus-demo-') // short path: it shows in the UI
const dataDir = path.join(tmp, 'data')
fs.mkdirSync(dataDir, { mode: 0o700 })
const tokenFile = path.join(dataDir, 'admin-token')
fs.writeFileSync(tokenFile, DEMO_TOKEN + '\n', { mode: 0o600 })
const configPath = path.join(tmp, 'janus.toml')
let daemon

function cleanup() {
  try { daemon?.kill('SIGTERM') } catch { /* gone */ }
  try { mockServer.close() } catch { /* gone */ }
  if (!args.has('--keep')) fs.rmSync(tmp, { recursive: true, force: true })
}
process.on('SIGINT', () => { cleanup(); process.exit(0) })
process.on('SIGTERM', () => { cleanup(); process.exit(0) })
process.on('exit', cleanup)

function janus(cliArgs, input) {
  return execFileSync(CLI, ['--config', configPath, ...cliArgs], { input, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'] }).trim()
}

// Async variant for commands that call back into the in-process mock (a sync
// exec would block the event loop the mock runs on).
const janusAsync = (cliArgs) =>
  new Promise((res, rej) =>
    execFile(CLI, ['--config', configPath, ...cliArgs], { encoding: 'utf8' }, (e, out, err) => (e ? rej(new Error(err || String(e))) : res(out.trim()))),
  )

async function main() {
  const mockPort = await port(27290)
  const dataPlane = await port(27200)
  const adminPort = await port()
  const uiPort = await port(uiPortArg > 0 ? Number(process.argv[uiPortArg + 1]) : 27210)
  await new Promise((r) => mockServer.listen(mockPort, '127.0.0.1', r))
  const mockBase = `http://127.0.0.1:${mockPort}`

  fs.writeFileSync(configPath, `data_dir = "${dataDir}"

[data_plane]
bind = "127.0.0.1:${dataPlane}"

[admin]
bind = "127.0.0.1:${adminPort}"

[ui]
bind = "127.0.0.1:${uiPort}"
assets_dir = "${path.join(ROOT, 'ui/dist')}"

[request_log]
enabled = true
retain = "30d"

[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }

[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "${mockBase}/mock/anthropic"
token_url = "${mockBase}/oauth/token"

[[upstream]]
name = "openai"
path_prefix = "/openai"
base_url = "${mockBase}/mock/openai"

  [[upstream.route]]
  path = "/v1/models"
  identity = { method = "none" }

[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "${mockBase}/mock/openrouter"
`)
  janus(['check-config'])

  daemon = spawn(DAEMON, ['--config', configPath], { stdio: ['ignore', 'ignore', args.has('--verbose') ? 'inherit' : 'ignore'] })
  daemon.on('exit', (code) => { console.error(`janus-daemon exited (${code})`); process.exit(1) })
  for (let i = 0; ; i++) {
    try { janus(['status']); break } catch { if (i > 100) throw new Error('daemon did not start'); await sleep(100) }
  }

  const dp = `http://127.0.0.1:${dataPlane}`
  const addKey = (upstream, ref, key, label) => janus(['credential', 'add', '--upstream', upstream, '--ref', ref, '--label', label, '--api-key-stdin'], key + '\n')

  // Identities.
  const ident = (name, label, claims = [], extra = []) =>
    janus(['identity', 'create', '--identity', name, '--label', label, ...claims.flatMap((c) => ['--claim', c]), ...extra])
  const idOf = (name) => JSON.parse(janus(['--json', 'identity', 'list'])).identities.find((i) => i.identity === name).id
  const T = {
    research: ident('agent-research', 'Research agent (nightly literature runs)', ['team=research', 'env=prod']),
    ci: ident('agent-ci', 'CI reviewer bot', ['team=platform', 'env=ci']),
    sandbox: ident('sandbox-42', 'Throwaway sandbox for the onboarding demo', ['env=sandbox'], ['--expires-in', '7d']),
    support: ident('agent-support', 'Support triage assistant', ['team=support', 'env=prod']),
    contractor: ident('contractor-77', 'External contractor (access ended)', ['team=ext']),
    shortlived: ident('sandbox-41', 'Expired sandbox', ['env=sandbox'], ['--expires-in', '2s']),
  }

  // Credentials. Pool order matters: the default chain picks the oldest
  // active slot, so the ones that are about to be throttled or rejected go in
  // first and are tripped before the healthy ones are added.
  const seedOnly = args.has('--seed-only')
  const call = async (upstream, token, reqPath, body, method = 'POST') => {
    const started = Date.now()
    try {
      const r = await fetch(dp + '/' + upstream + reqPath, {
        method,
        headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json', 'anthropic-version': '2023-06-01' },
        body: method === 'GET' ? undefined : JSON.stringify(body),
      })
      await r.text()
      return r.status
    } catch (e) { return String(e) } finally { void started }
  }
  const msg = (model, stream = false) => ({ model, max_tokens: 1024, stream, messages: [{ role: 'user', content: 'Summarise the attached notes.' }] })

  addKey('anthropic', 'anthropic-batch-throttled', 'sk-demo-ant-throttled-0001', 'Batch workloads (hit its rate limit)')
  if (!seedOnly) await call('anthropic', T.research, '/v1/messages', msg('claude-sonnet-4-5'))
  addKey('openai', 'openai-legacy-revoked', 'sk-demo-openai-revoked-0002', 'Old project key (revoked upstream)')
  if (!seedOnly) await call('openai', T.ci, '/v1/chat/completions', msg('gpt-4.1'))

  addKey('anthropic', 'anthropic-ci-disabled', 'sk-demo-ant-ci-0003', 'CI key (paused)')
  janus(['credential', 'disable', 'anthropic-ci-disabled'])
  const oauthFile = path.join(tmp, 'claude-max.json')
  fs.writeFileSync(oauthFile, JSON.stringify({
    claudeAiOauth: {
      accessToken: 'sk-demo-oauth-access-0',
      refreshToken: 'sk-demo-oauth-refresh-0',
      expiresAt: Date.now() + 25 * 60 * 1000,
      scopes: ['user:inference'],
    },
  }), { mode: 0o600 })
  janus(['credential', 'add', '--upstream', 'anthropic', '--ref', 'claude-max-oauth', '--label', 'Claude Max subscription (OAuth)', '--from-file', oauthFile])
  fs.rmSync(oauthFile)
  await janusAsync(['credential', 'refresh', 'claude-max-oauth']) // exercises the mock token_url

  addKey('openai', 'openai-prod', 'sk-demo-openai-prod-0004', 'Production project')
  addKey('openrouter', 'openrouter-main', 'sk-demo-or-main-0005', 'Shared OpenRouter key')
  addKey('openrouter', 'openrouter-sandbox', 'sk-demo-or-sandbox-0006', 'Sandbox spend cap (off)')
  janus(['credential', 'disable', 'openrouter-sandbox'])

  if (!seedOnly) {
    // Traffic. Deterministic pseudo-random spread so reruns look alike.
    const plan = [
      ['anthropic', 'research', 'claude-sonnet-4-5', 14, 0.5],
      ['anthropic', 'research', 'claude-opus-4-1', 4, 0.5],
      ['anthropic', 'ci', 'claude-haiku-4-5', 18, 0.3],
      ['anthropic', 'support', 'claude-sonnet-4-5', 8, 0.6],
      ['openai', 'ci', 'gpt-4.1', 7, 0.2],
      ['openai', 'support', 'gpt-4o-mini', 12, 0.4],
      ['openai', 'research', 'o3-mini', 3, 0],
      ['openrouter', 'sandbox', 'meta-llama/llama-3.3-70b-instruct', 6, 0.5],
      ['openrouter', 'research', 'deepseek/deepseek-chat', 5, 0.3],
    ]
    const jobs = []
    let seed = 7
    const rnd = () => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff)
    for (const [up, who, model, n, streamP] of plan)
      for (let i = 0; i < n; i++) jobs.push({ up, who, model, stream: rnd() < streamP })
    jobs.sort(() => rnd() - 0.5)
    const route = (up) => (up === 'anthropic' ? '/v1/messages' : '/v1/chat/completions')
    for (let i = 0; i < jobs.length; i += 4)
      await Promise.all(jobs.slice(i, i + 4).map((j) => call(j.up, T[j.who], route(j.up), msg(j.model, j.stream))))

    // Identity-less route, then denials: unknown token, no token, revoked, expired.
    await call('openai', '', '/v1/models', null, 'GET')
    await call('openai', 'jt_not-a-real-token-xxxxxxxxxxxxxxxx', '/v1/chat/completions', msg('gpt-4.1'))
    await call('anthropic', 'jt_stale-copy-of-a-token-yyyyyyyyyyyyyyy', '/v1/messages', msg('claude-haiku-4-5'))
    await call('openrouter', T.contractor, '/v1/chat/completions', msg('deepseek/deepseek-chat'))
    await call('anthropic', T.contractor, '/v1/messages', msg('claude-sonnet-4-5'))
    janus(['identity', 'revoke', idOf('contractor-77')])
    await call('anthropic', T.contractor, '/v1/messages', msg('claude-sonnet-4-5'))
    await call('openai', T.contractor, '/v1/chat/completions', msg('gpt-4o-mini'))
    await sleep(2200)
    await call('anthropic', T.shortlived, '/v1/messages', msg('claude-haiku-4-5'))
    // A last handful so recent activity is fresh.
    for (const who of ['research', 'ci', 'support'])
      await call('anthropic', T[who], '/v1/messages', msg('claude-sonnet-4-5', true))
    // Errors from upstream.
    await call('anthropic', T.ci, '/v1/messages', msg('demo-overloaded'))
    await call('anthropic', T.research, '/v1/messages', msg('demo-bad-request'))
    await call('openrouter', T.sandbox, '/v1/chat/completions', msg('demo-overloaded'))
  } else {
    janus(['identity', 'revoke', idOf('contractor-77')])
  }

  console.log('\nJanus demo is running.')
  console.log(`  UI:           http://127.0.0.1:${uiPort}/`)
  console.log(`  admin token:  ${tokenFile}   (value: ${DEMO_TOKEN})`)
  console.log(`  data plane:   ${dp}`)
  console.log(`  data dir:     ${dataDir}`)
  console.log('Ctrl-C to stop.')
  setInterval(() => {}, 1 << 30)
}

main().catch((e) => {
  console.error(e.stderr?.toString() || e)
  cleanup()
  process.exit(1)
})
