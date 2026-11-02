#!/usr/bin/env node
/**
 * Pull one PolySimulator account's fills for a window and split cash from
 * open-position mark. This is the recovery path for the Oct 24–28 2026
 * lua_momentum run: local orders.jsonl / trades.jsonl are empty (the
 * order-log bug fixed in 423327d / b999a95), and the venue is not on-chain.
 *
 * Auth is POLYSIM_API_KEY (X-API-Key). The process environment is the only
 * place the key is read. It is never printed, never written to the report,
 * and never put on the command line. Without it the command stops before
 * any request.
 *
 *   node scripts/polysim-history.mjs --self-test
 *   POLYSIM_API_KEY=… node scripts/polysim-history.mjs
 *   POLYSIM_API_KEY=… node scripts/polysim-history.mjs --from 2026-10-24 --to 2026-10-29
 *
 * --from / --to are calendar dates in Asia/Shanghai (UTC+8), --to exclusive.
 * Default window is 2026-10-24 .. 2026-10-29, the rise on the owner's curve.
 */
const BASE = process.env.POLYSIM_API_URL || 'https://api.polysimulator.com';
const TZ_OFFSET_MS = 8 * 60 * 60 * 1000;
const PAGE = 100;
const MAX_PAGES = 40;

const args = process.argv.slice(2);
const selfTest = args.includes('--self-test');
function flag(name, fallback) {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
}
const fromDate = flag('--from', '2026-10-24');
const toDate = flag('--to', '2026-10-29');

function shanghaiStartMs(date) {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date);
  if (!m) throw new Error(`bad date ${date} (want YYYY-MM-DD)`);
  const utc = Date.UTC(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
  return utc - TZ_OFFSET_MS;
}

function parseTime(raw) {
  if (raw == null || raw === '') return null;
  if (typeof raw === 'number') {
    return raw >= 1_000_000_000_000 ? raw : raw * 1000;
  }
  const s = String(raw).trim();
  if (/^\d+$/.test(s)) {
    const n = Number(s);
    return n >= 1_000_000_000_000 ? n : n * 1000;
  }
  const t = Date.parse(s);
  return Number.isNaN(t) ? null : t;
}

function num(v) {
  if (v == null || v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

/** One fill row, from either a FILLED order or a trade. */
function normalizeFilled(row) {
  return {
    source: 'filled_order',
    id: String(row.order_id ?? row.orderID ?? ''),
    at: parseTime(row.filled_at || row.filledAt || row.created_at),
    price: num(row.fill_price ?? row.fillPrice),
    size: num(row.quantity),
    side: row.side || null,
    outcome: row.outcome || null,
    market: row.market_id || row.marketId || null,
  };
}

function normalizeTrade(row) {
  return {
    source: 'trade',
    id: String(row.id ?? ''),
    at: parseTime(row.match_time ?? row.matchTime ?? row.last_update),
    price: num(row.price),
    size: num(row.size),
    side: row.side || null,
    outcome: row.outcome || null,
    market: row.market || row.asset_id || null,
    feeRateBps: num(row.fee_rate_bps ?? row.feeRateBps),
    takerOrderId: row.taker_order_id || null,
    makerOrders: Array.isArray(row.maker_orders) ? row.maker_orders.length : 0,
  };
}

function inWindow(row, fromMs, toMs) {
  return row.at != null && row.at >= fromMs && row.at < toMs;
}

/**
 * Website equity identity. `positionsMark` is the sum of open position
 * marks when the API gives one, else quantity * avg_entry_price (cost).
 * total = cash + mark. pnl = total - start. The mark is inside pnl, so a
 * positive headline can be unrealized.
 */
function equity(balance, positions) {
  const cash = num(balance && (balance.balance ?? balance.cash));
  const start = num(balance && (balance.starting_balance ?? balance.startingBalance));
  const reportedTotal = num(balance && (balance.total_value ?? balance.totalValue));
  const reportedUnreal = num(balance && (balance.unrealized_pnl ?? balance.unrealizedPnl));
  let cost = 0;
  let marked = 0;
  let priced = 0;
  for (const p of positions || []) {
    const qty = num(p.quantity) || 0;
    const avg = num(p.avg_entry_price ?? p.avgEntryPrice) || 0;
    const mark = num(p.mark_price ?? p.markPrice ?? p.current_price ?? p.currentPrice);
    cost += qty * avg;
    if (mark != null) {
      marked += qty * mark;
      priced += 1;
    }
  }
  const positionsMark = priced > 0 ? marked : cost;
  const total = reportedTotal != null ? reportedTotal : (cash != null ? cash + positionsMark : null);
  const pnl = total != null && start != null ? total - start : null;
  return {
    cash,
    start,
    positionsCost: cost,
    positionsMark,
    positionRows: (positions || []).length,
    marksAreMarket: priced > 0,
    reportedTotal,
    reportedUnrealized: reportedUnreal,
    total,
    pnl,
    pnlIncludesOpenMark: positionsMark > 0 && pnl != null,
  };
}

function summarize(fills, eq) {
  const priced = fills.filter((f) => f.price != null && f.size != null && f.size > 0);
  const notional = priced.reduce((s, f) => s + f.price * f.size, 0);
  const shares = priced.reduce((s, f) => s + f.size, 0);
  const vwap = shares > 0 ? notional / shares : null;
  const fee0 = priced.filter((f) => f.feeRateBps === 0).length;
  const feePos = priced.filter((f) => f.feeRateBps != null && f.feeRateBps > 0).length;
  const feeUnknown = priced.length - fee0 - feePos;
  return {
    n: fills.length,
    priced: priced.length,
    shares,
    notional,
    vwap,
    feeRateBpsZero: fee0,
    feeRateBpsPositive: feePos,
    feeRateBpsUnknown: feeUnknown,
    equity: eq,
  };
}

async function getJson(path, key) {
  const res = await fetch(BASE + path, {
    headers: {
      'X-API-Key': key,
      Accept: 'application/json',
      'User-Agent': 'BlitzkriegBot/polysim-history',
    },
  });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`${path} -> HTTP ${res.status} ${text.slice(0, 180)}`);
  }
  return JSON.parse(text);
}

