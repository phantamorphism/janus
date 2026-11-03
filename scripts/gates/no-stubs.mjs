#!/usr/bin/env node
// Oracle: fail if owned source paths contain stub/placeholder/suppression markers.
// Usage: node scripts/gates/no-stubs.mjs [--selftest] <dir-or-file>...
import { readdirSync, readFileSync, statSync, mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join, extname } from 'node:path';
import { tmpdir } from 'node:os';

const PATTERNS = [
  [/\btodo!\s*\(/, 'todo!()'],
  [/\bunimplemented!\s*\(/, 'unimplemented!()'],
  [/\bTODO\b/, 'TODO'],
  [/\bFIXME\b/, 'FIXME'],
  [/\bXXX\b/, 'XXX'],
  [/not yet implemented/i, '"not yet implemented"'],
  [/not implemented/i, '"not implemented"'],
  [/#!?\[allow\((dead_code|unused[a-z_]*)\)\]/, 'allow(dead_code/unused)'],
  [/@ts-ignore|@ts-nocheck|eslint-disable/, 'ts/eslint suppression'],
  [/\bplaceholder\b/i, 'placeholder'],
];
const EXT = new Set(['.rs', '.ts', '.tsx', '.js', '.mjs', '.toml', '.proto']);

function* walk(p) {
  const st = statSync(p, { throwIfNoEntry: false });
  if (!st) { throw new Error(`path does not exist: ${p}`); }
  if (st.isFile()) { yield p; return; }
  for (const e of readdirSync(p, { withFileTypes: true })) {
    if (e.name === 'node_modules' || e.name === 'target' || e.name === 'dist' || e.name.startsWith('.')) continue;
    yield* walk(join(p, e.name));
  }
}
function scan(paths) {
  const hits = []; let files = 0;
  for (const p of paths) for (const f of walk(p)) {
    if (!EXT.has(extname(f))) continue;
    files++;
    readFileSync(f, 'utf8').split('\n').forEach((line, i) => {
      for (const [re, name] of PATTERNS) if (re.test(line)) hits.push(`${f}:${i + 1}: ${name}: ${line.trim().slice(0, 100)}`);
    });
  }
  return { hits, files };
}

let args = process.argv.slice(2);
if (args[0] === '--selftest') {
  const d = mkdtempSync(join(tmpdir(), 'nostubs-'));
  writeFileSync(join(d, 'bad.rs'), 'fn a() { todo!() }\n');
  const bad = scan([d]);
  writeFileSync(join(d, 'bad.rs'), 'fn a() -> u8 { 1 }\n');
  const good = scan([d]);
  rmSync(d, { recursive: true });
  if (bad.hits.length !== 1 || good.hits.length !== 0) { console.error('selftest failed'); process.exit(1); }
  console.log('NO-STUBS SELFTEST OK');
  process.exit(0);
}
if (!args.length) { console.error('no paths'); process.exit(2); }
const { hits, files } = scan(args);
if (files === 0) { console.error('scanned zero files: wrong path?'); process.exit(1); }
if (hits.length) { console.error(hits.join('\n')); process.exit(1); }
console.log(`NO STUBS OK (${files} files)`);
