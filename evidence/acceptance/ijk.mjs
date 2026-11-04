#!/usr/bin/env node
// Black-box acceptance for DELIVERABLES.md sections I, J, K (D78-D96). Group "ijk", ports 25000-25999.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import https from 'node:https';
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { sleep, stripAnsi, req, portOpen, mockUpstream, grpcPlugin, Daemon } from './ijk-lib.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..');
const BIN = process.env.JANUS_BIN_DIR || (fs.existsSync(path.join(REPO, 'target/release/janus-daemon')) ? path.join(REPO, 'target/release') : '/tmp/qa-target-ijk/release');
const ART = path.join(REPO, 'evidence/artifacts/ijk');
const RES = path.join(REPO, 'evidence/results/ijk.json');
const ROOT = '/tmp/qa-ijk/run';
const PLUG_DIR = path.join(os.homedir(), '.janus-qa-ijk-plug');
const UI_ASSETS = path.join(REPO, 'ui/dist');
const OA = 25090, OR = 25091;
const ONLY = process.env.ONLY ? process.env.ONLY.split(',') : null;
const want = (id) => !ONLY || ONLY.includes(id);

fs.rmSync(ROOT, { recursive: true, force: true }); fs.mkdirSync(ROOT, { recursive: true });
fs.rmSync(ART, { recursive: true, force: true }); fs.mkdirSync(ART, { recursive: true });
fs.rmSync(PLUG_DIR, { recursive: true, force: true }); fs.mkdirSync(PLUG_DIR, { mode: 0o700 });

const results = [];
function D(id) {
  const o = { id, fails: [], oks: [], arts: [], blocks: [] };
  o.ok = (c, m) => { (c ? o.oks : o.fails).push(m); return !!c; };
  o.block = (m) => o.blocks.push(m);
  o.infos = []; o.info = (m) => o.infos.push(m);
  o.art = (name, content) => { const f = `${id}-${name}.txt`; fs.writeFileSync(path.join(ART, f), typeof content === 'string' ? content : JSON.stringify(content, null, 2)); o.arts.push(`evidence/artifacts/ijk/${f}`); };
  o.done = () => {
    const result = o.fails.length ? 'FAIL' : o.blocks.length ? 'BLOCKED' : 'PASS';
    const note = [...o.fails.map((m) => 'FAILED: ' + m), ...o.blocks.map((m) => 'BLOCKED: ' + m), ...o.oks.map((m) => 'ok: ' + m), ...o.infos.map((m) => 'info (not an assertion): ' + m)].join(' | ');
    if (!o.arts.length) o.art('summary', note);
    results.push({ id, result, artifacts: o.arts, note });
    console.log(`${id} ${result}${o.fails.length ? ' -- ' + o.fails.join('; ') : ''}${o.blocks.length ? ' -- ' + o.blocks.join('; ') : ''}`);
  };
  return o;
}

const baseCfg = ({ dp, orPort = OR, extra = '', retain = '' }) => `data_dir = "@DATA@"
[data_plane]
bind = "127.0.0.1:${dp}"
[[upstream]]
name = "openai"
path_prefix = "/openai"
base_url = "http://127.0.0.1:${OA}"
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:${orPort}"
[identity]
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }
${extra}
${retain}`;

const daemons = [];
async function mk(name, toml, { wait = true } = {}) {
  const d = new Daemon({ bin: BIN, name, root: ROOT, toml });
  daemons.push(d); d.start();
  if (wait) { const ok = await d.ready(); if (!ok) console.log(`!! daemon ${name} not ready:\n` + d.log().slice(-1500)); d.up = ok; }
  return d;
}
const dpReq = (port, p, tok, o = {}) => req({ port, path: p, method: o.method || 'GET', headers: { ...(tok ? { authorization: `Bearer ${tok}` } : {}), ...(o.headers || {}) }, body: o.body, abortAfter: o.abortAfter });
const mkId = async (d, identity, extra = {}) => { const r = await d.admin('POST', '/v1/identities', { identity, ...extra }); if (r.status !== 200 && r.status !== 201) throw new Error('mkId ' + r.status + r.body); return r.json; };
const addCred = async (d, body) => { const r = await d.admin('POST', '/v1/credentials', body); if (r.status >= 300) throw new Error('addCred ' + r.status + r.body); return r.json; };
const records = async (d, qs = '') => (await d.admin('GET', '/v1/requests' + qs)).json?.requests || [];
async function waitFor(fn, ms = 10000, step = 150) { const t0 = Date.now(); while (Date.now() - t0 < ms) { const v = await fn(); if (v) return v; await sleep(step); } return null; }
const recById = async (d, id) => waitFor(async () => { const r = await d.admin('GET', '/v1/requests/' + id); return r.status === 200 ? r.json : null; }, 8000);

