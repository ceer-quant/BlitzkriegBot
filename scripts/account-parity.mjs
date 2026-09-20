#!/usr/bin/env node
/**
 * E17-f acceptance: the DRY and LIVE accounting paths must produce a
 * BIT-IDENTICAL ledger. This is the end-to-end form of the Rust unit test
 * `account_precision_tests::dry_and_live_ledgers_agree_bit_for_bit`, driven over
 * the real UDS wire against two real cores.
 *
 * The two producers of fills are different code paths that meet at
 * `Core::apply_delta_effects`:
 *
 *   DRY  — the core's own matcher (`sim.rs`) decides the fill and states its
 *          role directly: it either crossed a resting side or rested and was
 *          hit.
 *   LIVE — the venue reports the execution. Modelled here by pushing trades
 *          through `orders.reconcile`, which is the real live path for a WS gap
 *          and carries the venue's OWN maker/taker flag
 *          (`MarketFill::maker`, plumbed from Polymarket's `taker_order_id`
 *           vs `maker_orders[]`). Nothing is re-guessed from cumulative size.
 *
 * What is asserted, over all four entry-role x exit-role combinations:
 *
 *   1. the two cores end with the SAME cash balance, the same realized net, the
 *      same per-trade fees and the same maker/taker record — to the last digit;
 *   2. the E17 identity holds on BOTH: `balance == seed + sum(netPnlUsd)`;
 *   3. when the venue reports a role that CONTRADICTS the order's fill policy,
 *      the venue wins — the fee and `wasMakerEntry`/`wasMakerExit` follow the
 *      execution, not our intent. That is the defect E17 fixed: reading the
 *      requested policy instead of what actually happened.
 *
 * The gap between the two sides used to be 0.0817722 USD on the plan's fixture.
 * It must now be exactly 0.
 *
 * Everything runs in scratch dirs on private sockets, dry mode only, no network,
 * no production data. Live is never reachable from here.
 *
 * Usage: node scripts/account-parity.mjs   (needs target/release/blitzkrieg-core)
 */
import { join } from 'path';
import { tmpdir } from 'os';
import { mkdtempSync, existsSync } from 'fs';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const SEED = 1000;
const ENTRY_PX = 0.43;
const EXIT_PX = 0.95;
const SIZE = 10;

if (!existsSync(BIN)) {
  console.error(`missing binary: ${BIN} (cargo build --release --workspace)`);
  process.exit(2);
}

let failures = 0;
function check(name, cond, detail = '') {
  if (cond) console.log(`  ok   ${name}`);
  else { failures++; console.log(`  FAIL ${name} ${detail}`); }
}

/** Polkadot-scale rounding so a comparison cannot be won by float dust. */
const round = (n) => Math.round(n * 1e8) / 1e8;

/**
 * Taker fee in USD for one execution: `0.125 * (p*(1-p))^2 * shares`, the
 * Polymarket schedule mirrored by `exit_policy::taker_fee_pct`. Derived rather
 * than hard-coded so the assertion survives a fee-model change.
 */
const takerFeeUsd = (price, shares) => 0.125 * (price * (1 - price)) ** 2 * shares;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A core in an isolated scratch dir, with nothing persisted or restored. */
function makeCore(label) {
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(label),
    mode: 'dry',
    seedBalance: SEED,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd: mkdtempSync(join(tmpdir(), `blitzkrieg-acct-${label}-`)),
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs: ['--no-auto-exits', '--max-positions', '99'],
  });
}

const order = (side, mode, tokenId, price, size, key, asset) => ({
  tokenId, conditionId: `cond-${tokenId}`, side, mode, price, size,
  internalKey: key, strategy: 'acct', asset, direction: 'up', roundSlot: 1,
});

/** Poll `fn` until it is truthy, or give up (the tick loop is asynchronous). */
async function until(fn, ms = 2000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await fn()) return true;
    await sleep(25);
  }
  return false;
}

/** Everything we compare, read from one core over the wire. */
async function snapshot(c) {
  const bal = await rpc.balance(c);
  const trades = (await rpc.tradesHistory(c, 50)).trades.map((t) => ({
    asset: t.asset,
    netPnlUsd: t.netPnlUsd,
    feesUsd: t.feesUsd,
    grossPnlUsd: t.grossPnlUsd,
    wasMakerEntry: t.wasMakerEntry,
    wasMakerExit: t.wasMakerExit,
    shares: t.shares,
  })).sort((a, b) => (a.asset < b.asset ? -1 : 1));
  return {
    balance: bal.balance,
    reserved: bal.reserved,
    trades,
    realizedNet: trades.reduce((s, t) => s + t.netPnlUsd, 0),
    fees: trades.reduce((s, t) => s + t.feesUsd, 0),
  };
}

