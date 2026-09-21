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
import { requireFreshStrategyDylibs, strategyDylibPath } from './lib/strategy-dylib-freshness.mjs';
import net from 'net';
import { join } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, rmSync } from 'fs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const DYLIB = strategyDylibPath('dog_strategy');
const ROUND_SEC = 3600;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`); process.exit(2); }

// #207: the dog cdylib is loaded by explicit path and is the SUBJECT of this
// gate (its sizing declarations and gate exemptions). Assert the library under
// test was built from this checkout before reporting anything about it.
requireFreshStrategyDylibs({ gate: 'strategy-gate-check', require: ['dog_strategy'] });

/**
 * Spawn a dry core on `sock` in `workdir`, wait for the socket, connect, and
 * return `{ proc, rpc, stderr }`. `stderr` is a THUNK, not a snapshot: the phase
 * that spawns the core checks it after driving the core, and a captured string
 * would be frozen at connect time. Shared by both phases so the second one
 * cannot drift from the first in how it boots or tears down.
 */
async function bootCore(sock, workdir, args) {
  const proc = spawn(BIN, args, { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
  let stderr = '';
  proc.stderr.on('data', (d) => { stderr += d.toString(); });

  let sockc = null, buf = '', seq = 0;
  const pending = new Map();
  const rpc = (method, params = {}) => new Promise((res, rej) => {
    const id = ++seq; pending.set(id, { resolve: res, reject: rej });
    sockc.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
  await new Promise((res, rej) => {
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
  await rpc('core.ready');
  return { proc, rpc, stderr: () => stderr };
}

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
    // `--min-time-left 0` keeps this a TooYoung-only block, which is exactly the
    // branch D-31 must leave waivable — the floor phase below covers the other one.
    '--min-round-age', '3600', '--min-time-left', '0',
  ];
  const { proc, rpc, stderr: stderrNow } = await bootCore(sock, workdir, args);
  const problems = [];
  try {

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
    // <= its 0.43 ceiling, bid depth 100 >= its 50 minimum). The market's
    // DECLARED expiry is 30 minutes out — the kernel's time_left follows the
    // venue declaration (not the wall-clock slot grid), so the dog's D-31 floor
    // (180s) is deterministically satisfied no matter when this gate runs.
    const now = Date.now();
    const slot = Math.floor(now / 1000 / ROUND_SEC);
    await rpc('engine.markets', {
      markets: [{
        asset: 'BTC', conditionId: '0xc', questionId: '0xq',
        upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
        expiresAtMs: now + 30 * 60 * 1000, roundSlot: slot,
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
    // D-31: the floor the dog declares must be visible on the wire too. Without
    // this, an operator cannot tell a bounded opt-out from an unbounded one.
    if (dog?.gateExemptionTimingFloorSec !== 180) {
      problems.push(`dog gateExemptionTimingFloorSec = ${JSON.stringify(dog?.gateExemptionTimingFloorSec)}, want 180`);
    }
    const declared = stats.blocked?.declaredExemptions ?? [];
    if (!declared.some((d) => d.strategy === 'dog_strategy' && JSON.stringify(d.gates) === JSON.stringify(['timing']))) {
      problems.push(`blocked.declaredExemptions lacks the dog/timing row: ${JSON.stringify(declared)}`);
    }
    if (!declared.some((d) => d.strategy === 'dog_strategy' && d.timingFloorSec === 180)) {
      problems.push(`blocked.declaredExemptions lacks the dog's timingFloorSec=180: ${JSON.stringify(declared)}`);
    }
    if (declared.some((d) => d.strategy === 'spread_arb')) {
      problems.push('the builtin must never appear in declaredExemptions');
    }
    const stderr = stderrNow();
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

