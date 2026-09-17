#!/usr/bin/env node
/**
 * E10-e: assemble a distributable release bundle and enforce the 500 MB ceiling.
 *
 * The other two E10 size targets are enforced by `binary-size-check.mjs` on every
 * CI run (per-binary, and the tracked tree). This one covers the third target —
 * the *deliverable* — which cannot be measured from the working tree at all: a
 * developer's checkout legitimately holds tens of GB of gitignored scratch
 * (target/, data/, node_modules/), so "how big is what we hand someone" is only
 * answerable by actually assembling it.
 *
 * The exclusion list is the real specification:
 *
 *   - release binaries + strategy cdylibs   (the product)
 *   - the built web panel                   (needed to serve the UI)
 *   - configs, docs, and the runtime shell  (needed to run and to operate)
 *
 * NOT included, and each for a reason that matters:
 *   - `data/`        real trade history and the market archive — user state, never
 *                    redistributed (and .gitignore'd: it is not ours to ship)
 *   - `node_modules/` reinstallable via `npm ci` from the committed lockfile
 *   - `target/`      intermediate objects; only the linked artifacts ship
 *   - `.git/`, `.mimosa/`, editor/OS noise
 *
 * Usage: node scripts/package-release.mjs [--out <dir>] [--keep]
 * Exit:  0 bundle within budget · 1 over budget (or nothing to pack) · 2 bad input
 *
 * SAFETY — read before touching the `--out` handling.
 * This script DELETES the destination directory before packing, so that a re-run
 * produces a clean bundle rather than a merge of two runs. That deletion destroyed
 * this repository's working tree once (see bk-recovery-20260917/INCIDENT_REPORT.md
 * and MIGRATION_LOG §50): the guard below was written as "allow unless it looks
 * like a source path", and `--out .` slipped through the gap because
 * `resolve('.')` has no trailing separator.
 *
 * The guard is therefore written the OTHER way round, and must stay that way:
 * EVERY destination is refused unless it is inside a scratch directory or under
 * this repo's `target/`. Default is refusal. Do not weaken it to a deny-list.
 */

import { execFileSync } from 'child_process';
import { existsSync, mkdirSync, cpSync, statSync, readdirSync, rmSync, writeFileSync } from 'fs';
import { join, dirname, resolve, sep } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(join(__dirname, '..'));
const MB = 1024 * 1024;
const BUNDLE_BUDGET = Number(process.env.BK_BUNDLE_BUDGET_MB ?? 500) * MB;

const argv = process.argv.slice(2);
const keep = argv.includes('--keep');
const outIdx = argv.indexOf('--out');
const OUT = resolve(outIdx >= 0 ? argv[outIdx + 1] ?? '' : join(tmpdir(), `blitzkrieg-release-${process.pid}`));

if (outIdx >= 0 && !argv[outIdx + 1]) {
  console.error('--out needs a directory');
  process.exit(2);
}

// ── Destination guard (allow-list, default-refuse) ────────────────────────────
{
  // `resolve` normalises away a trailing separator, so compare on normalised
  // values and test the root case EXPLICITLY before any prefix test.
  const isRoot = OUT === ROOT;
  const underTmp = OUT === resolve(tmpdir()) || OUT.startsWith(resolve(tmpdir()) + sep);
  const underTarget = OUT.startsWith(join(ROOT, 'target') + sep);

  if (isRoot || !(underTmp || underTarget)) {
    console.error(`refusing --out ${OUT}`);
    console.error('  the destination is deleted before packing, so only two places are allowed:');
    console.error(`    - a scratch dir under ${resolve(tmpdir())}`);
    console.error(`    - ${join(ROOT, 'target')}/<name>   (gitignored)`);
    process.exit(2);
  }
}

/** Release binaries that must exist — a bundle without the core is not a bundle. */
const REQUIRED_BINARIES = ['blitzkrieg-core'];
/** Built but optional: the UI crates are not built on every platform. */
const OPTIONAL_BINARIES = ['ui_kit_web', 'ui_kit_panel', 'ui_kit_app'];
/** Nested workspaces that build strategy cdylibs. */
const CDYLIB_DIRS = ['user_layer/strategies', 'user_layer/parity_strategy'];

function dirSize(p) {
  let total = 0;
  const stack = [p];
  while (stack.length) {
    const cur = stack.pop();
    let st;
    try { st = statSync(cur); } catch { continue; }
    if (st.isDirectory()) for (const e of readdirSync(cur)) stack.push(join(cur, e));
    else total += st.size;
  }
  return total;
}

const fmt = (b) => `${(b / MB).toFixed(2)} MB`;

// ── Preconditions ────────────────────────────────────────────────────────────
// Checked BEFORE the destination is touched, so a missing build cannot cost you
// a directory.
const missing = REQUIRED_BINARIES.filter((b) => !existsSync(join(ROOT, 'target', 'release', b)));
if (missing.length) {
  console.error(`missing release binaries: ${missing.join(', ')}`);
  console.error('run `cargo build --release --workspace --locked` first');
  process.exit(2);
}

if (existsSync(OUT)) rmSync(OUT, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });
const BUNDLE = join(OUT, 'blitzkrieg');
mkdirSync(join(BUNDLE, 'bin'), { recursive: true });

const included = [];
const absent = [];

function add(label, from, to) {
  if (included.some((r) => r.label === label)) {
    // A repeated destination silently inflates the reported size without adding any
    // bytes, turning the total into a number that cannot be trusted. This gate exists
    // only to produce a trustworthy number, so fail loudly instead.
    throw new Error(`duplicate bundle entry: ${label} (from ${from})`);
  }
  if (!existsSync(from)) {
    // Recorded rather than silently dropped: a runner that never built the panel would
    // otherwise print a small total that reads like the real deliverable.
    absent.push({ label, from });
    return false;
  }
  cpSync(from, to, { recursive: true });
  included.push({ label, bytes: dirSize(to) });
  return true;
}

