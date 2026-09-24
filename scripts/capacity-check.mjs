#!/usr/bin/env node
/**
 * Capacity and self-impact (#192).
 *
 * The system has no answer to "how much money can this carry?" — a strategy that
 * looks profitable at 4.8 USDC can be pure self-impact at 480, and the failure is
 * invisible because it looks like alpha: the fill is real, the price walked away
 * from the one the signal was based on, and the PnL attribution never mentions
 * it. This gate measures the thing that decides it: how an order's own size moves
 * the price it ends up paying, against a stated book.
 *
 * Method — the kernel's own dry matcher, not a model of it. For each size the
 * ladder is re-mirrored and a marketable taker BUY is placed; the kernel walks the
 * levels and reports the fill (a dry FOK that cannot walk the book is REJECTED,
 * per #171's honest-taker semantics). The walked VWAP is the impact. Nothing is
 * re-derived here: the fill price, the filled size and the fee all come back over
 * the wire, and the ledger movement is asserted against them.
 *
 * What it prints:
 *   * the impact curve — size, filled, VWAP, slippage in bps vs the best price on
 *     the walked side, and the USD notional, one row per size;
 *   * CAPACITY — the largest size that fills COMPLETELY and stays within
 *     `--max-slippage-bps` of that best price on this ladder;
 *   * how the configured caps stand against it. `--max-order-notional` is a RISK
 *     cap, not a liquidity cap: at a 0.40 best ask a 100 USD cap is 250 shares,
 *     which a thin ladder will refuse or fill far worse. This script prints that
 *     comparison and, with `--require-caps-within-capacity`, refuses to pass while
 *     the notional cap allows orders beyond the measured capacity.
 *
 * `--side buy` (the default) walks the ASK ladder — what an entry pays. `--side
 * sell` walks the BID ladder — what an exit gives up. Both are measured, because
 * "the exit side is symmetric with the entry side" was an assumption (#192 §6.3),
 * and on a real book it is measurably false: of the two production snapshots in
 * `docs/reports/data/capacity-gate/`, one is 7.6x deeper on the bid side than on
 * the ask (2253.09 vs 296.88 shares) and the other is near-symmetric (4257.25 vs
 * 4411.09) yet still worse to sell into at every size measured. The side is named
 * in every line of output so a curve cannot be read as the wrong one.
 *
 * What it is NOT: a measurement of the live market. The default ladder is a
 * FIXTURE — a deterministic depth profile that exercises the machinery and gives
 * the repository a number to reason about in `docs/CAPACITY_AND_EQUITY.md`. A real
 * capacity number needs a real book, so `--book <file>` takes a captured ladder
 * (`{"asks": [[price, size], ...]}` for buy, `{"bids": ...}` for sell, best level
 * first) and the doc says plainly which numbers here are fixtures and which are
 * not. `scripts/archive-book-extract.mjs` lifts such a file out of the production
 * event archive.
 *
 * Usage:
 *   node scripts/capacity-check.mjs
 *   node scripts/capacity-check.mjs --book /tmp/ladder.json --max-slippage-bps 50
 *   node scripts/capacity-check.mjs --book /tmp/real-book.json --side sell
 *   node scripts/capacity-check.mjs --sizes 1,5,10,25,50,100 --json out.json
 *
 * Exit 0 only if every assertion passes.
 */
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { coreBinaryPath, checkCoreProvenance } from './lib/core-provenance.mjs';
import { describeQuote, feeModelProblems, feeQuoter, feeUsdFor } from './lib/fee-model.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { pollUntil, sleep } from './lib/wait.mjs';

const argv = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};
const has = (name) => argv.includes(name);

/** Which side of the book is walked: buy takes liquidity from the asks, sell from the bids. */
const SIDE = opt('--side', 'buy');
if (SIDE !== 'buy' && SIDE !== 'sell') {
  console.error(`--side must be buy or sell (got ${JSON.stringify(SIDE)})`);
  process.exit(2);
}
/**
 * The fixture ladder. It is ONE book: [price, size] levels, best-first for
 * whichever side is being walked. Loaded as asks it is ascending (best ask 0.40);
 * loaded as bids the SAME depth profile is read descending (best bid 0.50), so the
 * sell fixture is the mirror of the buy one rather than a second, unrelated book.
 */
