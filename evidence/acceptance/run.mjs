#!/usr/bin/env node
// Runs every blind-QA group script against target/release (rebuild first), in parallel
// (groups use disjoint port ranges), then merges evidence/results/*.json into evidence/results.json.
// Prints "ACCEPTANCE PASS" only if every group script exits 0 and every deliverable result is PASS.
import { spawn, execFileSync } from 'node:child_process';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';

execFileSync('cargo', ['build', '--release', '--workspace'], { stdio: 'inherit' });
const groups = ['a', 'b', 'cd', 'ef', 'ghn', 'ijk', 'lm'];
const run = (g) => new Promise((res) => {
  const p = spawn('node', [`evidence/acceptance/${g}.mjs`], { env: { ...process.env, JANUS_BIN_DIR: `${process.cwd()}/target/release` } });
  let out = ''; p.stdout.on('data', (d) => (out += d)); p.stderr.on('data', (d) => (out += d));
  p.on('close', (code) => { writeFileSync(`evidence/results/${g}.log`, out); res({ g, code, tail: out.trim().split('\n').slice(-1)[0] }); });
});
const outs = await Promise.all(groups.map(run));
let bad = 0;
for (const o of outs) { console.log(`${o.g}: exit ${o.code} — ${o.tail}`); if (o.code !== 0) bad++; }
const all = readdirSync('evidence/results').filter((f) => f.endsWith('.json') && f !== 'results.json')
  .flatMap((f) => JSON.parse(readFileSync(`evidence/results/${f}`, 'utf8')));
all.sort((x, y) => Number(x.id.slice(1)) - Number(y.id.slice(1)));
writeFileSync('evidence/results.json', JSON.stringify(all, null, 1));
const np = all.filter((r) => r.result !== 'PASS');
for (const r of np) console.log(`NOT PASS ${r.id} ${r.result}`);
console.log(`${all.length} results, ${np.length} not passing`);
if (bad || np.length) { console.error('ACCEPTANCE FAIL'); process.exit(1); }
console.log('ACCEPTANCE PASS');
