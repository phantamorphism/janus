// Shared helpers for the ijk acceptance script (mock upstream, gRPC plugin, daemon driver).
import http from 'node:http';
import https from 'node:https';
import http2 from 'node:http2';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const stripAnsi = (s) => s.replace(/\x1b\[[0-9;]*m/g, '');

export function req({ host = '127.0.0.1', port, socketPath, path: p = '/', method = 'GET', headers = {}, body, tls, ca, abortAfter, timeout = 20000 }) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const mod = tls ? https : http;
    const opts = { method, path: p, headers: { ...headers }, agent: false, timeout };
    if (socketPath) opts.socketPath = socketPath; else { opts.host = host; opts.port = port; }
    if (tls) { opts.ca = ca; opts.servername = 'localhost'; opts.rejectUnauthorized = !!ca; }
    if (body !== undefined && body !== null) {
      const b = typeof body === 'string' || Buffer.isBuffer(body) ? body : JSON.stringify(body);
      if (typeof body === 'object' && !Buffer.isBuffer(body) && !opts.headers['content-type']) opts.headers['content-type'] = 'application/json';
      opts.headers['content-length'] = Buffer.byteLength(b);
      var payload = b;
    }
    const r = mod.request(opts, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const raw = Buffer.concat(chunks).toString();
        let json; try { json = JSON.parse(raw); } catch { json = undefined; }
        resolve({ status: res.statusCode, headers: res.headers, body: raw, json, ms: Date.now() - t0 });
      });
      res.on('error', reject);
    });
    r.on('error', (e) => { if (abortAfter) resolve({ aborted: true, status: 0, body: '', headers: {} }); else reject(e); });
    r.on('timeout', () => r.destroy(new Error('timeout')));
    if (abortAfter) setTimeout(() => { r.destroy(); resolve({ aborted: true, status: 0, body: '', headers: {}, ms: abortAfter }); }, abortAfter);
    if (payload !== undefined) r.write(payload);
    r.end();
  });
}

export function portOpen(port, host = '127.0.0.1') {
  return new Promise((resolve) => {
    const s = net.connect({ port, host });
    s.on('connect', () => { s.destroy(); resolve(true); });
    s.on('error', () => resolve(false));
  });
}

// ---------------- mock upstream ----------------
export function mockUpstream(port, tag) {
  const seen = [];
  const server = http.createServer((rq, rs) => {
    const chunks = [];
    rq.on('data', (c) => chunks.push(c));
    rq.on('end', () => {
      const body = Buffer.concat(chunks).toString();
      seen.push({ tag, method: rq.method, url: rq.url, headers: rq.headers, body });
      const auth = String(rq.headers.authorization || '') + String(rq.headers['x-api-key'] || '');
      if (rq.url.startsWith('/hang')) { setTimeout(() => { try { rs.end('late'); } catch {} }, 4000); return; }
      rs.setHeader('content-type', 'application/json');
      if (auth.includes('BAD')) { rs.statusCode = 401; return rs.end('{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}'); }
      if (auth.includes('RATE')) { rs.statusCode = 429; rs.setHeader('retry-after', '120'); return rs.end('{"error":{"message":"rate limited"}}'); }
      rs.end('{"ok":true,"note":"DISTINCT-RESP-7731","usage":{"input_tokens":3,"output_tokens":4}}');
    });
  });
  return new Promise((resolve) => server.listen(port, '127.0.0.1', () => resolve({ seen, close: () => server.close() })));
}

// ---------------- gRPC plugin (hand-rolled over node http2) ----------------
function varint(buf, i) { let r = 0, s = 0, b; do { b = buf[i++]; r += (b & 0x7f) * 2 ** s; s += 7; } while (b & 0x80); return [r, i]; }
function fields(buf) {
  const out = []; let i = 0;
  while (i < buf.length) {
    let tag; [tag, i] = varint(buf, i); const n = tag >> 3, wt = tag & 7;
    if (wt === 0) { let v; [v, i] = varint(buf, i); out.push({ n, v }); }
    else if (wt === 2) { let l; [l, i] = varint(buf, i); out.push({ n, v: buf.subarray(i, i + l) }); i += l; }
    else break;
  }
  return out;
}
const ldelim = (n, s) => { const b = Buffer.from(s); return Buffer.concat([Buffer.from([(n << 3) | 2, b.length]), b]); };
const frame = (m) => { const f = Buffer.alloc(5 + m.length); f.writeUInt32BE(m.length, 1); m.copy(f, 5); return f; };