/** The trade this combination produced. */
const tradeFor = (s, asset) => s.trades.find((t) => t.asset === asset);

/**
 * Float transport tolerance. The identity `balance == seed + Σ net` is EXACT in
 * the core's `Decimal`; what is compared here is the f64 rendering of that
 * balance against a client-side f64 summation of the same per-trade numbers, so
 * the last bit or two can differ. 1e-9 is deliberately ~26,000x tighter than the
 * 0.0817722 gap this gate exists to catch, so it cannot mask a real drift.
 */
const IDENTITY_TOL = 1e-9;
const near = (a, b) => Math.abs(a - b) <= IDENTITY_TOL;
const resid = (a, b) => Math.abs(a - b).toExponential(2);

/**
 * One round trip on the DRY-matcher core: the core itself decides each fill.
 * A taker order crosses at submit; a maker order rests until the book crosses
 * it. `makerAlways` shapes the book so the outcome is deterministic.
 */
async function dryRoundTrip(c, asset, token, entryRole, exitRole) {
  const entryPrice = ENTRY_PX;
  const exitPrice = EXIT_PX;

  if (entryRole === 'taker') {
    // The honest FOK fills at the WALKED ask, so the ask must rest at the
    // entry price — the walk VWAP then equals the fixture fill price and
    // both cores book the identical money.
    await rpc.bookSnapshot(c, token, [], [[entryPrice, 500]]);
    await rpc.placeOrder(c, order('buy', 'taker', token, entryPrice, SIZE, `e-${token}`, asset));
  } else {
    // Rest far from the book, then let the book cross it.
    await rpc.placeOrder(c, order('buy', 'maker', token, entryPrice, SIZE, `e-${token}`, asset));
    await rpc.bookSnapshot(c, token, [[entryPrice - 0.05, 500]], [[entryPrice + 0.05, 500]]);
    await sleep(40);
    await rpc.bookSnapshot(c, token, [], [[entryPrice, 500]]);
    const ok = await until(async () => (await rpc.positions(c)).positions.length > 0);
    if (!ok) throw new Error(`dry ${asset}: maker entry never filled`);
  }

  if (exitRole === 'taker') {
    // Symmetric: the FOK sell walks the resting bids, so the bid must sit
    // at the exit price.
    await rpc.bookSnapshot(c, token, [[exitPrice, 500]], []);
    await rpc.placeOrder(c, order('sell', 'taker', token, exitPrice, SIZE, `x-${token}`, asset));
  } else {
    await rpc.placeOrder(c, order('sell', 'maker', token, exitPrice, SIZE, `x-${token}`, asset));
    await rpc.bookSnapshot(c, token, [[exitPrice - 0.05, 500]], [[exitPrice + 0.05, 500]]);
    await sleep(40);
    await rpc.bookSnapshot(c, token, [[exitPrice, 500]], []);
  }
  await until(async () => (await rpc.positions(c)).positions.length === 0);
}

/**
 * The same round trip on the VENUE-ingest core: every order rests, and the
 * executions arrive as venue trade reports carrying their own maker/taker flag.
 * This is byte-for-byte what a live WS gap repair does.
 */
async function liveRoundTrip(c, asset, token, entryRole, exitRole) {
  const entryPrice = ENTRY_PX;
  const exitPrice = EXIT_PX;

  // The order is submitted with a policy that may well CONTRADICT the venue's
  // report — deliberately, so this also proves the report outranks the policy.
  const entry = await rpc.placeOrder(c, order('buy', 'maker', token, entryPrice, SIZE, `e-${token}`, asset));
  await rpc.reconcile(c, {
    openOrderIds: [entry.orderId],
    trades: [{
      venueOrderId: entry.orderId,
      tradeId: `v-e-${token}`,
      tokenId: token,
      side: 'buy',
      size: SIZE,
      price: entryPrice,
      tsMs: Date.now(),
      maker: entryRole === 'maker',
    }],
  });
  const opened = await until(async () => (await rpc.positions(c)).positions.length > 0);
  if (!opened) throw new Error(`live ${asset}: entry gap fill never applied`);

  const exit = await rpc.placeOrder(c, order('sell', 'maker', token, exitPrice, SIZE, `x-${token}`, asset));
  await rpc.reconcile(c, {
    openOrderIds: [exit.orderId],
    trades: [{
      venueOrderId: exit.orderId,
      tradeId: `v-x-${token}`,
      tokenId: token,
      side: 'sell',
      size: SIZE,
      price: exitPrice,
      tsMs: Date.now(),
      maker: exitRole === 'maker',
    }],
  });
  await until(async () => (await rpc.positions(c)).positions.length === 0);
}

