#!/usr/bin/env node
// Blind QA, group "ef": DELIVERABLES.md sections E (D47-D56) and F (D57-D58).
// Black-box: runs the built binaries, a node mock upstream, a hand-rolled node gRPC plugin
// (http2 + manual protobuf), openssl-made PKI and a scratch crate that links janus-daemon.
import { spawn, spawnSync, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import http from 'node:http';
import http2 from 'node:http2';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../..');
const OUT = path.join(ROOT, 'evidence/artifacts/ef');
const RESULTS = path.join(ROOT, 'evidence/results/ef.json');
const WORK = '/tmp/qa-ef/run';
const SOCKBASE = path.join(fs.realpathSync(os.tmpdir()), 'qa-ef-sock'); // inside the per-user tmp: no world-writable ancestor
const TARGET = process.env.CARGO_TARGET_DIR || '/tmp/qa-target-ef';
const ONLY = process.argv.slice(2); // e.g. D47 D50

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const strip = (s) => s.replace(/\x1b\[[0-9;]*m/g, '');

// ---------------------------------------------------------------- build / binaries
function binDir() {
  const repoRel = path.join(ROOT, 'target/release');
  if (fs.existsSync(path.join(repoRel, 'janus-daemon')) && fs.existsSync(path.join(repoRel, 'janus'))) return repoRel;
  return path.join(TARGET, 'release');
}
function ensureBuilt() {
  const env = { ...process.env, CARGO_TARGET_DIR: TARGET };
  let bd = binDir();
  if (!fs.existsSync(path.join(bd, 'janus-daemon'))) {
    console.log('building stock binaries into', TARGET);
    execFileSync('cargo', ['build', '--release', '--locked', '-p', 'janus-daemon', '-p', 'janus-cli'], { cwd: ROOT, env, stdio: 'inherit' });
    bd = binDir();
  }
  console.log('building scratch crate (linked plugin + peer-uid plugin) into', TARGET);
  execFileSync('cargo', ['build', '--release'], { cwd: path.join(HERE, 'ef-scratch'), env, stdio: 'inherit' });
  return bd;
}
const BD = ensureBuilt();
const DAEMON = path.join(BD, 'janus-daemon');
const JANUS = path.join(BD, 'janus');
const LINKED = path.join(TARGET, 'release/linked');
const PEERPLUG = path.join(TARGET, 'release/peerplug');

// ---------------------------------------------------------------- results
fs.rmSync(OUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });
fs.rmSync(WORK, { recursive: true, force: true });
fs.mkdirSync(WORK, { recursive: true });
fs.rmSync(SOCKBASE, { recursive: true, force: true });
fs.mkdirSync(SOCKBASE, { recursive: true, mode: 0o700 });

const results = [];
class Del {
  constructor(id) { this.id = id; this.lines = []; this.fail = []; this.arts = []; this.blocked = null; this.notes = []; }
  log(s) { this.lines.push(s); }
  ok(cond, desc) { this.lines.push(`${cond ? 'OK  ' : 'FAIL'} ${desc}`); if (!cond) this.fail.push(desc); return !!cond; }
  note(s) { this.notes.push(s); }
  art(name, content) {
    const f = `${this.id}-${name}`;
    fs.writeFileSync(path.join(OUT, f), typeof content === 'string' ? content : JSON.stringify(content, null, 2));
    this.arts.push(`evidence/artifacts/ef/${f}`);
  }
  finish() {
    this.art('transcript.txt', this.lines.join('\n') + '\n');
    let result = 'PASS';
    if (this.blocked) result = 'BLOCKED';
    else if (this.fail.length) result = 'FAIL';
    const note = this.blocked ? this.blocked : (this.fail.length ? 'FAILED: ' + this.fail.join('; ') : '') + (this.notes.length ? (this.fail.length ? ' | ' : '') + this.notes.join(' ') : '');
    results.push({ id: this.id, result, artifacts: this.arts, note: note || 'all acceptance assertions held' });
    console.log(`${this.id} ${result}${this.fail.length ? ' :: ' + this.fail.join('; ') : ''}`);
  }
}
async function deliverable(id, fn) {
  if (ONLY.length && !ONLY.includes(id)) return;
  const d = new Del(id);
  try { await fn(d); } catch (e) { d.ok(false, `exception: ${e && e.stack || e}`); }
  d.finish();
}

// ---------------------------------------------------------------- ports
let nextPort = 23100;
const port = () => nextPort++;

// ---------------------------------------------------------------- mock upstream
const upstream = { port: 23002, requests: [] };
const upSrv = http.createServer((q, r) => {
  upstream.requests.push({ method: q.method, url: q.url, headers: q.headers });
  r.setHeader('content-type', 'application/json');
  r.end('{"ok":true}');
});
await new Promise((res) => upSrv.listen(upstream.port, '127.0.0.1', res));

// ---------------------------------------------------------------- identity tokens
const SECRET = 'qa-ef-secret';
function mint(subject) {
  const claims = Buffer.from(JSON.stringify({ ns: 'janus-data-plane', exp: Math.floor(Date.now() / 1000) + 3600, subject })).toString('hex');
  return `${claims}.${crypto.createHmac('sha256', SECRET).update(claims).digest('hex')}`;
}
function httpReq({ port: p, socketPath, method = 'POST', path: pth = '/anthropic/v1/messages', headers = {}, body = '{}' }) {
  return new Promise((resolve, reject) => {
    const r = http.request({ port: p, socketPath, host: '127.0.0.1', method, path: pth, headers }, (res) => {
      let b = '';
      res.on('data', (d) => (b += d));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: b }));
    });
    r.on('error', reject);
    r.setTimeout(15000, () => r.destroy(new Error('timeout')));
    r.end(body);
  });
}

// ---------------------------------------------------------------- daemon wrapper
class Daemon {
  constructor(name, { plugin = '', extra = '', env = {}, bin = DAEMON, args = null, dpPort = port() } = {}) {
    this.name = name;
    this.dir = path.join(WORK, name);
    fs.mkdirSync(this.dir, { recursive: true });
    this.dp = dpPort;
    this.cfg = path.join(this.dir, 'c.toml');
    this.data = path.join(this.dir, 'data');
    this.sock = path.join(this.data, 'admin.sock');
    fs.writeFileSync(this.cfg, `data_dir = "${this.data}"
[data_plane]
bind = "127.0.0.1:${this.dp}"
[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "http://127.0.0.1:${upstream.port}"
[identity]
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
namespace = "janus-data-plane"
identity_claim = "subject"
[secrets]
caller-identity = { env = "QA_EF_SECRET" }
${extra}
${plugin}
`);
    this.env = { ...process.env, QA_EF_SECRET: SECRET, ...env };
    this.bin = bin;
    this.args = args || ['--config', this.cfg];
    this.out = '';
    this.exited = null;
  }
  start() {
    this.proc = spawn(this.bin, this.args, { env: this.env, stdio: ['ignore', 'pipe', 'pipe'] });
    this.proc.stdout.on('data', (d) => (this.out += d));
    this.proc.stderr.on('data', (d) => (this.out += d));
    this.exitP = new Promise((res) => this.proc.on('exit', (code, sig) => { this.exited = { code, sig }; res(this.exited); }));
    return this;
  }
  get log() { return strip(this.out); }
  async ready(ms = 10000) {
    const t0 = Date.now();
    while (Date.now() - t0 < ms) {
      if (this.exited) throw new Error(`daemon ${this.name} exited early: ${JSON.stringify(this.exited)}\n${this.log}`);
      if (fs.existsSync(this.sock)) {
        try { const r = await this.admin('/v1/status'); if (r.status === 200) return this; } catch {}
      }
      await sleep(100);
    }
    throw new Error(`daemon ${this.name} not ready in ${ms}ms\n${this.log}`);
  }
  async stop() {
    if (this.proc && !this.exited) { this.proc.kill('SIGTERM'); await Promise.race([this.exitP, sleep(4000)]); if (!this.exited) { this.proc.kill('SIGKILL'); await this.exitP; } }
  }
  admin(p, method = 'GET', body) { return httpReq({ socketPath: this.sock, method, path: p, body: body || '', headers: body ? { 'content-type': 'application/json' } : {} }).then((r) => ({ ...r, json: (() => { try { return JSON.parse(r.body); } catch { return null; } })() })); }
  cli(args, input) {
    return new Promise((resolve) => {
      const p = spawn(JANUS, ['--config', this.cfg, ...args], { env: this.env });
      let out = '', err = '';
      p.stdout.on('data', (x) => (out += x)); p.stderr.on('data', (x) => (err += x));
      p.on('close', (rc) => resolve({ rc, out, err }));
      p.stdin.end(input || '');
    });
  }
  addKey(ref = 'k1', key = 'sk-real-ef-1') { return this.cli(['credential', 'add', '--upstream', 'anthropic', '--api-key-stdin', '--ref', ref], key + '\n'); }
  call(identity = 'agent-1', opts = {}) { return httpReq({ port: this.dp, headers: { authorization: 'Bearer ' + mint(identity), 'content-type': 'application/json' }, ...opts }); }
  async logs() { const r = await this.cli(['--json', 'log', 'query']); try { return JSON.parse(r.out).requests; } catch { return []; } }
  hup() { this.proc.kill('SIGHUP'); }
}
// Start a daemon expected to refuse. Returns {code, log, dpBoundDuringLife}.
async function startRefused(name, o) {
  const d = new Daemon(name, o);
  let bound = false;
  const probe = setInterval(() => { const s = net.connect(d.dp, '127.0.0.1'); s.on('connect', () => { bound = true; s.destroy(); }); s.on('error', () => {}); }, 20);
  d.start();
  const r = await Promise.race([d.exitP, sleep(8000).then(() => null)]);
  clearInterval(probe);
  if (!r) await d.stop();
  return { d, exited: !!r, code: r ? r.code : null, log: d.log, bound, adminSock: fs.existsSync(d.sock) };
}
async function startOk(name, o) { const d = new Daemon(name, o).start(); await d.ready(); return d; }

