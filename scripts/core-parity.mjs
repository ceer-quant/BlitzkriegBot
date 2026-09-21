#!/usr/bin/env node
/**
 * Rust core P0 parity acceptance — drives the compiled core over UDS via the
 * bare-Node client (scripts/lib/core-client.mjs) and asserts the dry-mode
 * order/fill/ledger semantics:
 *
 *  - taker fills immediately and spends price*size
 *  - the three taker outcomes a dry FOK must tell apart: no opposing depth or
 *    depth outside the limit -> REJECTED (unfilled, unreserved), enough depth ->
 *    FILLED at the VWAP of the levels it walked
 *  - maker rests until the book crosses, then fills at its limit
 *  - maker_then_taker escalates to a taker after the timeout
 *  - cancel releases the reserved notional
 *  - risk (per-order cap) and ledger (insufficient funds) reject, structured
 *  - idempotent position effects arrive as typed FILL events
 *
 * It also states WHICH code it is testing (#172/#179) — the binary's embedded
 * revision must be the checkout's — and derives every fee it asserts from the
 * kernel's own `core.feeQuote`, checking that schedule against the pinned one
 * (#182). Both are here because this gate's most expensive historical failure
 * was a conclusion that silently described the wrong code state.
 *
 * Exit 0 only if every assertion passes. Requires `cargo build --release`.
 */

import { join } from 'path';
import { tmpdir } from 'os';
import { mkdtempSync } from 'fs';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { coreBinaryPath, checkCoreProvenance } from './lib/core-provenance.mjs';
import { describeQuote, feeModelProblems, feeQuoter, feeUsdFor } from './lib/fee-model.mjs';

const SOCK = scratchSocketPath('parity');
const BIN = coreBinaryPath();

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
  // roundSlot must describe a LIVE round: the position engine derives
  // expires_at_ms = (slot+1)*roundSec*1000, so the old fixed `1` was an expiry
  // in the deep past and any core tick between place and assertion force-exited
  // the position (the CI-only "positions.list []" failure). Derive the slot the
  // same way the backtest gate does.
  const liveSlot = Math.floor((Date.now() + 900_000) / 1000 / 900);
  return {
    tokenId, conditionId: 'cond', side: 'buy', mode, price, size,
    internalKey: key, strategy: 'spread_arb', asset, direction: 'up',
    roundSlot: liveSlot, ...extra,
  };
}

const makeCore = (socketPath, extraArgs = []) => new CoreClient({
  binaryPath: BIN,
  socketPath,
  mode: 'dry',
  seedBalance: 100,
  maxOrderNotional: 5,
  tickMs: 20,
  autoRestart: false,
  // Order-layer assertions must not be perturbed by the position/exit engine.
  cwd: WORKDIR,
  noTradeLog: true,     // these harnesses assert via events/positions, never the persisted ledger
  noOrderLog: true,     // and must not restore a prior harness's resting orders
  noPositionLog: true,  // nor its open positions (keeps co-located cores independent)
  extraArgs,
});

const fills = [];
const c = makeCore(SOCK, ['--no-auto-exits', '--max-positions', '99']);
c.onEvent = (e) => { if (e.kind === 'FILL') fills.push(e); };

/**
 * Taker fee in USD for one fill.
 *
 * The number comes from the KERNEL's own `core.feeQuote` — not from a formula
 * restated here (#182): a copy of the schedule cannot notice the kernel changing
 * its default, and a gate that keeps asserting the old arithmetic stops meaning
 * anything. `assertPinnedFeeModel` below is the other half: the kernel is also
 * required to report the model this repository pinned, so switching the default
 * without updating the gates is a RED gate rather than a quietly different
 * expectation.
 *
 * Maker fills are free, and the core charges taker fills on entry and exit alike
 * (`service.rs::apply_delta_effects`), so the cash ledger sits on the same
 * net-realized basis as the per-trade `netPnlUsd` views.
 */
const quoteFee = feeQuoter((method, params) => current.request(method, params));

// The core the fee quotes are taken from (assigned as soon as the first one is
// running — the quote is a fact about a running kernel, not about a file).
let current = null;

