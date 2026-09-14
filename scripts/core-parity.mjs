#!/usr/bin/env node
/**
 * Rust core P0 parity acceptance — drives the compiled core over UDS via the
 * real Node BlitzkriegCoreClient (the exact path production will use) and asserts the
 * dry-mode order/fill/ledger semantics the Node engine previously owned:
 *
 *  - taker fills immediately and spends price*size
 *  - maker rests until the book crosses, then fills at its limit
 *  - maker_then_taker escalates to a taker after the timeout
 *  - cancel releases the reserved notional
 *  - risk (per-order cap) and ledger (insufficient funds) reject, structured
 *  - idempotent position effects arrive as typed FILL events
 *
 * Exit 0 only if every assertion passes. Requires `cargo build --release`.
 */

import { join } from 'path';
import { tmpdir } from 'os';
import { mkdtempSync } from 'fs';
import { BlitzkriegCoreClient } from '../dist/core/blitzkrieg-core-client.js';
import { scratchSocketPath } from './lib/core-socket.mjs';

const SOCK = scratchSocketPath('parity');
const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');

// The core persists its trade log at a RELATIVE path, so every synthetic order
// this harness places would be appended to the real data/trades/trades.jsonl.
// Run each core in a throwaway working directory to keep test data out of prod.
const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-parity-'));

let failures = 0;
function check(name, cond, detail = '') {
  if (cond) console.log(`  ok   ${name}`);
  else { failures++; console.log(`  FAIL ${name} ${detail}`); }
}

function order(mode, tokenId, price, size, key, asset = 'BTC', extra = {}) {
  return {
    tokenId, conditionId: 'cond', side: 'buy', mode, price, size,
    internalKey: key, strategy: 'spread_arb', asset, direction: 'up', roundSlot: 1, ...extra,
  };
}

const c = new BlitzkriegCoreClient({
  binaryPath: BIN,
  socketPath: SOCK,
  mode: 'dry',
  seedBalance: 100,
  maxOrderNotional: 5,
  tickMs: 20,
  autoRestart: false,
  // Order-layer assertions must not be perturbed by the position/exit engine.
  cwd: WORKDIR,
    noTradeLog: true,  // these harnesses assert via events/positions, never the persisted ledger
    noOrderLog: true,  // and must not restore a prior harness's resting orders
    noPositionLog: true,  // nor its open positions (keeps co-located cores independent)
  extraArgs: ['--no-auto-exits', '--max-positions', '99'],
});

const fills = [];
c.on('fill', (e) => fills.push(e));