// ── The matrix: every entry-role x exit-role combination ─────────────────────
const COMBOS = [
  { entry: 'taker', exit: 'taker', asset: 'BTC', token: 'tok-tt' },
  { entry: 'taker', exit: 'maker', asset: 'ETH', token: 'tok-tm' },
  { entry: 'maker', exit: 'taker', asset: 'SOL', token: 'tok-mt' },
  { entry: 'maker', exit: 'maker', asset: 'XRP', token: 'tok-mm' },
];

const dryCore = makeCore('acct-dry');
const liveCore = makeCore('acct-live');
const done = [];

/** The cash one round trip must move, derived so it is never a magic number. */
function expectedNet(entryRole, exitRole) {
  const entryFee = entryRole === 'taker' ? takerFeeUsd(ENTRY_PX, SIZE) : 0;
  const exitFee = exitRole === 'taker' ? takerFeeUsd(EXIT_PX, SIZE) : 0;
  return (EXIT_PX - ENTRY_PX) * SIZE - entryFee - exitFee;
}

const expectedFees = (entryRole, exitRole) =>
  (entryRole === 'taker' ? takerFeeUsd(ENTRY_PX, SIZE) : 0) +
  (exitRole === 'taker' ? takerFeeUsd(EXIT_PX, SIZE) : 0);