/** Assert the kernel reports the pinned schedule; print what it reports. */
function assertPinnedFeeModel(quote) {
  console.log(`  fee  ${describeQuote(quote)}`);
  const problems = feeModelProblems(quote);
  check('kernel fee model matches the pinned schedule (#182)', problems.length === 0, problems.join(' | '));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  checkCoreProvenance(BIN, check);
  await c.start();
  current = c;
  await rpc.ping(c);
  const ready = await rpc.ready(c);
  check('ready handshake reports dry core', ready.mode === 'dry' && /^\d+\.\d+\.\d+$/.test(ready.version), JSON.stringify(ready));
  // The revision the SERVING core reports, not the one on disk: this is the
  // statement that ties every assertion below to a commit (#172).
  console.log(`  core ${ready.build ?? '(no build field)'} commit=${ready.commit ?? '?'} dirty=${ready.dirty ?? '?'}`);
  check('serving core names its commit', typeof ready.commit === 'string' && ready.commit.length > 0, JSON.stringify(ready));

  const fee04 = await quoteFee(0.4);
  const fee05 = await quoteFee(0.5);
  assertPinnedFeeModel(fee04);

  // 1. Taker fills immediately, spends 0.4*5=2. A live FOK needs resting
  //    depth at or inside its limit, so the fixture mirrors an ask first.
  await rpc.bookSnapshot(c, 'tk1', [], [[0.40, 100]]);
  const taker = await rpc.placeOrder(c, order('taker', 'tk1', 0.4, 5, 'k1'));
  check('taker immediate FILLED', taker.status === 'FILLED', JSON.stringify(taker));

  // 2. Risk cap: 0.4*100=40 > 5 → structured rejection.
  let coreCode;
  try { await rpc.placeOrder(c, order('taker', 'tkX', 0.4, 100, 'kX')); }
  catch (e) { coreCode = e.coreCode; }
  check('risk cap rejects with RISK_REJECTED', coreCode === 'RISK_REJECTED', `got ${coreCode}`);

  // 3. Maker rests, fills when ask crosses; reserve held meanwhile.
  const maker = await rpc.placeOrder(c, order('maker', 'mk1', 0.4, 5, 'k2', 'ETH', { direction: 'down' }));
  check('maker starts LIVE', maker.status === 'LIVE', JSON.stringify(maker));
  // Spent so far: the taker fill's 0.4*5 = 2 notional plus its taker fee, and
  // the resting maker's 0.4*5 = 2 reservation.
  const balResting = await rpc.balance(c);
  const expectedAvailable = 100 - 2 - feeUsdFor(fee04, 5) - 2;
  check('reservation held while resting', balResting.available === expectedAvailable,
    `available ${balResting.available} != ${expectedAvailable}`);
  await rpc.bookSnapshot(c, 'mk1', [], [[0.45, 100]]);
  await sleep(60);
  check('maker still LIVE above the bid', (await rpc.listOrders(c)).orders.find((o) => o.internalKey === 'k2').status === 'LIVE');
  await rpc.bookSnapshot(c, 'mk1', [], [[0.40, 100]]);
  await sleep(80);
  check('maker FILLED on cross', (await rpc.listOrders(c)).orders.find((o) => o.internalKey === 'k2').status === 'FILLED');

  // 4. Cancel releases reservation.
  const resting = await rpc.placeOrder(c, order('maker', 'cx1', 0.3, 5, 'k3', 'SOL'));
  await rpc.cancelOrder(c, resting.orderId);
  const afterCancel = await rpc.listOrders(c);
  check('cancelled status', afterCancel.orders.find((o) => o.internalKey === 'k3').status === 'CANCELLED');

  // 5. maker_then_taker escalates after timeout.
  const mtt = await rpc.placeOrder(c, { ...order('maker_then_taker', 'mt1', 0.4, 5, 'k4', 'XRP'), makerTimeoutMs: 100 });
  check('maker_then_taker rests LIVE', mtt.status === 'LIVE');
  await rpc.bookSnapshot(c, 'mt1', [], [[0.5, 100]]); // no depth inside 0.40
  await sleep(260);
  const escalated = (await rpc.listOrders(c)).orders.some((o) => o.internalKey.endsWith(':escalated') && o.status === 'FILLED');
  check('maker_then_taker escalated to a filled taker', escalated);

  // Ledger final: taker 2 + crossed maker 2 + escalated taker 2.5 = 6.5 of
  // notional, and the cancelled order released its reservation. The escalated
  // leg takes the ask the book is actually offering (0.50), not the maker's
  // rejected passive limit — a live FOK reprices; the two taker legs carry a
  // fee at their own prices, the crossed one is a *maker* fill and is free.
  // Balance 100 - 6.5 - fee(0.4) - fee(0.5) at the kernel's own quoted rates.
  const bal = await rpc.balance(c);
  const expectedFinal = 100 - 6.5 - feeUsdFor(fee04, 5) - feeUsdFor(fee05, 5);
  check('final balance reflects 3 fills, net of taker fees', bal.balance === expectedFinal && bal.reserved === 0,
    `${JSON.stringify(bal)} != ${expectedFinal}`);

  // FILL events: taker + crossed maker + escalated taker = 3.
  check('three authoritative FILL events', fills.length === 3, `got ${fills.length}`);
  check('fill deltas are typed', fills.every((f) => typeof f.delta.delta === 'number' && typeof f.delta.price === 'number'));

  // ── 5b. The three taker semantics a dry FOK must tell apart (#172) ─────────
  // A taker is priced off the mirrored book, so exactly three outcomes exist and
  // each must be reached for its own reason. These are asserted together on
  // purpose: the gate's earlier failure was a taker rejected for having no book
  // while the gate believed it had proved a fill — the distinction is the gate's
  // subject, so it is stated explicitly rather than inferred from order counts.
  //
  // (a) nothing on the opposing side at ANY price: a live FOK would be killed
  //     with nothing touched. REJECTED — not a fill at the limit, not an error.
  const noBook = await rpc.placeOrder(c, order('taker', 'tk-nb', 0.4, 5, 'k-nb', 'LINK'));
  check('taker with no book on the opposing side -> REJECTED', noBook.status === 'REJECTED', JSON.stringify(noBook));

  // (b) depth exists but none of it is inside the limit (0.45 > our 0.40): the
  //     order may not lift a price worse than it states.
  await rpc.bookSnapshot(c, 'tk-out', [], [[0.45, 100]]);
  const outside = await rpc.placeOrder(c, order('taker', 'tk-out', 0.4, 5, 'k-out', 'SOL'));
  check('taker whose depth is all outside its limit -> REJECTED', outside.status === 'REJECTED', JSON.stringify(outside));

  // (c) enough depth inside the limit, but it takes TWO levels: the fill is the
  //     volume-weighted price of the walk (2@0.39 + 3@0.40 = 0.396), not the
  //     best price and not the limit — that is the number the ledger must use.
  await rpc.bookSnapshot(c, 'tk-walk', [], [[0.39, 2], [0.40, 3]]);
  const before = await rpc.balance(c);
  const walk = await rpc.placeOrder(c, order('taker', 'tk-walk', 0.4, 5, 'k-walk', 'DOT'));
  check('marketable taker fills across two levels', walk.status === 'FILLED', JSON.stringify(walk));
  // The event travels out-of-band (mpsc -> broadcast -> socket), so a fill is
  // allowed to arrive a moment after the response that announced it.
  const fillDeadline = Date.now() + 2000;
  let walkFill;
  while (Date.now() < fillDeadline) {
    walkFill = fills.find((f) => f.delta.orderId === walk.orderId);
    if (walkFill) break;
    await sleep(25);
  }
  const vwap = (0.39 * 2 + 0.40 * 3) / 5;
  check('the fill is the VWAP of the levels it walked', walkFill !== undefined && Math.abs(walkFill.delta.price - vwap) < 1e-9,
    `fill price ${walkFill?.delta.price} != vwap ${vwap}`);
  const feeWalker = feeUsdFor(await quoteFee(vwap), 5);
  const after = await rpc.balance(c);
  check('the ledger charges the walked notional and its fee',
    Math.abs((Number(before.balance) - Number(after.balance)) - (0.39 * 2 + 0.40 * 3 + feeWalker)) < 1e-9,
    `${before.balance} -> ${after.balance}, expected -${0.39 * 2 + 0.40 * 3 + feeWalker}`);

  // 6. Reconcile method is exposed and is a no-op for a snapshot that mentions
  //    no local order (the venue-id-driven repair itself is covered by Rust
  //    unit tests, since dry orders carry no venue id).
  const rec = await rpc.reconcile(c, { openOrderIds: [], trades: [] });
  check('reconcile returns a typed report with no actions', rec.filled === 0 && rec.markedFilled === 0 && rec.ghostIds.length === 0, JSON.stringify(rec));

  // 7. Kill switch blocks new orders immediately.
  await rpc.kill(c, 'parity test');
  let killedCode;
  try { await rpc.placeOrder(c, order('taker', 'tkK', 0.4, 5, 'kK')); }
  catch (e) { killedCode = e.coreCode; }
  check('kill switch blocks placement', killedCode === 'KILL_SWITCH_ACTIVE', `got ${killedCode}`);
  await rpc.resume(c);
} catch (e) {
  failures++;
  console.log('  FAIL harness error', e?.stack || e);
} finally {
  await c.stop();
}