const DEFAULT_LADDER = [[0.40, 10], [0.41, 20], [0.42, 50], [0.44, 100], [0.50, 500]];
const SIZES = opt('--sizes', '1,2,5,10,20,50,100').split(',').map((s) => Number(s.trim())).filter((n) => Number.isFinite(n) && n > 0);
const MAX_SLIPPAGE_BPS = Number(opt('--max-slippage-bps', '100'));
const ORDER_NOTIONAL_CAP = Number(opt('--max-order-notional', '100'));
const MAX_SHARES = Number(opt('--max-shares', '10'));
const REQUIRE_CAPS_WITHIN = has('--require-caps-within-capacity');
const JSON_OUT = opt('--json', null);

const ladderPath = opt('--book', null);
let ladder = SIDE === 'buy' ? DEFAULT_LADDER : [...DEFAULT_LADDER].reverse();
let ladderName = 'fixture';
if (ladderPath) {
  if (!existsSync(ladderPath)) {
    console.error(`missing book file: ${ladderPath}`);
    process.exit(2);
  }
  const parsed = JSON.parse(readFileSync(ladderPath, 'utf8'));
  const key = SIDE === 'buy' ? 'asks' : 'bids';
  ladder = (parsed[key] ?? []).map(([price, size]) => [Number(price), Number(size)]);
  ladderName = ladderPath;
  if (ladder.length === 0) {
    console.error(`no ${key} in ${ladderPath} (expected {"${key}": [[price, size], ...]}); ` +
      `--side ${SIDE} walks the ${SIDE === 'buy' ? 'ask' : 'bid'} side`);
    process.exit(2);
  }
  // The best level is read as level 0 everywhere below, so a ladder that is not
  // best-first would produce a plausible, wrong curve rather than an error. The
  // kernel sorts internally either way (`sim.rs::walk_marketable`), which is
  // exactly why the mistake would go unnoticed.
  const ascending = ladder.every((l, i) => i === 0 || l[0] >= ladder[i - 1][0]);
  const descending = ladder.every((l, i) => i === 0 || l[0] <= ladder[i - 1][0]);
  const ordered = SIDE === 'buy' ? ascending : descending;
  if (!ordered) {
    console.error(`${ladderPath}: "${key}" must be ${SIDE === 'buy' ? 'ASCENDING' : 'DESCENDING'} (best level first) ` +
      `for --side ${SIDE}; got ${ladder.slice(0, 4).map(([p]) => p).join(', ')}... — re-extract with ` +
      'scripts/archive-book-extract.mjs, which normalizes the order');
    process.exit(2);
  }
}

const BIN = coreBinaryPath();
const gate = createChecks();
const { check } = gate;

const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-capacity-'));
const SOCK = scratchSocketPath('capacity');
// A live round slot: the position engine would force-exit a position whose round
// is in the past before the next order is placed (the same trap the parity gate
// documents). Each size also needs its OWN asset: the risk layer allows one open
// position per asset ("Already in BTC" rejects the second one), and these
// positions are deliberately left open by `--no-auto-exits`.
const liveSlot = () => Math.floor((Date.now() + 900_000) / 1000 / 900);
const ASSETS = ['BTC', 'ETH', 'SOL', 'XRP', 'ADA', 'DOT', 'LINK', 'AVAX', 'MATIC', 'UNI', 'ATOM', 'NEAR'];
/**
 * A marketable taker on the side under test: the limit is placed at the far end of
 * the ladder so the walk is bounded by DEPTH and never by the limit. A tighter
 * limit would measure the limit, not the book.
 *
 * The far end cannot be the extreme of the price range for a SELL: the risk layer
 * rejects `price <= min_price` (`risk.rs`, default min_price 0), so the "any price"
 * limit of 0.0 that a venue would accept is refused before the matcher sees it, and
 * a limit of 0.01 would stop the walk above a book whose bids go to 0.001 (the
 * production archive has such books). The ladder's own lowest level is the honest
 * substitute: strictly positive on any real book, and low enough that every level
 * above it is walked.
 *
 * A SELL also presents `price × size` as its notional, which at such a limit is
 * near zero, and `risk.rs` applies the notional cap to BUY commitment only
 * ("SELL is bounded by position"). The caps comparison the gate prints is
 * therefore DERIVED arithmetic on both sides, and the risk layer itself bounds
 * only the buy path — the sell curve is a pure liquidity measurement.
 */
