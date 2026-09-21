#!/usr/bin/env node
/** Smoke test: --trade-log override and --no-trade-log keep prod ledger clean. */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const PROD = join(process.cwd(), 'data', 'trades', 'trades.jsonl');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const prodLines = () => (existsSync(PROD) ? readFileSync(PROD, 'utf8').trim().split('\n').filter(Boolean).length : 0);

function rpc(sock, method, params = {}) {
  return new Promise((res) => {
    const c = net.connect(sock); let b = '';
    c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + '\n'));
    c.on('data', (d) => { b += d; const i = b.indexOf('\n'); if (i < 0) return;
      try { res(JSON.parse(b.slice(0, i)).result); } catch { res(null); } c.end(); });
    c.on('error', () => res(null)); setTimeout(() => { try { c.end(); } catch {} res(null); }, 3000);
  });
}

async function runCore(extra, sock, work) {
  try { unlinkSync(sock); } catch {}
  // #199: run the core INSIDE the temp work dir. It used to inherit the gate's
  // cwd (the repo root), where the ledgers this gate does not override — the
  // order log, the position log, the strategy-state file — all resolve to
  // `data/…` and were silently written into the production tree on every run.
  // The trade log stays explicit (that is what this gate asserts) and PROD below
  // is still the real repo-root ledger, so the assertion is unchanged.
  const p = spawn(BIN, ['--socket', sock, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '100', ...extra], { stdio: 'ignore', cwd: work });
  for (let i = 0; i < 60; i++) { if (existsSync(sock)) break; await sleep(50); }
  await sleep(300);
  // Open a position, then close it at a profit so a closed trade is recorded.
  // The FOK buy needs resting depth: mirror an ask at the entry price first.
  await rpc(sock, 'books.snapshot', { tokenId: 'tok', bids: [], asks: [{ price: 0.4, size: 100 }] });
  await rpc(sock, 'orders.place', { tokenId: 'tok', conditionId: 'c', side: 'buy', mode: 'taker', price: 0.4, size: 5, internalKey: 'k', strategy: 't', asset: 'BTC', direction: 'up', roundSlot: 1 });
  await rpc(sock, 'books.snapshot', { tokenId: 'tok', bids: [{ price: 0.95, size: 100 }], asks: [{ price: 0.99, size: 100 }] });
  await sleep(600);
  const trades = await rpc(sock, 'trades.history', { limit: 0 });
  p.kill('SIGTERM');
  await sleep(300);
  return (trades?.trades || []).length;
}

const base = prodLines();
console.log('prod ledger before     :', base);

// 1) explicit --trade-log under a temp dir
const dir = mkdtempSync(join(tmpdir(), 'tl-test-'));
const tlPath = join(dir, 'trades.jsonl');
const n1 = await runCore(['--trade-log', tlPath], join(tmpdir(), `tl-${process.pid}.sock`), dir);
console.log(`--trade-log <tmp>     : core saw ${n1} trade(s); file lines = ${existsSync(tlPath) ? readFileSync(tlPath,'utf8').trim().split('\n').filter(Boolean).length : 0}`);
console.log('prod ledger after (1) :', prodLines());

// 2) --no-trade-log
const n2 = await runCore(['--no-trade-log'], join(tmpdir(), `nl-${process.pid}.sock`), dir);
console.log(`--no-trade-log        : core saw ${n2} trade(s) (in-memory)`);
console.log('prod ledger after (2) :', prodLines());

const ok = prodLines() === base && n1 >= 1 && n2 >= 1;
console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — prod ledger untouched (${base} -> ${prodLines()}), both flags honoured`);
process.exit(ok ? 0 : 1);
