#!/usr/bin/env node
/**
 * #355 — the @almach GROUND-TRUTH analyzer.
 *
 * The benchmark's reverse gate needs the wallet's ACTUAL economics computed
 * the same way the backtest computes its own: per round, per leg VWAP, the
 * trigger-口径 pair cost (sum of both legs' VWAPs — what pair_discount_arb's
 * `max_pair_cost 0.995` gate compares), and the ledger cashflow (BUY out,
 * REDEEM/MERGE in, rebates). Everything derives from the pre-fetched
 * `/activity` export (`data/almach/activity-full.jsonl`) — the COMPLETE
 * ledger; the Data API's `/trades` endpoint answers only a taker-side
 * subset (17.6% of the same window's notional, measured 2026-10-02, see
 * onchain.rs module docs).
 *
 * Output: one JSON document on stdout. Deterministic for a given ledger.
 *
 * Usage:
 *   node scripts/almach-ground-truth.mjs <activity.jsonl> [YYYY-MM-DD YYYY-MM-DD [5m|15m|1h|4h]]
 */
import { readFileSync } from 'node:fs';

const [ledgerPath, startArg, endArg, durArg] = process.argv.slice(2);
if (!ledgerPath) {
  console.error('usage: almach-ground-truth.mjs <activity.jsonl> [start end [duration]]');
  process.exit(2);
}
const startSec = startArg ? Date.parse(`${startArg}T00:00:00Z`) / 1000 : -Infinity;
const endSec = endArg ? Date.parse(`${endArg}T00:00:00Z`) / 1000 + 86_400 : Infinity;
const durFilter = durArg ?? null;

const durOf = (slug) => {
  const p = (slug ?? '').split('-');
  return p.length >= 2 && ['5m', '15m', '1h', '4h'].includes(p.at(-2)) ? p.at(-2) : null;
};

// ── pass 1: fills into per-(condition, token) legs ──────────────────────────
// leg: {usd, shares} — VWAP = usd/shares, the price the wallet actually paid.
const legs = new Map(); // `${cid}\u0000${token}` -> {cid, token, usd, shares, slug, outcome}
const conds = new Map(); // cid -> {slug, tokens:Set}
let fills = 0;
let redeemUsd = 0, redeemRows = 0, mergeUsd = 0, mergeRows = 0;
let rebateUsd = 0, rewardUsd = 0;

const lines = readFileSync(ledgerPath, 'utf8').split('\n');
for (const line of lines) {
  if (!line) continue;
  const r = JSON.parse(line);
  const ts = r.timestamp ?? 0;
  if (ts < startSec || ts >= endSec) continue;
  const t = r.type;
  if (t === 'REDEEM') {
    // Settlement rows are duration-filtered by slug too: without it a 5m
    // window's cashflow would book every 15m/1h/4h round that settled in
    // the same wall-clock days (usdcSize = cash actually received; losers
    // still print a size row at $0).
    if (durFilter && durOf(r.slug) !== durFilter) continue;
    redeemUsd += r.usdcSize ?? 0; redeemRows += 1; continue;
  }
  if (t === 'MERGE') {
    if (durFilter && durOf(r.slug) !== durFilter) continue;
    mergeUsd += r.usdcSize ?? 0; mergeRows += 1; continue;
  }
  if (t === 'MAKER_REBATE' || t === 'TAKER_REBATE') { rebateUsd += r.usdcSize ?? 0; continue; }
  if (t === 'REWARD') { rewardUsd += r.usdcSize ?? 0; continue; }
  if (t !== 'TRADE') continue;
  const d = durOf(r.slug);
  if (!d) continue; // non-round markets the wallet also touched
  if (durFilter && d !== durFilter) continue;
  fills += 1;
  const key = `${r.conditionId}\u0000${r.asset}`;
  let leg = legs.get(key);
  if (!leg) {
    leg = { cid: r.conditionId, token: r.asset, usd: 0, shares: 0, slug: r.slug, outcome: r.outcome };
    legs.set(key, leg);
    let c = conds.get(r.conditionId);
    if (!c) { c = { slug: r.slug, dur: d, tokens: new Set() }; conds.set(r.conditionId, c); }
    c.tokens.add(r.asset);
  }
  leg.usd += r.usdcSize ?? 0;
  leg.shares += r.size ?? 0;
}

// ── pair economics in the TRIGGER 口径 ──────────────────────────────────────
// A pair exists when both legs were bought in the same round; its cost is
// legVWAP(up) + legVWAP(down) — exactly what the strategy's max_pair_cost
// gate compares against 0.995. Single-leg rounds are directional entries
// the replayed strategy never takes; they are counted, not averaged.
const pairCosts = [];
let singleLegRounds = 0;
for (const [cid, c] of conds) {
  if (c.tokens.size < 2) { singleLegRounds += 1; continue; }
  const vws = [];
  let usd = 0;
  for (const t of c.tokens) {
    const leg = legs.get(`${cid}\u0000${t}`);
    if (!leg || leg.shares <= 0) { vws.length = 0; break; }
    vws.push(leg.usd / leg.shares);
    usd += leg.usd;
  }
  if (vws.length === 2) {
    pairCosts.push({ cid, cost: vws[0] + vws[1], buyUsd: usd });
  }
}
pairCosts.sort((a, b) => a.cost - b.cost);
const costs = pairCosts.map((p) => p.cost);
const sum = (a) => a.reduce((s, x) => s + x, 0);
const below = costs.filter((c) => c < 0.995).length;

// ── cashflow ────────────────────────────────────────────────────────────────
let tradeUsd = 0;
for (const line of lines) {
  if (!line) continue;
  const r = JSON.parse(line);
  if (r.type !== 'TRADE') continue;
  const ts = r.timestamp ?? 0;
  if (ts < startSec || ts >= endSec) continue;
  const d = durOf(r.slug);
  if (!d || (durFilter && d !== durFilter)) continue;
  tradeUsd += r.usdcSize ?? 0;
}

const result = {
  ledger: ledgerPath,
  window: { start: startArg ?? null, end: endArg ?? null, duration: durFilter },
  fills,
  roundsTouched: conds.size,
  pairRounds: pairCosts.length,
  singleLegRounds,
  pairCost: costs.length
    ? {
        mean: sum(costs) / costs.length,
        median: costs[Math.floor(costs.length / 2)],
        min: costs[0],
        max: costs.at(-1),
        belowTrigger: below,
        belowTriggerPct: +((below / costs.length) * 100).toFixed(1),
      }
    : null,
  cashflow: {
    buyUsd: +tradeUsd.toFixed(2),
    redeemUsd: +redeemUsd.toFixed(2),
    redeemRows,
    mergeUsd: +mergeUsd.toFixed(2),
    mergeRows,
    rebatesUsd: +rebateUsd.toFixed(2),
    rewardsUsd: +rewardUsd.toFixed(2),
    netUsd: +(redeemUsd + mergeUsd + rebateUsd + rewardUsd - tradeUsd).toFixed(2),
  },
};
console.log(JSON.stringify(result, null, 2));