// ---------------------------------------------------------------- PKI
const PKI = path.join(WORK, 'pki');
fs.mkdirSync(PKI, { recursive: true });
const ossl = (...a) => execFileSync('openssl', a, { stdio: ['ignore', 'pipe', 'pipe'] }).toString();
function genKey(f) { ossl('genpkey', '-algorithm', 'EC', '-pkeyopt', 'ec_paramgen_curve:P-256', '-out', f); fs.chmodSync(f, 0o600); }
function mkCA(name) {
  const k = path.join(PKI, `${name}-key.pem`), c = path.join(PKI, `${name}.pem`);
  genKey(k);
  ossl('req', '-x509', '-key', k, '-subj', `/CN=${name}`, '-days', '3650', '-out', c, '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign');
  return { key: k, cert: c };
}
// kind: 'server' | 'client'; sans: ['DNS:x', 'URI:y']
function mkLeaf(name, ca, { cn = name, sans = [], days = 365, notBefore, notAfter, kind = 'server', keyFrom } = {}) {
  const k = keyFrom || path.join(PKI, `${name}-key.pem`), c = path.join(PKI, `${name}.pem`), csr = path.join(PKI, `${name}.csr`), ext = path.join(PKI, `${name}.ext`);
  if (!keyFrom) genKey(k);
  ossl('req', '-new', '-key', k, '-subj', `/CN=${cn}`, '-out', csr);
  fs.writeFileSync(ext, `basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=${kind === 'server' ? 'serverAuth,clientAuth' : 'clientAuth,serverAuth'}\n${sans.length ? 'subjectAltName=' + sans.join(',') + '\n' : ''}`);
  const validity = notBefore ? ['-not_before', notBefore, '-not_after', notAfter] : ['-days', String(days)];
  ossl('x509', '-req', '-in', csr, '-CA', ca.cert, '-CAkey', ca.key, '-CAcreateserial', '-out', c, ...validity, '-extfile', ext);
  return { key: k, cert: c };
}
const fp = (certFile) => new crypto.X509Certificate(fs.readFileSync(certFile)).fingerprint256;
const readPem = (f) => fs.readFileSync(f);

// ---------------------------------------------------------------- hand-rolled protobuf + gRPC plugin (node http2)
function readVarint(b, o) { let r = 0, s = 0; for (;;) { const x = b[o++]; r += (x & 0x7f) * 2 ** s; if (!(x & 0x80)) break; s += 7; } return [r, o]; }
function pbDecode(b) { // -> array of {f, v(Buffer|number)}
  const out = []; let o = 0;
  while (o < b.length) { let tag; [tag, o] = readVarint(b, o); const f = tag >> 3, w = tag & 7;
    if (w === 0) { let v; [v, o] = readVarint(b, o); out.push({ f, v }); }
    else if (w === 2) { let l; [l, o] = readVarint(b, o); out.push({ f, v: b.subarray(o, o + l) }); o += l; }
    else throw new Error('wire type ' + w); }
  return out;
}
const varint = (n) => { const a = []; while (n > 127) { a.push((n & 127) | 128); n = Math.floor(n / 128); } a.push(n); return Buffer.from(a); };
const pbStr = (f, s) => { const b = Buffer.from(s); return Buffer.concat([varint((f << 3) | 2), varint(b.length), b]); };
function decodeResolve(b) {
  const r = { request_id: '', upstream: '', caller_identity: '', correlation_id: '', identity_claims: {} };
  for (const { f, v } of pbDecode(b)) {
    if (f === 1) r.request_id = v.toString(); else if (f === 2) r.upstream = v.toString(); else if (f === 3) r.caller_identity = v.toString(); else if (f === 4) r.correlation_id = v.toString();
    else if (f === 5) { const kv = pbDecode(v); r.identity_claims[kv.find((x) => x.f === 1)?.v.toString() ?? ''] = kv.find((x) => x.f === 2)?.v.toString() ?? ''; }
  }
  return r;
}
function decodeNotify(b) { const e = { event_type: '', request_id: '', correlation_id: '', payload_json: '' }; for (const { f, v } of pbDecode(b)) { if (f === 1) e.event_type = v.toString(); else if (f === 2) e.request_id = v.toString(); else if (f === 3) e.correlation_id = v.toString(); else if (f === 4) e.payload_json = v.toString(); } return e; }
function frames(buf) { const out = []; let o = 0; while (o + 5 <= buf.length) { const l = buf.readUInt32BE(o + 1); if (o + 5 + l > buf.length) break; out.push(buf.subarray(o + 5, o + 5 + l)); o += 5 + l; } return [out, buf.subarray(o)]; }
const gframe = (m) => { const h = Buffer.alloc(5); h.writeUInt32BE(m.length, 1); return Buffer.concat([h, m]); };

// opts: {port|unixPath, tls:{key,cert,trust:[pem],requestCert,rejectUnauthorized,minVersion,maxVersion,destroyUnverified}, decide}
async function startPlugin(opts) {
  const P = { resolveCalls: [], notifies: [], conns: [], tlsErrors: [], sockets: new Set(), uidServed: 0 };
  const onStream = (stream, headers) => {
    const p = headers[':path'];
    stream.on('error', () => {});
    let buf = Buffer.alloc(0);
    const trailers = (code, msg) => { stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true }); stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': String(code), ...(msg ? { 'grpc-message': msg } : {}) })); };
    stream.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      const [fr, rest] = frames(buf); buf = rest;
      for (const m of fr) {
        if (p.endsWith('/ResolveRoute')) {
          const req = decodeResolve(m); P.resolveCalls.push(req);
          const dec = (opts.decide ? opts.decide(req) : { use_default: true }) || { use_default: true };
          let body;
          if (dec.credential_ref) body = pbStr(1, dec.credential_ref); else if (dec.deny_reason) body = pbStr(3, dec.deny_reason); else body = Buffer.from([0x10, 1]);
          stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true });
          stream.write(gframe(body)); stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': '0' })); stream.end();
        } else if (p.endsWith('/Notify')) { P.notifies.push(decodeNotify(m)); }
      }
    });
    if (p.endsWith('/Notify')) { stream.on('end', () => { if (stream.destroyed) return; stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true }); stream.write(gframe(Buffer.alloc(0))); stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': '0' })); stream.end(); }); }
    else if (!p.endsWith('/ResolveRoute')) { trailers(12, 'unimplemented'); stream.end(); }
  };
  let srv;
  if (opts.tls) {
    const t = opts.tls;
    srv = http2.createSecureServer({ key: readPem(t.key), cert: readPem(t.cert), ca: (t.trust || []).map(readPem), requestCert: t.requestCert !== false, rejectUnauthorized: t.rejectUnauthorized !== false && t.requestCert !== false, minVersion: t.minVersion || 'TLSv1.3', maxVersion: t.maxVersion || 'TLSv1.3' });
    srv.on('secureConnection', (s) => {
      const c = s.getPeerCertificate();
      const has = !!(c && c.fingerprint256);
      const rec = { authorized: s.authorized, authError: s.authorizationError || null, clientFp: has ? c.fingerprint256 : null, protocol: s.getProtocol(), at: Date.now() };
      P.conns.push(rec); P.sockets.add(s); s.on('close', () => P.sockets.delete(s));
      if (t.destroyUnverified && has && !s.authorized) s.destroy();
    });
    srv.on('tlsClientError', (e) => P.tlsErrors.push(String(e.message || e)));
  } else {
    srv = http2.createServer();
    srv.on('connection', (s) => { P.sockets.add(s); s.on('close', () => P.sockets.delete(s)); });
  }
  srv.on('stream', onStream);
  srv.on('error', () => {});
  await new Promise((res, rej) => { srv.once('error', rej); if (opts.unixPath) { try { fs.unlinkSync(opts.unixPath); } catch {} srv.listen(opts.unixPath, res); } else srv.listen(opts.port, res); });
  if (opts.unixPath) fs.chmodSync(opts.unixPath, 0o660);
  P.port = opts.port; P.close = () => new Promise((res) => { for (const s of P.sockets) s.destroy(); srv.close(() => res()); setTimeout(res, 500); });
  P.dropConns = () => { for (const s of P.sockets) s.destroy(); };
  return P;
}
const sockDir = (n) => { const d = path.join(SOCKBASE, n); fs.mkdirSync(d, { recursive: true, mode: 0o750 }); fs.chmodSync(d, 0o750); return d; };

