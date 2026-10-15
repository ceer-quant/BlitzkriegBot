#!/usr/bin/env node
/**
 * E10-e: assemble a distributable release bundle and enforce the 500 MB ceiling.
 *
 * The other two E10 size targets are enforced by `binary-size-check.mjs` on every
 * CI run (per-binary, and the tracked tree). This one covers the third target —
 * the *deliverable* — which cannot be measured from the working tree at all: a
 * developer's checkout legitimately holds tens of GB of gitignored scratch
 * (target/, data/), so "how big is what we hand someone" is only
 * answerable by actually assembling it.
 *
 * The exclusion list is the real specification:
 *
 *   - release binaries + strategy cdylibs   (the product)
 *   - the built web panel                   (needed to serve the UI)
 *   - user_layer configs + Lua strategy packages + the cdylib drop-point
 *     + the extension config drop-point + .env.example + docs
 *                                           (needed to run and to operate)
 *
 * NOT included, and each for a reason that matters:
 *   - `data/`        real trade history and the market archive — user state, never
 *                    redistributed (and .gitignore'd: it is not ours to ship)
 *   - `target/`      intermediate objects; only the linked artifacts ship
 *   - `.git/`, `.mimosa/`, editor/OS noise
 *   - `extensions/<name>/src`  market extensions are compiled INTO the kernel
 *                    binary (default features); the runtime-equivalent part of
 *                    that tree is its config drop-point only (issue 490)
 *
 * The in-bundle layout mirrors a checkout's runtime resolution surface, because
 * the kernel's own path rules are the equivalence spec (issue 490):
 *   - `user_layer/configs/`   cwd-relative (`default.toml`) AND up the exe's
 *                             ancestor chain (`execution_policy.toml`)
 *   - `user_layer/strategies_lua/` + `user_layer/strategies/`   same two rules
 *   - `extensions/`           cwd-relative (`extensions/<name>/config.toml`)
 *   - `webui/`                beside `bin/` (`../webui/index.html`)
 *   - `.env`                  cwd (self-loaded by the launcher; only the
 *                             `.env.example` template ships — a real one holds
 *                             credentials and is never redistributed)
 *
 * This script is the SINGLE packing list: the release workflow's tarball is
 * assembled by calling it (`release.yml`), and the CI size gate runs it too
 * (`ci.yml` release-bundle) — two products, one list, no drift.
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
// --bin-from <dir>: read binaries/cdylibs from a cross-compiled target root
// (e.g. target/aarch64-apple-darwin/release) instead of target/release. The
// release workflow builds with --target, so its artifacts land there; without
// this the archive would carry an EMPTY lib/ and no UI binaries.
const binFromIdx = argv.indexOf('--bin-from');
const BIN_FROM = binFromIdx >= 0 ? resolve(ROOT, argv[binFromIdx + 1] ?? '') : null;

if (outIdx >= 0 && !argv[outIdx + 1]) {
  console.error('--out needs a directory');
  process.exit(2);
}
if (binFromIdx >= 0 && !argv[binFromIdx + 1]) {
  console.error('--bin-from needs a directory');
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
const OPTIONAL_BINARIES = ['ui_kit_web', 'ui_kit_panel', 'ui_kit_app', 'blitzkrieg'];
/** Where strategy cdylibs are built: the drop-point operators add strategies to,
 *  and the reference implementation's nested workspace. */
const CDYLIB_DIRS = ['user_layer/strategies', 'user_layer/parity_strategy'];
/** Binaries/cdylibs are read from here (target/release unless --bin-from). */
const BIN_DIR = BIN_FROM ?? join(ROOT, 'target', 'release');
/** Bundle-relative paths pruned from every copied tree. Build residue, tool
 *  state, and OS noise are not runtime surface — the head comment's exclusion
 *  list names them, this enforces it for the user_layer/extensions copies. */
const PRUNE = ['.mimosa', '.DS_Store'];

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
const missing = REQUIRED_BINARIES.filter((b) => !existsSync(join(BIN_DIR, b)));
if (missing.length) {
  console.error(`missing release binaries: ${missing.join(', ')}`);
  console.error(`looked in: ${BIN_DIR}`);
  console.error('run `cargo build --release --workspace --locked` first');
  process.exit(2);
}

if (existsSync(OUT)) rmSync(OUT, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });
const BUNDLE = join(OUT, 'blitzkrieg');
mkdirSync(join(BUNDLE, 'bin'), { recursive: true });

const included = [];
const absent = [];

function add(label, from, to, transform) {
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
  // Prune BEFORE the row is measured: build residue, tool state and OS noise
  // must not be counted (a row that says 112 MB for a 4 KB drop-point is
  // exactly the untrustworthy number the accounting exists to prevent).
  // Implementation: walk the copied tree once, removing every PRUNE-named
  // directory (and file) at any depth.
  const stack = [to];
  while (stack.length) {
    const cur = stack.pop();
    let entries;
    try { entries = readdirSync(cur, { withFileTypes: true }); } catch { continue; }
    for (const e of entries) {
      if (PRUNE.includes(e.name) || (e.isDirectory() && e.name === 'target')) {
        // `target/` is build residue everywhere it appears inside a shipped
        // source tree (the bin/lib stages read artifacts directly and never
        // recurse); .mimosa/.DS_Store are tool state and OS noise.
        rmSync(join(cur, e.name), { recursive: true, force: true });
        continue;
      }
      if (e.isDirectory()) stack.push(join(cur, e.name));
    }
  }
  // Row-specific reshaping runs before measurement too — any row the caller
  // shrinks after `add()` returns would overstate its bytes here.
  if (transform) transform(to);
  included.push({ label, bytes: dirSize(to) });
  return true;
}

