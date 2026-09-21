/**
 * Strategy cdylib freshness — the guard against a gate that asserts STALE code (#207).
 *
 * The incident this exists to prevent: the kernel carries no strategy code. Every
 * strategy — the shipped `spread_arb` / `trend_follow` / `mean_reversion` legs and
 * the example `dog` dip buyer — is a cdylib the kernel `dlopen`s out of
 * `user_layer/strategies/target/release/` (`Core::load_strategy_dir`, resolving
 * `user_layer/strategies` from the cwd or from the binary's own path). The gate
 * scripts, though, build and drive `target/release/blitzkrieg-core` — the ROOT
 * workspace — and nothing ever checked that the cdylibs the kernel would load were
 * built from the sources in this checkout.
 *
 * So a change to `user_layer/strategy_logic/src/mean_reversion.rs` (the trend gate,
 * say) left `mean-reversion-check.mjs` green until someone happened to run
 * `cd user_layer/strategies && cargo build --release`. A gate that passes on the
 * previous build of the code is not evidence, it is the opposite: it reports a
 * conclusion about code it never ran. This is the same defect class as #172/#179
 * (`lib/core-provenance.mjs`), one layer down — the binary's revision was pinned,
 * the plugins it loads were not.
 *
 * What this module does: for a given checkout, compare the newest mtime under the
 * strategy SOURCE trees against the mtime of each cdylib the kernel would load.
 * Any cdylib older than the newest source — or missing outright — is a hard RED
 * with the fix named in the message. Never a skip, never a warning: "no dylib" is
 * not "no strategy to test", it is "the thing under test is not there".
 *
 * Two deliberate choices:
 *   * it compares mtimes, not content. A hash of the source would be exact but
 *     would need cargo's own fingerprinting to be re-derived here; mtime is what
 *     cargo itself uses, so it moves exactly when cargo would rebuild. The cost is
 *     the familiar one: touching a source without rebuilding is reported as stale,
 *     which is precisely the report we want.
 *   * it does not know which cdylibs a given gate needs beyond what the caller
 *     names. `requireFreshStrategyDylibs({ require: ['mean_reversion_strategy'] })`
 *     is the sharp form; the bare call only asserts "every cdylib present is newer
 *     than every source", which catches the stale case for all of them.
 *
 * Usage (a gate's first statement, before it spawns anything):
 *
 *   import { requireFreshStrategyDylibs } from './lib/strategy-dylib-freshness.mjs';
 *   requireFreshStrategyDylibs({ gate: 'mean-reversion-check', require: ['mean_reversion_strategy'] });
 *
 * or, when the gate has its own `check(name, ok, detail)` reporting:
 *
 *   const problems = checkStrategyDylibsFresh({ gate: '…' });
 */

import { existsSync, readdirSync, realpathSync, statSync } from 'fs';
import { dirname, join, relative, resolve } from 'path';
import { fileURLToPath } from 'url';

/** Repository root, derived from this module's own location (scripts/lib/..). */
const DEFAULT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

/** The nested workspace whose members are the cdylibs (own Cargo.lock). */
const STRATEGY_WORKSPACE = join('user_layer', 'strategies');

/**
 * Every tree a strategy cdylib is compiled from. The members of the nested
 * workspace, plus the two path-dependency crates they link (`strategy-logic` is
 * where the entry rules live, which is the file #207's reproduction edits).
 */
const SOURCE_ROOTS = [
  STRATEGY_WORKSPACE,
  join('user_layer', 'strategy_logic'),
  join('user_layer', 'strategy_api'),
];

/**
 * Which of those trees each shipped cdylib is actually compiled from.
 *
 * Kept explicit rather than "everything is an input to everything": `dog` is the
 * third-party template and deliberately links no `strategy-logic`, so a change to
 * the entry rules must not report its library as stale — a false positive here
 * teaches people to ignore the check, which is the failure mode this whole module
 * exists to prevent. The workspace manifest and Cargo.lock stay in every set: they
 * are inputs to all four builds.
 *
 * A cdylib with no entry falls back to ALL source roots (conservative: an unknown
 * library gets the strictest check, not the loosest).
 */
const CRATE_SOURCES = {
  dog_strategy: [STRATEGY_WORKSPACE, join('user_layer', 'strategy_api')],
  spread_arb_strategy: SOURCE_ROOTS,
  trend_follow_strategy: SOURCE_ROOTS,
  mean_reversion_strategy: SOURCE_ROOTS,
};