/**
 * D-31 phase: the declared floor must actually STOP an opt-out at the closing
 * window. Phase 1 proves the exemption still reaches a "too young" block (where
 * `time_left_sec` is large); this proves the other half — that a shut *time-left*
 * gate is now respected by the very strategy that waives the timing window.
 *
 * The scanner derives `time_left_sec` from wall-clock round slots, so the test
 * cannot simply "set" it. Instead the round is made SHORT (60 s) and the global
 * time-left gate is set well above a whole round (400 s): every instant is then
 * inside the closing window, deterministically. The dog's own floor is 180 s, so
 * its opt-out must not reach in — while phase 1 shows it still reaches a
 * too-young block. The block must be a *timing* block, not merely "no order"
 * (which would also pass if the strategy had simply found nothing to do).
 */
async function runFloorPhase() {
  const tag = `${process.pid}-${Math.random().toString(36).slice(2)}`;
  const sock = join(tmpdir(), `blitzkrieg-d31-${tag}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-d31-'));
  try { unlinkSync(sock); } catch {}

  const FLOOR_ROUND_SEC = 60;
  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '50',
    '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
    '--no-order-log', '--no-position-log',
    '--round-sec', String(FLOOR_ROUND_SEC),
    // Round age is fine; the shutting gate is the TIME-LEFT one, the exact
    // branch D-31 narrows. 400 > a whole 60 s round ⇒ it is shut at every instant.
    '--min-round-age', '0', '--min-time-left', '400',
  ];
  const { proc, rpc, stderr } = await bootCore(sock, workdir, args);
  const problems = [];
  try {
    const receipt = await rpc('strategy.load', { path: DYLIB });
    // The floor travels in the load receipt, so it is known before the strategy
    // is ever enabled.
    if (typeof receipt !== 'string' || !receipt.includes('timing floor 180s')) {
      problems.push(`load receipt does not name the declared floor: ${JSON.stringify(receipt)}`);
    }
    await rpc('strategy.enable', { name: 'dog_strategy', enabled: true });

    const now = Date.now();
    const slot = Math.floor(now / 1000 / FLOOR_ROUND_SEC);
    await rpc('engine.markets', {
      markets: [{
        asset: 'BTC', conditionId: '0xc', questionId: '0xq',
        upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
        expiresAtMs: (slot + 1) * FLOOR_ROUND_SEC * 1000, roundSlot: slot,
        negRisk: true, question: 'BTC up/down',
      }],
    });
    await rpc('books.snapshot', {
      tokenId: 'UP',
      bids: [{ price: 0.41, size: 100 }],
      asks: [{ price: 0.43, size: 100 }],
    });
    // The engine evaluates on its own tick; give it several ticks and require
    // that the dog was timing-blocked (so "no order" cannot pass by inaction).
    let dog = null;
    for (let i = 0; i < 60; i++) {
      await sleep(50);
      const s = await rpc('engine.stats');
      dog = (s.strategies || []).find((x) => x.name === 'dog_strategy');
      if ((s.blocked?.byStrategy?.dog_strategy?.timing ?? 0) >= 1) break;
    }

    const stats = await rpc('engine.stats');
    const dogRow = (stats.strategies || []).find((s) => s.name === 'dog_strategy');
    if (!dogRow) problems.push('engine.stats has no dog_strategy row');
    if ((dogRow?.gateExemptedTiming ?? 0) !== 0) {
      problems.push(`dog gateExemptedTiming = ${dogRow.gateExemptedTiming}, want 0 inside its own floor`);
    }
    if ((dogRow?.ordersPlaced ?? 0) !== 0) {
      problems.push(`dog placed ${dogRow.ordersPlaced} order(s) inside its declared floor`);
    }
    const live = ((await rpc('orders.list')).orders || []).filter((o) => o.strategy === 'dog_strategy');
    if (live.length) problems.push(`dog has ${live.length} live order(s) inside its declared floor`);
    // The block must be attributable to TIMING, not to the strategy simply finding
    // nothing to do.
    if (!(stats.blocked?.byStrategy?.dog_strategy?.timing >= 1)) {
      problems.push(`dog was not timing-blocked: ${JSON.stringify(stats.blocked?.byStrategy?.dog_strategy)}`);
    }
    const stderrText = stderr();
    if (stderrText.includes('ERROR') || stderrText.includes('panicked')) {
      problems.push(`core stderr looked unhealthy: ${stderrText.slice(-300)}`);
    }
  } catch (e) {
    problems.push(`error: ${e.message}`);
  } finally {
    proc.kill('SIGKILL');
    try { unlinkSync(sock); } catch {}
  }
  return problems;
}

/**
 * Zero-default + persistence phase: a fresh boot enables NOTHING (the kernel
 * couples to no strategy), an operator's enable is recorded to the state file,
 * and the NEXT boot replays it — the whole "enable it once" contract, driven
 * through the real strategy dir (all four shipped cdylibs auto-load disabled)
 * and a state file in a scratch dir.
 */
async function runPersistencePhase() {
  const sock = join(tmpdir(), `blitzkrieg-persist-${Math.random().toString(36).slice(2)}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-persist-'));
  const statePath = join(workdir, 'strategy-state.json');
  try { unlinkSync(sock); } catch {}
  const problems = [];

  const boot = async () => {
    const { proc, rpc } = await bootCore(sock, workdir, [
      '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
      '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
      '--no-order-log', '--no-position-log',
      // The whole shipped strategy dir: every cdylib registers, none enables.
      '--strategy-dir', join(process.cwd(), 'user_layer', 'strategies', 'target', 'release'),
      '--strategy-state', statePath,
    ]);
    return { proc, rpc };
  };
  const enabled = async (rpc, name) => {
    const s = await rpc('engine.stats');
    const row = (s.strategies || []).find((x) => x.name === name);
    return row ? row.enabled === true : null;
  };

  try {
    // 1. Fresh boot: everything registered, NOTHING enabled.
    let { proc, rpc } = await boot();
    if (!(await enabled(rpc, 'trend_follow') === false)) {
      problems.push('a fresh boot must start with trend_follow disabled');
    }
    if (!(await enabled(rpc, 'spread_arb') === false)) {
      problems.push('a fresh boot must start with spread_arb disabled');
    }

    // 2. The operator enables trend_follow; the file records it immediately.
    const en = await rpc('strategy.enable', { name: 'trend_follow', enabled: true });
    if (en?.found !== true) problems.push(`strategy.enable failed: ${JSON.stringify(en)}`);
    if (!(await enabled(rpc, 'trend_follow') === true)) problems.push('enable did not take effect');
    proc.kill('SIGTERM');
    await sleep(500);

    // 3. Reboot on the same state file: trend_follow comes back enabled, everything
    //    else stays off — no RPC needed.
    ({ proc, rpc } = await boot());
    if (!(await enabled(rpc, 'trend_follow') === true)) {
      problems.push('the persisted enable did not survive the restart');
    }
    if (!(await enabled(rpc, 'spread_arb') === false)) {
      problems.push('an unpersisted strategy must stay disabled after restart');
    }
    proc.kill('SIGTERM');
  } catch (e) {
    problems.push(`error: ${e.message}`);
  } finally {
    try { rmSync(workdir, { recursive: true, force: true }); } catch {}
  }
  return problems;
}

console.log('per-strategy gate opt-out on the real binary + cdylib (E2-b / #27, D-31 floor)\n');
const problems = [...(await run()), ...(await runFloorPhase()), ...(await runPersistencePhase())];
if (problems.length) {
  for (const p of problems) console.log(`  FAIL ${p}`);
  console.log(`\nstrategy-gate: ${problems.length} problem(s)`);
  process.exit(1);
}
console.log('  ok   load receipt declares timing; dog enters a shut window; builtin stays gated; exemption counted, momentum untouched');
console.log('  ok   D-31 floor: the load receipt names 180s; in a closing window the same opt-out is timing-blocked, nothing exempted');
console.log('\nstrategy-gate: pass');
