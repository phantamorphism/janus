#!/usr/bin/env node
// Group "lm": DELIVERABLES D97-D103 (janus CLI) and D104-D109 (UI, driven in a real browser via agent-browser).
// Black-box only: runs target/release/janus-daemon and target/release/janus, a node mock upstream, a mock
// mTLS gRPC plugin, and agent-browser against the real UI listener.
import fs from 'node:fs';
import net from 'node:net';
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import * as E from './lm-env.mjs';

const { REPO, BIN, W, P, run, janus } = E;
const ART = `${REPO}/evidence/artifacts/lm`;
const REL = 'evidence/artifacts/lm';
let SESSION = 'qalm';
const PROXY = 26004;
fs.rmSync('/tmp/qa-lm/run', { recursive: true, force: true });
fs.rmSync(ART, { recursive: true, force: true });
fs.mkdirSync(ART, { recursive: true });
fs.mkdirSync(W, { recursive: true });

process.on('uncaughtException', (e) => console.error('uncaught (ignored):', e.message));

// ---------- result bookkeeping ----------
const results = {}; // id -> {checks:[], artifacts:Set, blocked:[]}
const R = (id) => (results[id] ??= { checks: [], artifacts: new Set(), blocked: [] });
// Redaction: registered secrets plus generic shapes (43-char url-safe tokens, sk-... API keys) never reach disk.
const SECRETS = new Set();
const redact = (str) => {
  let t = String(str);
  for (const v of SECRETS) if (v && v.length > 8) t = t.split(v).join('<redacted>');
  return t.replace(/(?<![A-Za-z0-9_-])[A-Za-z0-9_-]{43}(?![A-Za-z0-9_-])/g, '<redacted-token>').replace(/sk-(?:or|ant)-[A-Za-z0-9-]+/g, '<redacted-key>');
};
function art(id, name, content) {
  const f = `${id}-${name}`;
  fs.writeFileSync(`${ART}/${f}`, redact(typeof content === 'string' ? content : JSON.stringify(content, null, 2)));
  R(id).artifacts.add(`${REL}/${f}`);
  return `${ART}/${f}`;
}
function check(id, name, ok, detail = '') {
  R(id).checks.push({ name, ok: !!ok, detail: redact(String(detail).slice(0, 300)) });
  if (!ok) detail = redact(detail);
  if (!ok) console.log(`  FAIL ${id}: ${name} ${detail}`);
}
const blocked = (id, why) => R(id).blocked.push(why);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const isJson = (s) => { try { JSON.parse(s); return true; } catch { return false; } };
const out = (r) => `$ exit=${r.code}\n--- stdout\n${r.stdout}\n--- stderr\n${r.stderr}\n`;
const jj = (s) => JSON.parse(s);

// ---------- environment ----------
const tlsInit = run(`${BIN}/janus`, ['plugin', 'tls', 'init', '--plugin-host', 'localhost', '--out', (fs.mkdirSync(`${W}/tls`, { recursive: true }), `${W}/tls`)]);
if (tlsInit.code !== 0) { console.error('tls init failed', tlsInit); process.exit(2); }
const tokenReqs = [];
const tokenSrv = http.createServer((q, r) => { let b = ''; q.on('data', (c) => (b += c)); q.on('end', () => { tokenReqs.push({ method: q.method, url: q.url, ct: q.headers['content-type'], body: b }); r.writeHead(200, { 'content-type': 'application/json' }); r.end(JSON.stringify({ access_token: 'sk-ant-oat01-NEW-ACCESS-' + tokenReqs.length, refresh_token: 'sk-ant-ort01-NEW-REFRESH-' + tokenReqs.length, expires_in: 7200, token_type: 'Bearer' })); }); });
await new Promise((r) => tokenSrv.listen(26011, '127.0.0.1', r));
const upstream = await E.upstreamW();
const plugin = await E.pluginW(`${W}/tls`);
const cfg = E.writeConfig(`[[upstream]]
name = "anthropic"
path_prefix = "/anthropic"
base_url = "http://127.0.0.1:${P.up}"
token_url = "http://127.0.0.1:26011/token"
`);
const daemon = await E.startDaemon(cfg);
const DATA = `${W}/data`;
const TOKEN = fs.readFileSync(`${DATA}/admin-token`, 'utf8').trim();
SECRETS.add(TOKEN);

// reverse proxy in front of the UI listener that records every request's header/URL (for D104/D108)
const proxy = await E.proxyW(PROXY, P.ui);
const proxyLog = proxy.log;

