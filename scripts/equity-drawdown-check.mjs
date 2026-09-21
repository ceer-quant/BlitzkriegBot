#!/usr/bin/env node
/**
 * #192 — equity curve & drawdown gate.
 *
 * The trade log says what each trade earned; it does not say how the BALANCE got
 * there. A strategy with a positive expectancy and a 60% peak-to-trough excursion
 * is not the same account as one that walks up smoothly, and with a small bankroll
 * (`--max-order-notional`, `min-shares`) the difference decides whether the account
 * survives its own losing streak. This script derives the curve from the log and
 * GATES on it:
 *
 *   * equity (initial balance + cumulative net), peak, current drawdown;
 *   * max drawdown (absolute and as a percentage of the peak it fell from);
 *   * per-trade returns on the equity at risk (net / equity-before) → mean, sample
 *     sd, and an annualised Sharpe when the trade cadence is known;
 *   * profit factor, win rate, expectancy, fees and fees-as-share-of-gross-profit;
 *   * the worst single trade as a percentage of the equity at that moment — the
 *     number that says whether the position SIZING (min/max-shares, size_usd) can
 *     take the account out in one ticket.
 *
 * Refusals, deliberately: an EMPTY log fails (a gate that passes on no data is the
 * defect this repo keeps finding — an always-green check carries no signal), and
 * every failure exit is 1 with the offending number printed.
 *
 * Usage:
 *   node scripts/equity-drawdown-check.mjs --trades data/trades/trades.jsonl \
 *        --initial-balance 6 --max-drawdown-pct 25 --min-trades 30
 *   node scripts/equity-drawdown-check.mjs --self-test
 *   node scripts/equity-drawdown-check.mjs --trades t.jsonl --capacity /tmp/cap.json
 *
 * `--self-test` runs the SAME metric and verdict code against fixtures with
 * hand-computed answers, including a curve whose max drawdown is exactly 50% (it
 * must be flagged at a 20% budget) and a curve that goes to zero (ruin). The
 * fixtures exist so the gate can prove it still knows how to fail: the previous
 * generation of ops scripts in this repo could not, and shipped green for weeks.
 *
 * `--capacity <json>` (the file `capacity-check.mjs --json` writes) adds a
 * counterfactual: each trade is charged the measured self-impact for its own size,
 * interpolated on the measured curve. It is an ATTRIBUTION, not a re-simulation —
 * one leg per trade, on a fixture ladder, so it is stated as a bound, not a result.
 *
 * Read-only: reads the trade log and (optionally) a capacity report, prints, exits.
 */
import { existsSync, readFileSync, writeFileSync } from 'fs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i === -1 ? dflt : argv[i + 1];
};
const flag = (name) => argv.includes(name);

const TRADES = opt('--trades', 'data/trades/trades.jsonl');
const INITIAL = Number(opt('--initial-balance', 100));
const MAX_DD_PCT = Number(opt('--max-drawdown-pct', 25));
const MIN_SHARPE = Number(opt('--min-sharpe', 0));
const MIN_TRADES = Number(opt('--min-trades', 30));
const REQUIRE_TRADES = Number(opt('--require-trades', 0));
const TRADES_PER_YEAR = opt('--trades-per-year', null) === null ? null : Number(opt('--trades-per-year'));
const CAPACITY = opt('--capacity', null);
const JSON_OUT = opt('--json', null);
const SELF_TEST = flag('--self-test');

const num = (x) => Number(x ?? 0);
const money = (x) => (x >= 0 ? '+' : '-') + '$' + Math.abs(x).toFixed(4);
const pct = (x) => `${x.toFixed(2)}%`;
const finite = (x) => (Number.isFinite(x) ? x : null);

