#!/usr/bin/env node
// Group B acceptance (D10-D26): caller identity extraction and verification.
// Black-box: starts real janus-daemon processes, mock upstreams and a mock gRPC plugin.
// Ports used: 21000-21999 only.
import net from 'node:net';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import crypto from 'node:crypto';
import * as L from './lib-b.mjs';
const { startMock, startPlugin, startDaemon, request, hmacToken, now, sleep, jwt, REPO, SCRATCH } = L;

fs.mkdirSync(SCRATCH, { recursive: true });
const R = new L.Results('b');
const IDS = Array.from({ length: 17 }, (_, i) => `D${10 + i}`);
const cleanup = [];
const J = (o) => JSON.stringify(o);
const hdrStr = (h) => J(h);

const SECRET = 'S';
const NS = 'janus-data-plane';
const OKTOK = (extra = {}) => hmacToken(SECRET, { ns: NS, exp: now() + 3600, subject: 'agent-1', ...extra });
const leaks = (rec, token) =>
  rec.url.includes(token) || J(rec.headers).includes(token) || rec.body.toString().includes(token) ||
  token.split('.').some((p) => p.length > 8 && (rec.url.includes(p) || J(rec.headers).includes(p)));

const BASE_ID = `
[secrets]
caller-identity={ env="QA_SECRET" }
caller-identity2={ env="QA_SECRET2" }
`;
const HMAC_DEFAULT = (extra = '') => `
[identity]
method="hmac"
source={ header="authorization", scheme="Bearer " }
secret_ref="caller-identity"
namespace="${NS}"
identity_claim="subject"
${extra}
`;
const UP = (name, port, extra = '') => `
[[upstream]]
name="${name}"
path_prefix="/${name}"
base_url="http://127.0.0.1:${port}"
${extra}
`;

