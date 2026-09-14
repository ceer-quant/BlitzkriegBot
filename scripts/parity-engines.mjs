#!/usr/bin/env node
/**
 * Engine parity harness — drive the Node decision pipeline and the real Rust
 * engine with the SAME market data and compare their decisions, without placing
 * any real orders (both sides are DRY).
 *
 * Why not the full Node engine: `evaluateAll` is internal and `start()` wires a
 * live Binance/Poly feed + Gamma scanner, so the Node engine is not headless-
 * drivable. The harness therefore drives the exact modules the Node engine's
 * `evaluateAll` calls for spread_arb — `createTrendTracker` + `evaluateSpreadArb`
 * (imported from dist) — and compares against the Rust engine running
 * end-to-end (it consumes the same events over UDS and places DRY orders).
 *
 * Comparison points: token, direction, resting-bid price.
 *
 * Usage: node scripts/parity-engines.mjs   (requires `npm run core:build` first)
 */

import { createRequire } from 'module';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';
import { mkdtempSync } from 'fs';
import { BlitzkriegCoreClient } from '../dist/core/blitzkrieg-core-client.js';

// Throwaway working dir: the core writes its trade log at a relative path, so
// this keeps the harness's synthetic orders out of data/trades/trades.jsonl.
const WORKDIR = mkdtempSync(join(tmpdir(), 'clodds-parity-eng-'));

const require = createRequire(import.meta.url);
const { createTrendTracker } = require('../dist/strategies/crypto-hft/trend-tracker.js');
const { evaluateSpreadArb, DEFAULT_SPREAD_ARB } = require('../dist/strategies/crypto-hft/strategies.js');

const __dirname = dirname(fileURLToPath(import.meta.url));
const BIN = join(__dirname, '..', 'target', 'release', 'blitzkrieg-core');

// ── Shared scenario ──────────────────────────────────────────────────────────
// Window must be >= 10s because the Node TrendTracker floors it at 10s; the Rust
// scenario therefore runs ~11s of wall clock so its (real-time) window matches.

const TREND_MIN_PRICE = 0.55;
const TREND_CONFIRM_SEC = 10;
const TREND_RATIO = 0.8;
const TREND_BROKEN = 0.35;
const TREND_ENTRY_FACTOR = 0.98;
const TREND_MAX_ENTRY = 0.45;

const WINDOW_MS = TREND_CONFIRM_SEC * 1000;
const STEP_MS = 1000; // 1s granularity: after the 10s window prunes the oldest
                      // samples the retained span is a solid 10s ≥ 0.9*window.
const HOLD_STEPS_END = WINDOW_MS + 2000; // run to 12s for pruning margin

const UP = 'UP_TOKEN';
const DOWN = 'DOWN_TOKEN';
const MARKET = {
  asset: 'BTC',
  conditionId: 'COND',
  questionId: 'Q',
  upTokenId: UP,
  downTokenId: DOWN,
  upPrice: 0.56,
  downPrice: 0.44,
  negRisk: true,
  question: 'BTC up or down',
};

// Book sequence: hold the UP trend above the threshold, then dip.
const HOLD = { bid: 0.55, ask: 0.57, mid: 0.56 };
const DIP = { bid: 0.43, ask: 0.45, mid: 0.44 }; // entry = round2(0.44*0.98)=0.43 (== bestBid)

function nodeCfg() {
  return {
    trendConfirmSec: TREND_CONFIRM_SEC,
    trendMinPrice: TREND_MIN_PRICE,
    trendBrokenPrice: TREND_BROKEN,
    trendRatio: TREND_RATIO,
  };
}

function bookObj(t) {
  return {
    tokenId: t,
    bids: [[HOLD.bid, 100]],
    asks: [[HOLD.ask, 100]],
    bidDepth: 100,
    askDepth: 100,
    obi: 0,
    spread: HOLD.ask - HOLD.bid,
    spreadPct: 0,
    bestBid: HOLD.bid,
    bestAsk: HOLD.ask,
    midPrice: HOLD.mid,
    timestamp: 0,
  };
}

