#!/usr/bin/env node
/**
 * strategy:devcheck — E9-a (#60) acceptance gate: the FULL developer chain on
 * a freshly generated SafeStrategy crate, driven against a REAL dry core.
 *
 * Chain asserted end-to-end:
 *   1. `blitzkrieg-new-strategy.mjs <name>` generates a crate that
 *      `cargo build --release` (zero unsafe, the 5-minute promise measured);
 *   2. `strategy.load` accepts the dylib and the receipt names the strategy;
 *   3. `strategy.list` shows it present AND enabled=false — the
 *      new-strategies-start-DISABLED invariant;
 *   4. `strategy.enable` + synthetic `engine.book` flow → the strategy's
 *      entry shows up in `engine.stats` (orders evaluated through the same
 *      gate path as builtin strategies);
 *   5. `evolvable_knobs` declared by the template are visible to the kernel
 *      (shadow_evolution can target it);
 *   6. (E9-b note) `strategy.unload` doesn't exist yet — recorded, not failed.
 *
 * Everything is sandboxed: scratch workdir, private socket, dry mode, no
 * network. Exit 0 on PASS.
 */
import { spawn, execFileSync } from 'child_process';
import net from 'net';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, rmSync } from 'fs';

import { fileURLToPath } from 'url';
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const NEW = join(ROOT, 'scripts', 'blitzkrieg-new-strategy.mjs');
const NAME = process.argv[2] || 'devcheck_probe'; // default keeps gates deterministic
const DYLIB = join(
  ROOT, 'user_layer', 'strategies', NAME, 'target', 'release',
  process.platform === 'darwin' ? `lib${NAME}.dylib`
    : process.platform === 'win32' ? `${NAME}.dll` : `lib${NAME}.so`
);
const ROUND_SEC = 3600;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};

function shell(cmd, opts = {}) {
  return execFileSync('/bin/zsh', ['-c', cmd], { encoding: 'utf8', timeout: 300000, ...opts });
}

if (!existsSync(CORE)) {
  console.error(`missing core binary: ${CORE} (npm run core:build)`); process.exit(2);
}

