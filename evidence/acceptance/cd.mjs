#!/usr/bin/env node
// Blind QA, group "cd": DELIVERABLES.md sections C (D27-D37) and D (D38-D46).
// Ports 22000-22999. Usage: node evidence/acceptance/cd.mjs   (needs /tmp/qa-cd/node_modules with @grpc/grpc-js + proto-loader)
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { spawn, spawnSync } from 'node:child_process';
import * as L from './cd-lib.mjs';
const { Daemon, startUpstream, startPlugin, mint, req, sleep, grpc, REPO, SCRATCH, BIN } = L;

const ART = `${REPO}/evidence/artifacts/cd`;
const RUN = `${REPO}/evidence/.run-cd`; // 0700 dir for unix plugin sockets (daemon refuses sockets under world-writable dirs)
fs.rmSync(ART, { recursive: true, force: true }); fs.mkdirSync(ART, { recursive: true });
fs.mkdirSync(RUN, { recursive: true, mode: 0o700 }); fs.chmodSync(RUN, 0o700);
for (const f of fs.readdirSync(RUN)) fs.rmSync(`${RUN}/${f}`, { force: true, recursive: true });

const SECRET = 'cd-shared-secret';
const R = {}; // id -> {checks:[], arts:Set, notes:[], blocked}
const cur = (id) => (R[id] ??= { checks: [], arts: new Set(), notes: [], blocked: null });
const art = (id, name, content) => { const f = `${id}-${name}.txt`; fs.writeFileSync(`${ART}/${f}`, typeof content === 'string' ? content : JSON.stringify(content, null, 2)); cur(id).arts.add(`evidence/artifacts/cd/${f}`); };
const check = (id, label, ok, detail = '') => { cur(id).checks.push({ label, ok: !!ok, detail }); console.log(`${ok ? 'ok  ' : 'FAIL'} ${id} ${label}${ok ? '' : ' :: ' + detail}`); return !!ok; };
const note = (id, s) => cur(id).notes.push(s);
const block = (id, why) => { cur(id).blocked = why; console.log(`BLOCKED ${id} ${why}`); };
const J = (x) => JSON.stringify(x);
const tok = (sub, extra = {}) => mint(SECRET, { subject: sub, ...extra });
const hdr = (sub, extra) => ({ authorization: 'Bearer ' + tok(sub, extra), 'content-type': 'application/json' });

const P = { dp: 22001, up: [22090, 22091, 22092] };
const toml = (o) => `data_dir = "@DIR@"
[data_plane]
bind = "127.0.0.1:${o.dp}"
[secrets]
caller-identity = { env = "QA_S" }
[identity]
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
namespace = "janus-data-plane"
identity_claim = "subject"
correlation_claim = "session"
${o.plugin || ''}
[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "http://127.0.0.1:${P.up[0]}"
[[upstream]]
name = "openai"
path_prefix = "/openai"
base_url = "http://127.0.0.1:${P.up[1]}"
${o.openaiExtra || ''}
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:${P.up[2]}"
`;
const ENV = { QA_S: SECRET };
const msg = (d, sub, up = 'anthropic', extra, session) => req(d, 'POST', `/${up}/v1/messages`, { headers: { ...hdr(sub, session ? { session } : {}), ...(extra || {}) }, body: '{"model":"m","messages":[]}' });
const settle = (st, rid) => st.notifies.filter((n) => n.ev.request_id === rid && (n.ev.event_type === 'call_completed' || n.ev.event_type === 'call_abandoned'));
const resolveOf = (st, sub) => st.resolves.filter((r) => r.req.caller_identity === sub);
const logOf = (d, rid) => d.logs().find((r) => r.request_id === rid);
const adminGet = (d, p) => new Promise((res) => { http.get({ socketPath: `${d.dir}/admin.sock`, path: p }, (r) => { const c = []; r.on('data', (x) => c.push(x)); r.on('end', () => res({ status: r.statusCode, body: Buffer.concat(c).toString() })); }).on('error', (e) => res({ status: 0, body: String(e) })); });
const pluginStatus = (d) => d.json(['plugin', 'status']);

const cleanups = [];
const mkUps = async () => { const u = []; for (const p of P.up) u.push(await startUpstream(p)); cleanups.push(() => u.forEach((x) => x.close())); return u; };

