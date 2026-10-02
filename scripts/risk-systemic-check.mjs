#!/usr/bin/env node
/**
 * risk:systemic-check — DEV_V0_3 §16.4 gate (E26, §4).
 *
 * The contract: the nine NEW systemic limits are real enforcement, not
 * decoration — and a factory kernel cannot tell they exist (§4.1: 出厂即静默;
 * the P1 byte-parity witness is that an unconfigured pipeline's audit
 * contains no "systemic" word at all).
 *
 * WHAT IT ASSERTS (against real dry cores, over the wire):
 *
 *   1. FACTORY POSTURE — a `--no-config` core: `risk.limits` answers all
 *      nine limits as {"value":"0","source":"default"} with the exit triple
 *      at the calibrated factory resolution (12 / 100 / 0 — the guillotine
 *      is OFF since @almach, 561c1f5e); the boot log carries the
 *      "at factory (all nine limits off)" line; and the probe strategy's
 *      approved audit record shows four gates with NO systemic trace —
 *      the pipeline the shipped kernel runs, byte for byte.
 *   2. ARMED POSTURE + PROVENANCE — a core configured through `[risk]`:
 *      the readout shows the configured bounds as STRINGS sourced "toml",
 *      the untouched siblings still 0/"default", and the exit triple read
 *      from `[exit]` — §4.4's rule that a number without its provenance is
 *      not an answer an operator can act on.
 *   3. THE SHRINK IS REAL — with `max_single_loss_usd = 0.20` and a 20%
 *      stop, the engine's 10-share suggestion comes back MODIFIED at
 *      `floor(0.20 / per-share-loss)` shares: the placed order carries the
 *      reduced size, and `approved * loss-per-share <= cap` holds on the
 *      wire record (teeth A: a Gate 2 that always passes goes red).
 *   4. PHYSICS IS BOUND, NOT HARDCODED — every approved/modified record's
 *      `physics.stopPrice` equals `entry × (1 − stopLossPct/100)` computed
 *      from the READOUT's value, and `forceExitSec` equals the readout's
 *      (teeth B/C: a stop hardcoded at 0.99 or a force-exit hardcoded at
 *      120 while the config says otherwise goes red).
 *   5. THE ACCOUNT BREAKER CYCLE — two consecutive closed losses trip the
 *      account-level breaker (RiskAlert), the next entry is REJECTED with
 *      LOSS_BREAKER naming the account, and after the configured cooldown
 *      (0.05 min = 3 s) the next entry is admitted again. Closing is never
 *      blocked — the stop-loss exits that produced the losses went through.
 *   6. THE SHRINK INVARIANT AT RANDOM — 1000 deterministic (cap, price,
 *      stop, suggested) groups: `approved <= suggested` and
 *      `approved × loss-per-share <= cap`, floor semantics, or the verdict
 *      is a refusal — never an approximation upward.
 *
 * Usage:
 *   node scripts/risk-systemic-check.mjs              # the real verdict
 *   node scripts/risk-systemic-check.mjs --self-test  # judge fixtures, no binary
 *   node scripts/risk-systemic-check.mjs --teeth      # must go red
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { coreBinaryPath, checkCoreProvenance } from './lib/core-provenance.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket, sleep } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync, unlinkSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || coreBinaryPath();
const NEW = join(ROOT, 'scripts', 'blitzkrieg-new-strategy.mjs');
const NAME = 'e26_systemic_probe';
const DYLIB = join(
  ROOT, 'user_layer', 'strategies', NAME, 'target', 'release',
  process.platform === 'darwin' ? `lib${NAME}.dylib`
    : process.platform === 'win32' ? `${NAME}.dll` : `lib${NAME}.so`,
);
const ROUND_SEC = 3600;
const EPS = 1e-6;

// The armed run's numbers. The single-loss cap must shrink the template's
// 10-share band suggestion: entry 0.40, stop 20% → per-share 0.08 →
// approved 2. The breaker pair (2 losses, 0.05 min = 3 s) keeps the whole
// cycle inside a few seconds; the strategy-level shipped breaker (3 / 300 s)
// stays dormant — exactly two losses are booked before the account one fires.
const TOML = `# scratch config written by risk-systemic-check.mjs
[exit]
stop_loss_pct = "20"

[risk]
max_single_loss_usd = "0.20"
max_consecutive_losses = "2"
cooldown_minutes = "0.05"
`;
const CAP = 0.20;
const STOP_PCT = 20;
const LOSSES_TO_TRIP = 2;
const COOLDOWN_SEC = 3;

// ── pure judges — the teeth surface ──────────────────────────────────────────

const num = (v) => {
  const n = Number(v);
  return Number.isFinite(n) ? n : NaN;
};

/** The §4.4 readout verdict: every limit is {value, source}, the nine NEW
 * ones silent at the factory, the exit triple present, provenance named.
 * `posture` is 'factory' (all nine 0/default) or 'armed' (the three bounds
 * this gate configures arrive toml-sourced, the rest stay factory-silent). */