// ── 1. Binaries ──────────────────────────────────────────────────────────────
for (const b of [...REQUIRED_BINARIES, ...OPTIONAL_BINARIES]) {
  add(`bin/${b}`, join(ROOT, 'target', 'release', b), join(BUNDLE, 'bin', b));
}

// Shared libraries, deduped by filename. The same library can sit in more than one
// source (a nested workspace that also builds against the root copies the ABI shim
// into its own target/), and counting it twice is wrong. The root release dir's
// non-recursive listing is exactly the cdylib products; build-script intermediates
// live under target/release/build/**/out and `cpSync` is not recursive here.
const dylibs = new Map();
for (const base of [
  ...CDYLIB_DIRS.map((d) => join(ROOT, d, 'target', 'release')),
  join(ROOT, 'target', 'release'),
]) {
  if (!existsSync(base)) continue;
  for (const f of readdirSync(base)) {
    if (!/\.(dylib|so|dll)$/.test(f)) continue;
    if (dylibs.has(f)) continue;
    dylibs.set(f, join(base, f));
  }
}
for (const [f, from] of dylibs) add(`lib/${f}`, from, join(BUNDLE, 'lib', f));

// ── 2. Built web panel ───────────────────────────────────────────────────────
add('webui/dist', join(ROOT, 'ui', 'webapp', 'webui', 'dist'), join(BUNDLE, 'webui'));

// ── 3. Configs ───────────────────────────────────────────────────────────────
add('configs', join(ROOT, 'user_layer', 'configs'), join(BUNDLE, 'configs'));

// ── 4. Runtime shell (compiled Node, no node_modules) ────────────────────────
add('shell/dist', join(ROOT, 'dist'), join(BUNDLE, 'shell', 'dist'));
for (const f of ['package.json', 'package-lock.json']) {
  add(`shell/${f}`, join(ROOT, f), join(BUNDLE, 'shell', f));
}

// ── 5. Docs needed to operate it ─────────────────────────────────────────────
for (const f of ['README.md', 'HANDOFF.md']) add(f, join(ROOT, f), join(BUNDLE, f));
add('docs', join(ROOT, 'docs'), join(BUNDLE, 'docs'));

// ── Manifest ─────────────────────────────────────────────────────────────────
// Written BEFORE the final measurement so its own bytes count against the budget.
const rows = included.sort((a, b) => b.bytes - a.bytes);
const sha = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT }).toString().trim();
const dirty = execFileSync('git', ['status', '--porcelain'], { cwd: ROOT }).toString().trim() !== '';
writeFileSync(
  join(BUNDLE, 'MANIFEST.json'),
  JSON.stringify(
    {
      commit: sha,
      worktree_dirty: dirty,
      contents: rows.map((r) => ({ path: r.label, bytes: r.bytes })),
      // Non-empty means this bundle is PARTIAL: the named build steps were not run in
      // the environment that produced it, so `contents` is not the full deliverable.
      missing_from_this_bundle: absent.map((a) => ({ path: a.label, expected_at: a.from })),
      not_included: {
        'data/': 'real trade history and market archive — user state, never redistributed',
        'node_modules/': 'reinstall with `npm ci` from the committed lockfile',
        'target/': 'intermediate build objects; only the linked artifacts are bundled',
      },
      runtime_requirements: ['Node.js >= 22', 'npm ci before running the shell'],
    },
    null,
    2,
  ) + '\n',
);

// ── Report ───────────────────────────────────────────────────────────────────
const total = dirSize(BUNDLE);
const over = total > BUNDLE_BUDGET;
const manifestBytes = total - rows.reduce((s, r) => s + r.bytes, 0);

console.log('─'.repeat(72));
console.log(`release bundle: ${BUNDLE}`);
console.log(`commit ${sha}${dirty ? ' (dirty worktree)' : ''}`);
console.log('─'.repeat(72));
for (const r of rows) console.log(`  ${r.label.padEnd(28)} ${fmt(r.bytes).padStart(11)}`);
console.log(`  ${'MANIFEST.json'.padEnd(28)} ${fmt(manifestBytes).padStart(11)}`);
console.log('─'.repeat(72));
console.log(
  `  ${'TOTAL'.padEnd(28)} ${fmt(total).padStart(11)}  ` +
    `(${((total / BUNDLE_BUDGET) * 100).toFixed(1)}% of the ${fmt(BUNDLE_BUDGET)} budget)`,
);
console.log('─'.repeat(72));

// What was NOT packed, stated plainly. Without this, a partial bundle prints a small
// total that reads like the whole deliverable, and the 500 MB target looks met for
// the wrong reason.
if (absent.length) {
  console.log('not present in this bundle (build step not run in this environment):');
  for (const a of absent) console.log(`  - ${a.label}  (expected at ${a.from})`);
  console.log('  the total above therefore UNDERSTATES the full deliverable.');
  console.log('─'.repeat(72));
}

if (over) {
  console.error(`package-release — the bundle is ${fmt(total - BUNDLE_BUDGET)} over the ${fmt(BUNDLE_BUDGET)} target.`);
  console.error('  Find the offender: the table above is sorted largest-first.');
  process.exit(1);
}
if (rows.length === 0) {
  console.error('package-release — nothing was packed.');
  process.exit(1);
}
console.log(`package-release — bundle is ${fmt(total)}, within the ${fmt(BUNDLE_BUDGET)} budget.`);
console.log(keep ? `  kept at ${BUNDLE}` : '  (pass --keep to preserve it)');
if (!keep) rmSync(OUT, { recursive: true, force: true });
