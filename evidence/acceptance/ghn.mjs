// Blind acceptance script for DELIVERABLES.md sections G (D59-D64), H (D65-D77), N (D110).
// Usage: node evidence/acceptance/ghn.mjs        (prints "GHN PASS" and exits 0 only if every deliverable passed)
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import crypto from 'node:crypto';
import { spawn, spawnSync, execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');
const ART_REL = 'evidence/artifacts/ghn';
const ART = path.join(ROOT, ART_REL);
const RES = path.join(ROOT, 'evidence/results/ghn.json');
const WORK = '/tmp/qa-ghn/run';
const TARGET = process.env.CARGO_TARGET_DIR || '/tmp/qa-target-ghn';
const BIN = process.env.JANUS_BIN || (fs.existsSync(path.join(ROOT, 'target/release/janus-daemon')) ? path.join(ROOT, 'target/release') : path.join(TARGET, 'release'));
const DAEMON = path.join(BIN, 'janus-daemon');
const CLI = path.join(BIN, 'janus');
const NETSB = path.join(HERE, 'ghn-net.sb');
const UI_ASSETS = path.join(ROOT, 'ui/dist');
const GRPC_ROOT = process.env.GRPC_NODE_MODULES || '/Users/michaelhancock/hancock_industries/apps/agent/node_modules';
const PROTO = path.join(ROOT, 'proto/janus.proto');
const PLUG_DIR = path.join(os.tmpdir(), 'qa-ghn-plug');

// ports 24000-24999
const P = { A_DATA: 24001, A_UI: 24002, B_DATA: 24011, B_UI: 24012, MOCK: 24090, PROXY: 24094, TOKEN_TLS: 24095 };

fs.rmSync(WORK, { recursive: true, force: true });
fs.mkdirSync(WORK, { recursive: true });
fs.rmSync(ART, { recursive: true, force: true });
fs.mkdirSync(ART, { recursive: true });
fs.rmSync(PLUG_DIR, { recursive: true, force: true });
fs.mkdirSync(PLUG_DIR, { recursive: true, mode: 0o700 });

// ---------- result bookkeeping ----------
const results = {}; // id -> {checks:[], artifacts:Set, blocked:string|null}
function R(id) { return (results[id] ??= { checks: [], artifacts: new Set(), blocked: null }); }
function art(id, name, content) {
  const f = `${id}-${name}`;
  fs.writeFileSync(path.join(ART, f), typeof content === 'string' ? content : JSON.stringify(content, null, 2));
  R(id).artifacts.add(`${ART_REL}/${f}`);
}
function check(id, name, cond, detail = '') {
  R(id).checks.push({ name, ok: !!cond, detail: String(detail).slice(0, 300) });
  if (!cond) console.log(`  [FAIL] ${id}: ${name} ${detail}`);
}
function block(id, why) { R(id).blocked = why; }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- secrets (fake) ----------
const S = {
  orGood: 'sk-or-GOODKEY-aaaa1111aaaa1111',
  orBad: 'sk-or-BADKEY-bbbb2222bbbb2222',
  orRl: 'sk-or-RLIMIT-cccc3333cccc3333',
  orDis: 'sk-or-DISABLEDKEY-dddd4444dddd',
  orHand: 'sk-or-HANDMADE-eeee5555eeee5555',
  orEnv: 'sk-or-FROMENV-ffff6666ffff6666',
  orFile: 'sk-or-FROMFILE-gggg7777gggg7777',
  oaA: 'sk-oa-KEYAAAA-hhhh8888hhhh8888',
  oaB: 'sk-oa-KEYBBBB-iiii9999iiii9999',
  oaCost: 'sk-oa-COSTKEY-jjjj0000jjjj0000',
  anAT: 'AT-ANTH-secret-1234567890abcdef',
  anRT: 'RT-ANTH-secret-abcdef1234567890',
  anExpAT: 'AT-ANTH-EXPIRED-0f0f0f0f0f0f0f0f',
  anExpRT: 'RT-ANTH-EXPIRED-1e1e1e1e1e1e1e1e',
  anNewAT: 'AT-ANTH-REFRESHED-aa11bb22cc33dd44',
  anNewRT: 'RT-ANTH-REFRESHED-44dd33cc22bb11aa',
  oaAT: 'AT-OPENAI-secret-0123456789abcdef',
  oaRT: 'RT-OPENAI-secret-fedcba9876543210',
  oaHandRT: 'RT-OPENAI-HAND-5a5a5a5a5a5a5a5a',
  anHandRT: 'RT-ANTH-HAND-6b6b6b6b6b6b6b6b',
};
const SECRET_STRINGS = Object.values(S);

// ---------- generic helpers ----------
function req(opts, body) {
  return new Promise((resolve, reject) => {
    const mod = opts.tls ? https : http;
    const r = mod.request(opts, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }));
    });
    r.on('error', reject);
    r.setTimeout(20000, () => r.destroy(new Error('timeout')));
    if (body !== undefined) r.write(body);
    r.end();
  });
}
const j = (s) => { try { return JSON.parse(s); } catch { return null; } };

// ---------- mock upstream ----------
const mockLog = [];
const mock = http.createServer((rq, rs) => {
  const chunks = [];
  rq.on('data', (c) => chunks.push(c));
  rq.on('end', () => {
    const auth = rq.headers['authorization'] || rq.headers['x-api-key'] || '';
    const rec = { t: Date.now(), method: rq.method, url: rq.url, headers: rq.headers, body: Buffer.concat(chunks).toString('utf8'), auth };
    mockLog.push(rec);
    if (auth.includes('BADGENERIC')) {
      rs.writeHead(401, { 'content-type': 'application/json' });
      return rs.end('{"error":{"message":"invalid api key"}}');
    }
    if (auth.includes('BADKEY') || auth.includes('INVALIDATED')) {
      rs.writeHead(401, { 'content-type': 'application/json' });
      if (rq.url.startsWith('/v1/messages')) return rs.end('{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}');
      return rs.end('{"error":{"message":"User not found.","code":401}}'); // OpenRouter-shaped
    }
    if (auth.includes('RLIMIT')) {
      rs.writeHead(429, { 'content-type': 'application/json', 'retry-after': '120' });
      return rs.end('{"error":{"message":"rate limited"}}');
    }
    let usage = { prompt_tokens: 3, completion_tokens: 4, total_tokens: 7 };
    if (auth.includes('COSTKEY')) usage = { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2, cost: 5 };
    rs.writeHead(200, { 'content-type': 'application/json' });
    rs.end(JSON.stringify({ id: 'mock-1', model: 'mock', choices: [], usage }));
  });
});
await new Promise((r) => mock.listen(P.MOCK, '127.0.0.1', r));
const authsSince = (n) => mockLog.slice(n).map((m) => m.auth);

// ---------- fake token endpoint (TLS) + CONNECT proxy for OAuth refresh (D60/D61) ----------
const tlsDir = path.join(WORK, 'tls');
fs.mkdirSync(tlsDir, { recursive: true });
execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', path.join(tlsDir, 'k.pem'), '-out', path.join(tlsDir, 'c.pem'), '-days', '3', '-subj', '/CN=platform.claude.com',
  '-addext', 'subjectAltName=DNS:platform.claude.com,DNS:auth.openai.com,DNS:localhost'], { stdio: 'ignore' });
