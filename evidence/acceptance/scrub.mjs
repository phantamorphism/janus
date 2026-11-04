// Rewrites machine-specific absolute paths in generated evidence (artifacts, results and logs)
// to <repo>, <home> and <user>, so a re-run on any machine leaves no local paths behind.
// run.mjs calls this after every full run; it can also be run by hand:
//   node evidence/acceptance/scrub.mjs
import { readdirSync, readFileSync, writeFileSync, statSync } from 'node:fs';
import { homedir, userInfo } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');

export function scrubPaths(dir) {
  const subs = [[REPO, '<repo>'], [homedir(), '<home>'], [userInfo().username, '<user>']];
  const walk = (d) => {
    for (const name of readdirSync(d)) {
      const p = path.join(d, name);
      if (name === 'acceptance' || name === 'node_modules') continue;
      if (statSync(p).isDirectory()) { walk(p); continue; }
      if (!/\.(json|txt|log|md)$/.test(name)) continue;
      const before = readFileSync(p, 'utf8');
      let after = before;
      for (const [from, to] of subs) after = after.split(from).join(to);
      if (after !== before) writeFileSync(p, after);
    }
  };
  walk(path.resolve(REPO, dir));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) scrubPaths('evidence');
