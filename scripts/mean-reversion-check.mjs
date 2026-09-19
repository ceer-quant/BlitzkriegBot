#!/usr/bin/env node
/**
 * E4-b (#31) acceptance on the REAL binary: the fade leg (`mean_reversion`) is a
 * first-class strategy, not a variant of the dip buyer. What must hold, all
 * observed over the IPC wire against a real dry-mode engine:
 *
 *   1. independent start/stop — `strategy.list` shows three builtins with the
 *      fade leg OFF by default (adding it cannot change what an existing
 *      session trades), `--enable-strategy mean_reversion` starts a session with
 *      it ON, and `strategy.enable` toggles it at runtime. Enabling one never
 *      moves the others;
 *   2. independent accounting — `engine.stats.strategies[]` carries its own row
 *      with its own orders, attributed by name, and `source = builtin`;
 *   3. it fades a crash by RESTING a bid below the falling mid (the inverse of
 *      the chase leg's lift), and it is the fade leg that takes a deep fall the
 *      dip buyer's spread/TrendConfig path would refuse;
 *   4. no starvation — with all three enabled and each presented the setup it
 *      wants on a different asset, all three enter in the same cycle;
 *   5. it declares a `momentum` gate exemption and it is HONOURED: with spot
 *      falling against the entry the shared momentum gate would stop it — the
 *      row shows `gateExemptedMomentum > 0` and the order still places. The
 *      timing gate is NOT waived;
 *   6. it is evolvable like any other strategy — Shadow Evolution carries its
 *      own six declared knobs (read off the live instance, so present even while
 *      the strategy is off), and a runtime toggle neither adds nor drops that
 *      cell.
 *
 * Everything runs in a scratch dir on a private socket: no production data, no
 * network, dry mode only. Live is never reachable from here.
 *
 * Usage: node scripts/mean-reversion-check.mjs
 *   (needs target/release/blitzkrieg-core built)
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

if (!existsSync(BIN)) { console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`); process.exit(2); }

/**
 * Spawn a core, hand the connected RPC client to `body`, always tear down.
 * `extraArgs` is where a startup flag under test (e.g. --enable-strategy) goes.
 */
async function session(tag, extraArgs, body) {
  const sock = join(tmpdir(), `blitzkrieg-e4b-${tag}-${process.pid}.sock`);
  const workdir = mkdtempSync(join(tmpdir(), `blitzkrieg-e4b-${tag}-`));
  try { unlinkSync(sock); } catch {}

  const args = [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '50',
    '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
    // Scratch dir only: never restore, or leave behind, a real position/order.
    '--no-order-log', '--no-position-log',
    '--round-sec', String(ROUND_SEC),
    // Round-window gates off. The core derives time_left from the WALL clock
    // (the declared expiresAtMs is not plumbed to the scanner), so with a
    // 3600s round this check fails for the three minutes before every hour:
    // entries are refused with "Too close to expiry" and the assertions below
    // collapse into "placed NO entry". Pinning the round boundary is not what
    // this gate is about — the fade entry and its pricing are — so open the
    // window the way every other engine gate does.
    '--min-round-age', '0', '--min-time-left', '0',
    ...extraArgs,
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
    return await body({ rpc, stderr: () => stderr });
  } finally {
    proc.kill('SIGKILL');
    try { unlinkSync(sock); } catch {}
  }
}

const rowFor = (stats, name) => (stats.strategies || []).find((s) => s.name === name);
const isOn = (list, name) => (list.strategies || []).find((s) => s.name === name)?.enabled;

/**
 * Declare a round's markets in one shot. `specs` is one entry per asset:
 * `{ asset, upToken, downToken, upBid, upAsk, downBid, downAsk }`. A side is
 * left untraded when its bid is null. Feed every asset in the SAME call: the
 * handler replaces the whole market list, so two calls would drop the first.
 */
