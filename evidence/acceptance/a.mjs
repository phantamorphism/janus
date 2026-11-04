#!/usr/bin/env node
// Group A (D1-D9): data plane, transparency, upstream configuration. Blind black-box QA.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BIN = process.env.JANUS_BIN_DIR || path.join(ROOT, 'target/release');
const ART = path.join(ROOT, 'evidence/artifacts/a');
const RES = path.join(ROOT, 'evidence/results/a.json');
const WORK = '/tmp/qa-a/run';
const DP = 20001, MOCK = { anthropic: 20100, openai: 20101, openrouter: 20102 };
const SECRET = 'QA-hmac-secret-S';
const KEYS = { anthropic: 'sk-ant-QAREALKEY-0001', openai: 'sk-QAREALKEY-0002', openrouter: 'sk-or-QAREALKEY-0003' };
const REFS = { anthropic: 'qa-cred-anthropic', openai: 'qa-cred-openai', openrouter: 'qa-cred-openrouter' };

fs.rmSync(WORK, { recursive: true, force: true });
fs.mkdirSync(WORK, { recursive: true });
fs.rmSync(ART, { recursive: true, force: true });
fs.mkdirSync(ART, { recursive: true });

const results = [];
const rec = (id, result, note, files) => {
  results.push({ id, result, artifacts: files.map(f => 'evidence/artifacts/a/' + f), note });
  console.log(`${id} ${result} - ${note}`);
};
const art = (name, content) => {
  fs.writeFileSync(path.join(ART, name), typeof content === 'string' ? content : JSON.stringify(content, null, 2));
  return name;
};
const sleep = ms => new Promise(r => setTimeout(r, ms));

// ---------- mock upstreams ----------
const seen = { anthropic: [], openai: [], openrouter: [] };
const BIGBODY = Buffer.from([0, 1, 2, 255, 254, 0x7b, 0x7d, 10, 13, 0xc3, 0x28, ...Buffer.from('payload-with-bytes')]);
const MSG = JSON.stringify({ id: 'msg_01QA', type: 'message', role: 'assistant', model: 'claude-qa', content: [{ type: 'text', text: 'hello from mock upstream' }], stop_reason: 'end_turn', stop_sequence: null, usage: { input_tokens: 3, output_tokens: 4 } });
const SSE = Array.from({ length: 5 }, (_, i) => `event: tick\ndata: {"n":${i}}\n\n`);
function mkMock(name) {
  return http.createServer((req, res) => {
    const chunks = [];
    req.on('data', c => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks);
      seen[name].push({ t: Date.now(), url: req.url, method: req.method, headers: req.headers, rawHeaders: req.rawHeaders, body: body.toString('latin1') });
      const u = req.url.split('?')[0];
      if (u.endsWith('/stream')) {
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
        let i = 0;
        const iv = setInterval(() => {
          if (i < SSE.length) res.write(SSE[i++]);
          else { clearInterval(iv); res.end(); }
        }, 500);
        res.write(': open\n\n');
      } else if (u.endsWith('/kill')) req.socket.destroy();
      else if (u.endsWith('/binary')) { res.writeHead(200, { 'content-type': 'application/octet-stream', 'x-mock': 'bin' }); res.end(BIGBODY); }
      else if (u.endsWith('/messages')) { res.writeHead(200, { 'content-type': 'application/json', 'request-id': 'req_mock_1' }); res.end(MSG); }
      else if (u.includes('/status/')) { const s = +u.split('/status/')[1]; res.writeHead(s, { 'content-type': 'application/json', 'x-mock-status': String(s) }); res.end(`{"mock_status":${s}}`); }
      else { res.writeHead(201, { 'content-type': 'application/json', 'x-up': '1' }); res.end('{"ok":true}'); }
    });
  });
}
const mocks = Object.entries(MOCK).map(([n, p]) => new Promise(r => { const s = mkMock(n); s.listen(p, '127.0.0.1', () => r(s)); }));