async function main() {
  const ups = await mkUps();
  const upCreds = () => ups.flatMap((u) => u.reqs.map((r) => r.key));

  // ============================================================ D27 no plugin block
  {
    const id = 'D27';
    const conns = []; // connections to the address a plugin would use
    const sockPath = `${RUN}/d27.sock`;
    const unixL = net.createServer((s) => { conns.push('unix'); s.destroy(); }); await new Promise((r) => unixL.listen(sockPath, r));
    const tcpL = net.createServer((s) => { conns.push('tcp'); s.destroy(); }); await new Promise((r) => tcpL.listen(22010, '127.0.0.1', r));
    const d = new Daemon('d27', toml({ dp: P.dp }), ENV);
    await d.start();
    d.addCred('anthropic', 'a1', 'sk-ant-A1'); d.addCred('openai', 'o1', 'sk-oai-O1'); d.addCred('openrouter', 'r1', 'sk-or-R1');
    const res = [];
    for (const u of ['anthropic', 'openai', 'openrouter']) res.push(await msg(P.dp, 'p-' + u, u));
    await sleep(500);
    const st = await adminGet(d, '/v1/status');
    art(id, 'responses', res.map((r) => ({ status: r.status, body: r.body })));
    art(id, 'status', st);
    art(id, 'listener-connections', { conns });
    check(id, 'all 3 upstreams forwarded 200', res.every((r) => r.status === 200), J(res.map((r) => r.status)));
    check(id, 'listeners saw zero connections', conns.length === 0, J(conns));
    let sj; try { sj = JSON.parse(st.body); } catch {}
    check(id, 'GET /v1/status plugin.mode == "none"', sj?.plugin?.mode === 'none', st.body.slice(0, 300));
    // CLI view of same
    art(id, 'plugin-status-cli', d.cli(['plugin', 'status']).stdout);
    await d.stop(); unixL.close(); tcpL.close();
    ups.forEach((u) => (u.reqs.length = 0));
  }

  // ============================================================ main plugin daemon A (unix, default timeout)
  const behaviour = { resolve: async () => ({ use_default: true }), notifyMode: 'record' };
  let mode = (r) => ({ use_default: true });
  behaviour.resolve = async (r) => mode(r);
  const sockA = `${RUN}/a.sock`;
  const plA = await startPlugin(behaviour, { unixPath: sockA });
  const A = new Daemon('dA', toml({ dp: P.dp, plugin: `[plugin]\nendpoint = "unix://${sockA}"`, openaiExtra: `[[upstream.route]]\npath = "/v1/models"\nidentity = { method = "none" }` }), ENV);
  {
    const c = A.check(); if (c.status !== 0) { console.log('A check-config:', c.stderr); }
  }
  await A.start();
  for (const [u, ref, k] of [['anthropic', 'acct-a', 'sk-ant-AAA'], ['anthropic', 'acct-b', 'sk-ant-BBB'], ['openai', 'oa-1', 'sk-oai-111'], ['openrouter', 'or-1', 'sk-or-111']]) A.addCred(u, ref, k);
  const st = plA.state;
  const keyOf = (u, i) => u.reqs[i]?.key;

  // ------------------------------------------------------------ D28
  {
    const id = 'D28';
    const sent = [];
    const r1 = await msg(P.dp, 'u28a', 'anthropic', { 'x-request-id': 'caller-supplied-123' }, 'sessX');
    const r2 = await msg(P.dp, 'u28b', 'openai');
    const r3 = await msg(P.dp, 'u28c', 'openrouter', { 'x-request-id': 'caller-supplied-456' });
    const r4 = await msg(P.dp, 'u28d', 'anthropic');
    await sleep(500);
    const rs = ['u28a', 'u28b', 'u28c', 'u28d'].map((s) => resolveOf(st, s)[0]?.req);
    art(id, 'resolve-requests', rs);
    art(id, 'responses', [r1, r2, r3, r4].map((r) => ({ status: r.status, headers: r.headers })));
    check(id, 'all resolved', rs.every(Boolean));
    const ids = rs.map((r) => r?.request_id);
    check(id, 'request_ids non-empty and distinct', ids.every(Boolean) && new Set(ids).size === ids.length, J(ids));
    check(id, 'request_id unrelated to caller-supplied x-request-id', !ids.some((x) => /caller-supplied/.test(x)) && ids[0] !== 'caller-supplied-123');
    check(id, 'upstream field correct', J(rs.map((r) => r?.upstream)) === J(['anthropic', 'openai', 'openrouter', 'anthropic']), J(rs.map((r) => r?.upstream)));
    check(id, 'caller_identity verified/non-empty', rs.every((r, i) => r?.caller_identity === ['u28a', 'u28b', 'u28c', 'u28d'][i]));
    check(id, 'correlation_id carried from claim (a) / empty (d)', rs[0]?.correlation_id === 'sessX' && (rs[3]?.correlation_id ?? '') === '', J([rs[0]?.correlation_id, rs[3]?.correlation_id]));
    const lg = ids.map((x) => logOf(A, x));
    check(id, 'same request_id on request-log record', lg.every((l, i) => l && l.request_id === ids[i]), J(lg.map((l) => l?.request_id)));
    const nots = ids.map((x) => st.notifies.filter((n) => n.ev.request_id === x));
    art(id, 'notify-events', nots);
    check(id, 'same request_id on that request\'s Notify event', nots.every((n) => n.length >= 1), J(nots.map((n) => n.length)));
  }

  // ------------------------------------------------------------ D29
  {
    const id = 'D29';
    mode = (r) => ({ credential_ref: r.caller_identity === 'idB' ? 'acct-b' : 'acct-a' });
    const n0 = ups[0].reqs.length;
    const rB = await msg(P.dp, 'idB'); const rA = await msg(P.dp, 'idA');
    await sleep(400);
    const lB = logOf(A, resolveOf(st, 'idB')[0]?.req.request_id), lA = logOf(A, resolveOf(st, 'idA')[0]?.req.request_id);
    const seen = ups[0].reqs.slice(n0).map((r) => r.key);
    art(id, 'evidence', { rB: rB.status, rA: rA.status, lB, lA, upstreamSawKeys: seen });
    check(id, 'both 200', rA.status === 200 && rB.status === 200);
    check(id, 'log credential_ref acct-b / acct-a', lB?.credential_ref === 'acct-b' && lA?.credential_ref === 'acct-a', J([lB?.credential_ref, lA?.credential_ref]));
    check(id, 'upstream saw BBB for B then AAA for A', seen[0] === 'sk-ant-BBB' && seen[1] === 'sk-ant-AAA', J(seen));
  }

  // ------------------------------------------------------------ D30
  {
    const id = 'D30';
    mode = () => ({ use_default: true });
    const def = A.json(['credential', 'default', '--upstream', 'anthropic']);
    const n0 = ups[0].reqs.length;
    const r = await msg(P.dp, 'u30');
    await sleep(400);
    const rid = resolveOf(st, 'u30')[0].req.request_id;
    const l = logOf(A, rid);
    const cc = settle(st, rid);
    const keyUsed = ups[0].reqs[n0]?.key;
    art(id, 'evidence', { def, status: r.status, log: l, settle: cc, keyUsed });
    const dref = def.ref || def.credential_ref;
    check(id, 'forwarded 200', r.status === 200);
    check(id, 'default endpoint names a credential', !!dref, J(def));
    check(id, 'log credential_ref == default-reported credential', l?.credential_ref === dref, J([l?.credential_ref, dref]));
    const payload = JSON.parse(cc[0]?.ev.payload_json || '{}');
    check(id, 'call_completed.credential_ref == actually used', cc.length === 1 && cc[0].ev.event_type === 'call_completed' && payload.credential_ref === l?.credential_ref, J(cc));
    const dcred = A.json(['credential', 'show', dref]);
    check(id, 'upstream saw the default credential\'s material', (dref === 'acct-a' && keyUsed === 'sk-ant-AAA') || (dref === 'acct-b' && keyUsed === 'sk-ant-BBB'), J([dref, keyUsed]));
  }

  // ------------------------------------------------------------ D31
  {
    const id = 'D31';
    mode = () => ({ deny_reason: 'budget exhausted' });
    const n0 = ups[0].reqs.length;
    const r = await msg(P.dp, 'u31');
    await sleep(400);
    const rid = resolveOf(st, 'u31')[0].req.request_id;
    const l = logOf(A, rid);
    art(id, 'evidence', { status: r.status, body: r.body, headers: r.headers, log: l, settle: settle(st, rid) });
    check(id, 'non-success status', r.status >= 400, String(r.status));
    check(id, 'body contains "budget exhausted"', r.body.includes('budget exhausted'), r.body);
    check(id, 'nothing forwarded upstream', ups[0].reqs.length === n0);
    check(id, 'log outcome denied + reason', l?.outcome === 'denied' && /budget exhausted/.test(l?.reason ?? l?.error ?? ''), J(l));
    note(id, `denial HTTP status observed: ${r.status}`);
  }

  // ------------------------------------------------------------ D33 (b)(c) default 250ms  + D40 + D34 prep
  {
    const id = 'D33';
    mode = async () => { await sleep(1000); return { use_default: true }; };
    const n0 = ups[0].reqs.length;
    const rb = await msg(P.dp, 'u33b');
    const ps = pluginStatus(A);
    const l = logOf(A, resolveOf(st, 'u33b')[0].req.request_id);
    art(id, 'b-default-sleep1000', { ms: rb.ms, status: rb.status, body: rb.body, pluginStatus: ps, log: l });
    check(id, '(b) default timeout: fails, ~250ms (<800ms)', rb.status >= 400 && rb.ms >= 200 && rb.ms < 800, `${rb.status} ${rb.ms}ms`);
    check(id, '(b) not forwarded', ups[0].reqs.length === n0);
    check(id, '(b) log plugin_failed / plugin_timeout', l?.outcome === 'plugin_failed' && JSON.stringify(l).includes('plugin_timeout'), J(l));
    check(id, '(b) /v1/plugin health == timeout', ps.health === 'timeout', J(ps));
    art(id, 'b-raw-v1-plugin', await adminGet(A, '/v1/plugin'));
    mode = async () => { await sleep(100); return { use_default: true }; };
    const rc = await msg(P.dp, 'u33c');
    art(id, 'c-default-sleep100', { ms: rc.ms, status: rc.status });
    check(id, '(c) 100ms answer succeeds', rc.status === 200, `${rc.status} ${rc.ms}ms`);
    // D40 reuse: charge then sleep past timeout; observe notify
    const d40 = 'D40';
    let charged = [];
    mode = async (r) => { charged.push(r.request_id); await sleep(700); return { use_default: true }; };
    const rt = await msg(P.dp, 'u40');
    await sleep(1500);
    const rid = resolveOf(st, 'u40')[0].req.request_id;
    const ab = st.notifies.filter((n) => n.ev.request_id === rid);
    art(d40, 'evidence', { clientStatus: rt.status, ms: rt.ms, charged, notifies: ab, log: logOf(A, rid) });
    check(d40, 'request timed out (fail closed)', rt.status >= 400);
    const abEv = ab.filter((n) => n.ev.event_type === 'call_abandoned');
    const pj = abEv[0] ? JSON.parse(abEv[0].ev.payload_json) : {};
    check(d40, 'plugin receives call_abandoned for timed-out request_id', abEv.length === 1, J(ab));
    check(d40, 'stage names the resolve step', /resolve/i.test(pj.stage || ''), J(pj));
    check(d40, 'payload has caller_identity and reason', pj.caller_identity === 'u40' && !!pj.reason, J(pj));
    check(d40, 'no call_completed', !ab.some((n) => n.ev.event_type === 'call_completed'));
    mode = () => ({ use_default: true });
  }

  // ------------------------------------------------------------ D34 (a)(b)
  {
    const id = 'D34';
    mode = () => { const e = new Error('plugin internal error'); e.code = grpc.status.INTERNAL; throw e; };
    const n0 = ups[0].reqs.length;
    const ra = await msg(P.dp, 'u34a');
    const la = logOf(A, resolveOf(st, 'u34a')[0].req.request_id);
    const psa = pluginStatus(A);
    mode = () => ({}); // oneof unset
    const rb = await msg(P.dp, 'u34b');
    const lb = logOf(A, resolveOf(st, 'u34b')[0].req.request_id);
    art(id, 'a-grpc-error', { status: ra.status, body: ra.body, log: la, pluginStatus: psa });
    art(id, 'b-empty-response', { status: rb.status, body: rb.body, log: lb, pluginStatus: pluginStatus(A) });
    check(id, '(a) fails, not forwarded', ra.status >= 400 && ups[0].reqs.length === n0, String(ra.status));
    check(id, '(a) log plugin_failed / plugin_error', la?.outcome === 'plugin_failed' && J(la).includes('plugin_error'), J(la));
    check(id, '(a) health == error', psa.health === 'error', J(psa));
    check(id, '(b) empty outcome fails, not forwarded (never use_default)', rb.status >= 400 && ups[0].reqs.length === n0, String(rb.status));
    note(id, `(b) observed log: ${J(lb)}`);
    mode = () => ({ use_default: true });
    const rok = await msg(P.dp, 'u34ok'); check(id, 'daemon recovers after plugin errors', rok.status === 200, String(rok.status));
  }

  // ------------------------------------------------------------ D37 + D41 mix + D39 + D20-ish
  {
    const id = 'D41';
    const ids37 = 'D37', id39 = 'D39';
    const base = st.resolves.length;
    const logBefore = A.logs().length;
    // successful
    mode = () => ({ use_default: true });
    const r_ok = await msg(P.dp, 'm-ok');
    // denied
    mode = (r) => (r.caller_identity === 'm-deny' ? { deny_reason: 'nope' } : r.caller_identity === 'm-credfail' ? { credential_ref: 'acct-dis' } : r.caller_identity === 'm-timeout' ? new Promise((res) => setTimeout(() => res({ use_default: true }), 700)) : { use_default: true });
    const r_deny = await msg(P.dp, 'm-deny');
    // credential-failed: disabled credential
    A.addCred('anthropic', 'acct-dis', 'sk-ant-DIS'); A.cli(['credential', 'disable', 'acct-dis']);
    const r_cf = await msg(P.dp, 'm-credfail');
    // timed-out
    const r_to = await msg(P.dp, 'm-timeout');
    // rejected identity
    const r_rej1 = await req(P.dp, 'POST', '/anthropic/v1/messages', { headers: { 'content-type': 'application/json' }, body: '{}' });
    const r_rej2 = await req(P.dp, 'POST', '/anthropic/v1/messages', { headers: { authorization: 'Bearer ' + mint('wrong', { subject: 'm-forged' }) }, body: '{}' });
    // none route
    const r_none = await req(P.dp, 'GET', '/openai/v1/models', {});
    // nonexistent credential ref
    mode = (r) => (r.caller_identity === 'm-noref' ? { credential_ref: 'does-not-exist' } : { use_default: true });
    const r_noref = await msg(P.dp, 'm-noref');
    await sleep(2000);
    const resolved = st.resolves.slice(base).map((x) => x.req);
    const rids = resolved.map((x) => x.request_id);
    const evs = st.notifies;
    const cnt = Object.fromEntries(rids.map((rid) => [rid, evs.filter((n) => n.ev.request_id === rid && ['call_completed', 'call_abandoned'].includes(n.ev.event_type)).map((n) => n.ev.event_type)]));
    const allLogs = A.logs();
    const newLogs = allLogs.slice(0, allLogs.length - logBefore);
    const unresolvedLogs = newLogs.filter((l) => !rids.includes(l.request_id));
    const stray = unresolvedLogs.map((l) => ({ rid: l.request_id, outcome: l.outcome, binding: l.binding, events: evs.filter((n) => n.ev.request_id === l.request_id).map((n) => n.ev.event_type) }));
    art(id, 'status-codes', { r_ok: r_ok.status, r_deny: r_deny.status, r_cf: r_cf.status, r_to: r_to.status, r_rej1: r_rej1.status, r_rej2: r_rej2.status, r_none: r_none.status, r_noref: r_noref.status });
    art(id, 'settlement-per-resolved-request', { resolvedIdentities: resolved.map((x) => [x.request_id, x.caller_identity]), cnt });
    art(id, 'unresolved-requests-events', stray);
    art(id, 'request-log-new', newLogs);
    check(id, 'mix executed: ok=200,deny/credfail/timeout/noref fail,rejects 401,none route forwarded', r_ok.status === 200 && r_deny.status >= 400 && r_cf.status >= 400 && r_to.status >= 400 && r_rej1.status === 401 && r_rej2.status === 401 && r_none.status === 200 && r_noref.status >= 400, J([r_ok.status, r_deny.status, r_cf.status, r_to.status, r_rej1.status, r_rej2.status, r_none.status, r_noref.status]));
    check(id, 'resolved identities are exactly the 5 plugin-visible ones', J(resolved.map((x) => x.caller_identity).sort()) === J(['m-ok', 'm-deny', 'm-credfail', 'm-timeout', 'm-noref'].sort()), J(resolved.map((x) => x.caller_identity)));
    check(id, 'every resolved request_id has exactly ONE settlement event', rids.length === 5 && rids.every((r) => cnt[r].length === 1), J(cnt));
    check(id, 'm-ok => call_completed; others => call_abandoned', resolved.every((x) => (x.caller_identity === 'm-ok') === (cnt[x.request_id][0] === 'call_completed')), J(cnt));
    check(id, 'rejected/none requests: zero events of any kind', stray.length >= 3 && stray.every((s) => s.events.length === 0), J(stray));
    // D37
    check(ids37, 'plugin recorded no ResolveRoute with empty caller_identity (whole run so far)', st.resolves.every((x) => !!x.req.caller_identity));
    check(ids37, 'no ResolveRoute for rejected (m-forged / no-material) or none-route requests', !st.resolves.some((x) => x.req.caller_identity === 'm-forged') && resolved.length === 5 && stray.length >= 3);
    art(ids37, 'resolve-identities', st.resolves.map((x) => x.req.caller_identity));
    // D39
    const abCF = evs.filter((n) => n.ev.request_id === resolved.find((x) => x.caller_identity === 'm-credfail')?.request_id);
    const abNR = evs.filter((n) => n.ev.request_id === resolved.find((x) => x.caller_identity === 'm-noref')?.request_id);
    const abDeny = evs.filter((n) => n.ev.request_id === resolved.find((x) => x.caller_identity === 'm-deny')?.request_id);
    art(id39, 'disabled-credential-events', abCF); art(id39, 'unknown-ref-events', abNR); art(id39, 'denied-events', abDeny);
    art(id39, 'disabled-credential-response', { status: r_cf.status, body: r_cf.body, log: logOf(A, resolved.find((x) => x.caller_identity === 'm-credfail')?.request_id) });
    const pjc = abCF[0] ? JSON.parse(abCF[0].ev.payload_json) : {};
    check(id39, 'disabled credential => exactly one call_abandoned', abCF.length === 1 && abCF[0].ev.event_type === 'call_abandoned', J(abCF));
    check(id39, 'stage names the credential step', /cred/i.test(pjc.stage || ''), J(pjc));
    check(id39, 'payload has caller_identity + reason', pjc.caller_identity === 'm-credfail' && !!pjc.reason, J(pjc));
    check(id39, 'no call_completed for it', !abCF.some((n) => n.ev.event_type === 'call_completed'));
    const pjd = abDeny[0] ? JSON.parse(abDeny[0].ev.payload_json) : {};
    note(id39, `denied request -> ${abDeny.map((n) => n.ev.event_type)} stage=${pjd.stage}; unknown ref stage=${abNR[0] && JSON.parse(abNR[0].ev.payload_json).stage}`);
    A.cli(['credential', 'rm', 'acct-dis']);
    mode = () => ({ use_default: true });
  }

  // ------------------------------------------------------------ D38 (clean single request)
  {
    const id = 'D38';
    mode = () => ({ use_default: true });
    const r = await msg(P.dp, 'u38', 'anthropic', {}, 'corr38');
    await sleep(500);
    const rid = resolveOf(st, 'u38')[0].req.request_id;
    const evs = st.notifies.filter((n) => n.ev.request_id === rid);
    const l = logOf(A, rid);
    const ccs = evs.filter((n) => n.ev.event_type === 'call_completed');
    const pj = ccs[0] ? JSON.parse(ccs[0].ev.payload_json) : {};
    art(id, 'evidence', { status: r.status, events: evs, log: l });
    check(id, 'exactly one call_completed', ccs.length === 1 && evs.length === 1, J(evs));
    check(id, 'request_id + correlation_id match', ccs[0]?.ev.request_id === rid && ccs[0]?.ev.correlation_id === 'corr38', J(ccs[0]?.ev));
    check(id, 'payload fields: caller_identity, upstream, status, credential_ref, duration_ms, usage', pj.caller_identity === 'u38' && pj.upstream === 'anthropic' && pj.status === 200 && pj.credential_ref === l?.credential_ref && typeof pj.duration_ms === 'number' && pj.usage !== undefined, J(pj));
    check(id, 'usage identical to request-log usage', J(pj.usage) === J(l?.usage) && l?.usage != null, J([pj.usage, l?.usage]));
  }

  // ------------------------------------------------------------ D42 / D43
  {
    const id = 'D42';
    A.addCred('anthropic', 'acct-bad', 'sk-ant-garbage-key');
    mode = () => ({ credential_ref: 'acct-bad' });
    const before = A.json(['credential', 'show', 'acct-bad']);
    const r = await msg(P.dp, 'u42');
    await sleep(600);
    const rid = resolveOf(st, 'u42')[0].req.request_id;
    const evs = st.notifies.filter((n) => n.ev.request_id === rid || n.ev.event_type === 'credential_invalid');
    const after = A.json(['credential', 'show', 'acct-bad']);
    art(id, 'evidence', { clientStatus: r.status, body: r.body, events: evs, before, after, cliShow: A.cli(['credential', 'show', 'acct-bad']).stdout });
    check(id, 'upstream rejected (client sees 401-ish)', r.status === 401 || r.status >= 400, String(r.status));
    check(id, 'plugin receives credential_invalid', st.notifies.some((n) => n.ev.event_type === 'credential_invalid'), J(st.notifies.map((n) => n.ev.event_type)));
    check(id, 'credential status becomes invalid (before=active)', before.status === 'active' && after.status === 'invalid', J([before.status, after.status]));
    A.cli(['credential', 'rm', 'acct-bad']);

    const id2 = 'D43';
    const tried = {};
    for (const [nm, key] of [['billing402', 'sk-ant-acctbilling'], ['credit400', 'sk-ant-acctcredit'], ['disabled403', 'sk-ant-acctdisabled']]) {
      A.addCred('anthropic', 'acct-ad', key);
      mode = () => ({ credential_ref: 'acct-ad' });
      const n0 = st.notifies.length;
      const rr = await msg(P.dp, 'u43' + nm);
      await sleep(600);
      const ev = st.notifies.slice(n0);
      tried[nm] = { status: rr.status, body: rr.body, events: ev.map((n) => n.ev), cred: A.json(['credential', 'show', 'acct-ad']) };
      const hasAE = ev.some((n) => n.ev.event_type === 'account_error');
      if (nm !== 'disabled403') check(id2, `upstream ${nm} billing-class error => plugin gets account_error`, hasAE, J(ev.map((n) => n.ev.event_type)));
      else check(id2, 'upstream 403 permission_error (per-request refusal) must NOT emit account_error', !hasAE && rr.status === 403, J([rr.status, ev.map((n) => n.ev.event_type)]));
      A.cli(['credential', 'rm', 'acct-ad']);
    }
    art(id2, 'evidence', tried);
    mode = () => ({ use_default: true });
  }

  // ------------------------------------------------------------ D45 gRPC part (one stream so far)
  {
    const id = 'D45';
    const kinds = [...new Set(st.notifies.map((n) => n.ev.event_type))];
    let parse = true; for (const n of st.notifies) { try { JSON.parse(n.ev.payload_json); } catch { parse = false; } }
    art(id, 'evidence', { totalEvents: st.notifies.length, notifyStreamsOpened: st.notifyStreams, kinds, sample: st.notifies.slice(0, 3).map((n) => n.ev) });
    check(id, 'all events arrived on a single Notify client stream', st.notifyStreams === 1 && st.notifies.length > 10, `streams=${st.notifyStreams} events=${st.notifies.length}`);
    check(id, 'every payload_json parses as JSON', parse);
    check(id, 'envelope kinds incl. call_completed, call_abandoned, credential_invalid, account_error', ['call_completed', 'call_abandoned', 'credential_invalid', 'account_error'].every((k) => kinds.includes(k)), J(kinds));
    note(id, 'linked-plugin half is asserted in the D36 linked run: notify() call count equals the number of events emitted (recorded in D45-linked-notify-calls.txt)');
  }

  // ------------------------------------------------------------ D46 restart
  {
    const id = 'D46';
    mode = () => ({ use_default: true });
    const r0 = await msg(P.dp, 'u46pre');
    await plA.stop();
    await sleep(300);
    const n0 = ups[0].reqs.length;
    const rOut = await msg(P.dp, 'u46out');
    const lOut = A.logs().find((l) => l.caller_identity === 'u46out' || (l.outcome === 'plugin_failed' && l.path.includes('messages') && l.status !== 200 && Date.parse(l.received_at) > Date.now() - 3000));
    const psOut = pluginStatus(A);
    art(id, 'outage', { status: rOut.status, body: rOut.body, log: lOut, pluginStatus: psOut, forwarded: ups[0].reqs.length - n0 });
    check(id, 'outage request fails closed, not forwarded', rOut.status >= 400 && ups[0].reqs.length === n0, String(rOut.status));
    check(id, 'outage log plugin_failed / plugin_unreachable; health unreachable + last_error', lOut?.outcome === 'plugin_failed' && J(lOut).includes('plugin_unreachable') && psOut.health === 'unreachable' && !!psOut.last_error, J([lOut, psOut]));
    // restart plugin with fresh state
    const plA2 = await startPlugin(behaviour, { unixPath: sockA });
    await sleep(500);
    const results = [];
    for (let i = 0; i < 6; i++) { results.push(await msg(P.dp, 'u46post' + i)); await sleep(500); }
    await sleep(1500);
    const st2 = plA2.state;
    const evs = st2.notifies.map((n) => n.ev);
    art(id, 'after-restart', { statuses: results.map((r) => r.status), resolves: st2.resolves.map((x) => x.req), notifyStreams: st2.notifyStreams, events: evs, pluginStatus: pluginStatus(A) });
    check(id, 'after restart, ResolveRoute reaches new plugin and requests succeed (no daemon restart)', st2.resolves.length >= 1 && results.some((r) => r.status === 200), J(results.map((r) => r.status)));
    const ok = results.map((r, i) => (r.status === 200 ? 'u46post' + i : null)).filter(Boolean);
    check(id, 'settlement events for new requests arrive over a NEW Notify stream', st2.notifyStreams >= 1 && ok.every((s) => evs.some((e) => e.request_id === resolveOf(st2, s)[0]?.req.request_id && e.event_type === 'call_completed')), J({ streams: st2.notifyStreams, n: evs.length }));
    check(id, 'each settlement event carries caller_identity', evs.filter((e) => /call_(completed|abandoned)/.test(e.event_type)).every((e) => !!JSON.parse(e.payload_json).caller_identity) && evs.length > 0);
    check(id, 'plugin status healthy again', pluginStatus(A).health === 'ok', J(pluginStatus(A)));
    await plA2.stop();
  }
  await A.stop();

  // ============================================================ D33(a): timeout_ms = 1000
  {
    const id = 'D33';
    const sock = `${RUN}/b.sock`;
    const pl = await startPlugin({ resolve: async () => { await sleep(2000); return { use_default: true }; } }, { unixPath: sock });
    const d = new Daemon('dB', toml({ dp: P.dp, plugin: `[plugin]\nendpoint = "unix://${sock}"\ntimeout_ms = 1000` }), ENV);
    await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
    const n0 = ups[0].reqs.length;
    const r = await msg(P.dp, 'u33a');
    const ps = pluginStatus(d); const l = d.logs()[0];
    art(id, 'a-timeout1000-sleep2000', { ms: r.ms, status: r.status, body: r.body, pluginStatus: ps, log: l });
    check(id, '(a) fails after roughly 1s', r.status >= 400 && r.ms >= 900 && r.ms < 1700, `${r.status} ${r.ms}ms`);
    check(id, '(a) not forwarded; log plugin_failed/plugin_timeout; health timeout', ups[0].reqs.length === n0 && l?.outcome === 'plugin_failed' && J(l).includes('plugin_timeout') && ps.health === 'timeout', J([l, ps]));
    await d.stop(); await pl.stop();
  }

  // ============================================================ D32 plugin unreachable (never started)
  {
    const id = 'D32';
    const sock = `${RUN}/none.sock`;
    const d = new Daemon('dC', toml({ dp: P.dp, plugin: `[plugin]\nendpoint = "unix://${sock}"` }), ENV);
    art(id, 'check-config', (({ status, stdout, stderr }) => ({ status, stdout, stderr }))(d.check()));
    await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
    const n0 = ups[0].reqs.length;
    const r = await msg(P.dp, 'u32');
    const l = d.logs()[0]; const ps = pluginStatus(d);
    art(id, 'evidence', { status: r.status, body: r.body, log: l, pluginStatus: ps, rawPlugin: await adminGet(d, '/v1/plugin') });
    check(id, 'request fails, not forwarded', r.status >= 400 && ups[0].reqs.length === n0, String(r.status));
    check(id, 'log plugin_failed / plugin_unreachable', l?.outcome === 'plugin_failed' && J(l).includes('plugin_unreachable'), J(l));
    check(id, '/v1/plugin health=unreachable with last_error', ps.health === 'unreachable' && !!ps.last_error, J(ps));
    await d.stop();
  }

  // ============================================================ D44 notify blocking / erroring
  {
    const id = 'D44';
    const out = {};
    for (const nm of ['block', 'error']) {
      const sock = `${RUN}/n-${nm}.sock`;
      const pl = await startPlugin({ resolve: async () => ({ use_default: true }), notifyMode: nm }, { unixPath: sock });
      const d = new Daemon('dN' + nm, toml({ dp: P.dp, plugin: `[plugin]\nendpoint = "unix://${sock}"` }), ENV);
      await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
      const lat = []; const sts = []; let bodyOk = true;
      for (let i = 0; i < 40; i++) { const r = await msg(P.dp, 'n' + nm + i); lat.push(r.ms); sts.push(r.status); bodyOk &&= r.body.includes('"ok":true'); }
      out[nm] = { sts: [...new Set(sts)], maxMs: Math.max(...lat), medianMs: lat.sort((a, b) => a - b)[20], notifiesSeen: pl.state.notifies.length, streams: pl.state.notifyStreams, pluginStatus: pluginStatus(d) };
      check(id, `${nm}: all 40 responses 200 with normal body`, sts.every((s) => s === 200) && bodyOk, J(out[nm]));
      check(id, `${nm}: latency normal (max < 400ms)`, Math.max(...lat) < 400, J(out[nm]));
      await d.stop(); await pl.stop();
    }
    art(id, 'evidence', out);
  }

  // ============================================================ D35 TLS
  {
    const id = 'D35';
    const good = `${SCRATCH}/tls-good`, other = `${SCRATCH}/tls-other`;
    fs.rmSync(good, { recursive: true, force: true }); fs.rmSync(other, { recursive: true, force: true }); fs.mkdirSync(good, { recursive: true }); fs.mkdirSync(other, { recursive: true });
    const run = (dir) => spawnSync(`${BIN}/janus`, ['plugin', 'tls', 'init', '--plugin-host', 'localhost', '--out', dir], { encoding: 'utf8' });
    const g = run(good), o = run(other);
    art(id, 'tls-init', { good: g.stdout + g.stderr, other: o.stdout + o.stderr });
    const tlsBlock = `\n[plugin.tls]\npinned_cert = "${good}/plugin.pem"\nclient_cert = "${good}/daemon.pem"\nclient_key = "${good}/daemon-key.pem"\n`;
    const cases = [
      { nm: 'untrusted', port: 22031, tls: { cert: `${other}/plugin.pem`, key: `${other}/plugin-key.pem`, clientCa: `${good}/daemon.pem` }, cls: 'plugin_tls_error', health: 'tls_error' },
      { nm: 'noclient', port: 22032, tls: { cert: `${good}/plugin.pem`, key: `${good}/plugin-key.pem`, clientCa: null }, cls: 'plugin_no_client_auth', health: 'no_client_auth' },
      { nm: 'rejected', port: 22033, tls: { cert: `${good}/plugin.pem`, key: `${good}/plugin-key.pem`, clientCa: `${other}/daemon.pem` }, cls: 'plugin_rejected_client_cert', health: 'rejected_client_cert' },
    ];
    // sanity: a correct TLS plugin works (proves config format and harness)
    const sane = await startPlugin({ resolve: async () => ({ use_default: true }) }, { port: 22030, tls: { cert: `${good}/plugin.pem`, key: `${good}/plugin-key.pem`, clientCa: `${good}/daemon.pem` } });
    const outs = {};
    const mk = (port) => new Daemon('dT' + port, toml({ dp: P.dp, plugin: `[plugin]\nendpoint = "https://localhost:${port}"\ntimeout_ms = 2000${tlsBlock}` }), ENV);
    {
      const d = mk(22030); const c = d.check(); art(id, 'check-config-sane', { status: c.status, stderr: c.stderr, stdout: c.stdout });
      await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
      const r = await msg(P.dp, 'tls-sane'); outs.sane = { status: r.status, pluginStatus: pluginStatus(d) };
      check(id, 'harness sanity: correctly configured TLS plugin => 200', r.status === 200, J([r.status, r.body, outs.sane.pluginStatus]));
      await d.stop();
    }
    await sane.stop();
    for (const c of cases) {
      let pl, py;
      if (c.nm === 'rejected') {
        // A server that rejects the client cert then lingers before closing (so the TLS alert is not lost to a TCP RST).
        py = spawn('python3', ['-I', `${REPO}/evidence/acceptance/support/cd-rejsrv.py`, String(c.port), `${good}/plugin.pem`, `${good}/plugin-key.pem`, `${other}/daemon.pem`], { stdio: 'ignore' });
        pl = { state: { resolves: [] }, stop: async () => py.kill('SIGKILL') };
        await sleep(600);
      } else pl = await startPlugin({ resolve: async () => ({ use_default: true }) }, { port: c.port, tls: c.tls });
      const d = mk(c.port);
      await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
      const n0 = ups[0].reqs.length;
      const r = await msg(P.dp, 'tls-' + c.nm);
      const l = d.logs()[0]; const ps = pluginStatus(d);
      outs[c.nm] = { status: r.status, body: r.body, log: l, pluginStatus: ps, resolves: pl.state.resolves.length, forwarded: ups[0].reqs.length - n0 };
      check(id, `${c.nm}: fails closed, not forwarded`, r.status >= 400 && ups[0].reqs.length === n0 && pl.state.resolves.length === 0, J(outs[c.nm]));
      check(id, `${c.nm}: log class ${c.cls}`, l?.outcome === 'plugin_failed' && J(l).includes(c.cls), J(l));
      check(id, `${c.nm}: /v1/plugin health ${c.health}`, ps.health === c.health, J(ps));
      await d.stop(); await pl.stop();
    }
    {
      // informational: node/grpc-js server that rejects the client cert and immediately resets the TCP connection
      const pl = await startPlugin({ resolve: async () => ({ use_default: true }) }, { port: 22034, tls: { cert: `${good}/plugin.pem`, key: `${good}/plugin-key.pem`, clientCa: `${other}/daemon.pem` } });
      const d = mk(22034); await d.start(); d.addCred('anthropic', 'a', 'sk-ant-A');
      const r = await msg(P.dp, 'tls-reset'); const l = d.logs()[0];
      outs.rejected_immediate_reset_server = { status: r.status, class: l?.class, health: pluginStatus(d).health };
      check(id, 'rejection by bare TCP reset (no TLS alert) is classed plugin_unreachable, not rejected_client_cert', r.status >= 400 && l?.outcome === 'plugin_failed' && l?.class === 'plugin_unreachable' && outs.rejected_immediate_reset_server.health === 'unreachable', J(outs.rejected_immediate_reset_server));
      await d.stop(); await pl.stop();
    }
    art(id, 'evidence', outs);
  }

  // ============================================================ D36 linked plugin panic / D45 linked half
  {
    const id = 'D36';
    try { await linked(id, ups); } catch (e) { block(id, 'linked-plugin attempt errored: ' + e.message); }
  }
}

