#!/usr/bin/env node
/**
 * Shadow Engine analyzer (v3) — replay through the SHARED exit policy.
 *
 * Unlike v2 (which re-implemented TP/SL/trailing inline and inevitably drifted
 * from production), this imports the exact pure functions the live engine uses
 * (dist/strategies/crypto-hft/exit-policy.js). A strategy change changes both
 * the backtest and live trading at once.
 *
 * Outputs:
 *   - realized (actual) net PnL from the trades
 *   - in-sample best over a small parameter grid (optimistic upper bound)
 *   - walk-forward OUT-OF-SAMPLE PnL: pick params on an expanding past window,
 *     apply once to the next record — guards against curve-fitting a few paths
 *
 * Triggers and PnL are evaluated on the EXECUTABLE bid (new records carry b/a);
 * old records that only stored mid fall back to mid == bid (v2 assumption).
 *
 * Run `npm run build` first so dist/ is fresh.
 *
 * Usage: node scripts/analyze-shadow.mjs [--file data/shadow/positions.jsonl]
 */

import { readFileSync, existsSync } from 'fs';
import { resolve } from 'path';
import { pathToFileURL } from 'url';

const args = process.argv.slice(2);
const i = args.indexOf('--file');
const FILE = i >= 0 ? args[i + 1] : 'data/shadow/positions.jsonl';
const POLICY = resolve('dist/strategies/crypto-hft/exit-policy.js');

if (!existsSync(FILE)) {
  console.error(`No shadow file at ${resolve(FILE)} — run the engine first.`);
  process.exit(1);
}
if (!existsSync(POLICY)) {
  console.error(`Shared policy not built at ${POLICY}. Run \`npm run build\` first.`);
  process.exit(1);
}

const { createExitState, updateExitState, decideExit } = await import(pathToFileURL(POLICY).href);

// ── Load records ────────────────────────────────────────────────────────────
const rows = readFileSync(FILE, 'utf-8')
  .split('\n')
  .filter(Boolean)
  .map((l) => { try { return JSON.parse(l); } catch { return null; } })
  .filter((r) => r && r.shares > 0 && r.own && Array.isArray(r.own.samples) && r.own.samples.length >= 3)
  .sort((a, b) => a.enteredAt - b.enteredAt);

if (rows.length === 0) {
  console.error('No records with a usable path yet.');
  process.exit(1);
}

// ── Base config — exit fields MUST mirror DEFAULT_CONFIG in index.ts ─────────
const BASE_CFG = {
  forceExitSec: 120,
  minTimeLeftSec: 180,
  takeProfitPct: 100,
  stopLossPct: 50,
  simpleExitEnabled: true,
  dynamicStopEnabled: true,
  stopTightenStartSec: 300,
  stopMinPct: 10,
  trailingEnabled: true,
  trailingMinHighPct: 15,
  minTrailPct: 10,
  proportionalTrailEnabled: true,
  proportionalTrailPct: 15,
  proportionalTrailMinPct: 15,
  proportionalTrailMinGivebackPct: 3,
  tightStopEnabled: true,
  tightStopPct: 12,
  ratchetEnabled: false,
  ratchetConfirmTicks: 3,
  ratchetConfirmTolerancePct: 0.5,
  maxBidWickPct: 8,
  staleProfitPct: 20,
  staleProfitBidUnchangedSec: 10,
  stagnantProfitPct: 5,
  stagnantDurationSec: 30,
  depthCollapseThresholdPct: 60,
  exitGraceSec: 3,
  makerExitsForTpOnly: false,
  makerFirstExitEnabled: true,
};

const fee = (p) => 0.125 * Math.pow(p * (1 - p), 2);

// Minimal book the pure policy accepts. New shadow records carry executable
// bid/ask (b/a); old records only stored mid (p) → assume bid == mid.
function bookAt(sample) {
  const mid = sample.p;
  const bid = typeof sample.b === 'number' && sample.b > 0 ? sample.b : mid;
  const ask = typeof sample.a === 'number' && sample.a > 0 ? sample.a : mid;
  return {
    bestBid: bid,
    bestAsk: ask,
    midPrice: mid,
    bidDepth: 100,
    askDepth: 100,
    bids: [[bid, 100]],
    asks: [[ask, 100]],
    timestamp: 0,
  };
}

/**
 * Replay ONE recorded path through the shared policy. Net USD after taker fees.
 * Conservative: every exit pays the taker fee; live maker-first TP can only beat this.
 */
function replay(rec, cfg) {
  const entry = rec.own.entryRef;
  const shares = rec.shares;
  const samples = rec.own.samples;
  const state = createExitState(entry, rec.enteredAt);
  const entryFee = rec.wasMakerEntry ? 0 : fee(entry) * shares;

  // Default: the policy's force-exit deadline ends the trade at the last bid.
  let exitPrice = samples[samples.length - 1].b || samples[samples.length - 1].p;
  let exitReason = 'force_exit';

  for (const s of samples) {
    const book = bookAt(s);
    const now = rec.enteredAt + s.t * 1000;
    updateExitState(state, entry, book, now, cfg);
    const timeLeftSec = (rec.expiredAt - now) / 1000;
    const holdSec = s.t;
    const d = decideExit({ entryPrice: entry, book, timeLeftSec, holdSec, state, now, cfg });
    if (d) {
      exitPrice = book.bestBid;
      exitReason = d.reason;
      break;
    }
  }

  const exitFee = fee(exitPrice) * shares;
  return {
    pnl: shares * (exitPrice - entry) - entryFee - exitFee,
    exitReason,
    exitPrice,
    highPct: state.highPnlPct,
  };
}

