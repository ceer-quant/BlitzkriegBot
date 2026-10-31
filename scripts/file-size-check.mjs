#!/usr/bin/env node
/**
 * file-size-check — the file-size ratchet gate (#780).
 *
 * The campaign rule it enforces: no `.rs` file in the trading-repo scope
 * (core/, tools/, user_layer/, extensions/ — and, since the #819-#821 ui
 * split wave, `ui/` too) may exceed 1500 lines. A file that big is where review
 * coverage dies and where the split campaigns (#699 family, #780's
 * service-test split) come to rot back together; this gate is the arm that
 * keeps them split.
 *
 * Ratchet semantics — the allowlist is debt, not licence, and it only
 * tightens:
 *   1. a scanned `.rs` file over LIMIT that is not in ALLOW is red;
 *   2. an ALLOW entry whose file no longer exists is red — drop the entry;
 *   3. an ALLOW entry whose file is now ≤ LIMIT is red — prune the
 *      allowlist, the ratchet only tightens. An entry never grows: shrinking
 *      is the only direction out of it.
 *
 * Lines are counted with `wc -l` semantics: newline bytes, nothing else.
 *
 * Usage:
 *   node scripts/file-size-check.mjs             # the real verdict
 *   node scripts/file-size-check.mjs --self-test # fixtures in a tmpdir, no tree
 *
 * Exit: 0 pass / 1 violations.
 */

import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// Code-bearing trees in scope. `ui/` joined when its five oversized files
// were split by domain (#819 gateway command/supervisor, #820 panel ui/app,
// #821 web) — same rule, same owner of the debt now.
const SCOPES = ['core', 'tools', 'user_layer', 'extensions', 'ui'];

// Machine-generated trees a checkout can legitimately carry; never scanned.
const SKIP_DIRS = ['target', 'dist', 'node_modules'];

const LIMIT = 1500;

// The ratchet's teeth at authoring time, measured on this gate's base
// (ceer/main @ 05f809a6, 2026-10-31): empty. The #780/#781 split campaign
// (#795-#816) retired the last oversized files — risk_gates.rs became the
// tests/risk_gates/ directory, strategy_dispatch_tests.rs 4866→490 and
// audit_fix_tests.rs 2191→299 with every child ≤1461 — and the largest
// in-scope file is now lua_loader.rs at 1495. The ui split wave (#819-#821)
// brought `ui/` into scope the same day its last oversized file fell (the
// largest ui file is web/auth_tests.rs at 1302), so the allowlist stays
// empty across the whole tree. An entry here is a debt
// marker — the file may shrink or die, never grow into a bigger exception,
// and once it is ≤ LIMIT the entry must be pruned (rule 3 above).
const ALLOW = [];

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.error(`  ${ok ? 'PASS' : 'RED '} ${name}${detail ? ` — ${detail}` : ''}`);
};

/** wc -l semantics: count newline bytes. Buffer scan, so encoding never
 * distorts the count (a non-UTF-8 file still counts honestly). */
function lineCount(p) {
  const buf = readFileSync(p);
  let n = 0;
  for (let i = 0; i < buf.length; i += 1) if (buf[i] === 0x0a) n += 1;
  return n;
}

/** Every in-scope `.rs` file under `root`, repo-relative (`/`-separated),
 * recursively, skipping machine-generated directories. A scope that does
 * not exist in this checkout is fine. */
function scopeFiles(root) {
  const files = [];
  for (const dir of SCOPES) {
    const walk = (d) => {
      let entries;
      try {
        entries = readdirSync(d, { withFileTypes: true });
      } catch {
        return; // missing scope directory — nothing to scan there
      }
      for (const e of entries) {
        const p = join(d, e.name);
        if (e.isDirectory()) {
          if (SKIP_DIRS.includes(e.name)) continue;
          walk(p);
        } else if (e.isFile() && e.name.endsWith('.rs')) {
          files.push({ path: p, rel: p.slice(root.length + 1).split('\\').join('/') });
        }
      }
    };
    walk(join(root, dir));
  }
  return files;
}

/** The audit: scan, then judge the allowlist. Returns the RED rows —
 * empty means the tree passes. `allow` defaults to the real allowlist and
 * is a parameter so --self-test can pin its own fixtures. */
