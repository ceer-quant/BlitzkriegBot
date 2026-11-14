#!/usr/bin/env node
/**
 * store-layer-check — the store-layering CI gate (issue #1199 acceptance).
 *
 * The #1199 end-state: runtime state has ONE write path — the `StoreApi`
 * trait (`core/blitzkrieg_core/src/store/`), with the env backend and the
 * in-memory backend as interchangeable implementations, and "swap the
 * backend = change one assembly line, business code untouched". Two layering
 * rules make that promise enforceable, and this gate is their arm:
 *
 *   [env]  Outside the allowlist below, core/blitzkrieg_core/src/ may not
 *          write runtime state directly:
 *            - std::env::set_var / remove_var (process-global env mutation);
 *            - env_file::upsert_env_vars / remove_env_vars / write_atomic
 *              (.env file writes — the EnvStore's own primitives);
 *            - the env_persist_* family (account/config env writers);
 *            - .account_env_write_paths() calls (callers reaching for the
 *              .env path to write it themselves);
 *            - persist_cockpit_changes (cockpit settings env writer).
 *          `#[cfg(test)]` modules are exempt: env-manipulating test code is
 *          a fixture, not a runtime write path (and the Rust suite itself
 *          is the coverage for those seams).
 *   [sql]  Outside the allowlist, no direct rusqlite use: business code
 *          talks to `db::` functions (or the store seam); only the store
 *          implementations open connections themselves. Test modules are
 *          exempt for the same reason.
 *
 * Ratchet semantics — the baseline (scripts/store-layer-baseline.txt) is a
 * SNAPSHOT of the violations that exist on ceer/main today (the state issue
 * #1199 describes). The gate reds on NEW violations only; the existing debt
 * is carried, visibly, until the StoreApi migration retires it and the
 * baseline is narrowed with --write-baseline. The baseline is debt, not
 * licence: it only shrinks.
 *
 * Scope notes (cross-checked against the #1199 migration map,
 * dev-docs/1199-store-migration-map.md on the wt-1199-default-open-db line):
 *   - The 25 direct .env write calls (upsert_env_vars / remove_env_vars)
 *     the map enumerates are ALL in the baseline or allowlisted: the 9
 *     account-book ones live in account/config/book.rs, which is the
 *     allowlisted env-writer primitive module (the future EnvStore face);
 *     the other 16 are carried as business-layer debt (exactly the list
 *     the migration retires).
 *   - NON-env business-file writes (risk-state.json, strategy-state.json,
 *     data/runtime/config.json) are OUT of this gate's scope by design:
 *     the issue's red line is env writes + direct SQL. If PR-1 brings the
 *     "panel settings" domain into the trait, extend PATTERNS here in that
 *     PR (and say so in its body).
 *   - The session.rs .env watcher (ipc/server/session.rs:222-262) is a READ
 *     path (mtime poll → cockpit re-derive): no write pattern matches it,
 *     and it must stay that way. It is part of the PR-2 retirement face
 *     (asymmetry A2 in the map) — do not "clean it up" by deleting it.
 *
 * Usage:
 *   node scripts/store-layer-check.mjs                 # the real verdict
 *   node scripts/store-layer-check.mjs --self-test     # teeth on fixtures, no tree
 *   node scripts/store-layer-check.mjs --write-baseline
 *                                                        # rewrite the snapshot
 *   node scripts/store-layer-check.mjs --baseline <f>  # judge against a file
 *   node scripts/store-layer-check.mjs --baseline <sha>
 *                                                        # judge against the file
 *                                                        # as of that revision
 *
 * Exit: 0 pass / 1 violations.
 */

import { execFileSync } from 'child_process';
import { readFileSync, writeFileSync } from 'fs';
import { resolve, dirname, join, relative } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BASELINE = resolve(ROOT, 'scripts', 'store-layer-baseline.txt');

// Scanned trees (repo-relative). blitzkrieg_core is the business layer; the
// launcher is the assembly point (main.rs is allowlisted for exactly that
// reason — wiring is not a violation).
const SCAN_DIRS = ['core/blitzkrieg_core/src', 'core/blitzkrieg_launcher/src'];

