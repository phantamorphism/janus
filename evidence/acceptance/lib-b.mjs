// Shared harness for the group-b acceptance script (black-box: runs built binaries only).
import { spawn, spawnSync, execSync } from 'node:child_process';
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRequire } from 'node:module';

export const REPO = '/Users/michaelhancock/hancock_industries/apps/janus';
export const SCRATCH = '/tmp/qa-b/run';
export const BIN = process.env.JANUS_BIN_DIR ||
  (fs.existsSync(`${REPO}/target/release/janus-daemon`) ? `${REPO}/target/release` : '/tmp/qa-target-b/release');
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const NODE_DIR = '/tmp/qa-b/node';
function loadGrpc() {
  if (!fs.existsSync(`${NODE_DIR}/node_modules/@grpc/grpc-js`)) {
    fs.mkdirSync(NODE_DIR, { recursive: true });
    execSync('npm init -y >/dev/null && npm i @grpc/grpc-js @grpc/proto-loader', { cwd: NODE_DIR, stdio: 'ignore' });
  }
  const req = createRequire(`${NODE_DIR}/x.js`);
  return { grpc: req('@grpc/grpc-js'), loader: req('@grpc/proto-loader') };
}

// ---- recording upstream -------------------------------------------------
export async function startMock(port, handler) {
  const reqs = [];
  const state = { status: 200, body: '{"ok":1}', ctype: 'application/json', connections: 0 };
  const srv = http.createServer((q, r) => {
    const chunks = [];
    q.on('data', (d) => chunks.push(d));
    q.on('end', () => {
      const rec = { method: q.method, url: q.url, headers: q.headers, rawHeaders: q.rawHeaders, body: Buffer.concat(chunks) };
      reqs.push(rec);
      if (handler) return handler(q, r, rec, state);
      r.statusCode = state.status;
      r.setHeader('content-type', state.ctype);
      r.end(state.body);
    });
  });
  srv.on('connection', () => state.connections++);
  await new Promise((res) => srv.listen(port, '127.0.0.1', res));
  return { reqs, state, port, close: () => { srv.closeAllConnections?.(); srv.close(); } };
}

// ---- gRPC plugin mock ---------------------------------------------------
export async function startPlugin(addr /* host:port */, opts = {}) {
  const { grpc, loader } = loadGrpc();
  const def = loader.loadSync(`${REPO}/proto/janus.proto`, { keepCase: true, longs: String, defaults: true, oneofs: true });
  const pkg = grpc.loadPackageDefinition(def).janus.v1;
  const resolves = [];
  const events = [];
  const behaviour = { outcome: { use_default: true } };
  const server = new grpc.Server();
  server.addService(pkg.JanusPlugin.service, {
    ResolveRoute: (call, cb) => {
      resolves.push(call.request);
      cb(null, behaviour.outcome);
    },
    Notify: (call, cb) => {
      call.on('data', (e) => events.push(e));
      call.on('end', () => cb(null, {}));
      call.on('error', () => {});
    },
  });
  await new Promise((res, rej) => server.bindAsync(addr, grpc.ServerCredentials.createInsecure(), (e) => (e ? rej(e) : res())));
  return { resolves, events, behaviour, close: () => server.forceShutdown() };
}

