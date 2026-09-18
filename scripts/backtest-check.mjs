#!/usr/bin/env node
/**
 * End-to-end check for the event-driven backtester (P-1.2/P-1.3): capture a
 * market-data archive from a live core, then replay that exact archive offline
 * and require the SAME result (orders, fills, closed trades, net PnL, per-
 * strategy ledger).
 *
 * The capture is driven through `engine.book` — the engine choke point the
 * Rust-native feed (`--feed-ws`) uses and the path `--backtest` replays. The
 * older `books.snapshot` bridge is deliberately NOT used: it additionally runs
 * the DRY maker-fill simulation, so a capture through it would fill a crossing
 * maker entry that the production feed leaves resting
 * (dev-docs/DECISIONS_PENDING.md D-11). Same driver on both sides is what makes
 * the comparison meaningful.
 *
 * Deterministic and isolated: the core runs on a private socket in a scratch
 * working directory, so it can never touch production data files or the live
 * socket. No network. Usage: node scripts/backtest-check.mjs
 */

import { spawn } from 'child_process';
import net from 'net';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, readFileSync, statSync } from 'fs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const SOCK = join(tmpdir(), `blitzkrieg-backtest-${process.pid}.sock`);
const ROUND_SEC = 3600;
// Maker entries escalate to a taker order after this long. Short so the
// scripted cycle stays compact; the events below keep >150 ms of clearance on
// either side of the deadline, so the tick phase cannot change the outcome.
const ENTRY_TIMEOUT_MS = 600;
const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-backtest-'));
const ARCHIVE = join(WORKDIR, 'events.jsonl');
const REPORT = join(WORKDIR, 'report.json');