// ── the allowlist: the only places allowed to write runtime state ────────────
// Each entry is a path prefix or exact path (repo-relative), with the reason
// it is here. An allowlist entry is licence for ONE named seam, not for a
// layer: adding to this list in a PR that is not the store migration is a
// design decision and belongs in the PR body.
const ALLOW = [
  // The store implementations themselves — the whole point of #1199. The
  // trait module (store/, from the feat/1199-store-trait-env line) and the
  // current env-write primitives it will absorb.
  { path: 'core/blitzkrieg_core/src/store/', why: 'StoreApi trait + its implementations (#1199) — the one sanctioned write path' },
  // The .env write primitives the EnvStore is built on: the single module
  // allowed to touch the .env file (temp+rename, 0600, key-name-only logs).
  { path: 'core/blitzkrieg_core/src/env_file.rs', why: '.env write primitives (upsert/remove/parse) — the file-format owner' },
  // Account config's env read/write face (the env_persist_* family lives
  // here): the account-domain EnvStore before the trait absorbs it.
  { path: 'core/blitzkrieg_core/src/account/config/', why: 'account config env read/write primitives (env_persist_* family)' },
  // The SQLite store implementation: db/ opens/manipulates the database;
  // risk/account_store.rs is the durable per-account risk/exit store seam;
  // db_import.rs is the one-shot importer (its own connection by design).
  { path: 'core/blitzkrieg_core/src/db/', why: 'SQLite store implementation — the DB layer itself' },
  { path: 'core/blitzkrieg_core/src/db_import.rs', why: 'one-shot legacy-file importer (opens its own connection)' },
  { path: 'core/blitzkrieg_core/src/risk/account_store.rs', why: 'per-account risk/exit durable store seam (db::Database behind it)' },
  // The composition root: main.rs wires config paths (which store, which
  // .env). Assembly is not a business-layer write.
  { path: 'core/blitzkrieg_launcher/src/main.rs', why: 'launcher assembly — wires the env/db paths' },
];

