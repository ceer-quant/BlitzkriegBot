#!/usr/bin/env node
/**
 * #355 — the equity-curve overlay: backtest vs @almach actual, same caliber.
 *
 * Both sides are placed on ONE timeline by the same rule: a round's realized
 * PnL is booked at its round END (slug epoch + duration), with the
 * conditionId → round-end registry built from the /activity ledger itself
 * (every condition the backtest trades comes from that ledger's universe).
 *
 *   actual side  : per condition, realized = (REDEEM + MERGE usdc received
 *                  for that conditionId) − (TRADE usdc paid into it). Rows
 *                  are slug-duration filtered like almach-ground-truth.mjs.
 *   backtest side: per condition, sum of `netPnlUsd` over the report's
 *                  tradeLines (the pair strategy books one close per leg).
 *
 * HONESTY GUARD: a round is only booked on the actual side if at least one
 * settlement row (REDEEM/MERGE) exists for it — losers still print a $0
 * REDEEM row, so "buys without any settle row" means the settlement fell
 * outside the window cut, NOT a total loss. Such rounds are counted in
 * `unsettledExcluded`, never silently booked.
 *
 * The comparison runs at two scopes, and the difference between them IS a
 * distortion finding, not noise:
 *   pair scope  — rounds where the wallet bought BOTH legs (the strategy's
 *                 action space).
 *   whole scope — every round the wallet traded (it also takes single-leg
 *                 directional rounds the replayed strategy structurally
 *                 cannot take).
 *
 * Deterministic JSON on stdout. `points` is stride-decimated to ≤ 2000
 * entries; all deviation stats are computed on the FULL grid first.
 *
 * Usage:
 *   node scripts/almach-overlay.mjs <activity.jsonl> <backtest-report.json> <start> <end> <5m|15m|1h|4h>
 */
import { readFileSync } from 'node:fs';

const [ledgerPath, reportPath, startArg, endArg, durArg] = process.argv.slice(2);
if (!ledgerPath || !reportPath || !startArg || !endArg || !durArg) {
  console.error('usage: almach-overlay.mjs <activity.jsonl> <backtest-report.json> <start> <end> <dur>');
  process.exit(2);
}
const startSec = Date.parse(`${startArg}T00:00:00Z`) / 1000;
const endSec = Date.parse(`${endArg}T00:00:00Z`) / 1000 + 86_400;
const DUR_SEC = { '5m': 300, '15m': 900, '1h': 3600, '4h': 14_400 };
const durSec = DUR_SEC[durArg];
if (!durSec) {
  console.error(`unknown duration ${durArg}`);
  process.exit(2);
}

const isDur = (slug) => (slug ?? '').split('-').at(-2) === durArg;
const endOfSlug = (slug) => {
  const start = Number((slug ?? '').split('-').at(-1));
  return Number.isFinite(start) ? start + durSec : null;
};

// ── pass 1: the ledger → registry + per-condition cashflow ─────────────────
const roundEnd = new Map(); // cid -> round end sec (from fills)
const pairRounds = new Set(); // cids where BOTH legs were bought
const legTokens = new Map(); // cid -> Set(token)
const buyUsd = new Map(); // cid -> usd paid
const settleUsd = new Map(); // cid -> usd received (REDEEM + MERGE)
const settled = new Set(); // cids with at least one settle row

const lines = readFileSync(ledgerPath, 'utf8').split('\n');
for (const line of lines) {
  if (!line) continue;
  const r = JSON.parse(line);
  const ts = r.timestamp ?? 0;
  if (ts < startSec || ts >= endSec) continue;
  const t = r.type;
  const cid = r.conditionId;
  if (!cid) continue;
  if (t === 'REDEEM' || t === 'MERGE') {
    if (!isDur(r.slug)) continue;
    settled.add(cid);
    settleUsd.set(cid, (settleUsd.get(cid) ?? 0) + (r.usdcSize ?? 0));
    continue;
  }
  if (t !== 'TRADE' || !isDur(r.slug)) continue;
  if (!roundEnd.has(cid)) roundEnd.set(cid, endOfSlug(r.slug));
  buyUsd.set(cid, (buyUsd.get(cid) ?? 0) + (r.usdcSize ?? 0));
  if (!legTokens.has(cid)) legTokens.set(cid, new Set());
  legTokens.get(cid).add(r.asset);
}
for (const [cid, toks] of legTokens) if (toks.size >= 2) pairRounds.add(cid);

// ── backtest side: per-condition net from the report's tradeLines ──────────
const report = JSON.parse(readFileSync(reportPath, 'utf8'));
const btNet = new Map(); // cid -> sum netPnlUsd
for (const tl of report.tradeLines ?? []) {
  const cid = tl.conditionId;
  if (!cid) continue;
  btNet.set(cid, (btNet.get(cid) ?? 0) + (tl.netPnlUsd ?? 0));
}

// ── curves (pair scope + whole scope) ──────────────────────────────────────
let unsettledExcluded = 0;
const actualRealized = (pairOnly) => {
  const m = new Map();
  for (const [cid, end] of roundEnd) {
    if (pairOnly && !pairRounds.has(cid)) continue;
    if (!settled.has(cid)) { unsettledExcluded += 1; continue; }
    m.set(cid, (settleUsd.get(cid) ?? 0) - (buyUsd.get(cid) ?? 0));
  }
  return m;
};
const curveOf = (map) => {
  const pts = [...map].map(([cid, v]) => [roundEnd.get(cid), v]);
  pts.sort((a, b) => a[0] - b[0]);
  let acc = 0;
  return pts.map(([t, v]) => [t, +(acc += v).toFixed(4)]);
};

