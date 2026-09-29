#!/usr/bin/env node
/**
 * strategy:declaration-check — DEV_V0_3 §16.4 gate 2 (E24 / #330).
 *
 * The §7.4 contract: `bk_strategy_declare_modes` is the sixth optional symbol;
 * its payload is validated by ONE validator (`parse_strategy_modes`) and an
 * INVALID declaration REFUSES the load — declared-but-broken is a
 * configuration error, never a silent opt-out. The case matrix is §16.4:
 * 3 legal payloads load and register; 5 illegal ones are refused with the
 * validator's diagnostic (`index=…`, `got="…"`, `Empty`).
 *
 * The real verdict drives a REAL dry core over UDS (`strategy.load`), through
 * `user_layer/examples/e24_modes_fixture` — a hand-written ABI whose
 * `bk_strategy_declare_modes` returns whatever this gate writes to
 * `BK_E24_DECLARATION_FILE` between loads, so one dylib serves every case.
 * The fixture is why the core runs with
 * `BLITZKRIEG_STRATEGY_ALLOW_DIRS=user_layer/examples`.
 *
 * Usage:
 *   node scripts/strategy-declaration-check.mjs                    # real verdict
 *   node scripts/strategy-declaration-check.mjs --self-test        # fixtures, no binary
 *   node scripts/strategy-declaration-check.mjs --teeth            # must go red
 *   node scripts/strategy-declaration-check.mjs --compat-dylib P   # + load a 0.2
 *       library (no declare_modes symbol at all => "undeclared" => registers).
 *       The E24 acceptance "0.2-built spread_arb_strategy.dylib loads WITHOUT
 *       recompiling" is evidenced by running exactly this.
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn, execFileSync } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const EXAMPLES = join(ROOT, 'user_layer', 'examples');
const DYLIB_EXT = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so';
const FIXTURE_DYLIB = join(EXAMPLES, 'target', 'release', `libe24_modes_fixture.${DYLIB_EXT}`);
const EXAMPLE_DYLIB = join(EXAMPLES, 'target', 'release', `libmomentum_alpha_strategy.${DYLIB_EXT}`);
const FIXTURE_NAME = 'e24_modes_fixture';

// §7.4 case matrix — §16.4: 3 legal load, 5 illegal refuse, diagnostics carry
// `index` (and `got` where a token exists). `canon` is the receipt the CURRENT
// implementation produces; --self-test pins the judge against it.
const REFUSE = 'invalid modes declaration';
const CASES = [
  { name: 'legal: full declaration', legal: true,
    payload: '{"modes":[{"market_type":"prediction","structure":"binary_outcome_wheel","capabilities":["websocket_feed","level2_snapshot"]}]}' },
  { name: 'legal: market_type only (loose)', legal: true,
    payload: '{"modes":[{"market_type":"prediction"}]}' },
  { name: 'legal: empty capabilities (requires nothing)', legal: true,
    payload: '{"modes":[{"market_type":"prediction","structure":"binary_outcome_wheel","capabilities":[]}]}' },
  { name: 'illegal: not JSON', legal: false, payload: 'not json at all',
    canon: 'modes payload is not JSON: expected value at line 1 column 1', expect: ['not JSON'] },
  { name: 'illegal: empty modes array (declared-but-empty)', legal: false, payload: '{"modes":[]}',
    canon: "modes payload declares an empty array (Empty: declared-but-empty is an error, not 'undeclared')",
    expect: ['Empty'] },
  { name: 'illegal: missing market_type', legal: false,
    payload: '{"modes":[{"structure":"binary_outcome_wheel"}]}',
    canon: 'mode[index=0] is missing market_type (the only required field)',
    expect: ['index=0', 'missing market_type'] },
  { name: 'illegal: unknown structure', legal: false,
    payload: '{"modes":[{"market_type":"prediction","structure":"nonexistent_structure"}]}',
    canon: 'mode[index=0] has unknown structure got="nonexistent_structure"',
    expect: ['index=0', 'got="nonexistent_structure"'] },
  { name: 'illegal: unknown capability', legal: false,
    payload: '{"modes":[{"market_type":"prediction","capabilities":["warp_drive"]}]}',
    canon: 'mode[index=0] has unknown capability got="warp_drive"',
    expect: ['index=0', 'got="warp_drive"'] },
];

/** The pure verdict: problems this receipt has against this case. Every
 * missing expectation is NAMED in a problem — that is what lets --teeth pin
 * the exact diagnostic (Empty, got=…) a broken validator would lose. */
function judge(c, receipt) {
  const r = String(receipt ?? '');
  const problems = [];
  if (c.legal) {
    if (!r.includes('registered')) {
      problems.push(`expected a successful registration, got: ${r.slice(0, 200)}`);
    }
    return problems;
  }
  if (!r.includes(REFUSE)) {
    problems.push(`expected refusal with "${REFUSE}", got: ${r.slice(0, 200)}`);
  }
  for (const want of c.expect) {
    if (!r.includes(want)) problems.push(`refusal lacks ${want}: ${r.slice(0, 200)}`);
  }
  return problems;
}

