#!/usr/bin/env node
/**
 * E2-a acceptance on the REAL binary: `--strategy-limit` must accept both the
 * legacy 3-segment caps form and the extended 6-segment sizing form, and the
 * sizing it applies must be visible on the wire. Three cases, each on its own
 * private socket in a scratch dir (no production data, no network):
 *
 *   1. caps only        `spread_arb:2:-`          → global lot, sizingSource "global"
 *   2. in-band override `spread_arb:2:-:1:4:4`    → 4-share lot, sizingSource "strategy"
 *   3. greedy override  `spread_arb:2:-:100:0:999`→ clamped to the global 2.5u / [10,10]
 *
 * Each case drives the real engine to a trend-confirmed dip so an order is
 * placed, then asserts the order size plus `engine.stats.strategies[]`
 * (maxOpenPositions / sizingSource / effectiveSizeUsd / effectiveMin|MaxShares).
 * Any banner on stderr means the core rejected the flag as malformed.
 *
 * Usage: node scripts/strategy-limit-check.mjs   (needs target/release built)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import net from 'net';
import { join } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync } from 'fs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const ROUND_SEC = 3600;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN} (cargo build --release)`); process.exit(2); }

/** One full engine cycle with `limitFlag` applied; returns the observed state. */
async function probe(limitFlag) {
  const tag = `${process.pid}-${Math.random().toString(36).slice(2)}`;
  const sock = join(tmpdir(), `blitzkrieg-e2a-${tag}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-e2a-'));
  try { unlinkSync(sock); } catch {}

  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '50',
    '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
    '--round-sec', String(ROUND_SEC), '--min-round-age', '0', '--min-time-left', '0',
    '--trend-confirm-sec', '3', '--trend-window-floor-ms', '1000',
    // The global band every override is measured against. The global notional
    // budget has no CLI flag — it is the CoreConfig default of 2.5u.
    '--min-shares', '10', '--max-shares', '10',
    '--strategy-limit', limitFlag,
  ];
  const proc = spawn(BIN, args, { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
  let stderr = '';
  proc.stderr.on('data', (d) => { stderr += d.toString(); });

  let sockc = null, buf = '', seq = 0;
  const pending = new Map();
  const rpc = (method, params = {}) => new Promise((res, rej) => {
    const id = ++seq; pending.set(id, { resolve: res, reject: rej });
    sockc.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  const connect = () => new Promise((res, rej) => {
    sockc = net.connect(sock, () => res());
    sockc.on('error', rej);
    sockc.on('data', (d) => {
      buf += d.toString(); let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        if (!line.trim()) continue;
        let msg; try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id != null && pending.has(msg.id)) {
          const p = pending.get(msg.id); pending.delete(msg.id);
          msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
        }
      }
    });
  });

  try {
    for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
    await connect();
    await rpc('core.ready');

    const now = Date.now();
    const slot = Math.floor(now / 1000 / ROUND_SEC);
    await rpc('engine.markets', {
      markets: [{
        asset: 'BTC', conditionId: '0xc', questionId: '0xq',
        upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
        expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
        negRisk: true, question: 'BTC up/down',
      }],
    });
    // Confirmed UP trend, then a dip to mid 0.435 → the strategy rests its bid
    // at its own discount; only the order's presence and size are asserted.
    const book = (b, a) => ({ tokenId: 'UP', bids: [{ price: b, size: 100 }], asks: [{ price: a, size: 100 }] });
    for (let i = 0; i < 14; i++) { await rpc('books.snapshot', book(0.57, 0.58)); await sleep(300); }
    await rpc('books.snapshot', book(0.43, 0.44));
    await sleep(500);

    const orders = (await rpc('orders.list')).orders.filter((o) => o.status === 'LIVE');
    const stats = await rpc('engine.stats');
    const sa = (stats.strategies || []).find((s) => s.name === 'spread_arb') || {};
    return {
      rejected: stderr.includes('blitzkrieg-core: ignoring'),
      size: orders.length ? Number(orders[0].size) : null,
      maxOpenPositions: sa.maxOpenPositions ?? null,
      sizingSource: sa.sizingSource ?? null,
      sizeUsd: sa.effectiveSizeUsd ?? null,
      minShares: sa.effectiveMinShares ?? null,
      maxShares: sa.effectiveMaxShares ?? null,
    };
  } catch (e) {
    return { error: e.message };
  } finally {
    proc.kill('SIGKILL');
  }
}

let failures = 0;
let passed = 0;

async function check(label, flag, want) {
  const got = await probe(flag);
  const problems = [];
  if (got.error) problems.push(`error: ${got.error}`);
  else {
    if (got.rejected) problems.push('core rejected the flag as malformed');
    if (got.size !== want.size) problems.push(`order size ${got.size} != ${want.size}`);
    if (got.sizingSource !== want.sizingSource) problems.push(`sizingSource ${JSON.stringify(got.sizingSource)} != ${JSON.stringify(want.sizingSource)}`);
    if (got.maxOpenPositions !== want.maxOpenPositions) problems.push(`maxOpenPositions ${JSON.stringify(got.maxOpenPositions)} != ${JSON.stringify(want.maxOpenPositions)}`);
    if (got.sizeUsd !== want.sizeUsd) problems.push(`effectiveSizeUsd ${got.sizeUsd} != ${want.sizeUsd}`);
    if (got.minShares !== want.minShares) problems.push(`effectiveMinShares ${got.minShares} != ${want.minShares}`);
    if (got.maxShares !== want.maxShares) problems.push(`effectiveMaxShares ${got.maxShares} != ${want.maxShares}`);
  }
  const ok = problems.length === 0;
  ok ? passed++ : failures++;
  console.log(`${ok ? '  ok  ' : '  FAIL'} ${label}`);
  console.log(`         --strategy-limit ${flag}`);
  if (got.error) console.log(`         ${got.error}`);
  else console.log(`         order=${got.size} source=${got.sizingSource} maxOpen=${JSON.stringify(got.maxOpenPositions)} band=[${got.minShares},${got.maxShares}] @${got.sizeUsd}u`);
  for (const p of problems) console.log(`         ! ${p}`);
  return ok;
}

console.log('strategy-limit grammar + effective sizing (E2-a)\n');
await check('legacy caps-only form keeps the global lot', 'spread_arb:2:-',
  { size: 10, sizingSource: 'global', maxOpenPositions: 2, sizeUsd: 2.5, minShares: 10, maxShares: 10 });
await check('in-band sizing override wins', 'spread_arb:2:-:1:4:4',
  { size: 4, sizingSource: 'strategy', maxOpenPositions: 2, sizeUsd: 1, minShares: 4, maxShares: 4 });
await check('greedy override is clamped to the global risk band', 'spread_arb:2:-:100:0:999',
  { size: 10, sizingSource: 'strategy', maxOpenPositions: 2, sizeUsd: 2.5, minShares: 10, maxShares: 10 });

const total = passed + failures;
console.log(`\nRESULT: ${failures === 0 ? 'PASS' : 'FAIL'} — ${passed}/${total} cases`);
process.exit(failures === 0 ? 0 : 1);
