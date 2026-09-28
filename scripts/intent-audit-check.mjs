#!/usr/bin/env node
/**
 * intent:audit-check — DEV_V0_3 §16.4 gate 7 (E25 / #331, §3.4).
 *
 * The contract: EVERY strategy suggestion is arbitrated by the four-gate
 * pipeline and leaves EXACTLY ONE audit line in `data/audit/intents.jsonl`,
 * whose shape is self-consistent:
 *   * `gates` carries at least one trace, and the LAST outcome agrees with the
 *     decision — APPROVED/MODIFIED carry NO reject trace; REJECTED carries
 *     EXACTLY one (§14.1 E25 acceptance 1);
 *   * an approved suggestion shows the FULL gate run — "Gate 1 then return"
 *     shows up as `gates[1] missing` (§16.6, reverse acceptance A);
 *   * an ILLEGAL suggestion (price outside (0,1]) is refused at LEGALITY and
 *     NEVER becomes an order — if the audit shows one approved and placed, the
 *     gate prints `reached OME` (§16.6, reverse acceptance B: the gate proves
 *     the pipeline really does keep illegal intents out of the OME).
 *
 * The real verdict drives a REAL dry core with a generated probe strategy
 * (the `blitzkrieg-new-strategy` template + one patched-in ILLEGAL entry), so
 * every assertion runs against the live pipeline + audit writer.
 *
 * The rejection-storm half of the acceptance (4000 rejections/second: audit
 * line-by-line, pushes folded to ≤ 1/s) is pinned by the Rust unit tests
 * `arbitration::tests::four_thousand_*` / `a_rejection_storm_*` — a JS gate
 * cannot drive 4000 intents/second through a real engine, and pretending it
 * can would be a weaker check than the unit test.
 *
 * Usage:
 *   node scripts/intent-audit-check.mjs              # the real verdict
 *   node scripts/intent-audit-check.mjs --self-test  # judge fixtures, no binary
 *   node scripts/intent-audit-check.mjs --teeth      # must go red
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn, execFileSync } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket, sleep } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, readFileSync, rmSync, unlinkSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const NEW = join(ROOT, 'scripts', 'blitzkrieg-new-strategy.mjs');
const NAME = 'e25_audit_probe';
const DYLIB = join(
  ROOT, 'user_layer', 'strategies', NAME, 'target', 'release',
  process.platform === 'darwin' ? `lib${NAME}.dylib`
    : process.platform === 'win32' ? `${NAME}.dll` : `lib${NAME}.so`,
);
const ROUND_SEC = 3600;
const AUDIT_LINE = 'e25_illegal_probe';

/**
 * The pure verdict for ONE audit record — every problem names what a broken
 * pipeline would have hidden, using the §16.6 vocabulary.
 */
export function judgeRecord(r) {
  const problems = [];
  if (!r || typeof r !== 'object') return ['record is not an object'];
  const status = r.decision?.status;
  if (!status) problems.push('record has no decision.status');
  const gates = Array.isArray(r.gates) ? r.gates : [];
  if (gates.length < 1) problems.push('gates empty — no trace at all');
  const rejects = gates.filter((g) => g?.outcome === 'REJECT');
  if (status === 'REJECTED') {
    if (rejects.length !== 1) {
      problems.push(`REJECTED must carry exactly one REJECT trace, got ${rejects.length}`);
    }
    if (!r.decision.gate) problems.push('REJECTED does not name the refusing gate');
    if (!r.decision.detail) problems.push('REJECTED carries no kernel detail');
  } else if (status) {
    if (rejects.length > 0) problems.push(`${status} carries a REJECT trace — decision/trace disagree`);
    if (gates.length < 2) {
      problems.push('gates[1] missing — approved after Gate 1 alone (the pipeline short-circuited)');
    }
  }
  // Reverse acceptance B: an illegal suggestion that was NOT refused reached
  // the order path. Price must be a legal prediction price — this is the one
  // place the gate parses a number, and it only reads strings the kernel wrote.
  const price = Number(r.intent?.price);
  if (Number.isFinite(price) && !(price > 0 && price <= 1) && status !== 'REJECTED') {
    problems.push(`reached OME — illegal suggestion price ${r.intent?.price} left the gates as ${status}`);
  }
  return problems;
}

/** The probe's two expected records, judged as a SET (normal + teeth B).
 * The illegal suggestion is identified by its price ("1.5", outside the
 * prediction band) — the wire carries the kernel's OrderRequest, whose
 * decimal fields are strings. */