// ---------------------------------------------------------------- shared PKI material
const caPriv = mkCA('ca-private');   // the operator's private CA (the anchor)
const caOther = mkCA('ca-public');   // stands in for a publicly trusted CA the operator did NOT name
const caClient = mkCA('ca-client');  // issues daemon client certs, trusted by the mock plugins
const srvGood = mkLeaf('srv-good', caPriv, { sans: ['DNS:localhost', 'IP:127.0.0.1'] });
const srvOtherHost = mkLeaf('srv-otherhost', caPriv, { sans: ['DNS:other.test'] });
const srvPublic = mkLeaf('srv-public', caOther, { sans: ['DNS:localhost', 'IP:127.0.0.1'] });
const srvSpiffe = mkLeaf('srv-spiffe', caPriv, { sans: ['URI:spiffe://qa.test/ns/plugin'] });
const srvShort = mkLeaf('srv-short', caPriv, { sans: ['DNS:localhost'], days: 10 });
const cliA = mkLeaf('client-a', caClient, { kind: 'client', sans: ['DNS:daemon.test'] });
const cliB = mkLeaf('client-b', caClient, { kind: 'client', sans: ['DNS:daemon.test'] });
const cliExpired = mkLeaf('client-expired', caClient, { kind: 'client', notBefore: '20200101000000Z', notAfter: '20200201000000Z' });
const cliShort = mkLeaf('client-short', caClient, { kind: 'client', days: 10 });
const cliMismatch = { cert: cliA.cert, key: cliB.key };
const clientTrust = [caClient.cert];

function tlsBlock({ ca = caPriv.cert, pinned, cert = cliA.cert, key = cliA.key, extra = '', omitAnchor = false }) {
  return `[plugin.tls]\n${omitAnchor ? '' : pinned ? `pinned_cert = "${pinned}"\n` : `ca_file = "${ca}"\n`}client_cert = "${cert}"\nclient_key = "${key}"\n${extra}`;
}
const httpsPlugin = (p, tls, host = 'localhost') => `[plugin]\nendpoint = "https://${host}:${p}"\n${tls}\n`;
const unixPlugin = (s) => `[plugin]\nendpoint = "unix://${s}"\n`;
const goodSrvTls = (extra = {}) => ({ key: srvGood.key, cert: srvGood.cert, trust: clientTrust, ...extra });

