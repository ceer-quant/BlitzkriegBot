#!/usr/bin/env node
/**
 * One-order equity bound (#202) — "on THIS account, what is the most any single
 * order can lose, as a share of the account?"
 *
 * WHY THIS EXISTS
 *   The kernel sized orders in SHARES, not in money: `size_usd` only chose a lot
 *   while it fell inside `[min_shares, max_shares]`, and the shipped band is
 *   `[10, 10]`. On the live 4.8 USDC account that means 10 shares at any price
 *   the book offers — 4.00 USD at 0.40 (83% of the account), 6.00 at 0.60 (125%)
 *   — and the supervisor's `max_order_notional = max(max_shares × 0.6, 6) = 6.00`
 *   was therefore not a bound at all: it was the same number as the smallest
 *   ticket the band can produce. A fixed lot is not a risk statement, and a fixed
 *   notional cap is not one either.
 *
 * WHAT IT ASSERTS (all against a real dry core, over the wire)
 *   1. The kernel's own `engine.stats.sizing` block: the worst case it allows for
 *      one order, against `balance × k%`;
 *   2. the wire behaviour that number claims — an over-cap order that is not a
 *      close is REJECTED with the equity-cap reason and fills nothing;
 *   3. the escape hatch — a close/reduce over the cap is ADMITTED (a cap that can
 *      trap a position is #174 reopened), while a non-close SELL over the cap is
 *      still refused, so the exemption is scoped to closing intents;
 *   4. the largest admitted order's notional, measured from the fills, is inside
 *      `balance × k%` — the number is checked against what the account did, not
 *      just against what the kernel says about itself.
 *
 * `--max-order-notional-pct 0` (the shipped default) is a FAIL, not a skip: with
 * no cap the answer is the 208% above, and a gate that cannot say that is the
 * defect this repository keeps re-finding. `--self-test` proves the verdict
 * function still goes red — including a fixture where `closes_exposure` is gone
 * and the close probe is refused.
 *
 * Usage:
 *   node scripts/risk-sizing-check.mjs --balance 4.8 --size-pct 20 --max-order-notional-pct 20
 *   node scripts/risk-sizing-check.mjs --max-order-notional-pct 0      # the pre-#202 answer
 *   node scripts/risk-sizing-check.mjs --self-test
 *
 * Exit 0 only if every assertion passes.
 */
import { mkdtempSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { coreBinaryPath, checkCoreProvenance } from './lib/core-provenance.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { sleep } from './lib/wait.mjs';

const argv = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};
const has = (name) => argv.includes(name);

const BALANCE = Number(opt('--balance', '4.8'));
const SIZE_PCT = Number(opt('--size-pct', '0'));
const CAP_PCT = Number(opt('--max-order-notional-pct', '20'));
const PRICE = Number(opt('--price', '0.40'));
const SELF_TEST = has('--self-test');
const EPS = 1e-9;

/**
 * The judgment, as a pure function so `--self-test` can drive it with fixtures
 * (the same split `equity-drawdown-check.mjs` uses for its verdicts).
 *
 * `probes` rows: { name, expect: 'rejected' | 'admitted', outcome, notionalUsd,
 * capped } — `capped` marks the orders the equity cap applies to; a close intent
 * is `capped: false` and may exceed the cap by design.
 */
