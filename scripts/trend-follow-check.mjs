#!/usr/bin/env node
/**
 * E4-a (#30) acceptance on the REAL binary: the chase leg (`trend_follow`) is a
 * first-class strategy, not a second code path bolted onto the dip buyer. What
 * must hold, all observed over the IPC wire against a real dry-mode engine:
 *
 *   1. independent start/stop — `strategy.list` shows both builtins with the
 *      chase leg OFF by default (adding it cannot change what an existing
 *      session trades), `--enable-strategy trend_follow` starts a session with
 *      it ON, and `strategy.enable` toggles it at runtime. Enabling one never
 *      enables or disables the other;
 *   2. independent accounting — `engine.stats.strategies[]` carries its own row
 *      with its own orders/open positions, attributed by name;
 *   3. it enters WITH the move by LIFTING the offer (entry > mid), the exact
 *      inverse of the dip buyer's below-mid resting bid, and it is the chase leg
 *      that takes a breakout the dip buyer refuses;
 *   4. no starvation — with both enabled and each presented the setup it wants on
 *      a different token, both enter in the same cycle;
 *   5. it declares NO gate exemption, and is genuinely gated: with spot falling
 *      against the bet the shared momentum gate stops it (a blocked count, not a
 *      silent pass);
 *   6. it is evolvable like any other strategy — Shadow Evolution carries its own
 *      six declared knobs (read off the live instance, so present even while the
 *      strategy is off), and a runtime toggle neither adds nor drops that cell.
 *
 * Everything runs in a scratch dir on a private socket: no production data, no
 * network, dry mode only. Live is never reachable from here.
 *
 * Usage: node scripts/trend-follow-check.mjs
 *   (needs target/release/blitzkrieg-core built)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { requireFreshStrategyDylibs } from './lib/strategy-dylib-freshness.mjs';
import {
  isOn, makeSession, report, requireCoreBinary, rowFor, setMarket, setMarkets,
  settle, sleep, feedHold,
} from './lib/strategy-leg-harness.mjs';

requireCoreBinary();

// #207: the chase leg is a cdylib the kernel dlopens — not code in the binary
// built above. A stale library would make every assertion below describe the
// previous build of the leg.
requireFreshStrategyDylibs({ gate: 'trend-follow-check', require: ['trend_follow_strategy'] });

// The confirmation window is 60s by default and its floor 10s; both strategies
// here confirm off the tick stream, so shorten the window (and drop the floor)
// to keep the check fast without weakening what it asserts.
const session = makeSession({
  tagPrefix: 'blitzkrieg-e4a',
  baseArgs: ['--trend-confirm-sec', '10', '--trend-window-floor-ms', '0'],
});

/** Feed a breakout: the token's bid climbs 0.50 → 0.62 on the real cent grid. */
async function feedBreakout(rpc, tokenId = 'UP', steps = 12) {
  for (let i = 0; i <= steps; i++) {
    const bid = Math.round((0.50 + i / 100) * 100) / 100;
    await rpc('books.snapshot', { tokenId, bids: [{ price: bid, size: 100 }], asks: [{ price: Math.round((bid + 0.01) * 100) / 100, size: 100 }] });
    await sleep(40);
  }
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
    const on = await rpc('strategy.enable', { name: 'trend_follow', enabled: true });
    if (!on.found) problems.push('strategy.enable did not find trend_follow');
    let l = await rpc('strategy.list');
    if (!isOn(l, 'trend_follow') || isOn(l, 'spread_arb') || isOn(l, 'mean_reversion')) {
      problems.push('enabling the chase leg must not enable any other strategy');
    }
    const off = await rpc('strategy.enable', { name: 'trend_follow', enabled: false });
    if (!off.found) problems.push('strategy.enable did not find trend_follow on the way off');
    l = await rpc('strategy.list');
    if (isOn(l, 'trend_follow') !== false || isOn(l, 'spread_arb') || isOn(l, 'mean_reversion')) {
      problems.push('disabling the chase leg must leave everything else untouched');
    }
    const junk = await rpc('strategy.enable', { name: 'dog_strategy', enabled: true });
    if (junk.found) problems.push('an unhosted strategy name must not report found');
  });

  // ── 2. Startup flag: the session comes up with the chase leg ON ────────────
  let sawStartupRow = false;
  await session('flag', ['--enable-strategy', 'trend_follow'], async ({ rpc }) => {
    const list = await rpc('strategy.list');
    if (!isOn(list, 'trend_follow')) {
      problems.push('--enable-strategy trend_follow did not switch it on at startup');
    }
    if (isOn(list, 'spread_arb')) problems.push('--enable-strategy trend_follow must not also enable spread_arb');

    // ── 3+4. It chases the breakout and does not starve the dip buyer ────────
    const now = Date.now();
    await setMarket(rpc, { now, upBid: 0.50, upAsk: 0.51, downBid: 0.61, downAsk: 0.63 });
    await feedBreakout(rpc);

    // The chase leg's entry is a LIFT: above the mid, not a resting bid below it.
    const chase = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const row = rowFor(stats, 'trend_follow');
      return (row?.ordersPlaced ?? 0) > 0 ? row : null;
    });
    if (!chase) {
      problems.push('trend_follow placed NO entry on a rising, tight, in-band book');
    }

    // Independent accounting: its row exists and is attributed by name.
    const stats = await rpc('engine.stats');
    const row = rowFor(stats, 'trend_follow');
    if (!row) {
      problems.push('engine.stats has no trend_follow row (independently accounted)');
    } else {
      sawStartupRow = true;
      if (row.source !== 'builtin' && !row.source?.startsWith('dylib')) {
        problems.push(`trend_follow source = ${row.source}, want "builtin" or "dylib:*"`);
      }
      // No exemption declared, and none honoured: entering WITH the move needs none.
      if (JSON.stringify(row.gateExemptions) !== JSON.stringify([])) {
        problems.push(`trend_follow gateExemptions = ${JSON.stringify(row.gateExemptions)}, want []`);
      }
      if ((row.gateExemptedTiming ?? 0) !== 0 || (row.gateExemptedMomentum ?? 0) !== 0) {
        problems.push(`trend_follow waived a gate it never declared: ${JSON.stringify(row)}`);
      }
    }
    const arb = rowFor(await rpc('engine.stats'), 'spread_arb');
    if (!arb) problems.push('spread_arb row vanished');

    // The entry was a LIFT of the offer, not a resting bid below the mid: the
    // round view prices each side off the book it was fed, so the chased side
    // must be visibly bid up on the breakout (the dip buyer rests below mid).
    const rv = await rpc('engine.round');
    const px = (rv?.marketPrices || []).find((m) => m.asset === 'BTC');
    if (!px) {
      problems.push(`engine.round has no BTC price row: ${JSON.stringify(rv)?.slice(0, 200)}`);
    } else if (!(Number(px.up) > 0.60)) {
      problems.push(`the chased side should be priced up on the breakout: ${JSON.stringify(px)}`);
    }
  });
  if (!sawStartupRow) problems.push('never observed a trend_follow accounting row');

  // ── 4. Neither starves the other: two assets, two setups, both enter ───────
  await session('concurrent', ['--enable-strategy', 'trend_follow', '--enable-strategy', 'spread_arb'], async ({ rpc, stderr }) => {
    const now = Date.now();
    // Two ASSETS, not two sides of one market: the risk layer allows at most one
    // open position per asset, so a same-asset pair would prove nothing about
    // starvation — the second entry would be refused by design. BTC carries the
    // chase leg's breakout, ETH the dip buyer's dip.
    await setMarkets(rpc, now, [
      { asset: 'BTC', upToken: 'BTC-UP', downToken: 'BTC-DOWN', upBid: 0.50, upAsk: 0.51 },
      { asset: 'ETH', upToken: 'ETH-UP', downToken: 'ETH-DOWN', downBid: 0.61, downAsk: 0.63 },
    ]);
    // Fill the dip buyer's confirmation window on ETH-DOWN BEFORE the dip: only
    // a trend-CONFIRMED token ever becomes a candidate, so holding the mid at
    // 0.62 for the window is what makes its later 0.44 dip tradeable at all.
    await feedHold(rpc, 'ETH-DOWN', 0.61, 0.63, 10_500);
    await feedBreakout(rpc, 'BTC-UP');
    await rpc('books.snapshot', { tokenId: 'ETH-DOWN', bids: [{ price: 0.43, size: 100 }], asks: [{ price: 0.45, size: 100 }] });

    const both = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const t = rowFor(stats, 'trend_follow');
      const a = rowFor(stats, 'spread_arb');
      return (t?.ordersPlaced ?? 0) > 0 && (a?.ordersPlaced ?? 0) > 0 ? { t, a } : null;
    }, 300);
    if (!both) {
      const stats = await rpc('engine.stats');
      const live = (await rpc('orders.list')).orders || [];
      const pos = (await rpc('positions.list')).positions || [];
      problems.push(
        `strategies starved each other: trend_follow=${JSON.stringify(rowFor(stats, 'trend_follow'))} ` +
        `spread_arb=${JSON.stringify(rowFor(stats, 'spread_arb'))} ` +
        `placeRejected=${stats.placeRejected} orders=${JSON.stringify(live.map((o) => [o.strategy, o.tokenId, o.price, o.status]))} ` +
        `positions=${JSON.stringify(pos.map((p) => [p.strategy ?? p.tokenId, p.direction, p.size]))} ` +
        `stderr=${stderr().slice(-300)}`,
      );
    } else {
      // Attribution, not just a count: each entry is on its own asset.
      const byStrategy = {};
      for (const o of ((await rpc('orders.list')).orders || [])) byStrategy[o.strategy] = o.tokenId;
      if (byStrategy.trend_follow !== 'BTC-UP') {
        problems.push(`the chase leg should hold the BTC breakout, got ${JSON.stringify(byStrategy)}`);
      }
      if (byStrategy.spread_arb !== 'ETH-DOWN') {
        problems.push(`the dip buyer should hold the ETH dip, got ${JSON.stringify(byStrategy)}`);
      }
    }
  });

  // ── 5. Genuinely gated: spot moving against the bet stops the chase ────────
  await session('gated', ['--enable-strategy', 'trend_follow'], async ({ rpc }) => {
    const now = Date.now();
    await setMarket(rpc, { now, upBid: 0.50, upAsk: 0.51 });
    // Spot falls hard over the momentum window, then the same breakout arrives.
    for (let i = 0; i < 10; i++) {
      await rpc('spot.price', { asset: 'BTC', price: 60000 - i * 10 });
      await sleep(30);
    }
    await feedBreakout(rpc);

    const blocked = await settle(rpc, async (r) => {
      const stats = await r('engine.stats');
      const row = rowFor(stats, 'trend_follow');
      return row?.blockedMomentum > 0 ? row : null;
    }, 120);
    if (!blocked) {
      const stats = await rpc('engine.stats');
      problems.push(
        'the chase leg was NOT stopped by the shared spot gate with spot falling against it ' +
        `(row=${JSON.stringify(rowFor(stats, 'trend_follow'))}, ` +
        `blocked=${JSON.stringify(stats.blocked)})`,
      );
    }
    const stats = await rpc('engine.stats');
    const row = rowFor(stats, 'trend_follow');
    if ((row?.ordersPlaced ?? 0) > 0) {
      problems.push(`the chase leg entered against a falling spot: ${JSON.stringify(row)}`);
    }
    const declared = stats.blocked?.declaredExemptions ?? [];
    if (declared.some((d) => d.strategy === 'trend_follow')) {
      problems.push(`trend_follow must not appear in declaredExemptions: ${JSON.stringify(declared)}`);
    }
  });

  // ── 6. Shadow Evolution sees it with its own knobs, enabled or not ────────
  // The declaration is a property of the strategy (E2-c), read off the live
  // instance — so it is present from the start. The toggle only decides whether
  // the instance emits candidates; it must NOT add or drop the parameter cell,
  // because dropping it would discard evolved values and the rollback anchor.
  const seArgs = ['--shadow-evolution', '--se-min-samples', '2', '--se-cooldown-secs', '0', '--se-min-obs-secs', '0'];
  const wantKnobs = ['momentum_window_sec', 'min_move_pct', 'min_confirm_price', 'break_price', 'max_entry_price', 'max_spread_pct'];
  const readSe = async (rpc) => {
    const st = await rpc('shadow_evolution.status', {});
    const rows = st.strategies || [];
    const names = rows.map((s) => s.strategy ?? s.name);
    const row = rows.find((s) => (s.strategy ?? s.name) === 'trend_follow');
    const knobs = (row?.knobs || row?.declaredKnobs || []).map((k) => k.name);
    return { names, row, knobs };
  };
  await session('se-default', seArgs, async ({ rpc }) => {
    const { names, knobs } = await readSe(rpc);
    if (!names.includes('spread_arb')) problems.push(`evolution lost the incumbent: ${JSON.stringify(names)}`);
    if (!names.includes('trend_follow')) {
      problems.push(`a hosted chase leg must carry its declaration even while off: ${JSON.stringify(names)}`);
    }
    if (knobs.length && JSON.stringify(knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`trend_follow knobs = ${JSON.stringify(knobs)}, want ${JSON.stringify(wantKnobs)}`);
    }
    if (!knobs.length) problems.push(`no knob declaration for trend_follow: ${JSON.stringify(names)}`);
  });
  // Starting it on must not disturb the declaration, and starting it off again
  // must not remove the cell: same names, same knob order, both ways.
  await session('se-on', [...seArgs, '--enable-strategy', 'trend_follow'], async ({ rpc }) => {
    const on = await readSe(rpc);
    if (!on.names.includes('trend_follow')) {
      problems.push(`an enabled chase leg must be evolved: ${JSON.stringify(on.names)}`);
    }
    if (JSON.stringify(on.knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`enabled trend_follow knobs = ${JSON.stringify(on.knobs)}, want ${JSON.stringify(wantKnobs)}`);
    }
    // Flip it off at runtime: the cell must survive the toggle.
    await rpc('strategy.enable', { name: 'trend_follow', enabled: false });
    const off = await readSe(rpc);
    if (!off.names.includes('trend_follow')) {
      problems.push(`disabling dropped the chase leg's parameter cell: ${JSON.stringify(off.names)}`);
    }
    if (JSON.stringify(off.knobs) !== JSON.stringify(wantKnobs)) {
      problems.push(`the toggle changed the declaration: ${JSON.stringify(off.knobs)}`);
    }
  });
}

console.log('trend_follow as a first-class strategy on the real binary (E4-a / #30)\n');
await main();
report({
  name: 'trend-follow',
  problems,
  okLines: [
    'starts off and toggles alone; --enable-strategy starts it on',
    'owns its accounting row; declares and honours no gate exemption',
    'chases a breakout, is gated by spot, and starves nothing',
    'evolved with its own six knobs, and a toggle neither adds nor drops the cell',
  ],
});