try {
  await c.start();
  await c.ping();
  const ready = await c.ready();
  check('ready handshake reports dry core', ready.mode === 'dry' && /^\d+\.\d+\.\d+$/.test(ready.version), JSON.stringify(ready));

  // 1. Taker fills immediately, spends 0.4*5=2.
  const taker = await c.placeOrder(order('taker', 'tk1', 0.4, 5, 'k1'));
  check('taker immediate FILLED', taker.status === 'FILLED', JSON.stringify(taker));

  // 2. Risk cap: 0.4*100=40 > 5 → structured rejection.
  let coreCode;
  try { await c.placeOrder(order('taker', 'tkX', 0.4, 100, 'kX')); }
  catch (e) { coreCode = e.coreCode; }
  check('risk cap rejects with RISK_REJECTED', coreCode === 'RISK_REJECTED', `got ${coreCode}`);

  // 3. Maker rests, fills when ask crosses; reserve held meanwhile.
  const maker = await c.placeOrder(order('maker', 'mk1', 0.4, 5, 'k2', 'ETH', { direction: 'down' }));
  check('maker starts LIVE', maker.status === 'LIVE', JSON.stringify(maker));
  check('reservation held while resting', (await c.balance()).available === 96, 'available wrong');
  await c.bookSnapshot('mk1', [], [[0.45, 100]]);
  await new Promise((r) => setTimeout(r, 60));
  check('maker still LIVE above the bid', (await c.listOrders()).orders.find((o) => o.internalKey === 'k2').status === 'LIVE');
  await c.bookSnapshot('mk1', [], [[0.40, 100]]);
  await new Promise((r) => setTimeout(r, 80));
  check('maker FILLED on cross', (await c.listOrders()).orders.find((o) => o.internalKey === 'k2').status === 'FILLED');

  // 4. Cancel releases reservation.
  const resting = await c.placeOrder(order('maker', 'cx1', 0.3, 5, 'k3', 'SOL'));
  await c.cancelOrder(resting.orderId);
  const afterCancel = await c.listOrders();
  check('cancelled status', afterCancel.orders.find((o) => o.internalKey === 'k3').status === 'CANCELLED');

  // 5. maker_then_taker escalates after timeout.
  const mtt = await c.placeOrder({ ...order('maker_then_taker', 'mt1', 0.4, 5, 'k4', 'XRP'), makerTimeoutMs: 100 });
  check('maker_then_taker rests LIVE', mtt.status === 'LIVE');
  await c.bookSnapshot('mt1', [], [[0.5, 100]]); // never crosses
  await new Promise((r) => setTimeout(r, 260));
  const escalated = (await c.listOrders()).orders.some((o) => o.internalKey.endsWith(':escalated') && o.status === 'FILLED');
  check('maker_then_taker escalated to a filled taker', escalated);

  // Ledger final: taker 2 + crossed maker 2 + escalated taker 2 = 6 spent;
  // cancelled order released its reservation. Balance 100-6 = 94.
  const bal = await c.balance();
  check('final balance reflects 3 fills (94)', bal.balance === 94 && bal.reserved === 0, JSON.stringify(bal));

  // FILL events: taker + crossed maker + escalated taker = 3.
  check('three authoritative FILL events', fills.length === 3, `got ${fills.length}`);
  check('fill deltas are typed', fills.every((f) => typeof f.delta.delta === 'number' && typeof f.delta.price === 'number'));

  // 6. Reconcile method is exposed and is a no-op for a snapshot that mentions
  //    no local order (the venue-id-driven repair itself is covered by Rust
  //    unit tests, since dry orders carry no venue id).
  const rec = await c.reconcile({ openOrderIds: [], trades: [] });
  check('reconcile returns a typed report with no actions', rec.filled === 0 && rec.markedFilled === 0 && rec.ghostIds.length === 0, JSON.stringify(rec));

  // 7. Kill switch blocks new orders immediately.
  await c.kill('parity test');
  let killedCode;
  try { await c.placeOrder(order('taker', 'tkK', 0.4, 5, 'kK')); }
  catch (e) { killedCode = e.coreCode; }
  check('kill switch blocks placement', killedCode === 'KILL_SWITCH_ACTIVE', `got ${killedCode}`);
  await c.resume();
} catch (e) {
  failures++;
  console.log('  FAIL harness error', e?.stack || e);
} finally {
  await c.stop();
}

// ── Position/exit layer (auto-exits ON) on a separate core instance ──────────
const POS_SOCK = scratchSocketPath('parity-pos');
const pc = new BlitzkriegCoreClient({
  binaryPath: BIN,
  socketPath: POS_SOCK,
  mode: 'dry',
  seedBalance: 100,
  maxOrderNotional: 5,
  tickMs: 20,
  autoRestart: false,
  cwd: WORKDIR,
    noTradeLog: true,  // these harnesses assert via events/positions, never the persisted ledger
    noOrderLog: true,  // and must not restore a prior harness's resting orders
    noPositionLog: true,  // nor its open positions (keeps co-located cores independent)
});

