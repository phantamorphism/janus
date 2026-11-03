#!/usr/bin/env node
// Oracle: no run of N consecutive significant lines is repeated (in two places) under the scanned dir.
// Usage: node scripts/gates/dup-scan.mjs <dir> <N> | --selftest
import { readdirSync, readFileSync, statSync, mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join, extname } from 'node:path';
import { tmpdir } from 'node:os';
function* walk(p) {
  const st = statSync(p);
  if (st.isFile()) { yield p; return; }
  for (const e of readdirSync(p, { withFileTypes: true })) { if (e.name === 'target' || e.name.startsWith('.')) continue; yield* walk(join(p, e.name)); }
}
function scan(dir, n) {
  const seen = new Map(); const dups = []; let files = 0;
  for (const f of walk(dir)) {
    if (extname(f) !== '.rs') continue;
    files++;
    const lines = readFileSync(f, 'utf8').split('\n').map((l, i) => ({ t: l.trim(), i: i + 1 }))
      .filter((l) => l.t.length > 3 && !/^(\/\/|#\[|use |}|\{|\)|\]|mod |pub mod )/.test(l.t) && !/^[})\]];?,?$/.test(l.t));
    for (let i = 0; i + n <= lines.length; i++) {
      const key = lines.slice(i, i + n).map((l) => l.t).join('\n');
      const prev = seen.get(key);
      if (prev && !(prev.f === f && Math.abs(prev.line - lines[i].i) < n)) dups.push(`${f}:${lines[i].i} duplicates ${prev.f}:${prev.line}`);
      else if (!prev) seen.set(key, { f, line: lines[i].i });
    }
  }
  return { dups, files };
}
const a = process.argv.slice(2);
if (a[0] === '--selftest') {
  const d = mkdtempSync(join(tmpdir(), 'dup-'));
  const block = Array.from({ length: 16 }, (_, i) => `    let value_${i} = compute_something(${i}) + 1;`).join('\n');
  writeFileSync(join(d, 'a.rs'), `fn a() {\n${block}\n}\n`);
  writeFileSync(join(d, 'b.rs'), `fn b() {\n${block}\n}\n`);
  const bad = scan(d, 15);
  writeFileSync(join(d, 'b.rs'), 'fn b() { let x = 1; }\n');
  const good = scan(d, 15);
  rmSync(d, { recursive: true });
  if (bad.dups.length === 0 || good.dups.length !== 0) { console.error('selftest failed'); process.exit(1); }
  console.log('DUP SCAN SELFTEST OK'); process.exit(0);
}
const { dups, files } = scan(a[0], Number(a[1]));
if (files === 0) { console.error('scanned zero files'); process.exit(1); }
if (dups.length) { console.error(dups.slice(0, 30).join('\n')); process.exit(1); }
console.log(`DUP SCAN OK (${files} files)`);