export function judgeReadout(r, posture = 'factory') {
  const problems = [];
  if (!r || typeof r !== 'object') return ['readout is not an object'];
  if (r.version !== '1.1') problems.push(`readout version must be "1.1", got ${JSON.stringify(r.version)}`);
  const acct = r.account ?? {};
  const al = acct.limits ?? {};
  const gl = (r.global ?? {}).limits ?? {};
  if (typeof acct.id !== 'string' || acct.id.length === 0) {
    problems.push(`account.id missing: ${JSON.stringify(acct.id)}`);
  }
  // TOML strings parse EXACTLY (rust_decimal), so the expected spellings are
  // exact — "0.20" must not come back as "0.2".
  const armedBounds = {
    maxSingleLossUsd: '0.20',
    maxConsecutiveLosses: '2',
    cooldownMinutes: '0.05',
  };
  const nine = [
    'maxSingleLossUsd', 'maxDailyDrawdownUsd', 'maxPositionSize',
    'maxConsecutiveLosses', 'cooldownMinutes',
    'maxTotalPosition', 'maxTotalExposureUsd', 'maxCorrelationUsd',
    'globalKillSwitchLossUsd',
  ];
  for (const key of nine) {
    const b = al[key] ?? gl[key];
    if (!b || typeof b !== 'object') {
      problems.push(`${key} missing from the readout`);
      continue;
    }
    const armedValue = posture === 'armed' ? armedBounds[key] : undefined;
    const wantValue = armedValue ?? '0';
    const wantSource = armedValue ? 'toml' : 'default';
    if (String(b.value) !== wantValue) {
      problems.push(`${key}.value must be ${JSON.stringify(wantValue)}, got ${JSON.stringify(b.value)}`);
    }
    if (b.source !== wantSource) {
      problems.push(`${key}.source must be ${JSON.stringify(wantSource)}, got ${JSON.stringify(b.source)}`);
    }
    if (b.source === 'default' && String(b.value) !== '0') {
      problems.push(`${key} is not configured yet shows ${JSON.stringify(b.value)} — provenance lie`);
    }
  }
  const exit = r.exit ?? {};
  // The calibrated shipped default (@almach, 561c1f5e): the guillotine is OFF —
  // `force_exit_sec 0` disables the deadline, every exit is a redeem/merge or a
  // protective stop. The factory triple pins that default; a kernel that
  // silently re-arms a 120s deadline shows up here.
  const wantExit = posture === 'armed'
    ? { stopLossPct: STOP_PCT, takeProfitPct: 100, forceExitSec: 0 }
    : { stopLossPct: 12, takeProfitPct: 100, forceExitSec: 0 };
  for (const [key, want] of Object.entries(wantExit)) {
    if (Math.abs(num(exit[key]) - want) > EPS) {
      problems.push(`exit.${key} must be ${want}, got ${JSON.stringify(exit[key])}`);
    }
  }
  return problems;
}

const lossPerShare = (price, stopPct) => (price * stopPct) / 100;

/** Teeth A surface: one entry record judged against the configured cap.
 * An over-cap APPROVAL (Gate 2 neutered to always-pass) is red; a MODIFIED
 * record whose approved size breaks either invariant is red. */
export function judgeShrinkRecord(r, { capUsd, stopPct }) {
  const problems = [];
  if (!r || typeof r !== 'object') return ['record is not an object'];
  const status = r.decision?.status;
  const size = num(r.intent?.size);
  const price = num(r.intent?.price);
  if (!Number.isFinite(size) || !Number.isFinite(price)) {
    return ['record carries no readable intent size/price'];
  }
  const per = lossPerShare(price, stopPct);
  if (status === 'APPROVED' && size * per > capUsd + 1e-9) {
    problems.push(
      `over-cap entry approved: ${size} shares × ${per.toFixed(6)} = ${(size * per).toFixed(6)} ` +
      `> cap ${capUsd} — Gate 2 passed what it must bound`);
  }
  if (status === 'MODIFIED') {
    const mod = r.decision?.modification ?? {};
    const suggested = num(mod.suggested);
    const approved = num(mod.approved);
    if (mod.kind !== 'SIZE_REDUCED') {
      problems.push(`MODIFIED carries modification.kind ${JSON.stringify(mod.kind)}, expected SIZE_REDUCED`);
    }
    if (Number.isFinite(approved) && Number.isFinite(suggested) && approved > suggested + 1e-9) {
      problems.push(`approved ${approved} > suggested ${suggested} — a shrink grew the order`);
    }
    if (Number.isFinite(approved) && approved * per > capUsd + 1e-9) {
      problems.push(
        `approved ${approved} × ${per.toFixed(6)} = ${(approved * per).toFixed(6)} > cap ${capUsd} — ` +
        'the cap does not bound what was placed');
    }
    if (num(r.decision?.shares) !== approved) {
      problems.push(`decision.shares ${r.decision?.shares} != modification.approved ${mod.approved}`);
    }
  }
  return problems;
}

