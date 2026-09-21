#!/usr/bin/env node
/**
 * E2-c (#28) acceptance on the REAL binary + the REAL dog cdylib: Shadow
 * Evolution is **per strategy** end to end. What must hold:
 *
 *   1. A strategy's declaration is explicit and visible at load — the receipt
 *      names the evolvable knobs, and a library with none says "not evolvable"
 *      rather than silently participating;
 *   2. `shadow_evolution.status` reports each strategy's OWN block (params,
 *      knob domains, counters) next to the aggregate counters older consumers
 *      read — isolation is only observable if both are visible side by side;
 *   3. an operator apply to ONE strategy moves only that strategy's values,
 *      and is audited as manual in that strategy's OWN file;
 *   4. rollback is per strategy, and a strategy that never changed anything has
 *      nothing to roll back to (an explicit error, not a silent no-op);
 *   5. the audit files are separate: `data/evolution/<strategy>.jsonl`, with no
 *      write ever creating another strategy's file;
 *   6. an out-of-domain or over-gradient proposal is refused, so the domain
 *      really is a hard bound and the ±5% lock really applies.
 *
 * Everything runs in a scratch dir on a private socket: no production data, no
 * network. Dry mode only; Live is never reachable from here.
 *
 * Usage: node scripts/strategy-evolution-check.mjs
 *   (needs target/release/blitzkrieg-core and the dog cdylib built)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { requireFreshStrategyDylibs, strategyDylibPath } from './lib/strategy-dylib-freshness.mjs';
import net from 'net';
import { join } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, readFileSync, readdirSync } from 'fs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const DYLIB = strategyDylibPath('dog_strategy');

const ROUND_SEC = 3600;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`); process.exit(2); }

// #207: this gate loads the `dog` cdylib by explicit path. The library — not the
// binary — carries the knob declarations and the per-strategy audit behaviour it
// asserts, so a stale one makes every claim below describe a previous build.
requireFreshStrategyDylibs({ gate: 'strategy-evolution-check', require: ['dog_strategy'] });

async function run() {
  const tag = `${process.pid}-${Math.random().toString(36).slice(2)}`;
  const sock = join(tmpdir(), `blitzkrieg-e2c-${tag}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-e2c-'));
  try { unlinkSync(sock); } catch {}

  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '50',
    '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
    // Scratch dir only: never restore, or leave behind, a real position/order.
    '--no-order-log', '--no-position-log',
    '--round-sec', String(ROUND_SEC),
    // Keep the evolution loop responsive: the knob swap is what is under test,
    // not the sample-count gate (which the unit/integration tests cover).
    '--shadow-evolution', '--se-min-samples', '2', '--se-cooldown-secs', '0',
    '--se-min-obs-secs', '0',
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
  /** Reject expected: resolves to the error message, or null when it succeeded. */
  const refuses = async (method, params) => {
    try { await rpc(method, params); return null; }
    catch (e) { return e.message; }
  };
  const auditDir = join(workdir, 'data', 'evolution');
  const readAudit = (strategy) => {
    const p = join(auditDir, `${strategy}.jsonl`);
    if (!existsSync(p)) return null;
    return readFileSync(p, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
  };

  const problems = [];
  try {
    for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
    await connect();
    await rpc('core.ready');

    // ── 1. The declaration is explicit at load ─────────────────────────────
    const receipt = await rpc('strategy.load', { path: DYLIB });
    if (typeof receipt !== 'string' || !receipt.includes('dog_strategy')) {
      problems.push(`load receipt unexpected: ${JSON.stringify(receipt)}`);
    }
    if (typeof receipt !== 'string' || !receipt.includes('declares evolvable knobs: trendMaxEntryPrice')) {
      problems.push(`load receipt does not name the declared evolvable knob: ${JSON.stringify(receipt)}`);
    }
    const enabled = await rpc('strategy.enable', { name: 'dog_strategy', enabled: true });
    if (!enabled.found) problems.push('dog_strategy not found after load');
    await rpc('strategy.enable', { name: 'spread_arb', enabled: true });

    // ── 2. Status is per strategy ──────────────────────────────────────────
    let st = await rpc('shadow_evolution.status');
    const blocks = st.strategies || [];
    const dog = blocks.find((b) => b.strategy === 'dog_strategy');
    const arb = blocks.find((b) => b.strategy === 'spread_arb');
    if (!dog) problems.push(`status has no dog_strategy block: ${JSON.stringify(blocks.map((b) => b.strategy))}`);
    if (!arb) problems.push(`status has no spread_arb block: ${JSON.stringify(blocks.map((b) => b.strategy))}`);
    if (dog && dog.params?.trendMaxEntryPrice !== '0.43') {
      problems.push(`dog's declared value = ${JSON.stringify(dog.params)}, want trendMaxEntryPrice "0.43"`);
    }
    const dogKnob = (dog?.knobs || []).find((k) => k.name === 'trendMaxEntryPrice');
    if (!dogKnob) problems.push(`dog's block does not carry its own knob domain: ${JSON.stringify(dog?.knobs)}`);
    // Decimals travel as strings and are NORMALISED (0.90 -> "0.9"), so compare
    // numerically: the domain is a value, not a formatting contract.
    else if (Number(dogKnob.min) !== 0.05 || Number(dogKnob.max) !== 0.9) {
      problems.push(`dog's domain = ${dogKnob.min}..${dogKnob.max}, want 0.05..0.90`);
    }
    // Aggregate keys stay for older consumers.
    for (const k of ['status', 'currentParams', 'variantCount', 'evolutionsApplied', 'evolutionsRejected']) {
      if (!(k in st)) problems.push(`status lost the aggregate key ${k}`);
    }

    // ── 3. Per-strategy apply moves only that strategy ─────────────────────
    const arbBefore = arb?.params ?? {};
    // +3% on dog's knob: inside the ±5% gradient lock AND inside 0.05..0.90.
    const applied = await rpc('shadow_evolution.apply', {
      strategy: 'dog_strategy', params: { trendMaxEntryPrice: '0.4429' },
    });
    if (applied?.strategy !== 'dog_strategy') {
      problems.push(`apply did not report the strategy: ${JSON.stringify(applied)}`);
    }
    st = await rpc('shadow_evolution.status');
    const dog2 = (st.strategies || []).find((b) => b.strategy === 'dog_strategy');
    const arb2 = (st.strategies || []).find((b) => b.strategy === 'spread_arb');
    if (dog2?.params?.trendMaxEntryPrice !== '0.4429') {
      problems.push(`dog's value after apply = ${JSON.stringify(dog2?.params)}, want "0.4429"`);
    }
    if (JSON.stringify(arb2?.params) !== JSON.stringify(arbBefore)) {
      problems.push(`spread_arb moved when dog was applied: ${JSON.stringify(arbBefore)} -> ${JSON.stringify(arb2?.params)}`);
    }

    // ── 4+6. The locks really bind ─────────────────────────────────────────
    const oob = await refuses('shadow_evolution.apply', {
      strategy: 'dog_strategy', params: { trendMaxEntryPrice: '0.99' },
    });
    if (!oob) problems.push('an out-of-domain proposal (0.99 > max 0.90) was ACCEPTED');
    const grad = await refuses('shadow_evolution.apply', {
      strategy: 'dog_strategy', params: { trendMaxEntryPrice: '0.60' },
    });
    if (!grad) problems.push('a +35% jump was ACCEPTED despite the ±5% gradient lock');
    const unknown = await refuses('shadow_evolution.apply', {
      strategy: 'no_such_strategy', params: { x: '0.1' },
    });
    if (!unknown) problems.push('applying to a non-evolvable strategy was ACCEPTED');
    st = await rpc('shadow_evolution.status');
    const dog3 = (st.strategies || []).find((b) => b.strategy === 'dog_strategy');
    if (dog3?.params?.trendMaxEntryPrice !== '0.4429') {
      problems.push(`a refused proposal still moved the value: ${JSON.stringify(dog3?.params)}`);
    }

    // ── 5. Separate audit files ────────────────────────────────────────────
    const dogLog = readAudit('dog_strategy');
    if (!dogLog) problems.push(`no audit file for dog_strategy in ${auditDir}`);
    else if (!dogLog.every((r) => r.strategy === 'dog_strategy')) {
      problems.push(`dog's audit file contains another strategy's record: ${JSON.stringify(dogLog.map((r) => r.strategy))}`);
    } else if (!dogLog.some((r) => r.manual === true)) {
      problems.push(`the operator override left no manual record: ${JSON.stringify(dogLog)}`);
    }
    const history = await rpc('shadow_evolution.history', { strategy: 'dog_strategy', limit: 20 });
    if (!(history.history || []).length) problems.push('history for dog_strategy is empty after an apply');
    if ((history.history || []).some((r) => r.strategy !== 'dog_strategy')) {
      problems.push('history for dog_strategy returned another strategy\'s records');
    }
    const otherHistory = await rpc('shadow_evolution.history', { strategy: 'spread_arb', limit: 20 });
    if ((otherHistory.history || []).length !== 0) {
      problems.push(`spread_arb has history but nothing was applied to it: ${JSON.stringify(otherHistory.history)}`);
    }

    // ── 4. Rollback is per strategy ────────────────────────────────────────
    const rolled = await rpc('shadow_evolution.rollback', { strategy: 'dog_strategy' });
    if (rolled?.rolledBack !== true || rolled?.strategy !== 'dog_strategy') {
      problems.push(`rollback response unexpected: ${JSON.stringify(rolled)}`);
    }
    st = await rpc('shadow_evolution.status');
    const dog4 = (st.strategies || []).find((b) => b.strategy === 'dog_strategy');
    if (dog4?.params?.trendMaxEntryPrice !== '0.43') {
      problems.push(`rollback did not restore 0.43: ${JSON.stringify(dog4?.params)}`);
    }
    // A strategy with nothing to roll back to is an explicit error, not a
    // silent no-op that dog's rollback could be mistaken for.
    const arbRollback = await refuses('shadow_evolution.rollback', { strategy: 'spread_arb' });
    if (!arbRollback) problems.push('rollback of spread_arb (never changed) was ACCEPTED');
    const noStrategy = await refuses('shadow_evolution.rollback', {});
    if (!noStrategy) problems.push('rollback without a strategy was ACCEPTED');
    // After the rollback, dog's own file gained a rollback record — still only dog.
    const dogLog2 = readAudit('dog_strategy') || [];
    if (!dogLog2.some((r) => r.rollback === true)) {
      problems.push(`rollback left no record in dog's file: ${JSON.stringify(dogLog2.map((r) => r.rollback))}`);
    }
    if (!dogLog2.every((r) => r.strategy === 'dog_strategy')) {
      problems.push('dog\'s file gained another strategy\'s record after rollback');
    }

    // ── 6'. Disabling detaches the overlay: nothing evolves while off ──────
    await rpc('shadow_evolution.disable');
    const off = await rpc('shadow_evolution.status');
    if (off.status !== 'disabled') {
      problems.push(`status after disable = ${off.status}, want "disabled"`);
    }
    // The declared values stay published (the overlay is a no-op, not an
    // invented parameter) — enabling it again must not move a value.
    const dogOff = (off.strategies || []).find((b) => b.strategy === 'dog_strategy');
    if (dogOff?.params?.trendMaxEntryPrice !== '0.43') {
      problems.push(`disabling changed a value: ${JSON.stringify(dogOff?.params)}`);
    }
    await rpc('shadow_evolution.enable');
    const back = await rpc('shadow_evolution.status');
    const dogBack = (back.strategies || []).find((b) => b.strategy === 'dog_strategy');
    if (dogBack?.params?.trendMaxEntryPrice !== '0.43') {
      problems.push(`re-enabling changed a value: ${JSON.stringify(dogBack?.params)}`);
    }

    // No strategy's file may ever appear for a strategy that never changed.
    const files = existsSync(auditDir) ? readdirSync(auditDir) : [];
    if (files.includes('spread_arb.jsonl')) {
      problems.push('spread_arb.jsonl exists although spread_arb never changed anything');
    }
    if (!files.includes('dog_strategy.jsonl')) {
      problems.push(`dog_strategy.jsonl missing from ${auditDir}: ${JSON.stringify(files)}`);
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

console.log('per-strategy Shadow Evolution on the real binary + cdylib (E2-c / #28)\n');
const problems = await run();
if (problems.length) {
  for (const p of problems) console.log(`  FAIL ${p}`);
  console.log(`\nstrategy-evolution: ${problems.length} problem(s)`);
  process.exit(1);
}
console.log('  ok   load declares the knob; status is per strategy; apply/rollback move only their own');
console.log('  ok   domain + ±5% gradient refuse; audit files separate; disable is a no-op, not a mutation');
console.log('\nstrategy-evolution: pass');