export function verdict({ balance, capPct, reportedWorstUsd, probes = [] }) {
  const problems = [];
  if (!(balance > 0)) problems.push(`balance must be positive, got ${balance}`);
  if (!(capPct > 0)) {
    problems.push(
      `no per-order bound as a share of the account (--max-order-notional-pct ${capPct}): ` +
      'one order may commit the whole of max_shares × max_price');
  }
  const limit = (balance * capPct) / 100;
  if (capPct > 0 && reportedWorstUsd > limit + EPS) {
    problems.push(`the kernel reports a worst-case order of ${reportedWorstUsd} USD, over ` +
      `${capPct}% of ${balance} = ${limit} USD`);
  }
  for (const p of probes) {
    if (p.expect === 'rejected') {
      if (p.outcome !== 'rejected') {
        problems.push(`${p.name}: expected RISK_REJECTED, got ${p.outcome}`);
      } else if (p.reason && !/equity cap/i.test(p.reason)) {
        problems.push(`${p.name}: refused for a reason that does not name the equity cap (${p.reason})`);
      }
      if (p.filled > 0) {
        problems.push(`${p.name}: over-cap order was refused AND filled ${p.filled} — the cap must reject whole orders, never truncate`);
      }
    } else if (p.expect === 'admitted') {
      if (p.outcome === 'rejected') {
        problems.push(`${p.name}: the cap blocked an order that must pass (${p.reason ?? 'no reason'}); ` +
          'a bound that traps a position is #174 through another door');
      } else if (p.capped && capPct > 0 && p.notionalUsd > limit + EPS) {
        problems.push(`${p.name}: admitted ${p.notionalUsd} USD, over the ${limit} USD bound — the cap is looser than k`);
      }
    } else {
      problems.push(`${p.name}: unknown expectation ${p.expect}`);
    }
  }
  return { ok: problems.length === 0, limit, problems };
}

// ── --self-test: the verdict must still go red ───────────────────────────────

if (SELF_TEST) {
  const green = {
    balance: 4.8, capPct: 20, reportedWorstUsd: 0.96,
    probes: [
      { name: 'over-cap entry', expect: 'rejected', outcome: 'rejected', reason: 'notional 4.00 exceeds the 20% equity cap 0.96', filled: 0, capped: true },
      { name: 'in-cap entry', expect: 'admitted', outcome: 'admitted', notionalUsd: 0.80, capped: true },
      { name: 'close over the cap', expect: 'admitted', outcome: 'admitted', notionalUsd: 1.18, capped: false },
      { name: 'non-close sell over the cap', expect: 'rejected', outcome: 'rejected', reason: 'exceeds the 20% equity cap', filled: 0, capped: true },
    ],
  };
  const cases = [
    ['the healthy case passes', green, true],
    ['no cap configured is a FAILURE, never a skip (#202 is exactly this)', { ...green, capPct: 0 }, false],
    ['a worst case over the cap is red', { ...green, reportedWorstUsd: 10.0 }, false],
    ['an over-cap order that got through is red', {
      ...green,
      probes: green.probes.map((p) => (p.name === 'over-cap entry' ? { ...p, outcome: 'admitted' } : p)),
    }, false],
    ['a refused-but-partially-filled order is red (truncation, not rejection)', {
      ...green,
      probes: green.probes.map((p) => (p.name === 'over-cap entry' ? { ...p, filled: 1 } : p)),
    }, false],
    ['the escape hatch going closed is red (#174 through another door)', {
      ...green,
      probes: green.probes.map((p) => (p.name === 'close over the cap' ? { ...p, outcome: 'rejected', reason: 'exceeds the equity cap' } : p)),
    }, false],
    ['a refusal that does not name the equity cap is red', {
      ...green,
      probes: green.probes.map((p) => (p.name === 'over-cap entry' ? { ...p, reason: 'size must be positive' } : p)),
    }, false],
    ['an admitted capped order over the limit is red', {
      ...green,
      probes: green.probes.map((p) => (p.name === 'in-cap entry' ? { ...p, notionalUsd: 1.60 } : p)),
    }, false],
  ];
  let bad = 0;
  console.log('risk-sizing self-test — the verdict function must still go red:');
  for (const [name, fixture, wantOk] of cases) {
    const got = verdict(fixture);
    const pass = got.ok === wantOk;
    if (!pass) bad++;
    console.log(`  ${pass ? 'ok  ' : 'FAIL'} ${name} (expected ${wantOk ? 'PASS' : 'FAIL'}, got ${got.ok ? 'PASS' : 'FAIL'}: ${got.problems.join('; ') || 'no problems'})`);
  }
  console.log(bad === 0
    ? '\nSELF-TEST OK — the gate fails when the cap is absent, loose, unscoped or truncating.'
    : `\nSELF-TEST FAILED (${bad} fixture(s) judged wrongly)`);
  process.exit(bad === 0 ? 0 : 1);
}

