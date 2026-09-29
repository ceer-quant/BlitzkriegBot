#!/usr/bin/env node
/**
 * plugin:modes-check — DEV_V0_3 §16.4 gate (E27 / #333, §7.5 / §8.2 / §8.3).
 *
 * The contract: the mode declaration a strategy ships (§2.3 wire objects) and
 * the mode declaration a market plugin ships are judged by ONE rule (§7.5) at
 * THREE handshake sites (§8.2) — startup scan (log only), `strategy.load`
 * (refuse), `strategy.enable` (refuse the enable, never the stand-down) — and
 * both lists carry the verdict (§8.3: `modes`/`compatible`/`incompatibleReason`
 * on strategy rows; `structure`/`capabilities`/`capabilitiesBits` on plugin
 * rows). Directionality is the one rule that is easy to get backwards: **a
 * strategy may be loose, a plugin must be specific** — a plugin declaring
 * `structure: None` is INCOMPATIBLE with a strategy requiring a concrete
 * structure.
 *
 * The real verdict drives a REAL dry core over UDS (the E24 fixture dylib,
 * whose `bk_strategy_declare_modes` reads `BK_E24_DECLARATION_FILE` fresh at
 * every load), so every assertion lands on the live handshake:
 *   1. `market.list` — the readable capability names and the raw bitmap are
 *      interchangeable (双向互推), and the declared structure names the plugin's
 *      mode; 0.2 rows are unchanged apart from the new fields.
 *   2. `strategy.list` — a 0.2 reference strategy (momentum_alpha, declared)
 *      reads compatible; a fixture declaring FUTURES against the prediction
 *      plugin reads `compatible: false` with a reason that names BOTH sides
 *      (R-B: `futures`, `prediction`, the plugin name).
 *   3. `strategy.enable` — the incompatible fixture is REFUSED
 *      (`enabled: false, found: true, reason`), while the compatible 0.2
 *      strategy enables as always (0.2 zero-change).
 *   4. startup scan + `--enable-strategy` — the scan REGISTERS the
 *      incompatible library (log only, receipt names the mismatch) and the
 *      startup enable is SKIPPED with an ERROR line; the boot survives (#265
 *      counts it resolved).
 *
 * Usage:
 *   node scripts/plugin-modes-check.mjs              # the real verdict
 *   node scripts/plugin-modes-check.mjs --self-test  # judge fixtures, no binary
 *   node scripts/plugin-modes-check.mjs --teeth      # must go red
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn, execFileSync } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket, sleep } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, readdirSync, rmSync, unlinkSync, writeFileSync, copyFileSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const EXAMPLES = join(ROOT, 'user_layer', 'examples');
const DYLIB_EXT = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so';
const FIXTURE_DYLIB = join(EXAMPLES, 'target', 'release', `libe24_modes_fixture.${DYLIB_EXT}`);
const FIXTURE_NAME = 'e24_modes_fixture';
const REFERENCE_NAME = 'momentum_alpha';

/** The §7.2 wire names ↔ bits table, mirrored from
 * `blitzkrieg_market_api::modes::CAPABILITY_NAMES`. The gate pins the wire
 * against THIS copy — a core that renumbers or renames a bit fails the
 * cross-check below. 10 bits stay far inside Number's exact range. */
const CAPS_BIT = {
  websocket_feed: 1,
  level2_snapshot: 2,
  kline_stream: 4,
  trade_stream: 8,
  leverage: 16,
  short_selling: 32,
  batch_orders: 64,
  post_only: 128,
  cancel_on_disconnect: 256,
  maker_rebate: 512,
};

/**
 * One (strategy mode, plugin mode) pair under §7.5 (1)-(3), the JS mirror of
 * `core/blitzkrieg_core/src/market/compat.rs::pair_compatible`. A plugin that
 * cannot name the structure is NOT a match for a concrete-structure strategy —
 * the directionality rule, restated here because every teeth case below is
 * about a core that got it backwards.
 */
function pairOk(mode, plugin) {
  if (mode.market_type !== plugin.type) return false; // (1) strong equality
  if (mode.structure != null && mode.structure !== plugin.structure) return false; // (2)
  const required = (mode.capabilities ?? []).reduce((a, n) => a | (CAPS_BIT[n] ?? 0), 0);
  return ((plugin.capabilitiesBits ?? 0) & required) === required; // (3) superset
}