// ---------------------------------------------------------------- linked plugin (custom build)
async function linked(id, ups) {
  const crate = `${REPO}/evidence/acceptance/support/cd-linked`;
  const bld = spawnSync('cargo', ['build', '--release'], { cwd: crate, env: { ...process.env, CARGO_TARGET_DIR: '/tmp/qa-target-cd' }, encoding: 'utf8' });
  if (bld.status !== 0) { block(id, 'custom-build compile failed: ' + bld.stderr.slice(-400)); return; }
  const notifyLog = `${SCRATCH}/linked-notify.log`; fs.rmSync(notifyLog, { force: true });
  const d = new Daemon('dLinked', toml({ dp: P.dp, plugin: '[plugin]\nlinked = true' }), { ...ENV, CD_NOTIFY_LOG: notifyLog });
  d.bin = '/tmp/qa-target-cd/release/cd-linked';
  await d.start(); d.addCred('anthropic', 'acct-a', 'sk-ant-A'); d.addCred('anthropic', 'acct-bad', 'sk-ant-garbage-x');
  const n0 = ups[0].reqs.length;
  const rCrash = await msg(P.dp, 'crash');
  const forwardedForCrash = ups[0].reqs.length - n0;
  const lCrash = d.logs().find((l) => l.caller_identity === 'crash');
  const alive1 = !d.exited;
  const rNext = await msg(P.dp, 'someone-else');
  const K = 5; const oks = [];
  for (let i = 0; i < K - 1; i++) oks.push((await msg(P.dp, 'ok' + i)).status);
  const rBad = await msg(P.dp, 'badkey');
  const rCrash2 = await msg(P.dp, 'crash');
  const rNext2 = await msg(P.dp, 'after-second-crash');
  await sleep(800);
  const lines = fs.existsSync(notifyLog) ? fs.readFileSync(notifyLog, 'utf8').trim().split('\n').map((l) => l.split('\t')) : [];
  const ps = pluginStatus(d);
  art(id, 'linked-run', { crash: { status: rCrash.status, body: rCrash.body, forwardedForCrash, log: lCrash }, next: rNext.status, oks, rBad: rBad.status, rCrash2: rCrash2.status, rNext2: rNext2.status, aliveAfterFirstCrash: alive1, aliveAtEnd: !d.exited, pluginStatus: ps, daemonLog: fs.readFileSync(d.logFile, 'utf8').slice(-1500) });
  check(id, 'crash request fails (non-success), not forwarded upstream', rCrash.status >= 400 && forwardedForCrash === 0, J([rCrash.status, forwardedForCrash]));
  check(id, 'daemon process keeps running after the panic', alive1 && !d.exited);
  check(id, 'next request (other identity) served normally', rNext.status === 200 && rNext2.status === 200, J([rNext.status, rNext2.status]));
  check(id, 'second crash also contained', rCrash2.status >= 400);
  note(id, `log outcome/class for crash (spec leaves open): ${J({ outcome: lCrash?.outcome, class: lCrash?.class })}`);
  // D45 linked half: expected events = (K ok completions) + (credential_invalid + call_completed for badkey) + (2 abandoned for the two crashes)
  const kinds = lines.map((l) => l[0]);
  const expected = (K - 1) + 1 /*someone-else*/ + 1 /*after-second-crash*/ + 2 /*badkey*/ + 2 /*crashes*/;
  art('D45', 'linked-notify-calls', { calls: lines.length, expected, kinds, lines });
  check('D45', 'linked plugin notify() call count == number of events (once per event)', lines.length === expected && lines.every((l) => { try { JSON.parse(l.slice(2).join('\t')); return true; } catch { return false; } }), J({ calls: lines.length, expected, kinds }));
  await d.stop();
}