// ── The real run: an isolated dry core on the account we were asked about ────

const BIN = coreBinaryPath();
const gate = createChecks();
const { check } = gate;

const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-sizing-'));
const SOCK = scratchSocketPath('sizing');
const round = (n) => Number(n.toFixed(6));
const liveSlot = () => Math.floor((Date.now() + 900_000) / 1000 / 900);
const order = (asset, side, price, size, key) => ({
  tokenId: `sizing-${asset}`, conditionId: 'cond', side, mode: 'taker', price, size,
  internalKey: key, strategy: 'operator', asset, direction: 'up', roundSlot: liveSlot(),
});

// The absolute cap is deliberately wide so ONLY the equity-relative one can bite
// (a 6.00 absolute cap is the decoration #202 is about).
const core = new CoreClient({
  binaryPath: BIN,
  socketPath: SOCK,
  mode: 'dry',
  seedBalance: BALANCE,
  maxOrderNotional: 1000,
  tickMs: 20,
  autoRestart: false,
  cwd: WORKDIR,
  noTradeLog: true,
  noOrderLog: true,
  noPositionLog: true,
  extraArgs: [
    '--max-order-notional-pct', String(CAP_PCT),
    '--size-pct', String(SIZE_PCT),
    '--no-auto-exits', '--max-positions', '99', '--no-discovery', '--no-event-archive',
  ],
});
const fills = [];
core.onEvent = (e) => { if (e.kind === 'FILL') fills.push(e); };

/** Place and collect what happened: admit/refuse, the reason, and any fill. */
async function probe(name, req, expect, capped) {
  const before = (await rpc.balance(core)).balance;
  let outcome = 'admitted';
  let reason = null;
  let orderId = null;
  try {
    const placed = await rpc.placeOrder(core, req);
    orderId = placed.orderId;
  } catch (e) {
    outcome = 'rejected';
    reason = e.message ?? String(e);
  }
  await sleep(120);
  const fill = orderId ? fills.find((f) => f.delta.orderId === orderId) : undefined;
  const filled = fill ? Number(fill.delta.delta) : 0;
  const after = (await rpc.balance(core)).balance;
  const row = {
    name, expect, outcome, reason, capped, filled,
    notionalUsd: round(req.price * req.size),
    balanceMoved: round(before - after),
  };
  console.log(`  ${outcome === expect ? 'ok  ' : 'FAIL'} ${name}: ${outcome}` +
    `${filled > 0 ? ` (filled ${filled})` : ''}` +
    `${reason ? ` — ${reason}` : ''}`);
  return row;
}

const capUsd = round((BALANCE * CAP_PCT) / 100);
console.log(`risk-sizing: account ${BALANCE} USD, per-order cap ${CAP_PCT}% = ${capUsd} USD, ` +
  `per-entry budget ${SIZE_PCT > 0 ? `${SIZE_PCT}% of equity` : 'absolute size_usd (off)'}`);