export function grpcPlugin({ listen, tlsOpts, behavior }) {
  const calls = { resolve: [], notify: [] };
  const handler = (stream, headers) => {
    const p = headers[':path'];
    let acc = Buffer.alloc(0);
    const msgs = [];
    const drain = () => { while (acc.length >= 5) { const l = acc.readUInt32BE(1); if (acc.length < 5 + l) break; msgs.push(acc.subarray(5, 5 + l)); acc = acc.subarray(5 + l); } };
    stream.on('data', (c) => { acc = Buffer.concat([acc, c]); drain(); if (p.endsWith('/Notify')) { while (msgs.length) { const f = fields(msgs.shift()); const g = (n) => (f.find((x) => x.n === n)?.v ?? Buffer.alloc(0)).toString(); calls.notify.push({ event_type: g(1), request_id: g(2), correlation_id: g(3), payload_json: g(4) }); } } });
    stream.on('error', () => {});
    stream.on('end', async () => {
      try {
      if (p.endsWith('/Notify')) {
        if (behavior.notifyBlock) return; // never answer
        stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true });
        stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': '0' }));
        stream.write(frame(Buffer.alloc(0))); stream.end(); return;
      }
      const f = fields(msgs[0] || Buffer.alloc(0));
      const g = (n) => (f.find((x) => x.n === n)?.v ?? Buffer.alloc(0)).toString();
      const rq = { request_id: g(1), upstream: g(2), caller_identity: g(3), correlation_id: g(4) };
      calls.resolve.push(rq);
      const r = behavior.resolve(rq);
      if (r.sleep) await sleep(r.sleep);
      if (r.rpcError) { stream.respond({ ':status': 200, 'content-type': 'application/grpc', 'grpc-status': '13', 'grpc-message': 'boom' }, { endStream: true }); return; }
      let m = Buffer.alloc(0);
      if (r.cred) m = ldelim(1, r.cred); else if (r.deny) m = ldelim(3, r.deny); else if (r.useDefault) m = Buffer.from([0x10, 1]);
      stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true });
      stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': '0' }));
      stream.write(frame(m)); stream.end();
      } catch {}
    });
  };
  const server = tlsOpts ? http2.createSecureServer(tlsOpts) : http2.createServer();
  server.on('stream', handler);
  server.on('error', () => {}); server.on('sessionError', () => {});
  return new Promise((resolve) => {
    if (typeof listen === 'string') { try { fs.unlinkSync(listen); } catch {} server.listen(listen, () => { try { fs.chmodSync(listen, 0o660); } catch {} resolve({ calls, close: () => server.close() }); }); }
    else server.listen(listen, '127.0.0.1', () => resolve({ calls, close: () => server.close() }));
  });
}

// ---------------- daemon driver ----------------
export class Daemon {
  constructor({ bin, name, root, toml, env }) {
    this.bin = bin; this.name = name; this.dir = path.join(root, name); this.data = path.join(this.dir, 'data');
    this.cfgPath = path.join(this.dir, 'config.toml'); this.logPath = path.join(this.dir, 'daemon.log');
    this.toml = toml.replaceAll('@DATA@', this.data); this.env = env || {};
  }
  writeCfg() { fs.mkdirSync(this.data, { recursive: true }); fs.writeFileSync(this.cfgPath, this.toml); }
  start() {
    this.writeCfg();
    this.logFd = fs.openSync(this.logPath, 'a');
    this.proc = spawn(this.bin + '/janus-daemon', ['--config', this.cfgPath], { stdio: ['ignore', this.logFd, this.logFd], env: { ...process.env, NO_COLOR: '1', ...this.env } });
    this.exited = new Promise((r) => this.proc.on('exit', (code, sig) => { this.exitCode = code; this.exitSig = sig; r({ code, sig }); }));
    return this;
  }
  async ready(ms = 8000) {
    const t0 = Date.now();
    while (Date.now() - t0 < ms) { if (this.exitCode !== undefined) return false; if (fs.existsSync(this.sock) && (await this.statusOk())) return true; await sleep(100); }
    return false;
  }
  get sock() { return path.join(this.data, 'admin.sock'); }
  async statusOk() { try { return (await this.admin('GET', '/v1/status')).status === 200; } catch { return false; } }
  admin(method, p, body, headers) { return req({ socketPath: this.sock, path: p, method, body, headers }); }
  log() { try { return stripAnsi(fs.readFileSync(this.logPath, 'utf8')); } catch { return ''; } }
  token() { return fs.readFileSync(path.join(this.data, 'admin-token'), 'utf8').trim(); }
  async stop() { if (this.proc && this.exitCode === undefined) { this.proc.kill('SIGTERM'); await Promise.race([this.exited, sleep(4000)]); if (this.exitCode === undefined) this.proc.kill('SIGKILL'); } }
  cli(args, extra = []) { return spawnSync(this.bin + '/janus', ['--config', this.cfgPath, ...extra, ...args], { encoding: 'utf8', env: { ...process.env, NO_COLOR: '1' } }); }
}