const tokenLog = []; // refresh requests received by the fake token endpoint
let tokenMode = 'fail'; // 'fail' | 'ok'
const jwt = (exp) => {
  const b = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
  return `${b({ alg: 'RS256', typ: 'JWT' })}.${b({ exp, iat: exp - 3600, iss: 'https://auth.openai.com', aud: ['https://api.openai.com/v1'], sub: 'u' })}.c2ln`;
};
const tokenSrv = http.createServer((rq, rs) => {
  const chunks = [];
  rq.on('data', (c) => chunks.push(c));
  rq.on('end', () => {
    const body = Buffer.concat(chunks).toString('utf8');
    tokenLog.push({ host: rq.headers.host, url: rq.url, method: rq.method, body, ct: rq.headers['content-type'] });
    if (tokenMode !== 'ok') { rs.writeHead(400, { 'content-type': 'application/json' }); return rs.end('{"error":"invalid_grant"}'); }
    rs.writeHead(200, { 'content-type': 'application/json' });
    const isOpenAI = rq.url.includes('openai');
    rs.end(JSON.stringify(isOpenAI
      ? { access_token: jwt(Math.floor(Date.now() / 1000) + 7200), refresh_token: S.anNewRT.replace('ANTH', 'OPENAI'), id_token: jwt(Math.floor(Date.now() / 1000) + 7200), token_type: 'Bearer', expires_in: 7200 }
      : { access_token: S.anNewAT, refresh_token: S.anNewRT, token_type: 'Bearer', expires_in: 7200, scope: 'user:inference' }));
  });
});
await new Promise((r) => tokenSrv.listen(P.TOKEN_TLS, '127.0.0.1', r));
const connectLog = [];
const proxy = http.createServer((q, s) => { s.writeHead(502); s.end(); });
proxy.on('connect', (rq, sock, head) => {
  connectLog.push(rq.url);
  const up = net.connect(P.TOKEN_TLS, '127.0.0.1', () => { sock.write('HTTP/1.1 200 Connection Established\r\n\r\n'); up.write(head); up.pipe(sock); sock.pipe(up); });
  up.on('error', () => sock.destroy()); sock.on('error', () => up.destroy());
});
await new Promise((r) => proxy.listen(P.PROXY, '127.0.0.1', r));

// ---------- gRPC plugin ----------
const gRequire = createRequire(path.join(GRPC_ROOT, 'x.js'));
const grpc = gRequire('@grpc/grpc-js');
const protoLoader = gRequire('@grpc/proto-loader');
const pkg = grpc.loadPackageDefinition(protoLoader.loadSync(PROTO, { keepCase: true, longs: String, defaults: true, oneofs: true }));
const resolveCalls = [];
const notifyEvents = [];
const meterState = { used: 0, limit: 3 };
function decide(identity) {
  if (identity.startsWith('ref:')) return { credential_ref: identity.slice(4) };
  if (identity === 'deny') return { deny_reason: 'denied by qa plugin' };
  if (identity.startsWith('meter:')) {
    if (meterState.used >= meterState.limit) return { deny_reason: `budget exhausted used=${meterState.used}` };
    return { use_default: true };
  }
  return { use_default: true };
}
const plugSock = path.join(PLUG_DIR, 'p.sock');
const grpcServer = new grpc.Server();
grpcServer.addService(pkg.janus.v1.JanusPlugin.service, {
  ResolveRoute: (call, cb) => {
    const rq = call.request;
    resolveCalls.push(rq);
    cb(null, decide(rq.caller_identity));
  },
  Notify: (call, cb) => {
    call.on('data', (ev) => {
      notifyEvents.push(ev);
      if (ev.event_type === 'call_completed') {
        const pl = j(ev.payload_json) || {};
        const cost = pl?.usage?.usage?.cost ?? pl?.usage?.cost;
        const rr = resolveCalls.find((x) => x.request_id === ev.request_id);
        if (rr && rr.caller_identity.startsWith('meter:')) meterState.used += typeof cost === 'number' ? cost : 1;
      }
    });
    call.on('end', () => cb(null, {}));
    call.on('error', () => {});
  },
});
await new Promise((res, rej) => grpcServer.bindAsync(`unix:${plugSock}`, grpc.ServerCredentials.createInsecure(), (e) => (e ? rej(e) : res())));

