#!/usr/bin/env node
/**
 * #389 — the CALIBRATED A/B harness: backtest vs @almach actual, three axes
 * aligned on purpose, each deviation measured, nothing asserted away.
 *
 * The #355 overlay proved the raw A/B is structurally unfair (backtest seed
 * $100k / ~$2.5 per order vs the wallet deploying tens of thousands per day
 * -> pair-scope final-value deviation 99.8%). This harness measures the SAME
 * window at three alignment levels and reports what each alignment fixes and
 * what it structurally cannot.
 *
 * The three axes (issue #389's 「三同」), each measured, none asserted:
 *
 *   1. 同仓位规模 (same position sizing) — the live per-round notional/share
 *      distribution (quantiles from the ledger) against the replayed
 *      per-round invested distribution (from the report's tradeLines, the
 *      exact overlay invested basis |netPnlUsd / netPnlPct x 100|). The
 *      kernel's sizing face is global (share band + per-order cap), so
 *      alignment is at the DISTRIBUTION level (quantile table), never
 *      per-round — and the residual says so.
 *   2. 同动作空间 (same action space) — each arm declares its strategy set;
 *      the live side's pair/single-leg split is the reference. A pair-only
 *      arm leaves 55.1% of the wallet's 15m rounds structurally untouchable;
 *      a pair+single-leg arm (single_leg_pair, #394) closes that gap. The
 *      overlay's shared/actual-only counts stay the judge.
 *   3. 同费率现金流 (same fee cashflow) — the report's `cashflows` block
 *      (#392: the wallet-level MAKER_REBATE/REWARD rows the corpus carries
 *      and the kernel credited) against the ledger's own in-window rows, BY
 *      TYPE. The type table makes the structural gap visible: TAKER_REBATE
 *      rows exist live and are not corpus cashflow events, so the replay can
 *      never credit them.
 *
 * Deviation calibers (per arm), each with an explicit pass/fail at 5%:
 *   roiPair / roiWhole — realized/invested, both sides computed here with the
 *       same settled-round rule as the overlay (live realized = REDEEM+MERGE
 *       usdc − buy usdc, NO rebates: the cashflow axis is separate; backtest
 *       realized = Σ netPnlUsd, which DOES carry the official fee schedule —
 *       that fee asymmetry is a stated caliber difference, not a hidden one).
 *   finalValuePair / finalValueWhole — from the overlay (pairScope /
 *       wholeScopeFinalRelPct).
 *   cashflowMakerRebate / cashflowReward — |bt − live| / live on the corpus
 *       cashflow pipe; a pass here means the #388 pipe replays the wallet's
 *       own rows 1:1, NOT that the strategy earned them.
 *
 * Live-side ground rules are almach-ground-truth.mjs's, re-derived in the
 * single pass below (same slug/duration filters, same VWAP legs); when the
 * output is written next to a GT dump the pairCost/cashflow blocks can be
 * diffed mechanically (--gt <gt.json>). TRADE rows are summed as BUY cost for
 * every leg — the @almach wallet in this corpus is buy-and-settle (measured:
 * every one of the window's 153,653 TRADE rows is side=BUY), so no sell-side
 * cashflow exists to net out.
 *
 * Deterministic JSON on stdout.
 *
 * Usage:
 *   node scripts/almach-calib.mjs <activity.jsonl> <start> <end> <5m|15m|1h|4h> \
 *        <tag>=<backtest-report.json> [more tag=path ...] \
 *        [--overlay <tag>=<overlay-output.json>] [--gt <ground-truth.json>]
 *
 *   node scripts/almach-calib.mjs --help    # this text
 *
 * Example (the #389 window, one arm per action space):
 *   node scripts/almach-calib.mjs data/almach/activity-full.jsonl \
 *        2026-08-05 2026-09-30 15m \
 *        pair-only=reports/arm-a.json dual=reports/arm-b.json \
 *        --gt docs/reports/almach-benchmark/almach-gt-15m.json
 *
 * `--overlay` reuses a saved almach-overlay.mjs stdout for that arm instead of
 * spawning it (the ledger pass per arm is ~1 min on the 240MB export).
 */
