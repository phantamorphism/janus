// Environment for group lm: mock upstream, mock TLS gRPC plugin, daemon lifecycle.
import http from 'node:http';
import http2 from 'node:http2';
import fs from 'node:fs';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

export const REPO = '/Users/michaelhancock/hancock_industries/apps/janus';
export const BIN = `${REPO}/target/release`;
export const W = '/tmp/qa-lm/run';
export const P = { dp: 26001, admin: 26002, ui: 26003, up: 26010, plugin: 26020 };

export function startUpstream(onSeen) {
  const seen = [];
  const srv = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const entry = { method: req.method, url: req.url, headers: req.headers };
      seen.push(entry); onSeen?.(entry);
      if (req.url.includes('/boom')) { res.writeHead(500, { 'content-type': 'application/json' }); return res.end('{"error":"boom"}'); }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ok: true, path: req.url }));
    });
  });
  return new Promise((r) => srv.listen(P.up, '127.0.0.1', () => r({ srv, seen })));
}

function varint(buf, i) { let v = 0, s = 0; for (;;) { const b = buf[i++]; v |= (b & 0x7f) << s; if (!(b & 0x80)) break; s += 7; } return [v, i]; }
function callerIdentity(msg) {
  let i = 0;
  while (i < msg.length) {
    let tag; [tag, i] = varint(msg, i);
    const f = tag >> 3, wt = tag & 7;
    if (wt === 2) { let l; [l, i] = varint(msg, i); if (f === 3) return msg.subarray(i, i + l).toString(); i += l; }
    else if (wt === 0) { [, i] = varint(msg, i); } else break;
  }
  return '';
}
// Minimal gRPC-over-h2c backend behind a python TLS 1.3 mTLS front (tls-front.py). identity "deny-*" is denied.
export function startPlugin(tlsDir, mode = 'required') {
  const srv = http2.createServer();
  srv.on('session', (s) => s.on('error', () => {}));
  srv.on('stream', (stream, headers) => {
    const chunks = [];
    stream.on('error', () => {});
    stream.on('data', (c) => chunks.push(c));
    stream.on('end', () => {
      const body = Buffer.concat(chunks);
      let msg = Buffer.alloc(0);
      if (headers[':path'].endsWith('/ResolveRoute')) {
        const id = callerIdentity(body.subarray(5));
        if (id.startsWith('deny-')) { const s = Buffer.from('policy says no'); msg = Buffer.concat([Buffer.from([0x1a, s.length]), s]); }
        else msg = Buffer.from([0x10, 0x01]);
      }
      const frame = Buffer.concat([Buffer.from([0]), Buffer.from([(msg.length >> 24) & 255, (msg.length >> 16) & 255, (msg.length >> 8) & 255, msg.length & 255]), msg]);
      stream.respond({ ':status': 200, 'content-type': 'application/grpc' }, { waitForTrailers: true });
      stream.on('wantTrailers', () => stream.sendTrailers({ 'grpc-status': '0' }));
      stream.end(frame);
    });
  });
  return new Promise((r) => srv.listen(P.plugin + 1, '127.0.0.1', () => {
    const front = spawn('python3', ['-I', `${REPO}/evidence/acceptance/tls-front.py`, String(P.plugin), String(P.plugin + 1), tlsDir, mode], { stdio: 'ignore' });
    setTimeout(() => r({ close() { front.kill(); srv.close(); } }), 500);
  }));
}

export function run(cmd, args, { env = {}, input, timeout = 20000 } = {}) {
  const r = spawnSync(cmd, args, { env: { PATH: process.env.PATH, ...env }, input, timeout, encoding: 'utf8' });
  return { code: r.status, stdout: r.stdout ?? '', stderr: r.stderr ?? '', signal: r.signal };
}
export const janus = (args, o = {}) => run(`${BIN}/janus`, args, { ...o, env: { JANUS_DATA_DIR: `${W}/data`, ...(o.env || {}) } });

export function writeConfig(extra = '') {
  fs.mkdirSync(W, { recursive: true });
  const cfg = `data_dir = "${W}/data"
[data_plane]
bind = "127.0.0.1:${P.dp}"
[admin]
bind = "127.0.0.1:${P.admin}"
[ui]
bind = "127.0.0.1:${P.ui}"
assets_dir = "${REPO}/ui/dist"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }
[plugin]
endpoint = "https://localhost:${P.plugin}"
[plugin.tls]
pinned_cert = "${W}/tls/plugin.pem"
client_cert = "${W}/tls/daemon.pem"
client_key = "${W}/tls/daemon-key.pem"
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:${P.up}"
${extra}`;
  fs.writeFileSync(`${W}/config.toml`, cfg);
  return `${W}/config.toml`;
}

export async function startDaemon(cfg) {
  const log = fs.openSync(`${W}/daemon.log`, 'a');
  const p = spawn(`${BIN}/janus-daemon`, ['--config', cfg], { stdio: ['ignore', log, log], env: { PATH: process.env.PATH, RUST_LOG: 'info' } });
  for (let i = 0; i < 60; i++) {
    await new Promise((r) => setTimeout(r, 250));
    if (fs.existsSync(`${W}/data/admin.sock`) && /daemon started/.test(fs.readFileSync(`${W}/daemon.log`, 'utf8'))) return p;
    if (p.exitCode !== null) throw new Error('daemon exited: ' + fs.readFileSync(`${W}/daemon.log`, 'utf8'));
  }
  throw new Error('daemon did not start');
}

// ---- worker wrappers (main thread) ----
import { Worker } from 'node:worker_threads';
function spawnWorker(data, onMsg) {
  const w = new Worker(new URL('./lm-worker.mjs', import.meta.url), { workerData: data });
  return new Promise((resolve, reject) => {
    w.on('error', reject);
    w.on('message', (m) => { if (m.ready) resolve(w); else onMsg?.(m); });
  });
}
export async function upstreamW() {
  const seen = [];
  const w = await spawnWorker({ role: 'upstream' }, (m) => m.seen && seen.push(m.seen));
  return { seen, close: () => w.postMessage('close') };
}
export async function proxyW(port, target) {
  const log = [], resp = [];
  const w = await spawnWorker({ role: 'proxy', port, target }, (m) => { if (m.log) log.push(m.log); if (m.resp) resp.push(m.resp); });
  return { log, resp, close: () => w.postMessage('close') };
}
export async function pluginW(tlsDir, mode = 'required') {
  let waiter;
  const w = await spawnWorker({ role: 'plugin', tlsDir, mode }, (m) => waiter?.(m));
  const call = (msg) => new Promise((r) => { waiter = r; w.postMessage(msg); });
  return { restart: (m) => call({ cmd: 'restart', mode: m }), close: () => call({ cmd: 'stop' }) };
}