async function main() {
  const mockOA = await mockUpstream(OA, 'openai');
  const mockOR = await mockUpstream(OR, 'openrouter');

  // ---------- plugin on a safe unix socket (for deny / credential cases) ----------
  const sockPath = path.join(PLUG_DIR, 'p.sock');
  const plugin = await grpcPlugin({
    listen: sockPath,
    behavior: {
      resolve: (rq) => {
        if (rq.caller_identity === 'denyme') return { deny: 'budget exhausted' };
        if (rq.caller_identity === 'ghost') return { cred: 'no-such-ref' };
        if (rq.caller_identity === 'usedisabled') return { cred: 'dis-ref' };
        return { useDefault: true };
      },
    },
  });

  // =====================================================================
  // main daemon: plugin, used for D78 / D79 / D80 / D92 (plugin naming disabled ref)
  // =====================================================================
  const main = await mk('main', baseCfg({ dp: 25001, orPort: 25099, extra: `[plugin]\nendpoint = "unix://${sockPath}"` }));
  // retention test daemon started early (runs in background)
  const ret = await mk('ret', baseCfg({ dp: 25003, extra: '', retain: '[request_log]\nretain = "1m"' }));
  const dis = await mk('dis', baseCfg({ dp: 25004, retain: '[request_log]\nenabled = false' }));

  await addCred(main, { upstream: 'openai', ref: 'oa-main', material: { api_key: 'sk-main-good' } });
  await addCred(main, { upstream: 'openrouter', ref: 'or-main', material: { api_key: 'sk-or-main' } });
  await addCred(main, { upstream: 'openai', ref: 'dis-ref', material: { api_key: 'sk-dis-ref' } });
  await main.admin('POST', '/v1/credentials/dis-ref/disable');

  // ---------------------------------------------------------------- D78 / D79
  if (want('D78') || want('D79')) {
    const d78 = D('D78'), d79 = D('D79');
    const tGood = await mkId(main, 'alice'), tDeny = await mkId(main, 'denyme'), tGhost = await mkId(main, 'ghost');
    const BODY = 'DISTINCTIVE-REQUEST-BODY-5521';
    const r1 = await dpReq(25001, '/openai/v1/chat', tGood.token, { method: 'POST', body: BODY, headers: { 'x-request-id': 'caller-chosen-id', 'content-type': 'text/plain' } });
    const r2 = await dpReq(25001, '/openai/v1/chat', 'not-a-registered-token', { method: 'POST', body: BODY, headers: { 'content-type': 'text/plain' } });
    const r3 = await dpReq(25001, '/openai/v1/chat', tDeny.token, { method: 'POST', body: BODY, headers: { 'content-type': 'text/plain' } });
    d78.ok(r1.status === 200 && r2.status === 401 && r3.status >= 400, `statuses forwarded=${r1.status} rejected=${r2.status} denied=${r3.status}`);
    await sleep(500);
    const recs = await records(main);
    const byOutcome = (o) => recs.find((x) => x.outcome === o);
    const shows = {};
    const required = ['request_id', 'received_at', 'duration_ms', 'upstream', 'route', 'method', 'path', 'binding', 'caller_identity', 'claimed_identity', 'correlation_id', 'outcome', 'credential_ref', 'status', 'usage', 'error'];
    for (const o of ['forwarded', 'rejected_identity', 'denied']) {
      const rec = byOutcome(o);
      if (!d78.ok(rec, `a '${o}' record exists`)) continue;
      const cli = main.cli(['log', 'show', rec.request_id, '--json']);
      d78.ok(cli.status === 0, `janus log show ${o} exit 0`);
      let parsed; try { parsed = JSON.parse(cli.stdout); } catch { parsed = null; }
      shows[o] = { stdout: cli.stdout, stderr: cli.stderr, human: main.cli(['log', 'show', rec.request_id]).stdout };
      d78.ok(parsed, `log show --json parses for ${o}`);
      const miss = required.filter((k) => !(parsed && k in parsed));
      d78.ok(miss.length === 0, `${o} record has all listed fields (missing: ${miss.join(',') || 'none'})`);
      d78.ok(!cli.stdout.includes('DISTINCTIVE') && !shows[o].human.includes('DISTINCTIVE'), `${o} record contains no request body text`);
      d78.ok(!cli.stdout.includes('DISTINCT-RESP'), `${o} record contains no response body text`);
    }
    const dbBytes = fs.readdirSync(main.data).filter((f) => f.startsWith('janus.db')).map((f) => fs.readFileSync(path.join(main.data, f))).map((b) => b.toString('latin1')).join('');
    d78.ok(!dbBytes.includes('DISTINCTIVE-REQUEST-BODY') && !dbBytes.includes('DISTINCT-RESP-7731'), 'janus.db (+wal) contains neither request nor response distinctive text');
    const fwd = byOutcome('forwarded'), rej = byOutcome('rejected_identity'), den = byOutcome('denied');
    d78.ok(fwd?.caller_identity === 'alice' && fwd?.status === 200 && fwd?.credential_ref && fwd?.upstream === 'openai' && fwd?.method === 'POST' && fwd?.path === '/openai/v1/chat', 'forwarded record content (identity, status, credential_ref, upstream, method, path)');
    d78.ok(rej && rej.caller_identity === '' && rej.status == null, 'rejected record: empty caller_identity, no upstream status');
    d78.ok(den && den.caller_identity === 'denyme', 'denied record carries caller_identity');
    d78.ok(fwd && fwd.request_id !== 'caller-chosen-id', 'request_id is Janus-assigned');
    d78.art('records', { forwarded: fwd, rejected: rej, denied: den, shows });
    d78.done();

    // ---- D79
    // plugin_failed: a daemon whose plugin endpoint has no listener
    const failSock = path.join(PLUG_DIR, 'absent.sock');
    const fail = await mk('fail', baseCfg({ dp: 25002, extra: `[plugin]\nendpoint = "unix://${failSock}"` }));
    await addCred(fail, { upstream: 'openai', ref: 'oa-f', material: { api_key: 'sk-f' } });
    const tF = await mkId(fail, 'bob');
    const rf = await dpReq(25002, '/openai/v1/chat', tF.token, { method: 'POST', body: 'x' });
    d79.ok(rf.status >= 400, `plugin-failed request is refused (status ${rf.status})`);
    // credential_unavailable / abandoned: plugin names a nonexistent ref
    const rg = await dpReq(25001, '/openai/v1/chat', tGhost.token, { method: 'POST', body: 'x' });
    d79.ok(rg.status >= 400, `unknown credential ref -> failure (status ${rg.status})`);
    // upstream_error: openrouter upstream on main points at a closed port
    const tOr = await mkId(main, 'carol');
    const ru = await dpReq(25001, '/openrouter/v1/chat', tOr.token, { method: 'POST', body: 'x' });
    d79.ok(ru.status >= 400, `upstream down -> failure (status ${ru.status})`);
    // abandoned: client aborts while the upstream hangs
    await dpReq(25001, '/openai/hang', tGood.token, { abortAfter: 400 });
    await sleep(5500);
    const allMain = [];
    let before = null;
    for (let i = 0; i < 20; i++) { const r = await main.admin('GET', '/v1/requests?limit=1000' + (before ? '&before=' + encodeURIComponent(before) : '')); allMain.push(...r.json.requests); before = r.json.next; if (!before) break; }
    const allFail = (await fail.admin('GET', '/v1/requests?limit=1000')).json.requests;
    const all = [...allMain, ...allFail];
    const vocab = ['forwarded', 'rejected_identity', 'denied', 'plugin_failed', 'credential_unavailable', 'upstream_error', 'abandoned'];
    d79.ok(all.every((x) => vocab.includes(x.outcome)), `every record (${all.length}) has an outcome in the 7-value vocabulary: ${[...new Set(all.map((x) => x.outcome))].join(',')}`);
    const ab = all.filter((x) => x.outcome === 'abandoned');
    d79.ok(ab.every((x) => x.stage), `all ${ab.length} abandoned records carry a stage`);
    const q = (d, o) => { const c = d.cli(['log', 'query', '--outcome', o, '--json']); try { return JSON.parse(c.stdout).requests; } catch { return []; } };
    const qf = q(main, 'forwarded'), qr = q(main, 'rejected_identity'), qd = q(main, 'denied'), qp = q(fail, 'plugin_failed');
    d79.ok(qf.length >= 1 && qf.every((x) => x.outcome === 'forwarded'), 'janus log query --outcome forwarded finds records');
    d79.ok(qr.length >= 1 && qr.every((x) => x.outcome === 'rejected_identity') && qr.every((x) => x.kind), `--outcome rejected_identity finds records with kind (${qr.map((x) => x.kind)})`);
    d79.ok(qd.length >= 1 && qd[0].reason === 'budget exhausted', `--outcome denied finds record with reason (${qd[0]?.reason})`);
    d79.ok(qp.length >= 1 && qp[0].class, `--outcome plugin_failed finds record with class (${JSON.stringify(qp[0] && { class: qp[0].class, outcome: qp[0].outcome })})`);
    d79.art('outcomes', { counts: all.reduce((a, x) => ((a[x.outcome] = (a[x.outcome] || 0) + 1), a), {}), samples: { forwarded: qf[0], rejected: qr[0], denied: qd[0], plugin_failed: qp[0], other: all.filter((x) => ['credential_unavailable', 'upstream_error', 'abandoned'].includes(x.outcome)) } });
    d79.done();
  }

  // ---------------------------------------------------------------- D80
  if (want('D80')) {
    const d = D('D80');
    const t = await mkId(main, 'dave');
    const base = await dpReq(25001, '/openai/v1/chat', t.token, { method: 'POST', body: 'baseline' });
    const dbp = path.join(main.data, 'janus.db');
    const holder = spawn('python3', ['-I', '-c', `import sqlite3,time,sys
c=sqlite3.connect(sys.argv[1],timeout=0,isolation_level=None)
c.execute('BEGIN IMMEDIATE')
print('locked',flush=True)
time.sleep(7)
c.execute('ROLLBACK')
print('released',flush=True)`, dbp], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = ''; holder.stdout.on('data', (b) => (out += b)); holder.stderr.on('data', (b) => (out += b));
    const locked = await waitFor(() => out.includes('locked'), 5000);
    d.ok(locked, 'external writer holds BEGIN IMMEDIATE on janus.db (' + out.trim() + ')');
    const times = [];
    const countDave = async () => (await records(main)).filter((x) => x.caller_identity === 'dave').length;
    const beforeLock = await countDave();
    d.ok(beforeLock === 1, `1 baseline record for dave before the lock (${beforeLock})`);
    for (let i = 0; i < 3; i++) { const r = await dpReq(25001, '/openai/v1/chat', t.token, { method: 'POST', body: 'locked-' + i }); times.push(r.ms); d.ok(r.status === 200, `request ${i} under locked DB returns 200`); }
    d.ok(Math.max(...times) < 1000 && Math.max(...times) < base.ms + 800, `latency under lock ${times.join(',')}ms (baseline ${base.ms}ms)`);
    const during = await countDave();
    d.ok(during === beforeLock, `while the DB is locked the 3 new records are NOT yet in the log (visible=${during}, before=${beforeLock}) - the writes are pending, yet all 3 responses already returned 200`);
    await waitFor(() => out.includes('released'), 10000, 200);
    const after = await waitFor(async () => { const r = (await records(main)).filter((x) => x.caller_identity === 'dave'); return r.length >= 4 ? r : null; }, 15000, 300);
    d.ok(after, `all 4 records appear after the lock is released (${after ? after.length : 'not found'})`);
    d.art('timing', { baseline_ms: base.ms, locked_ms: times, out, records_visible_during_lock: during, records_after: after?.length });
    d.done();
  }

  // ---------------------------------------------------------------- D81 (ret polled at the end)
  const d81 = D('D81');
  if (want('D81')) {
    const st = (await main.admin('GET', '/v1/status')).json;
    d81.ok(st.request_log.enabled === true && st.request_log.retain_seconds === 2592000, `default status.request_log=${JSON.stringify(st.request_log)} (retain rendered "${st.request_log.retain}", 30 days = 2592000 s)`);
    d81.ok(st.request_log.rows > 0, 'default config writes records (rows=' + st.request_log.rows + ')');
    const cliStatus = main.cli(['status']).stdout; d81.art('default-status', { status: st.request_log, cli: cliStatus });
    d81.ok(/^30\s*d(ays?)?$/.test(String(st.request_log.retain)), `retention rendered as "${st.request_log.retain}" (spec text: 30d)`);
    // disabled
    const td = await mkId(dis, 'eve');
    await addCred(dis, { upstream: 'openai', ref: 'oa-d', material: { api_key: 'sk-d' } });
    for (let i = 0; i < 3; i++) await dpReq(25004, '/openai/v1/chat', td.token, { method: 'POST', body: 'x' });
    await dpReq(25004, '/openai/v1/chat', 'bad', { method: 'POST', body: 'x' });
    await sleep(800);
    const lr = await dis.admin('GET', '/v1/requests'); const ds = (await dis.admin('GET', '/v1/status')).json;
    d81.ok(lr.status === 200 && lr.json.requests.length === 0 && lr.json.disabled === true, `disabled: GET /v1/requests -> ${lr.status} ${lr.body.slice(0, 120)}`);
    d81.ok(ds.request_log.enabled === false && ds.request_log.rows === 0, `disabled: status.request_log=${JSON.stringify(ds.request_log)}`);
    d81.art('disabled', { list: lr.body, status: ds.request_log, cli: dis.cli(['log', 'query']).stdout });
    // retain=1m: traffic now, then wait
    const tr = await mkId(ret, 'frank'); await addCred(ret, { upstream: 'openai', ref: 'oa-r', material: { api_key: 'sk-r' } });
    for (let i = 0; i < 3; i++) await dpReq(25003, '/openai/v1/chat', tr.token, { method: 'POST', body: 'x' });
    await sleep(500);
    d81.ret0 = Date.now(); d81.retRows0 = (await records(ret)).length;
    d81.ok(d81.retRows0 >= 3, `retain=1m daemon wrote ${d81.retRows0} rows initially`);
    d81.ok((await ret.admin('GET', '/v1/status')).json.request_log.retain_seconds === 60, 'retain=1m reflected in status (retain_seconds=60)');
  }

  // =====================================================================
  // adm daemon: no plugin, admin tcp + ui. Used for D82 D83 D85 D87 D88 D89 D90..D96
  // =====================================================================
  const adm = await mk('adm', baseCfg({ dp: 25010, extra: `[admin]\nbind = "127.0.0.1:25011"\n[ui]\nbind = "127.0.0.1:25012"\nassets_dir = "${UI_ASSETS}"` }));
  const TOK = adm.token();
  const atcp = (m, p, body, tok = TOK, headers = {}) => req({ port: 25011, path: p, method: m, body, headers: { ...(tok ? { authorization: `Bearer ${tok}` } : {}), ...headers } });

  // ---------------------------------------------------------------- D82
  if (want('D82')) {
    const d = D('D82');
    const st = fs.statSync(adm.sock);
    d.ok(st.isSocket() && (st.mode & 0o777) === 0o600, `admin.sock mode ${(st.mode & 0o777).toString(8)} (socket=${st.isSocket()})`);
    d.ok(st.uid === process.getuid(), 'socket owned by daemon user');
    const r = await adm.admin('GET', '/v1/status');
    d.ok(r.status === 200 && r.json.version, `GET /v1/status over socket with no token -> ${r.status}`);
    const curl = spawnSync('curl', ['-s', '-o', '/dev/null', '-w', '%{http_code}', '--unix-socket', adm.sock, 'http://x/v1/status'], { encoding: 'utf8' });
    d.ok(curl.stdout.trim() === '200', `curl --unix-socket -> ${curl.stdout}`);
    const sudo = spawnSync('sudo', ['-n', '-u', 'nobody', 'test', '-r', adm.sock], { encoding: 'utf8' });
    const sock2 = spawnSync('ls', ['-ld', adm.sock, adm.data], { encoding: 'utf8' }).stdout;
    d.art('socket', { stat: sock2, curl: curl.stdout, other_user_probe: { status: sudo.status, stderr: sudo.stderr } });
    if (!/password|not allowed|sudoers/i.test(sudo.stderr)) d.ok(sudo.status !== 0, 'another OS user (nobody) cannot read the socket');
    else d.info('another-user probe not possible (sudo needs a password); mode 0600 + owner-only enforced by kernel (stat evidence)');
    d.done();
  }

  // ---------------------------------------------------------------- D83
  if (want('D83')) {
    const d = D('D83'); const trace = [];
    for (const p of ['/v1/status', '/v1/credentials', '/v1/requests/stream', '/v1/plugin', '/v1/identities']) {
      for (const tok of [null, TOK]) {
        const r = await req({ port: 25010, path: p, headers: tok ? { authorization: `Bearer ${tok}` } : {}, timeout: 5000 });
        trace.push(`${p} token=${!!tok} -> ${r.status} ${r.body.slice(0, 80)}`);
        d.ok(r.status === 404, `data plane ${p} ${tok ? 'with' : 'without'} admin token -> ${r.status}`);
        d.ok(!/listeners|"credentials"|admin_socket|"requests"/.test(r.body), `no admin data in ${p} body`);
      }
    }
    d.art('trace', trace.join('\n')); d.done();
  }

  // ---------------------------------------------------------------- D85 (admin TCP loopback part)
  if (want('D85')) {
    const d = D('D85'); const trace = [];
    const shapeOk = (r) => r.json?.error && typeof r.json.error.code === 'string' && typeof r.json.error.message === 'string';
    const a = await atcp('GET', '/v1/status', null, null); const b = await atcp('GET', '/v1/status', null, 'wrong-token'); const c = await atcp('GET', '/v1/status');
    trace.push(`no token -> ${a.status} ${a.body}`, `wrong -> ${b.status} ${b.body}`, `correct -> ${c.status}`);
    d.ok(a.status >= 400 && a.status < 500 && shapeOk(a), `no token refused ${a.status} with JSON error shape`);
    d.ok(b.status >= 400 && b.status < 500 && shapeOk(b), `wrong token refused ${b.status} with JSON error shape`);
    d.ok(c.status === 200, 'correct token -> 200');
    const tl = await req({ port: 25011, path: '/v1/status', headers: { authorization: 'Basic ' + TOK } }); d.ok(tl.status >= 400, 'non-Bearer scheme refused');
    // non-loopback variants
    const bad = await tryStart('d85-noallow', baseCfg({ dp: 25020, extra: '[admin]\nbind = "0.0.0.0:25021"' }), 25020);
    d.ok(bad.refused, `0.0.0.0 without allow_non_loopback refuses to start: exit=${bad.code} ${bad.msg}`);
    const bad2 = await tryStart('d85-a', baseCfg({ dp: 25020, extra: '[admin]\nbind = "0.0.0.0:25021"\nallow_non_loopback = true' }), 25020);
    d.ok(bad2.refused, `(a) allow_non_loopback without tls refuses to start: exit=${bad2.code} ${bad2.msg}`);
    // (b) tls
    const pk = mkSelfSigned(path.join(ROOT, 'tls-admin'));
    const b3 = await mk('d85-b', baseCfg({ dp: 25030, extra: `[admin]\nbind = "0.0.0.0:25031"\nallow_non_loopback = true\ntls = { cert = "${pk.cert}", key = "${pk.key}" }` }));
    d.ok(b3.up, '(b) tls non-loopback admin starts');
    const tk = b3.token();
    const hs = async (tok) => req({ port: 25031, path: '/v1/status', tls: true, ca: fs.readFileSync(pk.cert), headers: tok ? { authorization: `Bearer ${tok}` } : {} });
    const h1 = await hs(null).catch((e) => ({ status: 'err ' + e.message })), h2 = await hs(tk).catch((e) => ({ status: 'err ' + e.message })), h3 = await hs('wrong').catch((e) => ({ status: 'err ' + e.message }));
    const plain = await req({ port: 25031, path: '/v1/status', headers: { authorization: `Bearer ${tk}` }, timeout: 3000 }).then((r) => r.status).catch((e) => 'conn-error:' + e.code);
    trace.push(`(b) https no token -> ${h1.status}; wrong -> ${h3.status}; token -> ${h2.status}; plaintext http with token -> ${plain}`);
    d.ok(h2.status === 200 && h1.status === 401 && h3.status >= 400, `(b) HTTPS requires the token (none=${h1.status} wrong=${h3.status} ok=${h2.status})`);
    d.ok(plain !== 200, `(b) plaintext HTTP to the TLS listener does not serve (${plain})`);
    // (c) allow_plaintext
    const c3 = await mk('d85-c', baseCfg({ dp: 25032, extra: `[admin]\nbind = "0.0.0.0:25033"\nallow_non_loopback = true\nallow_plaintext = true` }));
    d.ok(c3.up, '(c) allow_plaintext non-loopback admin starts');
    const ck = c3.token();
    const p1 = await req({ port: 25033, path: '/v1/status' }), p2 = await req({ port: 25033, path: '/v1/status', headers: { authorization: `Bearer ${ck}` } });
    trace.push(`(c) plaintext no token -> ${p1.status}; with token -> ${p2.status}`);
    d.ok(p1.status === 401 && p2.status === 200, `(c) plaintext listener still requires the token (none=${p1.status} ok=${p2.status})`);
    d.art('trace', trace.join('\n') + '\n\nnote: constant-time comparison is not observable black-box');
    d.done();
  }

  // ---------------------------------------------------------------- D84
  if (want('D84')) {
    const d = D('D84'); const rows = [];
    const sec = path.join(ROOT, 'secret-none');
    const cases = [
      ['missing-secret', `[identity]\nmethod = "hmac"\nsource = { header = "authorization", scheme = "Bearer " }\nsecret_ref = "nosuch"\nnamespace = "n"\nidentity_claim = "subject"`],
      ['unreachable-jwks', `[identity]\nmethod = "jwt"\nsource = { header = "authorization", scheme = "Bearer " }\nissuer = "i"\naudience = "a"\nidentity_claim = "sub"\njwks_url = "http://127.0.0.1:25097/jwks"`],
      ['custom-unregistered', `[identity]\nmethod = "custom:missing"`],
      ['plugin-both', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nendpoint = "unix://${PLUG_DIR}/x.sock"\nlinked = true`],
      ['plugin-neither', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\ntimeout_ms = 100`],
      ['plugin-http-plain', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nendpoint = "http://127.0.0.1:25098"`],
      ['plugin-https-no-tls', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nendpoint = "https://localhost:25098"`],
      ['plugin-ftp', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nendpoint = "ftp://x"`],
      ['plugin-unsafe-socket-dir', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nendpoint = "unix:///tmp/qa-ijk/x.sock"`],
      ['plugin-linked-stock', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[plugin]\nlinked = true`],
      ['admin-nonloopback-no-flag', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[admin]\nbind = "0.0.0.0:25052"`],
      ['admin-nonloopback-no-tls', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[admin]\nbind = "0.0.0.0:25052"\nallow_non_loopback = true`],
      ['ui-nonloopback-no-tls', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n[ui]\nbind = "0.0.0.0:25053"\nassets_dir = "${UI_ASSETS}"`],
    ];
    for (const [name, ident] of cases) {
      const cfg = `data_dir = "@DATA@"\n[data_plane]\nbind = "127.0.0.1:25050"\n[[upstream]]\nname = "openai"\npath_prefix = "/openai"\nbase_url = "http://127.0.0.1:${OA}"\n` + ident + '\n';
      const r = await tryStart('d84-' + name, cfg, 25050);
      rows.push(`${name}: exit=${r.code} dataplane_listened=${r.listened} msg=${r.msg}`);
      d.ok(r.refused, `${name} refused to start (exit ${r.code}); data-plane port never listened: ${!r.listened}`);
      d.ok(r.msg.length > 0, `${name} printed a diagnostic`);
    }
    // sanity: a good config does start (so refusals are not a broken harness)
    const good = await mk('d84-good', `data_dir = "@DATA@"\n[data_plane]\nbind = "127.0.0.1:25050"\n[[upstream]]\nname = "openai"\npath_prefix = "/openai"\nbase_url = "http://127.0.0.1:${OA}"\n[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n`);
    d.ok(good.up && (await portOpen(25050)), 'control: a valid config starts and listens'); await good.stop();
    d.art('refusals', rows.join('\n')); d.done();
  }

  // ---------------------------------------------------------------- D86
  if (want('D86')) {
    const d = D('D86');
    const tok = await mk('tok', baseCfg({ dp: 25040, extra: '[admin]\nbind = "127.0.0.1:25041"' }));
    const f = path.join(tok.data, 'admin-token'); const st = fs.statSync(f);
    d.ok((st.mode & 0o777) === 0o600, `admin-token mode ${(st.mode & 0o777).toString(8)}`);
    const t1 = tok.token(); d.ok(t1.length >= 32, `opaque token length ${t1.length}`);
    const idt = await mkId(tok, 'gina'); await addCred(tok, { upstream: 'openai', ref: 'oa-t', material: { api_key: 'sk-t' } });
    const a = await dpReq(25040, '/openai/v1/chat', t1, { method: 'POST', body: 'x' });
    d.ok(a.status === 401, `admin token as caller identity on data plane -> ${a.status}`);
    const ok = await dpReq(25040, '/openai/v1/chat', idt.token, { method: 'POST', body: 'x' });
    d.ok(ok.status === 200, 'control: caller token works on data plane');
    const b = await req({ port: 25041, path: '/v1/status', headers: { authorization: `Bearer ${idt.token}` } });
    d.ok(b.status === 401 || b.status === 403, `caller identity as admin bearer -> ${b.status} ${b.body}`);
    const sock = await tok.admin('GET', '/v1/status'); d.ok(sock.status === 200, 'socket still works');
    await tok.stop(); await sleep(300);
    const tok2 = await mk('tok', baseCfg({ dp: 25040, extra: '[admin]\nbind = "127.0.0.1:25041"' }));
    d.ok(tok2.up && tok2.token() === t1, 'token reused across restart');
    d.ok((fs.statSync(f).mode & 0o777) === 0o600, 'mode still 0600 after restart');
    const again = await dpReq(25040, '/openai/v1/chat', idt.token, { method: 'POST', body: 'x' }); d.ok(again.status === 200, 'registry persisted across restart');
    d.art('token', `mode=${(st.mode & 0o777).toString(8)} len=${t1.length}\nadmin-token-as-caller: ${a.status} ${a.body}\ncaller-as-admin: ${b.status} ${b.body}\nsame-after-restart: ${tok2.token() === t1}`);
    d.done();
  }

  // ---------------------------------------------------------------- D87
  if (want('D87')) {
    const d = D('D87'); const tr = [];
    // (a) enabling the UI without a ui.bind
    const a = await tryStart('d87-a', baseCfg({ dp: 25070, extra: `[ui]\nassets_dir = "${UI_ASSETS}"` }), 25070, true);
    tr.push(`(a) [ui] with no bind: refused=${a.refused} exit=${a.code} msg=${a.msg}`);
    d.ok(a.refused && a.code !== 0 && !a.listened, `(a) a [ui] section with no bind is refused at startup (exit ${a.code}, ${a.msg.slice(0, 160)})`);
    // (b)
    const st = (await adm.admin('GET', '/v1/status')).json;
    d.ok(/127\.0\.0\.1:25012/.test(JSON.stringify(st.listeners.ui)), `(a/b) loopback bind is the address reported by /v1/status: listeners.ui=${JSON.stringify(st.listeners.ui)}`);
    const root = await req({ port: 25012, path: '/' }); tr.push(`UI root -> ${root.status} ${root.headers['content-type']} ${root.body.length}B`);
    d.ok(root.status === 200 && /<html|<!doctype/i.test(root.body), 'UI root serves HTML');
    const asset = (root.body.match(/(?:src|href)="(\/assets\/[^"]+)"/) || [])[1];
    if (asset) { const ar = await req({ port: 25012, path: asset }); d.ok(ar.status === 200 && ar.body.length > 100, `UI asset ${asset} -> ${ar.status}`); }
    const n = await req({ port: 25012, path: '/v1/status' }); const w = await req({ port: 25012, path: '/v1/status', headers: { authorization: 'Bearer wrong' } }); const y = await req({ port: 25012, path: '/v1/status', headers: { authorization: `Bearer ${TOK}` } });
    tr.push(`UI api no token ${n.status}; wrong ${w.status}; ok ${y.status}`);
    d.ok(n.status === 401 && w.status >= 400 && y.status === 200, `UI API requires token (none=${n.status} wrong=${w.status} ok=${y.status})`);
    const pre = await req({ port: 25012, path: '/v1/status', method: 'OPTIONS', headers: { origin: 'http://evil.example', 'access-control-request-method': 'GET', 'access-control-request-headers': 'authorization' } });
    const acah = Object.keys(pre.headers).filter((h) => h.startsWith('access-control-allow'));
    tr.push(`preflight -> ${pre.status} headers=${JSON.stringify(pre.headers)}`);
    d.ok(acah.length === 0, `preflight response carries no Access-Control-Allow-* headers (status ${pre.status}, found: ${acah})`);
    const lg = adm.log();
    d.ok(lg.includes('http://127.0.0.1:25012') && lg.includes(path.join(adm.data, 'admin-token')), 'daemon log contains the UI URL and the admin-token path');
    // (c)
    const c = await tryStart('d87-c', baseCfg({ dp: 25070, extra: `[ui]\nbind = "0.0.0.0:25072"\nassets_dir = "${UI_ASSETS}"` }), 25070);
    d.ok(c.refused, `(c) non-loopback ui.bind without TLS refuses to start: exit=${c.code} ${c.msg}`);
    // (d) UI disabled
    const nu = await mk('noui', baseCfg({ dp: 25073 }));
    d.ok(nu.up, '(d) daemon without [ui] starts');
    const nuUi = (await nu.admin('GET', '/v1/status')).json.listeners.ui;
    d.ok(nuUi == null, `(d) UI disabled: listeners.ui=${JSON.stringify(nuUi)}`);
    d.ok(!(await portOpen(25074)), '(d) UI port refuses connections when UI disabled');
    d.ok((await nu.admin('GET', '/v1/status')).status === 200, '(d) admin socket still answers when UI disabled');
    // screenshot (best effort)
    const shot = path.join(ART, 'D87-ui-root.png');
    const ab = (args) => spawnSync('agent-browser', args, { encoding: 'utf8', timeout: 60000 });
    const o1 = ab(['open', 'http://127.0.0.1:25012/']); const o2 = ab(['wait', '2000']); const o3 = ab(['screenshot', shot]);
    if (fs.existsSync(shot)) d.arts.push('evidence/artifacts/ijk/D87-ui-root.png'); tr.push(`agent-browser: ${o1.status} ${o3.status} ${(o3.stdout + o3.stderr).slice(0, 100)}`);
    ab(['close']);
    d.art('trace', tr.join('\n')); d.done();
  }

  // ---------------------------------------------------------------- D88
  if (want('D88')) {
    const d = D('D88');
    await addCred(adm, { upstream: 'openai', ref: 'aud-1', material: { api_key: 'sk-aud-1' } });
    await addCred(adm, { upstream: 'openai', ref: 'aud-2', material: { api_key: 'sk-aud-2' } });
    const e = await adm.admin('GET', '/v1/credentials/does-not-exist');
    d.ok(e.status === 404 && /^[a-z]+(_[a-z]+)*$/.test(e.json?.error?.code || '') && typeof e.json.error.message === 'string', `404 error ${e.status} ${e.body}`);
    const s = await adm.admin('POST', '/v1/credentials/aud-1/disable'); const t = await atcp('POST', '/v1/credentials/aud-2/disable');
    d.ok(s.status < 300 && t.status < 300, `disable over socket=${s.status}, over TCP=${t.status}`);
    await sleep(300);
    const lg = adm.log().split('\n');
    const m1 = lg.filter((l) => l.includes('admin mutation') && l.includes('credential.disable') && l.includes('aud-1') && l.includes('principal="socket"'));
    const m2 = lg.filter((l) => l.includes('admin mutation') && l.includes('credential.disable') && l.includes('aud-2') && l.includes('principal="admin_tcp"'));
    d.ok(m1.length === 1, 'daemon log: one mutation entry, principal=socket naming aud-1');
    d.ok(m2.length === 1, 'daemon log: one mutation entry, principal=admin_tcp naming aud-2');
    await adm.admin('POST', '/v1/credentials/aud-1/enable'); await adm.admin('POST', '/v1/credentials/aud-2/enable');
    d.art('audit', { error: e.body, log_lines: lg.filter((l) => l.includes('admin mutation')) }); d.done();
  }

  // ---------------------------------------------------------------- D90 + D91 + D92 (adm)
  if (want('D90') || want('D91') || want('D92')) {
    const d90 = D('D90'), d91 = D('D91'), d92 = D('D92'); const T = {}; const bodies = [];
    const SECRETS = ['sk-list-A', 'sk-list-B', 'sk-or-NEW-9981'];
    // D90
    await addCred(adm, { upstream: 'openai', ref: 'lst-a', label: 'A', material: { api_key: 'sk-list-A' } });
    await addCred(adm, { upstream: 'openai', ref: 'lst-b', label: 'B', material: { api_key: 'sk-list-B' } });
    await addCred(adm, { upstream: 'openrouter', ref: 'lst-or', label: 'OR', material: { api_key: 'sk-list-OR' } });
    const list = await adm.admin('GET', '/v1/credentials'); const lo = await adm.admin('GET', '/v1/credentials?upstream=openai'); const lr = await adm.admin('GET', '/v1/credentials?upstream=openrouter');
    const show = await adm.admin('GET', '/v1/credentials/lst-a'); const peek = await adm.admin('GET', '/v1/credentials/default?upstream=openai');
    bodies.push(list.body, lo.body, lr.body, show.body, peek.body);
    const fieldsReq = ['ref', 'upstream', 'kind', 'label', 'source', 'status', 'rate_limited_until', 'expires_at', 'last_used_at', 'request_count', 'error_count', 'last_error'];
    const items = list.json?.credentials || list.json;
    d90.ok(Array.isArray(items) && items.length >= 2, `list returns array (${Array.isArray(items) ? items.length : list.body.slice(0, 100)})`);
    d90.ok((items || []).every((c) => fieldsReq.every((k) => k in c)), 'every list item has all listed metadata fields');
    const loItems = lo.json?.credentials || lo.json;
    d90.ok(loItems.length >= 2 && loItems.every((c) => c.upstream === 'openai'), `?upstream=openai filter applied (${loItems.length})`);
    const lrItems = lr.json?.credentials || lr.json; d90.ok(lrItems.length >= 1 && lrItems.every((c) => c.upstream === 'openrouter') && lrItems.length < items.length, `?upstream=openrouter filter applied (${lrItems.length} of ${items.length})`);
    d90.ok(show.status === 200 && fieldsReq.every((k) => k in show.json) && show.json.ref === 'lst-a' && show.json.label === 'A', 'show returns metadata for lst-a');
    d90.ok(peek.status === 200 && fieldsReq.every((k) => k in peek.json), `default peek returns metadata ${peek.body.slice(0, 160)}`);
    const keysSeen = new Set(); for (const b of bodies) for (const k of Object.keys(JSON.parse(b) ).concat(...(JSON.parse(b).credentials || []).map((c) => Object.keys(c)))) keysSeen.add(k);
    d90.ok(![...keysSeen].some((k) => /material|secret|api_key|token|password/i.test(k)), `no material-ish keys in responses (keys: ${[...keysSeen].join(',')})`);
    d90.ok(!bodies.join('').includes('sk-list-') && !bodies.join('').includes('sk-list-OR'), 'key bytes absent from every response');
    // default peek == credential used by following request (two enabled, repeated)
    const tId = await mkId(adm, 'hank'); const peeks = [];
    for (let i = 0; i < 4; i++) { const p = (await adm.admin('GET', '/v1/credentials/default?upstream=openai')).json.ref; const prevId = (await records(adm, '?limit=1'))[0]?.request_id; const rr = await dpReq(25010, '/openai/v1/chat', tId.token, { method: 'POST', body: 'x' }); const rec = await waitFor(async () => { const r0 = (await records(adm, '?limit=1'))[0]; return r0 && r0.request_id !== prevId && r0; }, 3000); peeks.push({ peek: p, used: rec.credential_ref, status: rr.status }); }
    d90.ok(peeks.every((x) => x.peek === x.used && x.status === 200), `default peek equals credential used by next request: ${JSON.stringify(peeks.map((x) => [x.peek, x.used]))}`);
    d90.art('metadata', { list: list.json, show: show.json, peek: peek.json, filtered_openrouter: lr.json, peeks });
    d90.done();
    await adm.admin('DELETE', '/v1/credentials/lst-or');

    // D91
    const before91 = mockOR.seen.length;
    const created = await adm.admin('POST', '/v1/credentials', { upstream: 'openrouter', label: 'generated', material: { api_key: 'sk-or-NEW-9981' } });
    d91.ok(created.status === 200 || created.status === 201, `POST status ${created.status}`);
    const cref = created.json?.ref; d91.ok(typeof cref === 'string' && cref.length > 0, `generated ref ${cref}`);
    d91.ok(!created.body.includes('sk-or-NEW-9981') && !/material/.test(created.body), 'response has no material');
    const t91 = await mkId(adm, 'ivy'); const r91 = await dpReq(25010, '/openrouter/v1/chat', t91.token, { method: 'POST', body: 'x' });
    d91.ok(r91.status === 200, `immediate request to openrouter -> ${r91.status}`);
    const last = mockOR.seen[mockOR.seen.length - 1];
    d91.ok(mockOR.seen.length === before91 + 1 && /sk-or-NEW-9981/.test(last?.headers.authorization || ''), `upstream received the new key without restart (${last?.headers.authorization})`);
    let rec91; for (let i = 0; i < 30; i++) { rec91 = (await records(adm, '?upstream=openrouter&limit=1'))[0]; if (rec91?.credential_ref === cref) break; await new Promise((r) => setTimeout(r, 100)); } d91.ok(rec91?.credential_ref === cref, `log credential_ref=${rec91?.credential_ref}`);
    const slot = path.join(adm.data, 'credentials', cref); d91.ok(fs.existsSync(slot) && fs.statSync(slot).isDirectory(), `slot directory ${slot} exists: ${fs.readdirSync(slot)}`);
    d91.art('add', { response: created.json, slot_listing: fs.readdirSync(slot), upstream_saw: last?.headers.authorization && '<redacted-in-artifact: matched key>' });
    d91.done();

    // D92 -- on a fresh daemon for clean counters
    const r = await mk('d92', baseCfg({ dp: 25080, extra: '' }));
    const tk = await mkId(r, 'jack');
    await addCred(r, { upstream: 'openai', ref: 'cred-bad', material: { api_key: 'sk-BAD-1' } });
    const rb = await dpReq(25080, '/openai/v1/chat', tk.token, { method: 'POST', body: 'x' });
    const sh1 = (await r.admin('GET', '/v1/credentials/cred-bad')).json;
    d92.ok(sh1.status === 'invalid' && sh1.last_error, `upstream 401 -> credential invalid (${sh1.status}, ${sh1.last_error}) [caller saw ${rb.status}]`);
    const rot = await r.admin('PUT', '/v1/credentials/cred-bad/material', { material: { api_key: 'sk-GOOD-rotated' } });
    d92.ok(rot.status < 300, `PUT material -> ${rot.status} ${rot.body.slice(0, 120)}`);
    const sh2 = (await r.admin('GET', '/v1/credentials/cred-bad')).json; d92.ok(sh2.status === 'active', `after rotate status=${sh2.status}`);
    d92.ok(!rot.body.includes('GOOD-rotated'), 'rotate response has no material');
    const rg = await dpReq(25080, '/openai/v1/chat', tk.token, { method: 'POST', body: 'x' }); d92.ok(rg.status === 200, 'rotated credential is usable');
    // disable / enable and the default chain
    await addCred(r, { upstream: 'openai', ref: 'cred-other', material: { api_key: 'sk-other' } });
    const dsb = await r.admin('POST', '/v1/credentials/cred-bad/disable'); d92.ok(dsb.status < 300 && dsb.json?.status === 'disabled', `disable -> ${dsb.json?.status}`);
    const used = new Set(); for (let i = 0; i < 4; i++) { await dpReq(25080, '/openai/v1/chat', tk.token, { method: 'POST', body: 'x' }); }
    for (const x of (await records(r, '?limit=4'))) used.add(x.credential_ref);
    d92.ok([...used].join() === 'cred-other', `while disabled the default chain only uses cred-other (${[...used]})`);
    const pk = (await r.admin('GET', '/v1/credentials/default?upstream=openai')).json; d92.ok(pk.ref === 'cred-other', `default peek skips disabled (${pk.ref})`);
    const en = await r.admin('POST', '/v1/credentials/cred-bad/enable'); d92.ok(en.json?.status === 'active', `enable -> ${en.json?.status}`);
    // plugin names a disabled ref -> fails w/o forwarding and without fall-through (main daemon)
    const tU = await mkId(main, 'usedisabled'); const seenBefore = mockOA.seen.length;
    const cnt0 = (await main.admin('GET', '/v1/credentials/oa-main')).json.request_count;
    const ru = await dpReq(25001, '/openai/v1/chat', tU.token, { method: 'POST', body: 'x' });
    await sleep(500);
    d92.ok(ru.status >= 400, `plugin-named disabled ref -> failure ${ru.status}`);
    d92.ok(mockOA.seen.length === seenBefore, 'nothing was forwarded upstream');
    d92.ok((await main.admin('GET', '/v1/credentials/oa-main')).json.request_count === cnt0, 'no fall-through: other credential request_count unchanged');
    const urec = (await records(main, '?limit=1'))[0]; d92.ok(['credential_unavailable', 'abandoned'].includes(urec.outcome), `log outcome ${urec.outcome}`);
    // refresh (OAuth, against a local mock token endpoint via upstream token_url)
    const rfr = await r.admin('POST', '/v1/credentials/cred-other/refresh');
    d92.ok(rfr.status === 409 && rfr.json?.error?.code, `refresh on an API-key credential -> ${rfr.status} ${rfr.body.slice(0, 90)}`);
    const tokCalls = [];
    const tokSrv = http.createServer((q, s2) => { let b = ''; q.on('data', (c) => (b += c)); q.on('end', () => { tokCalls.push({ url: q.url, body: b }); s2.setHeader('content-type', 'application/json'); s2.end(JSON.stringify({ access_token: 'NEW-ACCESS-' + tokCalls.length, refresh_token: 'NEW-REFRESH-' + tokCalls.length, expires_in: 3600, token_type: 'Bearer' })); }); });
    await new Promise((res) => tokSrv.listen(25092, '127.0.0.1', res));
    const anSeen = []; const anSrv = http.createServer((q, s2) => { q.resume(); q.on('end', () => { anSeen.push(q.headers); s2.setHeader('content-type', 'application/json'); s2.end('{"ok":true}'); }); });
    await new Promise((res) => anSrv.listen(25093, '127.0.0.1', res));
    const oa = await mk('d92o', `data_dir = "@DATA@"\n[data_plane]\nbind = "127.0.0.1:25094"\n[[upstream]]\nname = "anthropic"\npath_prefix = "/anthropic"\nbase_url = "http://127.0.0.1:25093"\ntoken_url = "http://127.0.0.1:25092/token"\n[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }\n`);
    d92.ok(oa.up, 'daemon with upstream token_url starts');
    const expired = (ref, rt) => ({ upstream: 'anthropic', ref, material: { claudeAiOauth: { accessToken: 'OLD-ACCESS-' + ref, refreshToken: rt, expiresAt: 1000, scopes: ['user:inference'], subscriptionType: 'max' } } });
    const c1 = await addCred(oa, expired('oauth-1', 'RT-ONE'));
    d92.ok(c1.kind === 'oauth' && new Date(c1.expires_at) < new Date(), `expired OAuth credential added (expires_at ${c1.expires_at})`);
    const m1 = await oa.admin('POST', '/v1/credentials/oauth-1/refresh');
    d92.ok(m1.status === 200 && new Date(m1.json.expires_at) > new Date(), `POST .../refresh -> ${m1.status}, expires_at ${m1.json?.expires_at}`);
    d92.ok(tokCalls.length === 1 && tokCalls[0].body.includes('RT-ONE') && tokCalls[0].body.includes('refresh_token'), 'token endpoint received a refresh_token grant with the stored refresh token');
    d92.ok(!m1.body.includes('NEW-ACCESS') && !m1.body.includes('NEW-REFRESH') && !m1.body.includes('RT-ONE'), 'refresh response contains no token material');
    // automatic refresh before use
    await oa.admin('POST', '/v1/credentials', expired('oauth-1b', 'RT-TWO')); await oa.admin('POST', '/v1/credentials/oauth-1/disable');
    const tAn = await mkId(oa, 'oauth-user'); const nCalls = tokCalls.length;
    const ra = await dpReq(25094, '/anthropic/v1/messages', tAn.token, { method: 'POST', body: '{}' });
    d92.ok(ra.status === 200, `request using an expired credential succeeds (${ra.status})`);
    d92.ok(tokCalls.length === nCalls + 1 && tokCalls[nCalls].body.includes('RT-TWO'), 'automatic refresh happened before use');
    const up = anSeen[anSeen.length - 1] || {}; const upAuth = JSON.stringify(up);
    d92.ok(/NEW-ACCESS-\d/.test(upAuth) && !/OLD-ACCESS/.test(upAuth), 'upstream received the refreshed access token, not the expired one');
    const after = (await oa.admin('GET', '/v1/credentials/oauth-1b')).json; d92.ok(new Date(after.expires_at) > new Date(), `expires_at moved into the future (${after.expires_at})`);
    d92.ok(!ra.body.includes('NEW-ACCESS') && !JSON.stringify(ra.headers).includes('NEW-'), 'caller never sees the refreshed token');
    d92.art('refresh-oauth', { manual: { status: m1.status, body: m1.json }, auto: { status: ra.status, expires_at: after.expires_at }, token_endpoint_calls: tokCalls.length, apikey_refresh: rfr.body });
    tokSrv.close(); anSrv.close();
    // delete
    const slotDir = path.join(r.data, 'credentials', 'cred-other'); const existed = fs.existsSync(slotDir);
    const del = await r.admin('DELETE', '/v1/credentials/cred-other'); const g = await r.admin('GET', '/v1/credentials/cred-other');
    d92.ok(existed && del.status < 300, `DELETE -> ${del.status}`);
    d92.ok(g.status === 404 && g.json?.error?.code, `GET after delete -> ${g.status} ${g.body}`);
    d92.ok(!fs.existsSync(slotDir), 'slot directory removed');
    d92.art('lifecycle', { invalid: sh1, after_rotate: sh2, disabled: dsb.json, default_when_disabled: pk, plugin_disabled_ref: { status: ru.status, outcome: urec }, after_delete: g.body });
    d92.done();
  }

  // ---------------------------------------------------------------- D93 D94
  if (want('D93') || want('D94')) {
    const d93 = D('D93'), d94 = D('D94');
    await addCred(adm, { upstream: 'openai', ref: 'id-cred', material: { api_key: 'sk-id' } });
    const c = await adm.admin('POST', '/v1/identities', { identity: 'kate', claims: { tenant: 'acme' }, label: 'L' });
    const tok = c.json?.token; const b = Buffer.from(String(tok).replace(/-/g, '+').replace(/_/g, '/'), 'base64');
    d93.ok(c.status < 300 && /^[A-Za-z0-9_-]+$/.test(tok) && b.length === 32, `token base64url decodes to ${b.length} bytes (${String(tok).length} chars)`);
    d93.ok(c.json.id && c.json.identity === 'kate' && c.json.claims.tenant === 'acme' && c.json.label === 'L', 'create returns entry metadata');
    const sh = await adm.admin('GET', '/v1/identities/' + c.json.id); const ls = await adm.admin('GET', '/v1/identities'); const shTok = await adm.admin('GET', '/v1/identities/' + tok);
    d93.ok(sh.status === 200 && !sh.body.includes(tok) && !ls.body.includes(tok), 'show and list never contain the token');
    d93.ok(shTok.status === 404, `entries are addressed by id, not token (GET by token -> ${shTok.status})`);
    const db = path.join(adm.data, 'janus.db');
    const schema = spawnSync('sqlite3', [db, '.schema identities'], { encoding: 'utf8' });
    const dump = spawnSync('sqlite3', ['-readonly', db, 'select * from identities'], { encoding: 'utf8' });
    const sha = (await import('node:crypto')).createHash('sha256').update(tok).digest('hex');
    const dbAll = fs.readdirSync(adm.data).filter((f) => f.startsWith('janus.db')).map((f) => fs.readFileSync(path.join(adm.data, f)).toString('latin1')).join('');
    d93.ok(/token_hash/.test(schema.stdout) && !/\btoken\b\s+(TEXT|BLOB)/i.test(schema.stdout), 'identities table has token_hash column: ' + schema.stdout.replace(/\s+/g, ' ').slice(0, 200));
    d93.ok(!dbAll.includes(tok), 'plaintext token appears nowhere in janus.db / -wal');
    const hashOk = dump.stdout.toLowerCase().includes(sha) || dbAll.includes(Buffer.from(sha, 'hex').toString('latin1')) ;
    d93.ok(hashOk, 'the stored value is the SHA-256 of the token');
    const g = await adm.admin('POST', '/v1/identities', { identity: 'lena', token: 'guessable123' });
    const gTok = g.json?.token;
    d93.ok(g.status >= 400 || gTok !== 'guessable123', `create with supplied token: status ${g.status}, returned token ${g.status < 300 ? (gTok === 'guessable123' ? 'EQUALS supplied' : 'differs') : 'n/a'}`);
    const gv = await dpReq(25010, '/openai/v1/chat', 'guessable123', { method: 'POST', body: 'x' }); d93.ok(gv.status === 401, `guessable123 on data plane -> ${gv.status}`);
    const gd = spawnSync('sqlite3', ['-readonly', db, 'select count(*) from identities'], { encoding: 'utf8' }).stdout;
    d93.art('create', { create: { ...c.json, token: '<redacted: ' + b.length + ' bytes b64url>' }, schema: schema.stdout, rowcount: gd, supplied_token_attempt: { status: g.status, body: g.status < 300 ? '<created>' : g.body } });
    d93.done();

    // D94
    const a1 = await mkId(adm, 'multi'), a2 = await mkId(adm, 'multi'), ex = await mkId(adm, 'multi', { expires_in: '1s' }); const other = await mkId(adm, 'someone-else');
    d94.ok(ex.expires_at, 'expires_in "1s" accepted, expires_at set');
    const use = async (t) => (await dpReq(25010, '/openai/v1/chat', t, { method: 'POST', body: 'x' })).status;
    d94.ok((await use(a1.token)) === 200 && (await use(a2.token)) === 200, 'both tokens for one identity verify');
    const before = (await adm.admin('GET', '/v1/identities/' + a1.id)).json;
    const rv1 = await adm.admin('POST', `/v1/identities/${a1.id}/revoke`), rv2 = await adm.admin('POST', `/v1/identities/${a1.id}/revoke`);
    d94.ok(rv1.status < 300 && rv2.status < 300, `revoke twice -> ${rv1.status}, ${rv2.status}`);
    d94.ok((await use(a1.token)) === 401, 'revoked token rejected immediately'); d94.ok((await use(a2.token)) === 200, 'sibling token unaffected');
    await sleep(1500);
    const ids = (r) => (r.json?.identities || r.json || []).map((x) => x.id);
    const def = ids(await adm.admin('GET', '/v1/identities?identity=multi')); d94.ok(def.length === 1 && def[0] === a2.id, `default list hides revoked + expired (${def.length} shown)`);
    const wr = ids(await adm.admin('GET', '/v1/identities?identity=multi&include_revoked=true')); d94.ok(wr.includes(a1.id) && !wr.includes(ex.id), 'include_revoked=true shows revoked only');
    const we = ids(await adm.admin('GET', '/v1/identities?identity=multi&include_expired=true')); d94.ok(we.includes(ex.id) && !we.includes(a1.id), 'include_expired=true shows expired only');
    const both = ids(await adm.admin('GET', '/v1/identities?identity=multi&include_expired=true&include_revoked=true')); d94.ok(both.length === 3, 'both flags show all 3');
    const all = ids(await adm.admin('GET', '/v1/identities?include_revoked=true&include_expired=true')); d94.ok(all.includes(other.id) && all.length > both.length, 'unfiltered list includes other identities');
    d94.ok(!ids(await adm.admin('GET', '/v1/identities?identity=someone-else')).includes(a2.id) && ids(await adm.admin('GET', '/v1/identities?identity=someone-else')).includes(other.id), 'identity filter restricts the list');
    const shown = (await adm.admin('GET', '/v1/identities/' + a1.id)).json;
    d94.ok(['id', 'identity', 'claims', 'label', 'created_at', 'expires_at', 'revoked_at', 'last_seen_at'].every((k) => k in shown) && shown.revoked_at, 'show has listed fields and revoked_at set');
    d94.ok((await use(ex.token)) === 401, 'expired token rejected');
    const ls1 = (await adm.admin('GET', '/v1/identities/' + a2.id)).json.last_seen_at;
    await use(a2.token);
    const ls2 = await waitFor(async () => { const v = (await adm.admin('GET', '/v1/identities/' + a2.id)).json.last_seen_at; return v && v !== ls1 && v ? v : null; }, 20000, 500);
    d94.ok(ls2, `last_seen_at advances after use (${ls1} -> ${ls2})`);
    const del = await adm.admin('DELETE', '/v1/identities/' + a2.id); d94.ok(del.status < 300, `delete -> ${del.status}`);
    const gdel = await adm.admin('GET', '/v1/identities/' + a2.id); d94.ok(gdel.status === 404 && gdel.json?.error?.code, `deleted entry GET -> ${gdel.status}`);
    d94.ok((await use(a2.token)) === 401, 'deleted token rejected');
    d94.art('lifecycle', { before, revoke: [rv1.status, rv2.status], default_list: def.length, include_revoked: wr.length, include_expired: we.length, last_seen: [ls1, ls2], shown });
    d94.done();
  }

  // ---------------------------------------------------------------- D95
  if (want('D95')) {
    const d = D('D95');
    const idsT = {}; for (const n of ['p1', 'p2', 'p3']) idsT[n] = (await mkId(adm, n)).token;
    await addCred(adm, { upstream: 'openai', ref: 'pg-cred', material: { api_key: 'sk-pg' } });
    const total = 1250;
    const jobs = [];
    for (let i = 0; i < total; i++) {
      const k = i % 5;
      const [path_, tok] = k === 0 ? ['/openai/v1/a', idsT.p1] : k === 1 ? ['/openrouter/v1/a', idsT.p2] : k === 2 ? ['/openai/v1/a', idsT.p3] : k === 3 ? ['/openai/v1/a', 'bogus-token'] : ['/openai/v1/a', idsT.p1];
      jobs.push([path_, tok]);
    }
    let idx = 0; const worker = async () => { while (idx < jobs.length) { const [p, t] = jobs[idx++]; await dpReq(25010, p, t, { method: 'POST', body: 'x' }).catch(() => {}); } };
    await Promise.all(Array.from({ length: 6 }, worker));
    await sleep(2500);
    const st = (await adm.admin('GET', '/v1/status')).json.request_log;
    d.ok(st.rows > 1000, `log holds ${st.rows} rows (dropped=${st.dropped_records})`);
    const p1 = await adm.admin('GET', '/v1/requests'); d.ok(p1.json.requests.length === 100 && p1.json.next, `default page = ${p1.json.requests.length} items, next=${!!p1.json.next}`);
    const tn = (s) => { const m = s.match(/^(.*?)(?:\.(\d+))?Z$/); return Date.parse(m[1] + 'Z') * 1000 + Number(((m[2] || '0') + '000000').slice(0, 6)); };
    const nonInc = (arr) => arr.every((x, i) => i === 0 || tn(x.received_at) <= tn(arr[i - 1].received_at));
    d.ok(nonInc(p1.json.requests), 'newest first');
    const big = await adm.admin('GET', '/v1/requests?limit=5000'); d.ok(big.status === 200 && big.json.requests.length === 1000, `limit=5000 -> ${big.json.requests.length} items (status ${big.status})`);
    const seen = new Set(); let dup = 0, pages = 0, cur = null, ordered = true, prev = null, count = 0;
    do { const r = await adm.admin('GET', '/v1/requests?limit=400' + (cur ? '&before=' + encodeURIComponent(cur) : '')); pages++; for (const x of r.json.requests) { if (seen.has(x.request_id)) dup++; seen.add(x.request_id); if (prev && tn(x.received_at) > prev) ordered = false; prev = tn(x.received_at); count++; } cur = r.json.next; } while (cur && pages < 20);
    d.ok(dup === 0 && ordered && pages >= 3, `walked ${pages} pages / ${count} records: duplicates=${dup} ordered=${ordered}`);
    d.ok(count === st.rows || count === (await adm.admin('GET', '/v1/status')).json.request_log.rows, `walk covers all rows (${count} vs ${st.rows})`);
    const bigNext = big.json.next && (await adm.admin('GET', '/v1/requests?limit=1000&before=' + encodeURIComponent(big.json.next))).json.requests; d.ok(bigNext && bigNext.length > 0 && !bigNext.some((x) => big.json.requests.some((y) => y.request_id === x.request_id)), 'next page after limit=1000 is older and disjoint');
    const one = p1.json.requests[0]; const shw = await adm.admin('GET', '/v1/requests/' + one.request_id); d.ok(shw.status === 200 && shw.json.request_id === one.request_id, 'GET /v1/requests/{id}');
    const f = async (qs) => (await adm.admin('GET', '/v1/requests?limit=1000&' + qs)).json.requests;
    const fi = await f('identity=p1'); d.ok(fi.length > 0 && fi.every((x) => x.caller_identity === 'p1'), `identity filter (${fi.length})`);
    const fu = await f('upstream=openrouter'); d.ok(fu.length > 0 && fu.every((x) => x.upstream === 'openrouter'), `upstream filter (${fu.length})`);
    const fo = await f('outcome=rejected_identity'); d.ok(fo.length > 0 && fo.every((x) => x.outcome === 'rejected_identity'), `outcome filter (${fo.length})`);
    const cref = (big.json.requests.find((x) => x.credential_ref) || {}).credential_ref; const fc = await f('credential=' + cref); d.ok(fc.length > 0 && fc.every((x) => x.credential_ref === cref), `credential filter ${cref} (${fc.length})`);
    const all1k = big.json.requests; const mid = all1k[500].received_at;
    const fs_ = await f('since=' + encodeURIComponent(mid)); d.ok(fs_.length > 0 && fs_.every((x) => tn(x.received_at) >= tn(mid)), `since filter (${fs_.length})`);
    const fu2 = await f('until=' + encodeURIComponent(mid)); d.ok(fu2.length > 0 && fu2.every((x) => tn(x.received_at) < tn(mid)), `until filter (${fu2.length})`);
    const combo = await f('identity=p1&upstream=openai&outcome=forwarded'); d.ok(combo.length > 0 && combo.every((x) => x.caller_identity === 'p1' && x.upstream === 'openai' && x.outcome === 'forwarded'), 'combined filters');
    const cli = adm.cli(['log', 'query', '--limit', '3', '--json']); d.ok(cli.status === 0 && JSON.parse(cli.stdout).requests.length === 3, 'CLI log query --limit 3');
    d.art('paging', { rows: st, page1_len: p1.json.requests.length, big_len: big.json.requests.length, pages, count, dup, ordered, filters: { identity: fi.length, upstream: fu.length, outcome: fo.length, credential: fc.length, since: fs_.length, until: fu2.length } });
    d.done();
  }

  // ---------------------------------------------------------------- D96
  let d96p;
  if (want('D96')) {
    const d = D('D96');
    const tA = await mkId(adm, 'sse-a'); await addCred(adm, { upstream: 'openai', ref: 'sse-cred', material: { api_key: 'sk-sse' } });
    const qs = await req({ port: 25011, path: '/v1/requests/stream?token=' + TOK, timeout: 4000 }).catch((e) => ({ status: 'err', body: String(e) }));
    d.ok(qs.status === 401 || qs.status === 403, `?token= without header refused (${qs.status} ${qs.body.slice(0, 80)})`);
    const events = []; let ctype = null, st = null;
    const sreq = http.request({ host: '127.0.0.1', port: 25011, path: '/v1/requests/stream?upstream=openai', headers: { authorization: `Bearer ${TOK}`, accept: 'text/event-stream' } }, (res) => {
      ctype = res.headers['content-type']; st = res.statusCode; let buf = '';
      res.on('data', (c) => { buf += c; let i; while ((i = buf.indexOf('\n\n')) >= 0) { const blk = buf.slice(0, i); buf = buf.slice(i + 2); const ev = (blk.match(/^event:\s*(.*)$/m) || [])[1]; const data = (blk.match(/^data:\s*(.*)$/m) || [])[1]; events.push({ t: Date.now(), ev, data, raw: blk }); } });
    });
    sreq.on('error', () => {}); sreq.end();
    await sleep(800);
    d.ok(st === 200 && /^text\/event-stream/.test(ctype || ''), `stream status ${st}, content-type ${ctype}`);
    for (let i = 0; i < 3; i++) { await dpReq(25010, '/openai/v1/chat', tA.token, { method: 'POST', body: 'x' }); await dpReq(25010, '/openrouter/v1/chat', tA.token, { method: 'POST', body: 'x' }); }
    await sleep(1500);
    const reqs = events.filter((e) => e.ev === 'request');
    const parsed = reqs.map((e) => { try { return JSON.parse(e.data); } catch { return null; } });
    d.ok(parsed.length >= 3 && parsed.every((x) => x && x.upstream === 'openai'), `only openai 'request' events arrive (${parsed.length} events, upstreams ${[...new Set(parsed.map((x) => x?.upstream))]})`);
    d.ok(parsed.filter((x) => x.caller_identity === 'sse-a').length === 3, 'all 3 openai calls delivered, none of the 3 openrouter calls');
    d96p = { d, events, sreq, t0: Date.now() };
  }

  // ---------------------------------------------------------------- D89
  if (want('D89')) {
    const d = D('D89'); const st = (await adm.admin('GET', '/v1/status')).json; const pl = (await adm.admin('GET', '/v1/plugin')).json;
    const topKeys = ['version', 'build', 'started_at', 'listeners', 'upstreams', 'request_log', 'plugin'];
    d.ok(topKeys.every((k) => k in st), `status top-level keys present (${Object.keys(st)})`);
    d.ok(st.listeners && 'data_plane' in st.listeners && 'admin_socket' in st.listeners && 'admin_tcp' in st.listeners && 'ui' in st.listeners, 'listeners lists data_plane/admin_socket/admin_tcp/ui');
    d.ok(st.upstreams.length === 2 && st.upstreams.every((u) => u.binding), `each upstream lists binding method (${st.upstreams.map((u) => u.name + ':' + u.binding)})`);
    d.ok('rows' in st.request_log && 'retain' in st.request_log, 'request log size and retention present');
    const pf = ['mode', 'endpoint', 'health', 'last_error', 'last_ok_at'];
    d.ok(pf.every((k) => k in st.plugin), 'no-plugin: plugin object has mode/endpoint/health/last_error/last_ok_at');
    d.ok(st.plugin.mode === 'none' && st.plugin.health === 'unknown', `no plugin: mode=${st.plugin.mode} health=${st.plugin.health}`);
    d.ok(JSON.stringify(sortKeys(pl)) === JSON.stringify(sortKeys(st.plugin)), '/v1/plugin body equals status.plugin (no plugin)');
    // TLS plugin
    const pki = path.join(ROOT, 'pki'); const init = spawnSync(BIN + '/janus', ['plugin', 'tls', 'init', '--plugin-host', 'localhost', '--out', pki, '--days', '30'], { encoding: 'utf8' });
    d.ok(init.status === 0, 'janus plugin tls init ran');
    const tlsPlugin = await grpcPlugin({ listen: 25061, tlsOpts: { key: fs.readFileSync(pki + '/plugin-key.pem'), cert: fs.readFileSync(pki + '/plugin.pem'), ca: [fs.readFileSync(pki + '/daemon.pem')], requestCert: true, rejectUnauthorized: true, minVersion: 'TLSv1.3', allowHTTP1: false }, behavior: { resolve: () => ({ useDefault: true }) } });
    const td = await mk('tlsplug', baseCfg({ dp: 25060, extra: `[plugin]\nendpoint = "https://localhost:25061"\n[plugin.tls]\npinned_cert = "${pki}/plugin.pem"\nclient_cert = "${pki}/daemon.pem"\nclient_key = "${pki}/daemon-key.pem"` }));
    d.ok(td.up, 'daemon with TLS plugin starts');
    const s0 = (await td.admin('GET', '/v1/status')).json; d.ok(s0.plugin.mode === 'tls' && s0.plugin.health === 'unknown', `TLS plugin before any call: mode=${s0.plugin.mode} health=${s0.plugin.health}`);
    const tt = await mkId(td, 'tlsuser'); await addCred(td, { upstream: 'openai', ref: 'oa-tls', material: { api_key: 'sk-tls' } });
    const rr = await dpReq(25060, '/openai/v1/chat', tt.token, { method: 'POST', body: 'x' }); d.ok(rr.status === 200, `request via TLS plugin -> ${rr.status}`);
    await sleep(500);
    const s1 = (await td.admin('GET', '/v1/status')).json, p1 = (await td.admin('GET', '/v1/plugin')).json;
    const healthVocab = ['unknown', 'ok', 'unreachable', 'timeout', 'tls_error', 'no_client_auth', 'rejected_client_cert', 'error'];
    d.ok(['none', 'linked', 'unix', 'tls', 'plaintext'].includes(s1.plugin.mode) && healthVocab.includes(s1.plugin.health), `mode/health in vocabularies (${s1.plugin.mode}/${s1.plugin.health})`);
    d.ok(s1.plugin.health === 'ok' && s1.plugin.last_ok_at && s1.plugin.endpoint.replace(/\/$/, '') === 'https://localhost:25061', `after call: health=ok last_ok_at=${s1.plugin.last_ok_at} endpoint=${s1.plugin.endpoint}`);
    d.ok(s1.plugin.client_cert_expires_at && s1.plugin.server_cert_expires_at, `cert expiries present (${s1.plugin.client_cert_expires_at}, ${s1.plugin.server_cert_expires_at})`);
    d.ok(pf.every((k) => k in s1.plugin), 'TLS plugin object has all base fields');
    d.ok(JSON.stringify(sortKeys(p1)) === JSON.stringify(sortKeys(s1.plugin)), '/v1/plugin body equals status.plugin (TLS plugin)');
    const cliP = td.cli(['plugin', 'status', '--json']); d.ok(cliP.status === 0, 'janus plugin status');
    d.art('status', { none: st, none_plugin_endpoint: pl, tls: s1.plugin, tls_plugin_endpoint: p1, plugin_calls_seen: tlsPlugin.calls.resolve.length });
    d.done(); tlsPlugin.close();
  }

  // ---------------------------------------------------------------- finish D81 + D96 waits
  if (d96p) {
    const { d, events, sreq } = d96p;
    const ping = await waitFor(() => events.find((e) => e.ev === 'ping'), 70000, 500);
    d.ok(ping, `periodic ping event arrives (first at +${ping ? Math.round((ping.t - d96p.t0) / 1000) : '>70'}s after traffic)`);
    if (ping) { const ping2 = await waitFor(() => events.filter((e) => e.ev === 'ping').length >= 2, 70000, 500); d.ok(ping2, 'pings keep arriving periodically'); }
    sreq.destroy();
    d.art('stream', events.map((e) => e.raw).join('\n---\n')); d.done();
  }
  if (want('D81')) {
    const t0 = d81.ret0; let gone = false, rows = null;
    while (Date.now() - t0 < 6 * 60 * 1000) { rows = (await records(ret).catch(() => [null])).length; if (rows === 0) { gone = true; break; } await sleep(5000); }
    d81.ok(gone, `rows older than retain=1m were purged by the periodic job (${d81.retRows0} -> ${rows} after ${Math.round((Date.now() - t0) / 1000)}s)`);
    d81.art('retain', { start_rows: d81.retRows0, end_rows: rows, elapsed_s: Math.round((Date.now() - t0) / 1000) });
    d81.done();
  }
  fs.mkdirSync(path.dirname(RES), { recursive: true });
  const order = (r) => Number(r.id.slice(1));
  results.sort((a, b) => order(a) - order(b));
  fs.writeFileSync(RES, JSON.stringify(results, null, 2));
  mockOA.close(); mockOR.close(); plugin.close();
}

function sortKeys(o) { return Array.isArray(o) ? o.map(sortKeys) : o && typeof o === 'object' ? Object.fromEntries(Object.keys(o).sort().map((k) => [k, sortKeys(o[k])])) : o; }
function mkSelfSigned(dir) {
  fs.mkdirSync(dir, { recursive: true }); const cert = path.join(dir, 'c.pem'), key = path.join(dir, 'k.pem');
  const r = spawnSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-keyout', key, '-out', cert, '-days', '2', '-subj', '/CN=localhost', '-addext', 'subjectAltName=IP:127.0.0.1,DNS:localhost']);
  if (r.status !== 0) throw new Error('openssl failed ' + r.stderr);
  fs.chmodSync(key, 0o600); return { cert, key };
}
// start a daemon expecting it to refuse; reports exit code, whether the data-plane port ever listened
async function tryStart(name, toml, dpPort, expectRun = false) {
  const d = new Daemon({ bin: BIN, name, root: ROOT, toml }); d.start();
  let listened = false; const t0 = Date.now();
  while (d.exitCode === undefined && Date.now() - t0 < (expectRun ? 2500 : 6000)) { if (await portOpen(dpPort)) { listened = true; if (!expectRun) break; } await sleep(40); }
  const msg = stripAnsi(d.log()).trim();
  let uiAddr = null; if (d.exitCode === undefined) { try { uiAddr = JSON.stringify((await d.admin('GET', '/v1/status')).json.listeners.ui); } catch {} }
  const refused = d.exitCode !== undefined && d.exitCode !== 0 && !listened;
  await d.stop();
  return { refused, code: d.exitCode, signal: d.exitSig, listened, msg: msg.split('\n').slice(0, 3).join(' / ').slice(0, 300), uiAddr };
}

let code = 1;
try {
  await main();
  const bad = results.filter((r) => r.result !== 'PASS');
  console.log(`\nSummary: ${results.filter((r) => r.result === 'PASS').length} PASS, ${results.filter((r) => r.result === 'FAIL').length} FAIL, ${results.filter((r) => r.result === 'BLOCKED').length} BLOCKED`);
  if (!bad.length) { console.log('IJK PASS'); code = 0; } else console.log('IJK FAIL (' + bad.map((b) => b.id + ':' + b.result).join(', ') + ')');
} catch (e) {
  console.log('SCRIPT ERROR', e.stack);
  fs.writeFileSync(path.join(ART, 'script-error.txt'), e.stack);
  try { fs.writeFileSync(RES, JSON.stringify(results, null, 2)); } catch {}
} finally {
  for (const d of daemons) { try { await d.stop(); } catch {} }
  process.exit(code);
}
