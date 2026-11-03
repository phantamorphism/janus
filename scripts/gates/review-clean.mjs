#!/usr/bin/env node
// Oracle: an independent review file must exist, carry "VERDICT: PASS", and have no open high/medium findings.
// Format the reviewer writes:  VERDICT: PASS|FAIL   and findings as
//   - [ ] [high|medium|low] text     (open)       - [x] [high|medium|low] text   (resolved)
// Usage: node scripts/gates/review-clean.mjs <review.md> [--selftest]
import { readFileSync, existsSync, mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

function judge(text) {
  const errs = [];
  if (!(/(^|\n)VERDICT:\s*PASS\b/.test(text) && !/(^|\n)VERDICT:\s*FAIL\b/.test(text))) errs.push('no "VERDICT: PASS" line');
  if (!/\bREVIEWER:\s*\S+/.test(text)) errs.push('no REVIEWER line');
  const open = [...text.matchAll(/^- \[ \] \[(high|medium)\].*$/gim)].map((m) => m[0]);
  for (const o of open) errs.push(`open finding: ${o.slice(0, 120)}`);
  if (text.length < 400) errs.push('review too short to be a review');
  return errs;
}
const a = process.argv.slice(2);
if (a[0] === '--selftest') {
  const d = mkdtempSync(join(tmpdir(), 'rev-'));
  const body = 'REVIEWER: x\n' + 'lorem '.repeat(100) + '\n';
  const bad = judge(body + 'VERDICT: PASS\n- [ ] [high] open thing\n');
  const bad2 = judge(body + 'VERDICT: FAIL\n');
  const good = judge(body + 'VERDICT: PASS\n- [x] [high] fixed\n- [ ] [low] nit\n');
  rmSync(d, { recursive: true });
  if (bad.length !== 1 || bad2.length !== 1 || good.length !== 0) { console.error('selftest failed', bad, bad2, good); process.exit(1); }
  console.log('REVIEW-CLEAN SELFTEST OK');
  process.exit(0);
}
if (!a[0] || !existsSync(a[0])) { console.error(`review file missing: ${a[0]}`); process.exit(1); }
const errs = judge(readFileSync(a[0], 'utf8'));
if (errs.length) { console.error(errs.join('\n')); process.exit(1); }
console.log('REVIEW CLEAN');