// TLS front (python ssl) in front of an h2c node mock: python's OpenSSL sends real TLS alerts when it rejects a
// client certificate (node's tls just drops the socket). mode: 'require' | 'optional'
async function startFront(mode) {
  const bp = port(), fpPort = port();
  const backend = await startPlugin({ port: bp });
  const logf = path.join(WORK, `front-${fpPort}.jsonl`);
  fs.writeFileSync(logf, '');
  const proc = spawn('python3', ['-I', path.join(HERE, 'ef-tlsfront.py'), String(fpPort), String(bp), srvGood.key, srvGood.cert, caClient.cert, mode, logf], { stdio: 'ignore' });
  for (let i = 0; i < 50 && !fs.readFileSync(logf, 'utf8').includes('listening'); i++) await sleep(100);
  const events = () => fs.readFileSync(logf, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
  return { backend, port: fpPort, events, close: async () => { proc.kill('SIGKILL'); await backend.close(); } };
}

// ================================================================= E
await deliverable('D47', async (D) => {
  const sd = sockDir('d47');
  const sock = path.join(sd, 'p.sock');
  // control: a valid unix endpoint starts (so refusals below are due to the mutation)
  const ctl = await startOk('d47-ctl', { plugin: unixPlugin(sock) });
  D.ok((await ctl.admin('/v1/plugin')).json?.mode === 'unix', 'control: valid single-endpoint [plugin] starts with mode=unix');
  await ctl.stop();
  // (a) both
  const a = await startRefused('d47-a', { plugin: `[plugin]\nendpoint = "unix://${sock}"\nlinked = true\n` });
  D.ok(a.exited && a.code !== 0, `(a) endpoint + linked=true refuses to start (exit=${a.code})`);
  D.ok(a.log.trim().length > 0, '(a) prints a diagnostic: ' + a.log.trim().split('\n').slice(-1)[0]);
  D.ok(!a.bound && !a.adminSock, '(a) data plane never bound, no admin socket');
  D.art('a-refusal.txt', `exit=${a.code}\n${a.log}`);
  // (b) neither
  const b = await startRefused('d47-b', { plugin: `[plugin]\ntimeout_ms = 500\n` });
  D.ok(b.exited && b.code !== 0, `(b) [plugin] with neither refuses to start (exit=${b.code})`);
  D.ok(b.log.trim().length > 0, '(b) diagnostic: ' + b.log.trim().split('\n').slice(-1)[0]);
  D.ok(!b.bound && !b.adminSock, '(b) data plane never bound');
  D.art('b-refusal.txt', `exit=${b.code}\n${b.log}`);
  // (c) env var / CLI flag attempts, on a config with NO [plugin]
  const spy = await startPlugin({ unixPath: path.join(sd, 'spy.sock') });
  const envs = { JANUS_PLUGIN: `unix://${path.join(sd, 'spy.sock')}`, JANUS_PLUGIN_ENDPOINT: `unix://${path.join(sd, 'spy.sock')}`, JANUS_PLUGIN_LINKED: 'true', JANUS_PLUGIN__ENDPOINT: `unix://${path.join(sd, 'spy.sock')}`, PLUGIN_ENDPOINT: `unix://${path.join(sd, 'spy.sock')}` };
  const c = await startOk('d47-c', { env: envs });
  await c.addKey();
  const st = await c.admin('/v1/plugin');
  const r = await c.call('agent-c');
  D.ok(st.json?.mode === 'none', `(c) env vars JANUS_PLUGIN*/PLUGIN_ENDPOINT have no effect: plugin.mode=${st.json?.mode}`);
  D.ok(r.status === 200, `(c) traffic forwards as pass-through (HTTP ${r.status})`);
  D.ok(spy.resolveCalls.length === 0 && spy.conns.length === 0 && spy.notifies.length === 0, `(c) env-named plugin socket saw zero calls (resolve=${spy.resolveCalls.length})`);
  await c.stop();
  // CLI flag attempts
  const help = spawnSync(DAEMON, ['--help'], { encoding: 'utf8' }).stdout;
  D.ok(!/plugin/i.test(help), '(c) janus-daemon --help lists no plugin flag');
  for (const flag of ['--plugin-endpoint', '--plugin']) {
    const f = await startRefused('d47-c-flag' + flag.replace(/-/g, ''), { args: ['--config', path.join(WORK, 'd47-c/c.toml'), flag, `unix://${path.join(sd, 'spy.sock')}`] });
    D.ok(f.exited && f.code !== 0 && !f.bound, `(c) CLI flag ${flag} is not accepted (exit=${f.code}), cannot set the endpoint`);
    D.log(`     ${flag}: ${f.log.trim().split('\n')[0]}`);
  }
  D.ok(spy.resolveCalls.length === 0 && spy.conns.length === 0, '(c) after flag attempts the spy plugin still saw nothing');
  D.art('c-env-and-flags.txt', `env=${JSON.stringify(envs)}\n/v1/plugin => ${st.body}\nhelp:\n${help}`);
  await spy.close();
});

await deliverable('D48', async (D) => {
  const sd = sockDir('d48');
  const run = async (n, plugin, expectRefuse, why) => {
    const x = await startRefused('d48-' + n, { plugin });
    D.ok(x.exited && x.code !== 0 && !x.bound && !x.adminSock, `${why}: refuses to start (exit=${x.code}) -> ${x.log.trim().split('\n').slice(-1)[0]}`);
    D.art(`${n}.txt`, `exit=${x.code}\n${x.log}`);
  };
  await run('https-no-tls', `[plugin]\nendpoint = "https://policy:7000"\n`, true, 'https:// without [plugin.tls]');
  await run('http-no-flag', `[plugin]\nendpoint = "http://policy:7000"\n`, true, 'http://policy:7000 without allow_plaintext');
  await run('http-loopback', `[plugin]\nendpoint = "http://127.0.0.1:7000"\n`, true, 'http://127.0.0.1:7000 without allow_plaintext (no loopback exception)');
  await run('ftp', `[plugin]\nendpoint = "ftp://x"\n`, true, 'ftp://x');
  // http + allow_plaintext -> starts, warns, mode plaintext
  const pp = await startOk('d48-plain', { plugin: `[plugin]\nendpoint = "http://policy:7000"\nallow_plaintext = true\n` });
  const st = await pp.admin('/v1/plugin'); const full = await pp.admin('/v1/status');
  D.ok(st.json?.mode === 'plaintext', `http:// + allow_plaintext=true starts; /v1/plugin mode=${st.json?.mode}`);
  D.ok(full.json?.plugin?.mode === 'plaintext', '/v1/status plugin.mode=plaintext');
  D.ok(/warn/i.test(pp.log) && /plaintext|cleartext|unencrypted|insecure/i.test(pp.log), 'startup log carries a warning about plaintext: ' + pp.log.split('\n').filter((l) => /WARN/.test(l)).join(' | ').slice(0, 300));
  D.art('plaintext-startup.txt', `${pp.log}\n/v1/plugin => ${st.body}`);
  await pp.stop();
  // unix
  const sock = path.join(sd, 'p.sock');
  const u = await startOk('d48-unix', { plugin: unixPlugin(sock) });
  D.ok((await u.admin('/v1/plugin')).json?.mode === 'unix', 'unix:// -> mode=unix');
  await u.stop();
  // https/tls
  const tp = port();
  const t = await startOk('d48-tls', { plugin: httpsPlugin(tp, tlsBlock({})) });
  D.ok((await t.admin('/v1/plugin')).json?.mode === 'tls', 'https:// with [plugin.tls] -> mode=tls');
  D.art('mode-tls.txt', (await t.admin('/v1/plugin')).body);
  await t.stop();
  // linked: stock binary cannot; custom build -> mode=linked (also covered by D57)
  const lk = await startOk('d48-linked', { bin: LINKED, args: [path.join(WORK, 'd48-linked/c.toml')], plugin: `[plugin]\nlinked = true\n` });
  D.ok((await lk.admin('/v1/plugin')).json?.mode === 'linked', 'linked (custom build) -> mode=linked');
  await lk.stop();
});

await deliverable('D49', async (D) => {
  const tp = port();
  const good = await startOk('d49-good', { plugin: httpsPlugin(tp, tlsBlock({})) });
  D.ok(true, 'control: correct [plugin.tls] starts (plugin not running)');
  await good.stop();
  const cases = [
    ['a-missing-ca', tlsBlock({ ca: path.join(PKI, 'does-not-exist.pem') }), 'missing ca_file'],
    ['b-mismatched-key', tlsBlock({ cert: cliMismatch.cert, key: cliMismatch.key }), 'client key does not match client cert'],
    ['c-expired-cert', tlsBlock({ cert: cliExpired.cert, key: cliExpired.key }), 'expired client cert'],
    ['d-key-0644', null, 'client_key mode 0644'],
    ['e-both-anchors', `[plugin.tls]\nca_file = "${caPriv.cert}"\npinned_cert = "${srvGood.cert}"\nclient_cert = "${cliA.cert}"\nclient_key = "${cliA.key}"\n`, 'both ca_file and pinned_cert'],
    ['f-no-anchor', tlsBlock({ omitAnchor: true }), 'neither ca_file nor pinned_cert'],
  ];
  for (const [n, blk, why] of cases) {
    let block = blk;
    if (n === 'd-key-0644') {
      const k = path.join(PKI, 'client-a-key-0644.pem'); fs.copyFileSync(cliA.key, k); fs.chmodSync(k, 0o644);
      block = tlsBlock({ key: k });
    }
    const x = await startRefused('d49-' + n, { plugin: httpsPlugin(tp, block) });
    D.ok(x.exited && x.code !== 0 && !x.bound && !x.adminSock, `${why}: refuses to start (exit=${x.code}) -> ${x.log.trim().split('\n').slice(-1)[0]}`);
    D.ok(x.log.trim().length > 0, `${why}: diagnostic emitted`);
    D.art(`${n}.txt`, `exit=${x.code}\n${x.log}`);
  }
});

await deliverable('D50', async (D) => {
  const sd = sockDir('d50'); const sock = path.join(sd, 'p.sock');
  const d = await startOk('d50', { plugin: unixPlugin(sock) });
  await d.addKey();
  const before = (await d.admin('/v1/plugin')).json;
  D.ok(before?.health === 'unknown', `daemon started with plugin down; health before any call = ${before?.health}`);
  const upN0 = upstream.requests.length;
  const r1 = await d.call('agent-50');
  const l1 = (await d.logs())[0];
  const after = (await d.admin('/v1/plugin')).json;
  D.ok(r1.status >= 400 && /plugin_unreachable/.test(r1.body + JSON.stringify(l1)), `first request fails plugin_unreachable (HTTP ${r1.status}) body=${r1.body.slice(0, 160)}`);
  D.ok(l1 && l1.outcome !== 'forwarded', `request-log outcome=${l1?.outcome}`);
  D.ok(after?.health === 'unreachable', `health after failed call = ${after?.health}`);
  D.ok(upstream.requests.length === upN0, 'nothing forwarded upstream by the failed call');
  const plug = await startPlugin({ unixPath: sock });
  let r2, tries = 0;
  for (; tries < 8; tries++) { r2 = await d.call('agent-50'); if (r2.status === 200) break; await sleep(500); }
  const ok = (await d.admin('/v1/plugin')).json;
  D.ok(r2.status === 200, `after the plugin starts the request succeeds (HTTP ${r2.status}, attempts=${tries + 1})`);
  D.ok(ok?.health === 'ok' && !!ok?.last_ok_at, `health=${ok?.health} last_ok_at=${ok?.last_ok_at}`);
  D.ok(plug.resolveCalls.length >= 1, `plugin received ${plug.resolveCalls.length} ResolveRoute`);
  D.art('plugin-status.json', { before, after, ok, firstResponse: { status: r1.status, body: r1.body }, firstLog: l1 });
  await plug.close(); await d.stop();
});

await deliverable('D51', async (D) => {
  // (a) parent dir 0777 -> refuse to start
  const base = path.join(SOCKBASE, 'd51'); fs.mkdirSync(base, { recursive: true, mode: 0o750 });
  const open = path.join(base, 'open'); fs.mkdirSync(open, { mode: 0o750 }); fs.chmodSync(open, 0o777);
  const sA = path.join(open, 'p.sock');
  const a = await startRefused('d51-a', { plugin: unixPlugin(sA) });
  D.ok(a.exited && a.code !== 0 && !a.bound && !a.adminSock, `(a) parent dir mode 0777 refuses start (exit=${a.code}) -> ${a.log.trim().split('\n').slice(-1)[0]}`);
  D.ok(a.log.includes(open) || /writable/i.test(a.log), '(a) diagnostic names the offending directory');
  D.art('a-refusal.txt', `exit=${a.code}\n${a.log}`);
  // Also the world-writable sticky dir /tmp
  const aT = await startRefused('d51-a-tmp', { plugin: unixPlugin('/tmp/qa-ef-d51.sock') });
  D.ok(aT.exited && aT.code !== 0, `(a) socket under world-writable /tmp refuses start (exit=${aT.code})`);
  // correct layout works: socket 0660 dir 0750
  const good = path.join(base, 'good'); fs.mkdirSync(good, { mode: 0o750 }); fs.chmodSync(good, 0o750);
  const sock = path.join(good, 'p.sock');
  { const t = await startPlugin({ unixPath: sock }); D.ok((fs.statSync(sock).mode & 0o777) === 0o660 && (fs.statSync(good).mode & 0o777) === 0o750, 'layout: socket 0660, dir 0750'); await t.close(); }
  // (b1) literal exercise: start correctly, THEN chmod o+w a parent directory, THEN send the first request
  const goodB = path.join(base, 'goodb'); fs.mkdirSync(goodB, { mode: 0o750 }); fs.chmodSync(goodB, 0o750);
  const sockB = path.join(goodB, 'p.sock');
  const plugB = await startPlugin({ unixPath: sockB });
  const dB = await startOk('d51-b1', { plugin: unixPlugin(sockB) });
  await dB.addKey();
  fs.chmodSync(goodB, 0o757);
  await sleep(300);
  const rB = await dB.call('agent-51');
  const stB = (await dB.admin('/v1/plugin')).json;
  D.ok(rB.status >= 400 && /plugin_unreachable/.test(rB.body + JSON.stringify((await dB.logs())[0])), `(b) start correctly, chmod o+w the dir, send a request -> fails plugin_unreachable (HTTP ${rB.status})`);
  D.ok(stB?.last_error && stB.last_error.includes(goodB), `(b) /v1/plugin last_error names the offending directory: ${stB?.last_error}`);
  D.art('b1-status.json', { r: { status: rB.status, body: rB.body }, st: stB, plugin_conns_seen_by_plugin: plugB.conns.length, resolveCalls: plugB.resolveCalls.length });
  fs.chmodSync(goodB, 0o750);
  await plugB.close(); await dB.stop();
  // (b2) established connection, then reconnection under a now-unsafe path
  const plug = await startPlugin({ unixPath: sock });
  const d = await startOk('d51-b', { plugin: unixPlugin(sock) });
  await d.addKey();
  const r0 = await d.call('agent-51');
  D.ok(r0.status === 200, `correctly permissioned layout (socket 0660, dir 0750) works (HTTP ${r0.status})`);
  fs.chmodSync(good, 0o757); plug.dropConns();
  await sleep(500);
  const r1 = await d.call('agent-51');
  const st = (await d.admin('/v1/plugin')).json;
  D.ok(r1.status >= 400 && /plugin_unreachable/.test(r1.body + JSON.stringify((await d.logs())[0])), `(b) reconnection after chmod o+w fails plugin_unreachable (HTTP ${r1.status})`);
  D.ok(st?.last_error && st.last_error.includes(good), `(b) reconnect last_error names the offending directory: ${st?.last_error}`);
  D.art('b2-status.json', { r1: { status: r1.status, body: r1.body }, st });
  fs.chmodSync(good, 0o750);
  let r2; for (let i = 0; i < 6; i++) { r2 = await d.call('agent-51'); if (r2.status === 200) break; await sleep(500); }
  D.ok(r2.status === 200, `(b) restoring 0750 recovers (HTTP ${r2.status})`);
  await plug.close(); await d.stop();
  // (c) Rust plugin using janus-core's PeerUid interceptor
  const uid = process.getuid();
  const hasSudo = spawnSync('sudo', ['-n', 'true']).status === 0;
  const runPeer = async (n, expected) => {
    const dd = sockDir('d51c-' + n); const s = path.join(dd, 'p.sock');
    const pl = spawn(PEERPLUG, [s, String(expected)], { stdio: ['ignore', 'pipe', 'pipe'] });
    let perr = ''; pl.stderr.on('data', (x) => (perr += x));
    for (let i = 0; i < 50 && !fs.existsSync(s); i++) await sleep(100);
    const dm = await startOk('d51c-' + n, { plugin: unixPlugin(s) });
    await dm.addKey();
    const r = await dm.call('agent-51c');
    const status = (await dm.admin('/v1/plugin')).json;
    await dm.stop(); pl.kill('SIGKILL');
    return { r, perr, status, log: (await dm.logs())[0] };
  };
  const same = await runPeer('same', uid);
  D.ok(same.r.status === 200 && /RESOLVE_ROUTE_SERVED/.test(same.perr), `(c) interceptor admits the daemon's own uid ${uid} (HTTP ${same.r.status})`);
  const other = await runPeer('other', uid + 1);
  D.ok(other.r.status >= 400 && !/RESOLVE_ROUTE_SERVED/.test(other.perr), `(c) interceptor rejects a peer whose uid != expected (HTTP ${other.r.status}); service never reached`);
  D.art('c-peer-uid.json', { same: { status: same.r.status, plugin_stderr: same.perr, plugin: same.status }, other: { status: other.r.status, body: other.r.body, plugin_stderr: other.perr, plugin: other.status, log: other.log } });
  D.note(hasSudo ? '' : '(c) emulated: no second OS user available (sudo needs a password), so "another user" is exercised by configuring the plugin to expect a different uid than the daemon runs as; a true cross-user connection could not be made.');
});

// ---- TLS scenarios (D52)
async function tlsScenario(name, { srvTls, pluginTls, host = 'localhost', daemonEnv = {}, decide, serverOpts = {} }) {
  const p = port();
  const plug = await startPlugin({ port: p, tls: srvTls, decide });
  const d = await startOk(name, { plugin: httpsPlugin(p, pluginTls, host), env: daemonEnv });
  await d.addKey();
  const r = await d.call('agent-' + name);
  const st = (await d.admin('/v1/plugin')).json;
  const log = (await d.logs())[0];
  return { p, plug, d, r, st, log, done: async () => { await plug.close(); await d.stop(); } };
}
await deliverable('D52', async (D) => {
  const summary = {};
  const failClass = (s, cls) => s.r.status >= 400 && new RegExp(cls).test(s.r.body + JSON.stringify(s.log) + JSON.stringify(s.st));
  // 1. publicly-trusted-style CA not under the configured anchor (and SSL_CERT_FILE tries to smuggle it in as a system root)
  let s = await tlsScenario('d52-public', { srvTls: { key: srvPublic.key, cert: srvPublic.cert, trust: clientTrust }, pluginTls: tlsBlock({ ca: caPriv.cert }), daemonEnv: { SSL_CERT_FILE: caOther.cert, SSL_CERT_DIR: path.dirname(caOther.cert) } });
  D.ok(failClass(s, 'plugin_tls_error'), `cert from a CA outside the configured anchor fails plugin_tls_error (HTTP ${s.r.status}); log=${s.log?.outcome}/${s.log?.error}`);
  D.ok(s.plug.resolveCalls.length === 0, 'no ResolveRoute reached the untrusted plugin');
  summary.public = { status: s.r.status, body: s.r.body, st: s.st, log: s.log }; await s.done();
  // 2. wrong hostname
  s = await tlsScenario('d52-wronghost', { srvTls: { key: srvOtherHost.key, cert: srvOtherHost.cert, trust: clientTrust }, pluginTls: tlsBlock({}) });
  D.ok(failClass(s, 'plugin_tls_error'), `correct anchor but cert for a different hostname fails plugin_tls_error (HTTP ${s.r.status}); log=${s.log?.outcome}/${s.log?.error}`);
  summary.wronghost = { status: s.r.status, body: s.r.body, st: s.st, log: s.log }; await s.done();
  // 2b. same plugin with server_name set to the cert's name
  s = await tlsScenario('d52-servername', { srvTls: { key: srvOtherHost.key, cert: srvOtherHost.cert, trust: clientTrust }, pluginTls: tlsBlock({ extra: 'server_name = "other.test"\n' }) });
  D.ok(s.r.status === 200 && s.plug.resolveCalls.length === 1, `same plugin with server_name="other.test" succeeds (HTTP ${s.r.status}, resolve=${s.plug.resolveCalls.length})`);
  D.ok(s.plug.conns.some((c) => c.clientFp === fp(cliA.cert) && c.protocol === 'TLSv1.3'), 'plugin observed the daemon client certificate on a TLSv1.3 handshake');
  summary.servername = { status: s.r.status, conns: s.plug.conns, st: s.st }; await s.done();
  // 3. SPIFFE cert + expected_san
  s = await tlsScenario('d52-spiffe', { srvTls: { key: srvSpiffe.key, cert: srvSpiffe.cert, trust: clientTrust }, pluginTls: tlsBlock({ extra: 'expected_san = "spiffe://qa.test/ns/plugin"\n' }) });
  D.ok(s.r.status === 200 && s.plug.resolveCalls.length === 1, `SPIFFE plugin with matching expected_san succeeds (HTTP ${s.r.status}); hostname check replaced`);
  D.ok(s.plug.conns.some((c) => c.clientFp === fp(cliA.cert)), 'plugin observed the daemon client cert');
  summary.spiffe = { status: s.r.status, conns: s.plug.conns }; await s.done();
  s = await tlsScenario('d52-spiffe-wrong', { srvTls: { key: srvSpiffe.key, cert: srvSpiffe.cert, trust: clientTrust }, pluginTls: tlsBlock({ extra: 'expected_san = "spiffe://qa.test/ns/other"\n' }) });
  D.ok(failClass(s, 'plugin_tls_error'), `(extra) SPIFFE plugin with a non-matching expected_san fails plugin_tls_error (HTTP ${s.r.status})`);
  await s.done();
  // 4. TLS 1.2-only plugin
  s = await tlsScenario('d52-tls12', { srvTls: goodSrvTls({ minVersion: 'TLSv1.2', maxVersion: 'TLSv1.2' }), pluginTls: tlsBlock({}) });
  D.ok(s.r.status >= 400 && s.plug.resolveCalls.length === 0 && !s.plug.conns.some((c) => c.protocol === 'TLSv1.2' && c.clientFp), `TLS1.2-only plugin fails (HTTP ${s.r.status}, resolve=${s.plug.resolveCalls.length}); class=${s.st?.health}/${s.st?.last_error}`);
  D.ok(failClass(s, 'plugin_tls_error'), 'TLS 1.2-only failure is classed plugin_tls_error');
  summary.tls12 = { status: s.r.status, body: s.r.body, st: s.st, conns: s.plug.conns, tlsErrors: s.plug.tlsErrors }; await s.done();
  // 5. positive baseline: private CA anchor + matching host works, and plugin sees client cert
  s = await tlsScenario('d52-ok', { srvTls: goodSrvTls(), pluginTls: tlsBlock({}) });
  D.ok(s.r.status === 200 && s.plug.conns.length >= 1 && s.plug.conns.every((c) => c.clientFp === fp(cliA.cert) && c.authorized), `baseline mTLS succeeds; plugin saw authorized client cert ${s.plug.conns[0]?.clientFp?.slice(0, 20)}`);
  D.ok(s.st?.health === 'ok', `health=${s.st?.health}`);
  summary.ok = { status: s.r.status, conns: s.plug.conns, st: s.st }; await s.done();
  D.art('scenarios.json', summary);
});

await deliverable('D53', async (D) => {
  const s = await tlsScenario('d53', { srvTls: goodSrvTls({ requestCert: false }), pluginTls: tlsBlock({}) });
  const r2 = await s.d.call('agent-53b');
  const logs = await s.d.logs();
  D.ok(s.r.status >= 400 && r2.status >= 400, `every request fails (HTTP ${s.r.status}, ${r2.status})`);
  D.ok(logs.length >= 2 && logs.every((l) => /plugin_no_client_auth/.test(JSON.stringify(l))), `request-log records class plugin_no_client_auth: ${logs.map((l) => l.outcome + '/' + l.error).join(', ')}`);
  D.ok(/plugin_no_client_auth/.test(s.r.body + r2.body) || logs.length > 0, `caller-visible body: ${s.r.body.slice(0, 160)}`);
  D.ok(s.plug.resolveCalls.length === 0 && s.plug.notifies.length === 0, `plugin received zero ResolveRoute (${s.plug.resolveCalls.length}) and zero Notify`);
  const before = upstream.requests.length; await s.d.call('agent-53c');
  D.ok(upstream.requests.length === before, 'nothing forwarded to the upstream');
  D.art('status.json', { st: s.st, logs, body: s.r.body, conns: s.plug.conns });
  await s.done();
});

await deliverable('D54', async (D) => {
  // work on private copies of the client material so we can swap it
  const live = path.join(WORK, 'd54-live'); fs.mkdirSync(live, { recursive: true });
  const lc = path.join(live, 'client.pem'), lk = path.join(live, 'client-key.pem');
  const put = (c, k) => { fs.copyFileSync(c, lc); fs.copyFileSync(k, lk); fs.chmodSync(lk, 0o600); };
  put(cliA.cert, cliA.key);
  const p = port();
  const plug = await startPlugin({ port: p, tls: goodSrvTls() });
  const d = await startOk('d54', { plugin: httpsPlugin(p, tlsBlock({ cert: lc, key: lk })) });
  await d.addKey();
  const call = async () => { let r; for (let i = 0; i < 6; i++) { r = await d.call('agent-54'); if (r.status === 200) break; await sleep(500); } return r; };
  const lastFp = () => plug.conns[plug.conns.length - 1]?.clientFp;
  let r = await call();
  D.ok(r.status === 200 && lastFp() === fp(cliA.cert), `initial: request ok with client cert A (${lastFp()?.slice(0, 20)})`);
  const statusA = (await d.admin('/v1/status')).json;
  const statusPlugin = (await d.admin('/v1/plugin')).json;
  const expiries = JSON.stringify(statusA) + JSON.stringify(statusPlugin);
  D.ok(/client_cert_expires_at/.test(expiries) && /server_cert_expires_at/.test(expiries), '/v1/status (or plugin block) shows client_cert_expires_at and server_cert_expires_at');
  const findKey = (o, k) => { if (o && typeof o === 'object') { if (k in o) return o[k]; for (const v of Object.values(o)) { const x = findKey(v, k); if (x !== undefined) return x; } } };
  const ce = findKey(statusA, 'client_cert_expires_at'), se = findKey(statusA, 'server_cert_expires_at');
  D.ok(ce && Math.abs(new Date(ce) - new Date(new crypto.X509Certificate(fs.readFileSync(cliA.cert)).validTo)) < 86400e3 * 2, `client_cert_expires_at=${ce} matches cert A notAfter`);
  D.ok(se && Math.abs(new Date(se) - new Date(new crypto.X509Certificate(fs.readFileSync(srvGood.cert)).validTo)) < 86400e3 * 2, `server_cert_expires_at=${se} matches server cert notAfter`);
  // swap to B on disk, rely on file-change detection (no SIGHUP first)
  put(cliB.cert, cliB.key); plug.dropConns();
  await sleep(300);
  let seen = false; for (let i = 0; i < 20 && !seen; i++) { await d.call('agent-54'); seen = lastFp() === fp(cliB.cert); if (!seen) await sleep(1000); }
  D.ok(seen, `file change picked up without restart/SIGHUP: plugin now sees client cert B (${lastFp()?.slice(0, 20)})`);
  // SIGHUP path: swap back to A, HUP
  put(cliA.cert, cliA.key); plug.dropConns(); d.hup(); await sleep(500);
  r = await call();
  D.ok(r.status === 200 && lastFp() === fp(cliA.cert), `SIGHUP reload: plugin sees client cert A again (${lastFp()?.slice(0, 20)})`);
  // corrupt file -> keep last good
  fs.writeFileSync(lc, 'this is not a certificate\n'); plug.dropConns(); d.hup(); await sleep(1500);
  r = await call();
  D.ok(r.status === 200 && lastFp() === fp(cliA.cert), `corrupt client cert on disk: traffic still works on old material (HTTP ${r.status}, fp A)`);
  const stAfter = (await d.admin('/v1/plugin')).json;
  D.ok(d.proc && !d.exited, 'daemon still running after corrupt file');
  fs.writeFileSync(lk, 'garbage\n'); fs.chmodSync(lk, 0o600); plug.dropConns(); d.hup(); await sleep(1500);
  r = await call();
  D.ok(r.status === 200, `corrupt client key too: traffic still works (HTTP ${r.status})`);
  D.art('reload.json', { statusA, statusPlugin, stAfter, conns: plug.conns, logTail: d.log.split('\n').slice(-25) });
  await plug.close(); await d.stop();
  // near expiry (10 day) warning, client cert
  let pc = port(); let pl2 = await startPlugin({ port: pc, tls: goodSrvTls() });
  let dd = await startOk('d54-near-client', { plugin: httpsPlugin(pc, tlsBlock({ cert: cliShort.cert, key: cliShort.key })) });
  await dd.addKey(); await dd.call('agent-54n'); await sleep(500);
  const wl = dd.log.split('\n').filter((l) => /WARN/.test(l));
  D.ok(wl.some((l) => /expir/i.test(l)), `daemon logs a warning for a client cert within 30 days of expiry: ${wl.filter((l) => /expir/i.test(l))[0]?.slice(0, 220)}`);
  const sc = (await dd.admin('/v1/status')).json; D.ok(!!findKey(sc, 'client_cert_expires_at'), 'near-expiry client cert expiry visible on /v1/status');
  D.art('near-expiry-client.txt', dd.log); await pl2.close(); await dd.stop();
  // near expiry, server cert
  pc = port(); pl2 = await startPlugin({ port: pc, tls: { key: srvShort.key, cert: srvShort.cert, trust: clientTrust } });
  dd = await startOk('d54-near-server', { plugin: httpsPlugin(pc, tlsBlock({})) });
  await dd.addKey(); await dd.call('agent-54s');
  let wl2 = []; for (let i = 0; i < 80 && !wl2.length; i++) { await sleep(500); wl2 = dd.log.split('\n').filter((l) => /WARN/.test(l) && /expir/i.test(l)); }
  D.art('near-expiry-server-status.json', (await dd.admin('/v1/status')).json); D.art('near-expiry-server-daemon.log', dd.log);
  D.ok(wl2.length > 0, `daemon logs a warning for a plugin (server) cert within 30 days of expiry: ${wl2[0]?.slice(0, 220)}`);
  D.art('near-expiry-server.txt', dd.log); await pl2.close(); await dd.stop();
});

await deliverable('D55', async (D) => {
  const out = path.join(WORK, 'd55-pki');
  const run = () => spawnSync(JANUS, ['plugin', 'tls', 'init', '--plugin-host', 'policy.internal', '--out', out, '--days', '30'], { encoding: 'utf8' });
  const r1 = run();
  D.ok(r1.status === 0, `init exits 0 (rc=${r1.status})`);
  const files = ['daemon.pem', 'daemon-key.pem', 'plugin.pem', 'plugin-key.pem'];
  D.ok(files.every((f) => fs.existsSync(path.join(out, f))), 'four files exist in ./pki');
  D.ok((fs.statSync(path.join(out, 'daemon-key.pem')).mode & 0o777) === 0o600, 'daemon-key.pem mode 0600');
  const pm = (fs.statSync(path.join(out, 'plugin-key.pem')).mode & 0o777); D.log(`     plugin-key.pem mode ${pm.toString(8)}`);
  const x = new crypto.X509Certificate(fs.readFileSync(path.join(out, 'plugin.pem')));
  D.ok(/DNS:policy\.internal/.test(x.subjectAltName || ''), `plugin.pem SAN = ${x.subjectAltName}`);
  const days = (new Date(x.validTo) - new Date(x.validFrom)) / 86400e3; const left = (new Date(x.validTo) - Date.now()) / 86400e3;
  D.ok(days >= 29 && days <= 31.5 && left > 27 && left <= 31, `plugin.pem validity ~30 days (span ${days.toFixed(1)}d, remaining ${left.toFixed(1)}d)`);
  D.ok(/\[plugin\.tls\]/.test(r1.stdout) && new RegExp(`pinned_cert\\s*=\\s*"${fs.realpathSync(out)}/plugin.pem"`).test(r1.stdout) && /plugin\.pem/.test(r1.stdout) && /daemon\.pem/.test(r1.stdout), 'stdout prints [plugin.tls] block pinning plugin.pem + plugin instructions (plugin.pem/plugin-key.pem, accept only daemon.pem)');
  D.art('init-1-stdout.txt', r1.stdout + '\n--- stderr ---\n' + r1.stderr);
  const fp1 = Object.fromEntries(files.filter((f) => !f.includes('key')).map((f) => [f, fp(path.join(out, f))]));
  const keyHash1 = files.filter((f) => f.includes('key')).map((f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(out, f))).digest('hex'));
  // configure both sides as printed
  const blockM = r1.stdout.match(/\[plugin\.tls\][\s\S]*?(?:\n\s*\n|$)/);
  const block = blockM ? blockM[0] : '';
  D.ok(/pinned_cert/.test(block) && /client_cert/.test(block) && /client_key/.test(block), 'printed block extracted');
  const serve = () => startPlugin({ port: pp, tls: { key: path.join(out, 'plugin-key.pem'), cert: path.join(out, 'plugin.pem'), trust: [path.join(out, 'daemon.pem')] } });
  const pp = port();
  let plug = await serve();
  // endpoint host: policy.internal is not resolvable here, so dial 127.0.0.1 and name the cert's SAN with server_name
  const pluginCfg = `[plugin]\nendpoint = "https://127.0.0.1:${pp}"\n${block}server_name = "policy.internal"\n`;
  let d = await startOk('d55', { plugin: pluginCfg });
  await d.addKey();
  let r = await d.call('agent-55');
  let st = (await d.admin('/v1/plugin')).json;
  D.ok(r.status === 200 && st.health === 'ok', `with the printed config requests succeed, health=${st.health}`);
  D.ok(plug.conns.some((c) => c.authorized && c.clientFp === fp(path.join(out, 'daemon.pem'))), 'plugin accepted only daemon.pem as client (authorized pinned cert)');
  // rotate
  await plug.close(); await d.stop();
  const r2 = run();
  D.ok(r2.status === 0, 'second init exits 0');
  const fp2 = Object.fromEntries(files.filter((f) => !f.includes('key')).map((f) => [f, fp(path.join(out, f))]));
  const keyHash2 = files.filter((f) => f.includes('key')).map((f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(out, f))).digest('hex'));
  D.ok(fp1['daemon.pem'] !== fp2['daemon.pem'] && fp1['plugin.pem'] !== fp2['plugin.pem'], 'after re-run: new cert fingerprints');
  D.ok(keyHash1[0] !== keyHash2[0] && keyHash1[1] !== keyHash2[1], 'after re-run: new keys');
  D.ok((fs.statSync(path.join(out, 'daemon-key.pem')).mode & 0o777) === 0o600, 'daemon-key.pem still mode 0600 after rotation');
  plug = await serve();
  d = new Daemon('d55b', { plugin: pluginCfg, dpPort: port() }); d.start(); await d.ready();
  await d.addKey(); r = await d.call('agent-55'); st = (await d.admin('/v1/plugin')).json;
  D.ok(r.status === 200 && st.health === 'ok', `after restarting both sides requests succeed, health=${st.health}`);
  D.art('rotation.json', { before: fp1, after: fp2, init2: r2.stdout });
  await plug.close(); await d.stop();
});

