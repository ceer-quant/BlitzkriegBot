#!/usr/bin/env node
/**
 * ci-lane-planner — the always-run change classifier (#844 scope 3).
 *
 * Answers ONE question cheaply: which CI lanes must this change set run?
 * It pairs with .github/workflows/ci.yml's `planner` job: the job always runs
 * (so the required summary can never go missing), and the heavy jobs condition
 * on the lane key this script emits.
 *
 * Lanes are a SUPERSET ladder — cheap lanes add work, never remove it:
 *   docs      markdown/screenshots/assets text only — NO build at all; the
 *             secret-scan + ops-gates + summary still run (always-on).
 *   contract  contracts themselves changed — fans out to every downstream
 *             suite (plugin+ui+strategy gates ride the contract job set).
 *   plugin    one (or more) extensions/* — plugin gates + contract suites.
 *   ui        ui/** — UI gates + contract suites.
 *   strategy  user_layer/** — strategy gates + contract suites.
 *   core      everything else (core/, tools/, Cargo root manifests + lockfile,
 *             .github/, scripts/) — the FULL matrix, the default and the
 *             safety net. Anything the classifier cannot confidently call
 *             cheap lands here: fail-NEVER-cheap is the rule (an unclassified
 *             file runs the heavy matrix rather than being silently skipped).
 *
 * Output: one JSON object on stdout —
 *   {"lanes": "docs"|"core"|"contract"|"plugin"|"ui"|"strategy"|"contract+plugin",
 *    "breakdown": {"docs":n,"contract":n,"plugin":n,"ui":n,"strategy":n,"other":n}}
 * `lanes` is a single key the workflow `if:` compares (not 'docs' ⇒ heavy
 # lanes schedule; equality checks live in ci.yml). The `breakdown` feeds the
 * run's report line so a reviewer sees HOW the lanes were chosen.
 *
 * Path rules (checked in this order; most specific wins):
 *   assets/screenshots/**            → docs (never executable)
 *   **.md                            → docs
 *   core/bk_contract/**              → contract
 *   core/market_api/**               → contract
 *   core/data_model/**               → contract
 *   extensions/**                    → plugin
 *   ui/**                            → ui
 *   user_layer/**                    → strategy
 *   tools/**                         → plugin (tools crate is loaded by core's
 *                                      dispatch but ships like a plugin lane)
 *   core/**, Cargo.toml, Cargo.lock  → core (the kernel + freezes)
 *   .github/**, scripts/**, **.cfg   → core (workflow/plumbing = full matrix;
 *                                      changing the checker itself must prove
 *                                      the whole matrix still passes)
 *   (anything else)                  → core (fail-never-cheap)
 *
 * Usage:
 *   node scripts/ci-lane-planner.mjs <base>     # base: 'origin/main' or a sha
 *   node scripts/ci-lane-planner.mjs <base> --self-test
 *
 * Exit: 0 pass; 2 usage/error (planner failure must NOT look like success).
 */

import { execFileSync } from 'child_process';

/** Classify one path into a lane bit. */
export function laneOfPath(path) {
  const p = path.replaceAll('\\', '/').toLowerCase();
  if (p.startsWith('core/bk_contract/') || p.startsWith('core/market_api/') || p.startsWith('core/data_model/')) {
    return 'contract';
  }
  if (p.startsWith('extensions/')) return 'plugin';
  if (p.startsWith('ui/')) return 'ui';
  if (p.startsWith('user_layer/')) return 'strategy';
  if (p.startsWith('tools/')) return 'plugin';
  if (p.startsWith('assets/screenshots/')) return 'docs';
  if (p.endsWith('.md') || p.endsWith('.markdown')) return 'docs';
  // The core lane also OWNS the CI plumbing and the cheap scripts: a change to
  // the guard/planner itself re-proves the full matrix (never-cheap rule).
  return 'core';
}

/** OR the per-file lanes into the single key ci.yml compares. docs cannot
 * downgrade anything: ANY non-docs bit upgrades the whole key. */
export function mergeLanes(bits) {
  const nonDocs = bits.filter((b) => b !== 'docs');
  if (nonDocs.length === 0) return bits.length ? 'docs' : 'core';
  if (nonDocs.includes('contract')) return 'contract';
  if (nonDocs.includes('core')) return 'core';
  return 'lane';
}

/** The verdict the workflow compares — one of the lane keys. */
export function plan(base, changedPaths) {
  const bits = changedPaths.map(laneOfPath);
  const counts = { docs: 0, contract: 0, plugin: 0, ui: 0, strategy: 0, other: 0 };
  for (const b of bits) counts[b === 'core' ? 'other' : b] += 1;
  const nonDocs = bits.filter((b) => b !== 'docs');
  let lanes;
  if (bits.length === 0) {
    // ZERO files changed vs base: never-cheap ⇒ full matrix. Also covers the
    // pathological "the diff source lied" case (plumbing failures must not
    // look cheap).
    lanes = 'core';
  } else if (nonDocs.length === 0) {
    lanes = 'docs';
  } else if (mergeLanes(bits) === 'contract') {
    lanes = 'contract';
  } else {
    lanes = mergeLanes(bits); // 'core' | 'lane'
  }
  return { lanes, breakdown: counts };
}

