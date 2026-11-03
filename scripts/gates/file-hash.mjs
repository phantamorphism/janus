#!/usr/bin/env node
// Oracle: a file's sha256 equals the expected digest. Usage: node scripts/gates/file-hash.mjs <file> <sha256>
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
const [f, want] = process.argv.slice(2);
const got = createHash('sha256').update(readFileSync(f)).digest('hex');
if (got !== want) { console.error(`hash mismatch for ${f}: got ${got}`); process.exit(1); }
console.log(`FILE HASH OK ${f}`);