/** The violation vocabulary — WHAT broke, named so --teeth can demand it. */
function violations(mode, plugin) {
  const v = [];
  if (mode.market_type !== plugin.type) v.push('market-type violated');
  if (mode.structure != null && mode.structure !== plugin.structure) {
    v.push(plugin.structure == null
      ? `unspecified structure accepted: plugin declares no structure but the strategy requires ${mode.structure}`
      : `structure violated (${mode.structure} vs ${plugin.structure})`);
  }
  const required = (mode.capabilities ?? []).reduce((a, n) => a | (CAPS_BIT[n] ?? 0), 0);
  if (((plugin.capabilitiesBits ?? 0) & required) !== required) v.push('capability superset violated');
  return v;
}

/** §8.3 judge for ONE strategy row against the ACTIVE plugin row. The verdict
 * (`compatible`) must agree with the declaration the row itself carries —
 * self-consistency, computable from the wire alone. Undeclared (`modes: null`)
 * participates not (§7.4): always compatible, reason absent. */
export function judgeStrategyRow(row, plugin, want = {}) {
  const problems = [];
  const modes = Array.isArray(row?.modes) ? row.modes : null;
  if (row?.compatible === false && typeof row?.incompatibleReason !== 'string') {
    problems.push('incompatible row carries no incompatibleReason');
  }
  if (modes === null) {
    if (row?.compatible !== true) {
      problems.push('undeclared row (modes null) must read compatible — undeclared participates not');
    }
  } else {
    const perMode = modes.map((m) => violations(m, plugin));
    const expected = perMode.some((v) => v.length === 0);
    if (row?.compatible !== expected) {
      problems.push(expected
        ? 'capability superset violated: the plugin offers a satisfying mode but the row reads incompatible'
        : `expected incompatible: no plugin mode satisfies any declared mode (violations: ${perMode.flat().join('; ') || 'none recorded'})`);
    }
  }
  for (const s of want.reasonContains ?? []) {
    if (!String(row?.incompatibleReason ?? '').includes(s)) {
      problems.push(`incompatibleReason lacks "${s}" — the refusal must name BOTH sides (§8.2)`);
    }
  }
  return problems;
}

/** §8.3 judge for ONE plugin row: the readable names and the bitmap must be
 * interchangeable (双向互推 — the two encodings are ONE table apart), and the
 * declared structure, when present, is a wire-plausible string. */
export function judgeMarketRow(row) {
  const problems = [];
  const names = row?.capabilities;
  if (!Array.isArray(names)) problems.push('market row carries no capabilities array');
  const bits = row?.capabilitiesBits;
  if (typeof bits !== 'number') problems.push('market row carries no capabilitiesBits');
  if (Array.isArray(names) && typeof bits === 'number') {
    const or = names.reduce((a, n) => {
      if (!(n in CAPS_BIT)) problems.push(`unknown capability name "${n}" — the table drifted`);
      return a | (CAPS_BIT[n] ?? 0);
    }, 0);
    if (or !== bits) {
      problems.push(`capabilitiesBits ${bits} != OR(names)=${or} — the bitmap and the names disagree`);
    }
  }
  if (row?.structure != null && typeof row.structure !== 'string') {
    problems.push('structure must be a wire string or null');
  }
  return problems;
}

