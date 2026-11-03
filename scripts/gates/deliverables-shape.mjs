#!/usr/bin/env node
// Oracle: evidence/DELIVERABLES.md has >= N distinct deliverables "Dn", each followed by an "Acceptance:" line before the next.
import { readFileSync, existsSync } from 'node:fs';
const min = Number(process.argv[2]);
const f = 'evidence/DELIVERABLES.md';
if (!existsSync(f)) { console.error('missing ' + f); process.exit(1); }
const text = readFileSync(f, 'utf8');
const parts = text.split(/^(?=(?:[-*]\s*)?\*{0,2}D\d+\b)/m).filter((p) => /^(?:[-*]\s*)?\*{0,2}D\d+\b/.test(p));
const ids = new Set(); const bad = [];
for (const p of parts) {
  const id = p.match(/D\d+/)[0];
  if (ids.has(id)) bad.push(`${id}: duplicate`);
  ids.add(id);
  if (!/Acceptance:/i.test(p)) bad.push(`${id}: no Acceptance line`);
}
if (bad.length) { console.error(bad.join('\n')); process.exit(1); }
if (ids.size < min) { console.error(`only ${ids.size} deliverables, need ${min}`); process.exit(1); }
console.log(`DELIVERABLES SHAPE OK ${ids.size}`);