// ── violation patterns ───────────────────────────────────────────────────────
// Each: a RegExp over ONE LINE of Rust source, the rule tag, and the
// human reason. One line in, at most one hit out (first match wins) — a
// violation is a line of code, and the baseline counts lines.
const PATTERNS = [
  { re: /\benv_persist_\w+\s*\(/, rule: 'env-direct', why: 'env_persist_* call — the account config env writer family; runtime state writes belong behind the store seam' },
  { re: /\.account_env_write_paths\s*\(/, rule: 'env-direct', why: '.account_env_write_paths() call — reaching for the .env path to write it outside the store seam' },
  { re: /\bpersist_cockpit_changes\s*\(/, rule: 'env-direct', why: 'persist_cockpit_changes call — cockpit settings env writer outside the store seam' },
  { re: /\benv_file::(upsert_env_vars|remove_env_vars|write_atomic)\s*\(/, rule: 'env-direct', why: 'env_file write primitive call — the .env write path is the EnvStore\'s, not the business layer\'s' },
  { re: /\bstd::env::(set_var|remove_var)\s*\(|\benv::(set_var|remove_var)\s*\(/, rule: 'env-direct', why: 'process-global env mutation (set_var/remove_var) — runtime state writes belong behind the store seam' },
  { re: /\brusqlite\b/, rule: 'sql-direct', why: 'direct rusqlite use — business code talks to db::/the store seam; only store implementations connect' },
  { re: /\bConnection::open\w*\s*\(/, rule: 'sql-direct', why: 'direct Connection::open — only store implementations open the database' },
];

// Comment lines are not code: a doc comment that MENTIONS a forbidden call
// (this gate's own docs do) is not a violation. Test modules are fixtures.
const COMMENT_LINE = /^\s*(\/\/|\/\*|\*)/;

/** Is this repo-relative path inside a test-only file?
 * The convention in this tree: `tests.rs` / `tests/` siblings, `*_tests.rs`,
 * and `test_fixtures.rs` are test-only files. */
function isTestFile(rel) {
  const base = rel.split('/').pop();
  return (
    base === 'tests.rs' ||
    base === 'test_fixtures.rs' ||
    base.endsWith('_tests.rs') ||
    rel.includes('/tests/') ||
    rel.includes('/test/')
  );
}

/** Line numbers (1-based) of `#[cfg(test)]` module blocks in one file.
 * An inline `#[cfg(test)] mod … { … }` block is a test fixture even inside a
 * production file — env manipulation there is a fixture, not a write path.
 * The scan tracks brace depth from the attribute to its block's close (this
 * tree puts test modules last, but the depth walk does not assume it). */
export function cfgTestRanges(text) {
  const lines = text.split('\n');
  const ranges = [];
  for (let n = 0; n < lines.length; n += 1) {
    if (!/#\[cfg\(test\)\]/.test(lines[n])) continue;
    // find the `mod NAME {` (or `mod NAME;`) that the attribute decorates,
    // then walk braces to the block's end.
    let m = n;
    while (m < lines.length && !/\bmod\s+\w+/.test(lines[m])) m += 1;
    if (m >= lines.length) continue;
    let depth = 0;
    let started = false;
    for (; m < lines.length; m += 1) {
      const code = stripLiterals(lines[m]);
      for (const ch of code) {
        if (ch === '{') { depth += 1; started = true; }
        else if (ch === '}') depth -= 1;
      }
      if (started && depth === 0) break;
    }
    ranges.push([n + 1, m + 1]);
    n = m;
  }
  return ranges;
}

/** Strip string literals and char literals from one line so a pattern match
 * inside a string (an error message, a test fixture) is not a violation.
 * Not a Rust parser — the state machine covers the shapes this tree writes:
 * "…" with \" escapes, '…' char literals (with the 'static lifetime
 * misread guarded by the closing-quote requirement), and raw strings
 * r#"…"#. Comments were already dropped by the caller. */
function stripLiterals(line) {
  let out = '';
  let i = 0;
  while (i < line.length) {
    const c = line[i];
    if (c === '"') {
      // raw string?  r#"…"# / r"…" — the prefix was copied verbatim above;
      // only an opening quote reaches here, so scan to the matching close.
      i += 1;
      while (i < line.length) {
        if (line[i] === '\\') i += 2;
        else if (line[i] === '"') { i += 1; break; }
        else i += 1;
      }
      out += '""';
    } else if (c === "'" && /'[a-zA-Z]?/.test(line.slice(i, i + 3))) {
      // char literal or lifetime: only treat as literal when it closes on
      // the same line within a few chars ('x', '\n', ' ' — not 'static).
      const m = line.slice(i).match(/^'(?:\\.[^']?|.{1,2})'/);
      if (m) { out += "''"; i += m[0].length; }
      else { out += c; i += 1; }
    } else {
      out += c;
      i += 1;
    }
  }
  return out;
}

/** Scan one file's text: returns [{ line, text, rule, why }] — one entry per
 * violating line. Pure text, no judgement (allowlist/judge come later).
 * Lines inside `#[cfg(test)]` module blocks are fixtures, not write paths. */
export function scanText(text) {
  const hits = [];
  const lines = text.split('\n');
  const testRanges = cfgTestRanges(text);
  for (let n = 0; n < lines.length; n += 1) {
    if (testRanges.some(([a, b]) => n + 1 >= a && n + 1 <= b)) continue;
    const raw = lines[n];
    if (!raw.trim()) continue;
    if (COMMENT_LINE.test(raw)) continue;
    const code = stripLiterals(raw);
    for (const p of PATTERNS) {
      if (p.re.test(code)) {
        hits.push({ line: n + 1, text: raw.trim().slice(0, 100), rule: p.rule, why: p.why });
        break; // one violation per line — the baseline counts lines
      }
    }
  }
  return hits;
}

/** Is this repo-relative path inside the allowlist? (prefix or exact match) */
function isAllowlisted(rel) {
  return ALLOW.some((a) => (a.path.endsWith('/') ? rel.startsWith(a.path) : rel === a.path));
}

/** Walk the scanned trees: returns [{ file, hits }] for every file with at
 * least one violation. Allowlisted paths and test-only files are skipped. */
export function scanTree(root) {
  const out = [];
  for (const dir of SCAN_DIRS) {
    let entries;
    try {
      entries = execFileSync('git', ['ls-files', '--', dir], {
        cwd: root,
        encoding: 'utf8',
        maxBuffer: 64 * 1024 * 1024,
      }).split('\n');
    } catch {
      continue; // tree without that directory — nothing to scan there
    }
    for (const rel of entries) {
      if (!rel.endsWith('.rs')) continue;
      if (isTestFile(rel)) continue;
      if (isAllowlisted(rel)) continue;
      const text = readFileSync(join(root, rel), 'utf8');
      const hits = scanText(text);
      if (hits.length) out.push({ file: rel, hits });
    }
  }
  return out;
}

// ── baseline I/O ─────────────────────────────────────────────────────────────

const HEADER = `# store-layer-baseline — the store-layering debt snapshot (issue #1199).
# Generated by: node scripts/store-layer-check.mjs --write-baseline
#
# One line per violating source line: <file>:<line> <rule> <text>
# The gate reds on violations NOT in this file (new debt); entries here that
# no longer exist are reported so the baseline can be narrowed (it only
# shrinks — the store migration retires entries as it lands).
#
# Carried debt = the env-write call sites of the 25-point migration map
# (dev-docs/1199-store-migration-map.md §2) that live OUTSIDE the allowlist,
# plus the env_persist_*/account_env_write_paths/persist_cockpit_changes call
# faces and the db-layer sql-direct lines outside the allowlisted store
# implementations. It is the exact list PR-1/PR-2 retire.
# The session.rs .env watcher is a READ path — not a violation, and part of
# the PR-2 retirement face (map §3.2 A2): do not delete it.
`;

/** The violation set as baseline lines, sorted (the baseline body). */
export function baselineLines(scan) {
  const lines = [];
  for (const { file, hits } of scan) {
    for (const h of hits) lines.push(`${file}:${h.line} ${h.rule} ${h.text}`);
  }
  return lines.sort();
}

function renderBaseline(scan) {
  const lines = baselineLines(scan);
  return HEADER + (lines.length ? lines.join('\n') + '\n' : '');
}

function parseBaseline(text) {
  const entries = new Set();
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    entries.add(line);
  }
  return entries;
}

// ── the judge (shared by the real verdict and --self-test) ───────────────────

/** Returns { added: [lines], removed: [lines] } — `added` are NEW violations
 * (the gate reds on those), `removed` are baseline entries that no longer
 * exist (reported, so the baseline can be narrowed). */
export function judge(baselineText, scan) {
  const want = parseBaseline(baselineText);
  const got = new Set(baselineLines(scan));
  const added = [...got].filter((l) => !want.has(l)).sort();
  const removed = [...want].filter((l) => !got.has(l)).sort();
  return { added, removed };
}

// ── --self-test: prove the teeth on in-memory fixtures (§A.4) ────────────────

function selfTest() {
  const checks = [];
  const expect = (name, ok) => checks.push({ name, ok });

  // scanText: the rule matrix. Every shape below is lifted from the real
  // tree (or its bug class); the compliant lines must stay silent.
  const s = (t) => scanText(t);

  // 1. THE acceptance: a business file gaining env::set_var is RED.
  expect('env::set_var in business code reds',
    s('std::env::set_var("K", "v");').some((h) => h.rule === 'env-direct'));
  // 2. env::remove_var, same class.
  expect('env::remove_var reds',
    s('std::env::remove_var("K");').some((h) => h.rule === 'env-direct'));
  // 3. the env_persist_* family call.
  expect('env_persist_account_venue call reds',
    s('crate::account::config::env_persist_account_venue(&legacy, &env_path, id, venue)')
      .some((h) => h.rule === 'env-direct'));
  // 4. .account_env_write_paths() call.
  expect('.account_env_write_paths() call reds',
    s('let Some((env_path, legacy)) = self.config.account_env_write_paths() else {')
      .some((h) => h.rule === 'env-direct'));
  // 5. persist_cockpit_changes call.
  expect('persist_cockpit_changes call reds',
    s('self.persist_cockpit_changes(&changes, now_ms);').some((h) => h.rule === 'env-direct'));
  // 6. the .env write primitives called from business code.
  expect('env_file::upsert_env_vars call reds',
    s('crate::env_file::upsert_env_vars(env_path, &vars)?;').some((h) => h.rule === 'env-direct'));
  expect('env_file::remove_env_vars call reds',
    s('crate::env_file::remove_env_vars(env_path, &keys)?;').some((h) => h.rule === 'env-direct'));
  // 7. direct rusqlite use.
  expect('rusqlite::params! reds (sql-direct)',
    s('rusqlite::params![account_id, crate::db::now_ms()],').some((h) => h.rule === 'sql-direct'));
  expect('Connection::open reds (sql-direct)',
    s('let mut conn = Connection::open(db_path)').some((h) => h.rule === 'sql-direct'));
  expect('Connection::open_with_flags reds (sql-direct)',
    s('let conn = Connection::open_with_flags(db_path, flags)').some((h) => h.rule === 'sql-direct'));

  // ── the compliant arms: a gate that cannot pass is the same defect as
  // one that cannot fail.
  // 8. db:: calls are the SANCTIONED path.
  expect('db:: call passes',
    s('let rows = crate::db::load_accounts(&conn)?;').length === 0);
  expect('db::Database type use passes',
    s('use crate::db::Database;').length === 0);
  // 9. comments mentioning the forbidden names pass (this gate's own docs
  //    are full of them).
  expect('doc comment mentioning env_persist passes',
    s('/// see [`crate::account::config::env_persist_account_active`]').length === 0);
  expect('line comment mentioning Connection passes',
    s('// Connection::open is the store layer\'s business').length === 0);
  // 10. string literals mentioning the names pass.
  expect('string literal mentioning set_var passes',
    s('let msg = "no set_var here, only a diagnostic";').length === 0);
  expect('error message mentioning rusqlite passes',
    s('return Err(format!("rusqlite constraint failed: {e}"));').length === 0);
  // 11. the jsonl::rewrite_atomic namesake is NOT env_file::write_atomic.
  expect('jsonl::rewrite_atomic passes (namesake guard)',
    s('crate::jsonl::rewrite_atomic(&path, buf.as_bytes());').length === 0);
  // 12. a char literal and a lifetime do not eat the rest of the line.
  expect("char literal + lifetime parse without eating the line",
    s("let c = 'x'; let s: &'static str = \"set_var\";").length === 0);

  // ── scanText on a file with the violation on line N reports line N.
  const body = ['fn a() {}', '', 'fn b() {', '    std::env::set_var("K", "v");', '}'].join('\n');
  const hit = s(body);
  expect('violation reports its real line number',
    hit.length === 1 && hit[0].line === 4 && hit[0].rule === 'env-direct');
  // 13. one violation per line (the baseline counts lines, not matches).
  expect('one violation per line even with two patterns on it',
    s('crate::env_file::upsert_env_vars(p, &v); crate::env_file::upsert_env_vars(p, &v);').length === 1);
  // 14. an inline #[cfg(test)] module is a fixture: the same env write
  //     inside it is NOT a violation, outside it is.
  const withTestMod = [
    'fn prod() {',
    '    std::env::set_var("A", "1");',
    '}',
    '#[cfg(test)]',
    'mod tests {',
    '    #[test]',
    '    fn fixture() {',
    '        std::env::set_var("B", "2");',
    '    }',
    '}',
  ].join('\n');
  const w = s(withTestMod);
  expect('env write in a #[cfg(test)] module passes (fixture)',
    w.length === 1 && w[0].line === 2);
  const ranges = cfgTestRanges(withTestMod);
  expect('cfgTestRanges finds the block',
    ranges.length === 1 && ranges[0][0] === 4 && ranges[0][1] === 10);
  // 15. isAllowlisted: prefix and exact forms.
  expect('allowlist prefix matches (db/)',
    isAllowlisted('core/blitzkrieg_core/src/db/credentials.rs'));
  expect('allowlist exact path matches',
    isAllowlisted('core/blitzkrieg_core/src/db_import.rs'));
  expect('non-allowlisted business file is not matched',
    !isAllowlisted('core/blitzkrieg_core/src/service/accounts/persist.rs'));

  // ── isTestFile: the fixture exemption.
  expect('tests.rs is a test file', isTestFile('core/blitzkrieg_core/src/execution_policy/tests.rs'));
  expect('*_tests.rs is a test file', isTestFile('core/blitzkrieg_core/src/db_import_tests.rs'));
  expect('/tests/ dir is a test file', isTestFile('core/blitzkrieg_core/src/service/tests/account_gate_tests.rs'));
  expect('production file is NOT a test file',
    !isTestFile('core/blitzkrieg_core/src/service/accounts/persist.rs'));
  expect('test_fixtures.rs is a test file', isTestFile('core/blitzkrieg_core/src/test_fixtures.rs'));

  // ── judge: the ratchet.
  const base = renderBaseline([{ file: 'a.rs', hits: [{ line: 3, text: 'x', rule: 'env-direct', why: '' }] }]);
  const same = [{ file: 'a.rs', hits: [{ line: 3, text: 'x', rule: 'env-direct', why: '' }] }];
  const grown = [{ file: 'a.rs', hits: [{ line: 3, text: 'x', rule: 'env-direct', why: '' }, { line: 9, text: 'std::env::set_var("K","v");', rule: 'env-direct', why: '' }] }];
  const healed = [{ file: 'a.rs', hits: [] }];
  expect('identical scan passes the baseline',
    judge(base, same).added.length === 0);
  expect('a NEW violation reds against the baseline',
    judge(base, grown).added.length === 1 && judge(base, grown).added[0].includes('a.rs:9'));
  expect('a HEALED violation is reported as removable (not red)',
    judge(base, healed).added.length === 0 && judge(base, healed).removed.length === 1);
  expect('baseline text with comments/blank lines parses',
    parseBaseline('# c\n\n' + 'a.rs:3 env-direct x\n').has('a.rs:3 env-direct x'));

  const failed = checks.filter((c) => !c.ok);
  for (const c of checks) console.log(`${c.ok ? 'ok  ' : 'RED '} self-test: ${c.name}`);
  if (failed.length) {
    console.error(`\nstore-layer-check self-test: ${failed.length} RED`);
    process.exit(1);
  }
  console.log('\nstore-layer-check self-test: all green');
}

// ── main ─────────────────────────────────────────────────────────────────────

function baselineFromArg() {
  const arg = process.argv.includes('--baseline')
    ? process.argv[process.argv.indexOf('--baseline') + 1]
    : null;
  if (arg === undefined) {
    console.error('store-layer-check: --baseline needs a file path or a sha');
    process.exit(1);
  }
  if (!arg) return readFileSync(BASELINE, 'utf8');
  // a sha (or ref) resolves to the baseline file as of that revision.
  if (!arg.includes('/') && !arg.endsWith('.txt')) {
    try {
      return execFileSync('git', ['show', `${arg}:scripts/store-layer-baseline.txt`],
        { cwd: ROOT, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
    } catch {
      console.error(`store-layer-check: no scripts/store-layer-baseline.txt at ${arg}`);
      process.exit(1);
    }
  }
  return readFileSync(resolve(ROOT, arg), 'utf8');
}

if (process.argv.includes('--self-test')) {
  selfTest();
  process.exit(0);
}

const scan = scanTree(ROOT);

if (process.argv.includes('--write-baseline')) {
  writeFileSync(BASELINE, renderBaseline(scan));
  const n = baselineLines(scan).length;
  console.log(`store-layer-check: baseline rewritten — ${n} carried violation(s) across ${scan.length} file(s)`);
  process.exit(0);
}

const { added, removed } = judge(baselineFromArg(), scan);
if (added.length) {
  console.error(`store-layer-check: ${added.length} NEW store-layer violation(s) —\n`);
  for (const l of added) console.error(`  + ${l}`);
  console.error(
    '\nThe store seam (StoreApi, #1199) is the ONE sanctioned write path: route the' +
      ' write through it, or (if this is a store implementation) extend the ALLOW' +
      ' list with the reason in the PR body.',
  );
  process.exit(1);
}
const carried = baselineLines(scan).length;
if (removed.length) {
  // not a failure — the debt shrank; say so and point at the narrowing.
  console.log(`store-layer-check: PASS — 0 new violations; ${removed.length} baseline entr${removed.length === 1 ? 'y' : 'ies'} healed:`);
  for (const l of removed) console.log(`  - ${l}`);
  console.log('  (narrow the baseline: node scripts/store-layer-check.mjs --write-baseline)');
} else {
  console.log(`store-layer-check: PASS — 0 new violations; ${carried} baseline debt carried (the #1199 migration retires it)`);
}