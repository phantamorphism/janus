#!/usr/bin/env node
// Oracle: every deliverable id in evidence/DELIVERABLES.md has a PASS entry in evidence/results.json
// whose artifact files exist and are non-empty. results.json: [{id, result:"PASS"|"FAIL", artifacts:[paths], note}]
// Usage: node scripts/gates/evidence-coverage.mjs [--selftest]
import { readFileSync, existsSync, statSync } from 'node:fs';

export function check(deliverablesText, results, exists, size) {
  const ids = [...new Set([...deliverablesText.matchAll(/^\s*(?:[-*]\s*)?\*{0,2}(D\d+)\b/gm)].map((m) => m[1]))];
  const errs = [];
  if (ids.length === 0) errs.push('no deliverable ids (Dn) found');
  const byId = new Map(results.map((r) => [r.id, r]));
  for (const id of ids) {
    const r = byId.get(id);
    if (!r) { errs.push(`${id}: no result`); continue; }
    if (r.result !== 'PASS') errs.push(`${id}: result is ${r.result}`);
    if (!Array.isArray(r.artifacts) || r.artifacts.length === 0) errs.push(`${id}: no artifacts`);
    for (const f of r.artifacts ?? []) if (!exists(f) || size(f) === 0) errs.push(`${id}: artifact missing or empty: ${f}`);
  }
  return { ids, errs };
}
if (process.argv[2] === '--selftest') {
  const d = '- D1 first\n- D2 second\n';
  const ex = (f) => f === 'a'; const sz = () => 5;
  const g = check(d, [{ id: 'D1', result: 'PASS', artifacts: ['a'] }, { id: 'D2', result: 'PASS', artifacts: ['a'] }], ex, sz);
  const b1 = check(d, [{ id: 'D1', result: 'PASS', artifacts: ['a'] }], ex, sz);
  const b2 = check(d, [{ id: 'D1', result: 'PASS', artifacts: ['a'] }, { id: 'D2', result: 'PASS', artifacts: ['zz'] }], ex, sz);
  if (g.errs.length !== 0 || b1.errs.length !== 1 || b2.errs.length !== 1) { console.error('selftest failed'); process.exit(1); }
  console.log('EVIDENCE-COVERAGE SELFTEST OK'); process.exit(0);
}
if (!existsSync('evidence/DELIVERABLES.md') || !existsSync('evidence/results.json')) { console.error('evidence/DELIVERABLES.md or evidence/results.json missing'); process.exit(1); }
const { ids, errs } = check(readFileSync('evidence/DELIVERABLES.md', 'utf8'), JSON.parse(readFileSync('evidence/results.json', 'utf8')), existsSync, (f) => statSync(f).size);
if (errs.length) { console.error(errs.join('\n')); process.exit(1); }
console.log(`EVIDENCE COVERAGE OK ${ids.length}/${ids.length}`);
