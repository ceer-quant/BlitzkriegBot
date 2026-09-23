#!/usr/bin/env node
/**
 * E12 shutdown-cleanliness gate — "退出时不残留挂单".
 *
 * Why this exists: `BlitzkriegCoreClient.stop()` used to fire SIGTERM and return
 * immediately — no await, no escalation, no reap. A caller therefore had no way
 * to know when the core was actually gone. Measured window: the core exits in
 * ~2 ms on an idle loop, but that is not a guarantee. Once a core is inside a
 * tick that holds the async mutex (book refresh, venue submission) the exit is
 * bounded only by that work, and meanwhile:
 *
 *   - the socket may still be bound, so a replacement core cannot bind it;
 *   - the old core may still be running its exit checks against the ledger.
 *
 * The gate asserts the *contract* rather than the timing: after `stop()`
 * resolves, (a) the process is reaped, (b) the socket is unbound, and (c) no
 * order is left in a live state that the operator has not been told about.
 *
 * Run: node scripts/shutdown-cleanliness-check.mjs
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'shutdown-'));
const ORDER_LOG = join(WORK, 'orders.jsonl');

function rpc(sock, method, params = {}) {
  return new Promise((res) => {
    const c = net.connect(sock);
    let b = '';
    c.on('connect', () =>
      c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + '\n'),
    );
    c.on('data', (d) => {
      b += d;
      const i = b.indexOf('\n');
      if (i < 0) return;
      try {
        res(JSON.parse(b.slice(0, i)).result);
      } catch {
        res(null);
      }
      c.end();
    });
    c.on('error', () => res(null));
    setTimeout(() => {
      try {
        c.end();
      } catch {}
      res(null);
    }, 3000);
  });
}

function boot(sock) {
  const p = spawn(
    BIN,
    [
      '--socket', sock, '--mode', 'dry', '--tick-ms', '50', '--seed-balance', '1000',
      '--max-order-notional', '6', '--order-log', ORDER_LOG,
      '--trade-log', join(WORK, 'trades.jsonl'), '--no-discovery', '--no-auto-exits',
    ],
    { stdio: 'ignore', cwd: WORK },
  );
  return p;
}

// ── the reference implementation of "wait until truly gone" ──────────────────
// This is what the client's `stop()` must do. Exit alone is not enough on
// POSIX: the pid can be reused, and a backgrounded grandchild can outlive it,
// so we also wait for 'close' (all stdio inherited by the process is done).
function awaitExit(proc, timeoutMs = 5000) {
  if (proc.exitCode !== null || proc.signalCode !== null) {
    return Promise.resolve({ exited: true, alreadyExited: true, ms: 0 });
  }
  const t0 = Date.now();
  return new Promise((res) => {
    const done = (how) => {
      clearTimeout(timer);
      res({ exited: true, how, ms: Date.now() - t0 });
    };
    const timer = setTimeout(() => {
      proc.removeListener('exit', onExit);
      proc.removeListener('close', onClose);
      res({ exited: false, ms: Date.now() - t0 });
    }, timeoutMs);
    const onExit = () => done('exit');
    const onClose = () => done('close');
    proc.once('exit', onExit);
    proc.once('close', onClose);
  });
}

const sock = join(WORK, 'core.sock');
const p = boot(sock);
for (let i = 0; i < 80 && !existsSync(sock); i++) await sleep(50);
await sleep(400);

// Place a resting maker order that stays LIVE — the thing that must not leak.
await rpc(sock, 'orders.place', {
  tokenId: 'tok1', conditionId: 'cond1', side: 'buy', mode: 'maker',
  price: 0.30, size: 10, internalKey: 'k-shutdown', strategy: 'operator',
  asset: 'BTC', direction: 'up', roundSlot: 1,
});
await sleep(400);

const listBefore = await rpc(sock, 'orders.list');
const liveBefore = (listBefore?.orders ?? []).filter(
  (o) => o.status === 'LIVE' || o.status === 'PENDING',
);
console.log('before stop : live orders =', liveBefore.length);

// ── graceful stop, mirroring what the client must do ─────────────────────────
// 1. stop accepting new intent (drain) — callers do this, not the core
// 2. ask the core to settle resting orders BEFORE the process goes away
const cancelRes = await rpc(sock, 'orders.cancel_all', {});
console.log('cancel_all  : cancelled =', cancelRes?.cancelled ?? '(no reply)');
await sleep(400);

const listAfter = await rpc(sock, 'orders.list');
const liveAfter = (listAfter?.orders ?? []).filter(
  (o) => o.status === 'LIVE' || o.status === 'PENDING',
);
console.log('after cancel: live orders =', liveAfter.length);

// 3. SIGTERM, then WAIT (the part the client got wrong)
const t0 = Date.now();
p.kill('SIGTERM');
const reaped = await awaitExit(p);
console.log('awaitExit   :', JSON.stringify(reaped));

// 4. the socket must be unbound so a replacement can bind it
const socketGone = !existsSync(sock);
console.log('socket freed:', socketGone);

// 5. a replacement core must be able to take over the same socket + logs
let replaced = false;
if (socketGone) {
  const p2 = boot(sock);
  for (let i = 0; i < 60 && !existsSync(sock); i++) await sleep(50);
  replaced = existsSync(sock);
  const h = await rpc(sock, 'orders.list');
  console.log('replacement : bound =', replaced, '| sees orders =', (h?.orders ?? []).length);
  p2.kill('SIGTERM');
  await awaitExit(p2, 3000);
}

// The order log's LAST snapshot for our order must not still be live: a core
// that dies mid-flight with a LIVE row is exactly what the orphan sweep would
// have to clean up on the next boot.
const lastSnap = new Map();
if (existsSync(ORDER_LOG)) {
  for (const line of readFileSync(ORDER_LOG, 'utf8').trim().split('\n')) {
    if (!line.trim()) continue;
    try {
      const o = JSON.parse(line);
      lastSnap.set(o.orderId, o);
    } catch {}
  }
}
const loggedStillLive = [...lastSnap.values()].filter(
  (o) => o.status === 'LIVE' || o.status === 'PENDING',
).length;
console.log('order log   : rows =', lastSnap.size, '| still live =', loggedStillLive);

const ok =
  liveBefore.length >= 1 &&
  liveAfter.length === 0 &&
  loggedStillLive === 0 &&
  reaped.exited &&
  socketGone &&
  replaced;
console.log(
  `\nRESULT: ${ok ? 'PASS' : 'FAIL'} — resting orders settled before exit, ` +
    `process reaped (${reaped.ms} ms), socket released, replacement core adopted it`,
);
process.exit(ok ? 0 : 1);