/** Directories never walked: build output, VCS metadata, editor scratch. */
const SKIP_DIRS = new Set(['target', 'node_modules', '.git', 'deps', 'build', 'incremental']);

const DYLIB_EXTENSIONS = ['.dylib', '.so', '.dll'];

const isDylibName = (name) => DYLIB_EXTENSIONS.some((ext) => name.endsWith(ext));

/** The three platform spellings of one crate's cdylib. */
function dylibNamesFor(crate) {
  return [`lib${crate}.dylib`, `${crate}.dll`, `lib${crate}.so`];
}

/** `libmean_reversion_strategy.dylib` → `mean_reversion_strategy`. */
function crateNameOf(fileName) {
  const base = fileName.replace(/\.(dylib|so|dll)$/, '');
  return base.startsWith('lib') ? base.slice(3) : base;
}

function walkFiles(dir, out, depth = 0) {
  if (depth > 8) return out;
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name) || entry.name.startsWith('.')) continue;
      walkFiles(path, out, depth + 1);
    } else if (entry.isFile()) {
      out.push(path);
    }
  }
  return out;
}

function mtimeMsOf(path) {
  try {
    return statSync(path).mtimeMs;
  } catch {
    return null;
  }
}

/** The newest input mtime across `roots`, as `{ path, mtimeMs }` or null. */
function newestSourceUnder(root, roots) {
  let newest = null;
  for (const rel of roots) {
    const abs = join(root, rel);
    if (!existsSync(abs)) continue;
    for (const file of walkFiles(abs, [])) {
      const ms = mtimeMsOf(file);
      if (ms === null) continue;
      if (newest === null || ms > newest.mtimeMs) newest = { path: file, mtimeMs: ms };
    }
  }
  return newest;
}

/** `2026-09-21T13:40:02.123Z` — comparable at a glance, locale-independent. */
const stamp = (ms) => (ms === null ? 'missing' : new Date(ms).toISOString());

/**
 * Where the kernel under test would look for cdylibs.
 *
 * Mirrors `default_strategy_dir()` in `core/blitzkrieg_core/src/main.rs`: the
 * cwd first (gates spawn the core in a scratch dir, so this never hits), then the
 * binary's own parent chain — which is what makes `BK_CORE_BIN` point at a
 * different checkout work here too.
 */
export function strategyDirFor({ root = DEFAULT_ROOT, binPath = null } = {}) {
  const fromRoot = join(root, STRATEGY_WORKSPACE);
  if (existsSync(fromRoot)) return fromRoot;
  if (binPath) {
    let cur = dirname(resolve(binPath));
    for (let i = 0; i < 8 && cur !== dirname(cur); i++) {
      const candidate = join(cur, STRATEGY_WORKSPACE);
      if (existsSync(candidate)) return candidate;
      cur = dirname(cur);
    }
  }
  return fromRoot;
}

/**
 * The freshness report, as data. Pure inspection — no printing, no exit — so a
 * gate can fold `problems` into its own assertion count and a caller can log the
 * evidence either way.
 *
 * `require` is a list of crate names (`mean_reversion_strategy`) whose cdylib MUST
 * exist: a gate that drives that leg cannot pass by finding no library, because
 * the kernel reports "no strategy registered" and the gate's real assertions then
 * fail for a reason that has nothing to do with what it claims to test.
 */