// ── Node side: drive the decision pipeline with injected timestamps ──────────
function nodeDecision() {
  const tracker = createTrendTracker(nodeCfg);
  // Build the trend with a 0..10s span (injected clock).
  for (let t = 0; t <= HOLD_STEPS_END; t += STEP_MS) {
    tracker.onPrice(UP, HOLD.mid, t);
    tracker.onPrice(DOWN, 0.30, t); // below threshold → never confirms
  }
  const confirmed = tracker.confirmedTokens();

  // Dip book for UP.
  const dipBook = {
    tokenId: UP,
    bids: [[DIP.bid, 100]],
    asks: [[DIP.ask, 100]],
    bidDepth: 100,
    askDepth: 100,
    obi: 0,
    spread: DIP.ask - DIP.bid,
    spreadPct: 0,
    bestBid: DIP.bid,
    bestAsk: DIP.ask,
    midPrice: DIP.mid,
    timestamp: 0,
  };
  const cfg = {
    ...DEFAULT_SPREAD_ARB,
    trendMinPrice: TREND_MIN_PRICE,
    trendConfirmSec: TREND_CONFIRM_SEC,
    trendBrokenPrice: TREND_BROKEN,
    trendRatio: TREND_RATIO,
    trendEntryFactor: TREND_ENTRY_FACTOR,
    trendMaxEntryPrice: TREND_MAX_ENTRY,
    trendEntryPrice: 0,
  };
  // polyBuffer isn't consulted by spread_arb's decision; pass a minimal stub.
  const polyBuf = { mean: () => 0, range: () => 0, movePct: () => 0, reversals: () => 0, prices: [] };
  const sig = evaluateSpreadArb(
    MARKET,
    0, // spotMovePct (flat)
    polyBuf,
    { up: dipBook, down: null },
    cfg,
    confirmed
  );
  return { confirmed: [...confirmed], signal: sig ? { tokenId: sig.tokenId, direction: sig.direction, price: sig.price } : null };
}

// ── Rust side: real engine over UDS ──────────────────────────────────────────
async function rustDecision() {
  const sock = join(tmpdir(), `clodds-parity-engines-${process.pid}.sock`);
  const client = new BlitzkriegCoreClient({
    binaryPath: BIN,
    socketPath: sock,
    mode: 'dry',
    seedBalance: 1000,
    maxOrderNotional: 5,
    tickMs: 20,
    autoRestart: false,
    cwd: WORKDIR,
    noTradeLog: true,  // these harnesses assert via events/positions, never the persisted ledger
    noOrderLog: true,  // keep recovery files of co-located cores from leaking across
    noPositionLog: true,
    // Match the Node timing gates + trend window; disable exits so no SELL noise.
    // `--no-discovery` is required: this harness injects one synthetic market via
    // `engine.markets`, and Rust-native round discovery would otherwise overwrite
    // it with a real round, making the comparison non-deterministic.
    extraArgs: [
      '--engine', '--no-auto-exits', '--no-discovery',
      '--min-round-age', '0', '--min-time-left', '0',
      '--trend-confirm-sec', String(TREND_CONFIRM_SEC),
      '--trend-window-floor-ms', String(WINDOW_MS),
      '--max-positions', '5',
    ],
  });
  const ordersSeen = [];
  client.on('event', (e) => { if (e.kind === 'ORDER_UPDATE') ordersSeen.push(e.order); });

  await client.start();
  const now = Date.now();
  const endMs = now + 900_000;
  await client.setMarkets([{ ...MARKET, expiresAtMs: endMs, roundSlot: Math.floor(endMs / 1000 / 900), negRisk: true }]);

  // Build the trend in real wall-clock over ~10s.
  for (let t = 0; t <= HOLD_STEPS_END; t += STEP_MS) {
    await client.bookSnapshot(UP, [[HOLD.bid, 100]], [[HOLD.ask, 100]]);
    await client.bookSnapshot(DOWN, [[0.29, 100]], [[0.31, 100]]);
    await client.spotPrice('BTC', 60000);
    if (t < HOLD_STEPS_END) await new Promise((r) => setTimeout(r, STEP_MS));
  }
  // Dip.
  await client.bookSnapshot(UP, [[DIP.bid, 100]], [[DIP.ask, 100]]);
  await client.spotPrice('BTC', 60000);
  await new Promise((r) => setTimeout(r, 300)); // let the engine tick

  const { orders } = await client.listOrders();
  await client.stop();
  const entry = orders.find((o) => o.strategy === 'spread_arb' && o.side === 'buy') ?? null;
  return {
    signal: entry ? { tokenId: entry.tokenId, direction: entry.direction, price: Number(entry.price) } : null,
    allOrders: orders.map((o) => ({ t: o.tokenId, side: o.side, price: Number(o.price), status: o.status })),
  };
}

// ── Run + compare ────────────────────────────────────────────────────────────
(async () => {
  console.log('Engine parity: Node decision pipeline vs Rust engine (DRY, same data)\n');
  const n = nodeDecision();
  console.log('Node confirmed tokens :', JSON.stringify(n.confirmed));
  console.log('Node signal           :', JSON.stringify(n.signal));

  const r = await rustDecision();
  console.log('Rust signal           :', JSON.stringify(r.signal));
  console.log('Rust orders           :', JSON.stringify(r.allOrders));

  const same = JSON.stringify(n.signal) === JSON.stringify(r.signal);
  console.log('\n--- verdict ---');
  if (n.signal && r.signal && same) {
    console.log('PARITY OK: identical token/direction/price');
    process.exit(0);
  } else if (!n.signal && !r.signal) {
    console.log('PARITY OK (both: no signal)');
    process.exit(0);
  } else {
    console.log('PARITY MISMATCH');
    process.exit(1);
  }
})().catch((e) => { console.error('harness error', e); process.exit(1); });