// ── 1. Binaries ──────────────────────────────────────────────────────────────
for (const b of [...REQUIRED_BINARIES, ...OPTIONAL_BINARIES]) {
  add(`bin/${b}`, join(BIN_DIR, b), join(BUNDLE, 'bin', b));
}

// Shared libraries, deduped by filename. The same library can sit in more than one
// source (a nested workspace that also builds against the root copies the ABI shim
// into its own target/), and counting it twice is wrong. The release dir's
// non-recursive listing is exactly the cdylib products; build-script intermediates
// live under target/release/build/**/out and `cpSync` is not recursive here.
// The nested workspaces always build into their OWN target/, never --target.
const dylibs = new Map();
for (const base of [
  ...CDYLIB_DIRS.map((d) => join(ROOT, d, 'target', 'release')),
  BIN_DIR,
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

// ── 3. user_layer runtime surface (issue 490) ────────────────────────────────
// The kernel resolves its runtime surface as CHECKOUT-RELATIVE paths — cwd
// first, then up the executable's ancestor chain — so the bundle carries the
// same `user_layer/…` tree a checkout has. A flat `configs/` at the bundle
// root is NOT equivalent: `default.toml` would never be found (cwd-relative)
// and the execution policy would silently fall back to NEUTRAL.
add('user_layer/configs', join(ROOT, 'user_layer', 'configs'), join(BUNDLE, 'user_layer', 'configs'));
// Lua strategy packages (the source users edit) and the cdylib drop-point
// (empty except its README; the kernel scans both). Both trees carry a
// gitignored target/ when a developer built locally — build residue, not
// runtime surface; it is pruned before the row is measured.
add('user_layer/strategies_lua', join(ROOT, 'user_layer', 'strategies_lua'), join(BUNDLE, 'user_layer', 'strategies_lua'));
add('user_layer/strategies', join(ROOT, 'user_layer', 'strategies'), join(BUNDLE, 'user_layer', 'strategies'));

// ── 4. Extension config drop-point (issue 490) ───────────────────────────────
// Market extensions are compiled into the kernel binary; what the runtime
// reads from `extensions/` is each extension's `config.toml` (cwd-relative,
// `extensions/<name>/config.toml`) — plus the README that explains the
// contract. Source trees stay out of the bundle: they are build inputs, not
// runtime surface. The transform runs INSIDE add() so the row's bytes are
// measured after the reshaping, not before.
add('extensions', join(ROOT, 'extensions'), join(BUNDLE, 'extensions'), (extRoot) => {
  for (const e of readdirSync(extRoot)) {
    const dir = join(extRoot, e);
    if (!statSync(dir).isDirectory()) continue;
    if (!existsSync(join(ROOT, 'extensions', e, 'Cargo.toml'))) continue;
    // An extension crate (build input): replace its tree with the config
    // drop-point only, copying from the SOURCE checkout.
    const srcConfig = join(ROOT, 'extensions', e, 'config.toml');
    const hasConfig = existsSync(srcConfig);
    rmSync(dir, { recursive: true, force: true });
    if (hasConfig) {
      mkdirSync(dir, { recursive: true });
      cpSync(srcConfig, join(dir, 'config.toml'));
    }
  }
});

// ── 5. Environment template (issue 490) ──────────────────────────────────────
// The launcher self-loads `<cwd>/.env`. Only the EXAMPLE ships: a real .env
// holds wallet/API/panel credentials and is never redistributed.
add('.env.example', join(ROOT, '.env.example'), join(BUNDLE, '.env.example'));

// ── 6. Gate driver client (bare Node stdlib, no node_modules) ────────────────
// The Node verification layer was removed; production is 100% Rust. The client
// ships so the acceptance gates in this bundle's source tree still run.
add('scripts/lib', join(ROOT, 'scripts', 'lib'), join(BUNDLE, 'scripts', 'lib'));

// ── 7. Docs needed to operate it ─────────────────────────────────────────────
// HANDOFF.md is deliberately absent: it is operator-local and untracked, so it
// may not exist in a clean checkout. `add()` records missing files rather than
// throwing, so listing it here would silently ship a bundle without the entry
// point it promises.
for (const f of ['README.md', 'LICENSE']) add(f, join(ROOT, f), join(BUNDLE, f));
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
        'target/': 'intermediate build objects; only the linked artifacts are bundled',
        '.env': 'credentials (wallet/API/panel) — copy .env.example and fill it in locally',
        'extensions/*/src': 'market extensions are compiled into bin/blitzkrieg-core; only their config drop-point ships',
      },
      runtime_requirements: ['Node.js >= 22 (gate drivers only; runtime is 100% Rust)'],
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
