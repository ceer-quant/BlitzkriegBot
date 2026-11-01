#!/usr/bin/env node
/**
 * test-support-default-check — the #825 gate.
 *
 * #825: `test-support` used to sit in blitzkrieg-core's default features, so
 * a plain production build compiled the blunt test mutators
 * (positions_mut / accounts_mut / config_mut / risk_config_mut / ledger_mut)
 * and the `strategies::test_support` adapter module straight into the shipped
 * surface. The fix took the feature out of `default` and re-gated every
 * mutator as `#[cfg(any(test, feature = "test-support"))]` — unit tests
 * reach them via cfg(test), integration tests via the opt-in feature, the
 * shipped default surface reaches neither (the release builds in CI are the
 * compile-level proof).
 *
 * This gate pins the text that makes that true, so it cannot quietly rot
 * back:
 *   1. `default = [...]` in core/blitzkrieg_core/Cargo.toml must NOT name
 *      test-support;
 *   2. `test-support = []` must stay declared — the integration tests and
 *      benches still need their opt-in;
 *   3. every mutator listed below must carry
 *      `#[cfg(any(test, feature = "test-support"))]` on the line directly
 *      above its `pub fn` — both tokens, not the old feature-only cfg.
 *
 * Usage:
 *   node scripts/test-support-default-check.mjs             # the real verdict
 *   node scripts/test-support-default-check.mjs --self-test # tmpdir fixtures
 *
 * Exit: 0 pass / 1 violations.
 */

import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const MANIFEST = 'core/blitzkrieg_core/Cargo.toml';

// The dual cfg every test seam must carry: `test` OR the opt-in feature.
// Both tokens is the acceptance bar (#825) — a feature-only cfg would put
// the seam back into every build that flips the feature on for production
// convenience, and a bare cfg would strand the integration tests.
const CFG_ATTR = '#[cfg(any(test, feature = "test-support"))]';

// The five blunt test mutators (#825): (file, fn name). The four the issue
// names plus `ledger_mut` — the same class of seam, and the gate the other
// four's doc comments cross-reference ("the same gate ledger_mut carries"),
// so the set cannot drift out of sync.
const MUTATORS = [
  ['core/blitzkrieg_core/src/service/strategies.rs', 'positions_mut'],
  ['core/blitzkrieg_core/src/service/strategies.rs', 'accounts_mut'],
  ['core/blitzkrieg_core/src/service/strategies.rs', 'config_mut'],
  ['core/blitzkrieg_core/src/service/risk_admission/freeze.rs', 'risk_config_mut'],
  ['core/blitzkrieg_core/src/service/accounts/audit.rs', 'ledger_mut'],
];

/** Parse the `default = [...]` feature list out of a manifest. Null when the
 * line is missing or malformed — which the audit reports as its own red. */
function defaultFeatures(text) {
  const m = text.match(/^default\s*=\s*\[([^\]]*)\]/m);
  if (!m) return null;
  return m[1].split(',').map((s) => s.trim().replace(/^"|"$/g, '')).filter(Boolean);
}

/** Is the LAST non-blank line before `pub fn NAME` exactly the dual cfg?
 * Doc comments sit ABOVE the cfg attr in these files, so the attribute is
 * the fn's immediate neighbour by construction; anything wedged between
 * (another attr, a reorder) reds and gets a human look. */
function mutatorCfg(src, name) {
  const m = src.match(new RegExp(`pub fn ${name}\\b`));
  if (!m) return { ok: false, why: 'pub fn not found' };
  const before = src.slice(0, m.index).replace(/\s+$/, '');
  const lineStart = before.lastIndexOf('\n') + 1;
  const attr = before.slice(lineStart).trim();
  if (attr !== CFG_ATTR) return { ok: false, why: `cfg line is \`${attr}\` — must be \`${CFG_ATTR}\`` };
  return { ok: true };
}

/** The audit: judge the manifest and the five mutator cfgs against `root`.
 * Returns the RED rows — empty means pass. Parameterized over root so
 * --self-test can pin its own fixtures. */
export function audit(root) {
  const violations = [];
  const manifestPath = join(root, MANIFEST);
  let text;
  try {
    text = readFileSync(manifestPath, 'utf8');
  } catch {
    return [{ check: 'manifest', detail: `${MANIFEST}: unreadable` }];
  }

  // (a) default must not carry the test feature.
  const feats = defaultFeatures(text);
  if (!feats) {
    violations.push({ check: 'manifest-default', detail: 'cannot parse `default = [...]` in core/blitzkrieg_core/Cargo.toml' });
  } else if (feats.includes('test-support')) {
    violations.push({ check: 'manifest-default', detail: `default = [${feats.join(', ')}] contains test-support — a plain build ships the test seams (#825)` });
  }

  // (b) the opt-in must survive: integration tests and benches need it.
  if (!/^\s*test-support\s*=\s*\[\]/m.test(text)) {
    violations.push({ check: 'manifest-opt-in', detail: '`test-support = []` declaration missing — integration tests lost their opt-in' });
  }

  // (c) every mutator keeps the dual cfg.
  const sources = new Map();
  for (const [file] of MUTATORS) {
    if (sources.has(file)) continue;
    try {
      sources.set(file, readFileSync(join(root, file), 'utf8'));
    } catch {
      sources.set(file, null);
    }
  }
  for (const [file, name] of MUTATORS) {
    const src = sources.get(file);
    if (src === null) {
      violations.push({ check: 'mutator-cfg', detail: `${file}: unreadable` });
      continue;
    }
    const r = mutatorCfg(src, name);
    if (!r.ok) violations.push({ check: 'mutator-cfg', detail: `${file}: ${name} — ${r.why}` });
  }

  return violations;
}

