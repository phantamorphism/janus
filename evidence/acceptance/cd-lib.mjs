// Shared harness for cd.mjs (blind QA, group "cd"): mocks + daemon launcher.
import http from 'node:http';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

export const REPO = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
export const SCRATCH = '/tmp/qa-cd';
fs.mkdirSync(SCRATCH, { recursive: true });
if (!fs.existsSync(SCRATCH + '/node_modules/@grpc/grpc-js')) {
  spawnSync('npm', ['install', '--prefix', SCRATCH, '@grpc/grpc-js', '@grpc/proto-loader'], { stdio: 'ignore' });
}
const require = createRequire(SCRATCH + '/');
export const grpc = require('@grpc/grpc-js');
export const protoLoader = require('@grpc/proto-loader');
const pkgDef = protoLoader.loadSync(REPO + '/proto/janus.proto', { keepCase: true, defaults: false, oneofs: true });
export const janusProto = grpc.loadPackageDefinition(pkgDef).janus.v1;

export const BIN = process.env.JANUS_BIN_DIR ||
  (fs.existsSync(REPO + '/target/release/janus-daemon') ? REPO + '/target/release' : '/tmp/qa-target-cd/release');
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- hmac token minting (independent implementation of the spec format)
export function mint(secret, claims, { ns = 'janus-data-plane', exp = Math.floor(Date.now() / 1000) + 3600 } = {}) {
  const hex = Buffer.from(JSON.stringify({ ns, exp, ...claims })).toString('hex');
  const sig = crypto.createHmac('sha256', secret).update(hex).digest('hex');
  return `${hex}.${sig}`;
}

// ---------- mock HTTP upstream
export async function startUpstream(port) {
  const reqs = [];
  const srv = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString();
      const key = req.headers['x-api-key'] || (req.headers.authorization || '').replace(/^Bearer /, '');
      reqs.push({ t: Date.now(), method: req.method, url: req.url, headers: req.headers, body, key });
      if (String(key).includes('garbage')) {
        res.writeHead(401, { 'content-type': 'application/json' });
        return res.end(JSON.stringify({ type: 'error', error: { type: 'authentication_error', message: 'invalid x-api-key' } }));
      }
      if (String(key).includes('acctbilling')) {
        res.writeHead(402, { 'content-type': 'application/json' });
        return res.end(JSON.stringify({ type: 'error', error: { type: 'billing_error', message: 'billing issue: account on hold' } }));
      }
      if (String(key).includes('acctcredit')) {
        res.writeHead(400, { 'content-type': 'application/json' });
        return res.end(JSON.stringify({ type: 'error', error: { type: 'invalid_request_error', message: 'Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.' } }));
      }
      if (String(key).includes('acctdisabled')) {
        res.writeHead(403, { 'content-type': 'application/json' });
        return res.end(JSON.stringify({ type: 'error', error: { type: 'permission_error', message: 'Your account has been disabled' } }));
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ok: true, path: req.url, usage: { input_tokens: 11, output_tokens: 7 }, id: 'msg_mock' }));
    });
  });
  await new Promise((r) => srv.listen(port, '127.0.0.1', r));
  return { reqs, close: () => srv.close(), port };
}

// ---------- mock gRPC plugin
// behaviour: { resolve(req)->{...outcome} | throws {code,message} | Promise, notifyMode: 'record'|'block'|'error' }
export function makePluginImpl(b) {
  const state = { resolves: [], notifies: [], notifyStreams: 0 };
  const impl = {
    ResolveRoute: async (call, cb) => {
      const req = call.request;
      state.resolves.push({ t: Date.now(), req: JSON.parse(JSON.stringify(req)) });
      try {
        const out = await b.resolve(req);
        cb(null, out);
      } catch (e) {
        cb({ code: e.code ?? grpc.status.INTERNAL, message: e.message || 'boom' });
      }
    },
    Notify: (call, cb) => {
      state.notifyStreams++;
      call.on('data', (ev) => {
        state.notifies.push({ t: Date.now(), ev: JSON.parse(JSON.stringify(ev)) });
        if (b.notifyMode === 'error') { call.destroy(new Error('notify boom')); }
      });
      call.on('end', () => cb(null, {}));
      call.on('error', () => {});
      if (b.notifyMode === 'block') { /* never ack, never end */ }
    },
  };
  return { impl, state };
}

