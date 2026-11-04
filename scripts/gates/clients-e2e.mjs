#!/usr/bin/env node
// Oracle: the real janus CLI drives a real janus-daemon over the admin socket and over token-gated TCP,
// and the daemon's UI listener serves the built UI bundle. Needs target/release and ui/dist.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const bin = path.resolve('target/release');
const dir = mkdtempSync(path.join(tmpdir(), 'janus-e2e-'));
const [dp, ad, ui] = [27101, 27102, 27103];
const cfg = path.join(dir, 'c.toml'); const data = path.join(dir, 'data');
writeFileSync(cfg, `data_dir = "${data}"
[data_plane]
bind = "127.0.0.1:${dp}"
[admin]
bind = "127.0.0.1:${ad}"
[ui]
bind = "127.0.0.1:${ui}"
assets_dir = "${path.resolve('ui/dist')}"
[secrets.k]
env = "E2E_SECRET"
[identity]
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "k"
namespace = "e2e"
identity_claim = "sub"
[[credential]]
ref = "or-main"
upstream = "openrouter"
env = "E2E_KEY"
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:1"
`);
const env = { ...process.env, E2E_SECRET: 'e2e-secret-value-0123456789abcdef', E2E_KEY: 'sk-or-e2e' };
const d = spawn(path.join(bin, 'janus-daemon'), ['--config', cfg], { env, stdio: "ignore" });
const fail = (m) => { console.error('FAIL: ' + m); d.kill(); rmSync(dir, { recursive: true, force: true }); process.exit(1); };
const cli = (...a) => spawnSync(path.join(bin, 'janus'), a, { env: { ...env, JANUS_ADMIN: `unix://${data}/admin.sock` }, encoding: 'utf8' });
for (let i = 0; i < 100 && !existsSync(path.join(data, 'admin.sock')); i++) await new Promise((r) => setTimeout(r, 100));
await new Promise((r) => setTimeout(r, 500));
const st = cli('status', '--json'); if (st.status !== 0) fail('status: ' + st.stderr);
if (!JSON.parse(st.stdout).upstreams) fail('status json shape');
const cl = cli('credential', 'list', '--json'); if (cl.status !== 0 || !cl.stdout.includes('or-main')) fail('credential list: ' + cl.stdout + cl.stderr);
if (cl.stdout.includes('sk-or-e2e')) fail('credential material leaked by the CLI');
const tokFile = path.join(data, 'admin-token');
const tcp = spawnSync(path.join(bin, 'janus'), ['--admin', `http://127.0.0.1:${ad}`, '--token-file', tokFile, 'status', '--json'], { encoding: 'utf8' });
if (tcp.status !== 0) fail('tcp status with token: ' + tcp.stderr);
const bad = spawnSync(path.join(bin, 'janus'), ['--admin', `http://127.0.0.1:${ad}`, 'status'], { encoding: 'utf8', env: { ...env, JANUS_TOKEN_FILE: '', JANUS_ADMIN_TOKEN: 'wrong' } });
if (bad.status === 0) fail('wrong token accepted over TCP');
const root = await fetch(`http://127.0.0.1:${ui}/`); const html = await root.text();
if (root.status !== 200 || !/<div id="root"/.test(html)) fail('ui root not served');
const asset = html.match(/src="(\/assets\/[^"]+\.js)"/)?.[1]; if (!asset) fail('no script asset in ui html');
if ((await fetch(`http://127.0.0.1:${ui}${asset}`)).status !== 200) fail('ui asset not served');
const tok = readFileSync(tokFile, 'utf8').trim();
if ((await fetch(`http://127.0.0.1:${ui}/v1/status`)).status !== 401) fail('ui api without token not 401');
if ((await fetch(`http://127.0.0.1:${ui}/v1/status`, { headers: { authorization: `Bearer ${tok}` } })).status !== 200) fail('ui api with token not 200');
d.kill('SIGTERM'); rmSync(dir, { recursive: true, force: true });
console.log('CLIENTS E2E OK');