export function judgeProbe(records, { illegalPlaced = false } = {}) {
  const problems = [];
  const mine = records.filter((r) => r.strategy === NAME);
  if (mine.length < 2) {
    problems.push(`expected an audit record for BOTH probe suggestions, got ${mine.length}`);
    return problems;
  }
  const isIllegal = (r) => Number(r?.intent?.price) === 1.5;
  const rejected = mine.find((r) => isIllegal(r) && r.decision?.status === 'REJECTED');
  const approved = mine.find((r) => !isIllegal(r) && r.decision?.status === 'APPROVED');
  if (!rejected) problems.push(`no REJECTED record for the illegal suggestion (gate LEGALITY)`);
  else {
    problems.push(...judgeRecord(rejected).map((p) => `illegal probe: ${p}`));
    if (rejected.decision?.gate !== 'LEGALITY') {
      problems.push('illegal suggestion was not refused at LEGALITY');
    }
  }
  if (!approved) problems.push('no APPROVED record for the legal suggestion');
  else problems.push(...judgeRecord(approved).map((p) => `legal probe: ${p}`));
  if (illegalPlaced) problems.push('reached OME — an order exists for the illegal suggestion');
  return problems;
}

function selfTest() {
  const good = { tsMs: 1, strategy: NAME, intentId: 'i1', latencyUs: 4,
    intent: { token: 'UP', price: '0.39', reason: 'dip' },
    decision: { status: 'APPROVED', request_id: 'k' },
    gates: [
      { gate: 'LEGALITY', outcome: 'PASS', detail: 'ok' },
      { gate: 'RISK', outcome: 'PASS', detail: 'ok' },
      { gate: 'RESERVATION', outcome: 'PASS', detail: 'ok' },
      { gate: 'PHYSICS', outcome: 'PASS', detail: 'ok' },
    ] };
  const rejected = { ...good, intentId: 'i2',
    intent: { token: 'UP', price: '1.5', reason: AUDIT_LINE },
    decision: { status: 'REJECTED', gate: 'LEGALITY', reason: 'OUT_OF_PRICE_BAND', detail: 'prediction price must be in (0,1]' },
    gates: [{ gate: 'LEGALITY', outcome: 'REJECT', detail: 'prediction price must be in (0,1]' }] };
  const rows = [
    ['a full four-gate approval is clean', judgeRecord(good), 0, null],
    ['a one-trace rejection is clean', judgeRecord(rejected), 0, null],
    ['teeth A: approved after Gate 1 alone is red', judgeRecord({ ...good, gates: good.gates.slice(0, 1) }), 1, 'gates[1] missing'],
    ['an approval carrying a REJECT trace is red', judgeRecord({ ...good, gates: [...good.gates.slice(0, 3), { gate: 'PHYSICS', outcome: 'REJECT', detail: 'x' }] }), 1, null],
    ['a REJECTED with two reject traces is red', judgeRecord({ ...rejected, gates: [rejected.gates[0], rejected.gates[0]] }), 1, null],
    ['teeth B: an illegal suggestion approved is red', judgeRecord({ ...good, intent: rejected.intent }), 1, 'reached OME'],
    ['a record with no gates is red', judgeRecord({ ...good, gates: [] }), 1, null],
    ['the probe set (good kernel) is clean', judgeProbe([rejected, good]), 0, null],
    ['the probe set (teeth B kernel: illegal placed) is red', judgeProbe([good, { ...good, intentId: 'i3', intent: { price: '0.39' } }], { illegalPlaced: true }), 1, 'reached OME'],
  ];
  let bad = 0;
  for (const [name, problems, want, mention] of rows) {
    const ok = want === 0 ? problems.length === 0
      : problems.length > 0 && (mention === null || problems.some((p) => p.includes(mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — ${JSON.stringify(problems)}`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nintent-audit self-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\nintent-audit self-test: ${rows.length} fixtures passed`);
}

function teeth() {
  // §16.6: feed the two broken implementations' OUTPUT to the judge; the judge
  // must go red naming the expected message, else this gate has no teeth.
  const good = { strategy: NAME, intent: { token: 'UP', price: '0.39' }, latencyUs: 4,
    decision: { status: 'APPROVED' },
    gates: ['LEGALITY', 'RISK', 'RESERVATION', 'PHYSICS'].map((g) => ({ gate: g, outcome: 'PASS', detail: 'ok' })) };
  const mutations = [
    { name: 'teeth A: process_intent returns Approved after Gate 1',
      record: { ...good, gates: good.gates.slice(0, 1) }, mention: 'gates[1] missing' },
    { name: 'teeth B: Gate 1 validate() neutralized → illegal suggestion survives',
      record: { ...good, intent: { token: 'UP', price: '1.5', reason: AUDIT_LINE }, decision: { status: 'APPROVED' } },
      mention: 'reached OME' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = judgeRecord(m.record);
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 140)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\nintent-audit --teeth: every broken pipeline output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\nintent-audit --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

function shell(cmd) {
  return execFileSync('/bin/bash', ['-c', cmd], { encoding: 'utf8', timeout: 600000 });
}

/** Patch the generated template: BEFORE its own dip rule, emit ONE illegal
 * suggestion (price outside the prediction band) per evaluation. */
function patchProbe() {
  const librs = join(ROOT, 'user_layer', 'strategies', NAME, 'src', 'lib.rs');
  const src = readFileSync(librs, 'utf8');
  const anchor = 'let mut buys = 0;';
  if (!src.includes(anchor)) {
    console.error(`probe template does not look like the generator's output: ${librs}`);
    process.exit(2);
  }
  const patch = `${anchor}
        // e25 audit probe: an ILLEGAL suggestion — the kernel must refuse it
        // at Gate 1 and it must NEVER reach the OME (intent-audit-check). It
        // rides the DOWN token (whose book this probe never feeds) so it can
        // never crowd the template's legal UP entry out of the per-token
        // dedup ahead of arbitration.
        if let Some(m0) = ctx.markets.first() {
            intents.entries.push(Entry {
                token: m0.down_token.clone(),
                price: "1.5".into(),
                reason: "${AUDIT_LINE}".into(),
                shares: None,
            });
        }`;
  writeFileSync(librs, src.replace(anchor, patch));
}

import { writeFileSync } from 'fs';

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  if (!existsSync(CORE)) {
    console.error(`missing core binary ${CORE}: cargo build --release --workspace --locked`);
    process.exit(2);
  }

  const gate = createChecks();
  const { check } = gate;
  const temp = mkdtempSync(join(tmpdir(), 'bk-e25-audit-'));
  // The probe crate lives where a developer's crate lives; the CORE gets the
  // scratch dir (so the audit writes under the scratch data/).
  try {
    shell(`cd "${ROOT}" && node scripts/blitzkrieg-new-strategy.mjs ${NAME}`);
    patchProbe();
    shell(`cd "${join(ROOT, 'user_layer', 'strategies', NAME)}" && cargo build --release -q`);
    if (!existsSync(DYLIB)) { console.error(`missing probe dylib ${DYLIB}`); process.exit(2); }

    const sock = join(tmpdir(), `bk-e25-audit-${process.pid}.sock`);
    try { unlinkSync(sock); } catch {}
    const args = [
      '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
      '--seed-balance', '1000', '--max-order-notional', '6',
      '--engine', '--no-discovery', '--no-event-archive',
      '--no-trade-log', '--no-order-log', '--no-position-log',
      '--no-strategy-dir', '--round-sec', String(ROUND_SEC),
      '--min-round-age', '0', '--min-time-left', '0',
    ];
    const proc = spawn(CORE, args, { stdio: ['ignore', 'ignore', 'pipe'], cwd: temp });
    await waitForSocket(sock, { timeoutMs: 10000 });
    const client = await CoreClient.connect({ socketPath: sock });
    await client.request('core.ready');

    const receipt = await client.request('strategy.load', { path: DYLIB });
    check('probe strategy loads', String(receipt).includes('registered'), String(receipt).slice(0, 120));
    const en = await client.request('strategy.enable', { name: NAME, enabled: true });
    check('probe enabled', en.found === true, JSON.stringify(en));

    const now = Date.now();
    const slot = Math.floor(now / 1000 / ROUND_SEC);
    await client.request('engine.markets', {
      markets: [{
        asset: 'BTC', conditionId: '0xc', questionId: '0xq',
        upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
        expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
        negRisk: true, question: 'BTC up/down',
      }],
    });
    const feed = async (mid, depth = 100) => client.request('engine.book', {
      tokenId: 'UP',
      bids: [{ price: mid - 0.01, size: depth }],
      asks: [{ price: mid + 0.01, size: depth }],
    });
    await feed(0.55); // warm book
    await sleep(150);
    await feed(0.39); // dip: the template's legal entry fires; the illegal one fires every cycle
    let placed = false;
    for (let i = 0; i < 100 && !placed; i++) {
      await sleep(60);
      const orders = (await client.request('orders.list')).orders || [];
      placed = orders.some((o) => o.strategy === NAME);
    }
    check('the legal suggestion became an order', placed);

    const auditPath = join(temp, 'data', 'audit', 'intents.jsonl');
    let records = [];
    for (let i = 0; i < 50 && records.length < 2; i++) {
      await sleep(100);
      if (existsSync(auditPath)) {
        records = readFileSync(auditPath, 'utf8')
          .split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
      }
    }
    check('the audit file exists and has records', records.length >= 2, `${records.length} record(s)`);

    const mine = records.filter((r) => r.strategy === NAME);
    // One storm of illegal suggestions is ONE story; check the first few
    // individually and the SET verdict below.
    for (const r of mine.slice(0, 5)) {
      const problems = judgeRecord(r);
      check(`audit record ${r.intentId} self-consistent`, problems.length === 0,
        problems.length > 0 ? problems.join(' | ') : `${r.decision.status} × ${r.gates.length} gates`);
    }
    const illegalPlaced = ((await client.request('orders.list')).orders || [])
      .some((o) => Number(o.price) === 1.5);
    check('probe set consistent (illegal refused at LEGALITY, never an order)',
      judgeProbe(records, { illegalPlaced }).length === 0,
      judgeProbe(records, { illegalPlaced }).join(' | ').slice(0, 200));

    client.stop();
  } finally {
    rmSync(join(ROOT, 'user_layer', 'strategies', NAME), { recursive: true, force: true });
    rmSync(temp, { recursive: true, force: true });
  }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — every suggestion audited, self-consistent, and illegal ones kept out of the OME`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`intent-audit-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