function printHelp() {
  console.log(`almach-calib.mjs — #389 calibrated A/B harness (backtest vs @almach)

USAGE
  node scripts/almach-calib.mjs <activity.jsonl> <start> <end> <5m|15m|1h|4h>
      <tag>=<backtest-report.json> [more tag=path ...]
      [--overlay <tag>=<overlay-output.json>] [--gt <ground-truth.json>]
  node scripts/almach-calib.mjs --help

ARGUMENTS
  <activity.jsonl>        /activity ledger export (the wallet's full history)
  <start> <end>           UTC window, inclusive start, YYYY-MM-DD each
  <5m|15m|1h|4h>          round duration (slug-filtered, e.g. ...-15m-<epoch>)
  <tag>=<report.json>     one A/B arm per backtest report; repeatable. The tag
                          names the arm in the output (e.g. pair-only, dual).

OPTIONS
  --overlay <tag>=<path>  reuse a saved almach-overlay.mjs stdout for that arm
                          instead of spawning it (saves ~1 min/arm on the
                          240MB ledger)
  --gt <gt.json>          cross-check the live side against an
                          almach-ground-truth.mjs dump; all deltas must be 0
                          for the live side to count as the same ground truth

THREE AXES (issue #389)
  1 same sizing      live per-round notional/share quantiles vs the replayed
                     per-round invested distribution (|netPnlUsd/netPnlPct x
                     100| per close) — distribution-level only; the kernel's
                     sizing face is global (share band + per-order cap)
  2 same actions     each arm's enabled strategy set vs the live pair/single-
                     leg split; the overlay's shared/actual-only counts judge
  3 same cashflow    report cashflows (MAKER_REBATE/REWARD, the #388 wallet-
                     level pipe) vs the ledger's in-window rows BY TYPE;
                     TAKER_REBATE is a structural residual (live-only, the
                     corpus carries no such cashflow event)

OUTPUT
  Deterministic JSON on stdout: { live, liveCrossCheck?, arms[] } — every arm
  carries roiPair/roiWhole/finalValuePair/finalValueWhole/cashflow deviations,
  each with an explicit passAt5Pct, plus a verdict block. Deviations are
  never averaged away and never asserted out of existence: what fails, fails
  in print.
`);
}
import { readFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const PASS_PCT = 5;

const argv = process.argv.slice(2);
if (argv.includes('--help') || argv.includes('-h')) {
  printHelp();
  process.exit(0);
}
const positional = [];
const overlays = new Map();
let gtPath = null;
for (let i = 0; i < argv.length; i += 1) {
  const a = argv[i];
  if (a === '--overlay') { const v = argv[++i]; if (!v) die('--overlay needs tag=path'); const [t, p] = splitTag(v); overlays.set(t, p); continue; }
  if (a === '--gt') { gtPath = argv[++i]; if (!gtPath) die('--gt needs a ground-truth JSON path'); continue; }
  positional.push(a);
}
const [ledgerPath, startArg, endArg, durArg] = positional;
if (!ledgerPath || !startArg || !endArg || !durArg) {
  die('usage: almach-calib.mjs <activity.jsonl> <start> <end> <dur> <tag>=<report.json> [...] [--overlay tag=path] [--gt gt.json]');
}
const DUR_SEC = { '5m': 300, '15m': 900, '1h': 3600, '4h': 14_400 };
const durSec = DUR_SEC[durArg];
if (!durSec) die(`unknown duration ${durArg}`);
const startSec = Date.parse(`${startArg}T00:00:00Z`) / 1000;
const endSec = Date.parse(`${endArg}T00:00:00Z`) / 1000 + 86_400;

const arms = positional.slice(4).map((spec) => {
  const [tag, path] = splitTag(spec);
  return { tag, path };
});
if (arms.length === 0) die('at least one <tag>=<report.json> arm is required');

function die(msg) { console.error(`almach-calib: FATAL: ${msg}`); process.exit(2); }
function splitTag(spec) {
  const i = spec.indexOf('=');
  if (i <= 0) die(`arm spec must be tag=path, got "${spec}"`);
  return [spec.slice(0, i), spec.slice(i + 1)];
}

const isDur = (slug) => (slug ?? '').split('-').at(-2) === durArg;
const endOfSlug = (slug) => {
  const start = Number((slug ?? '').split('-').at(-1));
  return Number.isFinite(start) ? start + durSec : null;
};

// ── live side: ONE pass over the ledger ─────────────────────────────────────
// Same row grammar as almach-ground-truth.mjs (legs → VWAP pairCost, settle
// cashflow, duration-filtered) plus what GT never printed: the per-round
// sizing distribution the calibration aligns against.
const legs = new Map(); // cid\0token -> {usd, shares}
const conds = new Map(); // cid -> {slug, tokens:Set}
const buyUsd = new Map(); // cid -> usd paid (all rounds)
const settleUsd = new Map(); // cid -> usdc received (REDEEM + MERGE)
const settled = new Set();
const cashType = new Map(); // MAKER_REBATE/TAKER_REBATE/REWARD -> {usd, rows}
let fills = 0;

const lines = readFileSync(ledgerPath, 'utf8').split('\n');
for (const line of lines) {
  if (!line) continue;
  const r = JSON.parse(line);
  const ts = r.timestamp ?? 0;
  if (ts < startSec || ts >= endSec) continue;
  const t = r.type;
  if (t === 'MAKER_REBATE' || t === 'TAKER_REBATE' || t === 'REWARD') {
    // wallet-level rows: no conditionId, no slug — the window is the only filter
    const e = cashType.get(t) ?? { usd: 0, rows: 0 };
    e.usd += r.usdcSize ?? 0; e.rows += 1;
    cashType.set(t, e);
    continue;
  }
  const cid = r.conditionId;
  if (!cid) continue;
  if (t === 'REDEEM' || t === 'MERGE') {
    if (!isDur(r.slug)) continue;
    settled.add(cid);
    settleUsd.set(cid, (settleUsd.get(cid) ?? 0) + (r.usdcSize ?? 0));
    continue;
  }
  if (t !== 'TRADE' || !isDur(r.slug)) continue;
  fills += 1;
  buyUsd.set(cid, (buyUsd.get(cid) ?? 0) + (r.usdcSize ?? 0));
  const key = `${cid}\u0000${r.asset}`;
  let leg = legs.get(key);
  if (!leg) {
    leg = { usd: 0, shares: 0 };
    legs.set(key, leg);
    let c = conds.get(cid);
    if (!c) { c = { slug: r.slug, tokens: new Set() }; conds.set(cid, c); }
    c.tokens.add(r.asset);
  }
  leg.usd += r.usdcSize ?? 0;
  leg.shares += r.size ?? 0;
}

// per-round economics, split by the wallet's OWN action space
const pairUsd = [], pairShares = [], singleUsd = [], singleShares = [];
const pairCosts = [];
const pairRounds = new Set();
for (const [cid, c] of conds) {
  const legsOf = [...c.tokens].map((tk) => legs.get(`${cid}\u0000${tk}`)).filter((l) => l.shares > 0);
  if (legsOf.length >= 2) {
    pairRounds.add(cid);
    let usd = 0, cost = 0, minShares = Infinity;
    for (const l of legsOf) {
      usd += l.usd;
      cost += l.usd / l.shares;
      if (l.shares < minShares) minShares = l.shares;
    }
    pairUsd.push(usd);
    pairShares.push(minShares);
    pairCosts.push(cost);
  } else if (legsOf.length === 1) {
    singleUsd.push(legsOf[0].usd);
    singleShares.push(legsOf[0].shares);
  }
}
pairCosts.sort((a, b) => a - b);

const quantiles = (a) => {
  if (a.length === 0) return null;
  const s = [...a].sort((x, y) => x - y);
  const q = (p) => {
    const i = (s.length - 1) * p;
    const lo = Math.floor(i), hi = Math.min(lo + 1, s.length - 1), f = i - lo;
    return +(s[lo] * (1 - f) + s[hi] * f).toFixed(4);
  };
  return {
    n: a.length,
    p10: q(0.10), p25: q(0.25), p50: q(0.50), p75: q(0.75),
    p90: q(0.90), p95: q(0.95), p99: q(0.99), max: +s[s.length - 1].toFixed(4),
    mean: +(a.reduce((t, x) => t + x, 0) / a.length).toFixed(4),
  };
};

// live ROI calibers — the overlay's settled-round rule, pair and whole scope
const liveRoiScope = (pairOnly) => {
  let invested = 0, realized = 0, rounds = 0;
  for (const [cid, buy] of buyUsd) {
    if (pairOnly && !pairRounds.has(cid)) continue;
    if (!settled.has(cid)) continue;
    invested += buy;
    realized += (settleUsd.get(cid) ?? 0) - buy;
    rounds += 1;
  }
  return {
    rounds,
    investedUsd: +invested.toFixed(2),
    realizedUsd: +realized.toFixed(2),
    finalPct: invested > 0 ? +((realized / invested) * 100).toFixed(3) : null,
    note: 'live realized = REDEEM+MERGE usdc − buy usdc over settled rounds (rebates excluded here; the cashflow axis carries them)',
  };
};

const cashflowByType = Object.fromEntries([...cashType.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).map(([k, v]) => [k, { usd: +v.usd.toFixed(2), rows: v.rows }]));
const cashflowTotalUsd = +[...cashType.values()].reduce((t, e) => t + e.usd, 0).toFixed(2);

const live = {
  fills,
  roundsTouched: conds.size,
  pairRounds: pairRounds.size,
  singleLegRounds: conds.size - pairRounds.size,
  pairCost: pairCosts.length
    ? {
        median: +pairCosts[Math.floor(pairCosts.length / 2)].toFixed(10),
        min: +pairCosts[0].toFixed(10),
        max: +pairCosts[pairCosts.length - 1].toFixed(10),
        belowTrigger: pairCosts.filter((c) => c < 0.995).length,
        belowTriggerPct: +((pairCosts.filter((c) => c < 0.995).length / pairCosts.length) * 100).toFixed(1),
      }
    : null,
  cashflow: { byType: cashflowByType, totalUsd: cashflowTotalUsd },
  sizing: {
    pairRoundUsd: quantiles(pairUsd),
    pairRoundShares: quantiles(pairShares),
    singleRoundUsd: quantiles(singleUsd),
    singleRoundShares: quantiles(singleShares),
    note: 'per-round notional/shares the wallet ACTUALLY deployed (pair rounds: equal-share legs, shares = smaller leg; entries are pre-settlement so unsettled rounds count)',
  },
  roi: { pairScope: liveRoiScope(true), wholeScope: liveRoiScope(false) },
};

// optional mechanical cross-check against a ground-truth dump
let liveCrossCheck = null;
if (gtPath) {
  const gt = JSON.parse(readFileSync(gtPath, 'utf8'));
  const gtMaker = gt.cashflow?.rebatesUsd ?? 0; // GT sums MAKER+TAKER — compare the SUM
  const mineRebates = ((cashType.get('MAKER_REBATE')?.usd ?? 0) + (cashType.get('TAKER_REBATE')?.usd ?? 0));
  liveCrossCheck = {
    gt: gtPath,
    fillsDelta: fills - (gt.fills ?? 0),
    pairRoundsDelta: pairRounds.size - (gt.pairRounds ?? 0),
    pairCostMedianDelta: gt.pairCost ? +(live.pairCost.median - gt.pairCost.median).toFixed(12) : null,
    rebatesUsdDelta: +((mineRebates ?? 0) - gtMaker).toFixed(2),
    rewardsUsdDelta: +((cashType.get('REWARD')?.usd ?? 0) - (gt.cashflow?.rewardsUsd ?? 0)).toFixed(2),
    buyUsdDelta: gt.cashflow ? +((buyUsd.size ? [...buyUsd.values()].reduce((t, x) => t + x, 0) : 0) - gt.cashflow.buyUsd).toFixed(2) : null,
    note: 'all deltas MUST be 0 for the live side to count as the same ground truth; rebates compared against GT rebatesUsd (MAKER+TAKER)',
  };
}

// ── backtest arms ───────────────────────────────────────────────────────────
const relDevPct = (liveV, btV) => (liveV == null || liveV === 0 || btV == null ? null : +(((btV - liveV) / Math.abs(liveV)) * 100).toFixed(1));
const absRelDevPct = (liveV, btV) => (liveV == null || liveV === 0 || btV == null ? null : +(Math.abs(((btV - liveV) / Math.abs(liveV)) * 100)).toFixed(1));

function readReport(path) {
  const report = JSON.parse(readFileSync(path, 'utf8'));
  if ((report.tradeLinesTruncated ?? 0) > 0) {
    die(`${path} reports tradeLinesTruncated=${report.tradeLinesTruncated} — a clipped trade list would bias every per-round statistic (same fail-closed rule as the overlay).`);
  }
  return report;
}

// backtest ROI calibers, computed here so both sides share one rule set:
//   invested per close = |netPnlUsd / netPnlPct x 100| (the overlay's exact
//   basis; 0-PnL flats carry 0 in both and are excluded consistently),
//   realized per close = netPnlUsd (fees INCLUDED — the official schedule).
function btRoiScopes(report) {
  const investedByCid = new Map();
  const realizedByCid = new Map();
  const closeShares = [];
  for (const tl of report.tradeLines ?? []) {
    const cid = tl.conditionId;
    if (!cid) continue;
    const inv = tl.netPnlPct && tl.netPnlPct !== 0 ? Math.abs((tl.netPnlUsd / tl.netPnlPct) * 100) : 0;
    investedByCid.set(cid, (investedByCid.get(cid) ?? 0) + inv);
    realizedByCid.set(cid, (realizedByCid.get(cid) ?? 0) + (tl.netPnlUsd ?? 0));
    if (tl.entryPrice > 0 && inv > 0) closeShares.push(inv / tl.entryPrice);
  }
  const scope = (pairOnly) => {
    let invested = 0, realized = 0, rounds = 0;
    for (const [cid, inv] of investedByCid) {
      if (pairOnly && !pairRounds.has(cid)) continue;
      if (pairOnly && !settled.has(cid)) continue;
      invested += inv;
      realized += realizedByCid.get(cid) ?? 0;
      rounds += 1;
    }
    return {
      rounds,
      investedUsd: +invested.toFixed(2),
      realizedUsd: +realized.toFixed(2),
      finalPct: invested > 0 ? +((realized / invested) * 100).toFixed(3) : null,
    };
  };
  return {
    pairScope: scope(true),
    wholeScope: scope(false),
    perRoundInvestedUsd: quantiles([...investedByCid.values()]),
    perCloseShares: quantiles(closeShares),
  };
}

async function overlayFor(tag, reportPath) {
  if (overlays.has(tag)) return JSON.parse(readFileSync(overlays.get(tag), 'utf8'));
  const overlayScript = join(dirname(fileURLToPath(import.meta.url)), 'almach-overlay.mjs');
  const res = await new Promise((resolve) => {
    const child = spawn(process.execPath, [overlayScript, ledgerPath, reportPath, startArg, endArg, durArg], {
      stdio: ['ignore', 'pipe', 'pipe'],
      maxBuffer: 64 * 1024 * 1024,
    });
    let out = '', err = '';
    child.stdout.on('data', (d) => { out += d; });
    child.stderr.on('data', (d) => { err += d; });
    child.on('close', (code) => resolve({ code, out, err }));
    child.on('error', (e) => resolve({ code: -1, out: '', err: String(e) }));
  });
  if (res.code !== 0) {
    die(`overlay for arm "${tag}" failed (exit ${res.code}): ${res.err.trim() || 'no stderr'}`);
  }
  return JSON.parse(res.out);
}

function verdictRow(name, liveV, btV, dev, note) {
  const devAbs = dev == null ? null : Math.abs(dev);
  return {
    caliber: name,
    live: liveV,
    backtest: btV,
    relDevPct: dev,
    passAt5Pct: devAbs != null ? devAbs <= PASS_PCT : false,
    note: note ?? null,
  };
}

const armResults = [];
for (const arm of arms) {
  const report = readReport(arm.path);
  const btSizing = btRoiScopes(report);
  const ov = await overlayFor(arm.tag, arm.path);

  const strategies = (report.strategies ?? []).map((s) => ({
    name: s.name,
    enabled: s.enabled ?? false,
    sizingSource: s.sizingSource ?? null,
    effectiveSizeUsd: s.effectiveSizeUsd ?? null,
    entryBudgetUsd: s.entryBudgetUsd ?? null,
    effectiveMaxShares: s.effectiveMaxShares ?? null,
    ordersPlaced: s.ordersPlaced ?? null,
    ordersRejected: s.ordersRejected ?? null,
  }));
  const enabledNames = strategies.filter((s) => s.enabled).map((s) => s.name);
  const pairOnlyArm = enabledNames.length > 0 && enabledNames.every((n) => n === 'pair_discount_arb');

  // cashflow pipe: the corpus carries MAKER_REBATE + REWARD only — compare by type
  const btCash = report.cashflows ?? {};
  const liveMaker = cashType.get('MAKER_REBATE')?.usd ?? 0;
  const liveReward = cashType.get('REWARD')?.usd ?? 0;
  const liveTaker = cashType.get('TAKER_REBATE')?.usd ?? 0;
  const cashflowDeviations = [
    verdictRow('cashflowMakerRebate', +liveMaker.toFixed(2), +(btCash.rebatesUsd ?? 0), absRelDevPct(liveMaker, btCash.rebatesUsd ?? 0),
      'the #388 pipe credits the wallet\'s own corpus rows; a pass means replay 1:1, not strategy earnings'),
    verdictRow('cashflowReward', +liveReward.toFixed(2), +(btCash.rewardsUsd ?? 0), absRelDevPct(liveReward, btCash.rewardsUsd ?? 0), null),
  ];
  if (liveTaker > 0) {
    cashflowDeviations.push(verdictRow('cashflowTakerRebate', +liveTaker.toFixed(2), 0, null,
      'STRUCTURAL: TAKER_REBATE rows are live wallet cash the corpus does not carry as cashflow events — the replay cannot credit them; reported, never averaged away'));
  }

  const roiPairDev = relDevPct(live.roi.pairScope.finalPct, btSizing.pairScope.finalPct);
  const roiWholeDev = relDevPct(live.roi.wholeScope.finalPct, btSizing.wholeScope.finalPct);
  const roiPairDevAbs = roiPairDev == null ? null : Math.abs(roiPairDev);
  const roiWholeDevAbs = roiWholeDev == null ? null : Math.abs(roiWholeDev);

  const deviations = {
    roiPair: {
      liveFinalPct: live.roi.pairScope.finalPct,
      backtestFinalPct: btSizing.pairScope.finalPct,
      relDevPct: roiPairDev,
      absRelDevPct: roiPairDevAbs,
      passAt5Pct: roiPairDevAbs != null && roiPairDevAbs <= PASS_PCT,
      rounds: { live: live.roi.pairScope.rounds, backtest: btSizing.pairScope.rounds },
    },
    roiWhole: {
      liveFinalPct: live.roi.wholeScope.finalPct,
      backtestFinalPct: btSizing.wholeScope.finalPct,
      relDevPct: roiWholeDev,
      absRelDevPct: roiWholeDevAbs,
      passAt5Pct: roiWholeDevAbs != null && roiWholeDevAbs <= PASS_PCT,
      rounds: { live: live.roi.wholeScope.rounds, backtest: btSizing.wholeScope.rounds },
    },
    finalValuePair: {
      liveNetUsd: ov.netUsd?.actualPair ?? null,
      backtestNetUsd: ov.netUsd?.backtest ?? null,
      relDevPct: ov.deviation?.pairScopeFinalRelPct ?? null,
      passAt5Pct: ov.deviation?.pairScopeFinalRelPct != null && Math.abs(ov.deviation.pairScopeFinalRelPct) <= PASS_PCT,
    },
    finalValueWhole: {
      liveNetUsd: ov.netUsd?.actualWhole ?? null,
      backtestNetUsd: ov.netUsd?.backtest ?? null,
      relDevPct: ov.deviation?.wholeScopeFinalRelPct ?? null,
      passAt5Pct: ov.deviation?.wholeScopeFinalRelPct != null && Math.abs(ov.deviation.wholeScopeFinalRelPct) <= PASS_PCT,
    },
    signals: {
      sharedRounds: ov.signals?.sharedRounds ?? null,
      backtestOnlyRounds: ov.signals?.backtestOnlyRounds ?? null,
      actualOnlyRounds: ov.signals?.actualOnlyRounds ?? null,
      coverageOfActualPairsPct: ov.signals?.coverageOfActualPairsPct ?? null,
      note: 'coverage is reported, not judged at 5%: a shared round is a binary fact, and the trigger gate (max_pair_cost 0.995) deliberately declines rounds the wallet took',
    },
    cashflow: cashflowDeviations,
  };

  armResults.push({
    tag: arm.tag,
    report: arm.path,
    actionSpace: {
      enabledStrategies: enabledNames,
      pairOnly: pairOnlyArm,
      liveReference: { pairRounds: pairRounds.size, singleLegRounds: conds.size - pairRounds.size },
    },
    kernel: {
      trades: report.trades ?? null,
      cashflows: btCash,
      feeSchedule: report.feeSchedule ?? null,
      strategies,
    },
    sizing: {
      perRoundInvestedUsd: btSizing.perRoundInvestedUsd,
      perCloseShares: btSizing.perCloseShares,
      liveReference: {
        pairRoundUsd: live.sizing.pairRoundUsd,
        pairRoundShares: live.sizing.pairRoundShares,
        singleRoundUsd: live.sizing.singleRoundUsd,
        singleRoundShares: live.sizing.singleRoundShares,
      },
      note: 'the kernel sizing face is GLOBAL (share band + per-order cap); declared share counts follow replayed top-of-book depth, so alignment is distribution-level at best — one entry attempt per leg per round vs the wallet accumulating fills across the round lifetime is the residual no cap can remove',
    },
    roi: {
      pairScope: btSizing.pairScope,
      wholeScope: btSizing.wholeScope,
      note: 'backtest realized = Σ netPnlUsd (official fee schedule INCLUDED); live realized = settle − buy (ledger books no per-fill fee rows) — the fee asymmetry is a stated口径 difference',
    },
    overlay: {
      rounds: ov.rounds ?? null,
      netUsd: ov.netUsd ?? null,
      deviation: ov.deviation ?? null,
      signals: ov.signals ?? null,
    },
    deviations,
    verdict: {
      roiPairPassAt5Pct: deviations.roiPair.passAt5Pct,
      roiWholePassAt5Pct: deviations.roiWhole.passAt5Pct,
      finalValuePairPassAt5Pct: deviations.finalValuePair.passAt5Pct,
      finalValueWholePassAt5Pct: deviations.finalValueWhole.passAt5Pct,
      cashflowPipePassAt5Pct: cashflowDeviations.filter((c) => c.passAt5Pct !== null).every((c) => c.passAt5Pct !== false),
      allPassAt5Pct: [
        deviations.roiPair.passAt5Pct, deviations.roiWhole.passAt5Pct,
        deviations.finalValuePair.passAt5Pct, deviations.finalValueWhole.passAt5Pct,
      ].every(Boolean),
    },
  });
}

const result = {
  harness: 'almach-calib (#389): calibrated A/B — same sizing / same action space / same fee cashflow',
  passThresholdPct: PASS_PCT,
  window: { start: startArg, end: endArg, duration: durArg },
  ledger: ledgerPath,
  live,
  liveCrossCheck,
  arms: armResults,
};
console.log(JSON.stringify(result, null, 2));