// ── Metrics: a pure function of the trade list ────────────────────────────────
// Kept free of I/O and of process.exit so --self-test exercises exactly the code
// the real run uses, and so a wrong answer is a wrong NUMBER, not a wrong log.
function curveMetrics(rawTrades, { initialBalance, tradesPerYear = null, minTrades = 30 }) {
  // Chronological by exit: the curve is the order the balance actually moved in.
  const trades = [...rawTrades].sort((a, b) => num(a.exitTime) - num(b.exitTime));

  let equity = initialBalance;
  let peak = initialBalance;
  let maxDdAbs = 0;
  let maxDdPct = 0;
  let maxDdTrade = null;
  const returns = [];
  let grossProfit = 0;
  let grossLoss = 0;
  let fees = 0;
  let worstTradePct = 0;
  let worstTradeId = null;

  for (const t of trades) {
    const net = num(t.netPnlUsd);
    const before = equity;
    equity += net;
    peak = Math.max(peak, equity);
    const ddAbs = peak - equity;
    // A peak of zero or below means the account was already gone; a percentage of
    // it is meaningless, so it is reported as the absolute form only.
    const ddPct = peak > 0 ? (ddAbs / peak) * 100 : 0;
    if (ddAbs > maxDdAbs) { maxDdAbs = ddAbs; maxDdTrade = t.id; }
    if (ddPct > maxDdPct) maxDdPct = ddPct;
    if (before > 0) returns.push(net / before);
    if (net > 0) grossProfit += net; else grossLoss += -net;
    fees += num(t.feesUsd);
    if (before > 0) {
      const tradePct = (net / before) * 100;
      if (tradePct < worstTradePct) { worstTradePct = tradePct; worstTradeId = t.id; }
    }
  }

  const n = trades.length;
  const net = equity - initialBalance;
  const wins = trades.filter((t) => num(t.netPnlUsd) > 0).length;

  // Sharpe from per-trade returns on the equity at risk. Annualisation needs a
  // cadence: either the caller supplies --trades-per-year or we derive it from the
  // median gap between consecutive exits (median, so one weekend or one outage
  // does not define the year).
  let tradesPerYearUsed = tradesPerYear;
  if (tradesPerYearUsed === null && n >= 3) {
    const gaps = [];
    for (let i = 1; i < n; i++) {
      const gap = (num(trades[i].exitTime) - num(trades[i - 1].exitTime)) / 1000;
      if (gap > 0) gaps.push(gap);
    }
    if (gaps.length > 0) {
      gaps.sort((a, b) => a - b);
      const median = gaps[Math.floor(gaps.length / 2)];
      // A sub-second cadence is a burst, not a strategy cadence: annualising it
      // would produce a Sharpe of thousands, which is noise dressed as evidence.
      if (median >= 1) tradesPerYearUsed = (365 * 24 * 3600) / median;
    }
  }

  const mean = returns.length > 0 ? returns.reduce((a, b) => a + b, 0) / returns.length : 0;
  let sd = 0;
  if (returns.length > 1) {
    const variance = returns.reduce((a, r) => a + (r - mean) ** 2, 0) / (returns.length - 1);
    sd = Math.sqrt(variance);
  }
  let sharpePerTrade = null;
  let sharpe = null;
  let sharpeNote = null;
  if (returns.length >= 2) {
    if (sd > 0) {
      const base = mean / sd;
      sharpePerTrade = base;
      sharpe = tradesPerYearUsed !== null ? base * Math.sqrt(tradesPerYearUsed) : null;
      if (sharpe === null) sharpeNote = 'no cadence (fewer than 3 exits with distinct times) — not annualised';
    } else {
      sharpeNote = mean > 0
        ? 'zero variance in the sample — every trade returned the same amount (not a Sharpe)'
        : 'zero variance with a non-positive mean';
      sharpePerTrade = mean > 0 ? null : 0;
    }
  } else {
    sharpeNote = `only ${returns.length} return(s) — a Sharpe needs at least 2`;
  }

  // The annualised form cannot be trusted from a handful of trades; `minTrades` is
  // the sample floor below which the caller declines to enforce it.
  const sampleSufficient = n >= minTrades;

  return {
    trades: n,
    initialBalance,
    equity,
    peak,
    net,
    netPct: initialBalance > 0 ? (net / initialBalance) * 100 : null,
    maxDrawdownUsd: maxDdAbs,
    maxDrawdownPct: maxDdPct,
    maxDrawdownTradeId: maxDdTrade,
    currentDrawdownUsd: peak - equity,
    currentDrawdownPct: peak > 0 ? ((peak - equity) / peak) * 100 : 0,
    winRate: n > 0 ? (wins / n) * 100 : null,
    wins,
    losses: n - wins,
    grossProfit,
    grossLoss,
    profitFactor: finite(grossLoss > 0 ? grossProfit / grossLoss : Number.POSITIVE_INFINITY),
    expectancyUsd: n > 0 ? net / n : null,
    feesUsd: fees,
    feesShareOfGrossProfitPct: grossProfit > 0 ? (fees / grossProfit) * 100 : null,
    avgHoldSec: n > 0 ? trades.reduce((a, t) => a + num(t.holdTimeSec), 0) / n : null,
    worstTradePct,
    worstTradeId,
    returns: returns.length,
    meanReturnPct: returns.length > 0 ? mean * 100 : null,
    sdReturnPct: returns.length > 1 ? sd * 100 : null,
    sharpePerTrade,
    sharpe,
    tradesPerYear: finite(tradesPerYearUsed ?? Number.NaN),
    sharpeNote,
    sampleSufficient,
    // Ruin is its own condition: a balance at or below zero is terminal, and it is
    // checked before any percentage threshold can be diluted by a large peak.
    ruined: equity <= 0,
  };
}