function selfTest() {
  // The REAL wire shapes, transcribed from a live v0.3 core (§2.3 wire for
  // modes, §8.3 rows for both lists).
  const poly = {
    name: 'polymarket', type: 'prediction', structure: 'binary_outcome_wheel',
    capabilities: ['websocket_feed', 'level2_snapshot', 'post_only'],
    capabilitiesBits: 1 | 2 | 128, active: true,
  };
  const loose = {
    name: REFERENCE_NAME, enabled: false,
    modes: [{ market_type: 'prediction', structure: 'binary_outcome_wheel', capabilities: ['websocket_feed', 'level2_snapshot'] }],
    compatible: true, incompatibleReason: null,
  };
  const futures = {
    name: FIXTURE_NAME, enabled: false,
    modes: [{ market_type: 'futures', structure: 'central_limit_order_book', capabilities: ['leverage'] }],
    compatible: false,
    incompatibleReason: "incompatible modes: strategy 'e24_modes_fixture' wants [futures/central_limit_order_book(leverage)] but active plugin 'polymarket' offers [prediction/binary_outcome_wheel(websocket_feed,level2_snapshot,post_only)]",
  };
  const undeclared = { name: 'legacy', enabled: false, modes: null, compatible: true, incompatibleReason: null };
  const rows = [
    ['a 0.2-declared strategy against its serving plugin is clean', judgeStrategyRow(loose, poly), 0, null],
    ['the R-B row (futures vs prediction) is clean', judgeStrategyRow(futures, poly, { reasonContains: ['futures', 'prediction', 'polymarket'] }), 0, null],
    ['a refusal that drops the plugin name is red', judgeStrategyRow({ ...futures, incompatibleReason: 'incompatible modes: strategy … wants [futures] but active plugin offers [prediction]' }, poly, { reasonContains: ['polymarket'] }), 1, 'BOTH sides'],
    ['an undeclared row reads compatible', judgeStrategyRow(undeclared, poly), 0, null],
    ['an undeclared row marked incompatible is red', judgeStrategyRow({ ...undeclared, compatible: false, incompatibleReason: 'x' }, poly), 1, 'undeclared'],
    ['an incompatible row without a reason is red', judgeStrategyRow({ ...futures, incompatibleReason: null }, poly), 1, 'no incompatibleReason'],
    ['teeth A core (satisfies flipped: incompatible read compatible) is red', judgeStrategyRow({ ...loose, modes: [{ market_type: 'prediction', structure: 'binary_outcome_wheel', capabilities: ['leverage'] }], incompatibleReason: null }, poly), 1, 'superset violated'],
    ['teeth B core (market-type filter dropped: futures read compatible) is red', judgeStrategyRow({ ...futures, compatible: true, incompatibleReason: null }, poly), 1, 'expected incompatible'],
    ['teeth C core (plugin None matched a concrete structure) is red', judgeStrategyRow({ ...loose, modes: [{ market_type: 'prediction', structure: 'central_limit_order_book', capabilities: [] }] }, { ...poly, structure: null, capabilitiesBits: 0 }), 1, 'unspecified structure accepted'],
    ['a plugin row whose names and bits agree is clean', judgeMarketRow(poly), 0, null],
    ['a plugin row whose bitmap disagrees with its names is red', judgeMarketRow({ ...poly, capabilitiesBits: 7 }), 1, 'disagree'],
    ['a plugin row naming an unknown capability is red', judgeMarketRow({ ...poly, capabilities: ['websocket_feed', 'hovercraft'] }), 1, 'drifted'],
    ['a plugin row with a non-string structure is red', judgeMarketRow({ ...poly, structure: 9 }), 1, 'structure'],
  ];
  let bad = 0;
  for (const [name, problems, wantProblems, mention] of rows) {
    const ok = wantProblems === 0 ? problems.length === 0
      : problems.length > 0 && (mention === null || problems.some((p) => p.includes(mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — ${JSON.stringify(problems)}`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nmodes-check self-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\nmodes-check self-test: ${rows.length} fixtures passed`);
}

function teeth() {
  // §16.6 discipline (as intent-audit-check): feed the BROKEN implementation's
  // OUTPUT to the judge and expect red naming the vocabulary. Each record is
  // what a core with that mutation would put on the wire.
  const poly = {
    name: 'polymarket', type: 'prediction', structure: 'binary_outcome_wheel',
    capabilities: ['websocket_feed', 'level2_snapshot', 'post_only'], capabilitiesBits: 131,
  };
  const mutations = [
    { name: 'A: satisfies direction flipped (required must cover plugin) → incompatible row reads compatible',
      row: { name: FIXTURE_NAME, modes: [{ market_type: 'prediction', structure: 'binary_outcome_wheel', capabilities: ['leverage'] }], compatible: true, incompatibleReason: null },
      mention: 'superset violated' },
    { name: 'B: market-type filter dropped → a futures strategy reads compatible against the prediction plugin',
      row: { name: FIXTURE_NAME, modes: [{ market_type: 'futures', structure: 'central_limit_order_book', capabilities: [] }], compatible: true, incompatibleReason: null },
      mention: 'market-type violated' },
    { name: 'C: plugin structure None treated as "matches everything" → a CLOB-requiring strategy reads compatible',
      row: { name: FIXTURE_NAME, modes: [{ market_type: 'prediction', structure: 'central_limit_order_book', capabilities: [] }], compatible: true, incompatibleReason: null },
      plugin: { ...poly, structure: null, capabilitiesBits: 0 },
      mention: 'unspecified structure accepted' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = judgeStrategyRow(m.row, m.plugin ?? poly);
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 150)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\nmodes-check --teeth: every broken-handshake output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\nmodes-check --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

function findReferenceDylib() {
  const dir = join(EXAMPLES, 'target', 'release');
  try {
    for (const f of readdirSync(dir)) {
      if (f.startsWith('libmomentum_alpha') && f.endsWith(`.${DYLIB_EXT}`)) return join(dir, f);
    }
  } catch {}
  return null;
}

/** Spawn a core, wait for its socket, hand back the client AND the process. */
async function bootCore(sock, args, env) {
  const proc = spawn(CORE, ['--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--engine', '--no-discovery', '--no-event-archive',
    '--no-trade-log', '--no-order-log', '--no-position-log',
    '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0', ...args],
    { stdio: ['ignore', 'pipe', 'pipe'], cwd: mkdtempSync(join(tmpdir(), 'bk-e27-modes-')), env });
  await waitForSocket(sock, { timeoutMs: 15000 });
  const client = await CoreClient.connect({ socketPath: sock });
  await client.request('core.ready');
  return { proc, client };
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  if (!existsSync(CORE)) {
    console.error(`missing core binary ${CORE}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  console.log('building user_layer/examples (fixture + reference strategy)…');
  execFileSync('/bin/bash', ['-c', `cd "${EXAMPLES}" && cargo build --release --locked`], { stdio: 'inherit', timeout: 600000 });
  if (!existsSync(FIXTURE_DYLIB)) { console.error(`missing fixture dylib ${FIXTURE_DYLIB}`); process.exit(2); }
  const reference = findReferenceDylib();
  if (!reference) { console.error('missing momentum_alpha dylib under user_layer/examples/target/release'); process.exit(2); }

  const gate = createChecks();
  const { check } = gate;
  const temp = mkdtempSync(join(tmpdir(), 'bk-e27-modes-lib-'));
  // The R-B declaration: futures/CLOB/leverage against the compiled-in
  // prediction plugin — refused by EVERY handshake, named on both sides.
  const declarationFile = join(temp, 'declaration.txt');
  writeFileSync(declarationFile,
    '{"modes":[{"market_type":"futures","structure":"central_limit_order_book","capabilities":["leverage"]}]}');
  // One scratch dir holds both libraries so ONE `--strategy-dir` loads them;
  // it is allow-listed for this dry core exactly so the loads pass the
  // approved-root policy (see loader.rs ENV_ALLOW_DIRS).
  const libs = join(temp, 'libs');
  execFileSync('/bin/bash', ['-c', `mkdir -p "${libs}" && cp "${FIXTURE_DYLIB}" "${reference}" "${libs}/"`]);
  const sock1 = join(tmpdir(), `bk-e27-modes-${process.pid}.sock`);

  try {
    // ── Process 1: the §8.3 lists + the §8.2 enable refusal ──────────────────
    const { proc, client } = await bootCore(sock1, ['--strategy-dir', libs], {
      ...process.env,
      BLITZKRIEG_STRATEGY_ALLOW_DIRS: libs,
      BK_E24_DECLARATION_FILE: declarationFile,
    });
    try {
      const ml = await client.request('market.list');
      const active = (ml.plugins ?? []).find((p) => p.active) ?? ml.plugins?.[0];
      check('market.list has an active plugin', !!active, JSON.stringify(ml).slice(0, 200));
      for (const p of ml.plugins ?? []) {
        const problems = judgeMarketRow(p);
        check(`market.list row "${p.name}": names ↔ bitmap interchangeable (双向互推)`,
          problems.length === 0, problems.join(' | '));
      }
      check('active plugin declares its mode (structure + capabilities present)',
        active.structure === 'binary_outcome_wheel'
        && Array.isArray(active.capabilities) && active.capabilities.length > 0,
        JSON.stringify(active).slice(0, 200));

      // §8.2 row 1 — the startup scan LOGS ONLY: the incompatible fixture is
      // registered (disabled), the compatible 0.2 strategy as always.
      const sl = await client.request('strategy.list');
      const rows = sl.strategies ?? [];
      const fixture = rows.find((r) => r.name === FIXTURE_NAME);
      const ref = rows.find((r) => r.name === REFERENCE_NAME);
      check('startup scan registered the incompatible fixture (log-only handshake)', !!fixture,
        rows.map((r) => r.name).join(','));
      check('startup scan registered the 0.2 reference strategy', !!ref, rows.map((r) => r.name).join(','));

      if (ref) {
        const problems = judgeStrategyRow(ref, active);
        check('0.2 reference row: declared modes compatible (0.2 zero-change)',
          problems.length === 0, problems.join(' | '));
        check('0.2 reference row carries its §2.3 wire declaration',
          Array.isArray(ref.modes) && ref.modes[0]?.market_type === 'prediction'
          && ref.modes[0]?.structure === 'binary_outcome_wheel',
          JSON.stringify(ref.modes));
      }
      if (fixture) {
        const problems = judgeStrategyRow(fixture, active, { reasonContains: ['futures', 'prediction', 'polymarket'] });
        check('R-B: incompatible fixture row is red with a BOTH-sides reason',
          problems.length === 0, problems.join(' | '));
      }

      // §8.2 row 2 — strategy.enable refuses; disable never gated.
      const en = await client.request('strategy.enable', { name: FIXTURE_NAME, enabled: true });
      check('R-B: enable of the incompatible fixture is REFUSED with the §8.2 reason',
        en.found === true && en.enabled === false
        && ['futures', 'prediction', 'polymarket'].every((s) => String(en.reason ?? '').includes(s)),
        JSON.stringify(en).slice(0, 300));
      const off = await client.request('strategy.enable', { name: FIXTURE_NAME, enabled: false });
      check('stand-down is never gated (disable of an incompatible strategy applies)',
        off.found === true && off.enabled === false, JSON.stringify(off));
      if (ref) {
        const enRef = await client.request('strategy.enable', { name: REFERENCE_NAME, enabled: true });
        check('0.2 reference strategy enables as always (handshake invisible to it)',
          enRef.found === true && enRef.enabled === true && !enRef.reason, JSON.stringify(enRef));
      }
    } finally {
      client.stop();
      proc.kill('SIGTERM');
      await sleep(300);
    }

    // ── Process 2: startup `--enable-strategy` vs the handshake (§8.2 row 3) ─
    const sock2 = join(tmpdir(), `bk-e27-modes-boot-${process.pid}.sock`);
    const boot = spawn(CORE, ['--socket', sock2, '--mode', 'dry', '--tick-ms', '50',
      '--seed-balance', '1000', '--engine', '--no-discovery', '--no-event-archive',
      '--no-trade-log', '--no-order-log', '--no-position-log',
      '--strategy-dir', libs, '--enable-strategy', FIXTURE_NAME,
      '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0'],
      { stdio: ['ignore', 'pipe', 'pipe'], cwd: mkdtempSync(join(tmpdir(), 'bk-e27-modes-')), env: {
        ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: libs, BK_E24_DECLARATION_FILE: declarationFile,
      } });
    let output = '';
    boot.stdout?.on('data', (d) => { output += d; });
    boot.stderr?.on('data', (d) => { output += d; });
    await waitForSocket(sock2, { timeoutMs: 15000 });
    await sleep(700); // let the boot sequence finish its logging
    boot.kill('SIGTERM');
    await sleep(300);
    check('startup scan receipt names the mismatch (log-only handshake)',
      /INCOMPATIBLE: incompatible modes/.test(output),
      output.split('\n').filter((l) => l.includes('auto-load')).join(' ⏎ ').slice(0, 200));
    check('startup --enable-strategy skipped with the refusal surfaced',
      /startup enable refused|enabled=\[\]/.test(output),
      output.split('\n').filter((l) => /refused|self-check/.test(l)).join(' ⏎ ').slice(0, 220));
    check('the boot survives (the #265 self-check counts a refused-but-registered request resolved)',
      !output.includes('refusing to start'), output.split('\n').slice(-6).join(' ⏎ ').slice(0, 220));
  } finally {
    rmSync(temp, { recursive: true, force: true });
    try { unlinkSync(sock1); } catch {}
    try { unlinkSync(join(tmpdir(), `bk-e27-modes-boot-${process.pid}.sock`)); } catch {}
  }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — both lists carry the §8.3 verdict, both §8.2 refusal sites hold, 0.2 unchanged`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`plugin-modes-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