async function setMarkets(rpc, now, specs) {
  const slot = Math.floor(now / 1000 / ROUND_SEC);
  await rpc('engine.markets', {
    markets: specs.map((s) => ({
      asset: s.asset, conditionId: `0xc-${s.asset}`, questionId: `0xq-${s.asset}`,
      upTokenId: s.upToken, downTokenId: s.downToken, upPrice: 0.5, downPrice: 0.5,
      expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
      negRisk: true, question: `${s.asset} up/down`,
    })),
  });
  for (const s of specs) {
    if (s.upBid != null) {
      await rpc('books.snapshot', { tokenId: s.upToken, bids: [{ price: s.upBid, size: 100 }], asks: [{ price: s.upAsk, size: 100 }] });
    }
    if (s.downBid != null) {
      await rpc('books.snapshot', { tokenId: s.downToken, bids: [{ price: s.downBid, size: 100 }], asks: [{ price: s.downAsk, size: 100 }] });
    }
  }
}

/** One market, BTC, tokens UP/DOWN — the shape most sections only need. */
async function setMarket(rpc, { now, upBid, upAsk, downBid, downAsk }) {
  await setMarkets(rpc, now, [
    { asset: 'BTC', upToken: 'UP', downToken: 'DOWN', upBid, upAsk, downBid, downAsk },
  ]);
}

/**
 * Feed a crash: the token's bid sinks 0.50 → 0.30 on the real cent grid. The
 * fade leg needs a lookback-high drop >= 10% within 120s, so every step below
 * the first extends the window the entry is measured against.
 */
async function feedCrash(rpc, tokenId = 'UP', steps = 12) {
  for (let i = 0; i <= steps; i++) {
    const bid = Math.round((0.50 - i / 75) * 100) / 100;
    await rpc('books.snapshot', { tokenId, bids: [{ price: bid, size: 100 }], asks: [{ price: Math.round((bid + 0.01) * 100) / 100, size: 100 }] });
    await sleep(40);
  }
}

/**
 * Hold a book steady for `ms`. The dip buyer's candidate list only contains
 * trend-CONFIRMED tokens, and confirmation is a rolling window, so the side
 * meant to dip has to be held above the threshold for the whole window first.
 */
async function feedHold(rpc, tokenId, bid, ask, ms, stepMs = 40) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    await rpc('books.snapshot', { tokenId, bids: [{ price: bid, size: 100 }], asks: [{ price: ask, size: 100 }] });
    await sleep(stepMs);
  }
}

/** Wait until `pick(rpc)` returns something truthy, or give up. */
async function settle(rpc, pick, tries = 120) {
  for (let i = 0; i < tries; i++) {
    await sleep(50);
    const v = await pick(rpc);
    if (v) return v;
  }
  return null;
}

const problems = [];