async function main() {
  const up = await startMock(21100); cleanup.push(up.close);
  const pl = await startPlugin('127.0.0.1:21300'); cleanup.push(pl.close);

  // =====================================================================
  // MAIN daemon: hmac default (correlation_claim=session), plugin, openai=opaque, route none
  // =====================================================================
  const main = await startDaemon('main', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21200"
[plugin]
endpoint="http://127.0.0.1:21300"
allow_plaintext=true
${BASE_ID}
${HMAC_DEFAULT('correlation_claim="session"')}
${UP('anthropic', 21100, `
[[upstream.route]]
path="/v1/nocorr"
identity={ method="hmac", source={ header="authorization", scheme="Bearer " }, secret_ref="caller-identity", namespace="${NS}", identity_claim="subject" }
`)}
${UP('openai', 21100, `
[upstream.identity]
method="opaque"
source={ header="x-api-key" }
[[upstream.route]]
path="/v1/models"
identity={ method="none" }
`)}
`);
  cleanup.push(() => main.stop());
  main.addKey('anthropic', 'a1', 'sk-pool-anthropic-AAAA');
  main.addKey('openai', 'o1', 'sk-pool-openai-OOOO');
  let nlog = 0;
  const hit = async (d, o) => { const r = await request({ port: 21200, ...o }); nlog++; const l = await d.waitLogs(nlog); return { r, rec: l[0] }; };
  const A = (path = '/anthropic/v1/messages') => ({ method: 'POST', path, body: '{"m":1}' });
  const bearer = (t, extra = {}) => ({ authorization: `Bearer ${t}`, 'content-type': 'application/json', ...extra });

  // ---------------- D10 ----------------
  {
    const t = OKTOK();
    const before = up.reqs.length;
    const { r, rec } = await hit(main, { ...A(), headers: bearer(t) });
    R.art('D10', 'transcript', `token=${t}\nstatus=${r.status}\nbody=${r.body}\nlog=${J(rec)}`);
    R.check('D10', r.status === 200 && up.reqs.length === before + 1, 'independently minted <claims-hex>.<sig-hex> token forwarded (HTTP 200, upstream hit)');
    R.check('D10', rec.binding === 'hmac' && rec.outcome === 'forwarded' && rec.caller_identity === 'agent-1', `log binding=${rec.binding} outcome=${rec.outcome} caller_identity=${rec.caller_identity}`);
  }

  // ---------------- D11 ----------------
  {
    const cases = [];
    const good = OKTOK();
    const [h] = good.split('.');
    const wrongSecret = hmacToken('other-secret', { ns: NS, exp: now() + 3600, subject: 'agent-1' });
    const altered = Buffer.from(JSON.stringify({ ns: NS, exp: now() + 3600, subject: 'mallory' })).toString('hex') + '.' + good.split('.')[1];
    const expired = hmacToken(SECRET, { ns: NS, exp: now() - 100, subject: 'agent-1' });
    const wrongNs = hmacToken(SECRET, { ns: 'other', exp: now() + 3600, subject: 'agent-1' });
    cases.push(['a1-wrong-secret', wrongSecret, ['invalid']], ['a2-altered-claims', altered, ['invalid']],
      ['b-expired', expired, ['expired', 'invalid']], ['c-wrong-ns', wrongNs, ['wrong_namespace']]);
    for (const [n, t, kinds] of cases) {
      const before = up.reqs.length;
      const { r, rec } = await hit(main, { ...A(), headers: bearer(t) });
      R.art('D11', 'transcript', `${n}: status=${r.status} body=${r.body}\n  log=${J(rec)}`);
      R.check('D11', r.status === 401 && up.reqs.length === before, `${n}: HTTP 401 and not forwarded (got ${r.status})`);
      R.check('D11', rec.outcome === 'rejected_identity' && kinds.includes(rec.kind), `${n}: outcome=${rec.outcome} kind=${rec.kind} (want one of ${kinds})`);
    }
  }

  // ---------------- D16 misplaced (main portion) / D22 no_material ----------------
  {
    const t = OKTOK();
    const before = up.reqs.length;
    const { r, rec } = await hit(main, { ...A(), headers: { 'x-api-key': t } });
    R.art('D16', 'misplaced-main', `status=${r.status} body=${r.body}\nlog=${J(rec)}`);
    R.check('D16', r.status === 401 && rec.kind === 'no_material' && up.reqs.length === before, `misplaced token (x-api-key to authorization binding): 401 kind=${rec.kind}`);
    R.art('D22', 'no_material', J(rec));
    R.check('D22', rec.outcome === 'rejected_identity' && rec.kind === 'no_material', 'misplaced -> kind no_material');
  }

  // ---------------- D13 / D14 / D22 opaque ----------------
  const OAI = (path = '/openai/v1/chat/completions') => ({ method: 'POST', path, body: '{"m":2}' });
  let opaqueId;
  {
    const c = main.json('identity', 'create', '--identity', 'agent-7', '--claim', 'tenant=acme');
    const tok = c.token; opaqueId = c.id;
    const plain = main.janus('identity', 'create', '--identity', 'agent-7b');
    R.art('D13', 'cli', `create(--json)=${J(c)}\nhuman create: exit=${plain.code} stdout=${J(plain.out)} stderr=${plain.err}`);
    R.check('D13', /^[\w-]+\n$/.test(plain.out), 'human `identity create` prints only the token on stdout');
    const before = up.reqs.length;
    pl.resolves.length = 0;
    const { r, rec } = await hit(main, { ...OAI(), headers: { 'x-api-key': tok } });
    R.art('D13', 'transcript', `status=${r.status}\nlog=${J(rec)}\nplugin.resolves=${J(pl.resolves)}`);
    R.check('D13', r.status === 200 && up.reqs.length === before + 1, 'opaque token forwarded');
    R.check('D13', rec.binding === 'opaque' && rec.caller_identity === 'agent-7', `log binding=${rec.binding} caller_identity=${rec.caller_identity}`);
    R.check('D13', pl.resolves.length === 1 && pl.resolves[0].caller_identity === 'agent-7' && J(pl.resolves[0].identity_claims) === '{"tenant":"acme"}', `plugin ResolveRoute identity_claims=${J(pl.resolves[0]?.identity_claims)}`);

    // D14: revoke
    const rv = main.janus('identity', 'revoke', opaqueId);
    const before2 = up.reqs.length;
    const rev = await hit(main, { ...OAI(), headers: { 'x-api-key': tok } });
    R.art('D14', 'revoked', `revoke exit=${rv.code} ${rv.err}\nstatus=${rev.r.status}\nlog=${J(rev.rec)}`);
    R.check('D14', rv.code === 0 && rev.r.status === 401 && up.reqs.length === before2 && rev.rec.kind === 'revoked', `revoked: 401 kind=${rev.rec.kind} (immediately, no restart)`);
    R.art('D22', 'revoked', J(rev.rec));
    R.check('D22', rev.rec.outcome === 'rejected_identity' && rev.rec.kind === 'revoked', 'revoked -> kind revoked');

    // expiry
    const e = main.json('identity', 'create', '--identity', 'agent-exp', '--expires-in', '1s');
    const okfirst = await hit(main, { ...OAI(), headers: { 'x-api-key': e.token } });
    await sleep(1600);
    const exp = await hit(main, { ...OAI(), headers: { 'x-api-key': e.token } });
    R.art('D14', 'expired', `first(before expiry) status=${okfirst.r.status}\nafter expiry status=${exp.r.status}\nlog=${J(exp.rec)}`);
    R.check('D14', okfirst.r.status === 200, 'entry usable before expiry');
    R.check('D14', exp.r.status === 401 && exp.rec.kind === 'expired', `expired: 401 kind=${exp.rec.kind}`);
    R.art('D22', 'expired', J(exp.rec));
    R.check('D22', exp.rec.outcome === 'rejected_identity' && exp.rec.kind === 'expired', 'expired -> kind expired');

    // unknown
    const unk = await hit(main, { ...OAI(), headers: { 'x-api-key': crypto.randomBytes(32).toString('base64url') } });
    R.art('D14', 'unknown', `status=${unk.r.status}\nlog=${J(unk.rec)}`);
    R.check('D14', unk.r.status === 401 && unk.rec.kind === 'invalid', `unknown token: 401 kind=${unk.rec.kind}`);
  }

  // ---------------- D22 / D23 hmac kinds + claimed identity ----------------
  {
    const tampered = (() => { const t = OKTOK(); const [h, s] = t.split('.'); return Buffer.from(JSON.stringify({ ns: NS, exp: now() + 3600, subject: 'x' })).toString('hex') + '.' + s; })();
    const wn = hmacToken(SECRET, { ns: 'other', exp: now() + 3600, subject: 'agent-1' });
    const a = await hit(main, { ...A(), headers: bearer(tampered) });
    const b = await hit(main, { ...A(), headers: bearer(wn) });
    R.art('D22', 'invalid', J(a.rec)); R.art('D22', 'wrong_namespace', J(b.rec));
    R.check('D22', a.rec.outcome === 'rejected_identity' && a.rec.kind === 'invalid', 'tampered hmac -> invalid');
    R.check('D22', b.rec.outcome === 'rejected_identity' && b.rec.kind === 'wrong_namespace', 'wrong ns -> wrong_namespace');
    const q = main.janus('log', 'query', '--outcome', 'rejected_identity');
    const qj = main.json('log', 'query', '--outcome', 'rejected_identity', '--limit', '1000');
    const kinds = new Set((qj.requests || []).map((x) => x.kind));
    R.art('D22', 'log-query-human', `exit=${q.code}\n${q.out}`);
    R.art('D22', 'log-query-json-kinds', J([...kinds]));
    for (const k of ['no_material', 'invalid', 'wrong_namespace', 'expired', 'revoked'])
      R.check('D22', kinds.has(k) && (qj.requests || []).every((x) => x.outcome === 'rejected_identity'), `janus log query --outcome rejected_identity (json) has kind ${k}`);
    R.check('D22', ['no_material', 'invalid', 'wrong_namespace', 'expired', 'revoked'].every((k) => q.out.includes(k)), 'human `log query --outcome rejected_identity` output shows all five kinds');

    // D23
    pl.resolves.length = 0;
    const ev0 = pl.events.length;
    const bobTok = hmacToken(SECRET, { ns: NS, exp: now() - 50, subject: 'bob' });
    const before = up.reqs.length;
    const { r, rec } = await hit(main, { ...A(), headers: bearer(bobTok) });
    await sleep(300);
    R.art('D23', 'transcript', `status=${r.status}\nlog=${J(rec)}\nplugin.resolves=${J(pl.resolves)}\nplugin.events(new)=${J(pl.events.slice(ev0))}`);
    R.check('D23', rec.outcome === 'rejected_identity' && rec.caller_identity === '' && rec.claimed_identity === 'bob', `outcome=${rec.outcome} caller_identity=${J(rec.caller_identity)} claimed_identity=${rec.claimed_identity}`);
    R.check('D23', r.status === 401 && up.reqs.length === before, 'not forwarded');
    R.check('D23', !J(pl.resolves).includes('bob') && !J(pl.events.slice(ev0)).includes('bob') && pl.resolves.length === 0, 'plugin saw no call carrying bob');
  }

  // ---------------- D24 / D25 (claim variant) ----------------
  {
    pl.resolves.length = 0; const ev0 = pl.events.length;
    const t = OKTOK({ subject: 's1', agent_type: 'coder', session: 'sess-42' });
    const { r, rec } = await hit(main, { ...A(), headers: bearer(t) });
    await sleep(400);
    const res = pl.resolves[0] || {};
    const evs = pl.events.slice(ev0);
    R.art('D24', 'transcript', `status=${r.status}\nResolveRoute=${J(res)}\nlog=${J(rec)}`);
    R.check('D24', res.caller_identity === 's1' && res.identity_claims?.agent_type === 'coder', `ResolveRoute caller_identity=${res.caller_identity} identity_claims=${J(res.identity_claims)}`);
    R.art('D25', 'claim-variant', `ResolveRoute=${J(res)}\nNotify=${J(evs)}\nlog=${J(rec)}`);
    R.check('D25', res.correlation_id === 'sess-42' && rec.correlation_id === 'sess-42' && evs.length > 0 && evs.every((e) => e.correlation_id === 'sess-42'), 'correlation_claim: ResolveRoute, Notify and log all carry claim value sess-42');
    // neither configured: route with its own block without correlation
    pl.resolves.length = 0; const ev1 = pl.events.length;
    const n = await hit(main, { ...A('/anthropic/v1/nocorr'), headers: bearer(OKTOK({ session: 'ignored' })) });
    await sleep(400);
    R.art('D25', 'neither', `ResolveRoute=${J(pl.resolves)}\nNotify=${J(pl.events.slice(ev1))}\nlog=${J(n.rec)}`);
    R.check('D25', n.r.status === 200 && n.rec.route && !n.rec.correlation_id && !pl.resolves[0]?.correlation_id && pl.events.slice(ev1).every((e) => !e.correlation_id), `neither configured: correlation_id empty (route=${n.rec.route}, log=${J(n.rec.correlation_id)})`);
  }

  // ---------------- D21: rejection first ----------------
  {
    const cb = main.json('credential', 'show', 'a1');
    const resolves0 = pl.resolves.length, ev0 = pl.events.length, reqs0 = up.reqs.length, conn0 = up.state.connections;
    const t0 = Date.now();
    const res = await new Promise((resolve) => {
      const s = net.connect(21200, '127.0.0.1');
      let buf = '', done = false, written = 0;
      const finish = (v) => { if (!done) { done = true; clearInterval(iv); s.destroy(); resolve(v); } };
      s.on('data', (d) => { buf += d.toString(); if (buf.includes('\r\n\r\n')) finish({ head: buf.split('\r\n')[0], ms: Date.now() - t0, written }); });
      s.on('error', () => finish({ head: buf.split('\r\n')[0] || 'ERR', ms: Date.now() - t0, written }));
      s.write('POST /anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAuthorization: Bearer bogus\r\nTransfer-Encoding: chunked\r\n\r\n');
      const chunk = Buffer.alloc(65536, 0x61);
      const iv = setInterval(() => { try { s.write(`${chunk.length.toString(16)}\r\n`); s.write(chunk); s.write('\r\n'); written += chunk.length; } catch {} }, 100);
      setTimeout(() => finish({ head: buf.split('\r\n')[0] || 'TIMEOUT', ms: Date.now() - t0, written, timeout: true }), 4000);
    });
    nlog++;
    await sleep(500);
    const ca = main.json('credential', 'show', 'a1');
    R.art('D21', 'transcript', `response=${J(res)}\nplugin.resolves delta=${pl.resolves.length - resolves0}\nplugin.events delta=${pl.events.length - ev0}\nupstream reqs delta=${up.reqs.length - reqs0} connections delta=${up.state.connections - conn0}\ncred before=${J(cb)}\ncred after=${J(ca)}`);
    R.check('D21', /401/.test(res.head) && !res.timeout, `401 returned while the (endless) body was still being sent: ${res.head} after ${res.ms}ms`);
    R.check('D21', pl.resolves.length === resolves0 && pl.events.length === ev0, 'plugin saw zero ResolveRoute and zero Notify');
    R.check('D21', up.reqs.length === reqs0 && up.state.connections === conn0, 'upstream saw no request/connection');
    R.check('D21', ca.request_count === cb.request_count && ca.last_used_at === cb.last_used_at, `credential request_count (${cb.request_count}->${ca.request_count}) / last_used_at unchanged`);
  }

  // ---------------- D20: method=none ----------------
  {
    pl.resolves.length = 0; const ev0 = pl.events.length;
    const before = up.reqs.length;
    const { r, rec } = await hit(main, { method: 'GET', path: '/openai/v1/models', headers: {} });
    await sleep(400);
    const o1 = up.reqs[before];
    R.art('D20', 'ok-case', `status=${r.status}\nlog=${J(rec)}\nupstream saw headers=${hdrStr(o1?.headers)}\nResolveRoute=${J(pl.resolves)}\nNotify=${J(pl.events.slice(ev0))}`);
    R.check('D20', r.status === 200 && o1 && J(o1.headers).includes('sk-pool-openai-OOOO'), 'forwarded with default-chain pool credential');
    R.check('D20', pl.resolves.length === 0, 'plugin ResolveRoute calls = 0');
    R.check('D20', !pl.events.slice(ev0).some((e) => e.event_type === 'call_completed' || e.event_type === 'call_abandoned'), 'no call_completed/call_abandoned for it');
    R.check('D20', rec.caller_identity === '' && rec.binding === 'none' && rec.outcome === 'forwarded', `log caller_identity=${J(rec.caller_identity)} binding=${rec.binding}`);
    // positive control: identity-bearing call does emit call_completed
    const c = main.json('identity', 'create', '--identity', 'ctl');
    const ev1 = pl.events.length;
    await hit(main, { ...OAI(), headers: { 'x-api-key': c.token } });
    await sleep(400);
    R.check('D20', pl.events.slice(ev1).some((e) => e.event_type === 'call_completed'), 'control: an identity-bearing call does produce call_completed (so absence above is meaningful)');
    // rejection case
    // OpenAI's real invalid-key error shape (a bare 401 with an unrecognised body was observed NOT to invalidate the credential)
    up.state.status = 401; up.state.body = '{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}';
    pl.resolves.length = 0; const ev2 = pl.events.length;
    const rj = await hit(main, { method: 'GET', path: '/openai/v1/models', headers: {} });
    await sleep(800);
    up.state.status = 200; up.state.body = '{"ok":1}';
    const evs = pl.events.slice(ev2);
    R.art('D20', 'rejection-case', `status=${rj.r.status}\nlog=${J(rj.rec)}\nResolveRoute=${J(pl.resolves)}\nNotify=${J(evs)}`);
    R.check('D20', pl.resolves.length === 0 && evs.some((e) => e.event_type === 'credential_invalid'), `plugin got credential_invalid (${evs.map((e) => e.event_type)}) with zero ResolveRoute`);
    R.check('D20', !evs.some((e) => e.event_type === 'call_completed' || e.event_type === 'call_abandoned'), 'still no call_completed/call_abandoned in rejection case');
    R.check('D20', rj.rec.caller_identity === '' && rj.rec.binding === 'none', 'rejection-case log: empty caller_identity, binding none');
  }
  main.stop();

  // =====================================================================
  // D12 jwt
  // =====================================================================
  {
    const kp = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const other = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const pem = `${SCRATCH}/jwt-pub.pem`;
    fs.writeFileSync(pem, kp.publicKey.export({ type: 'spki', format: 'pem' }));
    const jwk = { ...kp.publicKey.export({ format: 'jwk' }), kid: 'k1', alg: 'RS256', use: 'sig' };
    const jwks = http_server_jwks(jwk);
    await jwks.ready; cleanup.push(jwks.close);
    const jup = await startMock(21101); cleanup.push(jup.close);
    const block = (extra) => `method="jwt"\nsource={ header="authorization", scheme="Bearer " }\nissuer="https://issuer.example"\naudience="janus-test"\nidentity_claim="sub"\n${extra}`;
    const d = await startDaemon('jwt', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21201"
${UP('anthropic', 21101, `[upstream.identity]\n${block('jwks_url="http://127.0.0.1:21400/jwks"')}`)}
${UP('openai', 21101, `[upstream.identity]\n${block(`public_key_file="${pem}"`)}`)}
`);
    cleanup.push(() => d.stop());
    d.addKey('anthropic', 'a1', 'sk-j-a'); d.addKey('openai', 'o1', 'sk-j-o');
    const claims = (o = {}) => ({ iss: 'https://issuer.example', aud: 'janus-test', sub: 'jwt-user-1', exp: now() + 3600, iat: now(), ...o });
    let n = 0;
    const go = async (prefix, token) => { const r = await request({ port: 21201, method: 'POST', path: `/${prefix}/v1/x`, headers: { authorization: `Bearer ${token}` }, body: '{}' }); n++; const l = await d.waitLogs(n); return { r, rec: l[0] }; };
    for (const [prefix, label] of [['anthropic', 'JWKS'], ['openai', 'static-public-key']]) {
      const before = jup.reqs.length;
      const a = await go(prefix, jwt(kp.privateKey, claims()));
      R.art('D12', `${label}-a-valid`, `status=${a.r.status}\n${J(a.rec)}`);
      R.check('D12', a.r.status === 200 && a.rec.binding === 'jwt' && a.rec.caller_identity === 'jwt-user-1' && jup.reqs.length === before + 1, `[${label}] valid token forwarded, binding=${a.rec.binding}, caller_identity=${a.rec.caller_identity}`);
      const bad = [['b-wrong-aud', claims({ aud: 'nope' }), kp.privateKey, null], ['c-wrong-iss', claims({ iss: 'https://evil.example' }), kp.privateKey, 'wrong_namespace'], ['d-other-key', claims(), other.privateKey, 'invalid']];
      for (const [nm, cl, key, want] of bad) {
        const b4 = jup.reqs.length;
        const x = await go(prefix, jwt(key, cl));
        R.art('D12', `${label}-${nm}`, `status=${x.r.status}\n${J(x.rec)}`);
        R.check('D12', x.r.status === 401 && jup.reqs.length === b4 && x.rec.outcome === 'rejected_identity' && x.rec.kind && x.rec.kind !== 'no_material' && (!want || x.rec.kind === want), `[${label}] ${nm}: 401, not forwarded, kind=${x.rec.kind}${want ? ` (want ${want})` : ''}`);
      }
    }
    d.stop();
  }

  // =====================================================================
  // D15 trusted_header
  // =====================================================================
  {
    const tup = await startMock(21102); cleanup.push(tup.close);
    const tb = await startDaemon('tb-a', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21202"
[identity]
method="trusted_header"
source={ header="x-caller" }
${UP('anthropic', 21102)}
`);
    cleanup.push(() => tb.stop());
    tb.addKey('anthropic', 'a1', 'sk-tb');
    const r = await request({ port: 21202, method: 'POST', path: '/anthropic/v1/m', headers: { 'x-caller': 'alice' }, body: '{}' });
    const l = await tb.waitLogs(1);
    R.art('D15', 'a-explicit', `status=${r.status}\n${J(l[0])}`);
    R.check('D15', r.status === 200 && l[0].caller_identity === 'alice' && l[0].binding === 'trusted_header', `(a) explicit trusted_header: caller_identity=${l[0].caller_identity} binding=${l[0].binding}`);
    tb.stop();

    // (b) method omitted
    const bd = await startDaemon('tb-b', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21203"
${BASE_ID}
[identity]
source={ header="x-caller" }
${UP('anthropic', 21102)}
`, { expectFail: true });
    let bOK;
    if (bd.failed) {
      R.art('D15', 'b-omitted-method', `daemon refused config at startup, exit=${bd.exitCode}\n${bd.log}`);
      bOK = true;
    } else {
      bd.addKey('anthropic', 'a1', 'sk-tb');
      const before = tup.reqs.length;
      const rb = await request({ port: 21203, method: 'POST', path: '/anthropic/v1/m', headers: { 'x-caller': 'alice' }, body: '{}' });
      const lb = await bd.waitLogs(1);
      R.art('D15', 'b-omitted-method', `daemon started; status=${rb.status}\n${J(lb)}`);
      bOK = tup.reqs.length === before && !lb.some((x) => x.binding === 'trusted_header' || x.caller_identity === 'alice');
      bd.stop();
    }
    R.check('D15', bOK, '(b) method omitted: never forwarded as alice / no trusted_header record');

    // (c) upstream override of only source over an hmac default
    const cd = await startDaemon('tb-c', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21204"
${BASE_ID}
${HMAC_DEFAULT()}
${UP('anthropic', 21102, `[upstream.identity]\nsource={ header="x-caller" }`)}
`, { expectFail: true });
    let cOK;
    if (cd.failed) {
      R.art('D15', 'c-source-override', `daemon refused config, exit=${cd.exitCode}\n${cd.log}`);
      cOK = true;
    } else {
      cd.addKey('anthropic', 'a1', 'sk-tb');
      const before = tup.reqs.length;
      const rc = await request({ port: 21204, method: 'POST', path: '/anthropic/v1/m', headers: { 'x-caller': 'alice' }, body: '{}' });
      const lc = await cd.waitLogs(1);
      R.art('D15', 'c-source-override', `daemon started; status=${rc.status}\n${J(lc)}`);
      cOK = tup.reqs.length === before && rc.status === 401 && !lc.some((x) => x.binding === 'trusted_header' || x.caller_identity === 'alice');
      cd.stop();
    }
    R.check('D15', cOK, '(c) source-only upstream override over hmac default: never forwarded as alice / no trusted_header record');
  }

  // =====================================================================
  // D16 sources
  // =====================================================================
  {
    const sup = await startMock(21103); cleanup.push(sup.close);
    const route = (p, src) => `[[upstream.route]]\npath="${p}"\nidentity={ method="hmac", source=${src}, secret_ref="caller-identity", namespace="${NS}", identity_claim="subject" }\n`;
    const d = await startDaemon('src', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21205"
${BASE_ID}
${HMAC_DEFAULT()}
${UP('anthropic', 21103)}
${UP('openai', 21103, [route('/v1/hdr', '{ header="x-api-key" }'), route('/v1/qry', '{ query="key" }'), route('/v1/basic', '"basic_password"')].join('\n'))}
`);
    cleanup.push(() => d.stop());
    d.addKey('anthropic', 'a1', 'sk-src-a'); d.addKey('openai', 'o1', 'sk-src-o');
    const tok = OKTOK({ subject: 'src-user' });
    let n = 0;
    const go = async (o) => { const b4 = sup.reqs.length; const r = await request({ port: 21205, method: 'POST', body: '{}', ...o }); n++; const l = await d.waitLogs(n); return { r, rec: l[0], out: sup.reqs.length > b4 ? sup.reqs[sup.reqs.length - 1] : null }; };
    const basic = Buffer.from(`user:${tok}`).toString('base64');
    const cases = [
      ['header+scheme', { path: '/anthropic/v1/m', headers: { authorization: `Bearer ${tok}` } }],
      ['header-noscheme', { path: '/openai/v1/hdr', headers: { 'x-api-key': tok } }],
      ['query', { path: `/openai/v1/qry?key=${tok}&other=1`, headers: {} }],
      ['basic_password', { path: '/openai/v1/basic', headers: { authorization: `Basic ${basic}` } }],
    ];
    for (const [nm, o] of cases) {
      const x = await go(o);
      const leaked = x.out ? leaks(x.out, tok) || J(x.out.headers).includes(basic) : null;
      R.art('D16', nm, `status=${x.r.status}\nlog=${J(x.rec)}\noutbound url=${x.out?.url}\noutbound headers=${J(x.out?.headers)}`);
      R.check('D16', x.r.status === 200 && x.rec.caller_identity === 'src-user' && x.rec.outcome === 'forwarded', `${nm}: forwarded with caller_identity=${x.rec.caller_identity}`);
      R.check('D16', x.out && !leaked, `${nm}: outbound request carries no identity material`);
    }
    // misplaced
    for (const [nm, o] of [
      ['hdr-route gets Bearer', { path: '/openai/v1/hdr', headers: { authorization: `Bearer ${tok}` } }],
      ['query-route gets header', { path: '/openai/v1/qry', headers: { 'x-api-key': tok } }],
      ['basic-route gets x-api-key', { path: '/openai/v1/basic', headers: { 'x-api-key': tok } }],
      ['bearer-binding gets query', { path: `/anthropic/v1/m?key=${tok}`, headers: {} }],
    ]) {
      const b4 = sup.reqs.length;
      const x = await go(o);
      R.art('D16', 'misplaced-' + nm.replaceAll(' ', '_'), `status=${x.r.status}\nlog=${J(x.rec)}`);
      R.check('D16', x.r.status === 401 && x.rec.kind === 'no_material' && sup.reqs.length === b4, `misplaced (${nm}): 401 kind=${x.rec.kind}`);
    }
    d.stop();
  }

  // =====================================================================
  // D17 / D18 precedence and merge
  // =====================================================================
  {
    const pup = await startMock(21104); cleanup.push(pup.close);
    const d = await startDaemon('prec', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21206"
${BASE_ID}
${HMAC_DEFAULT()}
${UP('anthropic', 21104)}
${UP('openai', 21104, `[upstream.identity]\nsource={ header="x-api-key" }\n[[upstream.route]]\npath="/v1/models"\nidentity={ method="opaque", source={ header="x-api-key" } }`)}
`);
    cleanup.push(() => d.stop());
    d.addKey('anthropic', 'a1', 'sk-p-a'); d.addKey('openai', 'o1', 'sk-p-o');
    const hm = OKTOK({ subject: 'prec-user' });
    const op = d.json('identity', 'create', '--identity', 'opaque-user').token;
    let n = 0;
    const go = async (o) => { const b4 = pup.reqs.length; const r = await request({ port: 21206, method: 'POST', body: '{}', ...o }); n++; const l = await d.waitLogs(n); return { r, rec: l[0], fwd: pup.reqs.length > b4 }; };
    const show = (nm, x) => R.art('D17', nm, `status=${x.r.status} fwd=${x.fwd}\n${J(x.rec)}`);
    const a = await go({ path: '/openai/v1/models', headers: { 'x-api-key': op } });
    const b = await go({ path: '/openai/v1/chat', headers: { 'x-api-key': hm } });
    const c = await go({ path: '/anthropic/v1/messages', headers: { authorization: `Bearer ${hm}` } });
    show('route', a); show('upstream-override', b); show('daemon-default', c);
    R.check('D17', a.fwd && a.rec.binding === 'opaque' && a.rec.route === '/v1/models', `route request: binding=${a.rec.binding} route=${a.rec.route}`);
    R.check('D17', b.fwd && b.rec.binding === 'hmac' && !b.rec.route && b.rec.caller_identity === 'prec-user', `other openai path (upstream override x-api-key hmac): binding=${b.rec.binding} route=${b.rec.route}`);
    R.check('D17', c.fwd && c.rec.binding === 'hmac' && !c.rec.route && c.rec.caller_identity === 'prec-user', `anthropic path (daemon default): binding=${c.rec.binding}`);
    // D18
    const e1 = await go({ path: '/openai/v1/chat', headers: { 'x-api-key': hm } });
    const e2 = await go({ path: '/openai/v1/chat', headers: { authorization: `Bearer ${hm}` } });
    const e3 = await go({ path: '/openai/v1/models', headers: { 'x-api-key': op } });
    const e4 = await go({ path: '/openai/v1/models', headers: { 'x-api-key': hm } });
    const e5 = await go({ path: '/openai/v1/models', headers: { authorization: `Bearer ${hm}` } });
    for (const [nm, x] of [['x-api-key-hmac', e1], ['authorization-hmac', e2], ['route-opaque', e3], ['route-hmac-xapikey', e4], ['route-hmac-bearer', e5]]) R.art('D18', nm, `status=${x.r.status} fwd=${x.fwd}\n${J(x.rec)}`);
    R.check('D18', e1.fwd && e1.rec.binding === 'hmac', 'upstream override: x-api-key hmac token verifies (binding hmac; method/secret/ns inherited)');
    R.check('D18', !e2.fwd && e2.r.status === 401 && e2.rec.kind === 'no_material', `upstream override: authorization token -> 401 ${e2.rec.kind}`);
    R.check('D18', e3.fwd && e3.rec.binding === 'opaque', 'route: opaque token verifies');
    R.check('D18', !e4.fwd && e4.r.status === 401, `route replaces (no merge): hmac token in x-api-key rejected (${e4.rec.kind})`);
    R.check('D18', !e5.fwd && e5.r.status === 401 && e5.rec.kind === 'no_material', 'route: hmac Bearer token rejected (no_material)');
    d.stop();
  }

  // =====================================================================
  // D19 no binding
  // =====================================================================
  {
    const nup = await startMock(21105); cleanup.push(nup.close);
    const d = await startDaemon('noid', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21207"
${UP('anthropic', 21105)}
`, { expectFail: true });
    if (d.failed) {
      R.art('D19', 'transcript', `daemon refused to start: exit=${d.exitCode}\n${d.log}`);
      R.check('D19', true, 'daemon refuses a config where no binding resolves (nothing is served as an open route)');
    } else {
      cleanup.push(() => d.stop());
      d.addKey('anthropic', 'a1', 'sk-noid');
      const cb = d.json('credential', 'show', 'a1');
      const outs = [];
      for (const h of [{}, { authorization: 'Bearer anything' }, { 'x-api-key': 'anything' }]) outs.push(await request({ port: 21207, method: 'POST', path: '/anthropic/v1/m', headers: h, body: '{}' }));
      await sleep(400);
      const ca = d.json('credential', 'show', 'a1');
      const l = d.logs();
      R.art('D19', 'transcript', `statuses=${J(outs.map((o) => o.status))}\nbodies=${J(outs.map((o) => o.body.toString()))}\nlog=${J(l)}\ncred before=${J(cb)} after=${J(ca)}`);
      R.check('D19', outs.every((o) => o.status >= 400) && nup.reqs.length === 0, `no binding: all requests rejected (${outs.map((o) => o.status)}), upstream saw ${nup.reqs.length} requests`);
      R.check('D19', ca.request_count === cb.request_count, 'no credential spent');
      R.check('D19', !l.some((x) => x.binding === 'none' && x.outcome === 'forwarded'), 'never behaves as method=none');
    }
    d.stop?.();
  }

  // =====================================================================
  // D25 header variant
  // =====================================================================
  {
    const cup = await startMock(21106); cleanup.push(cup.close);
    const cpl = await startPlugin('127.0.0.1:21301'); cleanup.push(cpl.close);
    const d = await startDaemon('corr', `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:21208"
[plugin]
endpoint="http://127.0.0.1:21301"
allow_plaintext=true
${BASE_ID}
${HMAC_DEFAULT('correlation={ from_header="x-request-id" }')}
${UP('anthropic', 21106)}
`);
    cleanup.push(() => d.stop());
    d.addKey('anthropic', 'a1', 'sk-c');
    let n = 0;
    const go = async (h) => { const r = await request({ port: 21208, method: 'POST', path: '/anthropic/v1/m', headers: { authorization: `Bearer ${OKTOK({ subject: 'same-subject', session: 'claim-session' })}`, ...h }, body: '{}' }); n++; const l = await d.waitLogs(n); await sleep(300); return { r, rec: l[0] }; };
    const x1 = await go({ 'x-request-id': 'req-AAA' });
    const x2 = await go({ 'x-request-id': 'req-BBB' });
    const x3 = await go({});
    R.art('D25', 'header-variant', `ResolveRoute=${J(cpl.resolves)}\nNotify=${J(cpl.events)}\nlogs=${J([x1.rec, x2.rec, x3.rec])}`);
    const ids = cpl.resolves.map((r) => r.correlation_id);
    R.check('D25', J(ids) === J(['req-AAA', 'req-BBB', '']), `ResolveRoute correlation_ids = ${J(ids)}`);
    R.check('D25', x1.rec.correlation_id === 'req-AAA' && x2.rec.correlation_id === 'req-BBB' && !x3.rec.correlation_id, 'log correlation_id equals header (empty when absent)');
    const comp = cpl.events.filter((e) => e.event_type === 'call_completed');
    R.check('D25', comp.length === 3 && J(comp.map((e) => e.correlation_id)) === J(['req-AAA', 'req-BBB', '']), `Notify call_completed correlation_ids = ${J(comp.map((e) => e.correlation_id))}`);
    R.check('D25', cpl.resolves.every((r) => r.caller_identity === 'same-subject') && [x1, x2, x3].every((x) => x.rec.caller_identity === 'same-subject' && x.rec.credential_ref === 'a1'), 'changing the header alone never changes caller_identity or the decision');
    d.stop();
  }

  // =====================================================================
  // D26 custom extractor (scratch custom build depending on janus-daemon by path)
  // =====================================================================
  {
    const crate = `${REPO}/evidence/acceptance/b-custom-daemon`;
    const tgt = '/tmp/qa-target-b-custom';
    const bld = spawnSync('cargo', ['build', '--release', '--manifest-path', `${crate}/Cargo.toml`], { env: { ...process.env, CARGO_TARGET_DIR: tgt }, encoding: 'utf8' });
    R.art('D26', 'custom-build', `cargo build exit=${bld.status}\n${(bld.stderr || '').split('\n').slice(-5).join('\n')}`);
    R.check('D26', bld.status === 0, 'scratch custom daemon (Embedded { extractors: vec![introspect] }) builds against the public API');
    const cbin = `${tgt}/release/qa-custom-daemon`;
    const cup = await startMock(21107); cleanup.push(cup.close);
    const cfg = (bind, method) => `
data_dir="@DATA@"
[data_plane]
bind="127.0.0.1:${bind}"
${UP('anthropic', 21107, `[upstream.identity]\nmethod="${method}"`)}
`;
    const d = await startDaemon('custom-ok', cfg(21209, 'custom:introspect'), { bin: cbin });
    cleanup.push(() => d.stop());
    d.addKey('anthropic', 'a1', 'sk-custom');
    let n = 0;
    const go = async (h) => { const b4 = cup.reqs.length; const r = await request({ port: 21209, method: 'POST', path: '/anthropic/v1/m', headers: h, body: '{}' }); n++; const l = await d.waitLogs(n); return { r, rec: l[0], fwd: cup.reqs.length > b4 }; };
    const ok = await go({ 'x-introspect': 'good-zed' });
    const bad = await go({ 'x-introspect': 'nope' });
    const none = await go({});
    R.art('D26', 'custom-extractor', `accept: status=${ok.r.status} ${J(ok.rec)}\nreject-invalid: status=${bad.r.status} ${J(bad.rec)}\nreject-nomaterial: status=${none.r.status} ${J(none.rec)}`);
    R.check('D26', ok.fwd && ok.r.status === 200 && ok.rec.binding === 'custom:introspect' && ok.rec.caller_identity === 'zed', `custom accepts: binding=${ok.rec.binding} caller_identity=${ok.rec.caller_identity}`);
    R.check('D26', !bad.fwd && bad.r.status === 401 && bad.rec.kind === 'invalid' && bad.rec.binding === 'custom:introspect', `custom rejects invalid: ${bad.r.status} kind=${bad.rec.kind}`);
    R.check('D26', !none.fwd && none.r.status === 401 && none.rec.kind === 'no_material', `custom rejects missing material: ${none.r.status} kind=${none.rec.kind}`);
    d.stop();
    const m1 = await startDaemon('custom-missing-custombuild', cfg(21210, 'custom:missing'), { bin: cbin, expectFail: true });
    const m2 = await startDaemon('custom-missing-stock', cfg(21211, 'custom:introspect'), { expectFail: true });
    R.art('D26', 'refuse-to-start', `custom build + custom:missing -> failed=${m1.failed} exit=${m1.exitCode}\n${m1.log || ''}\nstock build + custom:introspect (not registered) -> failed=${m2.failed} exit=${m2.exitCode}\n${m2.log || ''}`);
    m1.stop?.(); m2.stop?.();
    R.check('D26', m1.failed && m1.exitCode !== 0, 'config naming custom:missing: daemon refuses to start (custom build)');
    R.check('D26', m2.failed && m2.exitCode !== 0, 'stock daemon refuses a registered-nowhere custom method too');
  }
}

function http_server_jwks(jwk) {
  // eslint-disable-next-line
  const httpMod = globalThis.__http || (globalThis.__http = null);
  const srvP = import('node:http').then(({ default: http }) => new Promise((res) => {
    const s = http.createServer((q, r) => { r.setHeader('content-type', 'application/json'); r.end(JSON.stringify({ keys: [jwk] })); });
    s.listen(21400, '127.0.0.1', () => res(s));
  }));
  return { ready: srvP, close: () => srvP.then((s) => s.close()) };
}

let failed = false;
try {
  await main();
} catch (e) {
  console.error('HARNESS ERROR', e);
  failed = true;
  R.check('D10', false, 'harness error: ' + e.message);
} finally {
  for (const c of cleanup.reverse()) { try { await c(); } catch {} }
}
const out = R.finish(IDS);
for (const o of out) console.log(`${o.id} ${o.result}  ${o.note.slice(0, 200)}`);
const bad = out.filter((o) => o.result !== 'PASS');
if (bad.length || failed) { console.log(`B FAIL (${bad.map((b) => `${b.id}:${b.result}`).join(', ')})`); process.exit(1); }
console.log('B PASS');
process.exit(0);