try {
  await pc.start();
  // 8. A BUY fill opens a position that a profitable book closes on tick.
  const opened = await pc.placeOrder(order('taker', 'pos1', 0.4, 5, 'kp1', 'ADA'));
  check('buy fills and opens a position', opened.status === 'FILLED');
  const posList = await pc.positions();
  check('positions.list shows the open position', posList.positions.length === 1 && posList.positions[0].asset === 'ADA', JSON.stringify(posList.positions));

  let positionClosed = null;
  pc.on('event', (e) => { if (e.kind === 'POSITION_CLOSED') positionClosed = e; });
  await pc.bookSnapshot('pos1', [[0.99, 100]], [[1.0, 100]]);
  await new Promise((r) => setTimeout(r, 250));
  check('profit exit closes the position', (await pc.positions()).positions.length === 0);
  check('POSITION_CLOSED event carries realised PnL', positionClosed !== null && positionClosed.netPnlUsd > 0, JSON.stringify(positionClosed));

  // 9. Manual flatten via positions.exit.
  await pc.placeOrder(order('taker', 'pos2', 0.4, 5, 'kp2', 'DOT'));
  const flat = await pc.exitPositions();
  check('manual flatten closes open positions', flat.closed === 1, JSON.stringify(flat));
} catch (e) {
  failures++;
  console.log('  FAIL position harness error', e?.stack || e);
} finally {
  await pc.stop();
}

// ── P3: self-driving engine (Node feeds data, Rust decides and trades) ────────
const ENG_SOCK = scratchSocketPath('parity-eng');
const ec = new BlitzkriegCoreClient({
  binaryPath: BIN,
  socketPath: ENG_SOCK,
  mode: 'dry',
  seedBalance: 1000,
  maxOrderNotional: 5,
  tickMs: 20,
  autoRestart: false,
  cwd: WORKDIR,
    noTradeLog: true,  // these harnesses assert via events/positions, never the persisted ledger
    noOrderLog: true,  // and must not restore a prior harness's resting orders
    noPositionLog: true,  // nor its open positions (keeps co-located cores independent)
  extraArgs: ['--engine', '--no-event-archive', '--min-round-age', '0', '--min-time-left', '0', '--trend-confirm-sec', '0', '--trend-window-floor-ms', '0'],
});

try {
  await ec.start();
  const startMs = Date.now();
  const endMs = startMs + 900_000;
  const slot = Math.floor(endMs / 1000 / 900);
  const market = {
    asset: 'BTC', conditionId: 'c', questionId: 'q',
    upTokenId: 'up', downTokenId: 'down',
    upPrice: 0.6, downPrice: 0.4,
    expiresAtMs: endMs, roundSlot: slot, negRisk: true, question: 'BTC up?',
  };
  await ec.setMarkets([market]);

  // Confirm the UP trend with a >=10s window of mid >= 0.5, then dip to 0.44.
  for (let i = 0; i < 12; i++) {
    await ec.bookSnapshot('up', [[0.55, 100]], [[0.57, 100]]);
    await new Promise((r) => setTimeout(r, 12));
  }
  await ec.spotPrice('BTC', 60000);
  await ec.bookSnapshot('up', [[0.43, 100]], [[0.45, 100]]);
  await ec.spotPrice('BTC', 60000);

  // The engine evaluates on its own tick; wait for it to place + fill.
  await new Promise((r) => setTimeout(r, 400));
  const orders = (await ec.listOrders()).orders;
  const entry = orders.find((o) => o.strategy === 'spread_arb');
  check('engine placed an entry from fed data', entry !== undefined, JSON.stringify(orders.map((o) => o.status)));
  check('engine entry is trend-confirmed maker_then_taker', entry !== undefined && entry.side === 'buy' && entry.mode === 'maker_then_taker');
} catch (e) {
  failures++;
  console.log('  FAIL engine harness error', e?.stack || e);
} finally {
  await ec.stop();
}

console.log(failures === 0 ? '\nRUST CORE PARITY OK' : `\nRUST CORE PARITY FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);