#!/usr/bin/env node
// Oracle: run `cargo test <args>` and require zero failures and at least N passing tests.
// Usage: node scripts/gates/cargo-tests.mjs --min N [--require nameA,nameB] -- <cargo test args...>
// --require: each substring must match a test that ran and passed.
// Prints "TESTS OK <passed>" only when every assertion holds.
import { spawnSync } from 'node:child_process';

const argv = process.argv.slice(2);
const minIdx = argv.indexOf('--min');
const sep = argv.indexOf('--');
const reqIdx = argv.indexOf('--require');
const required = reqIdx >= 0 && reqIdx < sep ? argv[reqIdx + 1].split(',').filter(Boolean) : [];
if (minIdx < 0 || sep < 0) { console.error('usage: --min N -- <cargo test args>'); process.exit(2); }
const min = Number(argv[minIdx + 1]);
const cargoArgs = argv.slice(sep + 1);
const r = spawnSync('cargo', ['test', ...cargoArgs], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
const out = `${r.stdout}\n${r.stderr}`;
if (r.status !== 0) { console.error(out.split('\n').slice(-60).join('\n')); console.error(`cargo test exited ${r.status}`); process.exit(1); }
let passed = 0, failed = 0, ignored = 0;
for (const m of out.matchAll(/test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored/g)) {
  if (m[1] !== 'ok') { console.error('non-ok result line'); process.exit(1); }
  passed += Number(m[2]); failed += Number(m[3]); ignored += Number(m[4]);
}
if (failed > 0) { console.error(`${failed} failed`); process.exit(1); }
if (ignored > 0) { console.error(`${ignored} tests ignored: ignored tests do not count as evidence`); process.exit(1); }
if (!(passed >= min)) { console.error(`only ${passed} passed, need at least ${min}`); process.exit(1); }
for (const name of required) {
  const re = new RegExp(`^test .*${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}.* \\.\\.\\. ok$`, 'm');
  if (!re.test(out)) { console.error(`required test not found passing: ${name}`); process.exit(1); }
}
console.log(`TESTS OK ${passed}`);
