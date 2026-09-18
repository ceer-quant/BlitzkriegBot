#!/usr/bin/env node
/**
 * E2-b (#27) acceptance on the REAL binary + the REAL dog cdylib. A strategy
 * may declare the shared entry-quality gates (round timing window, spot
 * momentum) unnecessary for its OWN candidates. The declaration must be:
 *
 *   1. explicit and visible at load (the load receipt names the waived gates);
 *   2. effective — with the timing window shut, dog_strategy still enters while
 *      the builtin, which declares nothing, stays gated;
 *   3. auditable on the wire — strategy gateExemptions + gateExemptedTiming and
 *      engine.stats.blocked.declaredExemptions;
 *   4. minimal — only the declared timing gate is ever waived (momentum stays 0).
 *
 * Everything runs in a scratch dir on a private socket: no production data,
 * no network, dry mode only.
 *
 * Usage: node scripts/strategy-gate-check.mjs
 *   (needs target/release/blitzkrieg-core and the dog cdylib built)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import net from 'net';
import { join } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync } from 'fs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const DYLIB = join(
  process.cwd(),
  'user_layer', 'strategies', 'target', 'release',
  process.platform === 'darwin' ? 'libdog_strategy.dylib'
    : process.platform === 'win32' ? 'dog_strategy.dll'
    : 'libdog_strategy.so',
);
const ROUND_SEC = 3600;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`); process.exit(2); }
if (!existsSync(DYLIB)) { console.error(`missing strategy library: ${DYLIB} ((cd user_layer/strategies && cargo build --release))`); process.exit(2); }

async function run() {
  const tag = `${process.pid}-${Math.random().toString(36).slice(2)}`;
  const sock = join(tmpdir(), `blitzkrieg-e2b-${tag}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-e2b-'));
  try { unlinkSync(sock); } catch {}

  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '50',
    '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
    // Scratch dir only: never restore, or leave behind, a real position/order.
    '--no-order-log', '--no-position-log',
    '--round-sec', String(ROUND_SEC),
    // Timing window deliberately SHUT for everyone: a 1-hour-old round minimum.
    '--min-round-age', '3600', '--min-time-left', '0',
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

  const problems = [];
  try {
    for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
    await connect();
    await rpc('core.ready');

    // 1. The declaration is explicit and visible at load, before enabling.
    const receipt = await rpc('strategy.load', { path: DYLIB });
    if (typeof receipt !== 'string' || !receipt.includes('dog_strategy')) {
      problems.push(`load receipt unexpected: ${JSON.stringify(receipt)}`);
    }
    if (typeof receipt !== 'string' || !receipt.includes('declares gate exemptions: timing')) {
      problems.push(`load receipt does not name the declared timing exemption: ${JSON.stringify(receipt)}`);
    }
    if (typeof receipt === 'string' && receipt.includes('momentum')) {
      problems.push(`dog declares no momentum exemption; receipt says otherwise: ${receipt}`);
    }
    const enabled = await rpc('strategy.enable', { name: 'dog_strategy', enabled: true });
    if (!enabled.found) problems.push('dog_strategy not found after load');

    // A round that is brand new (age 0) and a deep dip the dog trades (mid 0.42
    // <= its 0.43 ceiling, bid depth 100 >= its 50 minimum).
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
    await rpc('books.snapshot', {
      tokenId: 'UP',
      bids: [{ price: 0.41, size: 100 }],
      asks: [{ price: 0.43, size: 100 }],
    });

    // 2. The exemption is effective: wait for the dog's entry through the gate.
    //    In dry mode the maker-fill simulation can fill the maker order on the
    //    very next snapshot, so accept either a LIVE order or a recorded
    //    placement/fill on the strategy ledger — both prove the entry cleared
    //    the shut timing window.
    let dogEntry = null;
    for (let i = 0; i < 100 && !dogEntry; i++) {
      await sleep(50);
      const s2 = await rpc('engine.stats');
      dogEntry = (s2.strategies || []).find((s) => s.name === 'dog_strategy');
      const live = (await rpc('orders.list')).orders || [];
      const placed = live.some((o) => o.strategy === 'dog_strategy');
      if (!placed && (dogEntry?.ordersPlaced ?? 0) === 0 && (dogEntry?.openPositions ?? 0) === 0) {
        dogEntry = null;
      }
    }
    if (!dogEntry) problems.push('dog_strategy placed NO order through the shut timing window');

    // 3+4. Audit fields on the wire; the builtin stays fully gated.
    const stats = await rpc('engine.stats');
    const strategies = stats.strategies || [];
    const dog = strategies.find((s) => s.name === 'dog_strategy');
    const arb = strategies.find((s) => s.name === 'spread_arb');
    if (!dog) problems.push('engine.stats has no dog_strategy row');
    if (JSON.stringify(dog?.gateExemptions) !== JSON.stringify(['timing'])) {
      problems.push(`dog gateExemptions = ${JSON.stringify(dog?.gateExemptions)}, want ["timing"]`);
    }
    if (!(dog?.gateExemptedTiming >= 1)) {
      problems.push(`dog gateExemptedTiming = ${dog?.gateExemptedTiming}, want >= 1`);
    }
    if ((dog?.gateExemptedMomentum ?? 0) !== 0) {
      problems.push(`dog gateExemptedMomentum = ${dog.gateExemptedMomentum}, want 0 (not declared)`);
    }
    if (JSON.stringify(arb?.gateExemptions ?? null) !== JSON.stringify([])) {
      problems.push(`spread_arb gateExemptions = ${JSON.stringify(arb?.gateExemptions)}, want []`);
    }
    const declared = stats.blocked?.declaredExemptions ?? [];
    if (!declared.some((d) => d.strategy === 'dog_strategy' && JSON.stringify(d.gates) === JSON.stringify(['timing']))) {
      problems.push(`blocked.declaredExemptions lacks the dog/timing row: ${JSON.stringify(declared)}`);
    }
    if (declared.some((d) => d.strategy === 'spread_arb')) {
      problems.push('the builtin must never appear in declaredExemptions');
    }
    if (stderr.includes('ERROR') || stderr.includes('panicked')) {
      problems.push(`core stderr looked unhealthy: ${stderr.slice(-300)}`);
    }
  } catch (e) {
    problems.push(`error: ${e.message}`);
  } finally {
    proc.kill('SIGKILL');
    try { unlinkSync(sock); } catch {}
  }
  return problems;
}

console.log('per-strategy gate opt-out on the real binary + cdylib (E2-b / #27)\n');
const problems = await run();
if (problems.length) {
  for (const p of problems) console.log(`  FAIL ${p}`);
  console.log(`\nstrategy-gate: ${problems.length} problem(s)`);
  process.exit(1);
}
console.log('  ok   load receipt declares timing; dog enters a shut window; builtin stays gated; exemption counted, momentum untouched');
console.log('\nstrategy-gate: pass');