const actualCurve = curveOf(actualRealized(true));
const wholeCurve = curveOf(actualRealized(false));
const btCurve = curveOf(btNet);

// ── ROI curves: the dimensionless caliber ───────────────────────────────────
// Absolute dollars compare a $1000-seed replay against a wallet deploying
// tens of thousands per day — a size mismatch, not a signal mismatch. The
// honest overlay is return on INVESTED capital: realized-so-far divided by
// capital deployed-so far, both sides at their own sizes.
const investedByRound = new Map(); // cid -> buy usd (pair scope, settled only)
for (const [cid, end] of roundEnd) {
  if (!pairRounds.has(cid) || !settled.has(cid)) continue;
  investedByRound.set(cid, buyUsd.get(cid) ?? 0);
}
const roiCurve = (curve, investedPerRound) => {
  const inv = new Map();
  let acc = 0;
  for (const [cid, v] of investedPerRound) inv.set(roundEnd.get(cid), (inv.get(roundEnd.get(cid)) ?? 0) + v);
  const times = [...new Set([...curve.map((p) => p[0]), ...inv.keys()])].sort((a, b) => a - b);
  let invested = 0, realized = 0, i = 0;
  const out = [];
  for (const t of times) {
    invested += inv.get(t) ?? 0;
    while (i < curve.length && curve[i][0] <= t) { realized = curve[i][1]; i += 1; }
    out.push([t, invested > 0 ? +((realized / invested) * 100).toFixed(3) : 0]);
  }
  return out;
};
const actualRoi = roiCurve(actualCurve, investedByRound);
const btInvested = new Map();
for (const tl of report.tradeLines ?? []) {
  const cid = tl.conditionId;
  if (!cid || !pairRounds.has(cid) || !settled.has(cid)) continue;
  // entryPrice is per-share; shares are not in the report — approximate the
  // invested basis with entryPrice (1 share units) ONLY for the ratio shape;
  // the comparison that matters is per-round edge, not this approximation.
  btInvested.set(cid, (btInvested.get(cid) ?? 0) + (tl.entryPrice ?? 0));
}
const btRoi = roiCurve(btCurve, btInvested);

// ── signal agreement: which rounds did each side take? ─────────────────────
const both = [...btNet.keys()].filter((c) => pairRounds.has(c) && settled.has(c)).length;
const btOnly = [...btNet.keys()].filter((c) => !pairRounds.has(c)).length;
const actualOnly = [...pairRounds].filter((c) => !btNet.has(c) && settled.has(c)).length;

// ── deviation on the shared timeline ───────────────────────────────────────
const lastLE = (curve, t) => {
  let lo = 0, hi = curve.length - 1, ans = null;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (curve[mid][0] <= t) { ans = curve[mid][1]; lo = mid + 1; } else hi = mid - 1;
  }
  return ans ?? 0;
};
const grid = [...new Set([...actualCurve.map((p) => p[0]), ...btCurve.map((p) => p[0])])].sort((a, b) => a - b);
const devs = grid.map((t) => {
  const a = lastLE(actualCurve, t);
  const b = lastLE(btCurve, t);
  return { t, actual: a, backtest: b, diff: +(b - a).toFixed(4) };
});
const relDev = (a, b) => (a === 0 ? null : Math.abs(b - a) / Math.abs(a));
const finalDev = relDev(actualCurve.at(-1)?.[1] ?? 0, btCurve.at(-1)?.[1] ?? 0);
const wholeDev = relDev(wholeCurve.at(-1)?.[1] ?? 0, btCurve.at(-1)?.[1] ?? 0);
// stride-decimate the point list for the report; stats above are full-grid
const stride = Math.max(1, Math.ceil(devs.length / 2000));
const points = devs.filter((_, i) => i % stride === 0);

const result = {
  window: { start: startArg, end: endArg, duration: durArg },
  rounds: {
    actualTraded: roundEnd.size,
    actualPairRounds: pairRounds.size,
    unsettledExcluded,
    backtestTraded: btNet.size,
    backtestOutsideLedger: [...btNet.keys()].filter((c) => !roundEnd.has(c)).length,
  },
  netUsd: {
    actualPair: +(actualCurve.at(-1)?.[1] ?? 0).toFixed(2),
    backtest: +(btCurve.at(-1)?.[1] ?? 0).toFixed(2),
    actualWhole: +(wholeCurve.at(-1)?.[1] ?? 0).toFixed(2),
  },
  deviation: {
    pairScopeFinalRelPct: finalDev == null ? null : +(finalDev * 100).toFixed(1),
    wholeScopeFinalRelPct: wholeDev == null ? null : +(wholeDev * 100).toFixed(1),
    maxAbsDiffUsd: devs.length ? +Math.max(...devs.map((d) => Math.abs(d.diff))).toFixed(2) : 0,
  },
  // dimensionless calibers — where a size mismatch stops lying
  roi: {
    actualPairFinalPct: actualRoi.at(-1)?.[1] ?? 0,
    backtestFinalPct: btRoi.at(-1)?.[1] ?? 0,
    actualCurve: actualRoi,
    backtestCurve: btRoi,
    note: 'backtest invested-basis approximates 1-share units (entryPrice sum): the shape, not the level, is comparable',
  },
  signals: {
    sharedRounds: both,
    backtestOnlyRounds: btOnly,
    actualOnlyRounds: actualOnly,
    coverageOfActualPairsPct: pairRounds.size ? +((both / pairRounds.size) * 100).toFixed(1) : null,
  },
  curves: { actualPair: actualCurve, backtest: btCurve, actualWhole: wholeCurve },
  pointsStride: stride,
  points,
};
console.log(JSON.stringify(result, null, 2));