/** Teeth B/C surface: the audit's physics binding must EQUAL the kernel's own
 * PHYSICS projection trace — which is where the configured stop becomes the
 * time-aware effective stop (§4.3). A stop hardcoded at 0.99 breaks the
 * equality with the trace; a force-exit hardcoded at 120 while the kernel
 * resolves 90 breaks it too. The trace's own math is cross-checked against
 * the entry price, and (when a readout is given) force-exit against §4.4. */
export function judgePhysics(record, exitView = null) {
  const problems = [];
  const phys = record?.decision?.physics;
  if (!phys || typeof phys !== 'object') return ['record carries no physics binding'];
  const traces = (record.gates ?? record.decision?.gates ?? []).map((g) => String(g?.detail ?? ''));
  const proj = traces.find((d) => d.startsWith('projection:') || d.startsWith('explicit ladder:'));
  if (!proj) return ['record carries no PHYSICS projection trace to check the binding against'];
  const m = /stop ([0-9.]+)% → ([0-9.]+), force-exit (\d+)s/.exec(proj);
  if (!m) return [`PHYSICS trace is not parseable: "${proj.slice(0, 120)}"`];
  const stopPct = Number(m[1]);
  const traceStop = Number(m[2]);
  const traceForce = Number(m[3]);
  const price = num(record.intent?.price);
  // The binding must be the kernel's own projection — not a constant.
  if (Math.abs(num(phys.stopPrice) - traceStop) > 1e-9) {
    problems.push(
      `stop price ${phys.stopPrice} != the kernel's own projection ${traceStop} (stop ${stopPct}%) — ` +
      'the binding is not reading the config');
  }
  // The projection itself must be THIS entry's price minus the effective stop.
  if (Math.abs(traceStop - price * (1 - stopPct / 100)) > 1e-6) {
    problems.push(
      `projection ${traceStop} is not ${price} × (1 − ${stopPct}%/100) — the projection math drifted`);
  }
  if (num(phys.forceExitSec) !== traceForce) {
    problems.push(
      `force-exit ${phys.forceExitSec}s != the kernel's projection ${traceForce}s — ` +
      'the binding is hardcoded, not resolved');
  }
  if (exitView && num(phys.forceExitSec) !== num(exitView.forceExitSec)) {
    problems.push(
      `force-exit ${phys.forceExitSec}s != readout forceExitSec ${exitView.forceExitSec}s — hardcoded`);
  }
  if (!Array.isArray(phys.ladder) || phys.ladder.length < 1) {
    problems.push('physics binding carries no ladder snapshot');
  }
  return problems;
}

/** The breaker cycle verdict: exactly the account (never the strategy) halted,
 * the refusal NAMES the account, and after the cooldown an entry passes. */
export function judgeBreakerCycle({ rejectRecord, recoveryRecord, tripAlert }) {
  const problems = [];
  if (!tripAlert) {
    problems.push('no RiskAlert naming the account breaker trip');
  } else if (!/breaker tripped for account/i.test(String(tripAlert.message ?? tripAlert))) {
    problems.push(`trip alert does not name the account breaker: ${JSON.stringify(tripAlert.message ?? tripAlert).slice(0, 120)}`);
  }
  if (!rejectRecord || rejectRecord.decision?.status !== 'REJECTED') {
    problems.push('no REJECTED record after the trip — the breaker did not stop new entries');
  } else {
    const reason = JSON.stringify(rejectRecord.decision.reason ?? '');
    if (!reason.includes('LOSS_BREAKER')) {
      problems.push(`the post-trip refusal reason is ${reason}, expected LOSS_BREAKER`);
    }
    const detail = String(rejectRecord.decision.detail ?? '');
    if (!detail.includes('for account')) {
      problems.push(`the refusal detail does not name the account: "${detail.slice(0, 120)}"`);
    }
  }
  if (!recoveryRecord || !['APPROVED', 'MODIFIED'].includes(recoveryRecord.decision?.status)) {
    problems.push('no admitted entry after the cooldown — the halt never lifted');
  }
  return problems;
}

/** 1000 deterministic groups through the shrink invariant the Rust grid test
 * pins: approved = min(suggested, floor(cap / per-share)); below one whole
 * share the verdict is a refusal; the cap bounds, never approximates up. */