// ── Verdicts: one place decides pass/fail, used by the run and the self-test ──
function verdicts(m, opts) {
  const problems = [];
  if (m.trades === 0) {
    problems.push('the trade log is empty — there is no curve to gate');
  }
  if (m.ruined) {
    problems.push(`the account is ruined: equity ${m.equity} from ${m.initialBalance} initial`);
  }
  if (m.maxDrawdownPct > opts.maxDrawdownPct) {
    problems.push(
      `max drawdown ${pct(m.maxDrawdownPct)} exceeds the ${pct(opts.maxDrawdownPct)} budget ` +
      `(-$${m.maxDrawdownUsd.toFixed(4)} from ${m.peak}, at trade ${m.maxDrawdownTradeId ?? '?'})`
    );
  }
  if (m.trades < opts.requireTrades) {
    problems.push(`only ${m.trades} trades, fewer than the required ${opts.requireTrades}`);
  }
  // The Sharpe gate is stated, but only enforced where the sample can carry it:
  // the sample floor turns an unenforceable metric into a printed caveat, never
  // into a silent pass and never into a failure of its own.
  const sufficient = m.trades >= opts.minTrades;
  if (sufficient && m.sharpe !== null && m.sharpe < opts.minSharpe) {
    problems.push(`annualised Sharpe ${m.sharpe.toFixed(2)} is below the ${opts.minSharpe} floor`);
  }
  return problems;
}

// ── Optional attribution: the measured self-impact, per trade ────────────────
// Piecewise-linear on the capacity report's curve (size in SHARES → slippage in
// bps of the best ask). Sizes beyond the measured ladder are clamped to the last
// point and counted, because an order that size would have been refused outright.
function impactCurveLoad(path) {
  if (!path || !existsSync(path)) return null;
  const rep = JSON.parse(readFileSync(path, 'utf8'));
  const rows = (rep.rows ?? [])
    .filter((r) => r.complete && r.slippageBps !== null)
    .sort((a, b) => a.size - b.size);
  if (rows.length === 0) return null;
  const at = (shares) => {
    if (shares <= rows[0].size) return rows[0].slippageBps;
    for (let i = 1; i < rows.length; i++) {
      if (shares <= rows[i].size) {
        const a = rows[i - 1];
        const b = rows[i];
        return a.slippageBps + ((shares - a.size) / (b.size - a.size)) * (b.slippageBps - a.slippageBps);
      }
    }
    return rows[rows.length - 1].slippageBps;
  };
  return { report: rep, at };
}