// ── Parameter grid (only exit levers the shadow data can inform) ─────────────
const GRID = [
  { name: 'current',        patch: {} },
  { name: 'SL25',           patch: { stopLossPct: 25 } },
  { name: 'SL15',           patch: { stopLossPct: 15 } },
  { name: 'SL25/trail8',    patch: { stopLossPct: 25, minTrailPct: 8 } },
  { name: 'SL15/trail8',    patch: { stopLossPct: 15, minTrailPct: 8 } },
  { name: 'SL25/arm10',     patch: { stopLossPct: 25, trailingMinHighPct: 10 } },
  { name: 'SL15/arm10/tr8', patch: { stopLossPct: 15, trailingMinHighPct: 10, minTrailPct: 8 } },
  { name: 'SL15/prop25',    patch: { stopLossPct: 15, proportionalTrailPct: 25 } },
];
const cfgFor = (patch) => ({ ...BASE_CFG, ...patch });

const actualTotal = rows.reduce((a, r) => a + (r.actual?.netPnlUsd || 0), 0);
const totalFor = (patch) => rows.reduce((a, r) => a + replay(r, cfgFor(patch)).pnl, 0);

console.log(`Loaded ${rows.length} shadow records (time-ordered) from ${FILE}`);
console.log(`Policy:   ${POLICY}`);
console.log(`\nActual realized net        : $${actualTotal.toFixed(2)}`);

console.log('\n=== In-sample grid (OPTIMISTIC — fit on all paths) ===');
const ranked = GRID
  .map((g) => ({ name: g.name, pnl: totalFor(g.patch) }))
  .sort((a, b) => b.pnl - a.pnl);
for (const g of ranked) console.log(`  ${g.name.padEnd(20)} $${g.pnl.toFixed(2).padStart(8)}`);

// ── Walk-forward: expanding train window, one-step-out-of-sample test ────────
const MIN_TRAIN = 6;
if (rows.length >= MIN_TRAIN + 2) {
  let oosPnl = 0;
  const oosRows = [];
  for (let k = MIN_TRAIN; k < rows.length; k++) {
    const train = rows.slice(0, k);
    let best = GRID[0], bestPnl = -Infinity;
    for (const g of GRID) {
      const p = train.reduce((a, r) => a + replay(r, cfgFor(g.patch)).pnl, 0);
      if (p > bestPnl) { bestPnl = p; best = g; }
    }
    const testRec = rows[k];
    const res = replay(testRec, cfgFor(best.patch));
    oosPnl += res.pnl;
    oosRows.push({ id: testRec.id, asset: testRec.asset, params: best.name, ...res });
  }

  console.log(`\n=== Walk-forward OUT-OF-SAMPLE (train on expanding past, test next record) ===`);
  console.log(`OOS records tested : ${oosRows.length}`);
  console.log(`OOS net PnL        : $${oosPnl.toFixed(2)}`);
  console.log(`In-sample best     : $${ranked[0].pnl.toFixed(2)} (${ranked[0].name}) — a large gap vs OOS signals overfitting`);
  console.log('\nid        asset params                 exit             pnl$   hi%');
  for (const r of oosRows) {
    console.log(
      `${String(r.id).padEnd(9)} ${String(r.asset).padEnd(5)} ${r.params.padEnd(22)} ${r.exitReason.padEnd(14)} ${r.pnl.toFixed(2).padStart(6)} ${Math.round(r.highPct)}`
    );
  }
} else {
  console.log(`\nNeed >= ${MIN_TRAIN + 2} records for walk-forward; have ${rows.length}. Keep collecting (window now runs to the force-exit deadline).`);
}

// ── Per-position detail (shared policy, current params) ─────────────────────
console.log('\n=== Per-position (current params, executable-bid replay) ===');
console.log('id        asset dir  entry  actual$  replay$  exit             hi%');
for (const r of rows) {
  const res = replay(r, BASE_CFG);
  console.log(
    `${String(r.id).padEnd(9)} ${String(r.asset).padEnd(5)} ${String(r.direction).padEnd(4)} ` +
    `${String(r.entryPrice).padStart(5)}  ${(r.actual?.netPnlUsd ?? 0).toFixed(2).padStart(6)}  ` +
    `${res.pnl.toFixed(2).padStart(6)}  ${res.exitReason.padEnd(14)} ${Math.round(res.highPct)}`
  );
}

console.log('\nNote: replay charges the TAKER fee on every exit. Live maker-first TP exits can only raise the result.');