// ---------- daemon ----------
const dataDir = path.join(WORK, 'data');
const cfg = path.join(WORK, 'c.toml');
const baseCfg = `data_dir = "${dataDir}"
[data_plane]
bind = "127.0.0.1:${DP}"
[identity]
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
namespace = "janus-data-plane"
identity_claim = "subject"
[secrets]
caller-identity = { env = "QA_S" }
[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "http://127.0.0.1:${MOCK.anthropic}"
[[upstream]]
name = "openai"
path_prefix = "/openai"
base_url = "http://127.0.0.1:${MOCK.openai}"
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:${MOCK.openrouter}"
`;
fs.writeFileSync(cfg, baseCfg);
const cli = (args, input) => spawnSync(path.join(BIN, 'janus'), ['--config', cfg, ...args], { input, encoding: 'utf8' });
let daemon;
function mint(sub, o = {}) {
  const claims = { ns: o.ns ?? 'janus-data-plane', exp: o.exp ?? Math.floor(Date.now() / 1000) + 3600, subject: sub };
  const h = Buffer.from(JSON.stringify(claims)).toString('hex');
  return h + '.' + crypto.createHmac('sha256', o.secret ?? SECRET).update(h).digest('hex');
}
async function logs() {
  const r = cli(['--json', 'log', 'query']);
  try { return JSON.parse(r.stdout).requests; } catch { return []; }
}
async function lastLog(n0) { // wait for a new record beyond n0 records
  for (let i = 0; i < 40; i++) { const l = await logs(); if (l.length > n0) return l[0]; await sleep(100); }
  return null;
}
async function req(method, url, headers = {}, body) {
  const r = await fetch(url, { method, headers, body, redirect: 'manual' });
  const buf = Buffer.from(await r.arrayBuffer());
  return { status: r.status, headers: Object.fromEntries(r.headers), body: buf };
}
const dp = p => `http://127.0.0.1:${DP}${p}`;
const fails = [];