async function main() {
  // ── 1. Default registration + independent start/stop ───────────────────────
  await session('default', [], async ({ rpc }) => {
    const list = await rpc('strategy.list');
    // Zero-default: the kernel couples to no strategy — a fresh boot enables
    // nothing, whatever the strategy dir holds.
    for (const s of ['spread_arb', 'trend_follow', 'mean_reversion']) {
      if (isOn(list, s)) problems.push(`${s} must start DISABLED on a fresh boot`);
    }
    // Runtime toggle: on, then off again — nothing else moves with it.
    const on = await rpc('strategy.enable', { name: 'mean_reversion', enabled: true });
    if (!on.found) problems.push('strategy.enable did not find mean_reversion');
    let l = await rpc('strategy.list');
    if (!isOn(l, 'mean_reversion') || isOn(l, 'spread_arb') || isOn(l, 'trend_follow') !== false) {
      problems.push(`enabling the fade leg must not enable any other strategy: ${JSON.stringify(l)}`);
    }
    const off = await rpc('strategy.enable', { name: 'mean_reversion', enabled: false });
    if (!off.found) problems.push('strategy.enable did not find mean_reversion on the way off');
    l = await rpc('strategy.list');
    if (isOn(l, 'mean_reversion') !== false || isOn(l, 'spread_arb')) {
      problems.push('disabling the fade leg must leave everything else untouched');
    }
    const junk = await rpc('strategy.enable', { name: 'dog_strategy', enabled: true });
    if (junk.found) problems.push('an unhosted strategy name must not report found');
  });

  // ── 2. Startup flag: the session comes up with the fade leg ON ─────────────
  let sawStartupRow = false;
  await session('flag', ['--enable-strategy', 'mean_reversion'], async ({ rpc }) => {
    const list = await rpc('strategy.list');
    if (!isOn(list, 'mean_reversion')) {
      problems.push('--enable-strategy mean_reversion did not switch it on at startup');
    }
    if (isOn(list, 'spread_arb')) problems.push('--enable-strategy mean_reversion must not also enable spread_arb');
    if (isOn(list, 'trend_follow') !== false) {
      problems.push('--enable-strategy mean_reversion must not enable trend_follow');
    }

    // ── 3. It fades the crash with a resting bid BELOW the falling mid ──────
    const now = Date.now();
    await setMarket(rpc, { now, upBid: 0.50, upAsk: 0.51 });
    await feedCrash(rpc);

    const fade = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const row = rowFor(stats, 'mean_reversion');
      return (row?.ordersPlaced ?? 0) > 0 ? row : null;
    });
    if (!fade) {
      const stats = await rpc('engine.stats');
      const live = (await rpc('orders.list')).orders || [];
      problems.push(
        'mean_reversion placed NO entry on a deep, tight, cheap crash ' +
        `(row=${JSON.stringify(rowFor(stats, 'mean_reversion'))} orders=${JSON.stringify(live.map((o) => [o.strategy, o.tokenId, o.price, o.status]))})`,
      );
      return;
    }

    // Independent accounting: its row exists, is attributed by name, and the
    // entry is a resting bid below the mid on the crashing token.
    const stats = await rpc('engine.stats');
    const row = rowFor(stats, 'mean_reversion');
    sawStartupRow = true;
    if (row.source !== 'builtin' && !row.source?.startsWith('dylib')) {
      problems.push(`mean_reversion source = ${row.source}, want "builtin" or "dylib:*"`);
    }
    const orders = (await rpc('orders.list')).orders || [];
    const entry = orders.find((o) => o.strategy === 'mean_reversion');
    if (!entry) problems.push('no mean_reversion order in orders.list');
    else if (entry.tokenId !== 'UP') problems.push(`the fade leg should hold the crashing UP token, got ${entry.tokenId}`);
    const rv = await rpc('engine.round');
    const px = (rv?.marketPrices || []).find((m) => m.asset === 'BTC');
    if (px && !(entry.price < Number(px.up) * 0.99)) {
      problems.push(`the fade entry should rest below the falling mid: entry=${entry.price} mid=${px.up}`);
    }
    const arb = rowFor(stats, 'spread_arb');
    if (!arb) problems.push('spread_arb row vanished');
  });
  if (!sawStartupRow) problems.push('never observed a mean_reversion accounting row');

  // ── 4. Neither starves the others: three builtins, three setups, all enter ─
  await session('concurrent',
    // The dip buyer confirms over a rolling window: shorten it (and drop the
    // floor) so the check stays fast without weakening what it asserts.
    ['--trend-confirm-sec', '10', '--trend-window-floor-ms', '0',
     '--enable-strategy', 'mean_reversion', '--enable-strategy', 'trend_follow',
     '--enable-strategy', 'spread_arb'],
    async ({ rpc, stderr }) => {
    const now = Date.now();
    // Three ASSETS again: the risk layer allows at most one open position per
    // asset, so shared assets would prove nothing. BTC crashes (fade leg),
    // ETH breaks out (chase leg), SOL dips (spread_arb).
    await setMarkets(rpc, now, [
      { asset: 'BTC', upToken: 'BTC-UP', downToken: 'BTC-DOWN', upBid: 0.50, upAsk: 0.51 },
      { asset: 'ETH', upToken: 'ETH-UP', downToken: 'ETH-DOWN', upBid: 0.50, upAsk: 0.51 },
      { asset: 'SOL', upToken: 'SOL-UP', downToken: 'SOL-DOWN', downBid: 0.61, downAsk: 0.63 },
    ]);
    // Fill the dip buyer's confirmation window on SOL-DOWN BEFORE the dip.
    await feedHold(rpc, 'SOL-DOWN', 0.61, 0.63, 10_500);
    await feedCrash(rpc, 'BTC-UP');
    for (let i = 0; i <= 12; i++) {
      const bid = Math.round((0.50 + i / 100) * 100) / 100;
      await rpc('books.snapshot', { tokenId: 'ETH-UP', bids: [{ price: bid, size: 100 }], asks: [{ price: Math.round((bid + 0.01) * 100) / 100, size: 100 }] });
      await sleep(40);
    }
    await rpc('books.snapshot', { tokenId: 'SOL-DOWN', bids: [{ price: 0.43, size: 100 }], asks: [{ price: 0.45, size: 100 }] });

    const all = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const f = rowFor(stats, 'mean_reversion');
      const t = rowFor(stats, 'trend_follow');
      const a = rowFor(stats, 'spread_arb');
      return (f?.ordersPlaced ?? 0) > 0 && (t?.ordersPlaced ?? 0) > 0 && (a?.ordersPlaced ?? 0) > 0 ? { f, t, a } : null;
    }, 300);
    if (!all) {
      const stats = await rpc('engine.stats');
      const live = (await rpc('orders.list')).orders || [];
      problems.push(
        `strategies starved each other: mean_reversion=${JSON.stringify(rowFor(stats, 'mean_reversion'))} ` +
        `trend_follow=${JSON.stringify(rowFor(stats, 'trend_follow'))} spread_arb=${JSON.stringify(rowFor(stats, 'spread_arb'))} ` +
        `placeRejected=${stats.placeRejected} orders=${JSON.stringify(live.map((o) => [o.strategy, o.tokenId, o.price, o.status]))} ` +
        `stderr=${stderr().slice(-300)}`,
      );
    } else {
      const byStrategy = {};
      for (const o of ((await rpc('orders.list')).orders || [])) byStrategy[o.strategy] = o.tokenId;
      if (byStrategy.mean_reversion !== 'BTC-UP') {
        problems.push(`the fade leg should hold the BTC crash, got ${JSON.stringify(byStrategy)}`);
      }
      if (byStrategy.trend_follow !== 'ETH-UP') {
        problems.push(`the chase leg should hold the ETH breakout, got ${JSON.stringify(byStrategy)}`);
      }
      if (byStrategy.spread_arb !== 'SOL-DOWN') {
        problems.push(`the dip buyer should hold the SOL dip, got ${JSON.stringify(byStrategy)}`);
      }
    }
  });

  // ── 5. Genuinely waived: spot falling against the fade, the order places ───
  await session('waived', ['--enable-strategy', 'mean_reversion'], async ({ rpc }) => {
    const now = Date.now();
    await setMarket(rpc, { now, upBid: 0.50, upAsk: 0.51 });
    // Spot falls hard over the momentum window — exactly what the shared gate
    // reads as momentum against a BUY — then the same crash arrives.
    for (let i = 0; i < 10; i++) {
      await rpc('spot.price', { asset: 'BTC', price: 60000 - i * 10 });
      await sleep(30);
    }
    await feedCrash(rpc);

    const waived = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const row = rowFor(stats, 'mean_reversion');
      return (row?.gateExemptedMomentum ?? 0) > 0 ? row : null;
    }, 120);
    if (!waived) {
      const stats = await rpc('engine.stats');
      problems.push(
        'the momentum exemption was never recorded for the fade leg ' +
        `(row=${JSON.stringify(rowFor(stats, 'mean_reversion'))}, blocked=${JSON.stringify(stats.blocked)})`,
      );
    }
    const stats = await rpc('engine.stats');
    const row = rowFor(stats, 'mean_reversion');
    if ((row?.ordersPlaced ?? 0) === 0) {
      problems.push(`the spot gate still blocked the fade leg's entry: ${JSON.stringify(row)}`);
    }
    if (JSON.stringify(row?.gateExemptions || []) !== JSON.stringify(['momentum'])) {
      problems.push(`mean_reversion gateExemptions = ${JSON.stringify(row?.gateExemptions)}, want ["momentum"]`);
    }
    if ((row?.gateExemptedTiming ?? 0) !== 0) {
      problems.push(`mean_reversion must NOT waive the timing gate: ${JSON.stringify(row)}`);
    }
  });

  // ── 6. Shadow Evolution sees it with its own knobs, enabled or not ────────
  const seArgs = ['--shadow-evolution', '--se-min-samples', '2', '--se-cooldown-secs', '0', '--se-min-obs-secs', '0'];
  const wantKnobs = ['lookback_sec', 'min_drop_pct', 'max_price', 'entry_factor', 'max_spread_pct', 'cooldown_sec'];
  const readSe = async (rpc) => {
    const st = await rpc('shadow_evolution.status', {});
    const rows = st.strategies || [];
    const names = rows.map((s) => s.strategy ?? s.name);
    const row = rows.find((s) => (s.strategy ?? s.name) === 'mean_reversion');
    const knobs = (row?.knobs || row?.declaredKnobs || []).map((k) => k.name);
    return { names, row, knobs };
  };
  await session('se-default', seArgs, async ({ rpc }) => {
    const { names, knobs } = await readSe(rpc);
    for (const want of ['spread_arb', 'trend_follow', 'mean_reversion']) {
      if (!names.includes(want)) problems.push(`evolution lost ${want}: ${JSON.stringify(names)}`);
    }
    if (knobs.length && JSON.stringify(knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`mean_reversion knobs = ${JSON.stringify(knobs)}, want ${JSON.stringify(wantKnobs)}`);
    }
    if (!knobs.length) problems.push(`no knob declaration for mean_reversion: ${JSON.stringify(names)}`);
  });
  // Starting it on must not disturb the declaration, and starting it off again
  // must not remove the cell: same names, same knob order, both ways.
  await session('se-on', [...seArgs, '--enable-strategy', 'mean_reversion'], async ({ rpc }) => {
    const on = await readSe(rpc);
    if (!on.names.includes('mean_reversion')) {
      problems.push(`an enabled fade leg must be evolved: ${JSON.stringify(on.names)}`);
    }
    if (JSON.stringify(on.knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`enabled mean_reversion knobs = ${JSON.stringify(on.knobs)}, want ${JSON.stringify(wantKnobs)}`);
    }
    await rpc('strategy.enable', { name: 'mean_reversion', enabled: false });
    const off = await readSe(rpc);
    if (!off.names.includes('mean_reversion')) {
      problems.push(`disabling dropped the fade leg's parameter cell: ${JSON.stringify(off.names)}`);
    }
    if (JSON.stringify(off.knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`the toggle changed the declaration: ${JSON.stringify(off.knobs)}`);
    }
  });
}

console.log('mean_reversion as a first-class strategy on the real binary (E4-b / #31)\n');
await main();
if (problems.length) {
  for (const p of problems) console.log(`  FAIL ${p}`);
  console.log(`\nmean-reversion: ${problems.length} problem(s)`);
  process.exit(1);
}
console.log('  ok   starts off and toggles alone; --enable-strategy starts it on');
console.log('  ok   owns its accounting row and rests its fade bid below the falling mid');
console.log('  ok   three builtins enter together, one setup each, no starvation');
console.log('  ok   momentum gate exemption declared and honoured; timing never waived');
console.log('  ok   evolved with its own six knobs, and a toggle neither adds nor drops the cell');
console.log('\nmean-reversion: pass');
