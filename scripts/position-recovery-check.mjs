#!/usr/bin/env node
/**
 * Crash-recovery regression: OPEN POSITIONS must survive a core restart.
 *
 * Reproduces the unmanaged-position failure (same class as the orphan-order bug):
 * a position is opened, the process is killed, and a fresh core (same position-log
 * path) must RESTORE it — so the bot keeps valuing it and running exit rules
 * instead of letting the trade drift to expiry unmanaged.
 *
 * The opening order goes in over the operator wire (`orders.place`) and is filled
 * by a crossing book: recovery is about the position log, not about who decided
 * to trade, and the kernel ships no strategy to decide it.
 *
 * Usage: node scripts/position-recovery-check.mjs
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'positiondb-'));
const POS_LOG = join(WORK, 'positions.jsonl');

// ── JSON-RPC over the UDS ────────────────────────────────────────────────────
// One adopted connection at a time, from the shared client: this gate spawns
// each core itself and re-attaches to the replacement's socket.
let client = null;
async function connect(path) {
  await client?.stop({ cancelRestingOrders: false });
  client = await CoreClient.connect({ socketPath: path });
}
const rpc = (method, params = {}) => client.request(method, params);
async function boot(path, extra = []) {
  try { unlinkSync(path); } catch {}
  const p = spawn(BIN, ['--socket', path, '--mode', 'dry', '--tick-ms', '50', '--seed-balance', '1000',
    '--max-order-notional', '6', '--position-log', POS_LOG, '--no-trade-log', '--no-order-log',
    '--engine', '--no-event-archive', '--no-discovery', '--no-auto-exits', '--round-sec', '3600',
    '--min-round-age', '0', '--min-time-left', '0', ...extra],
    { stdio: 'ignore', cwd: WORK });
  for (let i = 0; i < 100 && !existsSync(path); i++) await sleep(50);
  await sleep(300);
  return p;
}
const MARKET = 'engine.markets', BOOK = 'books.snapshot', POS = 'positions.list', ORDERS = 'orders.list';
const book = (bid, ask) => ({ tokenId: 'UP', bids: [{ price: bid, size: 100 }], asks: [{ price: ask, size: 100 }] });
const REST = 0.30;

async function openPosition(path) {
  await connect(path);
  await rpc('core.ready');
  const now = Date.now(), ROUND = 3600, slot = Math.floor(now / 1000 / ROUND);
  await rpc(MARKET, { markets: [{ asset: 'BTC', conditionId: '0xcond', questionId: '0xq',
    upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
    expiresAtMs: (slot + 1) * ROUND * 1000, roundSlot: slot, negRisk: true, question: 'BTC up/down' }] });
  // Rest a maker bid, then cross it: ask reaches the bid -> maker fill -> position.
  await rpc('orders.place', {
    tokenId: 'UP', conditionId: '0xcond', side: 'buy', mode: 'maker',
    price: REST, size: 10, internalKey: 'k-open',
    strategy: 'operator', asset: 'BTC', direction: 'up', roundSlot: slot,
  });
  await sleep(300);
  const resting = ((await rpc(ORDERS)).orders || []).filter((o) => o.status === 'LIVE');
  if (resting.length === 0) throw new Error('the maker bid did not rest — nothing to cross');
  await rpc(BOOK, book(REST - 0.01, REST)); await sleep(600);   // cross → maker fill → position
}

// 1) Boot and open a position.
const sock1 = join(tmpdir(), `positiondb-1-${process.pid}.sock`);
const p1 = await boot(sock1);
await openPosition(sock1);
const before = (await rpc(POS)).positions;
console.log('before crash: open positions =', before.length, before[0] ? `(${before[0].asset} ${before[0].direction} entry=${before[0].entryPrice})` : '');

// 2) Hard kill, boot a FRESH core on the same position log.
p1.kill('SIGKILL');
await sleep(500);
const sock2 = join(tmpdir(), `positiondb-2-${process.pid}.sock`);
const p2 = await boot(sock2);
await connect(sock2);
await rpc('core.ready');

// 3) The new core must remember the position and keep valuing it.
const after = (await rpc(POS)).positions;
await rpc(BOOK, book(0.70, 0.72)); await sleep(300);
const revalued = (await rpc(POS)).positions;
console.log('after restart: open positions =', after.length, after[0] ? `(${after[0].asset} ${after[0].direction} entry=${after[0].entryPrice})` : '');
console.log('restored position present :', after.length > 0);
console.log('re-valued after restart   :', revalued[0] ? `cur=${revalued[0].currentPrice} pnl=${revalued[0].unrealizedPct}%` : 'n/a');

p2.kill('SIGTERM');
await sleep(300);

const logLines = existsSync(POS_LOG) ? readFileSync(POS_LOG, 'utf8').trim().split('\n').filter(Boolean) : [];
console.log('position log lines        :', logLines.length);

const ok = before.length >= 1 && after.length >= 1;
console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — an open position survived a hard kill and was restored (kept managed)`);
process.exit(ok ? 0 : 1);