const MARKETABLE_LIMIT = SIDE === 'buy' ? 1.0 : ladder[ladder.length - 1][0];
if (!(MARKETABLE_LIMIT > 0)) {
  console.error(`a ${SIDE} limit of ${MARKETABLE_LIMIT} would be refused by the risk layer (price <= min_price); ` +
    'the ladder has no positive price to stand at');
  process.exit(2);
}
const order = (asset, price, size, key) => ({
  tokenId: `cap-${asset}`, conditionId: 'cond', side: SIDE, mode: 'taker', price, size,
  internalKey: key, strategy: 'operator', asset, direction: 'up',
  roundSlot: liveSlot(),
});
if (SIZES.length + 1 > ASSETS.length) {
  console.error(`too many sizes for the ${ASSETS.length} distinct assets the fixture can hold open`);
  process.exit(2);
}

const fills = [];
const core = new CoreClient({
  binaryPath: BIN,
  socketPath: SOCK,
  mode: 'dry',
  seedBalance: 1_000_000,
  maxOrderNotional: 100_000,
  tickMs: 20,
  autoRestart: false,
  cwd: WORKDIR,
  noTradeLog: true,
  noOrderLog: true,
  noPositionLog: true,
  // No exits: an open position must not be flatten onto a ladder that is about
  // to be replaced for the next size.
  extraArgs: ['--no-auto-exits', '--max-positions', '99', '--no-discovery', '--no-event-archive'],
});
core.onEvent = (e) => { if (e.kind === 'FILL') fills.push(e); };

const bestPrice = ladder[0][0];
const depthShares = ladder.reduce((s, [, size]) => s + size, 0);
const depthNotional = ladder.reduce((s, [price, size]) => s + price * size, 0);
const sideName = SIDE === 'buy' ? 'ask' : 'bid';

console.log(`capacity:${' '} side ${SIDE} (walks the ${sideName} ladder) — ${ladder.length} levels, ${depthShares} shares, ` +
  `${depthNotional.toFixed(2)} USD of ${sideName}s (best ${bestPrice})`);
console.log(`  sizes ${SIZES.join(', ')} shares; impact budget ${MAX_SLIPPAGE_BPS} bps; ` +
  `caps: max-order-notional ${ORDER_NOTIONAL_CAP} USD, max-shares ${MAX_SHARES}`);

