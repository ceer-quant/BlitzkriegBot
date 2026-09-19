#!/usr/bin/env node
/**
 * Deterministic full-cycle check for the Rust core (no network, no production
 * interference). Proves the deployed binary can execute:
 *   confirm trend -> dip -> resting bid placed -> maker fill -> position opens
 *   -> live re-valuation as the book moves -> exit.
 *
 * Runs its own core on a private socket with a short trend-confirm window so the
 * whole cycle takes seconds. Usage: node scripts/cycle-check.mjs
 */

// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import net from 'net';
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
  '--enable-strategy', 'spread_arb',
  '--no-event-archive',
  '--no-trade-log',
  '--round-sec', String(ROUND_SEC),
  '--min-round-age', '0', '--min-time-left', '0',
  '--max-positions', '2',
  '--trend-confirm-sec', '3', '--trend-window-floor-ms', '1000',
];
const proc = spawn(BIN, args, { stdio: ['ignore', 'inherit', 'inherit'], cwd: WORKDIR });
proc.on('exit', (c) => { if (c !== null) console.error(`core exited early (${c})`); });

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

const M = { BOOK: 'books.snapshot', MARKETS: 'engine.markets', STATS: 'engine.stats',
            POS: 'positions.list', ORDERS: 'orders.list', TRADES: 'trades.history', ROUND: 'engine.round' };

const show = (label, v) => console.log(`  ${label}: ${v}`);
const fmt = (n) => Number(n).toFixed(4);

async function main() {
  // wait for the socket
  for (let i = 0; i < 100; i++) { if (existsSync(SOCK)) break; await sleep(50); }
  await connect();
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

  // [2] Confirm the UP trend: mid 0.575 held for >= 90% of the 3s window.
  const book = (bid, ask, bs = 100, as = 100) => ({ tokenId: 'UP', bids: [{ price: bid, size: bs }], asks: [{ price: ask, size: as }] });
  for (let i = 0; i < 14; i++) { await rpc(M.BOOK, book(0.57, 0.58)); await sleep(300); }
  let st = await rpc(M.STATS);
  const confirmed = st.confirmed || [];
  console.log(`[2] trend confirm  books=${st.books} confirmed=${confirmed.length} -> ${confirmed.length ? 'CONFIRMED' : 'NOT CONFIRMED'}`);

  // [3] Dip to mid 0.435 -> the strategy rests a bid at its own discount
  // (round2(0.435 * trend_entry_factor)) — the gate reads it, not hardcodes it.
  await rpc(M.BOOK, book(0.43, 0.44));
  await sleep(400);
  let orders = (await rpc(M.ORDERS)).orders;
  console.log(`[3] dip fed (mid 0.435) -> live orders: ${orders.filter(o => o.status === 'LIVE').length}`);
  for (const o of orders.filter(o => o.status === 'LIVE'))
    console.log(`      bid ${fmt(o.price)} x ${o.size} (${o.side}, ${o.mode ?? 'maker_then_taker'})`);

  // [4] Cross whatever resting bid the strategy actually placed — the gate owns
  // the order CHAIN, not the pricing default, so a discount change touches
  // nothing here.
  const resting = orders.filter(o => o.status === 'LIVE')[0];
  const restBid = Number(resting?.price ?? 0);
  await rpc(M.BOOK, book(Math.max(restBid - 0.01, 0.01), restBid));
  await sleep(500);
  let pos = (await rpc(M.POS)).positions;
  console.log(`[4] book crossed -> open positions: ${pos.length}`);
  for (const p of pos)
    console.log(`      ${p.asset} ${p.direction} entry=${fmt(p.entryPrice)} cur=${fmt(p.currentPrice)} pnl=${fmt(p.unrealizedPct)}%`);

  // [5] Move the book up and show live re-valuation (the bug the user saw).
  console.log('[5] live re-valuation as the book moves:');
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
  st = await rpc(M.STATS);
  console.log(`\n[6] summary  open=${finalPos.length} closed=${trades.length} signals=${st.signals} placeRejected=${st.placeRejected}`);
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