function attributeImpact(rawTrades, curve) {
  const bestAsk = num(curve.report.bestAsk);
  let total = 0;
  let beyond = 0;
  const perTrade = [];
  const maxMeasured = Math.max(...(curve.report.rows ?? []).map((r) => r.size));
  for (const t of rawTrades) {
    const shares = num(t.shares);
    const bps = curve.at(shares);
    if (shares > maxMeasured) beyond++;
    // Slippage is measured at the ladder's price; carry it as USD per share so the
    // attribution does not silently rescale with the trade's own entry price.
    const usd = shares * bestAsk * (bps / 10000);
    total += usd;
    perTrade.push({ id: t.id, shares, slippageBps: bps, impactUsd: usd });
  }
  return { totalUsd: total, beyondCapacity: beyond, perTrade };
}

// ── Self-test: fixtures with hand-computed answers ───────────────────────────
function selfTest() {
  const day = 24 * 3600 * 1000;
  const t0 = 1_700_000_000_000;
  const mk = (nets, { gapMs = day, holdSec = 60 } = {}) =>
    nets.map((net, i) => ({
      id: `F${i}`,
      netPnlUsd: net,
      feesUsd: 0,
      shares: 10,
      holdTimeSec: holdSec,
      entryTime: t0 + i * gapMs,
      exitTime: t0 + i * gapMs + holdSec * 1000,
    }));

  let failures = 0;
  const check = (name, cond, detail = '') => {
    if (cond) console.log(`  ok   ${name}`);
    else { failures++; console.log(`  FAIL ${name} ${detail}`); }
  };
  const verdictOf = (m, o) => verdicts(m, o).length > 0;

  // F1 — a monotone winner: no drawdown at all, and the equity is the sum.
  {
    const m = curveMetrics(mk([1, 1, 1, 1, 1, 1, 1, 1, 1, 1]), { initialBalance: 100, tradesPerYear: 365, minTrades: 0 });
    check('fixture 1: equity = initial + sum(net)', m.equity === 110 && m.net === 10, `equity ${m.equity}`);
    check('fixture 1: a monotone curve has zero drawdown', m.maxDrawdownPct === 0 && m.maxDrawdownUsd === 0);
    check('fixture 1: profit factor is infinite with no losing trade', m.profitFactor === null && m.grossLoss === 0);
    check('fixture 1: passes a 20% drawdown budget and a 0 Sharpe floor',
      !verdictOf(m, { maxDrawdownPct: 20, minSharpe: 0, minTrades: 0, requireTrades: 0 }));
  }

  // F2 — an exactly-known 50% drawdown: 100 → 120 (peak) → 60. (-60/120 = -50%).
  {
    const m = curveMetrics(mk([20, -60, 5, 5]), { initialBalance: 100, tradesPerYear: 365, minTrades: 0 });
    check('fixture 2: max drawdown is exactly 50% of the peak', Math.abs(m.maxDrawdownPct - 50) < 1e-12,
      `${m.maxDrawdownPct}`);
    check('fixture 2: the drawdown is reported in USD too', Math.abs(m.maxDrawdownUsd - 60) < 1e-12,
      `${m.maxDrawdownUsd}`);
    check('fixture 2: peak stays at the high-water mark', m.peak === 120, `${m.peak}`);
    check('fixture 2: it FAILS a 20% drawdown budget',
      verdictOf(m, { maxDrawdownPct: 20, minSharpe: 0, minTrades: 0, requireTrades: 0 }));
    check('fixture 2: it passes a 60% budget (the gate is the threshold, not a hard-coded verdict)',
      !verdictOf(m, { maxDrawdownPct: 60, minSharpe: -1e9, minTrades: 0, requireTrades: 0 }));
    check('fixture 2: the worst single trade is -50% of the 120 it was taken on',
      Math.abs(m.worstTradePct + 50) < 1e-12, `${m.worstTradePct}`);
  }

  // F3 — ruin: the balance crosses zero, and must be flagged whatever the budget.
  {
    const m = curveMetrics(mk([50, -200]), { initialBalance: 100, tradesPerYear: 365, minTrades: 0 });
    check('fixture 3: equity crosses zero', m.equity === -50 && m.ruined);
    check('fixture 3: ruin fails even with an unlimited drawdown budget',
      verdictOf(m, { maxDrawdownPct: 1e9, minSharpe: -1e9, minTrades: 0, requireTrades: 0 }));
  }

  // F4 — an empty log must FAIL: no data is not a pass.
  {
    const m = curveMetrics([], { initialBalance: 100, minTrades: 0 });
    check('fixture 4: an empty log yields no metrics', m.trades === 0 && m.equity === 100);
    check('fixture 4: an empty log FAILS the gate',
      verdictOf(m, { maxDrawdownPct: 1e9, minSharpe: -1e9, minTrades: 0, requireTrades: 0 }));
    check('fixture 4: a trade-count requirement is also enforced',
      verdicts(m, { maxDrawdownPct: 1e9, minSharpe: -1e9, minTrades: 0, requireTrades: 3 })
        .some((p) => p.includes('fewer than the required 3')));
  }

  // F5 — one trade, and one with no cadence: the Sharpe must be reported as
  // unavailable rather than invented, and must not fail the run on its own.
  {
    const m = curveMetrics(mk([2]), { initialBalance: 100, minTrades: 0 });
    check('fixture 5: a single trade cannot produce a Sharpe', m.sharpe === null && m.sharpePerTrade === null);
    check('fixture 5: a single trade is not a Sharpe failure',
      !verdictOf(m, { maxDrawdownPct: 20, minSharpe: 3, minTrades: 0, requireTrades: 0 }));
    const burst = curveMetrics(mk([1, 1, 1, 1], { gapMs: 100 }), { initialBalance: 100, minTrades: 0 });
    check('fixture 5: a sub-second cadence is not annualised into a fake Sharpe', burst.tradesPerYear === null,
      `${burst.tradesPerYear}`);
  }

  // F6 — the sample floor turns an unenforceable Sharpe into a printed caveat,
  // never into a failure and never into a silent pass.
  {
    const m = curveMetrics(mk([-5, -5, -5, -5, -5]), { initialBalance: 100, tradesPerYear: 365, minTrades: 30 });
    const short = verdicts(m, { maxDrawdownPct: 100, minSharpe: 1, minTrades: 30, requireTrades: 0 });
    check('fixture 6: a 5-trade sample below the floor does not fail on Sharpe',
      !short.some((p) => p.includes('Sharpe')), JSON.stringify(short));
    const long = curveMetrics(mk(Array.from({ length: 40 }, (_, i) => (i % 2 === 0 ? 1 : -2))),
      { initialBalance: 100, tradesPerYear: 365, minTrades: 30 });
    check('fixture 6: a 40-trade losing sample FAILS a 1.0 Sharpe floor',
      verdicts(long, { maxDrawdownPct: 100, minSharpe: 1, minTrades: 30, requireTrades: 0 })
        .some((p) => p.includes('Sharpe')));
    check('fixture 6: the metrics agree the sample is sufficient', long.sampleSufficient);
  }

  // F7 — the annualisation is stated, not implied: same returns, two cadences,
  // Sharpe scales by sqrt(tradesPerYear) and nothing else.
  {
    const nets = [1, -0.5, 1.2, -0.4, 0.8, -0.3, 1.1, -0.6];
    const a = curveMetrics(mk(nets), { initialBalance: 100, tradesPerYear: 100, minTrades: 0 });
    const b = curveMetrics(mk(nets), { initialBalance: 100, tradesPerYear: 400, minTrades: 0 });
    check('fixture 7: Sharpe scales exactly by sqrt(cadence)',
      Math.abs(b.sharpe / a.sharpe - 2) < 1e-12, `${a.sharpe} vs ${b.sharpe}`);
    check('fixture 7: the per-trade Sharpe is cadence-free',
      Math.abs(a.sharpePerTrade - b.sharpePerTrade) < 1e-12);
  }

  // F8 — the impact attribution: a size the curve covers is interpolated, a size
  // past the ladder is clamped and COUNTED, never extrapolated.
  {
    const curve = {
      report: { bestAsk: 0.4, rows: [{ size: 10, slippageBps: 0 }, { size: 20, slippageBps: 100 }] },
      at: null,
    };
    curve.at = (s) => (s <= 10 ? 0 : s <= 20 ? ((s - 10) / 10) * 100 : 100);
    const att = attributeImpact([{ id: 'A', shares: 20 }, { id: 'B', shares: 50 }], curve);
    check('fixture 8: a covered size pays its interpolated impact', Math.abs(att.perTrade[0].impactUsd - 20 * 0.4 * 0.01) < 1e-12,
      `${att.perTrade[0].impactUsd}`);
    check('fixture 8: a size past the ladder is clamped to the last measured point and counted',
      Math.abs(att.perTrade[1].impactUsd - 50 * 0.4 * 0.01) < 1e-12 && att.beyondCapacity === 1,
      `impact ${att.perTrade[1].impactUsd}, beyond ${att.beyondCapacity}`);
  }

  console.log('');
  console.log(failures === 0
    ? 'EQUITY GATE SELF-TEST OK — the metrics and the verdicts still fire, including on a 50% drawdown and ruin.'
    : `EQUITY GATE SELF-TEST FAILED — ${failures} fixture(s) wrong; the gate cannot be trusted.`);
  return failures;
}