export function audit(root, allow = ALLOW) {
  const violations = [];
  const allowByPath = new Map(allow.map((a) => [a.path, a]));
  const allowSeen = new Set();
  let scanned = 0;

  for (const f of scopeFiles(root)) {
    scanned += 1;
    const lines = lineCount(f.path);
    const entry = allowByPath.get(f.rel);
    if (entry) {
      allowSeen.add(f.rel);
      if (lines <= LIMIT) {
        violations.push({
          kind: 'prune',
          path: f.rel,
          lines,
          detail: `now ${lines} lines (was ${entry.atAuthoring} at authoring) — prune the allowlist — the ratchet only tightens`,
        });
      }
    } else if (lines > LIMIT) {
      violations.push({
        kind: 'over-limit',
        path: f.rel,
        lines,
        detail: `${lines} lines > ${LIMIT} — split it, or add it to ALLOW with its atAuthoring count (the ratchet only tightens)`,
      });
    }
  }

  for (const entry of allow) {
    if (!allowSeen.has(entry.path)) {
      violations.push({
        kind: 'gone',
        path: entry.path,
        lines: null,
        detail: 'allowlisted file no longer exists — drop the entry',
      });
    }
  }

  return { scanned, violations };
}

function selfTest() {
  console.error('file-size-check --self-test: the judge pinned against tmpdir fixtures');
  const sandbox = mkdtempSync(join(tmpdir(), 'file-size-selftest-'));
  try {
    const write = (rel, newlines) => {
      const p = join(sandbox, rel);
      mkdirSync(dirname(p), { recursive: true });
      // exactly `newlines` newline bytes — wc -l counts what it says
      writeFileSync(p, '\n'.repeat(newlines));
    };
    // A 3-line file, an exactly-at-limit file (1500 passes — the rule is
    // "over 1500"), a 1501-line offender, one allowlisted file still over
    // the limit (its debt marker is honoured), one allowlisted file shrunk
    // to ≤ limit (prune rule), one allowlisted file deleted (gone rule);
    // plus a target/ tree the scan must skip.
    write('core/a.rs', 3);
    write('tools/exact.rs', LIMIT);
    write('user_layer/over.rs', LIMIT + 1);
    write('core/still_over.rs', LIMIT + 5);
    write('extensions/shrunk.rs', 2);
    write('core/target/junk.rs', LIMIT + 7);
    const fixtureAllow = [
      { path: 'core/still_over.rs', atAuthoring: LIMIT + 5 },
      { path: 'extensions/shrunk.rs', atAuthoring: 900 },
      { path: 'core/vanished.rs', atAuthoring: 1600 },
    ];
    const { scanned, violations } = audit(sandbox, fixtureAllow);
    const over = violations.filter((v) => v.kind === 'over-limit');
    const prune = violations.filter((v) => v.kind === 'prune');
    const gone = violations.filter((v) => v.kind === 'gone');

    check('at-limit file (1500 lines exactly) passes', !over.some((v) => v.path === 'tools/exact.rs') && !prune.some((v) => v.path === 'tools/exact.rs'));
    check('small file passes', !over.some((v) => v.path === 'core/a.rs'));
    check('1501-line unallowlisted file reds (over-limit)', over.length === 1 && over[0].path === 'user_layer/over.rs', over.map((v) => v.path).join(','));
    check('allowlisted file still over limit passes (debt marker honoured)', !over.some((v) => v.path === 'core/still_over.rs') && !prune.some((v) => v.path === 'core/still_over.rs'));
    check('allowlisted file shrunk to ≤ limit reds (prune rule)', prune.length === 1 && prune[0].path === 'extensions/shrunk.rs');
    check('allowlisted file gone reds (drop the entry)', gone.length === 1 && gone[0].path === 'core/vanished.rs');
    check('target/ tree is skipped', !over.some((v) => v.path.includes('target/')));
    check('scan counted the five in-scope fixtures', scanned === 5, `scanned=${scanned}`);

    const failed = checks.filter((c) => !c.ok).length;
    if (failed > 0) {
      console.error(`\nfile-size-check self-test: ${failed} RED — ${checks.filter((c) => !c.ok).map((c) => c.name).join('; ')}`);
      process.exit(1);
    }
    console.error('\nfile-size-check self-test: all green');
  } finally {
    rmSync(sandbox, { recursive: true, force: true });
  }
}

function main() {
  if (process.argv.includes('--self-test')) return selfTest();

  const { scanned, violations } = audit(ROOT);
  for (const v of violations) {
    check(`${v.kind}: ${v.path}`, false, v.lines !== null ? `${v.lines} lines` : v.detail);
    // one plain grep-able line per violation, alongside the check row
    console.error(`    ${v.path}: ${v.detail}`);
  }
  if (violations.length > 0) {
    console.error(`\nfile-size-check: ${violations.length} RED — no .rs file over ${LIMIT} lines (ratchet: shrink or split; the allowlist only tightens)`);
    process.exit(1);
  }
  console.error(`\nfile-size-check: all green — ${scanned} .rs file(s) scanned across ${SCOPES.join(', ')}, limit ${LIMIT}, ALLOW ${ALLOW.length} entr${ALLOW.length === 1 ? 'y' : 'ies'}`);
}

main();