/** The self-test: teeth first (#844 acceptance) — a misrouted docs change
 * proves the point of the whole ticket. */
export function selfTest() {
  const checks = [];
  const expect = (name, ok) => checks.push({ name, ok });
  const pathsOf = (arr) => plan('origin/main', arr);

  // A README-only change: docs-only fixture ⇒ NO heavy build.
  const docsOnly = pathsOf(['README.md', 'docs/DEV_V0_3.md', 'assets/screenshots/a.png', 'CONTRIBUTING.md']);
  expect('docs-only change classifies docs (no heavy lane)', docsOnly.lanes === 'docs');

  // Zero-file diff: fail-never-cheap ⇒ full matrix (never a silent green).
  const zero = pathsOf([]);
  expect('empty diff ⇒ core (fail-never-cheap)', zero.lanes === 'core');

  // docs cannot downgrade: mixed docs+code ⇒ heavy.
  const mixed = pathsOf(['README.md', 'core/blitzkrieg_core/src/lib.rs']);
  expect('mixed docs+code runs the heavy matrix', mixed.lanes === 'core');

  // contract change fans out to every downstream suite.
  const contract = pathsOf(['core/bk_contract/src/types.rs']);
  expect('bk_contract change ⇒ contract lane (all downstream suites)', contract.lanes === 'contract');

  // one plugin: contract + plugin suites — scheduled, never a silent skip.
  const plugin = pathsOf(['extensions/kalshi/src/lib.rs']);
  expect('plugin change schedules the plugin lane', plugin.lanes === 'lane');

  // ui lane.
  const ui = pathsOf(['ui/webapp/webui/src/main.ts']);
  expect('ui change schedules the ui lane', ui.lanes === 'lane');

  // strategy lane.
  const strategy = pathsOf(['user_layer/parity_strategy/parity_strategy.rs']);
  expect('strategy change schedules the strategy lane', strategy.lanes === 'lane');

  // lockfile/manifests ⇒ full matrix.
  const lock = pathsOf(['Cargo.lock']);
  expect('lockfile change runs the full matrix', lock.lanes === 'core');

  // workflow/scripts changes ⇒ full matrix (the checker itself must be proven).
  const plumbing = pathsOf(['.github/workflows/ci.yml', 'scripts/dep-direction-check.mjs']);
  expect('workflow & guard-script changes run the full matrix', plumbing.lanes === 'core');

  // Screenshot-only: docs (never executable).
  const shots = pathsOf(['assets/screenshots/x/y.png']);
  expect('screenshot-only stays docs', shots.lanes === 'docs');

  // Fail-never-cheap: an unknown cheap-looking file lands in core.
  const odd = pathsOf(['blog/x.txt']);
  expect('unclassified file fail-goes-core, never skipped', odd.lanes === 'core');

  const failed = checks.filter((c) => !c.ok);
  for (const c of checks) console.log(`${c.ok ? 'ok  ' : 'RED '} lane-planner self-test: ${c.name}`);
  if (failed.length) {
    console.error(`\nci-lane-planner self-test: ${failed.length} RED`);
    process.exit(1);
  }
  console.log(`\nci-lane-planner self-test: all green (${checks.length} checks)`);
  process.exit(0);
}

// ── main ─────────────────────────────────────────────────────────────────────

if (process.argv.includes('--self-test')) {
  selfTest();
}

if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) {
  const [base] = process.argv.slice(2);
  if (!base) {
    console.error('usage: ci-lane-planner.mjs <base>   (e.g. origin/main or a sha)');
    process.exit(2);
  }
  const paths = changedPaths(base);
  const verdict = plan(base, paths);
  console.log(JSON.stringify(verdict));
}

/** Changed-file list vs base. Always resolves through the merge-base (so a
 * stale branch does not re-report files its base already changed), then
 * diffs `mergeBase..HEAD`. PRs and pushes both want the merge-base form;
 * an EMPTY diff ⇒ caller's fail-never-cheap 'core' (plan() handles it). */
export function changedPaths(base, cwd = process.cwd()) {
  const mergeBases = execFileSync('git', ['merge-base', '-a', base, 'HEAD'], { cwd, encoding: 'utf8' })
    .trim()
    .split('\n')
    .filter(Boolean);
  const spec = mergeBases.length ? mergeBases[0] : base;
  const out = execFileSync('git', ['diff', '--name-only', `${spec}..HEAD`], {
    cwd,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  return out.split('\n').filter(Boolean);
}
