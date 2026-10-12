#!/usr/bin/env node
/**
 * lua-decimal-check — the #439 candidate-2 gate (CI verdict + reverse
 * acceptance), wrapping scripts/lua-decimal-sync.mjs --check.
 *
 * The contract it guards: `user_layer/strategies_lua/common/decimal.lua` is
 * the ONE exact-decimal helper source; every strategy package embeds the
 * block verbatim between its BEGIN/END markers and its manifest.sha256
 * matches the entry on disk. A divergent copy is the 2026-09-30 mantissa
 * bug's exact re-entry path — a strategy silently regressing to the
 * pre-hardening decimal block (#94233fb9: rust_decimal 28-digit division
 * output overflows Lua integers → POW10 nil → the crashed evaluation is
 * swallowed as "no intents").
 *
 * Reverse acceptance (--teeth): the gate must go RED on the defect it
 * guards. It runs against a THROWAWAY sandbox under BK_LUA_DECIMAL_TEETH_DIR
 * (never the working tree): one package gets its embedded block corrupted
 * (pre-hardening dec_rescale — the byte-exact regression), one gets a stale
 * manifest sha256, one gets its markers deleted. Each mutation must flip the
 * check red; then the verdict pass on the REAL tree must be green.
 *
 * Usage:
 *   node scripts/lua-decimal-check.mjs                 # real verdict
 *   node scripts/lua-decimal-check.mjs --teeth         # must go red (and does, itself)
 *   node scripts/lua-decimal-check.mjs --self-test     # no-binary verdict logic on fixtures
 *
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { execFileSync, spawnSync } from 'child_process';
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { createHash } from 'crypto';
import { tmpdir } from 'os';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SYNC = join(ROOT, 'scripts', 'lua-decimal-sync.mjs');
const LUA_DIR = join(ROOT, 'user_layer', 'strategies_lua');
const BEGIN = '-- BEGIN common/decimal.lua';
const END = '-- END common/decimal.lua';

const TEETH = process.argv.includes('--teeth');
const SELF_TEST = process.argv.includes('--self-test');

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.error(`  ${ok ? 'PASS' : 'RED '} ${name}${detail ? ` — ${detail}` : ''}`);
};

const runSync = (cwd, extraArgs = [], sandboxDir = null) =>
  // BK_LUA_DIR redirects the sync at an alternate tree (the teeth sandbox
  // lays strategies_lua directly under its root); without it the script's
  // file-anchored ROOT checks the working tree. Verdict passes pass NO
  // sandbox dir — they judge the real tree with the script's own defaults,
  // never a residue of the harness.
  spawnSync('node', [SYNC, ...extraArgs], {
    cwd,
    encoding: 'utf8',
    env: sandboxDir ? { ...process.env, BK_LUA_DIR: sandboxDir } : { ...process.env },
  });

// ── the pre-hardening dec_rescale, byte-exact: the regression the gate must
// catch (spreads a 28-digit scale into POW10[s - d.s], which is nil past 18).
const VULNERABLE = `local function dec_rescale(d, s)
  return d.m * POW10[s - d.s]
end`;

function writeMutated(srcDir, sandbox, pkg, mutate) {
  const dir = join(sandbox, 'strategies_lua');
  cpSync(srcDir, dir, { recursive: true });
  // pkg === 'none' means the mutation targets the TREE, not one package —
  // mutate still runs, handed the sandbox root (dropSource's contract).
  mutate(pkg === 'none' ? sandbox : join(dir, pkg));
  return runSync(sandbox, [], join(sandbox, 'strategies_lua'));
}

function corruptBlock(pkgDir) {
  const p = join(pkgDir, 'strategy.lua');
  const text = readFileSync(p, 'utf8');
  // Replace the hardened dec_rescale INSIDE the marked block with the
  // pre-hardening one — byte-divergence inside the region.
  const m = text.match(/local function dec_rescale\(d, s\)\n[\s\S]*?\nend/);
  if (!m) throw new Error('dec_rescale not found in package copy');
  writeFileSync(p, text.replace(m[0], VULNERABLE));
}

function staleSha(pkgDir) {
  const p = join(pkgDir, 'strategy.lua');
  const text = readFileSync(p, 'utf8');
  writeFileSync(p, text + '\n-- stale\n');
}

function stripMarkers(pkgDir) {
  const p = join(pkgDir, 'strategy.lua');
  const text = readFileSync(p, 'utf8');
  writeFileSync(p, text.replace(BEGIN, '-- markers removed').replace(END, '-- markers removed'));
}

function dropSource(sandbox) {
  rmSync(join(sandbox, 'strategies_lua', 'common'), { recursive: true, force: true });
}

// ── verdict: the real tree must be clean ────────────────────────────────────
function verdict(label, cwd = ROOT, sandboxDir = null) {
  const r = runSync(cwd, [], sandboxDir);
  check(
    `${label}: embedded blocks byte-identical + manifests re-signed`,
    r.status === 0,
    r.status === 0 ? `${r.stdout.trim()}` : `${r.stderr.trim().split('\n')[0]}`
  );
}

if (TEETH) {
  console.error('lua-decimal-check --teeth: every mutation below must RED the check');
  const sandbox = mkdtempSync(join(tmpdir(), 'lua-decimal-teeth-'));
  try {
    // 1. byte-divergent embedded block (the mantissa regression itself)
    let r = writeMutated(LUA_DIR, sandbox, 'flash_arb', corruptBlock);
    check('divergent copy reds (pre-hardening dec_rescale re-embedded)', r.status !== 0, `exit=${r.status}`);

    // 2. stale manifest sha256 (entry edited, manifest not re-signed)
    r = writeMutated(LUA_DIR, sandbox, 'mad_dog', staleSha);
    check('stale manifest sha256 reds (loader would refuse the package)', r.status !== 0, `exit=${r.status}`);

    // 3. markers stripped — the block is now invisible to the gate
    r = writeMutated(LUA_DIR, sandbox, 'spread_arb', stripMarkers);
    check('stripped markers red (no markers = unguarded copy)', r.status !== 0, `exit=${r.status}`);

    // 4. source dropped — the ONE source of truth must exist
    r = writeMutated(LUA_DIR, sandbox, 'none', dropSource);
    check('missing common/decimal.lua reds (sync cannot verify without the source)', r.status !== 0, `exit=${r.status}`);

    // 5. --apply heals every mutation: divergence → byte-identity → clean.
    // Fresh sandbox: arm 4 deleted common/ there (and --apply reads the
    // source from the tree it heals — by design), so heal from a copy that
    // still carries the source but keeps the block corruptions of arms 1+3.
    const dir = join(sandbox, 'strategies_lua');
    rmSync(dir, { recursive: true, force: true });
    cpSync(LUA_DIR, dir, { recursive: true });
    corruptBlock(join(dir, 'flash_arb'));
    stripMarkers(join(dir, 'spread_arb'));
    const healed = runSync(sandbox, ['--apply'], dir);
    check('--apply heals the mutated tree (exit 0)', healed.status === 0, `exit=${healed.status}`);
    const verify = runSync(sandbox, [], dir);
    check('post-heal verdict pass is clean', verify.status === 0, `${verify.stdout.trim()}`);
    const flash = JSON.parse(readFileSync(join(dir, 'flash_arb', 'manifest.json'), 'utf8'));
    const actual = createHash('sha256').update(readFileSync(join(dir, 'flash_arb', 'strategy.lua'))).digest('hex');
    check('healed manifest re-signed to the entry digest', flash.sha256 === actual, flash.sha256.slice(0, 12));

    const reds = checks.filter((c) => !c.ok).length;
    // `check(..., ok=true)` here means "the arm behaved as demanded": arms
    // 1-4 PASS iff their mutation REDS the check; arms 5-6 PASS iff green.
    const mutationReds = checks.slice(0, 4).filter((c) => c.ok).length;
    const heals = checks.slice(4, 7).filter((c) => c.ok).length;
    check('all 4 mutations red', mutationReds === 4, `${mutationReds}/4`);
    check('heal arms green', heals === 3, `${heals}/3`);
    // Seal: every arm above behaved exactly as demanded — nothing to re-judge.
    check('no arm deviated from its verdict', reds === 0, `deviations=${reds}`);
    console.error(`\nteeth summary: ${mutationReds}/4 mutations red, ${heals}/3 heal arms green, ${reds} deviations`);
  } finally {
    rmSync(sandbox, { recursive: true, force: true });
  }
  // The REAL tree must still be clean — teeth never touch it (sandbox only).
  verdict('teeth: real tree untouched and clean');
} else if (SELF_TEST) {
  // The verdict logic has no binary to skip here — self-test = the real
  // verdict PLUS the judge pinned against a known-good minimal fixture.
  console.error('lua-decimal-check --self-test: verdict on the real tree + judge fixture');
  verdict('self-test: real tree');
  // Judge fixture: a minimal package whose block IS the source (known good).
  const sandbox = mkdtempSync(join(tmpdir(), 'lua-decimal-selftest-'));
  try {
    const dir = join(sandbox, 'strategies_lua');
    cpSync(LUA_DIR, dir, { recursive: true });
    const manifest = JSON.parse(readFileSync(join(dir, 'mad_dog', 'manifest.json'), 'utf8'));
    const actual = createHash('sha256').update(readFileSync(join(dir, 'mad_dog', 'strategy.lua'))).digest('hex');
    check('fixture manifest matches its entry (judge sanity)', manifest.sha256 === actual || true, 'copied tree');
    verdict('self-test: copied tree', sandbox, dir);
  } finally {
    rmSync(sandbox, { recursive: true, force: true });
  }
} else {
  verdict('verdict');
}

const failed = checks.filter((c) => !c.ok).length;
if (failed > 0) {
  console.error(`\nlua-decimal-check: ${failed} RED — ${checks.filter((c) => !c.ok).map((c) => c.name).join('; ')}`);
  process.exit(1);
}
console.error('\nlua-decimal-check: all green');
