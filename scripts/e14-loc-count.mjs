#!/usr/bin/env node
/**
 * E14 LOC counter — reproducible rust-core LOC baseline / re-measure.
 *
 * Counts the Rust sources of every MAIN-workspace member (the `[workspace]`
 * members in the root Cargo.toml, NOT the excluded nested strategy
 * workspaces, which are user-layer strategy packages with their own
 * lifecycles). Mirrors `docs/perf/E14.md`'s counting rule so the archived
 * baseline and any later re-run are apples-to-apples:
 *
 *   .rs lines = every line of every .rs file under each member's src/
 *   (blank + comment lines included), excluding generated/vendor trees
 *   (any target dir, any .mimosa dir).
 *
 * Usage: node scripts/e14-loc-count.mjs [--json]
 */
import { execSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';

const MEMBERS = [
  'core/blitzkrieg_core',
  'core/market_api',
  'extensions/polymarket',
  'user_layer/strategy_api',
  'user_layer/strategy_logic',
  'user_layer/parity_logic',
  'ui/ui_kit',
  'ui/ui_kit_panel',
];

function count(dir) {
  const files = execSync(
    `find ${JSON.stringify(dir)}/src -name '*.rs' -not -path '*/target/*' -not -path '*/.mimosa/*'`,
    { encoding: 'utf8' },
  )
    .split('\n')
    .filter(Boolean);
  if (files.length === 0) return { lines: 0, files: 0 };
  const out = execSync(`wc -l ${files.map((f) => JSON.stringify(f)).join(' ')}`, {
    encoding: 'utf8',
  });
  // macOS wc prints `  <n> <file>` per file and `  <n> total` last when given
  // many files; with one file it prints just `  <n> <file>`. Take the last
  // purely-numeric token of the whole output.
  const tokens = out.split(/\s+/).filter(Boolean);
  const numbers = tokens.filter((t) => /^\d+$/.test(t));
  const total = Number(numbers.at(-1) ?? 0);
  return { lines: total, files: files.length };
}

const rows = MEMBERS.map((m) => ({ crate: m, ...count(m) }));
const total = rows.reduce((a, r) => a + r.lines, 0);
const totalFiles = rows.reduce((a, r) => a + r.files, 0);

console.log('E14 LOC baseline (main workspace, src/**/*.rs):');
for (const r of rows) {
  console.log(`  ${r.crate.padEnd(28)} ${String(r.lines).padStart(7)} lines  ${r.files} files`);
}
console.log(`  ${'TOTAL'.padEnd(28)} ${String(total).padStart(7)} lines  ${totalFiles} files`);

if (process.argv.includes('--json')) {
  writeFileSync('docs/perf/e14-loc.json', JSON.stringify({ rows, total, totalFiles }, null, 2));
  console.log('archived → docs/perf/e14-loc.json');
}