const rows = [];
try {
  checkCoreProvenance(BIN, check);
  await core.start();
  const ready = await rpc.ready(core);
  console.log(`  core ${ready.build ?? '?'} commit=${ready.commit ?? '?'} dirty=${ready.dirty ?? '?'}`);
  const quoteFee = feeQuoter((method, params) => core.request(method, params));
  const qBest = await quoteFee(bestPrice);
  const feeProblems = feeModelProblems(qBest);
  check('kernel fee model matches the pinned schedule (#182)', feeProblems.length === 0, feeProblems.join(' | '));
  console.log(`  fee  ${describeQuote(qBest)}`);
  // Round-trip cost at the best price: the taker fee is paid on BOTH legs of a
  // trip, and the doc's growth rule needs both halves. The complementary leg is
  // quoted at 1 - p, the price the other outcome of the same binary market rests
  // at, which is where an exit from this side actually lands.
  console.log(`  fee  ${(feeUsdFor(qBest, 1) + feeUsdFor(await quoteFee(1 - bestPrice), 1)).toFixed(6)} USD per share ` +
    `for a round trip at ${bestPrice}/${(1 - bestPrice).toFixed(2)} (entry + exit taker)`);

  for (let i = 0; i < SIZES.length; i++) {
    const size = SIZES[i];
    const asset = ASSETS[i];
    // Re-mirror the ladder: each size must meet the SAME book, or the curve
    // describes a book changing under it rather than an order doing so. The
    // walked side goes in its own slot; the other side is empty, so nothing but
    // the ladder under test can fill the order.
    await rpc.bookSnapshot(core, `cap-${asset}`, SIDE === 'sell' ? ladder : [], SIDE === 'buy' ? ladder : []);
    const before = await rpc.balance(core);
    const key = `cap-${size}`;
    const placed = await rpc.placeOrder(core, order(asset, MARKETABLE_LIMIT, size, key));
    const fill = await pollUntil(() => fills.find((f) => f.delta.orderId === placed.orderId), { timeoutMs: 2000 });
    const filled = fill ? Number(fill.delta.delta) : 0;
    const vwap = fill ? Number(fill.delta.price) : null;
    const after = await rpc.balance(core);

    const row = {
      size, status: placed.status, filled, vwap,
      // Slippage always reads as "worse than the best price on the side walked":
      // a buy pays ABOVE the best ask, a sell receives BELOW the best bid.
      slippageBps: vwap === null ? null
        : (SIDE === 'buy' ? ((vwap - bestPrice) / bestPrice) : ((bestPrice - vwap) / bestPrice)) * 10_000,
      notionalUsd: vwap === null ? null : Number((filled * vwap).toFixed(6)),
      complete: fill !== undefined && filled === size,
    };
    rows.push(row);

    const shown = row.vwap === null ? `${row.status} (no fill)` :
      `vwap ${row.vwap} (${row.slippageBps.toFixed(1)} bps), ${row.notionalUsd} USD`;
    console.log(`  ${String(size).padStart(4)} sh -> ${shown}`);

    // The walked fill must be what the ledger charged: a BUY pays the notional
    // plus its own fee at the walked price, a SELL receives the notional minus
    // it (`ledger.rs::settle_sell_fill`). The same one-basis rule the account
    // gates assert, measured here so the impact numbers cannot be a fiction of
    // the event stream.
    if (row.complete) {
      const fee = feeUsdFor(await quoteFee(row.vwap), size);
      const spent = Number(before.balance) - Number(after.balance);
      const credited = Number(after.balance) - Number(before.balance);
      const expected = SIDE === 'buy' ? row.notionalUsd + fee : row.notionalUsd - fee;
      const observed = SIDE === 'buy' ? spent : credited;
      check(`[${size} sh] the ledger ${SIDE === 'buy' ? 'charged the walked price plus' : 'credited the walked price minus'} its fee`,
        Math.abs(observed - expected) < 1e-9,
        `balance moved ${SIDE === 'buy' ? spent : credited}, walked ${row.notionalUsd} ${SIDE === 'buy' ? '+' : '-'} fee ${fee}`);
    }
  }

  // ── One more probe: more size than the whole ladder holds ──────────────────
  // A venue would kill an unfillable FOK with nothing touched, so the dry matcher
  // must refuse rather than part-fill: a partial fill here would make every
  // "capacity" number above describe a book that never existed.
  const overSize = depthShares + 10;
  const overAsset = ASSETS[SIZES.length];
  await rpc.bookSnapshot(core, `cap-${overAsset}`, SIDE === 'sell' ? ladder : [], SIDE === 'buy' ? ladder : []);
  const over = await rpc.placeOrder(core, order(overAsset, MARKETABLE_LIMIT, overSize, 'cap-over'));
  await sleep(200);
  const overFill = fills.find((f) => f.delta.orderId === over.orderId);
  rows.push({
    size: overSize, status: over.status,
    filled: overFill ? Number(overFill.delta.delta) : 0,
    vwap: overFill ? Number(overFill.delta.price) : null,
    slippageBps: null, notionalUsd: null, complete: false,
  });
  check(`an order larger than the whole ladder (${overSize} sh) is refused, not part-filled`,
    over.status === 'REJECTED' && overFill === undefined, JSON.stringify(over));

  // ── The invariants a walk must hold ────────────────────────────────────────
  const filledRows = rows.filter((r) => r.vwap !== null);
  check(`the impact curve is monotone on the ${sideName} side (a bigger order never pays less)`,
    filledRows.every((r, i) => i === 0 || r.slippageBps >= filledRows[i - 1].slippageBps - 1e-9),
    JSON.stringify(filledRows.map((r) => `${r.size}:${r.slippageBps?.toFixed(1)}`)));
  check('an order inside the best level pays the best price (no self-impact)',
    filledRows.filter((r) => r.size <= ladder[0][1]).every((r) => r.slippageBps === 0),
    JSON.stringify(filledRows.map((r) => `${r.size}:${r.slippageBps?.toFixed(1)}`)));

  // ── Capacity, and how the configured caps stand against it ─────────────────
  const within = rows.filter((r) => r.complete && r.slippageBps !== null && r.slippageBps <= MAX_SLIPPAGE_BPS);
  const capacity = within.reduce((best, r) => (best === null || r.size > best.size ? r : best), null);
  const capSharesAtBest = ORDER_NOTIONAL_CAP / bestPrice;
  console.log('');
  console.log(`  CAPACITY at <= ${MAX_SLIPPAGE_BPS} bps on the ${sideName} side of this ladder: ` +
    (capacity ? `${capacity.size} shares = ${capacity.notionalUsd} USD (vwap ${capacity.vwap})` : 'none — even the smallest size moves the price past the budget'));
  console.log(`  caps: max-order-notional ${ORDER_NOTIONAL_CAP} USD = ${capSharesAtBest.toFixed(1)} shares at ${bestPrice}, ` +
    `vs ${capacity ? capacity.size : 0} shares of capacity (${(capSharesAtBest / (capacity ? capacity.size : Number.POSITIVE_INFINITY)).toFixed(1)}x)`);
  console.log(`  caps: max-shares ${MAX_SHARES} = ${(MAX_SHARES * bestPrice).toFixed(2)} USD at ${bestPrice}, ` +
    `${MAX_SHARES <= (capacity ? capacity.size : 0) ? 'inside' : 'OUTSIDE'} the measured capacity`);
  check('the ladder produced a capacity at the requested impact budget', capacity !== null,
    `no size stayed within ${MAX_SLIPPAGE_BPS} bps — raise the budget or widen the ladder`);
  if (REQUIRE_CAPS_WITHIN) {
    check('the per-order notional cap is inside the measured capacity',
      capacity !== null && capSharesAtBest <= capacity.size,
      `max-order-notional ${ORDER_NOTIONAL_CAP} USD allows ${capSharesAtBest.toFixed(1)} shares but only ` +
      `${capacity ? capacity.size : 0} fill within ${MAX_SLIPPAGE_BPS} bps — the risk cap does not bound impact`);
  } else if (capacity !== null && capSharesAtBest > capacity.size) {
    console.log(`  note the notional cap (a RISK cap) is ${(capSharesAtBest / capacity.size).toFixed(1)}x the ` +
      'liquidity capacity above — set it from this measurement (or pass --require-caps-within-capacity to enforce).');
  }

  if (JSON_OUT) {
    writeFileSync(JSON_OUT, JSON.stringify({
      ladder: ladderName, side: SIDE, bestPrice, depthShares, depthNotional,
      maxSlippageBps: MAX_SLIPPAGE_BPS, orderNotionalCap: ORDER_NOTIONAL_CAP, maxShares: MAX_SHARES,
      rows, capacity, core: { build: ready.build, commit: ready.commit, dirty: ready.dirty },
    }, null, 2) + '\n');
    console.log(`  report written to ${JSON_OUT}`);
  }
} catch (e) {
  failures++;
  console.log('  FAIL harness error', e?.stack || e);
} finally {
  await core.stop();
}

console.log(gate.failures === 0
  ? `\nCAPACITY OK — the ${sideName}-side impact curve is what it claims, and the caps are stated against it.`
  : `\nCAPACITY FAILED (${gate.failures})`);
process.exit(gate.failures === 0 ? 0 : 1);