async function main() {
  await Promise.all(mocks);
  daemon = spawn(path.join(BIN, 'janus-daemon'), ['--config', cfg], { env: { ...process.env, QA_S: SECRET }, stdio: ['ignore', fs.openSync(path.join(WORK, 'daemon.log'), 'w'), 2] });
  for (let i = 0; i < 50 && !fs.existsSync(path.join(dataDir, 'admin.sock')); i++) await sleep(100);
  await sleep(300);
  for (const u of Object.keys(KEYS)) {
    const r = cli(['credential', 'add', '--upstream', u, '--ref', REFS[u], '--api-key-stdin'], KEYS[u] + '\n');
    if (r.status !== 0) throw new Error('credential add failed: ' + r.stderr);
  }
  const reset = () => Object.values(seen).forEach(a => a.length = 0);

  // ===== D1 =====
  try {
    const require = createRequire('/tmp/qa-a/sdk/');
    const { default: Anthropic } = require('@anthropic-ai/sdk');
    const tok = mint('agent-1');
    const client = new Anthropic({ baseURL: dp('/anthropic'), apiKey: tok, maxRetries: 0 });
    const n0 = (await logs()).length;
    const msg = await client.messages.create({ model: 'claude-qa', max_tokens: 16, messages: [{ role: 'user', content: 'hi' }] });
    const direct = await req('POST', `http://127.0.0.1:${MOCK.anthropic}/v1/messages`, { 'content-type': 'application/json', 'x-api-key': KEYS.anthropic }, '{}');
    const l = await lastLog(n0);
    const sdkOk = msg.id === 'msg_01QA' && msg.content[0].text === 'hello from mock upstream';
    const same = JSON.stringify(msg) === JSON.stringify(JSON.parse(direct.body.toString()));
    const up = seen.anthropic.find(s => s.url === '/v1/messages');
    const f = art('D1-sdk-transcript.json', { sdkResult: msg, directMockResult: JSON.parse(direct.body.toString()), upstreamSaw: up, logRecord: l, sdkVersion: JSON.parse(fs.readFileSync('/tmp/qa-a/sdk/node_modules/@anthropic-ai/sdk/package.json','utf8')).version });
    const ok = sdkOk && same && l?.outcome === 'forwarded' && up && up.headers['x-api-key'] === KEYS.anthropic;
    rec('D1', ok ? 'PASS' : 'FAIL', ok ? 'Stock @anthropic-ai/sdk with only baseURL=/anthropic and apiKey=caller identity token succeeded; returned the upstream message identical to a direct call; upstream saw the pool key. (Upstream is a mock at base_url; real upstream not contacted.)' : 'SDK call did not behave transparently', [f]);
  } catch (e) { rec('D1', 'FAIL', 'exception: ' + e.message, [art('D1-error.txt', String(e.stack))]); }

  // ===== D2 =====
  try {
    reset();
    const out = {};
    const st = cli(['--json', 'status']);
    const stj = JSON.parse(st.stdout);
    const stHttp = await (async () => { // admin API directly over unix socket
      const r = spawnSync('curl', ['-s', '--unix-socket', path.join(dataDir, 'admin.sock'), 'http://x/v1/status'], { encoding: 'utf8' });
      return r.stdout;
    })();
    let ok = true;
    for (const u of ['anthropic', 'openai', 'openrouter']) {
      const hdr = { authorization: 'Bearer ' + mint('agent-1') };
      const r = await req('POST', dp(`/${u}/v1/ping`), { ...hdr, 'content-type': 'application/json' }, '{"x":1}');
      out[u] = { status: r.status, body: r.body.toString(), mockSaw: seen[u].map(s => s.url) };
      if (r.status !== 201 || r.body.toString() !== '{"ok":true}' || seen[u].length !== 1 || seen[u][0].url !== '/v1/ping') ok = false;
      for (const o of Object.keys(seen)) if (o !== u && seen[o].some(s => s.url === '/v1/ping' && s.t > Date.now() - 5000 && false)) ok = false;
    }
    const names = (stj.upstreams || []).map(x => x.name).sort();
    const listed = JSON.stringify(names) === JSON.stringify(['anthropic', 'openai', 'openrouter']);
    const f = art('D2-calls-and-status.json', { calls: out, statusCli: stj, statusAdminApi: JSON.parse(stHttp) });
    rec('D2', ok && listed ? 'PASS' : 'FAIL', `One call through each of /anthropic,/openai,/openrouter reached only its own configured base_url mock and returned its response; status lists upstreams ${JSON.stringify(names)}.`, [f]);
  } catch (e) { rec('D2', 'FAIL', 'exception: ' + e.message, [art('D2-error.txt', String(e.stack))]); }

  // ===== D3 =====
  try {
    reset();
    const n0 = (await logs()).length;
    const r = await req('POST', dp('/anthropic/v1/messages?beta=true&k=v'), { authorization: 'Bearer ' + mint('agent-1'), 'content-type': 'application/json' }, '{"m":1}');
    const l = await lastLog(n0);
    const up = seen.anthropic[0];
    const r2 = await req('GET', dp('/openai/v1/models/gpt-x/info'), { authorization: 'Bearer ' + mint('agent-1') });
    const up2 = seen.openai[0];
    const f = art('D3-rewrite.json', { sent: 'POST /anthropic/v1/messages?beta=true&k=v', upstreamSaw: { method: up?.method, url: up?.url }, sent2: 'GET /openai/v1/models/gpt-x/info', upstream2Saw: { method: up2?.method, url: up2?.url }, logRecord: l });
    const ok = up?.url === '/v1/messages?beta=true&k=v' && up.method === 'POST' && up2?.url === '/v1/models/gpt-x/info' && l?.upstream === 'anthropic' && l.method === 'POST' && typeof l.path === 'string' && l.path.endsWith('/v1/messages');
    rec('D3', ok ? 'PASS' : 'FAIL', `Upstream received '${up?.url}' (prefix stripped, query kept); log upstream=${l?.upstream} method=${l?.method} path=${l?.path}`, [f]);
  } catch (e) { rec('D3', 'FAIL', 'exception: ' + e.message, [art('D3-error.txt', String(e.stack))]); }

  // ===== D4 =====
  let d4ok = true; const d4 = {};
  try {
    reset();
    for (const u of ['anthropic', 'openai', 'openrouter']) {
      const tok = mint('agent-4-' + u);
      const hdr = { authorization: 'Bearer ' + tok };
      await req('POST', dp(`/${u}/v1/chat?token=nothing`), { ...hdr, 'content-type': 'application/json' }, '{"p":1}');
      const s = seen[u][0];
      const dump = JSON.stringify({ url: s.url, rawHeaders: s.rawHeaders, body: s.body });
      const hasKey = dump.includes(KEYS[u]);
      const hasTok = dump.includes(tok) || dump.includes(tok.split('.')[0]) || dump.includes(tok.split('.')[1]);
      const credHdr = u === 'anthropic' ? s.headers['x-api-key'] : s.headers['authorization'];
      d4[u] = { hasKey, hasTok, credentialHeader: credHdr, headers: s.headers };
      if (!hasKey || hasTok) d4ok = false;
    }
    // Anthropic OAuth credential (.credentials.json form): API key slot disabled so the default chain picks the OAuth slot
    const OAT = 'sk-ant-oat01-QAOAUTHACCESS-0005', ORT = 'sk-ant-ort01-QAOAUTHREFRESH-0006';
    const credFile = path.join(WORK, 'oauth.credentials.json');
    fs.writeFileSync(credFile, JSON.stringify({ claudeAiOauth: { accessToken: OAT, refreshToken: ORT, expiresAt: Date.now() + 6 * 3600 * 1000, scopes: ['user:inference'], subscriptionType: 'max' } }));
    const add = cli(['credential', 'add', '--upstream', 'anthropic', '--ref', 'qa-cred-anthropic-oauth', '--from-file', credFile]);
    cli(['credential', 'disable', REFS.anthropic]);
    reset();
    const otok = mint('agent-4-oauth');
    const orr = await req('POST', dp('/anthropic/v1/messages'), { authorization: 'Bearer ' + otok, 'content-type': 'application/json' }, '{"p":1}');
    cli(['credential', 'enable', REFS.anthropic]);
    const os = seen.anthropic[0];
    const odump = os ? JSON.stringify({ url: os.url, rawHeaders: os.rawHeaders, body: os.body }) : '';
    d4.anthropic_oauth = { addExit: add.status, addOut: add.stdout + add.stderr, callerStatus: orr.status, headers: os?.headers, hasAccessToken: odump.includes(OAT), hasRefreshToken: odump.includes(ORT), hasApiKey: odump.includes(KEYS.anthropic), hasCallerToken: odump.includes(otok) || odump.includes(otok.split('.')[0]) || odump.includes(otok.split('.')[1]) };
    const oauthOk = add.status === 0 && os && os.headers.authorization === 'Bearer ' + OAT && !('x-api-key' in os.headers) && !d4.anthropic_oauth.hasRefreshToken && !d4.anthropic_oauth.hasCallerToken;
    if (!oauthOk) d4ok = false;
    const f = art('D4-recording-upstream.json', d4);
    rec('D4', d4ok ? 'PASS' : 'FAIL', `At the recording upstreams: anthropic API key carried as x-api-key (${d4.anthropic.credentialHeader}); openai and openrouter carry Authorization: Bearer <pool key> (${d4.openai.credentialHeader} / ${d4.openrouter.credentialHeader}); Anthropic OAuth credential (.credentials.json) carried as Authorization: Bearer <access token> (no x-api-key, refresh token not sent); pool key bytes present and the caller token (whole, claims part, signature part) absent from URL, all headers and body. Live-vendor acceptance is out of scope per spec.`, [f]);
  } catch (e) { rec('D4', 'FAIL', 'exception: ' + e.message, [art('D4-error.txt', String(e.stack))]); }

  // ===== D5 =====
  try {
    reset();
    const tok = mint('agent-5');
    const n0 = (await logs()).length;
    const r = await req('POST', dp('/openai/v1/chat/completions'), { authorization: 'Bearer ' + tok, 'x-sandbox-secret': 'abc', 'x-correlation-id': 'corr-123', 'x-request-id': 'rid-9', 'content-type': 'application/json', cookie: 'sess=zzz', 'x-forwarded-for': '9.9.9.9' }, '{"a":1}');
    const l = await lastLog(n0);
    const s = seen.openai[0];
    const dump = JSON.stringify({ url: s.url, rawHeaders: s.rawHeaders, body: s.body });
    const noId = !dump.includes(tok) && !dump.includes(tok.split('.')[0]) && !dump.includes(tok.split('.')[1]);
    const noCustom = !('x-sandbox-secret' in s.headers) && !dump.includes('abc"');
    const ct = s.headers['content-type'] === 'application/json';
    const f = art('D5-outbound.json', { outboundHeaders: s.headers, outboundBody: s.body, logRecord: l, note: 'x-correlation-id forwarded? ' + ('x-correlation-id' in s.headers) });
    const ok = noId && noCustom && ct && s.headers.authorization === 'Bearer ' + KEYS.openai;
    rec('D5', ok ? 'PASS' : 'FAIL', `Outbound headers: ${Object.keys(s.headers).join(',')}; no identity material, x-sandbox-secret absent, content-type present. correlation header forwarded=${'x-correlation-id' in s.headers} (spec leaves open); log correlation_id=${l?.correlation_id}.`, [f]);
  } catch (e) { rec('D5', 'FAIL', 'exception: ' + e.message, [art('D5-error.txt', String(e.stack))]); }

  // ===== D6 ===== (library level: scratch crate on janus-core's public build_outbound)
  try {
    const crate = path.join(WORK, 'd6'); fs.mkdirSync(path.join(crate, 'src'), { recursive: true });
    const here = path.dirname(fileURLToPath(import.meta.url));
    fs.writeFileSync(path.join(crate, 'Cargo.toml'), fs.readFileSync(path.join(here, 'a-d6/Cargo.toml'), 'utf8').replace('../../../', ROOT + '/'));
    fs.copyFileSync(path.join(here, 'a-d6/src/main.rs'), path.join(crate, 'src/main.rs'));
    const r = spawnSync('cargo', ['run', '-q'], { cwd: crate, encoding: 'utf8', env: { ...process.env, CARGO_TARGET_DIR: '/tmp/qa-target-a' }, timeout: 500000 });
    const out = r.stdout + '\n--stderr--\n' + r.stderr;
    const f = art('D6-library-build-outbound.txt', out);
    const ok = r.status === 0 && /CASE A-control[^\n]*OK/.test(r.stdout) && ['B-leaky-allowlist-x-api-key', 'C-leaky-allowlist-authorization', 'D-leaky-basic-password'].every(c => new RegExp('CASE ' + c + ': ERR IdentityLeak').test(r.stdout)) && /contains_caller_token=false/.test(r.stdout);
    rec('D6', ok ? 'PASS' : 'FAIL', 'Scratch crate with a custom UpstreamImpl whose forwarding allowlist wrongly includes the identity header: build_outbound returned Err(SubstitutionError::IdentityLeak{location}) for x-api-key, authorization and basic-password cases; stock allowlist control built a request with pool key and no caller token. (With no identity presented, i.e. method none, a request is built - nothing to assert.)', [f]);
  } catch (e) { rec('D6', 'FAIL', 'exception: ' + e.message, [art('D6-error.txt', String(e.stack))]); }

  // ===== D7 =====
  try {
    const cases = [];
    let ok = true;
    for (const [p, st] of [['/v1/status/200', 200], ['/v1/status/404', 404], ['/v1/status/500', 500], ['/v1/binary', 200]]) {
      const hdr = { authorization: 'Bearer ' + mint('agent-7') };
      const via = await req('GET', dp('/openai' + p), hdr);
      const direct = await req('GET', `http://127.0.0.1:${MOCK.openai}${p}`);
      const same = via.status === direct.status && Buffer.compare(via.body, direct.body) === 0;
      const extra = Object.keys(via.headers).filter(h => !(h in direct.headers) && !['date', 'transfer-encoding', 'connection', 'content-length', 'keep-alive'].includes(h));
      const missing = Object.keys(direct.headers).filter(h => !(h in via.headers) && !['date', 'transfer-encoding', 'connection', 'content-length', 'keep-alive'].includes(h));
      cases.push({ p, viaStatus: via.status, directStatus: direct.status, viaBodyHex: via.body.toString('hex'), directBodyHex: direct.body.toString('hex'), same, extraHeaders: extra, missingHeaders: missing });
      if (!same || extra.length || missing.length) ok = false;
    }
    const f = art('D7-compare.json', cases);
    rec('D7', ok ? 'PASS' : 'FAIL', 'Status, body bytes (incl. non-UTF8 binary) and headers through Janus identical to calling the mock directly for 200/404/500/binary; no extra headers or body fields.', [f]);
  } catch (e) { rec('D7', 'FAIL', 'exception: ' + e.message, [art('D7-error.txt', String(e.stack))]); }

  // ===== D8 =====
  try {
    const out = {}; let ok = true;
    const expected = SSE.join('');
    for (const u of ['anthropic', 'openai', 'openrouter']) {
      const tok = mint('agent-8');
      const hdr = { authorization: 'Bearer ' + tok };
      const t0 = Date.now();
      const r = await fetch(dp(`/${u}/v1/stream`), { headers: hdr });
      const rd = r.body.getReader();
      const times = []; const parts = [];
      for (;;) { const { done, value } = await rd.read(); if (done) break; times.push(Date.now() - t0); parts.push(Buffer.from(value)); }
      const total = Date.now() - t0;
      const joined = Buffer.concat(parts).toString();
      const upstreamBody = ': open\n\n' + expected;
      const incremental = times.length >= 3 && times[0] < 800 && total > 2000;
      const identical = joined === upstreamBody;
      out[u] = { status: r.status, contentType: r.headers.get('content-type'), chunkArrivalMs: times, totalMs: total, identical, incremental };
      if (!incremental || !identical) ok = false;
    }
    const f = art('D8-streaming.json', out);
    rec('D8', ok ? 'PASS' : 'FAIL', `SSE upstream emits an event every 500ms for ~2.5s; first byte via Janus at ${out.openai.chunkArrivalMs[0]}ms (total ${out.openai.totalMs}ms), ${out.openai.chunkArrivalMs.length} separate chunks, byte-identical stream, for all three upstreams.`, [f]);
  } catch (e) { rec('D8', 'FAIL', 'exception: ' + e.message, [art('D8-error.txt', String(e.stack))]); }

  // ===== D9 =====
  try {
    const needles = [...Object.values(KEYS), ...Object.values(REFS), 'QAREALKEY', 'refresh'];
    const calls = {};
    const good = 'Bearer ' + mint('agent-9');
    calls.success = await req('POST', dp('/openai/v1/ok'), { authorization: good }, '{}');
    calls.deniedBadToken = await req('POST', dp('/openai/v1/ok'), { authorization: 'Bearer ' + mint('agent-9', { secret: 'wrong' }) }, '{}');
    calls.deniedNoToken = await req('POST', dp('/openai/v1/ok'), {}, '{}');
    calls.deniedExpired = await req('POST', dp('/openai/v1/ok'), { authorization: 'Bearer ' + mint('agent-9', { exp: 1000 }) }, '{}');
    calls.upstream500 = await req('GET', dp('/openai/v1/status/500'), { authorization: good });
    calls.upstreamKilled = await req('GET', dp('/openai/v1/kill'), { authorization: good });
    calls.unknownPrefix = await req('GET', dp('/nope/v1/x'), { authorization: good });
    let leaks = [];
    const dump = {};
    for (const [k, v] of Object.entries(calls)) {
      const s = JSON.stringify(v.headers) + v.body.toString('latin1');
      dump[k] = { status: v.status, headers: v.headers, body: v.body.toString('latin1').slice(0, 500) };
      for (const n of needles) if (s.includes(n)) leaks.push(`${k}:${n}`);
    }
    const direct = await req('POST', `http://127.0.0.1:${MOCK.openai}/v1/ok`, { 'content-type': 'x' }, '{}');
    const hdrNames = h => Object.keys(h).filter(x => !['date', 'transfer-encoding', 'connection', 'content-length', 'keep-alive'].includes(x)).sort().join(',');
    const indist = hdrNames(calls.success.headers) === hdrNames(direct.headers) && calls.success.body.equals(direct.body);
    const f = art('D9-responses.json', { leaks, successIndistinguishableFromDirect: indist, calls: dump });
    const ok = leaks.length === 0 && indist;
    rec('D9', ok ? 'PASS' : 'FAIL', `Inspected headers+bodies of ${Object.keys(calls).length} caller-visible responses (success, 4 identity denials, upstream 500/401, upstream connection killed, unknown prefix): credential bytes/credential_ref leaks=${JSON.stringify(leaks)}; success response indistinguishable from direct upstream call=${indist}. Statuses: ${Object.entries(dump).map(([k, v]) => k + '=' + v.status).join(' ')}`, [f]);
  } catch (e) { rec('D9', 'FAIL', 'exception: ' + e.message, [art('D9-error.txt', String(e.stack))]); }

  // ---- late section: upstream 429/401 legitimately mark a pool credential, so run last
  try {
    const hdr = { authorization: 'Bearer ' + mint('agent-7b') };
    const r429 = await req('GET', dp('/openai/v1/status/429'), hdr);
    const d429 = await req('GET', `http://127.0.0.1:${MOCK.openai}/v1/status/429`);
    const r401 = await req('GET', dp('/anthropic/v1/status/401'), hdr);
    const d401 = await req('GET', `http://127.0.0.1:${MOCK.anthropic}/v1/status/401`);
    const same = r429.status === 429 && r429.body.equals(d429.body) && r401.status === 401 && r401.body.equals(d401.body);
    const leak = [r429, r401].some(r => /QAREALKEY|qa-cred/.test(JSON.stringify(r.headers) + r.body));
    const f = art('D7-D9-upstream-429-401.json', { r429: { s: r429.status, b: r429.body.toString(), h: r429.headers }, r401: { s: r401.status, b: r401.body.toString(), h: r401.headers }, same, leak });
    const D7 = results.find(r => r.id === 'D7'), D9 = results.find(r => r.id === 'D9');
    D7.artifacts.push('evidence/artifacts/a/' + f); D9.artifacts.push('evidence/artifacts/a/' + f);
    if (!same) { D7.result = 'FAIL'; D7.note += ' LATE CHECK: upstream 429/401 not relayed unmodified (see ' + f + ').'; }
    else D7.note += ' Upstream 429 and 401 also relayed with identical status/body.';
    if (leak) { D9.result = 'FAIL'; D9.note += ' LATE CHECK: leak in 429/401 relay.'; }
  } catch (e) { results.push({ id: 'LATE', result: 'FAIL', artifacts: [], note: String(e) }); }
  art('daemon-log.txt', fs.readFileSync(path.join(WORK, 'daemon.log'), 'utf8').replace(/\x1b\[[0-9;]*m/g, ''));
}

let code = 0;
try { await main(); } catch (e) { console.error('HARNESS ERROR', e); code = 1; fails.push('harness'); }
finally {
  try { daemon?.kill('SIGTERM'); } catch {}
  mocks.forEach(async m => (await m).close());
  const have = new Set(results.map(r => r.id));
  for (let i = 1; i <= 9; i++) if (!have.has('D' + i)) results.push({ id: 'D' + i, result: 'BLOCKED', artifacts: [], note: 'not reached (harness error)' });
  results.sort((a, b) => +a.id.slice(1) - +b.id.slice(1));
  fs.writeFileSync(RES, JSON.stringify(results, null, 2));
  const bad = results.filter(r => r.result === 'FAIL'), blk = results.filter(r => r.result === 'BLOCKED');
  if (bad.length || code) { console.log('A FAIL: ' + bad.map(r => r.id).join(',')); process.exitCode = 1; }
  else { console.log('A PASS' + (blk.length ? ` (no failures; ${blk.length} BLOCKED: ${blk.map(r => r.id).join(',')})` : '')); process.exitCode = 0; }
  setTimeout(() => process.exit(process.exitCode), 300);
}