// ---------- daemon management ----------
class Daemon {
  constructor(name, { dataPort, uiPort, plugin, creds = '', env = {}, sandbox = true }) {
    this.name = name; this.dataPort = dataPort; this.uiPort = uiPort; this.plugin = plugin; this.creds = creds; this.env = env; this.sandbox = sandbox;
    this.dir = path.join(WORK, name);
    this.data = path.join(this.dir, 'data');
    this.cfg = path.join(this.dir, 'config.toml');
    this.logFile = path.join(this.dir, 'daemon.log');
    fs.mkdirSync(this.dir, { recursive: true });
  }
  writeConfig() {
    const up = (n) => `[[upstream]]\nname = "${n}"\npath_prefix = "/${n}"\nbase_url = "http://127.0.0.1:${P.MOCK}"\n${n === 'anthropic' || n === 'openai' ? `token_url = "http://127.0.0.1:${P.TOKEN_TLS}/${n}/oauth/token"\n` : ''}`;
    fs.writeFileSync(this.cfg, `data_dir = "${this.data}"
[data_plane]
bind = "127.0.0.1:${this.dataPort}"
[ui]
bind = "127.0.0.1:${this.uiPort}"
assets_dir = "${UI_ASSETS}"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }
${up('openai')}
${up('anthropic')}
${up('openrouter')}
${this.plugin ? `[plugin]\nendpoint = "unix://${plugSock}"\n` : ''}
${this.creds}
`);
  }
  start() {
    this.writeConfig();
    const out = fs.openSync(this.logFile, 'a');
    const env = { ...process.env, NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost', ...this.env };
    const args = this.sandbox ? ['-f', NETSB, DAEMON, '--config', this.cfg] : ['--config', this.cfg];
    this.proc = spawn(this.sandbox ? 'sandbox-exec' : DAEMON, args, { env, stdio: ['ignore', out, out] });
    this.exited = new Promise((r) => this.proc.on('exit', (c, s) => r({ c, s })));
  }
  async waitReady() {
    for (let i = 0; i < 100; i++) {
      if (fs.existsSync(path.join(this.data, 'admin.sock'))) {
        try { const r = await this.adm('GET', '/v1/status'); if (r.status === 200) { const ok = await new Promise((res) => { const c = net.connect(this.dataPort, '127.0.0.1', () => { c.destroy(); res(true); }); c.on('error', () => res(false)); }); if (ok) return; } } catch {}
      }
      await sleep(100);
    }
    throw new Error(`daemon ${this.name} not ready:\n` + fs.readFileSync(this.logFile, 'utf8'));
  }
  async stop() {
    if (!this.proc) return;
    this.proc.kill('SIGTERM');
    await Promise.race([this.exited, sleep(5000)]);
    spawnSync('pkill', ['-f', `--config ${this.cfg}`]);
    this.proc = null;
  }
  adm(method, p, body, extra = {}) {
    const data = body === undefined ? undefined : JSON.stringify(body);
    return req({ socketPath: path.join(this.data, 'admin.sock'), path: p, method, headers: { 'content-type': 'application/json', ...extra } }, data).then((r) => ({ ...r, json: j(r.body) }));
  }
  ui(method, p, headers = {}) { return req({ host: '127.0.0.1', port: this.uiPort, path: p, method, headers }); }
  adminToken() { return fs.readFileSync(path.join(this.data, 'admin-token'), 'utf8').trim(); }
  cli(args, input) {
    const r = spawnSync(CLI, ['--config', this.cfg, ...args], { input, encoding: 'utf8', env: (() => { const e = { ...process.env }; delete e.JANUS_ADMIN; delete e.JANUS_DATA_DIR; delete e.JANUS_TOKEN_FILE; delete e.JANUS_ADMIN_TOKEN; return e; })() });
    return { code: r.status, out: r.stdout, err: r.stderr };
  }
  async newIdentity(identity, extra = []) {
    const r = this.cli(['identity', 'create', '--identity', identity, ...extra]);
    if (r.code !== 0) throw new Error('identity create failed ' + r.err);
    return r.out.trim();
  }
  dp(prefix, pth, token, { method = 'POST', body = '{"model":"m","messages":[]}', headers = {} } = {}) {
    return req({ host: '127.0.0.1', port: this.dataPort, path: `/${prefix}${pth}`, method, headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json', ...headers } }, body);
  }
  async logs(q = '') { const r = await this.adm('GET', `/v1/requests?limit=1000${q}`); return r.json?.requests || r.json?.items || []; }
  async cred(ref) { return (await this.adm('GET', `/v1/credentials/${ref}`)).json; }
}

const writeJson = (p, o) => { fs.mkdirSync(path.dirname(p), { recursive: true }); fs.writeFileSync(p, typeof o === 'string' ? o : JSON.stringify(o)); };
const anCredFile = (at, rt, expMs) => ({ claudeAiOauth: { accessToken: at, refreshToken: rt, expiresAt: expMs, scopes: ['user:inference'], subscriptionType: 'max' } });
const oaAuthFile = (at, rt, idt) => ({ auth_mode: 'chatgpt', OPENAI_API_KEY: null, tokens: { id_token: idt, access_token: at, refresh_token: rt, account_id: 'acct-1' }, last_refresh: '2026-01-01T00:00:00Z' });

// ---------------------------------------------------------------------------------------
// Daemon A: plugin attached; pool seeded from the filesystem (hand-made slots) + env/file creds
// ---------------------------------------------------------------------------------------
const A = new Daemon('A', {
  dataPort: P.A_DATA, uiPort: P.A_UI, plugin: true,
  creds: `[[credential]]\nref = "envcred"\nupstream = "openrouter"\nenv = "QA_ENV_KEY"\n\n[[credential]]\nref = "filecred"\nupstream = "openrouter"\nfile = "${path.join(WORK, 'A', 'filecred.key')}"\n`,
  env: { QA_ENV_KEY: S.orEnv },
});
fs.mkdirSync(A.dir, { recursive: true });
fs.writeFileSync(path.join(A.dir, 'filecred.key'), S.orFile + '\n');
// hand-made slot directories, written straight onto the filesystem before the daemon starts
const poolA = path.join(A.data, 'credentials');
fs.mkdirSync(poolA, { recursive: true, mode: 0o700 });
writeJson(path.join(poolA, 'hand-anthropic', '.credentials.json'), anCredFile(S.anAT.replace('secret', 'HAND'), S.anHandRT, Date.now() + 86400000 * 365));
writeJson(path.join(poolA, 'hand-openai', 'auth.json'), oaAuthFile(S.oaAT.replace('secret', 'HAND'), S.oaHandRT, jwt(Math.floor(Date.now() / 1000) + 99999)));
fs.mkdirSync(path.join(poolA, 'hand-or'), { recursive: true });
fs.writeFileSync(path.join(poolA, 'hand-or', 'api_key'), S.orHand);

A.start();
await A.waitReady();
console.log('daemon A ready');

// ---- D59 ----
{
  const id = 'D59';
  // add via CLI the remaining kinds
  const add = (args, input) => A.cli(['credential', 'add', ...args], input);
  check(id, 'cli add or api key', add(['--upstream', 'openrouter', '--ref', 'or-good', '--api-key-stdin'], S.orGood).code === 0);
  check(id, 'cli add oauth anthropic', (() => { writeJson(path.join(WORK, 'an.json'), anCredFile(S.anAT, S.anRT, Date.now() + 86400000 * 365)); return add(['--upstream', 'anthropic', '--ref', 'an-ok', '--from-file', path.join(WORK, 'an.json')]).code === 0; })());
  check(id, 'cli add oauth openai', (() => { writeJson(path.join(WORK, 'oa.json'), oaAuthFile(S.oaAT, S.oaRT, jwt(Math.floor(Date.now() / 1000) + 99999))); return add(['--upstream', 'openai', '--ref', 'oa-ok', '--from-file', path.join(WORK, 'oa.json')]).code === 0; })());
  const list = A.cli(['credential', 'list', '--json']);
  art(id, 'credential-list.json', list.out);
  const arr = j(list.out)?.credentials || [];
  const by = Object.fromEntries(arr.map((c) => [c.ref, c]));
  const expect = {
    'hand-anthropic': ['anthropic', 'pool'], 'hand-openai': ['openai', 'pool'], 'hand-or': ['openrouter', 'pool'],
    'an-ok': ['anthropic', 'pool'], 'oa-ok': ['openai', 'pool'], 'or-good': ['openrouter', 'pool'],
    envcred: ['openrouter', 'env'], filecred: ['openrouter', 'file'],
  };
  for (const [ref, [up, src]] of Object.entries(expect)) {
    const c = by[ref];
    check(id, `${ref} listed`, !!c);
    if (c) {
      check(id, `${ref} upstream=${up}`, c.upstream === up, c.upstream);
      check(id, `${ref} source=${src}`, c.source === src, c.source);
      check(id, `${ref} kind non-empty`, typeof c.kind === 'string' && c.kind.length > 0, c.kind);
    }
  }
  // slot layout listing + modes
  const ls = spawnSync('sh', ['-c', `cd ${poolA} && find . -mindepth 1 | sort | while read f; do stat -f '%Sp %N' "$f"; done`], { encoding: 'utf8' }).stdout;
  art(id, 'pool-layout.txt', ls);
  check(id, 'one directory per slot, named by ref', ['hand-anthropic', 'hand-openai', 'hand-or', 'an-ok', 'oa-ok', 'or-good'].every((r) => fs.statSync(path.join(poolA, r)).isDirectory()));
  check(id, 'anthropic slot holds .credentials.json', fs.existsSync(path.join(poolA, 'an-ok', '.credentials.json')));
  check(id, 'openai slot holds auth.json', fs.existsSync(path.join(poolA, 'oa-ok', 'auth.json')));
  // janus.db holds no credential material
  const dbDump = spawnSync('sh', ['-c', `cd ${A.data} && sqlite3 janus.db .dump; strings janus.db janus.db-wal 2>/dev/null`], { encoding: 'utf8', maxBuffer: 1 << 26 }).stdout;
  const leaked = SECRET_STRINGS.filter((s) => dbDump.includes(s));
  check(id, 'janus.db has no credential material', leaked.length === 0, leaked.join(','));
  art(id, 'janus-db-secret-grep.txt', `secrets searched: ${SECRET_STRINGS.length}\nfound in janus.db: ${leaked.length ? leaked.join(',') : 'none'}\ntables: ${spawnSync('sqlite3', [path.join(A.data, 'janus.db'), '.tables'], { encoding: 'utf8' }).stdout}`);
  // freshly-made pool file contents
  const hand = j(list.out);
  void hand;
}

// ---- common identities for daemon A ----
const tokens = {};
for (const ref of ['or-good', 'or-bad', 'or-rl', 'or-dis', 'nonexistent', 'an-exp', 'an-ok', 'oa-ok']) tokens[ref] = await A.newIdentity(`ref:${ref}`);
tokens.default = await A.newIdentity('default');
tokens.deny = await A.newIdentity('deny');
tokens.meter = await A.newIdentity('meter:budget');

// ---- D63: credential health ----
{
  const id = 'D63';
  check(id, 'add bad key', A.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-bad', '--api-key-stdin'], S.orBad).code === 0);
  check(id, 'add rl key', A.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-rl', '--api-key-stdin'], S.orRl).code === 0);
  const before = await A.cred('or-good');
  const t0 = Date.now();
  const rg = []; for (let i = 0; i < 3; i++) rg.push(await A.dp('openrouter', '/v1/chat/completions', tokens['or-good']));
  const rb = await A.dp('openrouter', '/v1/chat/completions', tokens['or-bad']);
  const rr = await A.dp('openrouter', '/v1/chat/completions', tokens['or-rl']);
  await sleep(500);
  const good = await A.cred('or-good'); const bad = await A.cred('or-bad'); const rl = await A.cred('or-rl');
  art(id, 'before-after.json', { before, good, bad, rl, statuses: { good: rg.map((r) => r.status), bad: rb.status, rl: rr.status } });
  check(id, 'good calls succeed', rg.every((r) => r.status === 200), rg.map((r) => r.status));
  check(id, 'good request_count +3', good.request_count === (before.request_count || 0) + 3, good.request_count);
  check(id, 'good last_used_at set', !!good.last_used_at);
  check(id, 'good status active', good.status === 'active', good.status);
  check(id, 'good error_count 0', good.error_count === 0, good.error_count);
  check(id, 'rejected cred invalid', bad.status === 'invalid', bad.status);
  check(id, 'rejected cred last_error set', !!bad.last_error, bad.last_error);
  check(id, 'rejected cred error_count>=1 and request_count>=1', bad.error_count >= 1 && bad.request_count >= 1, `${bad.error_count}/${bad.request_count}`);
  check(id, 'rejected upstream status relayed 401', rb.status === 401, rb.status);
  check(id, 'rate-limited cred status', rl.status === 'rate_limited', rl.status);
  check(id, 'rate_limited_until in future', rl.rate_limited_until && Date.parse(rl.rate_limited_until) > t0, rl.rate_limited_until);
  check(id, 'rate-limited request_count>=1', rl.request_count >= 1, rl.request_count);
  check(id, 'rate-limit 429 relayed', rr.status === 429, rr.status);
  // extra observation (not a pass condition): a 401 from the OpenRouter upstream with a non-OpenRouter-shaped body
  A.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-generic', '--api-key-stdin'], 'sk-or-BADGENERIC-pppp1111pppp1111');
  const tg = await A.newIdentity('ref:or-generic');
  const rgen = await A.dp('openrouter', '/v1/chat/completions', tg); await sleep(500);
  art(id, 'observation-generic-401-body.json', { status: rgen.status, credential: await A.cred('or-generic'), note: 'mock upstream answered 401 with {"error":{"message":"invalid api key"}} (no numeric error code)' });
  // rotate clears invalid
  const rot = A.cli(['credential', 'rotate', 'or-bad', '--api-key-stdin'], 'sk-or-ROTATEDOK-kkkk1111kkkk1111');
  const after = await A.cred('or-bad');
  art(id, 'rotate-output.txt', rot.out + rot.err);
  check(id, 'rotate clears invalid (status active)', rot.code === 0 && after.status === 'active', after.status);
  S.rotated = 'sk-or-ROTATEDOK-kkkk1111kkkk1111'; SECRET_STRINGS.push(S.rotated);
}

// ---- D61: unusable credential fails, no fall-through ----
{
  const id = 'D61';
  // expired OAuth anthropic credential whose refresh fails (fake token endpoint answers invalid_grant)
  writeJson(path.join(WORK, 'an-exp.json'), anCredFile(S.anExpAT, S.anExpRT, Date.now() - 3600_000));
  check(id, 'add expired anthropic cred', A.cli(['credential', 'add', '--upstream', 'anthropic', '--ref', 'an-exp', '--from-file', path.join(WORK, 'an-exp.json')]).code === 0);
  check(id, 'add disabled-target cred', A.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-dis', '--api-key-stdin'], S.orDis).code === 0);
  check(id, 'disable it', A.cli(['credential', 'disable', 'or-dis']).code === 0);
  tokenMode = 'fail';
  const snap = async () => Object.fromEntries((await A.adm('GET', '/v1/credentials')).json.credentials.map((c) => [c.ref, c.request_count]));
  const cases = [
    ['disabled-ref', 'openrouter', 'or-dis'],
    ['nonexistent-ref', 'openrouter', 'nonexistent'],
    ['refresh-fails', 'anthropic', 'an-exp'],
  ];
  const out = {};
  for (const [label, up, ref] of cases) {
    const before = await snap(); const m0 = mockLog.length; const n0 = notifyEvents.length; const r0 = resolveCalls.length;
    const resp = await A.dp(up, up === 'anthropic' ? '/v1/messages' : '/v1/chat/completions', tokens[ref]);
    await sleep(600);
    const after = await snap();
    const recs = (await A.logs()).filter((r) => (r.caller_identity === `ref:${ref}`));
    const rec = recs[0];
    const ev = notifyEvents.slice(n0).filter((e) => rec && e.request_id === rec.request_id);
    out[label] = { status: resp.status, body: resp.body.slice(0, 200), forwarded: mockLog.length - m0, countsChanged: Object.keys(after).filter((k) => after[k] !== before[k]), outcome: rec?.outcome, rec, events: ev.map((e) => e.event_type) };
    check(id, `${label}: request fails`, resp.status >= 400, resp.status);
    check(id, `${label}: not forwarded`, mockLog.length === m0, `forwarded=${mockLog.length - m0}`);
    check(id, `${label}: no credential request_count incremented`, out[label].countsChanged.length === 0, out[label].countsChanged.join(','));
    check(id, `${label}: outcome credential_unavailable|abandoned`, ['credential_unavailable', 'abandoned'].includes(rec?.outcome), rec?.outcome);
    check(id, `${label}: plugin got call_abandoned`, ev.some((e) => e.event_type === 'call_abandoned'), ev.map((e) => e.event_type).join(','));
    check(id, `${label}: no call_completed`, !ev.some((e) => e.event_type === 'call_completed'));
  }
  art(id, 'cases.json', out);
  check(id, 'refresh-failure path really contacted the token endpoint', tokenLog.length >= 1 && tokenLog.some((t) => t.body.includes(S.anExpRT)), `tokenLog=${tokenLog.length}`);
  art(id, 'refresh-failure-note.txt', `refresh attempted against the mock token endpoint (token_url), which answered 400 invalid_grant; token requests: ${tokenLog.length}; request outcome error: ${out['refresh-fails'].rec?.error}`);
}

// ---- D60: OAuth refresh before use ----
{
  const id = 'D60';
  tokenMode = 'ok';
  // observation: the credential whose refresh failed in D61 -- does the next request retry the refresh?
  { const t = tokenLog.length; const rr = await A.dp('anthropic', '/v1/messages', tokens['an-exp']); await sleep(400);
    const rf = A.cli(['credential', 'refresh', 'an-exp', '--json']); await sleep(300);
    art(id, 'observation-retry-after-failed-refresh.json', { nextRequestStatus: rr.status, tokenRequestsOnNextRequest: tokenLog.length - t, manualRefreshExit: rf.code, manualRefreshOut: (rf.out + rf.err).slice(0, 400), after: await A.cred('an-exp') }); }
  writeJson(path.join(WORK, 'an-exp2.json'), anCredFile(S.anExpAT.replace('EXPIRED', 'EXPIRED2'), S.anExpRT.replace('EXPIRED', 'EXPIRED2'), Date.now() - 3600_000));
  check(id, 'add expired anthropic cred (fresh)', A.cli(['credential', 'add', '--upstream', 'anthropic', '--ref', 'an-exp2', '--from-file', path.join(WORK, 'an-exp2.json')]).code === 0);
  tokens['an-exp2'] = await A.newIdentity('ref:an-exp2');
  S.anExpRT2 = S.anExpRT.replace('EXPIRED', 'EXPIRED2'); SECRET_STRINGS.push(S.anExpRT2, S.anExpAT.replace('EXPIRED', 'EXPIRED2'));
  const t0 = tokenLog.length;
  // an-exp is still expired and has a valid refresh token now; use it via plugin-named ref
  const before = await A.cred('an-exp2');
  const m0 = mockLog.length;
  const resp = await A.dp('anthropic', '/v1/messages', tokens['an-exp2']);
  await sleep(500);
  const after = await A.cred('an-exp2');
  art(id, 'anthropic.json', { before, after, status: resp.status, body: resp.body, mockAuth: authsSince(m0), tokenRequests: tokenLog.slice(t0).map((t) => ({ host: t.host, url: t.url, ct: t.ct, bodyHasRefresh: t.body.includes(S.anExpRT2) })), proxyConnects: connectLog });
  const refreshed = tokenLog.length > t0;
  check(id, 'refresh request reached the mock token endpoint with the refresh token', refreshed && tokenLog.slice(t0).some((t) => t.body.includes(S.anExpRT2)), JSON.stringify(tokenLog.slice(t0)).slice(0, 200));
  if (refreshed) {
    check(id, 'request succeeds', resp.status === 200, resp.status);
    check(id, 'expires_at moved into the future', after.expires_at && Date.parse(after.expires_at) > Date.now() + 3600_000, `${before.expires_at} -> ${after.expires_at}`);
    check(id, 'upstream saw refreshed access token', authsSince(m0).some((a) => a.includes(S.anNewAT)), authsSince(m0).join(','));
    check(id, 'caller never sees refreshed token', !resp.body.includes(S.anNewAT) && !JSON.stringify(resp.headers).includes(S.anNewAT));
    const f = fs.readFileSync(path.join(poolA, 'an-exp2', '.credentials.json'), 'utf8');
    check(id, 'pool file updated with refreshed token', f.includes(S.anNewAT));
    // OpenAI/Codex auth.json OAuth
    const eAt = jwt(Math.floor(Date.now() / 1000) - 3600);
    writeJson(path.join(WORK, 'oa-exp.json'), oaAuthFile(eAt, S.oaRT.replace('secret', 'EXPIRED'), eAt));
    A.cli(['credential', 'add', '--upstream', 'openai', '--ref', 'oa-exp', '--from-file', path.join(WORK, 'oa-exp.json')]);
    tokens['oa-exp'] = await A.newIdentity('ref:oa-exp');
    const oBefore = await A.cred('oa-exp'); const t1 = tokenLog.length; const m1 = mockLog.length;
    const ro = await A.dp('openai', '/v1/chat/completions', tokens['oa-exp']);
    await sleep(500);
    const oAfter = await A.cred('oa-exp');
    art(id, 'openai.json', { oBefore, oAfter, status: ro.status, tokenRequests: tokenLog.slice(t1).map((t) => ({ host: t.host, url: t.url })), mockAuth: authsSince(m1) });
    check(id, 'openai expired cred: refresh attempted', tokenLog.length > t1, `hosts=${tokenLog.slice(t1).map((t) => t.host)}`);
    art(id, 'openai-note.txt', `request status ${ro.status}: OAuth (Codex) credentials are sent to the vendor Codex host, not the mock base_url, so under the deny-external-network sandbox the forward itself cannot succeed; the pre-use refresh is what is asserted`);
    check(id, 'openai expires_at in future', oAfter.expires_at && Date.parse(oAfter.expires_at) > Date.now() + 3600_000, `${oBefore.expires_at} -> ${oAfter.expires_at}`);
  }
}

// ---- D64: credential material never leaks ----
{
  const id = 'D64';
  const corpus = []; // [label, text]
  const grab = (label, text) => corpus.push([label, text || '']);
  // data plane: success, denied, upstream-fail, unknown token
  for (const [label, up, tk] of [['dp-success', 'openrouter', tokens['or-good']], ['dp-denied', 'openrouter', tokens.deny], ['dp-bad-upstream-key', 'openrouter', tokens['or-bad']], ['dp-unknown-token', 'openrouter', 'garbage']]) {
    const r = await A.dp(up, '/v1/chat/completions', tk);
    grab(label, `${r.status}\n${JSON.stringify(r.headers)}\n${r.body}`);
  }
  // admin API: add / show / list / default / rotate / enable / disable via CLI --json and raw
  const cli = (args, input) => { const r = A.cli(args, input); grab('cli ' + args.join(' '), r.out + r.err); };
  cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-leakprobe', '--api-key-stdin', '--json'], 'sk-or-LEAKPROBE-llll2222llll2222'); S.leak = 'sk-or-LEAKPROBE-llll2222llll2222'; SECRET_STRINGS.push(S.leak);
  cli(['credential', 'show', 'or-leakprobe', '--json']);
  cli(['credential', 'list', '--json']);
  cli(['credential', 'list']);
  cli(['credential', 'default', '--upstream', 'openrouter', '--json']);
  cli(['credential', 'rotate', 'or-leakprobe', '--api-key-stdin', '--json'], 'sk-or-LEAKROT-mmmm3333mmmm3333'); S.leak2 = 'sk-or-LEAKROT-mmmm3333mmmm3333'; SECRET_STRINGS.push(S.leak2);
  cli(['credential', 'disable', 'or-leakprobe', '--json']); cli(['credential', 'enable', 'or-leakprobe', '--json']);
  cli(['credential', 'refresh', 'an-exp', '--json']);
  cli(['status', '--json']); cli(['log', 'query', '--json', '--limit', '1000']);
  const post = await A.adm('POST', '/v1/credentials', { upstream: 'openrouter', ref: 'or-leakprobe2', material: 'sk-or-LEAKPOST-nnnn4444nnnn4444' }); S.leak3 = 'sk-or-LEAKPOST-nnnn4444nnnn4444'; SECRET_STRINGS.push(S.leak3);
  grab('admin POST /v1/credentials', `${post.status} ${post.body}`);
  const put = await A.adm('PUT', '/v1/credentials/or-leakprobe2/material', { material: 'sk-or-LEAKPUT-oooo5555oooo5555' }); S.leak4 = 'sk-or-LEAKPUT-oooo5555oooo5555'; SECRET_STRINGS.push(S.leak4);
  grab('admin PUT material', `${put.status} ${put.body}`);
  art(id, 'add-response.txt', `POST status ${post.status}\n${post.body}\nPUT status ${put.status}\n${put.body}`);
  for (const p of ['/v1/credentials', '/v1/credentials/or-good', '/v1/credentials/default?upstream=openrouter', '/v1/status', '/v1/plugin', '/v1/identities', '/v1/credentials/nope']) { const r = await A.adm('GET', p); grab('admin GET ' + p, r.body); }
  for (const rec of await A.logs()) grab('log record ' + rec.request_id, JSON.stringify(rec));
  const first = (await A.logs())[0];
  if (first) { const sh = A.cli(['log', 'show', first.request_id, '--json']); grab('cli log show', sh.out + sh.err); const shh = A.cli(['log', 'show', first.request_id]); grab('cli log show (human)', shh.out + shh.err); }
  // UI listener: assets + token-authed API
  const tok = A.adminToken();
  const uiRoot = await A.ui('GET', '/');
  grab('ui GET /', uiRoot.body);
  const idx = uiRoot.body.match(/(?:src|href)="([^"]+\.(?:js|css))"/g) || [];
  for (const a of idx.slice(0, 6)) { const u = a.replace(/^(src|href)="/, '').replace(/"$/, ''); const r = await A.ui('GET', u.startsWith('/') ? u : '/' + u); grab('ui asset ' + u, r.body); }
  for (const p of ['/v1/credentials', '/v1/credentials/or-good', '/v1/status', '/v1/requests?limit=1000', '/v1/identities']) { const r = await A.ui('GET', p, { authorization: `Bearer ${tok}` }); grab('ui api ' + p, r.body); check(id, `ui api ${p} 200`, r.status === 200, r.status); }
  // sweep
  const hits = [];
  for (const [label, text] of corpus) for (const s of SECRET_STRINGS) if (text.includes(s) && !(label === 'cli credential add' )) hits.push(`${label}: ${s}`);
  // base64/hex encoded variants of key material
  for (const [label, text] of corpus) for (const s of [S.orGood, S.anAT, S.anRT, S.oaAT, S.oaRT]) { for (const enc of [Buffer.from(s).toString('base64'), Buffer.from(s).toString('hex')]) if (text.includes(enc)) hits.push(`${label}: encoded ${s}`); }
  check(id, 'no secret in any data-plane/admin/CLI/UI/log output', hits.length === 0, hits.slice(0, 5).join(' | '));
  art(id, 'leak-sweep.txt', `outputs scanned: ${corpus.length}\nsecrets searched (+base64/hex of five): ${SECRET_STRINGS.length}\nhits: ${hits.length}\n${hits.join('\n')}\n\nlabels:\n${corpus.map((c) => c[0]).join('\n')}`);
  // metadata keys only
  const list = (await A.adm('GET', '/v1/credentials')).json.credentials;
  const allowed = new Set(['ref', 'upstream', 'kind', 'label', 'source', 'status', 'rate_limited_until', 'expires_at', 'last_used_at', 'request_count', 'error_count', 'last_error']);
  const extra = [...new Set(list.flatMap((c) => Object.keys(c)).filter((k) => !allowed.has(k)))];
  check(id, 'credential metadata has no extra (material) fields', extra.length === 0, extra.join(','));
  // the daemon's own log
  const dlog = fs.readFileSync(A.logFile, 'utf8');
  const dleak = SECRET_STRINGS.filter((s) => dlog.includes(s));
  check(id, 'daemon log has no secret', dleak.length === 0, dleak.join(','));
  // UI views through a real browser: log in with the admin token, visit every view, screenshot, grep text for secrets
  try {
    const ab = (...a) => spawnSync('agent-browser', ['--session', 'ghn-ui', ...a], { encoding: 'utf8', timeout: 60000 });
    ab('open', `http://127.0.0.1:${P.A_UI}/`);
    let snap = ab('snapshot').stdout;
    const tbox = snap.match(/textbox "Admin token" \[ref=(e\d+)\]/)?.[1]; const btn = snap.match(/button "Connect" \[ref=(e\d+)\]/)?.[1];
    check(id, 'UI asks for admin token', !!tbox && !!btn);
    ab('fill', '@' + tbox, A.adminToken()); ab('click', '@' + btn); await sleep(1500);
    let uiText = '';
    for (const view of ['Overview', 'Credentials', 'Identities', 'Requests', 'Plugin']) {
      snap = ab('snapshot').stdout;
      const ref = snap.match(new RegExp(`link "${view}" \\[ref=(e\\d+)\\]`))?.[1];
      if (ref) { ab('click', '@' + ref); await sleep(1200); }
      const body = ab('snapshot').stdout;
      uiText += `\n=== ${view} ===\n` + body;
      const shot = path.join(ART, `D64-ui-${view.toLowerCase()}.png`);
      ab('screenshot', shot);
      if (fs.existsSync(shot)) R(id).artifacts.add(`${ART_REL}/D64-ui-${view.toLowerCase()}.png`);
      if (view === 'Credentials') check(id, 'UI Credentials view lists credentials', /or-good|hand-or|envcred/.test(body), body.slice(0, 120));
    }
    art(id, 'ui-views-text.txt', uiText);
    const uiHits = SECRET_STRINGS.filter((x) => uiText.includes(x));
    check(id, 'no secret in any UI view text', uiHits.length === 0, uiHits.join(','));
    ab('close');
  } catch (e) { check(id, 'UI browse', false, e.message); }
}

// ---- D77 (daemon A part: plugin metering; part 2 on daemon B w/o plugin) ----
{
  const id = 'D77';
  A.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-cost', '--api-key-stdin'], S.oaCost.replace('oa', 'or'));
  // route the meter identity through default chain: disable others for openrouter so the COSTKEY credential is the only enabled one
  for (const c of (await A.adm('GET', '/v1/credentials?upstream=openrouter')).json.credentials) if (c.ref !== 'or-cost') A.cli(['credential', 'disable', c.ref]);
  const seq = [];
  for (let i = 0; i < 4; i++) {
    const r = await A.dp('openrouter', '/v1/chat/completions', tokens.meter);
    await sleep(400);
    seq.push({ i, status: r.status, body: r.body.slice(0, 120), meterUsed: meterState.used });
  }
  art(id, 'plugin-meter-sequence.json', { limit: meterState.limit, seq, events: notifyEvents.filter((e) => e.event_type === 'call_completed').slice(-4).map((e) => e.payload_json) });
  // limit 3, each call costs 5 -> first call completes (crossing the limit), the next is denied by the plugin
  check(id, 'call crossing the limit completes (200)', seq[0].status === 200, seq[0].status);
  check(id, 'next call denied', seq[1].status >= 400 && seq[1].status !== 502, seq[1].status);
  const recs = (await A.logs()).filter((r) => r.caller_identity === 'meter:budget');
  check(id, 'log records denial as outcome=denied', recs.some((r) => r.outcome === 'denied'), recs.map((r) => r.outcome).join(','));
  for (const c of (await A.adm('GET', '/v1/credentials?upstream=openrouter')).json.credentials) A.cli(['credential', 'enable', c.ref]);
}

await A.stop();

// ---------------------------------------------------------------------------------------
// Daemon B: no plugin (D62, D77 part 2, D110)
// ---------------------------------------------------------------------------------------
const B = new Daemon('B', { dataPort: P.B_DATA, uiPort: P.B_UI, plugin: false });
fs.mkdirSync(B.dir, { recursive: true });
B.start();
await B.waitReady();
console.log('daemon B ready');

// ---- D62 ----
{
  const id = 'D62';
  check(id, 'add oa A', B.cli(['credential', 'add', '--upstream', 'openai', '--ref', 'oa-a', '--api-key-stdin'], S.oaA).code === 0);
  check(id, 'add oa B', B.cli(['credential', 'add', '--upstream', 'openai', '--ref', 'oa-b', '--api-key-stdin'], S.oaB).code === 0);
  const tk = await B.newIdentity('caller-1');
  const both = [];
  { const m0 = mockLog.length; for (let i = 0; i < 4; i++) await B.dp('openai', '/v1/chat/completions', tk); both.push(...authsSince(m0)); }
  check(id, 'sanity: some default chain pick works with both enabled', both.length === 4 && both.every((a) => a.includes('KEYAAAA') || a.includes('KEYBBBB')), both.join(','));
  check(id, 'disable oa-a', B.cli(['credential', 'disable', 'oa-a']).code === 0);
  const m0 = mockLog.length;
  const statuses = [];
  for (let i = 0; i < 6; i++) statuses.push((await B.dp('openai', '/v1/chat/completions', tk)).status);
  const used = authsSince(m0);
  const def = await B.adm('GET', '/v1/credentials/default?upstream=openai');
  const defCli = B.cli(['credential', 'default', '--upstream', 'openai', '--json']);
  const oaB = await B.cred('oa-b');
  art(id, 'default-chain.json', { statuses, upstreamAuths: used, defaultEndpoint: def.json, defaultCli: j(defCli.out), oaB });
  check(id, 'requests succeed', statuses.every((s) => s === 200), statuses);
  check(id, 'only enabled credential used', used.length === 6 && used.every((a) => a.includes('KEYBBBB')), used.join(','));
  check(id, 'default endpoint returns oa-b metadata', def.status === 200 && def.json?.ref === 'oa-b' && def.json?.upstream === 'openai', def.body);
  check(id, 'default endpoint metadata matches credential show', def.json && oaB && def.json.ref === oaB.ref && def.json.kind === oaB.kind && def.json.source === oaB.source && def.json.status === oaB.status);
  check(id, 'default peek has no material', !JSON.stringify(def.json).includes(S.oaB));
  const dis = await B.adm('POST', '/v1/credentials/oa-b/disable');
  const none = await B.adm('GET', '/v1/credentials/default?upstream=openai');
  const rn = await B.dp('openai', '/v1/chat/completions', tk);
  art(id, 'all-disabled.json', { disableStatus: dis.status, defaultStatus: none.status, defaultBody: none.body, requestStatus: rn.status, requestBody: rn.body });
  check(id, 'with all disabled the request is not served by a disabled credential', rn.status >= 400, rn.status);
  await B.adm('POST', '/v1/credentials/oa-b/enable');
}

// ---- D77 part 2: no metering endpoint / no daemon-side denial ----
{
  const id = 'D77';
  const probes = ['/v1/meters', '/v1/metering', '/v1/usage', '/v1/quotas', '/v1/counters', '/v1/budgets', '/v1/limits'];
  const pr = {};
  for (const p of probes) { const r = await B.adm('GET', p); pr[p] = r.status; }
  const st = await B.adm('GET', '/v1/status');
  const stJson = JSON.stringify(st.json);
  art(id, 'no-metering-endpoints.json', { probes: pr, statusKeys: Object.keys(st.json || {}), statusMentionsMeter: /meter|quota|budget|counter/i.test(stJson) });
  check(id, 'no metering endpoint exists (all 404-class)', Object.values(pr).every((s) => s >= 400 && s < 500), JSON.stringify(pr));
  check(id, 'status has no metering fields', !/meter|quota|budget|counter/i.test(stJson));
  const tk = await B.newIdentity('hammer');
  B.cli(['credential', 'add', '--upstream', 'openrouter', '--ref', 'or-hammer', '--api-key-stdin'], S.orGood);
  const sts = [];
  for (let i = 0; i < 60; i++) sts.push((await B.dp('openrouter', '/v1/chat/completions', tk)).status);
  check(id, 'no-plugin daemon never denies on its own (60/60 forwarded)', sts.every((s) => s === 200), [...new Set(sts)].join(','));
  art(id, 'no-plugin-60-requests.txt', `statuses: ${[...new Set(sts)].join(',')} count=${sts.length}`);
}

// ---- D110 ----
{
  const id = 'D110';
  const tkA = await B.newIdentity('d110-a', ['--claim', 'tenant=acme']);
  const tkB = await B.newIdentity('d110-b');
  const pr = await B.dp('openrouter', '/v1/chat/completions', tkA);
  await B.dp('openrouter', '/v1/chat/completions', tkB);
  check(id, 'traffic with opaque token forwarded', pr.status === 200, pr.status);
  await B.dp('openrouter', '/v1/chat/completions', 'not-a-token');
  const db = path.join(B.data, 'janus.db');
  const sq = (sql) => spawnSync('sqlite3', ['-json', db, sql], { encoding: 'utf8' });
  const tables = (spawnSync('sqlite3', [db, "select name from sqlite_master where type='table' and name not like 'sqlite_%' order by name"], { encoding: 'utf8' }).stdout || '').trim().split('\n');
  const cols = (t) => (j(sq(`pragma table_info(${t})`).stdout) || []).map((c) => c.name);
  const idxs = (t) => (j(sq(`pragma index_list(${t})`).stdout) || []).map((i) => ({ name: i.name, cols: (j(sq(`pragma index_info(${i.name})`).stdout) || []).map((c) => c.name) }));
  const idCols = cols('identities'); const rqCols = cols('requests');
  const rqIdx = idxs('requests');
  art(id, 'schema.json', { tables, identities: idCols, requests: rqCols, requestsIndexes: rqIdx, identitiesIndexes: idxs('identities'), ddl: spawnSync('sqlite3', [db, '.schema'], { encoding: 'utf8' }).stdout });
  check(id, 'exactly two tables: identities, requests', JSON.stringify(tables) === JSON.stringify(['identities', 'requests']), tables.join(','));
  for (const c of ['id', 'token_hash', 'identity', 'claims', 'label', 'created_at', 'expires_at', 'revoked_at', 'last_seen_at']) check(id, `identities.${c}`, idCols.includes(c));
  const hasIdx = (cs) => rqIdx.some((i) => JSON.stringify(i.cols) === JSON.stringify(cs));
  check(id, 'requests has index on received_at', hasIdx(['received_at']), JSON.stringify(rqIdx));
  check(id, 'requests has index on (caller_identity, received_at)', hasIdx(['caller_identity', 'received_at']));
  check(id, 'requests has index on request_id', hasIdx(['request_id']));
  for (const c of ['request_id', 'received_at', 'caller_identity']) check(id, `requests.${c}`, rqCols.includes(c));
  // no raw token / credential material
  const dump = spawnSync('sh', ['-c', `cd ${B.data} && sqlite3 janus.db .dump; strings janus.db janus.db-wal 2>/dev/null`], { encoding: 'utf8', maxBuffer: 1 << 26 }).stdout;
  check(id, 'no raw identity token in database', !dump.includes(tkA) && !dump.includes(tkB));
  const sha = crypto.createHash('sha256').update(tkA).digest('hex');
  const rows = j(sq('select id, token_hash, identity, claims, last_seen_at from identities').stdout) || [];
  const rowA = rows.find((r) => r.identity === 'd110-a');
  check(id, 'token_hash present and is not the token (sha256 hex/base64 of token)', rowA && rowA.token_hash !== tkA && (rowA.token_hash === sha || rowA.token_hash.length >= 32), rowA?.token_hash);
  check(id, 'claims stored as JSON', rowA && j(rowA.claims)?.tenant === 'acme', rowA?.claims);
  check(id, 'no credential material in database', SECRET_STRINGS.every((s) => !dump.includes(s)));
  // last_seen_at flushed periodically
  let seen = null;
  for (let i = 0; i < 40 && !seen; i++) { await sleep(1000); seen = (j(sq("select last_seen_at from identities where identity='d110-a'").stdout) || [])[0]?.last_seen_at; }
  check(id, 'last_seen_at flushed after use', !!seen, seen);
  // revocation visible to the very next request
  const idA = rowA.id;
  const rv = B.cli(['identity', 'revoke', idA]);
  const afterRevoke = await B.dp('openrouter', '/v1/chat/completions', tkA);
  check(id, 'revocation immediately visible (401)', rv.code === 0 && afterRevoke.status === 401, `${rv.code} ${afterRevoke.status}`);
  // restart: opaque token still verifies
  await B.stop();
  B.start(); await B.waitReady();
  const afterRestartB = await B.dp('openrouter', '/v1/chat/completions', tkB);
  const afterRestartA = await B.dp('openrouter', '/v1/chat/completions', tkA);
  check(id, 'opaque token verifies after restart', afterRestartB.status === 200, afterRestartB.status);
  check(id, 'revoked token still revoked after restart', afterRestartA.status === 401, afterRestartA.status);
  const recs = (await B.logs()).filter((r) => r.caller_identity === 'd110-b');
  check(id, 'request log persisted across restart', recs.length >= 2 && recs.some((r) => r.status === 200), recs.length);
  art(id, 'restart.txt', `token B after restart: ${afterRestartB.status}\ntoken A (revoked) after restart: ${afterRestartA.status}\nlast_seen_at: ${seen}\nrows: ${JSON.stringify(rows.map((r) => ({ ...r, token_hash: r.token_hash.slice(0, 8) + '…' })))}`);
}
await B.stop();

// ---------------------------------------------------------------------------------------
// Section H: Rust library, scratch crate depending on janus-core by path
// ---------------------------------------------------------------------------------------
{
  const hdir = path.join(WORK, 'h');
  fs.cpSync(path.join(HERE, 'ghn-h'), hdir, { recursive: true });
  const cargoToml = fs.readFileSync(path.join(hdir, 'Cargo.toml'), 'utf8').replace(/path = "[^"]+janus-core"/, `path = "${path.join(ROOT, 'crates/janus-core')}"`);
  fs.writeFileSync(path.join(hdir, 'Cargo.toml'), cargoToml);
  const env = { ...process.env, CARGO_TARGET_DIR: TARGET };
  const build = spawnSync('cargo', ['build', '--release'], { cwd: hdir, env, encoding: 'utf8' });
  art('D65', 'build.txt', build.stdout + build.stderr);
  if (build.status !== 0) { for (let n = 65; n <= 76; n++) check('D' + n, 'scratch crate builds', false, build.stderr.slice(-300)); }
  else {
    const exe = path.join(TARGET, 'release', 'qa-h');
    const run = spawnSync(exe, [], { encoding: 'utf8', timeout: 120000 });
    const lines = run.stdout.split('\n').filter(Boolean);
    for (const l of lines) {
      const m = l.match(/^(D\d+)(?:-(\w+))? (PASS|FAIL) (.*)$/);
      if (!m) continue;
      check(m[1], m[2] ? `${m[1]}-${m[2]}` : m[1], m[3] === 'PASS', m[4]);
    }
    for (let n = 65; n <= 74; n++) { const idn = 'D' + n; art(idn, 'output.txt', lines.filter((l) => l.startsWith(idn + ' ') || l.startsWith(idn + '-')).join('\n') || '(no output)'); if (!R(idn).checks.length) check(idn, 'ran', false, 'no result line'); }

    // D75: SqliteUsageStore durable, restart, second process refused, WAL
    {
      const id = 'D75'; const d = path.join(WORK, 's75'); fs.mkdirSync(d, { recursive: true });
      const f = path.join(d, 'u.db');
      const w = spawnSync(exe, ['sqlite-write', f], { encoding: 'utf8' });
      const rd = spawnSync(exe, ['sqlite-read', f], { encoding: 'utf8' });
      const holder = spawn(exe, ['sqlite-hold', f], { stdio: ['ignore', 'pipe', 'pipe'] });
      await new Promise((r) => holder.stdout.once('data', r));
      const probe = spawnSync(exe, ['sqlite-probe', f], { encoding: 'utf8' });
      holder.kill('SIGKILL');
      await sleep(500);
      const probe2 = spawnSync(exe, ['sqlite-probe', f], { encoding: 'utf8' });
      const c = path.join(d, 'c.db');
      const cl = spawnSync(exe, ['sqlite-closed', c], { encoding: 'utf8' });
      const jm = spawnSync('sqlite3', [c, 'PRAGMA journal_mode;'], { encoding: 'utf8' });
      art(id, 'output.txt', [w.stdout, rd.stdout, 'second-process-open while held: ' + probe.stdout, 'after holder killed: ' + probe2.stdout, cl.stdout, 'journal_mode: ' + jm.stdout, 'ls: ' + fs.readdirSync(d).join(' ')].join('\n'));
      check(id, 'durability() is Durable', /durability=Durable/.test(w.stdout), w.stdout);
      check(id, 'count survives restart', /count=7/.test(rd.stdout) && /used: 7/.test(rd.stdout), rd.stdout);
      check(id, 'second process open fails with error', /OPEN_ERR/.test(probe.stdout) && /lock/i.test(probe.stdout), probe.stdout);
      check(id, 'after holder dies, lock is released (open ok)', /OPEN_OK/.test(probe2.stdout), probe2.stdout);
      check(id, 'journal_mode is wal after close', jm.stdout.trim() === 'wal', jm.stdout);
      // compensating observation for durability: SIGKILL a writer mid-run; every charge it acknowledged must survive
      const cf = path.join(d, 'crash.db');
      const writer = spawn(exe, ['sqlite-crash', cf], { stdio: ['ignore', 'pipe', 'ignore'] });
      let acked = 0; let buf = '';
      writer.stdout.on('data', (c) => { buf += c; const m = [...buf.matchAll(/acked (\d+)/g)]; if (m.length) acked = +m[m.length - 1][1]; });
      while (acked < 30) await sleep(20);
      writer.kill('SIGKILL');
      await new Promise((r) => writer.on('exit', r));
      const ackedFinal = acked;
      const cnt = spawnSync(exe, ['sqlite-count', cf], { encoding: 'utf8' });
      const survived = +(cnt.stdout.match(/count=(\d+)/)?.[1] ?? -1);
      const walAfterKill = fs.existsSync(cf + '-wal');
      art(id, 'sigkill-durability.txt', `writer acknowledged ${ackedFinal} committed charges (printed after each returned) before SIGKILL; after reopen the Total counter is ${survived} (expected >= ${ackedFinal}, <= ${ackedFinal + 2}); -wal file present after kill: ${walAfterKill}`);
      check(id, 'SIGKILL mid-run: every acknowledged charge survives', survived >= ackedFinal && survived <= ackedFinal + 2, `acked=${ackedFinal} survived=${survived}`);
      check(id, 'store reopens after SIGKILL (stale lock released)', survived >= 0);
      R(id).note = 'VERIFIED: durability()==Durable; count survives restart; second-process open refused with a lock error and succeeds once the holder dies; journal_mode==wal (read by sqlite3 after clean close); acknowledged commits survive SIGKILL of the writer. NOT VERIFIED: synchronous=FULL and BEGIN IMMEDIATE/single-writer-thread are per-connection/internal settings that cannot be observed from outside; the SIGKILL test only shows process-crash durability (the OS page cache survives a process kill), not power-loss durability that synchronous=FULL guards.';
    }
    // D76: conformance suite
    {
      const id = 'D76';
      const t = spawnSync('cargo', ['test', '--release', '--test', 'conf'], { cwd: hdir, env, encoding: 'utf8', timeout: 600000 });
      const txt = t.stdout + t.stderr;
      const t2 = spawnSync('cargo', ['test', '-p', 'janus-core', '--features', 'testing,sqlite'], { cwd: ROOT, env, encoding: 'utf8', timeout: 900000 });
      const txt2 = t2.stdout + t2.stderr;
      art(id, 'conformance-scratch.txt', txt);
      art(id, 'janus-core-tests.txt', txt2.split('\n').filter((l) => /^test |test result|running|FAILED|failed/.test(l)).join('\n'));
      const m = txt.match(/test result: (\w+)\. (\d+) passed; (\d+) failed/);
      check(id, 'suite macro runs for Memory, Sqlite and a third-party store: all pass', t.status === 0 && m && m[1] === 'ok' && +m[2] >= 42 && +m[3] === 0, m?.[0]);
      check(id, 'suite covers memory, sqlite and third-party modules', ['mem::', 'sql::', 'third::'].every((p) => txt.includes('test ' + p)));
      check(id, 'cargo test --features testing,sqlite in janus-core passes', t2.status === 0 && !/FAILED|failed;/.test(txt2.replace(/0 failed/g, '')), `exit ${t2.status}`);
      const names = ['apply_is_all_or_nothing_across_bounds', 'apply_is_idempotent_per_request_id', 'release_lands_in_the_window_charged', 'compact_drops_expired_keys_only', 'concurrent_applies_never_exceed_limit'];
      check(id, 'suite includes atomicity/idempotency/release/compact checks', names.every((nm) => txt.includes(nm)));
    }
  }
}


R('D60').note = 'Anthropic: full path verified (expired token + valid refresh token -> mock token_url called, request 200, expires_at moved forward, upstream got the new token, caller never saw it, refresh-failure path -> 503 not forwarded). OpenAI/Codex: only the refresh itself is observable (token endpoint called, expires_at moved) because the Codex backend host is fixed and not redirected by base_url, so the forwarded request cannot be observed.';
// D77 note on 'Meter' part: counting lives in the plugin's process
{
  R('D77').note = 'Metering is exercised in Section H (library); the daemon-level checks use a node plugin that counts call_completed payloads.';
}

// ---------- teardown ----------
await A.stop(); await B.stop();
mock.close(); tokenSrv.close(); proxy.close(); grpcServer.forceShutdown();
fs.writeFileSync(path.join(ART, 'mock-upstream-log.json'), JSON.stringify(mockLog.map((m) => ({ method: m.method, url: m.url, auth: m.auth.replace(/(.{14}).*/, '$1…') })), null, 1));
fs.writeFileSync(path.join(ART, 'plugin-events.json'), JSON.stringify({ resolve: resolveCalls.length, events: notifyEvents.map((e) => ({ t: e.event_type, id: e.request_id })) }, null, 1));

// ---------- results ----------
const ids = []; for (let n = 59; n <= 77; n++) ids.push('D' + n); ids.push('D110');
const out = [];
let failed = 0;
for (const id of ids) {
  const r = results[id];
  if (!r || (!r.checks.length && !r.blocked)) { out.push({ id, result: 'FAIL', artifacts: [], note: 'no checks ran' }); failed++; continue; }
  const bad = r.checks.filter((c) => !c.ok);
  let result = bad.length ? 'FAIL' : r.blocked ? 'BLOCKED' : 'PASS';
  if (bad.length === 0 && r.blocked) result = 'BLOCKED';
  if (r.blocked && bad.length) result = 'FAIL';
  if (result !== 'PASS') failed++;
  const note = result === 'BLOCKED' ? r.blocked : bad.length ? `failed checks: ${bad.map((c) => `${c.name} (${c.detail})`).join('; ')}` : `${r.checks.length} checks passed${r.note ? '; ' + r.note : ''}`;
  if (!r.artifacts.size) { art(id, 'checks.json', r.checks); }
  else art(id, 'checks.json', r.checks);
  out.push({ id, result, artifacts: [...r.artifacts], note });
}
fs.writeFileSync(RES, JSON.stringify(out, null, 2));
console.log(out.map((o) => `${o.id} ${o.result}`).join('\n'));
console.log(failed === 0 ? 'GHN PASS' : `GHN FAIL (${failed} not passing)`);
process.exit(failed === 0 ? 0 : 1);