function selfTest() {
  console.error('test-support-default-check --self-test: the judge pinned against tmpdir fixtures');
  const sandbox = mkdtempSync(join(tmpdir(), 'test-support-default-selftest-'));
  try {
    const GOOD_MANIFEST = [
      '[features]',
      'default = ["polymarket", "kalshi", "strategy-loading", "account-precision"]',
      'test-support = []',
    ].join('\n');
    const DIRTY_MANIFEST = GOOD_MANIFEST.replace(
      '"account-precision"]',
      '"account-precision", "test-support"]',
    );
    const NO_OPTIN_MANIFEST = GOOD_MANIFEST.replace('test-support = []', '# gone');
    const seam = (name) => [
      `    /// TEST SEAM ONLY.`,
      CFG_ATTR,
      `    pub fn ${name}(&mut self) {}`,
    ].join('\n');
    const featureOnly = (name) => seam(name).replace(CFG_ATTR, '#[cfg(feature = "test-support")]');
    const bare = (name) => seam(name).replace(CFG_ATTR + '\n', '');

    const write = (rel, content) => {
      const p = join(sandbox, rel);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, content);
    };
    const goodTree = () => {
      write(MANIFEST, GOOD_MANIFEST);
      write('core/blitzkrieg_core/src/service/strategies.rs', `${seam('positions_mut')}\n${seam('accounts_mut')}\n${seam('config_mut')}\n`);
      write('core/blitzkrieg_core/src/service/risk_admission/freeze.rs', seam('risk_config_mut'));
      write('core/blitzkrieg_core/src/service/accounts/audit.rs', seam('ledger_mut'));
    };

    const counts = (v) => ({
      def: v.filter((x) => x.check === 'manifest-default').length,
      opt: v.filter((x) => x.check === 'manifest-opt-in').length,
      cfg: v.filter((x) => x.check === 'mutator-cfg').length,
    });

    // §A.4 order: prove it can go red first, then green.
    write(MANIFEST, DIRTY_MANIFEST);
    let v = counts(audit(sandbox));
    check('test-support in default reds (default rule)', v.def === 1, JSON.stringify(v));

    goodTree();
    write(MANIFEST, NO_OPTIN_MANIFEST);
    v = counts(audit(sandbox));
    check('missing test-support = [] reds (opt-in rule)', v.opt === 1, JSON.stringify(v));

    goodTree();
    const strategies = 'core/blitzkrieg_core/src/service/strategies.rs';
    write(strategies, `${featureOnly('positions_mut')}\n${seam('accounts_mut')}\n${seam('config_mut')}\n`);
    v = audit(sandbox);
    check('feature-only cfg reds (dual-cfg rule)', v.length === 1 && v[0].check === 'mutator-cfg' && v[0].detail.includes('positions_mut'), JSON.stringify(v));

    goodTree();
    write(strategies, `${bare('config_mut')}\n`);
    // keep the other two seams present so only the bare one is judged
    write(strategies, `${featureOnly('positions_mut').replace('#[cfg(feature = "test-support")]', CFG_ATTR)}\n${seam('accounts_mut')}\n${bare('config_mut')}\n`);
    v = audit(sandbox);
    check('missing cfg reds (dual-cfg rule)', v.length === 1 && v[0].check === 'mutator-cfg' && v[0].detail.includes('config_mut'), JSON.stringify(v));

    goodTree();
    v = audit(sandbox);
    check('clean tree passes (all five mutators dual-gated)', v.length === 0, JSON.stringify(v));

    const failed = checks.filter((c) => !c.ok).length;
    if (failed > 0) {
      console.error(`\ntest-support-default-check self-test: ${failed} RED — ${checks.filter((c) => !c.ok).map((c) => c.name).join('; ')}`);
      process.exit(1);
    }
    console.error('\ntest-support-default-check self-test: all green');
  } finally {
    rmSync(sandbox, { recursive: true, force: true });
  }
}

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.error(`  ${ok ? 'PASS' : 'RED '} ${name}${detail ? ` — ${detail}` : ''}`);
};

function main() {
  if (process.argv.includes('--self-test')) return selfTest();

  const violations = audit(ROOT);
  for (const v of violations) {
    check(`${v.check}`, false, v.detail);
  }
  if (violations.length > 0) {
    console.error(`\ntest-support-default-check: ${violations.length} RED — test-support stays out of default; every test seam keeps cfg(any(test, feature = "test-support")) (#825)`);
    process.exit(1);
  }
  console.error(`\ntest-support-default-check: all green — default clean, opt-in declared, ${MUTATORS.length} test mutators dual-gated (#825)`);
}

main();
