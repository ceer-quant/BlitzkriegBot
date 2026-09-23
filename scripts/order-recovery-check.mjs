#!/usr/bin/env node
/**
 * Crash-recovery regression: orders must survive a core restart.
 *
 * Reproduces the orphan-order failure: a live order is placed, the process is
 * killed, and a fresh core (same order-log path) must RESTORE it — so the bot
 * still knows the order exists instead of leaving it orphaned at the venue.
 *
 * The order goes in over the operator wire (`orders.place`), which is the same
 * `place_outcome` path the panel's manual order button drives. No strategy is
 * involved: the kernel ships none, and recovery is about the order log, not
 * about who decided to trade.
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'orderdb-'));
const ORDER_LOG = join(WORK, 'orders.jsonl');

function rpc(sock, method, params = {}) {
  return new Promise((res) => {
    const c = net.connect(sock); let b = '';
    c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + '\n'));
    c.on('data', (d) => { b += d; const i = b.indexOf('\n'); if (i < 0) return;
      try { res(JSON.parse(b.slice(0, i)).result); } catch { res(null); } c.end(); });
    c.on('error', () => res(null)); setTimeout(() => { try { c.end(); } catch {} res(null); }, 3000);
  });
}
async function boot(sock) {
  try { unlinkSync(sock); } catch {}
  const p = spawn(BIN, ['--socket', sock, '--mode', 'dry', '--tick-ms', '50', '--seed-balance', '1000',
    '--max-order-notional', '6', '--order-log', ORDER_LOG, '--trade-log', join(WORK, 'trades.jsonl'),
    '--no-discovery', '--no-auto-exits'], { stdio: 'ignore', cwd: WORK });
  for (let i = 0; i < 80 && !existsSync(sock); i++) await sleep(50);
  await sleep(300);
  return p;
}

// 1) Boot, place a resting (maker) order that stays LIVE.
const sock1 = join(tmpdir(), `orderdb-1-${process.pid}.sock`);
const p1 = await boot(sock1);
await rpc(sock1, 'orders.place', { tokenId: 'tok1', conditionId: 'cond1', side: 'buy', mode: 'maker',
  price: 0.30, size: 10, internalKey: 'k-rest', strategy: 'operator', asset: 'BTC', direction: 'up', roundSlot: 1 });
await sleep(300);
const before = (await rpc(sock1, 'orders.list'))?.orders ?? [];
const liveBefore = before.filter((o) => o.status === 'LIVE' || o.status === 'PENDING').length;
console.log('before crash: orders =', before.length, 'live =', liveBefore);

// 2) Hard kill (simulate crash) and boot a FRESH core on the same order log.
p1.kill('SIGKILL');
await sleep(500);
const sock2 = join(tmpdir(), `orderdb-2-${process.pid}.sock`);
const p2 = await boot(sock2);

// 3) The new core must remember the resting order.
const after = (await rpc(sock2, 'orders.list'))?.orders ?? [];
const liveAfter = after.filter((o) => o.status === 'LIVE' || o.status === 'PENDING').length;
const restored = after.find((o) => o.internalKey === 'k-rest');
console.log('after restart: orders =', after.length, 'live =', liveAfter);
console.log('restored order present :', Boolean(restored), restored ? `(status=${restored.status}, key=${restored.internalKey})` : '');

p2.kill('SIGTERM');
await sleep(300);

// 4) The order log must contain the order with its state.
const logLines = existsSync(ORDER_LOG) ? readFileSync(ORDER_LOG, 'utf8').trim().split('\n').filter(Boolean) : [];
console.log('order log lines        :', logLines.length);

const ok = liveBefore >= 1 && Boolean(restored) && liveAfter >= 1;
console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — a live order survived a hard kill and was restored (no orphan)`);
process.exit(ok ? 0 : 1);