// ---- daemon -------------------------------------------------------------
export async function startDaemon(name, toml, { env = {}, expectFail = false, bin = `${BIN}/janus-daemon` } = {}) {
  const dir = `${SCRATCH}/${name}`;
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  const cfg = `${dir}/config.toml`;
  fs.writeFileSync(cfg, toml.replaceAll('@DATA@', `${dir}/data`));
  const penv = { ...process.env, QA_SECRET: 'S', QA_SECRET2: 'S2', ...env };
  const out = fs.openSync(`${dir}/daemon.log`, 'w');
  const p = spawn(bin, ['--config', cfg], { env: penv, stdio: ['ignore', out, out] });
  let exited = null;
  p.on('exit', (c) => (exited = c));
  const d = { name, dir, cfg, p, env: penv, logCount: 0 };
  const sock = `${dir}/data/admin.sock`;
  for (let i = 0; i < 100; i++) {
    if (exited !== null) break;
    if (fs.existsSync(sock)) { await sleep(150); break; }
    await sleep(100);
  }
  if (exited !== null) {
    d.failed = true;
    d.exitCode = exited;
    d.log = fs.readFileSync(`${dir}/daemon.log`, 'utf8');
    if (!expectFail) throw new Error(`daemon ${name} exited ${exited}: ${d.log}`);
    return d;
  }
  d.stop = () => { try { p.kill('SIGTERM'); } catch {} };
  d.janus = (...args) => {
    const r = spawnSync(`${BIN}/janus`, ['--config', cfg, ...args], { env: penv, encoding: 'utf8' });
    return { code: r.status, out: r.stdout, err: r.stderr };
  };
  d.json = (...args) => {
    const r = d.janus('--json', ...args);
    try { return JSON.parse(r.out); } catch { return { _raw: r.out, _err: r.err, _code: r.code }; }
  };
  d.addKey = (upstream, ref, key) => {
    const r = spawnSync(`${BIN}/janus`, ['--config', cfg, 'credential', 'add', '--upstream', upstream, '--ref', ref, '--api-key-stdin'], { input: key + '\n', env: penv, encoding: 'utf8' });
    if (r.status !== 0) throw new Error('credential add failed: ' + r.stderr);
  };
  d.logs = () => d.json('log', 'query', '--limit', '1000').requests || [];
  // wait until the log holds >= n records, return newest-first list
  d.waitLogs = async (n) => {
    for (let i = 0; i < 60; i++) { const l = d.logs(); if (l.length >= n) return l; await sleep(100); }
    return d.logs();
  };
  return d;
}

// ---- data plane client --------------------------------------------------
export function request({ port, method = 'GET', path: p = '/', headers = {}, body = null }) {
  return new Promise((resolve, reject) => {
    const r = http.request({ host: '127.0.0.1', port, method, path: p, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (d) => chunks.push(d));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, rawHeaders: res.rawHeaders, body: Buffer.concat(chunks) }));
    });
    r.on('error', reject);
    if (body != null) r.write(body);
    r.end();
  });
}

// ---- token minting (independent of Janus) -------------------------------
export function hmacToken(secret, claims) {
  const h = Buffer.from(JSON.stringify(claims)).toString('hex');
  const sig = crypto.createHmac('sha256', secret).update(h, 'ascii').digest('hex');
  return `${h}.${sig}`;
}
export const now = () => Math.floor(Date.now() / 1000);

export function b64u(b) { return Buffer.from(b).toString('base64url'); }
export function jwt(privateKey, claims, kid = 'k1') {
  const head = b64u(JSON.stringify({ alg: 'RS256', typ: 'JWT', kid }));
  const pay = b64u(JSON.stringify(claims));
  const sig = crypto.sign('sha256', Buffer.from(`${head}.${pay}`), privateKey).toString('base64url');
  return `${head}.${pay}.${sig}`;
}

// ---- results collection -------------------------------------------------
export class Results {
  constructor(group) { this.group = group; this.items = {}; fs.mkdirSync(`${REPO}/evidence/artifacts/${group}`, { recursive: true }); }
  d(id) {
    if (!this.items[id]) this.items[id] = { id, fails: [], notes: [], artifacts: {}, blocked: null };
    return this.items[id];
  }
  check(id, cond, msg) { const it = this.d(id); (cond ? it.notes : it.fails).push((cond ? 'ok: ' : 'FAIL: ') + msg); return cond; }
  blocked(id, why) { this.d(id).blocked = why; }
  art(id, name, text) { const it = this.d(id); it.artifacts[name] = (it.artifacts[name] || '') + text + '\n'; }
  finish(ids) {
    const out = [];
    for (const id of ids) {
      const it = this.d(id);
      const files = [];
      for (const [n, t] of Object.entries(it.artifacts)) {
        const rel = `evidence/artifacts/${this.group}/${id}-${n}.txt`;
        fs.writeFileSync(`${REPO}/${rel}`, t || '(empty)\n');
        files.push(rel);
      }
      if (!files.length) {
        const rel = `evidence/artifacts/${this.group}/${id}-notes.txt`;
        fs.writeFileSync(`${REPO}/${rel}`, it.notes.concat(it.fails).join('\n') + '\n' + (it.blocked || '') + '\n');
        files.push(rel);
      }
      const result = it.blocked ? 'BLOCKED' : it.fails.length ? 'FAIL' : 'PASS';
      out.push({ id, result, artifacts: files, note: (it.blocked ? it.blocked + ' | ' : '') + it.fails.concat(it.notes).join('; ') });
    }
    fs.writeFileSync(`${REPO}/evidence/results/${this.group}.json`, JSON.stringify(out, null, 2));
    return out;
  }
}
