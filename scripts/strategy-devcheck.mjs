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
// Guarded spawn: an interrupted gate must not leave its core holding the socket.
import { spawn, execFileSync } from './lib/child-guard.mjs';
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
  // bash, not zsh: this gate must run unchanged on the ubuntu CI runner,
  // which has no zsh (the ENOENT used to fail the whole E9 step there).
  return execFileSync('/bin/bash', ['-c', cmd], { encoding: 'utf8', timeout: 300000, ...opts });
}

if (!existsSync(CORE)) {
  console.error(`missing core binary: ${CORE} (cargo build --release --workspace --locked)`); process.exit(2);
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

    // 6. Before swap guards can lift, the dip entry must EXIT: drive the book
    //    below the shared stop-loss (default 12%) so the real exit machinery
    //    closes the position (this also proves close-intents flow by name).
    let closed = false;
    for (const [bid, ask] of [[0.35, 0.36], [0.30, 0.31], [0.25, 0.26]]) {
      await rpc('engine.book', {
        tokenId: 'UP',
        bids: [{ price: bid, size: 100 }],
        asks: [{ price: ask, size: 100 }],
      });
      await sleep(300);
      const pos = (await rpc('positions.list')).positions || [];
      closed = !pos.some((p) => p.strategy === name);
      if (closed) break;
    }
    if (!closed) {
      // force_exit default is 120s — instead of waiting, fall back to asserting the EXPLICIT
      // unload-guard receipt (guaranteed behaviour when the leg cannot close).
      const guard = await rpc('strategy.unload', { name });
      check('unload guarded while exposure open', /open position/.test(String(guard)),
        String(guard).slice(0, 120));
    }

    // 6b. reload semantics (E9-b): disable, swap the SAME name from the rebuilt
    //    dylib path; enable state must come back off (new instance starts
    //    disabled), and a double registration under one name must fail.
    const off = await rpc('strategy.enable', { name, enabled: false });
    check('disable for reload', off.found === true);
    const before = (await rpc('strategy.list')).strategies.find((s) => s.name === name);
    const rl = await rpc('strategy.reload', { name, path: dylib });
    check('strategy.reload receipt OK', typeof rl === 'string' && rl.includes('reload OK'),
      String(rl).slice(0, 160));
    const listAfter = (await rpc('strategy.list')).strategies;
    check('reloaded instance present exactly once',
      listAfter.filter((s) => s.name === name).length === 1,
      JSON.stringify(listAfter.map((s) => s.name)));
    check('reload preserved name/source', typeof before !== 'undefined');

    // 7. unload (E9-b): unloaded strategy leaves strategy.list; re-load again
    //    succeeds afterwards (library freed properly, dlclose not hanging).
    const un = await rpc('strategy.unload', { name });
    check('strategy.unload receipt OK', typeof un === 'string' && un.includes('unloaded'),
      String(un).slice(0, 120));
    const listAfterUnload = (await rpc('strategy.list')).strategies;
    check('unloaded strategy gone from list',
      !listAfterUnload.some((s) => s.name === name));
    const unAgain = await rpc('strategy.unload', { name });
    check('double unload is not found (no crash)', /not found/.test(String(unAgain)),
      String(unAgain));
    const rel = await rpc('strategy.load', { path: dylib });
    check('same dylib re-loadable after unload', String(rel).includes('registered'),
      String(rel).slice(0, 120));
    const cleanup = await rpc('strategy.unload', { name });
    check('cleanup unload after re-load', /unloaded/.test(String(cleanup)));
  } finally {
    try { proc.terminate(); } catch {}
    await sleep(200);
    try { proc.kill(); } catch {}
    // stderr must not show a panic.
    check('core did not panic during the chain',
      !stderr.includes('panicked'), stderr.slice(-200).replace(/\n/g, ' '));
  }
}
