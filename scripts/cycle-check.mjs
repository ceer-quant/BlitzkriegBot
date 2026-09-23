#!/usr/bin/env node
/**
 * Deterministic full-cycle check for the Rust core (no network, no production
 * interference). Proves the deployed binary can execute:
 *   resting bid placed over the operator wire -> maker fill -> position opens
 *   -> live re-valuation as the book moves -> exit.
 *
 * The entry is placed by the gate itself (`orders.place`), not by a strategy:
 * this gate owns the order CHAIN, and the kernel ships no strategy to drive it.
 * The chain it exercises is production code either way — `orders.place` is the
 * same `place_outcome` the panel's manual order button calls.
 *
 * Runs its own core on a private socket. Usage: node scripts/cycle-check.mjs
 */

// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { waitForSocket } from './lib/wait.mjs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync } from 'fs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const SOCK = join(tmpdir(), `blitzkrieg-cycle-${process.pid}.sock`);
const ROUND_SEC = 3600;

// Isolated working directory: the core persists its trade log / near-miss file
// at RELATIVE paths, so running from a scratch dir guarantees this harness can
// never touch the live `data/trades/trades.jsonl`.
const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-cycle-'));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN}`); process.exit(2); }
try { unlinkSync(SOCK); } catch {}

const args = [
  '--socket', SOCK, '--mode', 'dry', '--tick-ms', '50',
  '--seed-balance', '1000', '--max-order-notional', '6',
  '--engine', '--no-discovery',
  '--no-event-archive',
  '--no-trade-log',
  '--round-sec', String(ROUND_SEC),
  '--min-round-age', '0', '--min-time-left', '0',
  '--max-positions', '2',
];
const proc = spawn(BIN, args, { stdio: ['ignore', 'inherit', 'inherit'], cwd: WORKDIR });
proc.on('exit', (c) => { if (c !== null) console.error(`core exited early (${c})`); });

// ── JSON-RPC over the UDS ────────────────────────────────────────────────────
// One adopted connection for the whole run, from the shared client: this gate
// spawns the core itself, so it attaches to the socket it just waited for.
let client = null;
const rpc = (method, params = {}) => client.request(method, params);

const M = { BOOK: 'books.snapshot', MARKETS: 'engine.markets', STATS: 'engine.stats',
            POS: 'positions.list', ORDERS: 'orders.list', TRADES: 'trades.history', ROUND: 'engine.round' };

const show = (label, v) => console.log(`  ${label}: ${v}`);
const fmt = (n) => Number(n).toFixed(4);

async function main() {
  // wait for the socket
  await waitForSocket(SOCK, { timeoutMs: 5000 });
  client = await CoreClient.connect({ socketPath: SOCK });
  await rpc('core.ready');
  console.log(`binary: ${BIN}`);
  console.log(`core ready (${args.join(' ')})\n`);

  const now = Date.now();
  const slot = Math.floor(now / 1000 / ROUND_SEC);
  const market = {
    asset: 'BTC', conditionId: '0xcond', questionId: '0xq',
    upTokenId: 'UP', downTokenId: 'DOWN',
    upPrice: 0.5, downPrice: 0.5,
    expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
    negRisk: true, question: 'BTC up/down',
  };
  await rpc(M.MARKETS, { markets: [market] });
  console.log(`[1] round fed  slot=${slot} expires in ${Math.round(((slot+1)*ROUND_SEC*1000-now)/1000)}s`);

  // [2] Rest a maker bid on UP over the operator wire.
  const book = (bid, ask, bs = 100, as = 100) => ({ tokenId: 'UP', bids: [{ price: bid, size: bs }], asks: [{ price: ask, size: as }] });
  const REST = 0.30;
  await rpc('orders.place', {
    tokenId: 'UP', conditionId: '0xcond', side: 'buy', mode: 'maker',
    price: REST, size: 10, internalKey: 'cycle-rest',
    strategy: 'operator', asset: 'BTC', direction: 'up', roundSlot: slot,
  });
  await sleep(300);
  const orders = (await rpc(M.ORDERS)).orders;
  console.log(`[2] maker bid placed -> live orders: ${orders.filter(o => o.status === 'LIVE').length}`);
  for (const o of orders.filter(o => o.status === 'LIVE'))
    console.log(`      bid ${fmt(o.price)} x ${o.size} (${o.side}, ${o.mode ?? 'maker_then_taker'})`);

  // [3] Cross it — the ask reaches the resting bid, so the maker fills.
  await rpc(M.BOOK, book(REST - 0.01, REST));
  await sleep(500);
  let pos = (await rpc(M.POS)).positions;
  console.log(`[3] book crossed -> open positions: ${pos.length}`);
  for (const p of pos)
    console.log(`      ${p.asset} ${p.direction} entry=${fmt(p.entryPrice)} cur=${fmt(p.currentPrice)} pnl=${fmt(p.unrealizedPct)}%`);

  // [4] Move the book up and show live re-valuation (the bug the user saw).
  console.log('[4] live re-valuation as the book moves:');
  for (const [b, a] of [[0.50, 0.52], [0.60, 0.62], [0.70, 0.72], [0.85, 0.87]]) {
    await rpc(M.BOOK, book(b, a));
    await sleep(250);
    const ps = (await rpc(M.POS)).positions;
    const line = ps.length
      ? ps.map((p) => `${p.asset} ${p.direction} cur=${fmt(p.currentPrice)} pnl=${fmt(p.unrealizedPct)}%`).join(' | ')
      : '(all positions closed by exit rules)';
    console.log(`      bid ${fmt(b)} -> ${line}`);
  }

  const finalPos = (await rpc(M.POS)).positions;
  const trades = (await rpc(M.TRADES, { limit: 10 })).trades;
  const st = await rpc(M.STATS);
  console.log(`\n[5] summary  open=${finalPos.length} closed=${trades.length} signals=${st.signals} placeRejected=${st.placeRejected}`);
  for (const t of trades) {
    console.log(`      ${t.asset} ${t.direction} ${fmt(t.entryPrice)} -> ${fmt(t.exitPrice ?? 0)} net=${fmt(t.netPnlUsd ?? 0)} reason=${t.exitReason ?? '-'}`);
  }
  const ok = (trades.length + finalPos.length) > 0;
  console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — order placed, filled, and position valued`);
  proc.kill('SIGTERM');
  await sleep(200);
  process.exit(ok ? 0 : 1);
}

main().catch((e) => { console.error('ERROR', e.message); proc.kill('SIGTERM'); process.exit(1); });