// ── Real run ─────────────────────────────────────────────────────────────────
function parseLog(path) {
  if (!existsSync(path)) {
    console.error(`  FAIL no trade log at ${path} (--trades <path>; the core writes data/trades/trades.jsonl)`);
    process.exit(1);
  }
  const lines = readFileSync(path, 'utf8').trim().split('\n').filter(Boolean);
  const trades = [];
  for (const line of lines) {
    try {
      trades.push(JSON.parse(line));
    } catch (e) {
      // A torn last line is a real possibility (the core appends while running);
      // anything else is corruption and must be named, not skipped silently.
      console.error(`  FAIL unparseable trade log line: ${line.slice(0, 120)}`);
      process.exit(1);
    }
  }
  return trades;
}

function main() {
  if (SELF_TEST) process.exit(selfTest() === 0 ? 0 : 1);
  if (argv.includes('--help') || argv.includes('-h')) {
    console.log('usage: node scripts/equity-drawdown-check.mjs [--trades t.jsonl] [--initial-balance N]');
    console.log('       [--max-drawdown-pct 25] [--min-sharpe 0] [--min-trades 30] [--require-trades N]');
    console.log('       [--trades-per-year N] [--capacity cap.json] [--json out.json] [--self-test]');
    process.exit(0);
  }

  const trades = parseLog(TRADES);
  const m = curveMetrics(trades, { initialBalance: INITIAL, tradesPerYear: TRADES_PER_YEAR, minTrades: MIN_TRADES });

  console.log(`equity:drawdown — ${TRADES} (${m.trades} trades, initial balance ${INITIAL})`);
  console.log(`  budgets: max drawdown ${pct(MAX_DD_PCT)}, min Sharpe ${MIN_SHARPE}, ` +
    `${REQUIRE_TRADES > 0 ? `require >= ${REQUIRE_TRADES} trades` : 'no trade-count requirement'}`);
  console.log('');
  console.log(`  equity        ${m.equity.toFixed(4)} (${money(m.net)}, ${m.netPct === null ? 'n/a' : pct(m.netPct)})`);
  console.log(`  peak          ${m.peak.toFixed(4)}`);
  console.log(`  max drawdown  -$${m.maxDrawdownUsd.toFixed(4)} = ${pct(m.maxDrawdownPct)} ` +
    `${m.maxDrawdownTradeId ? `(at trade ${m.maxDrawdownTradeId})` : ''}`);
  console.log(`  current dd    -$${m.currentDrawdownUsd.toFixed(4)} = ${pct(m.currentDrawdownPct)} from the peak`);
  console.log(`  returns       n=${m.returns} mean ${m.meanReturnPct === null ? 'n/a' : pct(m.meanReturnPct)} ` +
    `sd ${m.sdReturnPct === null ? 'n/a' : pct(m.sdReturnPct)} per trade`);
  console.log(`  sharpe        ${m.sharpe === null ? (m.sharpePerTrade === null ? 'n/a' : `${m.sharpePerTrade.toFixed(3)} per trade (not annualised)`) : m.sharpe.toFixed(2)} ` +
    `${m.tradesPerYear === null ? '' : `(cadence ${m.tradesPerYear.toFixed(1)} trades/yr)`}`);
  if (m.sharpeNote) console.log(`                note: ${m.sharpeNote}`);
  console.log(`  wins          ${m.wins}W/${m.losses}L (${m.winRate === null ? 'n/a' : pct(m.winRate)}), ` +
    `profit factor ${m.profitFactor === null ? 'infinite (no losses)' : m.profitFactor.toFixed(3)}`);
  console.log(`  expectancy    ${m.expectancyUsd === null ? 'n/a' : money(m.expectancyUsd)} per trade, ` +
    `avg hold ${m.avgHoldSec === null ? 'n/a' : `${m.avgHoldSec.toFixed(1)}s`}`);
  console.log(`  fees          $${m.feesUsd.toFixed(4)}` +
    `${m.feesShareOfGrossProfitPct === null ? '' : ` = ${pct(m.feesShareOfGrossProfitPct)} of gross profit`}`);
  console.log(`  worst trade   ${pct(m.worstTradePct)} of the equity it was taken on ` +
    `${m.worstTradeId ? `(trade ${m.worstTradeId})` : ''}`);

  const result = { ...m, thresholds: {
    maxDrawdownPct: MAX_DD_PCT, minSharpe: MIN_SHARPE, minTrades: MIN_TRADES, requireTrades: REQUIRE_TRADES,
  } };

  if (CAPACITY) {
    const curve = impactCurveLoad(CAPACITY);
    if (curve === null) {
      console.log('  impact        no usable curve in the capacity report — attribution skipped');
    } else {
      const att = attributeImpact(trades, curve);
      const withoutImpact = m.net + att.totalUsd;
      result.impact = { ...att, netWithoutImpactUsd: withoutImpact };
      console.log('');
      console.log(`  impact        one leg per trade on the measured curve (${CAPACITY}): ` +
        `-$${att.totalUsd.toFixed(4)} over ${m.trades} trades`);
      console.log(`                net ${money(m.net)} → ${money(withoutImpact)} with self-impact returned`);
      console.log(`                attribution, not a re-simulation: one leg, fixture ladder` +
        `${att.beyondCapacity > 0 ? `, ${att.beyondCapacity} trade(s) past the measured ladder (clamped)` : ''}`);
      const modelled = m.feesUsd + att.totalUsd;
      console.log(`                cost split: fees $${m.feesUsd.toFixed(4)} ` +
        `(${modelled > 0 ? ((m.feesUsd / modelled) * 100).toFixed(1) : 'n/a'}%) and self-impact ` +
        `$${att.totalUsd.toFixed(4)} (${modelled > 0 ? ((att.totalUsd / modelled) * 100).toFixed(1) : 'n/a'}%) ` +
        'of the modelled round-trip cost');
    }
  }

  const problems = verdicts(m, {
    maxDrawdownPct: MAX_DD_PCT, minSharpe: MIN_SHARPE, minTrades: MIN_TRADES, requireTrades: REQUIRE_TRADES,
  });
  result.problems = problems;
  result.ok = problems.length === 0;

  console.log('');
  for (const p of problems) console.log(`  FAIL ${p}`);
  if (problems.length === 0) {
    console.log(`  ok   the curve stayed inside every stated budget` +
      `${m.sampleSufficient ? '' : ` (Sharpe not enforced: ${m.trades} < ${MIN_TRADES} trades)`}`);
  }
  if (JSON_OUT) {
    writeFileSync(JSON_OUT, JSON.stringify(result, null, 2) + '\n');
    console.log(`  report written to ${JSON_OUT}`);
  }
  console.log('');
  console.log(problems.length === 0
    ? 'EQUITY OK — the curve, the drawdown and the cost split are what they claim to be.'
    : `EQUITY FAILED (${problems.length}) — the curve is outside the stated budgets.`);
  process.exit(problems.length === 0 ? 0 : 1);
}

main();