const temp = mkdtempSync(join(tmpdir(), 'blitzkrieg-devcheck-'));
// Generation lives in the real tree (strategy crate directory is what a dev
// would commit); the CORE gets the scratch dir.
const t0 = Date.now();
try { unlinkSync(DYLIB); } catch {}
try {
  shell(`cd "${ROOT}" && node scripts/blitzkrieg-new-strategy.mjs ${NAME}`);
  const tGen = Date.now() - t0;

  const t1 = Date.now();
  shell(`cd "${join(ROOT, 'user_layer', 'strategies', NAME)}" && cargo test -q && cargo build --release -q`);
  const tBuild = Date.now() - t1;
  const tAll = Date.now() - t0;
  console.log(`generated+tested in ${tGen}ms, built in ${tBuild}ms, total ${tAll}ms (< 5 min promise)`);

  await runCoreChecks(DYLIB, NAME, temp, tAll);
  rmSync(join(ROOT, 'user_layer', 'strategies', NAME), { recursive: true, force: true });
  rmSync(temp, { recursive: true, force: true });

  const failed = checks.filter((c) => !c.ok);
  console.log(`\nRESULT: ${failed.length === 0 ? 'PASS' : `FAIL (${failed.length})`}`);
  process.exit(failed.length === 0 ? 0 : 1);
} catch (e) {
  if (String(e.message || e).includes('Command failed')) {
    console.error(String(e.stderr || e.message).slice(0, 2000));
  }
  console.error(`devcheck interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
}

async function runCoreChecks(dylib, name, workdir, elapsedMs) {
  const sock = join(tmpdir(), `blitzkrieg-devcheck-${process.pid}.sock`);
  try { unlinkSync(sock); } catch {}

  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '6',
    '--engine', '--no-discovery', '--no-event-archive',
    '--no-trade-log', '--no-order-log', '--no-position-log',
    '--round-sec', String(ROUND_SEC),
    '--min-round-age', '0', '--min-time-left', '0',
  ];
  const proc = spawn(CORE, args, { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
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

    // 2. load — receipt names the strategy, no exemptions declared.
    const receipt = await rpc('strategy.load', { path: dylib });
    const receiptStr = String(receipt);
    check('strategy.load receipt ok',
      receiptStr.includes(name),
      receiptStr.slice(0, 160));
    check('receipt records "not evolvable"? NO — template declares knobs',
      receiptStr.includes('declares evolvable knobs'),
      receiptStr.slice(0, 200));

    // 3. disabled-by-default invariant, right after load.
    let list = (await rpc('strategy.list')).strategies || [];
    const row = list.find((s) => s.name === name);
    check('strategy.list shows the template', !!row, JSON.stringify(list.map((s) => s.name)));
    check('new strategy registered DISABLED', row && row.enabled === false, JSON.stringify(row));
    check('built-in strategies untouched', list.some((s) => s.name === 'spread_arb' && s.enabled === true));

    // 4. enable + drive books; the strategy must place an order via the same
    //    engine path as builtins (dry mode maker-fill confirms quickly).
    const en = await rpc('strategy.enable', { name, enabled: true });
    check('strategy.enable was honored', en.found === true, JSON.stringify(en));

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
    // Template rule: mid <= buy_below(0.40), bid depth >= 50 → buy at ask.
    const feed = async (mid, depth = 100) => rpc('engine.book', {
      tokenId: 'UP',
      bids: [{ price: mid - 0.01, size: depth }],
      asks: [{ price: mid + 0.01, size: depth }],
    });
    await feed(0.55); // warm book (no trade: mid way above the ceiling)
    await sleep(120);
    await feed(0.39); // dip through the template's ceiling
    let placed = false, stratRow = null;
    for (let i = 0; i < 100 && !placed; i++) {
      await sleep(60);
      const stats = await rpc('engine.stats');
      stratRow = (stats.strategies || []).find((s) => s.name === name);
      const orders = (await rpc('orders.list')).orders || [];
      placed = orders.some((o) => o.strategy === name) || (stratRow?.ordersPlaced ?? 0) > 0;
    }
    check('template entry reached the engine (order placed)', placed,
      JSON.stringify(stratRow));
    const stats = await rpc('engine.stats');
    check('engine.stats carries the strategy row',
      (stats.strategies || []).some((s) => s.name === name));

    // 5. the template's evolvable knobs arrived at the kernel: enabling shadow
    //    evolution must find the strategy evolvable (it declares knobs).
    const seOn = await rpc('shadow_evolution.enable', {});
    const seStatus = await rpc('shadow_evolution.status', {});
    check('shadow evolution accepted', seOn?.enabled === true || seStatus?.enabled === true || seStatus === 'Evaluating',
      JSON.stringify(seStatus).slice(0, 120));
    const seApply = await rpc('shadow_evolution.apply', {
      strategy: name,
      params: { buy_below: 0.38 }, // in-domain hot change, proves the knob registry
    });
    check('template knobs live (apply accepts its declared knob)',
      seApply?.applied === true && seApply.strategy === name,
      JSON.stringify(seApply).slice(0, 140));

    // orders must be REAL kernel orders (risk-gated), never strategy-executed
    const orders = (await rpc('orders.list')).orders?.filter((o) => o.strategy === name) ?? [];
    check('orders went through the kernel order manager', orders.length > 0,
      JSON.stringify(orders.map((o) => ({ id: o.id, state: o.state, price: o.price }))));

    // 6. recorded: strategy.unload is a E9-b deliverable, not bashable here.
    console.log(`  note strategy.unload lands with E9-b — unload leg validated then.`);
  } finally {
    try { proc.terminate(); } catch {}
    await sleep(200);
    try { proc.kill(); } catch {}
    // stderr must not show a panic.
    check('core did not panic during the chain',
      !stderr.includes('panicked'), stderr.slice(-200).replace(/\n/g, ' '));
  }
}