// ── Position/exit layer (auto-exits ON) on a separate core instance ──────────
const POS_SOCK = scratchSocketPath('parity-pos');
const pc = makeCore(POS_SOCK);

try {
  await pc.start();
  // 8. A BUY fill opens a position that a profitable book closes on tick.
  //    The FOK entry needs depth: mirror an ask at the entry price first.
  await rpc.bookSnapshot(pc, 'pos1', [], [[0.40, 100]]);
  const opened = await rpc.placeOrder(pc, order('taker', 'pos1', 0.4, 5, 'kp1', 'ADA'));
  check('buy fills and opens a position', opened.status === 'FILLED');
  const posList = await rpc.positions(pc);
  check('positions.list shows the open position', posList.positions.length === 1 && posList.positions[0].asset === 'ADA', JSON.stringify(posList.positions));

  let positionClosed = null;
  pc.onEvent = (e) => { if (e.kind === 'POSITION_CLOSED') positionClosed = e; };
  await rpc.bookSnapshot(pc, 'pos1', [[0.99, 100]], [[1.0, 100]]);
  // The exit fires on the core's own tick loop, not on the book push — poll
  // briefly instead of trusting one fixed wait (a loaded runner stretches it).
  const closedDeadline = Date.now() + 5000;
  while (Date.now() < closedDeadline && positionClosed === null) await sleep(100);
  check('profit exit closes the position', (await rpc.positions(pc)).positions.length === 0);
  check('POSITION_CLOSED event carries realised PnL', positionClosed !== null && positionClosed.netPnlUsd > 0, JSON.stringify(positionClosed));

  // 9. Manual flatten via positions.exit.
  await rpc.bookSnapshot(pc, 'pos2', [], [[0.40, 100]]);
  await rpc.placeOrder(pc, order('taker', 'pos2', 0.4, 5, 'kp2', 'DOT'));
  const flat = await rpc.exitPositions(pc);
  check('manual flatten closes open positions', flat.closed === 1, JSON.stringify(flat));
} catch (e) {
  failures++;
  console.log('  FAIL position harness error', e?.stack || e);
} finally {
  await pc.stop();
}