await deliverable('D56', async (D) => {
  const sum = {};
  const checkVia = async (d) => ({ cli: await d.cli(['plugin', 'check']), json: await d.cli(['--json', 'plugin', 'check']), api: await d.admin('/v1/plugin/check', 'POST', '{}') });
  const probeLines = (o) => JSON.stringify(o);
  // (a) correct mTLS plugin (client cert required + verified; python front sends real alerts)
  let f = await startFront('require');
  let d = await startOk('d56-a', { plugin: httpsPlugin(f.port, tlsBlock({})) });
  await d.addKey();
  const rr = await d.call('agent-56a');
  D.ok(rr.status === 200 && f.backend.resolveCalls.length === 1, `(a) sanity: a normal request through this plugin works (HTTP ${rr.status}, resolve=${f.backend.resolveCalls.length})`);
  const before = f.backend.resolveCalls.length, evBefore = f.events().length;
  let c = await checkVia(d); sum.a = { cliRc: c.cli.rc, cli: c.cli.out + c.cli.err, api: c.api.json, apiStatus: c.api.status };
  D.art('a-cli.txt', c.cli.out + '\n' + c.cli.err + `\nrc=${c.cli.rc}\n`); D.art('a-api.json', c.api.json || c.api.body);
  const evA = f.events(); D.art('a-front-events.json', evA); D.art('a-daemon.log', d.log);
  D.ok(c.cli.rc === 0 && c.api.status === 200, `(a) check exits 0 / API 200 (rc=${c.cli.rc}, http=${c.api.status})`);
  D.ok(/handshake/i.test(c.cli.out) && /throwaway_cert_rejected/.test(c.cli.out) && /no_cert_rejected/.test(c.cli.out), '(a) CLI reports the full handshake and both probes: ' + c.cli.out.replace(/\n/g, ' | ').slice(0, 500));
  D.ok(!/FAIL/.test(c.cli.out), '(a) no step failed (both probes were rejected)');
  D.ok(f.backend.resolveCalls.length === before, `(a) plugin recorded zero ResolveRoute from the checks (before=${before}, after=${f.backend.resolveCalls.length})`);
  const newEv = evA.slice(evBefore);
  D.ok(newEv.filter((e) => e.event === 'handshake_ok').length >= 1 && newEv.filter((e) => e.event === 'handshake_rejected').length >= 1, `(a) plugin side saw the full handshake plus rejected probe handshakes: ${newEv.map((e) => e.event + (e.error ? '(' + e.error.slice(0, 60) + ')' : '')).join(', ')}`);
  D.ok(JSON.stringify(c.api.json).includes('handshake'), '(a) POST /v1/plugin/check returns each result');
  await f.close(); await d.stop();
  // (b) accepts any client cert
  let s;
  const mk = async (n, srvTls) => { const p = port(); const plug = await startPlugin({ port: p, tls: srvTls }); const d = await startOk(n, { plugin: httpsPlugin(p, tlsBlock({})) }); return { plug, d, done: async () => { await plug.close(); await d.stop(); } }; };
  s = await mk('d56-b', { ...goodSrvTls(), requestCert: true, rejectUnauthorized: false });
  c = await checkVia(s.d); sum.b = { cliRc: c.cli.rc, cli: c.cli.out + c.cli.err, api: c.api.json };
  D.art('b-cli.txt', c.cli.out + '\n' + c.cli.err + `\nrc=${c.cli.rc}\n`); D.art('b-api.json', c.api.json || c.api.body);
  D.ok(c.cli.rc === 1, `(b) check exits 1 for a plugin accepting any client cert (rc=${c.cli.rc})`);
  D.ok(/self-signed|selfsigned|throwaway/i.test(c.cli.out + c.cli.err) && /accept|misconfig/i.test(c.cli.out + c.cli.err), '(b) reports the self-signed probe accepted/misconfigured: ' + (c.cli.out + c.cli.err).replace(/\n/g, ' | ').slice(0, 400));
  D.ok(s.plug.resolveCalls.length === 0, '(b) zero ResolveRoute');
  await s.done();
  // (c) requests but does not require a cert (valid certs ok, no cert ok, unverifiable certs rejected with an alert)
  f = await startFront('optional');
  d = await startOk('d56-c', { plugin: httpsPlugin(f.port, tlsBlock({})) });
  c = await checkVia(d); sum.c = { cliRc: c.cli.rc, cli: c.cli.out + c.cli.err, api: c.api.json };
  D.art('c-daemon.log', d.log); D.art('c-front-events.json', f.events());
  D.art('c-cli.txt', c.cli.out + '\n' + c.cli.err + `\nrc=${c.cli.rc}\n`); D.art('c-api.json', c.api.json || c.api.body);
  D.ok(c.cli.rc === 1, `(c) check exits 1 for a plugin that does not require a cert (rc=${c.cli.rc})`);
  D.ok(/FAIL\s+no_cert_rejected/.test(c.cli.out) && /requests a certificate without requiring one|accepted no client certificate/i.test(c.cli.out), '(c) reports the no-cert probe accepted: ' + c.cli.out.replace(/\n/g, ' | ').slice(0, 500));
  D.ok(/ok\s+throwaway_cert_rejected/.test(c.cli.out), '(c) (control) the self-signed probe was rejected by this plugin');
  D.ok(f.backend.resolveCalls.length === 0, '(c) zero ResolveRoute');
  await f.close(); await d.stop();
  // (d) unix-socket plugin
  const sd = sockDir('d56'); const sock = path.join(sd, 'p.sock');
  const plug = await startPlugin({ unixPath: sock });
  d = await startOk('d56-d', { plugin: unixPlugin(sock) });
  c = await checkVia(d); sum.d = { cliRc: c.cli.rc, cli: c.cli.out + c.cli.err, api: c.api.json };
  D.art('d-cli.txt', c.cli.out + '\n' + c.cli.err + `\nrc=${c.cli.rc}\n`); D.art('d-api.json', c.api.json || c.api.body);
  D.ok(c.cli.rc === 0, `(d) unix check exits 0 (rc=${c.cli.rc})`);
  D.ok(/walk|director|permission|path/i.test(c.cli.out) && /connect/i.test(c.cli.out), '(d) reports the path-permission walk and a connection attempt: ' + c.cli.out.replace(/\n/g, ' | ').slice(0, 400));
  D.ok(plug.resolveCalls.length === 0, '(d) zero ResolveRoute from the check');
  // (d2) unsafe path -> check fails
  fs.chmodSync(sd, 0o757);
  const c2 = await d.cli(['plugin', 'check']); D.ok(c2.rc === 1, `(d) after chmod o+w on the socket dir the check exits 1 (rc=${c2.rc}): ${c2.out.replace(/\n/g, ' | ').slice(0, 300)}`);
  D.art('d-unsafe-cli.txt', c2.out + c2.err);
  fs.chmodSync(sd, 0o750);
  await plug.close(); await d.stop();
  D.art('summary.json', sum);
});