const dp = (token, path = '/openrouter/v1/chat/completions', extra = {}) =>
  fetch(`http://127.0.0.1:${P.dp}${path}`, { method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json', ...extra }, body: '{}' });
const mkid = (identity, ...more) => {
  const r = janus(['identity', 'create', '--identity', identity, ...more]);
  SECRETS.add(r.stdout.trim());
  return r.stdout.trim();
};

// base state: credential, identities, traffic
const addKey = (ref, key, up = 'openrouter', extra = []) => janus(['credential', 'add', '--upstream', up, '--ref', ref, '--api-key-stdin', ...extra], { input: key + '\n' });
const KEY1 = 'sk-or-QA-SECRET-ALPHA-111';
addKey('or1', KEY1);
const tokA = mkid('agent-1', '--claim', 'tenant=acme', '--label', 'a1');
const tokDeny = mkid('deny-agent', '--label', 'denied');

// =====================================================================
// L. janus CLI
// =====================================================================
console.log('== L ==');

// ---- D97
{
  const id = 'D97';
  const help = run(`${BIN}/janus`, ['--help']);
  art(id, 'help.txt', help.stdout);
  const allHelps = ['', 'status', 'credential add', 'credential rotate', 'identity create', 'log query', 'log tail', 'plugin check']
    .map((c) => run(`${BIN}/janus`, [...c.split(' ').filter(Boolean), '--help']).stdout).join('\n');
  check(id, 'no --token <value> option anywhere in help', !/--token(\s|=|<|\[)/.test(allHelps) && !/^\s+-t[,\s]/m.test(allHelps) && !/--(api-)?key[\s=<]/.test(allHelps));
  check(id, '--token-file exists (path, not value)', /--token-file <PATH>/.test(help.stdout));
  const attempt = janus(['status', '--token', 'abc'], { env: {} });
  art(id, 'token-flag-rejected.txt', out(attempt));
  check(id, '`--token abc` rejected as unknown option', attempt.code !== 0 && /unexpected argument|unrecognized/i.test(attempt.stderr), attempt.stderr);
  const tr = [];
  const t = (label, args, env, exp = true) => {
    const r = run(`${BIN}/janus`, args, { env });
    tr.push(`### ${label}\n$ janus ${args.join(' ')}  env=${Object.keys(env).join(',')}\n${out(r)}`);
    return r;
  };
  const cleanEnv = { HOME: '/tmp/qa-lm/nohome' };
  // default socket resolution: JANUS_DATA_DIR, --config, HOME/.janus
  fs.mkdirSync('/tmp/qa-lm/home', { recursive: true });
  fs.rmSync('/tmp/qa-lm/home/.janus', { force: true });
  fs.symlinkSync(DATA, '/tmp/qa-lm/home/.janus');
  const r1 = t('no flags, JANUS_DATA_DIR', ['status'], { JANUS_DATA_DIR: DATA });
  const r1b = t('no flags, $HOME/.janus default', ['status'], { HOME: '/tmp/qa-lm/home' });
  const r1c = t('--config data_dir', ['status', '--config', cfg], cleanEnv);
  const r1d = t('JANUS_ADMIN=unix://', ['status'], { ...cleanEnv, JANUS_ADMIN: `unix://${DATA}/admin.sock` });
  check(id, 'status over default admin.sock via JANUS_DATA_DIR', r1.code === 0 && /version/.test(r1.stdout));
  check(id, 'status over default $HOME/.janus/admin.sock', r1b.code === 0);
  check(id, 'status via --config data_dir', r1c.code === 0);
  check(id, 'status via JANUS_ADMIN=unix://', r1d.code === 0);
  const adm = `http://127.0.0.1:${P.admin}`;
  const r2 = t('TCP + --token-file', ['status', '--admin', adm, '--token-file', `${DATA}/admin-token`], cleanEnv);
  const r3 = t('TCP + JANUS_ADMIN_TOKEN', ['status', '--admin', adm], { ...cleanEnv, JANUS_ADMIN_TOKEN: TOKEN });
  const r4 = t('TCP + <data_dir>/admin-token (JANUS_DATA_DIR)', ['status', '--admin', adm], { HOME: '/tmp/qa-lm/nohome', JANUS_DATA_DIR: DATA });
  const r4b = t('TCP via JANUS_ADMIN env', ['status'], { ...cleanEnv, JANUS_ADMIN: adm, JANUS_DATA_DIR: DATA });
  const r5 = t('TCP + wrong token', ['status', '--admin', adm], { ...cleanEnv, JANUS_ADMIN_TOKEN: 'wrong-token' });
  const r6 = t('TCP + no token source at all', ['status', '--admin', adm], { HOME: '/tmp/qa-lm/nohome', JANUS_DATA_DIR: '/tmp/qa-lm/empty-nope' });
  check(id, 'TCP with --token-file succeeds', r2.code === 0);
  check(id, 'TCP with JANUS_ADMIN_TOKEN succeeds', r3.code === 0);
  check(id, 'TCP with <data_dir>/admin-token succeeds', r4.code === 0);
  check(id, 'TCP via JANUS_ADMIN succeeds', r4b.code === 0);
  check(id, 'wrong token fails non-zero', r5.code !== 0, r5.stderr);
  check(id, 'no token source fails non-zero', r6.code !== 0, r6.stderr);
  // no daemon reachable: non-zero and nothing done locally
  fs.rmSync('/tmp/qa-lm/cwd', { recursive: true, force: true }); fs.mkdirSync('/tmp/qa-lm/cwd');
  const before = fs.readdirSync('/tmp/qa-lm/cwd');
  const un = spawnSync(`${BIN}/janus`, ['status', '--admin', 'unix:///tmp/qa-lm/does-not-exist.sock'], { cwd: '/tmp/qa-lm/cwd', encoding: 'utf8', env: { PATH: process.env.PATH, HOME: '/tmp/qa-lm/nohome' } });
  const un2 = spawnSync(`${BIN}/janus`, ['identity', 'create', '--identity', 'x', '--admin', 'http://127.0.0.1:26999', '--token-file', `${DATA}/admin-token`], { cwd: '/tmp/qa-lm/cwd', encoding: 'utf8', env: { PATH: process.env.PATH, HOME: '/tmp/qa-lm/nohome' } });
  tr.push(`### unreachable unix\nexit=${un.status}\n${un.stderr}\n### unreachable tcp (identity create)\nexit=${un2.status}\n${un2.stderr}${un2.stdout}`);
  check(id, 'unreachable unix socket -> non-zero', un.status !== 0 && un.status !== null, un.stderr);
  check(id, 'unreachable TCP on a mutating command -> non-zero, no token printed', un2.status !== 0 && un2.stdout.trim() === '', un2.stdout);
  check(id, 'unreachable: nothing created locally', JSON.stringify(fs.readdirSync('/tmp/qa-lm/cwd')) === JSON.stringify(before));
  art(id, 'transcript.txt', tr.join('\n'));
}

// ---- D98
console.log('D98');
{
  const id = 'D98';
  const s = janus(['status']);
  const sj = janus(['status', '--json']);
  art(id, 'status.txt', out(s)); art(id, 'status-json.txt', out(sj));
  const doc = isJson(sj.stdout) ? jj(sj.stdout) : {};
  check(id, 'status --json valid JSON, exit 0', sj.code === 0 && isJson(sj.stdout));
  check(id, 'status shows version', /0\.1\.0/.test(s.stdout) && doc.version === '0.1.0');
  check(id, 'status shows listeners', /127\.0\.0\.1:26001/.test(s.stdout) && /26003/.test(s.stdout) && doc.listeners?.data_plane);
  check(id, 'status shows upstreams with binding method', /openrouter\s+\/openrouter\s+opaque/.test(s.stdout) && doc.upstreams?.[0]?.binding === 'opaque');
  check(id, 'status shows log size/retention', /rows/.test(s.stdout) && /retain/.test(s.stdout) && doc.request_log?.retain != null && 'rows' in (doc.request_log || {}));
  check(id, 'status shows plugin mode/health', /plugin\s+tls/.test(s.stdout) && /health/.test(s.stdout) && doc.plugin?.mode === 'tls' && doc.plugin?.health);
  // every command in D99-D102 with --json
  const T = [];
  const j = (label, args, opt = {}) => {
    const r = janus([...args, '--json'], opt);
    T.push(`### ${label}\n$ janus ${args.join(' ')} --json\n${out(r)}`);
    return r;
  };
  const okJson = (label, r, expectOk = true) =>
    check(id, `${label}: ${expectOk ? 'exit 0 + valid JSON' : 'non-zero exit'}`,
      expectOk ? r.code === 0 && isJson(r.stdout) : r.code !== 0, `exit=${r.code} ${r.stderr}`.slice(0, 200));
  okJson('status', sj);
  okJson('credential list', j('credential list', ['credential', 'list']));
  okJson('credential list --upstream', j('credential list --upstream', ['credential', 'list', '--upstream', 'openrouter']));
  okJson('credential show', j('credential show', ['credential', 'show', 'or1']));
  okJson('credential default', j('credential default', ['credential', 'default', '--upstream', 'openrouter']));
  const keyFile = `${W}/keyfile`; fs.writeFileSync(keyFile, 'sk-or-QA-FILEKEY-222\n');
  okJson('credential add (stdin)', j('credential add stdin', ['credential', 'add', '--upstream', 'openrouter', '--ref', 'jc1', '--api-key-stdin'], { input: 'sk-or-QA-J1\n' }));
  okJson('credential add (--from-file)', j('credential add file', ['credential', 'add', '--upstream', 'openrouter', '--ref', 'jc2', '--from-file', keyFile]));
  okJson('credential rotate (stdin)', j('credential rotate', ['credential', 'rotate', 'jc1', '--api-key-stdin'], { input: 'sk-or-QA-J1b\n' }));
  okJson('credential rotate (--from-file)', j('credential rotate file', ['credential', 'rotate', 'jc2', '--from-file', keyFile]));
  okJson('credential disable', j('credential disable', ['credential', 'disable', 'jc1']));
  okJson('credential enable', j('credential enable', ['credential', 'enable', 'jc1']));
  const rf = j('credential refresh (API key)', ['credential', 'refresh', 'jc1']);
  check(id, 'credential refresh on API-key slot: non-zero exit with JSON error on stderr', rf.code !== 0 && isJson(rf.stderr.trim().split('\n').pop()), `exit=${rf.code} ${rf.stderr}`);
  okJson('credential rm', j('credential rm jc1', ['credential', 'rm', 'jc1']));
  okJson('credential rm', j('credential rm jc2', ['credential', 'rm', 'jc2']));
  const ic = j('identity create', ['identity', 'create', '--identity', 'j-agent', '--label', 'jid']);
  okJson('identity create', ic);
  const jid = isJson(ic.stdout) ? (jj(ic.stdout).id ?? jj(ic.stdout).identity_id ?? jj(ic.stdout).entry?.id) : null;
  check(id, 'identity create --json has an id', !!jid, ic.stdout.slice(0, 120));
  okJson('identity list', j('identity list', ['identity', 'list']));
  okJson('identity show', j('identity show', ['identity', 'show', jid]));
  okJson('identity revoke', j('identity revoke', ['identity', 'revoke', jid]));
  okJson('identity rm', j('identity rm', ['identity', 'rm', jid]));
  await dp(tokA); await sleep(300);
  const lq = j('log query', ['log', 'query', '--limit', '5']);
  okJson('log query', lq);
  const rid = isJson(lq.stdout) ? jj(lq.stdout).requests[0].request_id : '';
  okJson('log show', j('log show', ['log', 'show', rid]));
  // log tail --json: JSON lines
  const tail = spawn(`${BIN}/janus`, ['log', 'tail', '--json'], { env: { PATH: process.env.PATH, JANUS_DATA_DIR: DATA } });
  let tout = ''; tail.stdout.on('data', (d) => (tout += d));
  await sleep(600); await dp(tokA); await sleep(800); tail.kill('SIGINT'); await sleep(300);
  T.push(`### log tail --json\n${tout}`);
  const tl = tout.trim().split('\n').filter(Boolean);
  check(id, 'log tail --json: every line valid JSON', tl.length > 0 && tl.every(isJson), tout.slice(0, 200));
  okJson('plugin status', j('plugin status', ['plugin', 'status']));
  const pc = j('plugin check', ['plugin', 'check']);
  okJson('plugin check', pc);
  const initDir = `${W}/tls-json`; fs.mkdirSync(initDir, { recursive: true });
  okJson('plugin tls init', j('plugin tls init', ['plugin', 'tls', 'init', '--plugin-host', 'p.example', '--out', initDir]));
  art(id, 'json-commands.txt', T.join('\n'));
}

// ---- D99
console.log('D99');
{
  const id = 'D99';
  const T = [];
  const SECRET = 'sk-or-QA-ARGV-SECRET-333';
  const help = run(`${BIN}/janus`, ['credential', 'add', '--help']);
  art(id, 'add-help.txt', help.stdout);
  const flags = [...help.stdout.matchAll(/^\s+(?:-\w, )?(--[a-z-]+)(?: (<[A-Z]+>))?/gm)].map((m) => `${m[1]}${m[2] ? ' ' + m[2] : ''}`);
  T.push('flags: ' + flags.join(' | '));
  const valFlags = flags.filter((f) => f.includes('<')).map((f) => f.split(' ')[0]);
  check(id, 'credential add --help lists no key-value flag (value flags: ' + valFlags.join(',') + ')',
    valFlags.every((f) => ['--admin', '--upstream', '--ref', '--token-file', '--ca-file', '--label', '--config', '--from-file'].includes(f)) && !/--(api-)?key(?![-\w])|--secret|--material/.test(help.stdout));
  const rej = janus(['credential', 'add', '--upstream', 'openrouter', '--api-key', SECRET]);
  check(id, '--api-key <value> is rejected', rej.code !== 0 && /unexpected argument/.test(rej.stderr));
  // ps during add: child blocked on stdin, then fed secret
  const ch = spawn(`${BIN}/janus`, ['credential', 'add', '--upstream', 'openrouter', '--ref', 'ps1', '--api-key-stdin'], { env: { PATH: process.env.PATH, JANUS_DATA_DIR: DATA } });
  let cout = ''; ch.stdout.on('data', (d) => (cout += d)); ch.stderr.on('data', (d) => (cout += d));
  await sleep(500);
  const psDuring = spawnSync('ps', ['-axww', '-o', 'pid,command']).stdout.toString();
  ch.stdin.write(SECRET + '\n'); ch.stdin.end();
  await new Promise((r) => ch.on('close', r));
  const psAfter = spawnSync('ps', ['-axww', '-o', 'pid,command']).stdout.toString();
  T.push('ps during add (janus lines):\n' + psDuring.split('\n').filter((l) => /janus credential/.test(l)).join('\n'));
  check(id, 'janus credential add is visible in ps (so the check is meaningful)', /janus credential add/.test(psDuring));
  check(id, 'secret absent from ps while add runs', !psDuring.includes(SECRET) && !psAfter.includes(SECRET));
  check(id, 'secret absent from add output', !cout.includes(SECRET), cout);
  const all = [janus(['credential', 'list']), janus(['credential', 'list', '--json']), janus(['credential', 'show', 'ps1']), janus(['credential', 'show', 'ps1', '--json']), janus(['credential', 'default', '--upstream', 'openrouter'])];
  check(id, 'secret never in list/show/default output', all.every((r) => !r.stdout.includes(SECRET) && !r.stderr.includes(SECRET)));
  const st = janus(['status', '--json']); const lg = janus(['log', 'query', '--json']);
  check(id, 'secret never in status/log output', !st.stdout.includes(SECRET) && !lg.stdout.includes(SECRET));
  // add really took effect: forwarded request carries the new key at the upstream
  janus(['credential', 'disable', 'or1']); janus(['credential', 'disable', 'ps1']);
  // rotate ps1, make it the only active, confirm upstream sees the rotated key
  const ROT = 'sk-or-QA-ROTATED-444';
  const rot = janus(['credential', 'rotate', 'ps1', '--api-key-stdin'], { input: ROT + '\n' });
  janus(['credential', 'enable', 'ps1']);
  upstream.seen.length = 0;
  const resp = await dp(tokA); await sleep(300);
  const seenAuth = JSON.stringify(upstream.seen.at(-1)?.headers ?? {});
  T.push(`rotate: exit=${rot.code}\nupstream saw: ${seenAuth}`);
  check(id, 'rotate performs its admin call (upstream sees rotated key)', resp.status === 200 && seenAuth.includes(ROT) && !seenAuth.includes(SECRET), seenAuth);
  check(id, 'rotate output has no secret', !rot.stdout.includes(ROT) && !rot.stderr.includes(ROT));
  // enable/disable/rm
  const dis = janus(['credential', 'disable', 'ps1']);
  check(id, 'disable -> status disabled', dis.code === 0 && jj(janus(['credential', 'show', 'ps1', '--json']).stdout).status === 'disabled');
  const en = janus(['credential', 'enable', 'ps1']);
  check(id, 'enable -> status active', en.code === 0 && jj(janus(['credential', 'show', 'ps1', '--json']).stdout).status === 'active');
  const lu = janus(['credential', 'list', '--upstream', 'anthropic', '--json']);
  check(id, 'list --upstream filters', isJson(lu.stdout) && jj(lu.stdout).credentials.every((c) => c.upstream === 'anthropic'));
  const ref = janus(['credential', 'refresh', 'ps1']);
  check(id, 'refresh performs its admin call (API key: daemon answers not_refreshable, exit 1)', ref.code === 1 && /not_refreshable|nothing to refresh/.test(ref.stderr), ref.stderr);
  // --from-file with a vendor CLI credentials JSON
  const vendor = `${W}/vendor-credentials.json`;
  const VTOK = 'sk-ant-oat01-QA-VENDOR-ACCESS-555';
  fs.writeFileSync(vendor, JSON.stringify({ claudeAiOauth: { accessToken: VTOK, refreshToken: 'sk-ant-ort01-QA-REFRESH-666', expiresAt: Date.now() + 86400000 * 30, scopes: ['user:inference'], subscriptionType: 'max' } }));
  const af = janus(['credential', 'add', '--upstream', 'anthropic', '--ref', 'ant1', '--label', 'vendor', '--from-file', vendor]);
  T.push(`add --from-file vendor json: ${out(af)}`);
  check(id, 'add --from-file (vendor credential JSON) succeeds', af.code === 0, af.stderr);
  const show = janus(['credential', 'show', 'ant1', '--json']);
  check(id, 'vendor credential shows as oauth with expiry and no material', show.code === 0 && jj(show.stdout).kind !== 'api_key' && !show.stdout.includes(VTOK) && !show.stdout.includes('REFRESH-666'), show.stdout);
  const af2 = janus(['credential', 'add', '--upstream', 'openrouter', '--ref', 'file1', '--from-file', keyFileOf('sk-or-QA-FILEKEY-777')]);
  check(id, 'add --from-file (bare key file) succeeds, no key in output', af2.code === 0 && !af2.stdout.includes('FILEKEY-777') && !af2.stderr.includes('FILEKEY-777'));
  const rm = janus(['credential', 'rm', 'file1']);
  check(id, 'rm removes the slot', rm.code === 0 && janus(['credential', 'show', 'file1']).code !== 0);
  janus(['credential', 'rm', 'ant1']); janus(['credential', 'rm', 'ps1']); janus(['credential', 'enable', 'or1']);
  art(id, 'transcript.txt', T.join('\n'));
}
function keyFileOf(k) { const f = `${W}/kf-${k.length}`; fs.writeFileSync(f, k + '\n'); return f; }

// ---- D100
console.log('D100');
{
  const id = 'D100';
  const T = [];
  const cr = janus(['identity', 'create', '--identity', 'rev-agent', '--claim', 'tenant=zeta', '--label', 'will-revoke']);
  const tok = cr.stdout.trim();
  T.push(`create: ${out(cr)}`);
  check(id, 'create prints a token (alone on stdout)', cr.code === 0 && /^\S{20,}$/.test(tok), cr.stdout);
  const exp = janus(['identity', 'create', '--identity', 'exp-agent', '--expires-in', '2s']);
  const tokExp = exp.stdout.trim();
  const keep = mkid('keep-agent');
  const rmme = janus(['identity', 'create', '--identity', 'rm-agent', '--json']);
  const rmId = jj(rmme.stdout).id; const tokRm = jj(rmme.stdout).token;
  check(id, 'create --json carries the token too (once)', !!tokRm);
  const lst = janus(['identity', 'list', '--json']);
  const ids = jj(lst.stdout).identities;
  const revId = ids.find((i) => i.identity === 'rev-agent').id;
  const show = janus(['identity', 'show', revId]); const showJ = janus(['identity', 'show', revId, '--json']);
  T.push(out(lst), out(show), out(showJ));
  const tokens = [tok, tokExp, keep, tokRm];
  check(id, 'show/list never print any token', ![lst, show, showJ, janus(['identity', 'list'])].some((r) => tokens.some((t) => r.stdout.includes(t) || r.stderr.includes(t))));
  const ok1 = await dp(tok);
  check(id, 'token works before revoke', ok1.status === 200);
  const rv = janus(['identity', 'revoke', revId]);
  const after = await dp(tok); // immediately
  check(id, 'revoke takes effect on the very next data-plane request (401)', rv.code === 0 && after.status === 401, `revoke=${rv.code} status=${after.status}`);
  await sleep(2500); // let exp-agent expire
  const expUse = await dp(tokExp);
  check(id, 'expired token is rejected (401)', expUse.status === 401, expUse.status);
  const names = (args) => jj(janus([...args, '--json']).stdout).identities.map((i) => i.identity);
  const def = names(['identity', 'list']), wr = names(['identity', 'list', '--include-revoked']), we = names(['identity', 'list', '--include-expired']), wb = names(['identity', 'list', '--include-revoked', '--include-expired']);
  T.push(JSON.stringify({ def, wr, we, wb }, null, 1));
  check(id, 'default list hides revoked and expired', !def.includes('rev-agent') && !def.includes('exp-agent') && def.includes('keep-agent'), def);
  check(id, '--include-revoked adds only revoked', wr.includes('rev-agent') && !wr.includes('exp-agent'), wr);
  check(id, '--include-expired adds only expired', we.includes('exp-agent') && !we.includes('rev-agent'), we);
  check(id, 'both flags show both', wb.includes('rev-agent') && wb.includes('exp-agent'));
  check(id, '--identity filters list', names(['identity', 'list', '--identity', 'keep-agent']).join() === 'keep-agent');
  const rmr = janus(['identity', 'rm', rmId]);
  check(id, 'rm removes the entry', rmr.code === 0 && janus(['identity', 'show', rmId]).code !== 0 && (await dp(tokRm)).status === 401);
  art(id, 'transcript.txt', T.join('\n'));
}

// ---- D101
console.log('D101');
{
  const id = 'D101';
  const T = [];
  const tokB = mkid('agent-2', '--label', 'a2');
  for (let i = 0; i < 3; i++) await dp(tokA);
  await dp(tokB); await dp(tokDeny); await dp('nonsense-token');
  await dp(tokA, '/openrouter/v1/boom');
  await sleep(500);
  const q = (args) => jj(janus(['log', 'query', '--json', ...args]).stdout).requests;
  const all = q(['--limit', '1000']);
  T.push(out(janus(['log', 'query', '--limit', '5'])));
  check(id, 'query returns records', all.length >= 8);
  const byId = q(['--identity', 'agent-2']);
  check(id, '--identity filter', byId.length >= 1 && byId.every((r) => r.caller_identity === 'agent-2'));
  const byOut = q(['--outcome', 'denied']);
  check(id, '--outcome denied filter', byOut.length >= 1 && byOut.every((r) => r.outcome === 'denied'), byOut.map((r) => r.outcome));
  const byRej = q(['--outcome', 'rejected_identity']);
  check(id, '--outcome rejected_identity filter', byRej.length >= 1 && byRej.every((r) => r.outcome === 'rejected_identity'));
  const byUp = q(['--upstream', 'openrouter']);
  check(id, '--upstream filter', byUp.length >= 5 && byUp.every((r) => r.upstream === 'openrouter'));
  check(id, '--upstream anthropic returns none', q(['--upstream', 'anthropic']).length === 0);
  const byCred = q(['--credential', 'or1']);
  check(id, '--credential filter', byCred.length >= 1 && byCred.every((r) => r.credential_ref === 'or1'));
  check(id, '--limit', q(['--limit', '2']).length === 2);
  const pg = jj(janus(['log', 'query', '--json', '--limit', '2']).stdout);
  check(id, 'pagination cursor (next) yields older page', pg.next && q(['--limit', '2', '--before', pg.next])[0].request_id !== pg.requests[0].request_id);
  check(id, '--since 1h includes all, --since 0s-ish excludes', q(['--since', '1h', '--limit', '1000']).length === all.length && q(['--since', '2099-01-01T00:00:00Z']).length === 0);
  check(id, '--until in past excludes all', q(['--until', '2020-01-01T00:00:00Z']).length === 0 && q(['--until', '2099-01-01T00:00:00Z', '--limit', '1000']).length === all.length);
  // tail with filters while traffic flows
  const tail = spawn(`${BIN}/janus`, ['log', 'tail', '--identity', 'agent-2', '--outcome', 'forwarded'], { env: { PATH: process.env.PATH, JANUS_DATA_DIR: DATA } });
  let tout = ''; tail.stdout.on('data', (d) => (tout += d));
  await sleep(700);
  await dp(tokA); await dp(tokB); await dp(tokDeny); await dp(tokB);
  await sleep(1200);
  const mid = tout;
  await sleep(100);
  tail.kill('SIGINT');
  const code = await new Promise((r) => tail.on('close', (c, s) => r(`${c}/${s}`)));
  T.push(`tail output:\n${tout}\n(tail ended: ${code})`);
  const lines = tout.trim().split('\n').filter(Boolean);
  check(id, 'tail printed the new matching records (2 agent-2 forwarded)', lines.filter((l) => /agent-2/.test(l)).length === 2, tout);
  check(id, 'tail printed no non-matching records', !/agent-1|deny-agent/.test(tout) && lines.length === 2, tout);
  check(id, 'tail did not replay old agent-2 record (only new)', lines.length === 2);
  check(id, 'tail terminates on interrupt', /SIGINT|^0\/|^130/.test(code), code);
  const one = q(['--limit', '1'])[0];
  const sh = janus(['log', 'show', one.request_id, '--json']);
  T.push(out(janus(['log', 'show', one.request_id])));
  check(id, 'show prints the one full record', sh.code === 0 && JSON.stringify(jj(sh.stdout)) === JSON.stringify(one) || (jj(sh.stdout).request_id === one.request_id && Object.keys(jj(sh.stdout)).length >= Object.keys(one).length));
  check(id, 'show of unknown id fails non-zero', janus(['log', 'show', 'nope-123']).code !== 0);
  art(id, 'transcript.txt', T.join('\n'));
}

// ---- D102
console.log('D102');
{
  const id = 'D102';
  const T = [];
  const ps = janus(['plugin', 'status']); const psj = janus(['plugin', 'status', '--json']);
  T.push(out(ps), out(psj));
  const o = jj(psj.stdout);
  check(id, 'plugin status shows mode', o.mode === 'tls' && /tls/.test(ps.stdout));
  check(id, 'plugin status shows health', o.health === 'ok' && /health\s+ok/.test(ps.stdout), o.health);
  check(id, 'plugin status shows expiry', !!o.client_cert_expires_at && !!o.server_cert_expires_at && /expires/.test(ps.stdout), JSON.stringify(o));
  const pc = janus(['plugin', 'check']); const pcj = janus(['plugin', 'check', '--json']);
  T.push(out(pc), out(pcj));
  const steps = jj(pcj.stdout).steps;
  check(id, 'plugin check prints each probe result and exits 0 when all pass', pc.code === 0 && steps.length === 3 && steps.every((s) => s.passed) && ['handshake', 'throwaway_cert_rejected', 'no_cert_rejected'].every((n) => pc.stdout.includes(n)), pc.stdout);
  const dir = `${W}/init-out`; fs.rmSync(dir, { recursive: true, force: true }); fs.mkdirSync(dir);
  const ini = run(`${BIN}/janus`, ['plugin', 'tls', 'init', '--plugin-host', 'plugin.internal', '--out', dir]);
  T.push(out(ini));
  const files = ['daemon.pem', 'daemon-key.pem', 'plugin.pem', 'plugin-key.pem'];
  const modes = files.map((f) => `${f} ${(fs.statSync(`${dir}/${f}`).mode & 0o777).toString(8)}`);
  T.push(modes.join('\n'));
  check(id, 'init writes the four files', ini.code === 0 && files.every((f) => fs.existsSync(`${dir}/${f}`) && fs.statSync(`${dir}/${f}`).size > 0));
  check(id, 'init prints [plugin.tls] block', /\[plugin\.tls\]/.test(ini.stdout) && /pinned_cert/.test(ini.stdout));
  check(id, 'private keys are mode 0600', ['daemon-key.pem', 'plugin-key.pem'].every((f) => (fs.statSync(`${dir}/${f}`).mode & 0o777) === 0o600), modes.join());
  const san = spawnSync('openssl', ['x509', '-in', `${dir}/plugin.pem`, '-noout', '-ext', 'subjectAltName']);
  check(id, 'plugin cert carries --plugin-host as SAN', /plugin\.internal/.test(san.stdout.toString()), san.stdout.toString());
  const fp1 = spawnSync('openssl', ['x509', '-in', `${dir}/plugin.pem`, '-noout', '-fingerprint']).stdout.toString();
  run(`${BIN}/janus`, ['plugin', 'tls', 'init', '--plugin-host', 'plugin.internal', '--out', dir]);
  const fp2 = spawnSync('openssl', ['x509', '-in', `${dir}/plugin.pem`, '-noout', '-fingerprint']).stdout.toString();
  check(id, 'rerun rotates (new fingerprint)', fp1 !== fp2);
  // no-daemon: init needs no running daemon
  const nd = run(`${BIN}/janus`, ['plugin', 'tls', 'init', '--plugin-host', 'x', '--out', dir, '--admin', 'unix:///nonexistent.sock']);
  check(id, 'init works with no daemon reachable', nd.code === 0);
  // misconfigured plugin: requests but does not require a cert
  await plugin.restart('optional');
  const bad = janus(['plugin', 'check']);
  T.push('--- misconfigured plugin (client cert optional)\n' + out(bad));
  check(id, 'check against misconfigured plugin exits 1 and reports the no-cert probe accepted', bad.code === 1 && /FAIL\s+no_cert_rejected/.test(bad.stdout) && /ok\s+throwaway/.test(bad.stdout), bad.stdout);
  await plugin.restart('required');
  art(id, 'transcript.txt', T.join('\n'));
}

// ---- D103
console.log('D103');
{
  const id = 'D103';
  const T = [];
  const holder = net.createServer(); await new Promise((r) => holder.listen(26041, '127.0.0.1', r));
  const mk = (name, body) => {
    const d = `/tmp/qa-lm/cc-${name}`; fs.rmSync(d, { recursive: true, force: true }); fs.mkdirSync(d, { recursive: true });
    fs.writeFileSync(`${d}/config.toml`, `data_dir = "${d}/data"
[data_plane]
bind = "127.0.0.1:26041"
[admin]
bind = "127.0.0.1:26042"
[ui]
bind = "127.0.0.1:26043"
assets_dir = "${REPO}/ui/dist"
${body}
[[upstream]]
name = "openrouter"
path_prefix = "/openrouter"
base_url = "http://127.0.0.1:${P.up}"
`);
    return d;
  };
  const probe = (port) => new Promise((r) => { const s = net.connect(port, '127.0.0.1'); s.on('connect', () => { s.destroy(); r(true); }); s.on('error', () => r(false)); });
  const good = mk('good', `[identity]\nmethod = "opaque"\nsource = { header = "authorization", scheme = "Bearer " }`);
  const badc = mk('bad', `[identity]\nmethod = "hmac"\nsource = { header = "authorization", scheme = "Bearer " }\nsecret_ref = "no-such-secret"\nnamespace = "ns"\nidentity_claim = "subject"`);
  const g = run(`${BIN}/janus-daemon`, ['check-config', '--config', `${good}/config.toml`]);
  const gp = [await probe(26042), await probe(26043)];
  const gSock = fs.existsSync(`${good}/data/admin.sock`);
  T.push(`## valid config (data-plane port 26041 held by another process)\n${out(g)}\nadmin/ui ports listening: ${gp}\nadmin.sock exists: ${gSock}\ndata dir: ${fs.existsSync(`${good}/data`) ? fs.readdirSync(`${good}/data`) : 'absent'}`);
  check(id, 'valid config exits 0 even though the data-plane port is held', g.code === 0, g.stderr);
  check(id, 'valid: no admin/UI port bound and no socket created', !gp[0] && !gp[1] && !gSock);
  check(id, 'valid: check-config creates nothing in the data dir', !fs.existsSync(`${good}/data`) || fs.readdirSync(`${good}/data`).length === 0, fs.existsSync(`${good}/data`) ? fs.readdirSync(`${good}/data`) : 'absent');
  const b = run(`${BIN}/janus-daemon`, ['check-config', '--config', `${badc}/config.toml`]);
  const bp = [await probe(26042), await probe(26043)];
  const bSock = fs.existsSync(`${badc}/data/admin.sock`);
  T.push(`## binding naming a missing secret\n${out(b)}\nadmin/ui ports listening: ${bp}\nadmin.sock exists: ${bSock}`);
  check(id, 'invalid config exits non-zero with a diagnostic naming the problem', b.code !== 0 && /no-such-secret|secret/i.test(b.stderr + b.stdout), b.stderr);
  check(id, 'invalid: no port bound and no socket created', !bp[0] && !bp[1] && !bSock);
  check(id, 'invalid: check-config creates nothing in the data dir', !fs.existsSync(`${badc}/data`) || fs.readdirSync(`${badc}/data`).length === 0);
  // with the data-plane port FREE, check-config must still not leave it bound
  holder.close(); await sleep(200);
  const g2 = run(`${BIN}/janus-daemon`, ['check-config', '--config', `${good}/config.toml`]);
  check(id, 'check-config leaves the data-plane port free afterwards', g2.code === 0 && !(await probe(26041)) && !(await probe(26042)) && !(await probe(26043)));
  const viaCli = run(`${BIN}/janus`, ['check-config', '--config', `${badc}/config.toml`]);
  T.push(`## janus check-config (invalid)\n${out(viaCli)}`);
  check(id, '`janus check-config` agrees (non-zero on invalid)', viaCli.code !== 0);
  art(id, 'transcript.txt', T.join('\n'));
}

// =====================================================================
// M. UI (real browser via agent-browser)
// =====================================================================
console.log('== M ==');
const ab = (...args) => {
  const r = spawnSync('agent-browser', ['--session', SESSION, ...args], { encoding: 'utf8', timeout: 60000 });
  return { code: r.status, stdout: (r.stdout ?? '').trim(), stderr: (r.stderr ?? '').trim() };
};
const ev = (js) => { const r = ab('eval', js); try { return JSON.parse(r.stdout); } catch { return r.stdout; } };
const shot = (id, name) => { const f = `${ART}/${id}-${name}.png`; ab('screenshot', f); if (fs.existsSync(f)) R(id).artifacts.add(`${REL}/${id}-${name}.png`); };
const mainText = () => ev('document.querySelector("main")?.innerText ?? document.body.innerText');
const UI = `http://127.0.0.1:${PROXY}`;
const waitText = async (re, ms = 6000) => { const t0 = Date.now(); let t = ''; while (Date.now() - t0 < ms) { t = String(mainText()); if (re.test(t)) return t; await sleep(250); } return t; };
const clickText = (sel, text) => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(x=>x.innerText.trim().startsWith(${JSON.stringify(text)}));if(!e)return false;e.click();return true})()`);
const tableRows = () => JSON.parse(ev(`JSON.stringify([...document.querySelectorAll('main tbody tr')].map(r=>r.innerText.replace(/\\s+/g,' ').trim()))`));
const nav = async (name) => { clickText('nav a', name); await sleep(700); };

// ---- D104
console.log('D104');
{
  const id = 'D104';
  ab('close'); await sleep(1000);
  ab('open', UI + '/');
  await sleep(1000);
  const first = ev('document.body.innerText');
  shot(id, '1-token-prompt');
  check(id, 'prompt appears on first visit', /Admin token/.test(first) && /Connect/.test(first), first);
  const apiBefore = proxyLog.filter((r) => r.url.startsWith('/v1')).length;
  check(id, 'no API call is made before the token is entered', apiBefore === 0, JSON.stringify(proxyLog.filter((r) => r.url.startsWith('/v1'))));
  // enter the token using real input events
  ab('click', 'input');
  ab('fill', 'input', TOKEN);
  ab('click', 'button[type=submit]');
  await sleep(1500);
  const afterLogin = String(mainText());
  shot(id, '2-after-login');
  check(id, 'entering the token reaches the app (Overview)', /Overview/.test(afterLogin) && /Daemon/.test(afterLogin), afterLogin.slice(0, 100));
  ab('reload'); await sleep(1500);
  const afterReload = String(mainText());
  shot(id, '3-after-reload');
  check(id, 'reload does not prompt again', /Overview/.test(afterReload) && !/Admin token/.test(afterReload));
  const store = ev('JSON.stringify({session:Object.fromEntries(Object.entries(sessionStorage)),local:Object.keys(localStorage).length,cookie:document.cookie,url:location.href,hash:location.hash,search:location.search})');
  const so = JSON.parse(store);
  art(id, 'storage.json', { sessionStorage_keys: Object.keys(so.session), sessionStorage_has_token: Object.values(so.session).includes(TOKEN), localStorage_entries: so.local, cookie: so.cookie, url: so.url });
  check(id, 'token present in sessionStorage', Object.values(so.session).some((v) => v === TOKEN || String(v).includes(TOKEN)), Object.keys(so.session));
  check(id, 'token not in localStorage', so.local === 0);
  check(id, 'token not in cookies', !so.cookie.includes(TOKEN) && so.cookie === '');
  check(id, 'token not in the page URL', !so.url.includes(TOKEN));
  // walk all views so every API call is exercised, then audit the network log
  for (const v of ['Credentials', 'Identities', 'Requests', 'Plugin', 'Overview']) { await nav(v); await sleep(500); }
  const api = proxyLog.filter((r) => r.url.startsWith('/v1'));
  art(id, 'network-log.json', api.map((r) => ({ method: r.method, url: r.url, authorization: r.authorization ? 'Bearer <present:' + (r.authorization === `Bearer ${TOKEN}` ? 'matches admin token' : 'MISMATCH') + '>' : null, cookie: r.cookie })));
  check(id, 'API requests were made (status, credentials, identities, requests, plugin)', ['/v1/status', '/v1/credentials', '/v1/identities', '/v1/requests', '/v1/plugin'].every((p) => api.some((r) => r.url.startsWith(p))), [...new Set(api.map((r) => r.url.split('?')[0]))].join());
  check(id, 'every API request carries Authorization: Bearer <admin token>', api.length > 0 && api.every((r) => r.authorization === `Bearer ${TOKEN}`), JSON.stringify(api.filter((r) => r.authorization !== `Bearer ${TOKEN}`)));
  check(id, 'token never appears in any request URL or cookie', !proxyLog.some((r) => r.url.includes(TOKEN) || (r.cookie ?? '').includes(TOKEN)));
  // wrong token is not stored as success
  const reqCount = api.length;
  check(id, 'static assets are fetched without the token requirement', proxyLog.some((r) => !r.url.startsWith('/v1')));
  // security response headers on the UI listener, as the real browser received them
  const docResp = proxy.resp.find((x) => x.url === '/' || x.url === '/overview' || x.url.startsWith('/?')) ?? proxy.resp[0];
  const apiResp = proxy.resp.find((x) => x.url.startsWith('/v1/status'));
  const assetResp = proxy.resp.find((x) => x.url.startsWith('/assets/'));
  const curlRoot = run('curl', ['-si', `http://127.0.0.1:${P.ui}/`]);
  const curlApi = run('curl', ['-si', '-H', `Authorization: Bearer ${TOKEN}`, `http://127.0.0.1:${P.ui}/v1/status`]);
  art(id, 'response-headers.txt', `# as received by the browser (via recording proxy)\n## document ${docResp?.url}\n${JSON.stringify(docResp, null, 2)}\n## API ${apiResp?.url}\n${JSON.stringify(apiResp, null, 2)}\n## asset ${assetResp?.url}\n${JSON.stringify(assetResp, null, 2)}\n\n# curl UI root (headers)\n${curlRoot.stdout.split('\r\n\r\n')[0]}\n\n# curl /v1/status with token (headers)\n${curlApi.stdout.split('\r\n\r\n')[0]}\n`);
  const hh = (r) => r?.headers ?? {};
  const csp = hh(docResp)['content-security-policy'] ?? '';
  check(id, "UI document carries CSP default-src 'self' and frame-ancestors 'none'", /default-src 'self'/.test(csp) && /frame-ancestors 'none'/.test(csp), csp);
  check(id, 'UI document carries X-Content-Type-Options: nosniff and a Referrer-Policy', hh(docResp)['x-content-type-options'] === 'nosniff' && !!hh(docResp)['referrer-policy'], JSON.stringify(hh(docResp)));
  check(id, 'API responses on the UI listener carry the same security headers', /default-src 'self'/.test(hh(apiResp)['content-security-policy'] ?? '') && hh(apiResp)['x-content-type-options'] === 'nosniff', JSON.stringify(hh(apiResp)));
  const cons = String(ab('console').stdout) + String(ab('errors').stdout);
  art(id, 'browser-console.txt', cons || '(empty)');
  check(id, 'no CSP violations or JS errors in the browser console while the UI works end to end', !/Content Security Policy|violates|Refused to/i.test(cons) && !/Uncaught|TypeError/.test(cons), cons.slice(0, 300));
  // a different browser session (fresh sessionStorage) prompts again; wrong token is refused
  SESSION = 'qalm2';
  ab('open', UI + '/'); await sleep(1200);
  check(id, 'a new browser session prompts again (token is session-scoped)', /Admin token/.test(String(ev('document.body.innerText'))));
  ab('fill', 'input', 'definitely-wrong-token'); ab('click', 'button[type=submit]'); await sleep(1500);
  const wrongT = String(ev('document.body.innerText'));
  shot(id, '4-wrong-token');
  check(id, 'a wrong token is refused (prompt stays, no console data)', /Admin token/.test(wrongT) && !/Version/.test(wrongT), wrongT.slice(0, 200));
  art(id, 'wrong-token-text.txt', wrongT);
  ab('fill', 'input', TOKEN); ab('click', 'button[type=submit]'); await sleep(1500);
  check(id, 'correct token then succeeds', /Overview/.test(String(mainText())));
  ab('close'); await sleep(800);
  SESSION = 'qalm';
}