// ── P3: self-driving engine (the harness feeds data, Rust decides and trades) ─
// `--no-discovery`: without it the polymarket discovery loop boots with the core
// and, wherever the venue is reachable (CI), its gamma query wins the race and
// re-registers the round with REAL token ids — the synthetic 'up'/'down' books
// then price nothing and no entry can ever be placed (this exact failure).
const ENG_SOCK = scratchSocketPath('parity-eng');
const ec = makeCore(ENG_SOCK, ['--engine', '--enable-strategy', 'spread_arb', '--no-discovery', '--no-event-archive', '--min-round-age', '0', '--min-time-left', '0', '--trend-confirm-sec', '0', '--trend-window-floor-ms', '0']);

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
  await rpc.setMarkets(ec, [market]);

  // Confirm the UP trend with a >=10s window of mid >= 0.5, then dip to 0.44.
  for (let i = 0; i < 12; i++) {
    await rpc.bookSnapshot(ec, 'up', [[0.55, 100]], [[0.57, 100]]);
    await sleep(12);
  }
  await rpc.spotPrice(ec, 'BTC', 60000);
  await rpc.bookSnapshot(ec, 'up', [[0.43, 100]], [[0.45, 100]]);
  await rpc.spotPrice(ec, 'BTC', 60000);

  // The engine evaluates on its own tick (an interval task, not the book
  // push), so poll for the entry instead of assuming one fixed wait is enough
  // — a loaded runner can stretch the first evaluate well past 400 ms.
  let orders = [];
  let entry;
  const deadline = Date.now() + 8000;
  while (Date.now() < deadline) {
    orders = (await rpc.listOrders(ec)).orders;
    entry = orders.find((o) => o.strategy === 'spread_arb');
    if (entry) break;
    await sleep(200);
  }
  if (!entry) {
    // Diagnose rather than just fail: show WHY nothing was placed.
    const stats = await ec.request('engine.stats', {}, 5000).catch((e) => ({ error: e }));
    console.log('  diag engine.stats:', JSON.stringify(stats).slice(0, 600));
  }
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