function selfTest() {
  const rows = [];
  for (const c of CASES) {
    rows.push({
      name: `canon: ${c.name}`,
      c,
      receipt: c.legal
        ? `${FIXTURE_NAME}@0.1.0 registered into the engine dispatch (disabled)`
        : `Failed { path: x, reason: ${REFUSE}: ${c.canon} }`,
      want: 0,
    });
  }
  rows.push(
    { name: 'a legal payload that is refused is red', c: CASES[0],
      receipt: `Failed { path: x, reason: ${REFUSE}: mode[index=0] has unknown structure got="x" }`, want: 1 },
    { name: 'an illegal payload that registers is red (the R10 shape)', c: CASES[4],
      receipt: `${FIXTURE_NAME}@0.1.0 registered into the engine dispatch (disabled)`, want: 1, mention: 'Empty' },
    { name: 'a refusal without index=0 is red', c: CASES[5],
      receipt: `Failed { path: x, reason: ${REFUSE}: missing market_type }`, want: 1 },
    { name: 'a refusal that lost its got= diagnostic is red', c: CASES[7],
      receipt: `Failed { path: x, reason: ${REFUSE}: mode[index=0] has unknown capability }`, want: 1 },
  );
  let bad = 0;
  for (const r of rows) {
    const problems = judge(r.c, r.receipt);
    const ok = r.want === 0 ? problems.length === 0 : problems.length > 0
      && (r.mention === undefined || problems.some((p) => p.includes(r.mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${r.name} — ${JSON.stringify(problems)}`); }
    else console.log(`  ok   ${r.name}`);
  }
  if (bad > 0) { console.error(`\ndeclaration-check self-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\ndeclaration-check self-test: ${rows.length} fixtures passed`);
}

/**
 * --teeth: feed the BAD implementation's output to the judge and EXPECT
 * failure (§A.4 step 3). The mutation is §16.6 row 2 / risk R10: the
 * validator's `Empty` branch turned into "treat as undeclared" — the kernel
 * would then REGISTER an empty declaration, and the judge must go red naming
 * `Empty`. A judge that stays green here is the defect R10 warns about.
 */
function teeth() {
  const mutations = [
    { name: 'R10 mutation: {"modes":[]} treated as undeclared → registered',
      c: CASES.find((c) => c.payload === '{"modes":[]}'),
      receipt: `${FIXTURE_NAME}@0.1.0 registered into the engine dispatch (disabled)`,
      mention: 'Empty' },
    { name: 'mutation: refusal loses its got= diagnostic',
      c: CASES.find((c) => c.name.includes('unknown capability')),
      receipt: `Failed { path: x, reason: ${REFUSE}: mode[index=0] has unknown capability }`,
      mention: 'got="warp_drive"' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = judge(m.c, m.receipt);
    const caught = problems.length > 0 && problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems[0].slice(0, 160)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\ndeclaration-check --teeth: every broken-validator output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\ndeclaration-check --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

function shell(cmd) {
  return execFileSync('/bin/bash', ['-c', cmd], { encoding: 'utf8', timeout: 600000 });
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();
  const compatAt = argv.indexOf('--compat-dylib');
  const compatDylib = compatAt >= 0 ? argv[compatAt + 1] : null;

  if (!existsSync(CORE)) {
    console.error(`missing core binary ${CORE}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  console.log('building user_layer/examples (fixture + reference strategy)…');
  shell(`cd "${EXAMPLES}" && cargo build --release --locked`);
  for (const d of [FIXTURE_DYLIB, EXAMPLE_DYLIB]) {
    if (!existsSync(d)) { console.error(`missing fixture dylib ${d}`); process.exit(2); }
  }

  const gate = createChecks();
  const { check } = gate;
  const temp = mkdtempSync(join(tmpdir(), 'bk-e24-decl-'));
  const sock = join(tmpdir(), `bk-e24-decl-${process.pid}.sock`);
  const payloadFile = join(temp, 'declaration.txt');
  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '6',
    '--engine', '--no-discovery', '--no-event-archive',
    '--no-trade-log', '--no-order-log', '--no-position-log',
    '--no-strategy-dir', '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0',
  ];
  // user_layer/examples is not an approved strategy root; the gate declares it
  // for this dry core exactly so the fixture can be loaded (see loader.rs
  // ENV_ALLOW_DIRS). Scratch cwd, no logs, no network.
  const proc = spawn(CORE, args, {
    stdio: ['ignore', 'ignore', 'pipe'],
    cwd: temp,
    env: {
      ...process.env,
      BLITZKRIEG_STRATEGY_ALLOW_DIRS: EXAMPLES,
      BK_E24_DECLARATION_FILE: payloadFile,
    },
  });

  try {
    await waitForSocket(sock, { timeoutMs: 10000 });
    const client = await CoreClient.connect({ socketPath: sock });
    await client.request('core.ready');

    for (const c of CASES) {
      writeFileSync(payloadFile, c.payload);
      const receipt = await client.request('strategy.load', { path: FIXTURE_DYLIB });
      const problems = judge(c, receipt);
      check(c.name, problems.length === 0,
        problems.length > 0 ? problems.join(' | ') : String(receipt).slice(0, 110));
      // Unload EVERY successful registration — including one a broken case
      // produced — so a duplicate-name rejection can never poison later cases.
      if (String(receipt).includes('registered')) {
        await client.request('strategy.unload', { name: FIXTURE_NAME });
      }
    }

    // The reference example goes through the macro path: its declaration is
    // generated by `export_strategy!` and must load+register unchanged.
    const ex = await client.request('strategy.load', { path: EXAMPLE_DYLIB });
    check('reference example momentum_alpha loads (macro-generated declaration)',
      String(ex).includes('registered'), String(ex).slice(0, 130));

    if (compatDylib) {
      if (!existsSync(compatDylib)) { console.error(`missing compat dylib ${compatDylib}`); process.exit(2); }
      const r = await client.request('strategy.load', { path: compatDylib });
      check('0.2-built library loads unmodified (no declare_modes symbol = undeclared)',
        String(r).includes('registered'), String(r).slice(0, 130));
    }

    client.stop();
  } finally {
    rmSync(temp, { recursive: true, force: true });
    try { unlinkSync(sock); } catch {}
  }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — ${CASES.length} declaration cases + reference example`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`declaration-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