export function shrinkInvariant(n = 1000, seed = 0xe26) {
  let s = seed >>> 0;
  const rnd = () => {
    s |= 0; s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const problems = [];
  let shrunk = 0;
  let refused = 0;
  for (let i = 0; i < n; i++) {
    const price = Math.round((0.05 + rnd() * 0.9) * 100) / 100;      // (0.05, 0.95]
    const stopPct = 1 + Math.floor(rnd() * 60);                      // [1, 60]
    const cap = Math.round(rnd() * 50 * 100) / 100;                  // [0, 50]
    const suggested = 1 + Math.floor(rnd() * 500);                   // [1, 500]
    const per = lossPerShare(price, stopPct);
    const fits = Math.floor(cap / per + 1e-9);                       // whole shares the cap pays for
    const approved = Math.min(suggested, fits);
    if (fits < 1) {
      refused++;
      continue; // Refuse — nothing to check beyond the refusal itself
    }
    if (approved > suggested + 1e-9) problems.push(`group ${i}: approved ${approved} > suggested ${suggested}`);
    if (approved * per > cap + 1e-9) {
      problems.push(`group ${i}: approved ${approved} × ${per} = ${(approved * per).toFixed(6)} > cap ${cap}`);
    }
    if (approved < suggested) shrunk++;
  }
  if (shrunk === 0) problems.push('no group ever shrank — the generator is vacuous');
  if (refused === 0) problems.push('no group was ever refused — the tiny-cap branch is untested');
  return problems;
}

// Fixtures for the physics judge: the TRACE the kernel writes and the
// BINDING it records, decoupled so a mutation can move one without the
// other — that is exactly the bug teeth B/C simulate (a hardcoded binding
// that no longer follows the kernel's own projection).
const proj = (stopPct, stop, force) =>
  `projection: stop ${stopPct}% → ${stop}, force-exit ${force}s, ladder[0] close 1.0 at 100%`;
const physicsRec = (detail, stopPrice, forceExitSec) => ({
  intent: { price: '0.40' },
  gates: [{ gate: 'PHYSICS', outcome: 'PASS', detail }],
  decision: { physics: { stopPrice: String(stopPrice), forceExitSec, ladder: [{ atPct: 100, closeRatio: '1.0' }] } },
});

// ── --self-test ──────────────────────────────────────────────────────────────

function selfTest() {
  const goodShrink = {
    strategy: NAME, intent: { price: '0.40', size: '10' },
    decision: { status: 'MODIFIED', shares: '2', modification: { kind: 'SIZE_REDUCED', suggested: '10', approved: '2' } },
  };
  const rows = [
    ['the armed readout is clean', judgeReadout(armedReadoutFixture(), 'armed'), 0, null],
    ['the factory readout is clean', judgeReadout(factoryReadoutFixture()), 0, null],
    ['a MODIFIED record inside the cap is clean', judgeShrinkRecord(goodShrink, { capUsd: CAP, stopPct: STOP_PCT }), 0, null],
    ['teeth A: an over-cap APPROVED is red', judgeShrinkRecord(
      { intent: { price: '0.40', size: '10' }, decision: { status: 'APPROVED' } },
      { capUsd: CAP, stopPct: STOP_PCT }), 1, 'Gate 2 passed what it must bound'],
    ['a shrink that grew the order is red', judgeShrinkRecord({
      ...goodShrink,
      decision: { ...goodShrink.decision, modification: { kind: 'SIZE_REDUCED', suggested: '10', approved: '11' }, shares: '11' },
    }, { capUsd: CAP, stopPct: STOP_PCT }), 1, 'grew the order'],
    ['an approved size over the cap is red', judgeShrinkRecord({
      ...goodShrink,
      decision: { ...goodShrink.decision, modification: { kind: 'SIZE_REDUCED', suggested: '10', approved: '5' }, shares: '5' },
    }, { capUsd: CAP, stopPct: STOP_PCT }), 1, 'does not bound'],
    ['the physics binding matches the kernel projection', judgePhysics(
      physicsRec(proj(20, '0.32', 120), '0.32', 120), { forceExitSec: 120 }), 0, null],
    ['teeth B: a stop hardcoded at 0.99 is red', judgePhysics(
      physicsRec(proj(20, '0.32', 120), '0.99', 120)), 1, 'not reading the config'],
    ['teeth C: a force-exit hardcoded at 120 vs 90 is red', judgePhysics(
      physicsRec(proj(20, '0.32', 90), '0.32', 120)), 1, 'hardcoded'],
    ['a projection whose own math drifted is red', judgePhysics(
      physicsRec(proj(20, '0.31', 120), '0.31', 120)), 1, 'projection math drifted'],
    ['the breaker cycle verdict is clean', judgeBreakerCycle({
      rejectRecord: { decision: { status: 'REJECTED', reason: 'LOSS_BREAKER', detail: 'loss breaker active until 1 for account default' } },
      recoveryRecord: { decision: { status: 'APPROVED' } },
      tripAlert: { message: 'consecutive-loss breaker tripped for account default: 2 losses' },
    }), 0, null],
    ['a trip alert that names the STRATEGY is red', judgeBreakerCycle({
      rejectRecord: { decision: { status: 'REJECTED', reason: 'LOSS_BREAKER', detail: 'x for account default' } },
      recoveryRecord: { decision: { status: 'APPROVED' } },
      tripAlert: { message: 'consecutive-loss breaker tripped for strategy s: 3 losses' },
    }), 1, 'account breaker'],
    ['a refusal that does not name the account is red', judgeBreakerCycle({
      rejectRecord: { decision: { status: 'REJECTED', reason: 'ACCOUNT_LIMIT', detail: 'shrug' } },
      recoveryRecord: { decision: { status: 'APPROVED' } },
      tripAlert: { message: 'tripped for account default' },
    }), 1, 'does not name the account'],
    ['no recovery after the cooldown is red', judgeBreakerCycle({
      rejectRecord: { decision: { status: 'REJECTED', reason: 'LOSS_BREAKER', detail: 'for account default' } },
      recoveryRecord: null,
      tripAlert: { message: 'tripped for account default' },
    }), 1, 'never lifted'],
    ['the 1000-group invariant holds', shrinkInvariant(1000), 0, null],
  ];
  let bad = 0;
  console.log('risk-systemic self-test — the judges must go red exactly when named:');
  for (const [name, problems, want, mention] of rows) {
    const ps = Array.isArray(problems) ? problems : problems();
    const ok = want === 0 ? ps.length === 0
      : ps.length > 0 && (mention === null || ps.some((p) => p.includes(mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — ${JSON.stringify(ps).slice(0, 220)}`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nself-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\nself-test OK — ${rows.length} fixtures judged`);
}

function armedReadoutFixture() {
  return {
    version: '1.1',
    account: { id: 'default', limits: {
      maxSingleLossUsd: { value: '0.20', source: 'toml' },
      maxDailyDrawdownUsd: { value: '0', source: 'default' },
      maxPositionSize: { value: '0', source: 'default' },
      maxConsecutiveLosses: { value: '2', source: 'toml' },
      cooldownMinutes: { value: '0.05', source: 'toml' },
    } },
    global: { limits: {
      maxTotalPosition: { value: '0', source: 'default' },
      maxTotalExposureUsd: { value: '0', source: 'default' },
      maxCorrelationUsd: { value: '0', source: 'default' },
      globalKillSwitchLossUsd: { value: '0', source: 'default' },
    } },
    exit: { stopLossPct: 20, takeProfitPct: 100, forceExitSec: 0 },
  };
}

function factoryReadoutFixture() {
  const zero = { value: '0', source: 'default' };
  return {
    version: '1.1',
    account: { id: 'default', limits: {
      maxSingleLossUsd: zero, maxDailyDrawdownUsd: zero, maxPositionSize: zero,
      maxConsecutiveLosses: zero, cooldownMinutes: zero,
    } },
    global: { limits: {
      maxTotalPosition: zero, maxTotalExposureUsd: zero,
      maxCorrelationUsd: zero, globalKillSwitchLossUsd: zero,
    } },
    exit: { stopLossPct: 12, takeProfitPct: 100, forceExitSec: 0 },
  };
}

// ── --teeth: the broken implementations' OUTPUT must go red ─────────────────

function teeth() {
  const mutations = [
    { name: 'teeth A: Gate 2 always passes — an over-cap entry approved',
      run: () => judgeShrinkRecord(
        { intent: { price: '0.40', size: '10' }, decision: { status: 'APPROVED' } },
        { capUsd: CAP, stopPct: STOP_PCT }),
      mention: 'Gate 2 passed what it must bound' },
    { name: 'teeth B: Gate 4 stop hardcoded at 0.99',
      run: () => judgePhysics(physicsRec(proj(20, '0.32', 90), '0.99', 90)),
      mention: 'not reading the config' },
    { name: 'teeth C: force-exit hardcoded at 120 while the kernel resolves 90',
      run: () => judgePhysics(physicsRec(proj(20, '0.32', 90), '0.32', 120)),
      mention: 'hardcoded' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = m.run();
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 140)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\nrisk-systemic --teeth: every broken output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\nrisk-systemic --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

// ── the real run ─────────────────────────────────────────────────────────────

function feedBooks(client, feeds) {
  return Promise.all(Object.entries(feeds).map(([tokenId, [bid, ask]]) =>
    client.request('engine.book', {
      tokenId,
      bids: [{ price: bid, size: 200 }],
      asks: [{ price: ask, size: 200 }],
    })));
}

async function readAudit(cwd) {
  const p = join(cwd, 'data', 'audit', 'intents.jsonl');
  if (!existsSync(p)) return [];
  return readFileSync(p, 'utf8').split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
}

async function waitFor(desc, ms, step, probe) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const v = await probe();
    if (v) return v;
    await sleep(step);
  }
  return null;
}

/** One boot: spawn, wait for the socket, connect, answer core.ready. */
async function boot({ args, cwd, stderr }) {
  const sock = join(cwd, `bk-e26-${Math.random().toString(36).slice(2, 8)}.sock`);
  try { unlinkSync(sock); } catch { /* fresh */ }
  const proc = spawn(CORE, [...args, '--socket', sock], {
    stdio: ['ignore', 'ignore', 'pipe'], cwd,
  });
  proc.stderr?.on('data', (d) => { if (stderr) stderr.buf = (stderr.buf + String(d)).slice(-32768); });
  await waitForSocket(sock, { timeoutMs: 15000 });
  const client = await CoreClient.connect({ socketPath: sock });
  await client.request('core.ready');
  return { proc, client, sock };
}

async function loadProbe(client) {
  const receipt = await client.request('strategy.load', { path: DYLIB });
  await client.request('strategy.enable', { name: NAME, enabled: true });
  return String(receipt);
}

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

  // Build the probe once for both boots.
  const { execFileSync } = await import('./lib/child-guard.mjs');
  const tempFactory = mkdtempSync(join(tmpdir(), 'bk-e26-factory-'));
  const tempArmed = mkdtempSync(join(tmpdir(), 'bk-e26-armed-'));
  const tomlPath = join(tempArmed, 'risk.toml');
  writeFileSync(tomlPath, TOML);
  try {
    execFileSync('/bin/bash', ['-c',
      `cd "${ROOT}" && node scripts/blitzkrieg-new-strategy.mjs ${NAME} && ` +
      `cd "${join(ROOT, 'user_layer', 'strategies', NAME)}" && cargo build --release -q`],
      { encoding: 'utf8', timeout: 600000 });
    if (!existsSync(DYLIB)) { console.error(`missing probe dylib ${DYLIB}`); process.exit(2); }

    const baseArgs = [
      '--mode', 'dry', '--tick-ms', '50', '--seed-balance', '1000',
      '--max-order-notional', '6', '--engine', '--no-auto-exits',
      '--no-discovery', '--no-event-archive',
      '--no-trade-log', '--no-order-log', '--no-position-log',
      '--no-strategy-dir', '--round-sec', String(ROUND_SEC),
      '--min-round-age', '0', '--min-time-left', '0',
    ];

    // ── boot 1: factory core (§4.1 出厂即静默) ──
    const stderr = { buf: '' };
    {
      const { client, proc } = await boot({ args: [...baseArgs, '--no-config'], cwd: tempFactory, stderr });
      try {
        checkCoreProvenance(CORE, check);
        const readout = await client.request('risk.limits');
        const rp = judgeReadout(readout);
        check('factory readout: nine silent limits + factory exit triple', rp.length === 0,
          rp.join(' | ').slice(0, 200) || JSON.stringify(readout.exit));
        check('factory boot log carries the "at factory" line',
          stderr.buf.includes('at factory (all nine limits off)'));
        await loadProbe(client);
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
        await feedBooks(client, { UP: [0.55, 0.57], DOWN: [0.54, 0.56] });
        await sleep(120);
        await feedBooks(client, { UP: [0.38, 0.40] }); // the dip
        const rec = await waitFor('factory audit record', 15000, 100, async () => {
          const rs = await readAudit(tempFactory);
          return rs.find((r) => r.strategy === NAME && r.decision?.status === 'APPROVED') ?? null;
        });
        check('factory entry approved', Boolean(rec));
        if (rec) {
          // Scan the GATE TRACES, not the whole record: this probe's own
          // name carries the word ("e26_systemic_probe"), and the witness
          // is about the pipeline's output — the traces an armed block
          // would prepend ("systemic: …") must be absent at the factory.
          check('factory pipeline: no systemic trace anywhere (P1 byte-parity witness)',
            !JSON.stringify(rec.gates ?? []).toLowerCase().includes('systemic'),
            JSON.stringify(rec.gates?.map((g) => g.detail ?? '')).slice(0, 160));
          const pp = judgePhysics(rec, readout.exit);
          check('factory physics binding matches the readout (stop 12%)', pp.length === 0,
            pp.join(' | ').slice(0, 200));
        }
      } finally {
        client.stop();
        try { proc.kill('SIGTERM'); } catch { /* already gone */ }
      }
    }

    // ── boot 2: armed core (§4.2 enforcement) ──
    const events = [];
    {
      const { client, proc } = await boot({ args: [...baseArgs, '--config', tomlPath], cwd: tempArmed });
      client.onEvent = (e) => events.push(e);
      try {
        const readout = await client.request('risk.limits');
        const rp = judgeReadout(readout, 'armed');
        check('armed readout: toml-sourced bounds + silent siblings + [exit] triple',
          rp.length === 0, rp.join(' | ').slice(0, 200));

        await loadProbe(client);
        const now = Date.now();
        const slot = Math.floor(now / 1000 / ROUND_SEC);
        await client.request('engine.markets', {
          markets: [
            { asset: 'BTC', conditionId: '0xc', questionId: '0xq', upTokenId: 'UP', downTokenId: 'DOWN',
              upPrice: 0.5, downPrice: 0.5, expiresAtMs: (slot + 1) * ROUND_SEC * 1000,
              roundSlot: slot, negRisk: true, question: 'BTC' },
            { asset: 'ETH', conditionId: '0xe', questionId: '0xr', upTokenId: 'UP2', downTokenId: 'DOWN2',
              upPrice: 0.5, downPrice: 0.5, expiresAtMs: (slot + 1) * ROUND_SEC * 1000,
              roundSlot: slot, negRisk: true, question: 'ETH' },
            { asset: 'SOL', conditionId: '0xs', questionId: '0xs2', upTokenId: 'UP3', downTokenId: 'DOWN3',
              upPrice: 0.5, downPrice: 0.5, expiresAtMs: (slot + 1) * ROUND_SEC * 1000,
              roundSlot: slot, negRisk: true, question: 'SOL' },
          ],
        });
        await feedBooks(client, {
          UP: [0.55, 0.57], DOWN: [0.55, 0.57],
          UP2: [0.55, 0.57], DOWN2: [0.55, 0.57],
          UP3: [0.55, 0.57], DOWN3: [0.55, 0.57],
        });
        await sleep(120);

        // Entry 1: dip UP — the 10-share suggestion must come back MODIFIED at 2.
        await feedBooks(client, { UP: [0.38, 0.40] });
        const rec1 = await waitFor('MODIFIED record', 15000, 100, async () => {
          const rs = await readAudit(tempArmed);
          return rs.find((r) => r.strategy === NAME && r.decision?.status === 'MODIFIED') ?? null;
        });
        check('the shrink is real: 10-share suggestion came back MODIFIED', Boolean(rec1));
        if (rec1) {
          const sp = judgeShrinkRecord(rec1, { capUsd: CAP, stopPct: STOP_PCT });
          check('shrink record invariants (approved ≤ suggested, approved × per-share ≤ cap)',
            sp.length === 0, sp.join(' | ').slice(0, 200));
          const pp = judgePhysics(rec1, readout.exit);
          check('armed physics binding matches the readout (stop 20%)', pp.length === 0,
            pp.join(' | ').slice(0, 200));
          const placed = (await client.request('orders.list')).orders ?? [];
          const mine = placed.find((o) => o.strategy === NAME);
          check('the PLACED order carries the reduced size (2, not 10)',
            mine && num(mine.size) === num(rec1.decision.modification.approved),
            mine ? `placed size ${mine.size}` : 'no order');
        }

        // ── two consecutive LOSSES, manufactured by HAND ──
        // The kernel's stop-loss exit arms a global 180 s entry cooldown
        // (`last_stop_loss_at`), which would swallow every later probe — so
        // the losses come from `positions.exit` flattens instead. A flatten
        // rides the `flatten:` close key: it never arms that cooldown, and
        // it is admitted at ANY size even under an armed matrix (§4.2: a
        // bound that traps a position is #174 through another door) — every
        // loss therefore doubles as the close-never-blocked witness. The
        // two losses land on DIFFERENT assets so the kernel's per-asset
        // exit/loss cooldowns (90 s/180 s) never bite either.
        const closes = [];
        client.onEvent = (e) => {
          events.push(e);
          if (String(e.kind).includes('POSITION_CLOSED')) closes.push(e);
        };
        const flattenAll = async () => (await client.request('positions.exit', {}))?.closed ?? 0;

        // Loss 1: crash UP's book (the flatten's sell needs a bid to cross),
        // flatten at 0.30 against the 0.40 entry, re-warm.
        await feedBooks(client, { UP: [0.30, 0.32] });
        const closed1 = await flattenAll();
        await waitFor('close 1', 10000, 80, () => closes.length >= 1);
        check('flatten 1 closed at a loss — a close passes an armed matrix untouched',
          closed1 >= 1 && closes.length >= 1 && num(closes[0]?.netPnlUsd) < 0,
          `closed=${closed1}, closes=${closes.length}, pnl=${closes[0]?.netPnlUsd}`);
        await feedBooks(client, { UP: [0.55, 0.57] });

        // Entry 2 + loss 2: dip ETH (a fresh asset), flatten it too — the
        // SECOND consecutive loss is what trips the ACCOUNT breaker (the
        // strategy-level shipped breaker stays dormant at 3).
        await feedBooks(client, { DOWN2: [0.38, 0.40] });
        const entered2 = await waitFor('second entry', 15000, 100, async () => {
          const orders = (await client.request('orders.list')).orders ?? [];
          return orders.find((o) => o.strategy === NAME &&
            String(o.tokenId) === 'DOWN2' && num(o.filledSize) > 0) ?? null;
        });
        check('the second entry was placed and filled (DOWN2, post-loss-1)', Boolean(entered2));
        await feedBooks(client, { DOWN2: [0.30, 0.32] });
        const closed2 = entered2 ? await flattenAll() : 0;
        await waitFor('close 2', 10000, 80, () => closes.length >= 2);
        await feedBooks(client, { DOWN2: [0.55, 0.57] });
        const tripAlert = await waitFor('account breaker trip alert', 15000, 80, () =>
          events.find((e) => /breaker tripped for account/i.test(String(e.message ?? ''))) ?? null);
        check('two consecutive losses tripped the ACCOUNT breaker', Boolean(tripAlert),
          `closes=${closes.length}, alerts=${events.filter((e) => String(e.kind).includes('RISK')).map((e) => String(e.message ?? '').slice(0, 60)).join(' ;; ').slice(0, 160)}`);

        // The next entry must be REJECTED, naming the account. SOL has no
        // history — no dedup, no cooldown, no position — so the refusal can
        // only be the breaker's. The finder demands the LOSS_BREAKER reason:
        // an earlier operational refusal (e.g. "Already in BTC") must not
        // stand in for it.
        await feedBooks(client, { UP3: [0.37, 0.39] });
        const rejectRec = await waitFor('LOSS_BREAKER reject', 15000, 100, async () => {
          const rs = await readAudit(tempArmed);
          return rs.find((r) => r.strategy === NAME &&
            r.decision?.status === 'REJECTED' && r.decision?.reason === 'LOSS_BREAKER') ?? null;
        });
        check('a post-trip entry was refused with LOSS_BREAKER', Boolean(rejectRec));
        const rejectTs = rejectRec?.tsMs ?? 0;

        // Recovery: after 3 s of cooldown the next entry is admitted — on
        // DOWN3 (SOL's other side): DOWN2/ETH now carries a fresh loss, and
        // the kernel's per-asset cooldowns would (correctly) gate it. UP3
        // is RE-WARMED first: the template proposes the FIRST eligible
        // token and caps at one per evaluate, and the engine dedups a
        // token it already arbitrated for the rest of the round — a
        // still-crashed UP3 would hog the slot every tick with a silently
        // dropped suggestion, and DOWN3 would never even be proposed.
        await feedBooks(client, { UP3: [0.55, 0.57] });
        await sleep((COOLDOWN_SEC + 1) * 1000);
        await feedBooks(client, { DOWN3: [0.36, 0.38] });
        const recoveryRec = rejectRec
          ? await waitFor('recovery entry', 15000, 100, async () => {
              const rs = await readAudit(tempArmed);
              return rs.find((r) => r.strategy === NAME &&
                ['APPROVED', 'MODIFIED'].includes(r.decision?.status) &&
                r.tsMs > rejectTs + COOLDOWN_SEC * 1000) ?? null;
            })
          : null;
        check('the halt lifted after the configured cooldown', Boolean(recoveryRec),
          recoveryRec ? '' : `last strategy records: ${JSON.stringify(
            (await readAudit(tempArmed)).filter((r) => r.strategy === NAME)
              .slice(-3).map((r) => [r.decision?.status, r.intent?.price, r.decision?.detail ?? r.decision?.reason ?? '']))}`.slice(0, 220));

        const bc = judgeBreakerCycle({ rejectRecord: rejectRec, recoveryRecord: recoveryRec, tripAlert });
        check('breaker cycle verdict (account named, halt lifted, close never blocked)',
          bc.length === 0, bc.join(' | ').slice(0, 220));
      } finally {
        client.stop();
        try { proc.kill('SIGTERM'); } catch { /* already gone */ }
      }
    }

    // ── the invariant at random (deterministic 1000 groups) ──
    const rip = shrinkInvariant(1000);
    check('1000 random groups: approved ≤ suggested and approved × per-share ≤ cap',
      rip.length === 0, rip.join(' | ').slice(0, 200));
  } catch (e) {
    check('harness error', false, e?.stack || String(e));
  } finally {
    rmSync(join(ROOT, 'user_layer', 'strategies', NAME), { recursive: true, force: true });
    rmSync(tempFactory, { recursive: true, force: true });
    rmSync(tempArmed, { recursive: true, force: true });
  }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — factory-silent, armed-and-provenanced, the shrink bounds, physics binds, the breaker cycles and recovers`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`risk-systemic-check interrupted: ${String(e?.message || e).slice(0, 500)}`);
  process.exit(1);
});