try {
  await dryCore.start();
  await liveCore.start();

  for (const combo of COMBOS) {
    const label = `${combo.entry}→${combo.exit}`;
    try {
      await dryRoundTrip(dryCore, combo.asset, combo.token, combo.entry, combo.exit);
      await liveRoundTrip(liveCore, combo.asset, combo.token, combo.entry, combo.exit);
    } catch (e) {
      failures++;
      console.log(`  FAIL ${label} harness error ${e?.message || e}`);
      continue;
    }

    const dry = await snapshot(dryCore);
    const live = await snapshot(liveCore);
    const dTrade = tradeFor(dry, combo.asset);
    const lTrade = tradeFor(live, combo.asset);
    const net = expectedNet(combo.entry, combo.exit);
    const fees = expectedFees(combo.entry, combo.exit);

    if (!dTrade || !lTrade) {
      failures++;
      console.log(`  FAIL ${label} no trade record for ${combo.asset}`);
      continue;
    }

    // 1. Bit-for-bit parity between the two fill producers, per combination.
    check(`[${label}] the trade records agree exactly`,
      JSON.stringify(dTrade) === JSON.stringify(lTrade),
      `\n        dry  ${JSON.stringify(dTrade)}\n        live ${JSON.stringify(lTrade)}`);

    // 2. The role that ACTUALLY happened is what got recorded (E17-b/c). The
    //    venue-side order is always submitted as `maker`, so a `taker` verdict
    //    proves the reported execution overrode our stated intent, and vice versa.
    for (const [name, t] of [['dry', dTrade], ['live', lTrade]]) {
      check(`[${label}] ${name}: entry role is what the fill did`,
        t.wasMakerEntry === (combo.entry === 'maker'),
        `wasMakerEntry ${t.wasMakerEntry}, execution was ${combo.entry}`);
      check(`[${label}] ${name}: exit role is what the fill did`,
        t.wasMakerExit === (combo.exit === 'maker'),
        `wasMakerExit ${t.wasMakerExit}, execution was ${combo.exit}`);
      check(`[${label}] ${name}: shares exact on the grid`, t.shares === SIZE, `shares ${t.shares}`);
      check(`[${label}] ${name}: charges match the execution`,
        near(t.feesUsd, fees), `fees ${t.feesUsd} != ${fees}`);
      check(`[${label}] ${name}: net is gross minus those charges`,
        near(t.netPnlUsd, net), `net ${t.netPnlUsd} != ${net}`);
    }

    // 3. The E17 identity, cumulative over everything the core has done:
    //    balance == seed + Σ net, with nothing left reserved.
    for (const [name, s] of [['dry', dry], ['live', live]]) {
      check(`[${label}] ${name}: balance == seed + Σ realized net (residual ${resid(s.balance, SEED + s.realizedNet)})`,
        near(s.balance, SEED + s.realizedNet),
        `balance ${s.balance} vs ${SEED + s.realizedNet}`);
      check(`[${label}] ${name}: nothing left reserved`, s.reserved === 0, `reserved ${s.reserved}`);
    }

    done.push({ label, net, fees });
  }

  // 4. The whole session's cash, derived from the combinations rather than read
  //    back from the core: this is the "a model of the fixture reproduces the
  //    ledger" check, and it fails loudly if a fee is charged twice or skipped.
  const expectedTotal = SEED + done.reduce((s, d) => s + d.net, 0);
  for (const [name, core] of [['dry', dryCore], ['live', liveCore]]) {
    const s = await snapshot(core);
    check(`session ${name}: cash equals the derived total (residual ${resid(s.balance, expectedTotal)})`,
      near(s.balance, expectedTotal),
      `got ${s.balance}, derived ${expectedTotal}`);
    check(`session ${name}: every combination is on the book`, s.trades.length === COMBOS.length,
      `${s.trades.length} trades`);
  }

  // ── The plan's fixture, end to end: a MakerThenTaker entry that rests, is hit,
  // and exits at a profit as a maker. This is the exact scenario that measured a
  // 0.0817722 USD gap before E17.
  {
    const c = makeCore('acct-fixture');
    try {
      await c.start();
      const token = 'tok-mtt';
      const entry = await rpc.placeOrder(c, {
        ...order('buy', 'maker_then_taker', token, ENTRY_PX, SIZE, 'e-mtt', 'LINK'),
        makerTimeoutMs: 60_000,
      });
      await rpc.reconcile(c, {
        openOrderIds: [entry.orderId],
        trades: [{
          venueOrderId: entry.orderId, tradeId: 'v-mtt-e', tokenId: token,
          side: 'buy', size: SIZE, price: ENTRY_PX, tsMs: Date.now(),
          maker: true, // it rested at the bid and was hit
        }],
      });
      await until(async () => (await rpc.positions(c)).positions.length > 0);
      const exit = await rpc.placeOrder(
        c,
        order('sell', 'maker', token, EXIT_PX, SIZE, 'x-mtt', 'LINK'),
      );
      await rpc.reconcile(c, {
        openOrderIds: [exit.orderId],
        trades: [{
          venueOrderId: exit.orderId, tradeId: 'v-mtt-x', tokenId: token,
          side: 'sell', size: SIZE, price: EXIT_PX, tsMs: Date.now(),
          maker: true,
        }],
      });
      await until(async () => (await rpc.positions(c)).positions.length === 0);

      const s = await snapshot(c);
      const gross = (EXIT_PX - ENTRY_PX) * SIZE;
      const t = s.trades[0];
      check('fixture: both legs were makers, so nothing was charged', t.feesUsd === 0, `fees ${t.feesUsd}`);
      check('fixture: cash moved by the gross profit alone',
        near(s.balance, SEED + gross), `${s.balance} != ${SEED + gross}`);
      check('fixture: balance == seed + realized net (the old gap was 0.0817722)',
        near(s.balance, SEED + s.realizedNet),
        `balance ${s.balance} != ${SEED + s.realizedNet}`);
      check('fixture: the trade records the gross, un-fee\'d 5.20',
        t.netPnlUsd === gross, JSON.stringify(t));
      check('fixture: the escalation did not turn it into a taker',
        t.wasMakerEntry === true && t.wasMakerExit === true, JSON.stringify(t));
    } catch (e) {
      failures++;
      console.log(`  FAIL fixture harness error ${e?.stack || e}`);
    } finally {
      await c.stop();
    }
  }
} catch (e) {
  failures++;
  console.log('  FAIL harness error', e?.stack || e);
} finally {
  await dryCore.stop();
  await liveCore.stop();
}

console.log(failures === 0
  ? '\naccount:parity — dry and live ledgers are bit-identical.'
  : `\naccount:parity — ${failures} assertion(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
