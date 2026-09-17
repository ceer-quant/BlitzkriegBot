#!/usr/bin/env node
/**
 * E10-d size budget: the shipped artifacts must stay small enough to distribute.
 *
 * The v0.2 plan sets three measurable targets, and this gate enforces the two
 * that a build can actually regress:
 *
 *   - single-platform binary  ≤ 50 MB   (per binary, release, after strip+lto)
 *   - project body            ≤ 100 MB  (tracked files, i.e. what a clone pulls)
 *
 * The third target (whole deliverable ≤ 500 MB) is a packaging concern: `target/`
 * and `data/` are machine-generated and gitignored, so they are reported for
 * information but never gated — failing on them would make this gate fail on any
 * machine that had ever been built.
 *
 * Why this exists: the profile settings in the root `Cargo.toml` are the only
 * thing keeping the binaries small, and a single careless dependency (or a
 * well-meaning `strip = true` removal) can undo them silently. `cargo build`
 * gives no signal whatsoever that a binary tripled.
 *
 * Usage: node scripts/binary-size-check.mjs [--verbose]
 * Exit:  0 all budgets met · 1 a budget was exceeded · 2 the build is missing
 */

import { execFileSync } from 'child_process';
import { statSync, existsSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const VERBOSE = process.argv.includes('--verbose');

const MB = 1024 * 1024;
// Overridable so a packaging job can tighten them, and so the failure path is
// testable (a budget that cannot fail is not a gate).
const BINARY_BUDGET = Number(process.env.BK_BINARY_BUDGET_MB ?? 50) * MB;
const BODY_BUDGET = Number(process.env.BK_BODY_BUDGET_MB ?? 100) * MB;

/** Release binaries that ship. Missing ones are skipped: not every platform
 *  builds the Tauri/UI crates, and this gate must not invent a failure for a
 *  target the machine legitimately did not build. */
const BINARIES = ['blitzkrieg-core', 'ui_kit_web', 'ui_kit_panel', 'ui_kit_app'];

function fmt(bytes) {
  return `${(bytes / MB).toFixed(2)} MB`;
}

/** Human-readable reason a binary should not be assumed benign. */
function profileNote() {
  return 'root Cargo.toml [profile.release]: strip="debuginfo" + lto="fat" + codegen-units=1';
}

let failed = false;

// ── 1. Per-binary budget ─────────────────────────────────────────────────────
console.log('─'.repeat(72));
console.log(`binary budget: ${fmt(BINARY_BUDGET)} each  (${profileNote()})`);
console.log('─'.repeat(72));

let checked = 0;
for (const name of BINARIES) {
  const p = join(ROOT, 'target', 'release', name);
  if (!existsSync(p)) {
    if (VERBOSE) console.log(`  skip   ${name.padEnd(20)} (not built)`);
    continue;
  }
  const size = statSync(p).size;
  checked++;
  const over = size > BINARY_BUDGET;
  const pct = ((size / BINARY_BUDGET) * 100).toFixed(1);
  console.log(
    `  ${over ? 'FAIL' : 'ok  '}   ${name.padEnd(20)} ${fmt(size).padStart(10)}  ` +
      `(${pct.padStart(5)}% of budget)`,
  );
  if (over) {
    failed = true;
    console.error(
      `         ${name} is ${fmt(size - BINARY_BUDGET)} over the ${fmt(BINARY_BUDGET)} target.`,
    );
    console.error(`         Verify ${profileNote()} is still in effect, and that a`);
    console.error('         new dependency did not pull in a large static table.');
  }
}
if (checked === 0) {
  console.error('\nno release binaries found — run `cargo build --release` first');
  process.exit(2);
}

// ── 2. Tracked project body ──────────────────────────────────────────────────
// Measured with `git ls-files`, NOT `du` on the working tree: the target is
// "what a clone pulls", and the working tree legitimately contains multi-GB
// gitignored scratch (target/, data/, node_modules/).
console.log('─'.repeat(72));
console.log(`project body budget: ${fmt(BODY_BUDGET)}  (tracked files = what a clone pulls)`);
console.log('─'.repeat(72));

/** Bytes actually stored in the repo for the tracked paths — the clone cost. */
function trackedBodyBytes() {
  // `git ls-files -s` gives the blob hash + mode; `git cat-file --batch-check`
  // then reports each blob's true stored size. This is exactly what a fresh
  // clone downloads (before packfile compression), so it needs no working-tree
  // access and is stable regardless of local scratch.
  const listed = execFileSync('git', ['ls-files', '-s'], { cwd: ROOT, maxBuffer: 1 << 30 })
    .toString()
    .split('\n')
    .filter(Boolean);
  const hashes = listed.map((l) => l.split(/\s+/)[1]).filter(Boolean);
  if (hashes.length === 0) return { bytes: 0, files: 0 };
  const out = execFileSync('git', ['cat-file', '--batch-check=%(objectsize)'], {
    cwd: ROOT,
    input: hashes.join('\n') + '\n',
    maxBuffer: 1 << 30,
  })
    .toString()
    .split('\n')
    .filter(Boolean);
  const bytes = out.reduce((s, l) => s + (parseInt(l, 10) || 0), 0);
  return { bytes, files: hashes.length };
}

const { bytes: body, files } = trackedBodyBytes();
const bodyOver = body > BODY_BUDGET;
console.log(
  `  ${bodyOver ? 'FAIL' : 'ok  '}   tracked body ${fmt(body).padStart(10)}  ` +
    `across ${files} files (${((body / BODY_BUDGET) * 100).toFixed(1)}% of budget)`,
);
if (bodyOver) {
  failed = true;
  console.error(`         The tracked tree is ${fmt(body - BODY_BUDGET)} over the target.`);
  console.error('         Find the offender: git ls-files -s | sort -k4');
  console.error('         Large binaries belong in a GitHub release, not the tree.');
}

// ── 3. Informational: working-tree entropy (never gated) ─────────────────────
if (VERBOSE) {
  console.log('─'.repeat(72));
  console.log('working tree (informational — machine-generated, gitignored, never gated)');
  console.log('─'.repeat(72));
  for (const d of ['target', 'data', 'node_modules']) {
    const p = join(ROOT, d);
    if (!existsSync(p)) continue;
    try {
      const s = execFileSync('du', ['-sh', p], { cwd: ROOT }).toString().trim();
      console.log(`  ${s.split('\t')[0].padStart(8)}  ${d}/`);
    } catch {
      // du is not guaranteed to exist; this section is informational only.
    }
  }
}

console.log('─'.repeat(72));
if (failed) {
  console.error('binary-size-check — a size budget was exceeded.');
  process.exit(1);
}
console.log(`binary-size-check — all size budgets met (binaries ≤ ${fmt(BINARY_BUDGET)}, body ≤ ${fmt(BODY_BUDGET)}).`);
