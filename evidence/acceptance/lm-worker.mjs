// Worker-thread host for the mocks, so the main script's synchronous child-process calls never stall them.
import { parentPort, workerData } from 'node:worker_threads';
import http from 'node:http';
import * as E from './lm-env.mjs';

const { role } = workerData;
if (role === 'upstream') {
  const { srv } = await E.startUpstream((entry) => parentPort.postMessage({ seen: entry }));
  parentPort.on('message', (m) => { if (m === 'close') srv.close(); });
  parentPort.postMessage({ ready: true });
} else if (role === 'plugin') {
  let pl = await E.startPlugin(workerData.tlsDir, workerData.mode);
  parentPort.on('message', async (m) => {
    if (m.cmd === 'restart') { pl.close(); await new Promise((r) => setTimeout(r, 400)); pl = await E.startPlugin(workerData.tlsDir, m.mode); parentPort.postMessage({ restarted: m.mode }); }
    if (m.cmd === 'stop') { pl.close(); parentPort.postMessage({ stopped: true }); }
  });
  parentPort.postMessage({ ready: true });
} else if (role === 'proxy') {
  const { port, target } = workerData;
  const proxy = http.createServer((req, res) => {
    parentPort.postMessage({ log: { method: req.method, url: req.url, authorization: req.headers['authorization'] ?? null, cookie: req.headers['cookie'] ?? null, t: Date.now() } });
    const up = http.request({ host: '127.0.0.1', port: target, path: req.url, method: req.method, headers: req.headers }, (ur) => {
      parentPort.postMessage({ resp: { url: req.url, status: ur.statusCode, headers: ur.headers } });
      res.writeHead(ur.statusCode, ur.headers);
      ur.on('data', (c) => res.write(c)); ur.on('end', () => res.end()); ur.on('error', () => res.end());
    });
    up.on('error', () => { try { res.writeHead(502); res.end(); } catch {} });
    req.pipe(up);
    res.on('close', () => up.destroy());
  });
  proxy.listen(port, '127.0.0.1', () => parentPort.postMessage({ ready: true }));
  parentPort.on('message', (m) => { if (m === 'close') proxy.close(); });
}