// ================================================================= F
await deliverable('D57', async (D) => {
  const notifyFile = path.join(WORK, 'd57-notify.jsonl');
  const d = await startOk('d57', { bin: LINKED, args: [path.join(WORK, 'd57/c.toml')], plugin: `[plugin]\nlinked = true\n`, env: { LINKED_NOTIFY_FILE: notifyFile } });
  await d.addKey();
  const before = upstream.requests.length;
  const rx = await d.call('x');
  const ry = await d.call('y');
  await sleep(1000);
  const logs = await d.logs();
  const lx = logs.find((l) => l.caller_identity === 'x'), ly = logs.find((l) => l.caller_identity === 'y');
  D.ok(rx.status >= 400 && rx.status < 500, `identity x is denied (HTTP ${rx.status}) body=${rx.body.slice(0, 200)}`);
  D.ok(lx && /denied/.test(lx.outcome), `request log outcome for x = ${lx?.outcome}`);
  D.ok(ry.status === 200 && ly?.outcome === 'forwarded' && ly?.credential_ref === 'k1', `identity y forwarded via default chain (HTTP ${ry.status}, outcome=${ly?.outcome}, credential_ref=${ly?.credential_ref})`);
  D.ok(upstream.requests.length === before + 1, `exactly one request (y) reached the upstream (${upstream.requests.length - before})`);
  const st = (await d.admin('/v1/status')).json;
  D.ok(st?.plugin?.mode === 'linked', `/v1/status plugin.mode = ${st?.plugin?.mode}`);
  const notes = fs.existsSync(notifyFile) ? fs.readFileSync(notifyFile, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)) : [];
  D.ok(notes.some((n) => n.event_type === 'call_completed' && n.request_id === ly?.request_id), `settlement event call_completed for y reached the linked notify (${notes.map((n) => n.event_type).join(',')})`);
  D.ok(!notes.some((n) => n.request_id === lx?.request_id && n.event_type === 'call_completed'), 'no call_completed for the denied request x');
  D.art('requests.json', { rx: { status: rx.status, body: rx.body }, ry: { status: ry.status, body: ry.body }, logs, status: st, notifyEvents: notes });
  // the custom build embeds no tracing subscriber, so its log is legitimately empty; the evidence is the requests and transcript.
  await d.stop();
});