// opts: {unixPath} | {port, tls:{cert,key,clientCa|null(no client cert)}}
export async function startPlugin(b, opts) {
  const { impl, state } = makePluginImpl(b);
  const server = new grpc.Server();
  server.addService(janusProto.JanusPlugin.service, impl);
  let creds, addr;
  if (opts.unixPath) {
    try { fs.unlinkSync(opts.unixPath); } catch {}
    addr = 'unix:' + opts.unixPath; creds = grpc.ServerCredentials.createInsecure();
  } else if (opts.tls) {
    addr = `127.0.0.1:${opts.port}`;
    const t = opts.tls;
    creds = grpc.ServerCredentials.createSsl(t.clientCa ? fs.readFileSync(t.clientCa) : null,
      [{ private_key: fs.readFileSync(t.key), cert_chain: fs.readFileSync(t.cert) }], !!t.clientCa);
  } else { addr = `127.0.0.1:${opts.port}`; creds = grpc.ServerCredentials.createInsecure(); }
  await new Promise((res, rej) => server.bindAsync(addr, creds, (e) => (e ? rej(e) : res())));
  return { state, b, stop: () => new Promise((r) => { server.tryShutdown(() => r()); setTimeout(() => { server.forceShutdown(); r(); }, 500); }) };
}

// ---------- daemon
export const ALL = [];
export class Daemon {
  constructor(name, tomlBody, env = {}) {
    ALL.push(this);
    this.name = name;
    this.dir = `${SCRATCH}/${name}`;
    fs.rmSync(this.dir, { recursive: true, force: true });
    fs.mkdirSync(this.dir, { recursive: true });
    this.cfg = `${this.dir}/config.toml`;
    fs.writeFileSync(this.cfg, tomlBody.replaceAll('@DIR@', this.dir));
    this.env = env;
    this.logFile = `${this.dir}/daemon.log`;
  }
  check() {
    return spawnSync(`${BIN}/janus-daemon`, ['check-config', '--config', this.cfg], { env: { ...process.env, ...this.env }, encoding: 'utf8' });
  }
  async start() {
    const out = fs.openSync(this.logFile, 'a');
    this.proc = spawn(this.bin || `${BIN}/janus-daemon`, ['--config', this.cfg], { env: { ...process.env, ...this.env }, stdio: ['ignore', out, out] });
    this.exited = false;
    this.proc.on('exit', () => { this.exited = true; });
    for (let i = 0; i < 100; i++) {
      if (this.exited) throw new Error(`daemon exited early: ${fs.readFileSync(this.logFile, 'utf8').slice(-800)}`);
      if (fs.existsSync(`${this.dir}/admin.sock`)) { const r = this.cli(['status']); if (r.status === 0) return; }
      await sleep(100);
    }
    throw new Error('daemon did not come up: ' + fs.readFileSync(this.logFile, 'utf8').slice(-800));
  }
  async stop() { if (this.proc && !this.exited) { this.proc.kill('SIGTERM'); for (let i = 0; i < 30 && !this.exited; i++) await sleep(100); if (!this.exited) this.proc.kill('SIGKILL'); } }
  cli(args, input) {
    return spawnSync(`${BIN}/janus`, ['--config', this.cfg, ...args], { input, encoding: 'utf8' });
  }
  json(args) { const r = this.cli(['--json', ...args]); try { return JSON.parse(r.stdout); } catch { throw new Error(`cli ${args} -> ${r.status} ${r.stdout} ${r.stderr}`); } }
  addCred(upstream, ref, key) { const r = this.cli(['credential', 'add', '--upstream', upstream, '--ref', ref, '--api-key-stdin'], key); if (r.status !== 0) throw new Error('credential add: ' + r.stderr + r.stdout); return r; }
  logs(args = []) { const j = this.json(['log', 'query', '--limit', '1000', ...args]); return j.requests; }
}

// ---------- data-plane request helper
export function req(port, method, p, { headers = {}, body } = {}) {
  return new Promise((resolve) => {
    const t0 = Date.now();
    const r = http.request({ host: '127.0.0.1', port, method, path: p, headers }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString(), ms: Date.now() - t0 }));
    });
    r.on('error', (e) => resolve({ status: 0, error: e.message, ms: Date.now() - t0 }));
    if (body) r.write(body);
    r.end();
  });
}
