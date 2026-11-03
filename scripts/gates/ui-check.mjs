#!/usr/bin/env node
// Oracle: ui type-check, lint, tests and production build all succeed.
import { spawnSync } from 'node:child_process';
const steps = [['npx', ['tsc', '-b']], ['npm', ['run', 'lint']], ['npm', ['test', '--', '--run']], ['npm', ['run', 'build']]];
for (const [c, a] of steps) {
  const r = spawnSync(c, a, { cwd: 'ui', encoding: 'utf8' });
  if (r.status !== 0) { console.error(`${c} ${a.join(' ')} failed\n${(r.stdout + r.stderr).split('\n').slice(-40).join('\n')}`); process.exit(1); }
}
console.log('UI CHECK OK');