async function pages(path, key, pick) {
  const out = [];
  let cursor = null;
  const seen = new Set();
  for (let i = 0; i < MAX_PAGES; i++) {
    const q = new URLSearchParams();
    const base = path.includes('?') ? path + '&' : path + '?';
    q.set('limit', String(PAGE));
    if (cursor) q.set('next_cursor', cursor);
    const body = await getJson(base + q.toString(), key);
    const rows = pick(body) || [];
    out.push(...rows);
    const next = body.next_cursor || body.nextCursor || null;
    if (!next || rows.length === 0 || seen.has(next)) break;
    seen.add(next);
    cursor = next;
  }
  return out;
}

function assertEq(name, got, want) {
  if (got !== want) {
    console.error(`SELF-TEST FAIL ${name}: got ${got} want ${want}`);
    process.exit(1);
  }
}

function runSelfTest() {
  const from = shanghaiStartMs('2026-10-24');
  const to = shanghaiStartMs('2026-10-29');
  // 2026-10-23T15:00:00Z = Oct 23 23:00 Shanghai, outside.
  assertEq('before', inWindow({ at: Date.parse('2026-10-23T15:00:00Z') }, from, to), false);
  // 2026-10-25T19:06:39Z = Oct 26 03:06 Shanghai, inside the rise.
  assertEq('inside', inWindow({ at: Date.parse('2026-10-25T19:06:39Z') }, from, to), true);
  // 2026-10-28T16:00:00Z = Oct 29 00:00 Shanghai, exclusive end.
  assertEq('end', inWindow({ at: Date.parse('2026-10-28T16:00:00Z') }, from, to), false);
  assertEq('seconds', parseTime(1761500000), 1761500000000);
  const eq = equity(
    { balance: '111.62', starting_balance: '100', total_value: '131.62', unrealized_pnl: '0' },
    [
      { quantity: '40', avg_entry_price: '0.25' },
      { quantity: '40', avg_entry_price: '0.25' },
    ],
  );
  assertEq('cash', eq.cash, 111.62);
  assertEq('total', eq.total, 131.62);
  assertEq('pnl', Math.round(eq.pnl * 100) / 100, 31.62);
  assertEq('includes mark', eq.pnlIncludesOpenMark, true);
  // No market mark on the rows: cost is the stand-in, and it is still inside pnl.
  assertEq('cost', eq.positionsCost, 20);
  const fills = [
    normalizeTrade({
      id: 't1',
      match_time: 1790000000,
      price: '0.30',
      size: '10',
      side: 'BUY',
      fee_rate_bps: 0,
      taker_order_id: 't1',
      maker_orders: null,
    }),
  ];
  assertEq('fee0', fills[0].feeRateBps, 0);
  assertEq('taker is trade id', fills[0].takerOrderId, 't1');
  console.log('polysim-history self-test: PASS');
}

async function main() {
  if (selfTest) {
    runSelfTest();
    return;
  }
  const key = process.env.POLYSIM_API_KEY || '';
  if (!key) {
    console.error(
      'polysim-history: POLYSIM_API_KEY is not set. Account history (balance, positions, FILLED orders, trades) requires X-API-Key. No request was sent. The key is not in the repo.',
    );
    process.exit(2);
  }
  const fromMs = shanghaiStartMs(fromDate);
  const toMs = shanghaiStartMs(toDate);
  const balance = await getJson('/v1/account/balance', key);
  let positions = [];
  try {
    const body = await getJson('/v1/account/positions', key);
    positions = body.positions || body.data || (Array.isArray(body) ? body : []);
  } catch (e) {
    positions = [];
    console.error(`positions: ${e.message}`);
  }
  const filledRaw = await pages('/v1/orders?status=FILLED', key, (b) => b.orders || b.data || []);
  const tradeRaw = await pages('/v1/data/trades', key, (b) => b.data || []);
  const filled = filledRaw.map(normalizeFilled).filter((r) => inWindow(r, fromMs, toMs));
  const trades = tradeRaw.map(normalizeTrade).filter((r) => inWindow(r, fromMs, toMs));
  const eq = equity(balance, positions);
  const report = {
    window: { from: fromDate, to: toDate, tz: 'Asia/Shanghai', fromMs, toMs },
    fetched: { filledOrders: filledRaw.length, trades: tradeRaw.length },
    inWindow: { filledOrders: filled.length, trades: trades.length },
    filledSummary: summarize(filled, eq),
    tradeSummary: summarize(trades, eq),
    note: 'taker_order_id on this venue repeats the trade id and maker_orders is often empty, so trade rows cannot be joined to our orders. FILLED orders are the attribution channel. fee_rate_bps 0 means the simulator charged no taker fee. pnl includes open-position mark when positionsMark > 0.',
    filled,
    trades,
  };
  process.stdout.write(JSON.stringify(report, null, 2) + '\n');
}

main().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