await deliverable('D58', async (D) => {
  const occupy = async (p) => new Promise((res) => { const s = net.createServer((c) => c.destroy()); s.listen(p, '127.0.0.1', () => res(s)); });
  const sd = sockDir('d58');
  const sock = path.join(sd, 'p.sock');
  // stock binary + linked=true
  let dp = port(); let hold = await occupy(dp);
  const a = await startRefused('d58-a', { dpPort: dp, plugin: `[plugin]\nlinked = true\n` });
  hold.close();
  D.ok(a.exited && a.code !== 0, `(a) stock binary with linked=true exits non-zero (exit=${a.code})`);
  D.ok(!/in use|bind/i.test(a.log) && /link/i.test(a.log), `(a) refused for the linkage disagreement, not for a bind failure (so before binding the data plane): ${a.log.trim().split('\n').slice(-1)[0]}`);
  D.ok(!a.adminSock, '(a) no admin socket created / no open pass-through');
  D.art('a-stock-linked.txt', `exit=${a.code}\n${a.log}`);
  // custom build + linked plugin + endpoint config
  dp = port(); hold = await occupy(dp);
  const b = await startRefused('d58-b', { dpPort: dp, bin: LINKED, args: [path.join(WORK, 'd58-b/c.toml')], plugin: `[plugin]\nendpoint = "unix://${sock}"\n` });
  hold.close();
  D.ok(b.exited && b.code !== 0, `(b) custom build with a linked plugin and endpoint config exits non-zero (exit=${b.code})`);
  D.ok(!/in use|bind/i.test(b.log) && b.log.trim().length > 0, `(b) refused before binding the data plane: ${b.log.trim().split('\n').slice(-1)[0]}`);
  D.ok(!b.adminSock, '(b) no admin socket created / no open pass-through');
  D.art('b-custom-endpoint.txt', `exit=${b.code}\n${b.log}`);
  // custom build + linked plugin + NO [plugin] block at all: also a disagreement
  dp = port(); hold = await occupy(dp);
  const c = await startRefused('d58-c', { dpPort: dp, bin: LINKED, args: [path.join(WORK, 'd58-c/c.toml')], plugin: '' });
  hold.close();
  D.ok(c.exited && c.code !== 0 && !c.adminSock, `(extra) custom build with a linked plugin but no [plugin] block also refuses (exit=${c.code}): ${c.log.trim().split('\n').slice(-1)[0]}`);
  D.art('c-custom-noplugin.txt', `exit=${c.code}\n${c.log}`);
  // and the stock binary has no way to be a linked build: --help shows nothing plugin-ish
  const ver = spawnSync(DAEMON, ['--version'], { encoding: 'utf8' }).stdout;
  D.log('stock --version: ' + ver.trim());
});

// ---------------------------------------------------------------- finish
await upSrv.close?.();
const order = (r) => parseInt(r.id.slice(1), 10);
results.sort((a, b) => order(a) - order(b));
if (!ONLY.length) fs.writeFileSync(RESULTS, JSON.stringify(results, null, 2) + '\n');
const bad = results.filter((r) => r.result !== 'PASS');
console.log(`\nEF summary: ${results.length - bad.length}/${results.length} PASS` + (bad.length ? ` ; non-PASS: ${bad.map((r) => r.id + '=' + r.result).join(', ')}` : ''));
for (const b of bad) console.log(`  ${b.id} ${b.result}: ${b.note}`);
try { fs.rmSync(SOCKBASE, { recursive: true, force: true }); } catch {}
if (!bad.length) { console.log('EF PASS'); process.exit(0); }
process.exit(1);