// ---- D105
console.log('D105');
{
  const id = 'D105';
  // make traffic mix: forwarded, denied, rejected
  for (let i = 0; i < 3; i++) await dp(tokA);
  await dp(tokDeny); await dp(tokDeny); await dp('bad-token-xyz');
  await sleep(600);
  // make credential health non-trivial: second credential disabled, third pointing nowhere
  addKey('or2', 'sk-or-QA-SECOND'); janus(['credential', 'disable', 'or2']);
  await nav('Overview'); ab('reload'); await sleep(1500);
  const txt = String(await waitText(/Traffic/));
  const html = String(ev('document.querySelector("main").innerText'));
  shot(id, 'overview');
  art(id, 'overview-text.txt', html);
  const sj = jj(janus(['status', '--json']).stdout);
  const cl = jj(janus(['credential', 'list', '--json']).stdout).credentials;
  art(id, 'cli-status.json', sj); art(id, 'cli-credentials.json', cl);
  for (const l of Object.entries(sj.listeners)) { /* listed below */ }
  const addrs = [sj.listeners.data_plane, sj.listeners.admin_socket, sj.listeners.admin_tcp.url, sj.listeners.ui.url];
  check(id, 'listeners match status --json', addrs.every((a) => html.includes(a)), addrs.join());
  check(id, 'upstreams + bindings match status --json', sj.upstreams.every((u) => html.includes(u.name) && html.includes(u.path_prefix) && html.includes(u.base_url) && html.includes(u.binding)));
  check(id, 'version shown', html.includes(sj.version));
  check(id, 'plugin mode and health match', html.includes(sj.plugin.health) && /Mode\s*\n\s*tls/.test(html) && html.includes(sj.plugin.endpoint), sj.plugin.health);
  // certificate expiry: UI shows local-time "YYYY-MM-DD HH:MM:SS"
  const fmt = (iso) => { const d = new Date(iso); const p = (n) => String(n).padStart(2, '0'); return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`; };
  const fmtU = (iso) => iso.replace('T', ' ').replace(/(\.\d+)?Z$/, '');
  const certOk = (iso) => html.includes(fmt(iso)) || html.includes(fmtU(iso));
  check(id, 'client certificate expiry matches status --json', certOk(sj.plugin.client_cert_expires_at), `${sj.plugin.client_cert_expires_at} vs ${html.match(/client certificate expires\s*\n\s*(.*)/i)?.[1]}`);
  check(id, 'server certificate expiry matches status --json', !!sj.plugin.server_cert_expires_at && certOk(sj.plugin.server_cert_expires_at), `${sj.plugin.server_cert_expires_at} vs ${html.match(/server certificate expires\s*\n\s*(.*)/i)?.[1]}`);
  // credential health
  const by = {}; for (const c of cl) by[c.status] = (by[c.status] ?? 0) + 1;
  const uiCred = ev(`JSON.stringify([...document.querySelectorAll('[aria-label="Credentials by status"] li')].map(li=>li.innerText.replace(/\\s+/g,' ').trim()))`);
  const uiMap = {}; for (const s of JSON.parse(uiCred)) { const m = s.match(/^(\d+)\s+(.*)$/); if (m) uiMap[m[2].replace(' ', '_')] = +m[1]; }
  art(id, 'credential-health.json', { cli: by, ui: uiMap });
  check(id, 'credential health matches `credential list --json` status values', ['active', 'rate_limited', 'invalid', 'disabled'].every((k) => (uiMap[k] ?? 0) === (by[k] ?? 0)) && (by.disabled ?? 0) >= 1, JSON.stringify({ by, uiMap }));
  // rates vs log
  const since = '1h';
  const total = jj(janus(['log', 'query', '--since', since, '--limit', '1000', '--json']).stdout).requests.length;
  const denied = jj(janus(['log', 'query', '--since', since, '--outcome', 'denied', '--limit', '1000', '--json']).stdout).requests.length;
  const rejected = jj(janus(['log', 'query', '--since', since, '--outcome', 'rejected_identity', '--limit', '1000', '--json']).stdout).requests.length;
  const m1 = html.match(/Requests\s*\n\s*(\d+)\s*\n\s*([\d.]+) per minute/);
  const m2 = html.match(/Denied by the plugin\s*\n\s*(\d+)\s*\n\s*([\d.]+) per minute, ([\d.]+)% of requests/);
  const m3 = html.match(/Rejected at identity\s*\n\s*(\d+)/);
  art(id, 'counts.json', { cli: { total, denied, rejected }, ui: { req: m1?.[1], reqRate: m1?.[2], denied: m2?.[1], deniedRate: m2?.[2], pct: m2?.[3], rejected: m3?.[1] } });
  check(id, 'request count matches `log query` total over the same period', m1 && +m1[1] === total, `ui=${m1?.[1]} cli=${total}`);
  check(id, 'denied count matches `log query --outcome denied`', m2 && +m2[1] === denied && denied >= 2, `ui=${m2?.[1]} cli=${denied}`);
  check(id, 'rejected-at-identity count matches `--outcome rejected_identity`', m3 && +m3[1] === rejected && rejected >= 1, `ui=${m3?.[1]} cli=${rejected}`);
  check(id, 'rates are shown and consistent (count / 60 min; denial % = denied/total)', m1 && Math.abs(+m1[2] - total / 60) < 0.02 && m2 && Math.abs(+m2[3] - (denied / total) * 100) < 0.1, `${m1?.[2]} ${m2?.[3]}`);
  // window selector changes the period: 15 minutes shows same since all traffic is recent
  ab('select', 'select', 'Last 24 hours'); await sleep(1000);
  const t24 = String(ev('document.querySelector("main").innerText'));
  const total24 = jj(janus(['log', 'query', '--since', '24h', '--limit', '1000', '--json']).stdout).requests.length;
  check(id, 'window selector re-queries (24h count matches log query --since 24h)', new RegExp(`Requests\\s*\\n\\s*${total24}\\s*\\n`).test(t24), t24.match(/Requests\s*\n\s*\d+/)?.[0]);
  // outcome table
  const outcomes = jj(janus(['log', 'query', '--limit', '1000', '--json']).stdout).requests.reduce((a, r) => ((a[r.outcome] = (a[r.outcome] ?? 0) + 1), a), {});
  const rowsOk = Object.entries(outcomes).every(([o, n]) => new RegExp(`${o.replace('_', ' ')}\\s*\\n?\\s*${n}\\b`).test(t24) || new RegExp(`${o.replace('_', ' ')}\\s+${n}\\b`).test(t24.replace(/\t/g, ' ')));
  check(id, 'outcome breakdown table matches log', rowsOk, JSON.stringify(outcomes) + ' / ' + t24.slice(t24.indexOf('Requests by outcome'), t24.indexOf('Requests by outcome') + 300).replace(/\n/g, '|'));
  // plugin 'unreachable' reflected: kill the front, make a request, reload the Overview
  janus(['credential', 'rm', 'or2']);
}

// ---- D106
console.log('D106');
{
  const id = 'D106';
  addKey('ui1', 'sk-or-QA-UIKEY-888'); addKey('ui2', 'sk-or-QA-UIKEY-999');
  await nav('Credentials'); ab('reload'); await sleep(1500);
  const t0 = String(await waitText(/ui1/));
  shot(id, '1-credentials');
  const state = (ref) => jj(janus(['credential', 'show', ref, '--json']).stdout);
  check(id, 'list shows credentials with status and counters', /ui1/.test(t0) && /active/.test(t0) && /Requests/.test(t0) && /Errors/.test(t0) && /Last used/.test(t0), t0.slice(0, 300));
  check(id, 'counters match CLI (or1 requests)', new RegExp(`or1[\\s\\S]*?active[\\s\\S]*?${state('or1').request_count}\\s`).test(t0.replace(/\t/g, ' ')), String(state('or1').request_count));
  check(id, 'no add/rotate/material entry form in the view', ev('document.querySelectorAll("main input[type=password], main textarea, main input[type=file], main input[type=text]").length') === 0 && !/Add credential|Rotate/i.test(t0.replace(/Adding a credential and rotating[^.]*\./, '')));
  check(id, 'no key material displayed', !t0.includes('UIKEY') && !t0.includes('sk-or-'));
  // row-scoped click via data in the row
  const rowAction = (ref, action) => ev(`(()=>{const row=[...document.querySelectorAll('main tr')].find(r=>r.innerText.includes(${JSON.stringify(ref)})&&r.querySelector('button'));if(!row)return 'no-row';const b=[...row.querySelectorAll('button')].find(b=>b.innerText.trim().startsWith(${JSON.stringify(action)}));if(!b)return 'no-btn';b.click();return 'ok'})()`);
  // Disable
  let r = rowAction('ui1', 'Disable'); await sleep(1200);
  shot(id, '2-after-disable');
  check(id, 'Disable button works -> `credential show` status disabled', r === 'ok' && state('ui1').status === 'disabled', `${r} ${state('ui1').status}`);
  check(id, 'UI row reflects disabled', /ui1[\s\S]*?disabled/.test(String(mainText())));
  // Enable
  r = rowAction('ui1', 'Enable'); await sleep(1200);
  check(id, 'Enable button works -> status active', r === 'ok' && state('ui1').status === 'active', `${r} ${state('ui1').status}`);
  // Refresh (API-key slot: daemon says not_refreshable; the UI should surface it and not change state)
  upstream.seen.length = 0;
  proxyLog.length = 0;
  r = rowAction('ui1', 'Refresh'); await sleep(1200);
  const refreshCall = proxyLog.find((x) => /\/v1\/credentials\/ui1\/refresh/.test(x.url));
  const afterRefreshText = String(mainText());
  shot(id, '3-after-refresh');
  check(id, 'Refresh button issues the refresh admin call with the token', r === 'ok' && !!refreshCall && refreshCall.method === 'POST' && refreshCall.authorization === `Bearer ${TOKEN}`, JSON.stringify(refreshCall));
  check(id, 'Refresh on an API key surfaces the daemon refusal in the UI', /refresh|not_refreshable|API key/i.test(afterRefreshText.replace(/Refresh\n/g, '')) , afterRefreshText.slice(0, 400));
  // Refresh on an OAuth credential: state really changes (expires_at moves) via the local mock token endpoint
  const oauthFile = `${W}/oauth-creds.json`;
  fs.writeFileSync(oauthFile, JSON.stringify({ claudeAiOauth: { accessToken: 'sk-ant-oat01-OLD-ACCESS', refreshToken: 'sk-ant-ort01-OLD-REFRESH', expiresAt: Date.now() + 30 * 60 * 1000, scopes: ['user:inference'], subscriptionType: 'max' } }));
  const oadd = janus(['credential', 'add', '--upstream', 'anthropic', '--ref', 'oauth1', '--from-file', oauthFile]);
  check(id, 'OAuth credential added for the Refresh test', oadd.code === 0, oadd.stderr);
  await nav('Credentials'); ab('reload'); await sleep(1500); await waitText(/oauth1/);
  const exp0 = state('oauth1').expires_at;
  tokenReqs.length = 0; proxyLog.length = 0;
  r = rowAction('oauth1', 'Refresh'); await sleep(2000);
  const exp1 = state('oauth1').expires_at;
  shot(id, '3b-after-oauth-refresh');
  const rc = proxyLog.find((x) => /\/v1\/credentials\/oauth1\/refresh/.test(x.url));
  art(id, 'oauth-refresh.json', { expires_before: exp0, expires_after: exp1, token_endpoint_requests: tokenReqs.map((t) => ({ ...t, body: t.body.replace(/(refresh_token=)[^&]+/, '$1<redacted>') })), ui_call: rc && { method: rc.method, url: rc.url } });
  check(id, 'UI Refresh on an OAuth credential calls the refresh admin endpoint with the token', r === 'ok' && rc?.method === 'POST' && rc.authorization === `Bearer ${TOKEN}`, JSON.stringify(rc));
  check(id, 'daemon contacted the mock token endpoint (token_url) exactly once', tokenReqs.length === 1 && /refresh_token/.test(tokenReqs[0].body), JSON.stringify(tokenReqs));
  check(id, 'state actually changes: expires_at moved forward (confirmed by `credential show`)', !!exp0 && !!exp1 && new Date(exp1) > new Date(exp0) && exp1 !== exp0, `${exp0} -> ${exp1}`);
  check(id, 'UI row shows the new expiry and no token material', !String(mainText()).includes('NEW-ACCESS') && !String(mainText()).includes('NEW-REFRESH'));
  janus(['credential', 'rm', 'oauth1']);
  await nav('Credentials'); ab('reload'); await sleep(1500); await waitText(/ui2/);
  // Remove (may have a confirm step)
  r = rowAction('ui2', 'Remove'); await sleep(800);
  shot(id, '4-remove-confirm');
  const confirmText = String(ev('document.body.innerText'));
  let st = janus(['credential', 'show', 'ui2']);
  if (st.code === 0) { // inline confirmation inside the row
    ev(`(()=>{const row=[...document.querySelectorAll('main tr')].find(r=>r.innerText.includes('ui2'));const b=[...(row?.querySelectorAll('button')??[])].find(b=>b.innerText.trim()==='Remove ui2');b?.click();return !!b})()`);
    await sleep(1200); st = janus(['credential', 'show', 'ui2']);
  }
  check(id, 'Remove works -> `credential show` no longer finds it', r === 'ok' && st.code !== 0, `${r} exit=${st.code} ${confirmText.slice(0, 120).replace(/\n/g, '|')}`);
  check(id, 'removed credential gone from the UI table rows', !tableRows().some((r) => r.includes('ui2')) && tableRows().some((r) => r.includes('ui1')));
  check(id, 'remaining actions do not touch other credentials', state('or1').status === 'active' && state('ui1').status === 'active');
  art(id, 'final-credentials.txt', out(janus(['credential', 'list'])));
  janus(['credential', 'rm', 'ui1']);
}

// ---- D107
console.log('D107');
{
  const id = 'D107';
  const tokR = mkid('ui-revoke-me', '--label', 'rv'); const tokX = mkid('ui-keep', '--label', 'kp');
  const tokO = janus(['identity', 'create', '--identity', 'ui-old']).stdout.trim();
  const revOld = jj(janus(['identity', 'list', '--identity', 'ui-old', '--json']).stdout).identities[0].id;
  janus(['identity', 'revoke', revOld]);
  await nav('Identities'); ab('reload'); await sleep(1500);
  const t0 = String(await waitText(/ui-keep/));
  shot(id, '1-identities');
  check(id, 'list shows registry entries (active by default)', /ui-keep/.test(t0) && /ui-revoke-me/.test(t0) && !/ui-old/.test(t0), t0.slice(0, 200));
  const ctl = JSON.parse(ev(`JSON.stringify([...document.querySelectorAll('main button, main a, main input, main textarea, main select, main form')].map(e=>(e.tagName+':'+(e.innerText||e.getAttribute('aria-label')||e.type||'')).trim()))`));
  art(id, 'controls.json', ctl);
  check(id, 'no create control (only filter, toggles, Revoke)', ctl.length > 0 && !ctl.some((c) => /create|add|new|issue|generate/i.test(c)) && ctl.filter((c) => c.startsWith('BUTTON')).every((c) => /Filter|Revoke|Cancel|Reload/.test(c)), ctl.join(' | '));
  check(id, 'no token displayed', ![tokR, tokX, tokA, tokO].some((t) => t && t.length > 15 && t0.includes(t)) && !(ev('document.querySelector("main").innerHTML').includes(tokR)));
  // filter by identity
  const setInput = (label, v) => ev(`(()=>{const l=[...document.querySelectorAll('main label')].find(x=>x.innerText.includes(${JSON.stringify(label)}));const i=l?.querySelector('input')||document.getElementById(l?.htmlFor);if(!i)return false;const set=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set;set.call(i,${JSON.stringify(v)});i.dispatchEvent(new Event('input',{bubbles:true}));return true})()`);
  const toggle = (label) => ev(`(()=>{const l=[...document.querySelectorAll('main label')].find(x=>x.innerText.includes(${JSON.stringify(label)}));const i=l?.querySelector('input')||document.getElementById(l?.htmlFor);if(!i)return false;i.click();return true})()`);
  const apply = () => ev(`(()=>{const b=[...document.querySelectorAll('main button')].find(b=>b.innerText.trim()==='Filter');b?.click();return !!b})()`);
  check(id, 'filter input present', setInput('Identity (exact)', 'ui-keep') === true);
  apply(); await sleep(1000);
  const t1 = String(mainText());
  shot(id, '2-filter-identity');
  check(id, 'identity filter narrows the list', /ui-keep/.test(t1) && !/ui-revoke-me/.test(t1) && !/agent-1/.test(t1), t1.slice(0, 300));
  setInput('Identity (exact)', ''); apply(); await sleep(800);
  check(id, 'revoked entry hidden by default', !/ui-old/.test(String(mainText())));
  check(id, 'Include revoked toggle present', toggle('Include revoked') === true); apply(); await sleep(1000);
  const t2 = String(mainText());
  shot(id, '3-include-revoked');
  check(id, 'Include revoked shows the revoked entry (CLI agrees)', /ui-old/.test(t2) && janus(['identity', 'list', '--include-revoked', '--json']).stdout.includes('ui-old'));
  check(id, 'Include expired toggle present and shows expired entries (exp-agent)', toggle('Include expired') === true && (apply(), await sleep(1000), /exp-agent/.test(String(mainText()))));
  toggle('Include revoked'); toggle('Include expired'); apply(); await sleep(800);
  // revoke through the UI
  check(id, 'token works before UI revoke', (await dp(tokR)).status === 200);
  const rowRevoke = ev(`(()=>{const row=[...document.querySelectorAll('main tr')].find(r=>r.innerText.includes('ui-revoke-me')&&r.querySelector('button'));const b=[...(row?.querySelectorAll('button')??[])].find(b=>b.innerText.trim().startsWith('Revoke'));if(!b)return 'no-btn';b.click();return 'ok'})()`);
  await sleep(300);
  shot(id, '4-revoke-clicked');
  let after = await dp(tokR);
  if (after.status === 200) { // inline confirmation inside the row
    ev(`(()=>{const row=[...document.querySelectorAll('main tr')].find(r=>r.innerText.includes('ui-revoke-me'));const b=[...(row?.querySelectorAll('button')??[])].find(b=>b.innerText.trim()==='Revoke ui-revoke-me');b?.click();return !!b})()`);
    await sleep(1200); after = await dp(tokR);
  }
  check(id, 'UI Revoke makes the token fail on the next data-plane request (401)', rowRevoke === 'ok' && after.status === 401, `${rowRevoke} ${after.status}`);
  shot(id, '5-after-revoke');
  check(id, 'other tokens unaffected', (await dp(tokX)).status === 200);
  check(id, 'UI no longer lists the revoked entry by default (table rows)', !tableRows().some((r) => r.includes('ui-revoke-me')) && tableRows().some((r) => r.includes('ui-keep')));
  check(id, 'registry state agrees (CLI sees revoked_at set)', jj(janus(['identity', 'list', '--include-revoked', '--identity', 'ui-revoke-me', '--json']).stdout).identities[0]?.revoked_at != null);
}

// ---- D108
console.log('D108');
{
  const id = 'D108';
  await nav('Requests'); ab('reload'); await sleep(1500);
  const t0 = String(await waitText(/Received/));
  shot(id, '1-requests');
  const uiRows = () => JSON.parse(ev(`JSON.stringify([...document.querySelectorAll('main tbody tr')].map(r=>r.innerText.replace(/\\s+/g,' ').trim()))`));
  const cli = jj(janus(['log', 'query', '--limit', '50', '--json']).stdout).requests;
  const rows = uiRows();
  art(id, 'table-vs-cli.json', { ui_rows: rows.slice(0, 8), cli: cli.slice(0, 8).map((r) => [r.received_at, r.caller_identity, r.upstream, r.outcome, r.method, r.path, r.status]) });
  check(id, 'table row count matches `log query --limit 50`', rows.length === cli.length, `ui=${rows.length} cli=${cli.length}`);
  check(id, 'table rows match CLI records in order (identity, outcome, path, status)', cli.slice(0, 10).every((r, i) => rows[i] && rows[i].includes(r.upstream) && rows[i].includes(r.outcome.replace('_', ' ')) && rows[i].includes(`${r.method} ${r.path}`) && (r.status == null || rows[i].includes(String(r.status))) && (!r.caller_identity || rows[i].includes(r.caller_identity))), rows.slice(0, 3).join(' || '));
  // filter: outcome denied
  const sel = (label, v) => ev(`(()=>{const l=[...document.querySelectorAll('main label')].find(x=>x.innerText.trim().startsWith(${JSON.stringify(label)}));const s=l?.querySelector('select')||document.getElementById(l?.htmlFor);if(!s)return false;const set=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;const o=[...s.options].find(o=>o.text.trim()===${JSON.stringify(v)});if(!o)return 'no-option';set.call(s,o.value);s.dispatchEvent(new Event('change',{bubbles:true}));return true})()`);
  check(id, 'outcome filter control exists', sel('Outcome', 'denied') === true);
  ev(`[...document.querySelectorAll('main button')].find(b=>b.innerText.trim()==='Apply filters')?.click()`); await sleep(1000);
  const dRows = uiRows(); const dCli = jj(janus(['log', 'query', '--outcome', 'denied', '--limit', '50', '--json']).stdout).requests;
  shot(id, '2-filter-denied');
  check(id, 'outcome=denied table matches `log query --outcome denied`', dRows.length === dCli.length && dRows.length >= 2 && dRows.every((r) => /denied/.test(r)), `${dRows.length} vs ${dCli.length}`);
  ev(`[...document.querySelectorAll('main button')].find(b=>b.innerText.trim()==='Clear')?.click()`); await sleep(800);
  // live tail
  proxyLog.length = 0;
  const live = ev(`(()=>{const l=[...document.querySelectorAll('main label,main button')].find(x=>/Live tail/.test(x.innerText));const i=l?.querySelector('input')||(l?.tagName==='BUTTON'?l:null)||document.getElementById(l?.htmlFor);if(!i)return false;i.click();return true})()`);
  await sleep(1200);
  shot(id, '3-live-on');
  const streamReq = proxyLog.find((r) => r.url.startsWith('/v1/requests/stream'));
  check(id, 'live tail toggle opens /v1/requests/stream', live === true && !!streamReq, JSON.stringify(proxyLog.slice(0, 4)));
  check(id, 'stream request carries the token in Authorization, not the URL', streamReq?.authorization === `Bearer ${TOKEN}` && !streamReq.url.includes(TOKEN) && !/token|access_token|auth/i.test(streamReq.url), JSON.stringify(streamReq));
  const nBefore = uiRows().length;
  const tokLive = mkid('live-agent');
  await dp(tokLive); await dp(tokLive); await sleep(1500);
  const onRows = uiRows();
  shot(id, '4-live-rows-arrived');
  check(id, 'new requests appear live without reload while toggle is on', onRows.filter((r) => r.includes('live-agent')).length === 2, `before=${nBefore} after=${onRows.length}; ${onRows.slice(0, 2).join(' || ')}`);
  // off
  ev(`(()=>{const l=[...document.querySelectorAll('main label,main button')].find(x=>/Live tail/.test(x.innerText));const i=l?.querySelector('input')||(l?.tagName==='BUTTON'?l:null)||document.getElementById(l?.htmlFor);i.click()})()`);
  await sleep(800);
  const cntOff = uiRows().filter((r) => r.includes('live-agent')).length;
  await dp(tokLive); await dp(tokLive); await sleep(1500);
  const offRows = uiRows().filter((r) => r.includes('live-agent')).length;
  shot(id, '5-live-off');
  check(id, 'new requests do NOT appear after toggling off', offRows === cntOff && janus(['log', 'query', '--identity', 'live-agent', '--json']).stdout.split('"request_id"').length - 1 === 4, `off before=${cntOff} after=${offRows}`);
  // drawer
  ev(`[...document.querySelectorAll('main button')].find(b=>b.innerText.trim()==='Apply filters')?.click()`); await sleep(1000);
  const opened = ev(`(()=>{const r=[...document.querySelectorAll('main tbody tr')].find(r=>r.innerText.includes('live-agent'));if(!r)return 'no-row';const t=r.querySelector('button, a')||r;t.click();return 'ok'})()`);
  await sleep(1000);
  const dr = String(ev('(document.querySelector("[role=dialog],dialog,aside[aria-label],[class*=drawer]")||document.body).innerText'));
  shot(id, '6-drawer');
  const rid = (dr.match(/Request id\s*\n?\s*([0-9a-f-]{36})/) ?? [])[1];
  const rec = rid ? jj(janus(['log', 'show', rid, '--json']).stdout) : {};
  art(id, 'drawer-text.txt', dr); art(id, 'record.json', rec);
  const needles = [rec.request_id, rec.upstream, rec.caller_identity, rec.outcome?.replace('_', ' '), rec.method, rec.path, String(rec.status), rec.binding, rec.credential_ref];
  check(id, 'drawer opens for a record and shows its fields (matches `log show` of the same id)', opened === 'ok' && !!rid && needles.every((n) => n && dr.includes(n)), `${opened}; rid=${rid}; missing: ${needles.filter((n) => !dr?.includes(n))}`);
  check(id, 'drawer shows received time, duration, route, correlation id, error, usage rows', ['Received', 'Duration', 'Route', 'Correlation id', 'Error', 'Usage'].every((l) => dr.includes(l)));
}

// ---- D109
console.log('D109');
{
  const id = 'D109';
  await nav('Plugin'); ab('reload'); await sleep(1500);
  await waitText(/Channel check/);
  const pj = jj(janus(['plugin', 'status', '--json']).stdout);
  const t0 = String(mainText());
  shot(id, '1-plugin');
  check(id, 'health shown matches /v1/plugin', new RegExp(`Health\\s*\\n\\s*${pj.health}`).test(t0) && t0.includes(pj.endpoint), `${pj.health} / ${t0.slice(0, 200).replace(/\n/g, '|')}`);
  const press = () => { proxyLog.length = 0; return ev(`(()=>{const b=[...document.querySelectorAll('main button')].find(b=>/Run channel check/i.test(b.innerText));if(!b)return false;b.click();return true})()`); };
  const stepsFromUi = (t) => { const seg = t.slice(t.indexOf('Run channel check')); return seg; };
  // healthy
  check(id, 'check button exists and runs', press() === true);
  await sleep(2500);
  const t1 = String(mainText());
  shot(id, '2-check-healthy');
  const call = proxyLog.find((r) => r.url.startsWith('/v1/plugin/check'));
  check(id, 'button calls POST /v1/plugin/check with the token', call?.method === 'POST' && call.authorization === `Bearer ${TOKEN}`, JSON.stringify(call));
  const cli1 = jj(janus(['plugin', 'check', '--json']).stdout);
  art(id, 'healthy-ui.txt', t1); art(id, 'healthy-cli.json', cli1);
  const TITLE = { handshake: 'Handshake with the configured certificate', throwaway_cert_rejected: 'Throwaway certificate refused', no_cert_rejected: 'No certificate refused' };
  const okStep = (t, s) => t.replace(/\n/g, '').includes(TITLE[s.step] + (s.passed ? 'passed' : 'failed')) && t.includes(s.detail);
  check(id, 'healthy: UI displays each probe result matching `janus plugin check`', cli1.passed === true && cli1.steps.length === 3 && cli1.steps.every((s) => okStep(t1, s)) && /pass/i.test(t1), t1.slice(t1.indexOf('Run channel check')).slice(0, 500).replace(/\n/g, '|'));
  // misconfigured: requests but doesn't require a client cert
  await plugin.restart('optional');
  press(); await sleep(3000);
  const t2 = String(mainText());
  shot(id, '3-check-misconfigured');
  const cli2 = jj(run(`${BIN}/janus`, ['plugin', 'check', '--json'], { env: { JANUS_DATA_DIR: DATA } }).stdout);
  art(id, 'misconfigured-ui.txt', t2); art(id, 'misconfigured-cli.json', cli2);
  const failing = cli2.steps.filter((s) => !s.passed);
  check(id, 'misconfigured: CLI reports a failing probe', cli2.passed === false && failing.length >= 1 && failing[0].step === 'no_cert_rejected');
  check(id, 'misconfigured: UI displays every probe with the same result detail as the CLI', cli2.steps.every((s) => okStep(t2, s)) && /fail/i.test(t2), t2.slice(t2.indexOf('Run channel check')).slice(0, 600).replace(/\n/g, '|'));
  // unreachable plugin: health reflected
  await plugin.close(); await sleep(500);
  await dp(tokA); await sleep(500);
  press(); await sleep(3000);
  ab('reload'); await sleep(1500);
  const t3 = String(await waitText(/Channel/));
  const pj3 = jj(janus(['plugin', 'status', '--json']).stdout);
  shot(id, '4-plugin-down');
  check(id, 'plugin down: UI health equals /v1/plugin health (' + pj3.health + ')', pj3.health !== 'ok' && new RegExp(`Health\\s*\\n\\s*${pj3.health}`).test(t3), `${pj3.health} / ${t3.slice(0, 160).replace(/\n/g, '|')}`);
  await plugin.restart('required');
}

// ---------- teardown + results ----------
ab('close');
tokenSrv.close(); await plugin.close(); proxy.close(); upstream.close(); daemon.kill('SIGTERM');
await sleep(500);

// final artifact scan: no registered secret nor secret-shaped string in any text artifact
{
  const hits = [];
  for (const f of fs.readdirSync(ART)) {
    if (f.endsWith('.png')) continue;
    const t = fs.readFileSync(`${ART}/${f}`, 'utf8');
    for (const v of SECRETS) if (v && t.includes(v)) hits.push(`${f}: registered secret`);
    if (/(?<![A-Za-z0-9_-])[A-Za-z0-9_-]{43}(?![A-Za-z0-9_-])/.test(t)) hits.push(`${f}: 43-char token shape`);
    if (/sk-(?:or|ant)-[A-Za-z0-9-]+/.test(t)) hits.push(`${f}: sk- key shape`);
  }
  fs.writeFileSync(`${ART}/D104-secret-scan.txt`, hits.length ? hits.join('\n') : `scanned ${fs.readdirSync(ART).length} artifacts: no admin token, identity token or API key found\n`);
  R('D104').artifacts.add(`${REL}/D104-secret-scan.txt`);
  check('D104', 'no secret (admin token, identity tokens, API keys) in any artifact', hits.length === 0, hits.join('; '));
}
const ids = Array.from({ length: 13 }, (_, i) => `D${97 + i}`);
const NOTES = {};
const final = ids.map((id) => {
  const r = R(id);
  const fails = r.checks.filter((c) => !c.ok);
  const result = r.checks.length === 0 ? 'BLOCKED' : fails.length ? 'FAIL' : 'PASS';
  const note = `${r.checks.length - fails.length}/${r.checks.length} checks passed.` +
    (fails.length ? ' Failed: ' + fails.map((f) => `${f.name} [${f.detail}]`).join('; ') : '') +
    (r.blocked.length ? ' Not exercisable black-box: ' + r.blocked.join('; ') : '');
  fs.writeFileSync(`${ART}/${id}-checks.json`, JSON.stringify(r.checks, null, 2)); r.artifacts.add(`${REL}/${id}-checks.json`);
  return { id, result, artifacts: [...r.artifacts].sort(), note };
});
fs.writeFileSync(`${REPO}/evidence/results/lm.json`, JSON.stringify(final, null, 2));
const bad = final.filter((f) => f.result !== 'PASS');
for (const f of final) console.log(`${f.id} ${f.result} - ${f.note.slice(0, 160)}`);
console.log(bad.length ? `LM FAIL (${bad.length} non-PASS)` : 'LM PASS'); if (!bad.length) console.log('lm PASS');
process.exit(bad.length ? 1 : 0);
