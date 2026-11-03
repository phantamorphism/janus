#!/usr/bin/env node
// Oracle: vitest reports >= N passed tests and zero failed/skipped.
import { spawnSync } from 'node:child_process';
const min = Number(process.argv[2]);
const r = spawnSync('npx', ['vitest', 'run', '--reporter=json'], { cwd: 'ui', encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
const m = r.stdout.match(/\{[\s\S]*\}\s*$/);
if (!m) { console.error('no json from vitest\n' + r.stderr.slice(-2000)); process.exit(1); }
const j = JSON.parse(m[0]);
if (r.status !== 0 || j.numFailedTests > 0 || j.numPendingTests > 0 || j.numTodoTests > 0) { console.error(`failed=${j.numFailedTests} pending=${j.numPendingTests} todo=${j.numTodoTests}`); process.exit(1); }
if (j.numPassedTests < min) { console.error(`only ${j.numPassedTests} passed, need ${min}`); process.exit(1); }
console.log(`UI TESTS OK ${j.numPassedTests}`);