// Knobs shared by the capture run and the replay: the whole point is that the
// same strategy config sees the same events, so only the data source differs.
const KNOBS = [
  '--engine',
  '--no-discovery',
  '--no-trade-log',
  '--no-order-log',
  '--no-position-log',
  '--round-sec', String(ROUND_SEC),
  '--min-round-age', '0', '--min-time-left', '0',
  '--trend-confirm-sec', '3', '--trend-window-floor-ms', '1000',
  '--entry-maker-timeout-ms', String(ENTRY_TIMEOUT_MS),
  '--seed-balance', '1000',
  '--max-order-notional', '6',
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fmt = (n) => Number(n).toFixed(4);
const money = (n) => Number(n).toFixed(8);

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN}`); process.exit(2); }
try { unlinkSync(SOCK); } catch {}

const args = ['--socket', SOCK, '--mode', 'dry', '--tick-ms', '50', ...KNOBS, '--event-archive', ARCHIVE];
const proc = spawn(BIN, args, { stdio: ['ignore', 'inherit', 'inherit'], cwd: WORKDIR });
let liveExit = null;
proc.on('exit', (c) => { liveExit = c; });

// ── minimal JSON-RPC over the UDS ────────────────────────────────────────────
let sock = null, buf = '', seq = 0;
const pending = new Map();
function connect() {
  return new Promise((resolve, reject) => {
    sock = net.connect(SOCK, () => resolve());
    sock.on('error', reject);
    sock.on('data', (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        if (!line.trim()) continue;
        let msg; try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id != null && pending.has(msg.id)) {
          const { resolve: res, reject: rej } = pending.get(msg.id); pending.delete(msg.id);
          msg.error ? rej(new Error(`${msg.error.code}: ${msg.error.message}`)) : res(msg.result);
        }
      }
    });
  });
}
function rpc(method, params = {}) {
  const id = ++seq;
  return new Promise((res, rej) => {
    pending.set(id, { resolve: res, reject: rej });
    sock.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}

const M = { BOOK: 'engine.book', MARKETS: 'engine.markets', STATS: 'engine.stats',
            POS: 'positions.list', ORDERS: 'orders.list', TRADES: 'trades.history' };
const book = (bid, ask, bs = 100, as = 100) => ({ tokenId: 'UP', bids: [{ price: bid, size: bs }], asks: [{ price: ask, size: as }] });

const checks = [];
function check(name, ok, detail = '') {
  checks.push({ name, ok });
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
}

async function main() {
  for (let i = 0; i < 100; i++) { if (existsSync(SOCK)) break; await sleep(50); }
  await connect();
  await rpc('core.ready');
  console.log(`binary: ${BIN}`);
  console.log(`capture run: ${args.join(' ')}\n`);

  // ── capture: drive one full cycle so the archive holds a real trade ────────
  const now = Date.now();
  const slot = Math.floor(now / 1000 / ROUND_SEC);
  await rpc(M.MARKETS, { markets: [{
    asset: 'BTC', conditionId: '0xcond', questionId: '0xq',
    upTokenId: 'UP', downTokenId: 'DOWN',
    upPrice: 0.5, downPrice: 0.5,
    expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
    negRisk: true, question: 'BTC up/down',
  }] });
  for (let i = 0; i < 14; i++) { await rpc(M.BOOK, book(0.57, 0.58)); await sleep(300); }
  await rpc(M.BOOK, book(0.43, 0.44));               // dip -> resting bid 0.43
  await sleep(400);
  await rpc(M.BOOK, book(0.41, 0.42));               // crosses the resting bid -> the
  await sleep(900);                                  // feed path cross-fills it (KI-1)
  for (const [b, a] of [[0.60, 0.62], [0.80, 0.82], [0.95, 0.97]]) {
    await rpc(M.BOOK, book(b, a));                   // rally -> exit rules take profit
    await sleep(400);
  }
  let trades = [];
  for (let i = 0; i < 12 && trades.length === 0; i++) {
    await sleep(250);
    trades = (await rpc(M.TRADES, { limit: 10 })).trades;
  }
  const livePos = (await rpc(M.POS)).positions;
  const liveStats = await rpc(M.STATS);
  const liveOrders = (await rpc(M.ORDERS)).orders;
  const livePnl = trades.reduce((s, t) => s + Number(t.netPnlUsd ?? 0), 0);
  console.log(`[capture] books=${liveStats.books} signals=${liveStats.signals} orders=${liveOrders.length} closed=${trades.length} open=${livePos.length} netPnL=${fmt(livePnl)}`);
  for (const t of trades) {
    console.log(`          ${t.asset} ${t.direction} ${fmt(t.entryPrice)} -> ${fmt(t.exitPrice ?? 0)} net=${fmt(t.netPnlUsd ?? 0)} (${t.exitReason ?? '-'})`);
  }

  check('capture closed a trade', trades.length >= 1, `closed=${trades.length}`);

  // Stop the capture core; its archive must be complete and flushed on exit.
  proc.kill('SIGTERM');
  for (let i = 0; i < 40 && liveExit === null; i++) await sleep(50);
  check('capture core stopped cleanly', liveExit !== null, `exit=${liveExit}`);
  check('archive written', existsSync(ARCHIVE), ARCHIVE);
  const bytes = existsSync(ARCHIVE) ? statSync(ARCHIVE).size : 0;
  const lines = existsSync(ARCHIVE) ? readFileSync(ARCHIVE, 'utf8').trim().split('\n').length : 0;
  check('archive has events', lines > 0 && bytes > 0, `${lines} lines / ${bytes} bytes`);

  // ── replay: same knobs, same binary, offline ───────────────────────────────
  const replayArgs = ['--backtest', ARCHIVE, ...KNOBS, '--backtest-report', REPORT, '--backtest-tail-ms', '30000'];
  console.log(`\nreplay run: ${replayArgs.join(' ')}\n`);
  const replay = spawn(BIN, replayArgs, { stdio: ['ignore', 'inherit', 'inherit'], cwd: WORKDIR });
  const code = await new Promise((res) => replay.on('exit', res));
  check('replay exited 0', code === 0, `exit=${code}`);
  if (!existsSync(REPORT)) { check('report written', false, REPORT); return finish(); }
  const rep = JSON.parse(readFileSync(REPORT, 'utf8'));
  check('report written', true, REPORT);

  // ── equivalence: live vs replay on the same events ─────────────────────────
  check('event counts match', rep.sourceStats.events === liveStats.books + liveStats.tops + liveStats.spots + liveStats.rounds,
    `archive=${rep.sourceStats.events} live=${liveStats.books + liveStats.tops + liveStats.spots + liveStats.rounds}`);
  check('no malformed lines', rep.sourceStats.malformedLines === 0);
  check('no out-of-order events', rep.sourceStats.outOfOrderEvents === 0);
  check('closed trades match', rep.trades.closed === trades.length, `replay=${rep.trades.closed} live=${trades.length}`);
  const liveFilled = liveOrders.filter((o) => o.status === 'FILLED').length;
  const liveCancelled = liveOrders.filter((o) => o.status === 'CANCELLED').length;
  check('fills match', rep.fills === liveFilled, `replay=${rep.fills} live=${liveFilled}`);
  check('order counts match',
    rep.orders.orders === liveOrders.length && rep.orders.filled === liveFilled && rep.orders.cancelled === liveCancelled,
    `replay=${rep.orders.orders} (${rep.orders.filled} filled, ${rep.orders.cancelled} cancelled) live=${liveOrders.length} (${liveFilled} filled, ${liveCancelled} cancelled)`);
  // KI-1: the entry BUY must be cross-filled by the feed path at its resting
  // maker limit — not escalated to taker (the old dry behaviour) and not left
  // LIVE. The second FILLED order is the exit SELL.
  const entryBuy = liveOrders.find((o) => o.side === 'buy' && o.tokenId === 'UP');
  check('entry maker order CROSS-FILLED by the feed path (KI-1)',
    entryBuy && entryBuy.status === 'FILLED',
    entryBuy ? `entry status=${entryBuy.status} price=${entryBuy.price}` : 'no entry BUY order');
  check('net PnL matches', Math.abs(Number(rep.trades.netPnlUsd) - livePnl) < 1e-9,
    `replay=${money(rep.trades.netPnlUsd)} live=${money(livePnl)}`);
  check('open positions match', rep.openPositions === livePos.length, `replay=${rep.openPositions} live=${livePos.length}`);
  const liveStrat = (liveStats.strategies ?? []).find((s) => s.name === 'spread_arb');
  const repStrat = (rep.strategies ?? []).find((s) => s.name === 'spread_arb');
  check('strategy ledger matches',
    liveStrat != null && repStrat != null &&
      Math.abs(Number(repStrat.netPnlUsd) - Number(liveStrat.netPnlUsd)) < 1e-9 &&
      repStrat.closedTrades === liveStrat.closedTrades,
    `replay=${repStrat ? money(repStrat.netPnlUsd) : '?'} live=${liveStrat ? money(liveStrat.netPnlUsd) : '?'}`);

  // The replay must be dry and must not have written any trade/order log.
  check('replay forced dry', rep.forcedDry === true);
  check('entry maker timeout carried into the report', Number(rep.entryMakerTimeoutMs) === ENTRY_TIMEOUT_MS,
    `report=${rep.entryMakerTimeoutMs}`);
  for (const f of ['data/trades/trades.jsonl', 'data/orders/orders.jsonl', 'data/positions/positions.jsonl']) {
    check(`replay wrote no ${f}`, !existsSync(join(WORKDIR, f)));
  }
  return finish();
}

function finish() {
  const failed = checks.filter((c) => !c.ok);
  console.log(`\nRESULT: ${failed.length === 0 ? 'PASS' : 'FAIL'} — ${checks.length - failed.length}/${checks.length} checks`);
  if (failed.length) console.log(`failed: ${failed.map((c) => c.name).join(', ')}`);
  try { proc.kill('SIGTERM'); } catch {}
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((e) => { console.error('ERROR', e.message); check('harness completed', false, e.message); finish(); });