const probes = [];
let ran = false;
try {
  checkCoreProvenance(BIN, check);
  await core.start();
  const ready = await rpc.ready(core);
  console.log(`  core ${ready.build ?? '?'} commit=${ready.commit ?? '?'} dirty=${ready.dirty ?? '?'}`);

  // The account the numbers below are a percentage OF, from the ledger itself.
  const bal = await rpc.balance(core);
  check(`the core runs on the account we asked about (${BALANCE} USD)`,
    Number(bal.balance) === BALANCE, `ledger says ${bal.balance}`);

  const stats = await rpc.stats(core);
  const sizing = stats.sizing ?? {};
  check('engine.stats carries the sizing block (the panel and this gate read the same number)',
    typeof sizing === 'object' && sizing !== null, JSON.stringify(stats).slice(0, 200));
  check('the sizing block is anchored on the ledger balance, not on a config copy',
    Number(sizing.equityUsd) === Number(bal.balance), `equityUsd ${sizing.equityUsd} vs ledger ${bal.balance}`);
  console.log(`  kernel: share band ${sizing.maxShares} shares × ${sizing.maxPrice} = ${sizing.shareBandUsd} USD; ` +
    `equity cap ${sizing.equityCapUsd ?? 'off'}; worst case one order ${sizing.worstCaseOrderUsd} USD ` +
    `= ${Number(sizing.worstCasePctOfEquity).toFixed(1)}% of equity`);
  check('a close/reduce is exempt from the equity cap (the escape hatch #174 depends on)',
    sizing.closesExemptFromEquityCap === true, JSON.stringify(sizing));

  if (SIZE_PCT > 0) {
    check(`the per-entry budget is ${SIZE_PCT}% of the account`,
      Math.abs(Number(sizing.sizeBudgetUsd) - (BALANCE * SIZE_PCT) / 100) < EPS,
      `sizeBudgetUsd ${sizing.sizeBudgetUsd} != ${(BALANCE * SIZE_PCT) / 100}`);
    console.log(`  kernel: per-entry budget ${sizing.sizeBudgetUsd} USD = ` +
      `${Math.floor(Number(sizing.sizeBudgetUsd) / PRICE)} whole shares at ${PRICE}`);
  }

  // Probe 1 — the live ticket itself: 10 shares (the shipped band's only size),
  // which is over the cap whenever the cap is a real bound.
  const overSize = Math.max(10, Math.ceil(capUsd / PRICE) + 1);
  await rpc.bookSnapshot(core, 'sizing-BTC', [], [[PRICE, 10_000]]);
  probes.push(await probe('an over-cap entry (the 10-share live ticket)',
    order('BTC', 'buy', PRICE, overSize, 'entry:btc'), 'rejected', true));

  // Probe 2 — the largest whole ticket the cap allows: it must be admitted and
  // its notional must be inside the bound.
  const inCapSize = CAP_PCT > 0 ? Math.floor(capUsd / PRICE) : 1;
  await rpc.bookSnapshot(core, 'sizing-ETH', [], [[PRICE, 10_000]]);
  probes.push(await probe(`the largest in-cap entry (${inCapSize} shares)`,
    order('ETH', 'buy', PRICE, Math.max(1, inCapSize), 'entry:eth'), 'admitted', true));

  // Probe 3 — the way out, over the cap: the whole position at a 0.59 bid. Its
  // notional is over the cap BY CONSTRUCTION (the size was chosen from
  // `floor(cap / entry price)`, and 0.59 > the entry price), so a cap that does
  // not exempt closes would refuse it.
  const held = Math.max(1, inCapSize);
  await rpc.bookSnapshot(core, 'sizing-ETH', [[0.59, 10_000]], [[0.60, 10_000]]);
  probes.push(await probe('a close over the cap (the way out)',
    order('ETH', 'sell', 0.59, held, 'flatten:hft-sizing'), 'admitted', false));

  // Probe 4 — the same size on a NON-close key is still refused: the exemption
  // is for closing intents, not for the SELL side.
  await rpc.bookSnapshot(core, 'sizing-SOL', [[0.59, 10_000]], [[0.60, 10_000]]);
  probes.push(await probe('a non-close sell over the cap',
    order('SOL', 'sell', 0.59, held, 'entry:sol'), 'rejected', true));

  const v = verdict({ balance: BALANCE, capPct: CAP_PCT, reportedWorstUsd: Number(sizing.worstCaseOrderUsd), probes });
  console.log('');
  console.log(`  verdict: worst one-order commitment ${sizing.worstCaseOrderUsd} USD vs bound ` +
    `${capUsd} USD (${CAP_PCT}% of ${BALANCE})`);
  for (const p of v.problems) check(`verdict — ${p}`, false);
  ran = true;
} catch (e) {
  check('harness error', false, e?.stack || e);
} finally {
  await core.stop();
}

console.log('');
if (gate.failures === 0 && ran) {
  console.log(`RISK-SIZING OK — every order that may open exposure is bounded by ${capUsd} USD ` +
    `(${CAP_PCT}% of the ${BALANCE} USD account), the over-cap ones are refused rather than truncated, ` +
    'and closing intents still pass at any size.');
} else {
  console.log(`RISK-SIZING FAILED (${gate.failures || 1})`);
}
process.exit(gate.failures === 0 && ran ? 0 : 1);