export function strategyDylibReport({ root = DEFAULT_ROOT, binPath = null, require: required = [] } = {}) {
  // Canonicalize once: a worktree reached through a symlink (`/tmp` on macOS is
  // `/private/tmp`) would otherwise make every printed path look unrelated to the
  // root the caller passed.
  try {
    root = realpathSync(root);
  } catch {
    /* keep the caller's spelling; the missing-dir branch below reports it */
  }
  const dir = strategyDirFor({ root, binPath });
  const problems = [];

  if (!existsSync(dir)) {
    return {
      dir,
      root,
      dylibs: [],
      problems: [
        `找不到策略目录 ${dir} — 内核从这里 dlopen 策略 cdylib（user_layer/strategies）。` +
          ' 请先 `cd user_layer/strategies && cargo build --release`。',
      ],
    };
  }

  // Every cdylib the kernel would load out of the release dir, each paired with
  // the newest input of its OWN dependency set.
  const releaseDir = join(dir, 'target', 'release');
  let names = [];
  try {
    names = readdirSync(releaseDir).filter(isDylibName).sort();
  } catch {
    names = [];
  }
  const dylibs = names.map((name) => {
    const path = join(releaseDir, name);
    const crate = crateNameOf(name);
    const roots = CRATE_SOURCES[crate] ?? SOURCE_ROOTS;
    return { name, crate, path, mtimeMs: mtimeMsOf(path), sources: roots, newestSource: newestSourceUnder(root, roots) };
  });

  const rebuild = `cd ${relative(root, join(dir))} && cargo build --release`;

  if (dylibs.length === 0) {
    problems.push(
      `策略 cdylib 不存在：${releaseDir} 下没有任何 .dylib/.so —— 门禁要驱动的策略尚未构建，` +
        `不能当作「没有策略可测」而放行。请先 \`${rebuild}\`。`,
    );
  }

  for (const crate of required) {
    const want = dylibNamesFor(crate);
    if (!dylibs.some((d) => want.includes(d.name))) {
      problems.push(
        `门禁要驱动的策略库缺失：${join(releaseDir, want[0])}（或 .so/.dll）不存在。` +
          ` 请先 \`${rebuild}\`。`,
      );
    }
  }

  for (const d of dylibs) {
    if (d.mtimeMs === null || d.newestSource === null) continue;
    if (d.mtimeMs < d.newestSource.mtimeMs) {
      problems.push(
        `策略 dylib 陈旧（stale）：${d.path} 构建于 ${stamp(d.mtimeMs)}，` +
          `但它的源码 ${relative(root, d.newestSource.path)} 更新于 ${stamp(d.newestSource.mtimeMs)}。` +
          ` 内核加载的是这个 dylib，门禁断言的却是源码 —— 结果不可信。` +
          ` 请先 \`${rebuild}\` 再跑门禁。`,
      );
    }
  }

  return { dir, root, dylibs, problems };
}

/**
 * Print the evidence (every cdylib the kernel will load, with its path and mtime)
 * and return the problems found. `check` is optional: when given, each problem is
 * also reported through the caller's own assertion function so it lands in that
 * gate's failure count and formatting.
 *
 * The evidence line is deliberate — #207's whole complaint is that nothing in the
 * output said WHICH library was under test, so a stale one could not be told from
 * a fresh one after the fact.
 */
export function checkStrategyDylibsFresh({ root = DEFAULT_ROOT, binPath = null, require: required = [], gate = null, check = null } = {}) {
  const report = strategyDylibReport({ root, binPath, require: required });
  const label = gate ? `${gate}: ` : '';
  console.log(`${label}strategy cdylibs under test (${report.dir}):`);
  if (report.dylibs.length === 0) {
    console.log('  (none found)');
  } else {
    for (const d of report.dylibs) {
      const rel = relative(report.root, d.path);
      const src = d.newestSource ? relative(report.root, d.newestSource.path) : '(no source found)';
      console.log(`  ${rel}  mtime=${stamp(d.mtimeMs)}  <- newest input ${src}`);
    }
  }
  if (report.problems.length === 0) {
    console.log('  freshness: ok (every cdylib is newer than the newest input it is built from)');
  } else {
    for (const p of report.problems) {
      console.log(`  STALE ${p}`);
      check?.('strategy cdylibs are built from this checkout (#207)', false, p);
    }
  }
  return report.problems;
}

/**
 * The hard form: print the evidence and EXIT 1 on any problem, before the gate
 * spawns a core. A gate that cannot state which library it is driving must not
 * report a verdict at all.
 */
export function requireFreshStrategyDylibs(opts = {}) {
  const problems = checkStrategyDylibsFresh(opts);
  if (problems.length === 0) return;
  const name = opts.gate ? `${opts.gate}: ` : '';
  console.log(`\n${name}FAIL — 策略 dylib 与源码不同步（issue #207）：门禁会断言陈旧代码，结论无效。`);
  console.log(`  ${problems.length} problem(s) above; rebuild the cdylibs and re-run.`);
  process.exit(1);
}

/** Path of one crate's cdylib inside a checkout — for a gate that names it. */
export function strategyDylibPath(crate, { root = DEFAULT_ROOT } = {}) {
  const dir = join(strategyDirFor({ root }), 'target', 'release');
  const candidates = dylibNamesFor(crate).map((n) => join(dir, n));
  return candidates.find((p) => existsSync(p)) ?? candidates[0];
}

export { DEFAULT_ROOT as REPO_ROOT, SOURCE_ROOTS };