let failed = false;
try { await main(); } catch (e) { console.error('HARNESS ERROR', e); failed = true; }
for (const c of cleanups) try { c(); } catch {}
for (const d of L.ALL) try { d.proc?.kill('SIGKILL'); } catch {}

// ---------------------------------------------------------------- results
const IDS = Array.from({ length: 20 }, (_, i) => 'D' + (27 + i));
const results = IDS.map((id) => {
  const r = cur(id);
  let result;
  if (r.blocked && r.checks.length === 0) result = 'BLOCKED';
  else if (r.checks.length === 0) result = 'BLOCKED';
  else result = r.checks.every((c) => c.ok) ? 'PASS' : 'FAIL';
  const fails = r.checks.filter((c) => !c.ok).map((c) => `${c.label} [${c.detail}]`);
  const summary = `${r.checks.filter((c) => c.ok).length}/${r.checks.length} checks passed.` + (r.blocked ? ` BLOCKED: ${r.blocked}.` : '') + (fails.length ? ' FAILED: ' + fails.join('; ') : '') + (r.notes.length ? ' NOTES: ' + r.notes.join(' | ') : '');
  return { id, result, artifacts: [...r.arts], note: summary };
});
fs.mkdirSync(`${REPO}/evidence/results`, { recursive: true });
fs.writeFileSync(`${REPO}/evidence/results/cd.json`, JSON.stringify(results, null, 2));
console.log('\n' + results.map((r) => `${r.id} ${r.result}`).join('\n'));
const allPass = !failed && results.every((r) => r.result === 'PASS');
console.log(allPass ? 'CD PASS' : 'CD FAIL (see evidence/results/cd.json)');
process.exit(allPass ? 0 : 1);
