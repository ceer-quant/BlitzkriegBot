#!/usr/bin/env node
/**
 * 插件页行情插件切换卡的端到端验收（market.plugin.get / market.plugin.set，
 * issue #485）—— 以及扩展启停动词（extension.enable / extension.disable）。
 * 真内核 spawn，真 UDS wire，dry 模式，零网络。
 *
 * 钉住的事实（每个都对应一条 user-facing 承诺）：
 *
 *   1. readout：activePlugin 来自首个注册者（无旗标），registered 列出全部
 *      编译进内核的插件（polymarket/kalshi/predictfun/polysimulator），
 *      nextSessionPlugin 回落 active，persisted=false。
 *   2. 写路径（next_session）：market.plugin.set {name:"kalshi"} 落进
 *      data/runtime/config.json；回执 nextSessionPlugin=kalshi；**本会话的
 *      active 一个字节不动**（readout activePlugin 仍是 polymarket）。
 *   3. 重启生效：重启后的内核 activePlugin=kalshi（持久格 outrank 首个
 *      注册者），读数带 persisted=true。
 *   4. 显式旗标优先：--market-plugin predictfun 启动时，即使格里有 kalshi，
 *      activePlugin 仍是 predictfun —— 逐位复现的回放基线永远不被 UI 劫持。
 *   5. 未注册名拒绝：market.plugin.set {name:"nope"} 拒绝且格不动（点名
 *      switchable set）。
 *   6. reset 清格：下次会话回落启动解析。
 *   7. 歧义拒绝：name + reset 一起给 = 拒绝。
 *   8. 扩展启停：extension.enable/disable 真跑 on_load/on_unload —— state
 *      installed → enabled → disabled 逐字断言。
 *
 * 一切写入都在脚本自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * Usage: node scripts/market-plugin-cell-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, mkdirSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const { check, failures } = createChecks();
const cleanup = [];

function makeCore(label, extraArgs = [], cwdOverride = null) {
  // The cell is cwd-relative (data/runtime/config.json), so a "restart" MUST
  // reuse the SAME cwd the previous core ran in — a real deployment restarts
  // in place; a fresh scratch dir would be a different deployment with no
  // cell to read.
  const cwd = cwdOverride ?? mkdtempSync(join(tmpdir(), `blitzkrieg-mkplugin-${label}-`));
  if (!cwdOverride) cleanup.push(cwd);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`mkplugin-${label}`),
    mode: 'dry',
    seedBalance: 100,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd,
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs,
  });
}

try {
  const core = makeCore('main');
  await core.start();

  // 1. The pre-write readout: first-registered active, the full registry,
  //    nothing persisted.
  const before = await core.request('market.plugin.get', {});
  const names = (before.registered ?? []).map((r) => r.name);
  check('the readout answers the active plugin from the first-registered resolution',
    before.activePlugin === 'polymarket' && before.flagPinned === false,
    JSON.stringify(before));
  check('the registry lists every compiled-in plugin',
    ['polymarket', 'kalshi', 'predictfun', 'polysimulator'].every((n) => names.includes(n)),
    JSON.stringify(names));
  check('exactly one row is flagged active', (before.registered ?? []).filter((r) => r.active).length === 1,
    JSON.stringify(before.registered));
  check('no cell means the next session re-resolves to the active plugin',
    before.nextSessionPlugin === before.activePlugin && before.persisted === false,
    JSON.stringify(before));

  // 5. An unregistered name is refused BEFORE the disk is touched
  //    (plan-first: the registry is compiled in).
  let refused = null;
  try {
    refused = await core.request('market.plugin.set', { name: 'nope' });
  } catch (e) {
    refused = { message: e?.message ?? String(e) };
  }
  check('an unregistered name is refused by name',
    refused?.message?.includes('not a registered plugin'), JSON.stringify(refused));
  const afterBad = await core.request('market.plugin.get', {});
  check('a refused write leaves the cell exactly as it was',
    afterBad.persisted === false && afterBad.nextSessionPlugin === before.activePlugin,
    JSON.stringify(afterBad));

  // 7. Ambiguity guard: name + reset together is refused.
  let both = null;
  try {
    both = await core.request('market.plugin.set', { name: 'kalshi', reset: true });
  } catch (e) {
    both = { message: e?.message ?? String(e) };
  }
  check('an ambiguous write (name + reset) is refused',
    both?.message?.includes('not both'), JSON.stringify(both));

  // 2. The write path lands and the running session does not move.
  const set = await core.request('market.plugin.set', { name: 'kalshi', reason: 'e2e gate' });
  check('the set replies with the NEXT session plugin',
    set.nextSessionPlugin === 'kalshi' && set.reset === false, JSON.stringify(set));
  check('the audit trail names the actor',
    typeof set.actor === 'string' && set.actor.startsWith('uid:'), JSON.stringify(set));
  const mid = await core.request('market.plugin.get', {});
  check('the running session keeps its active plugin after a write',
    mid.activePlugin === before.activePlugin && mid.nextSessionPlugin === 'kalshi'
      && mid.persisted === true,
    JSON.stringify(mid));

  await core.stop();

  // 3. The restart resolution: the new core reads the cell (same deployment
  //    cwd — a restart re-runs in place) and activates kalshi.
  const core2 = makeCore('restart', [], core.cwd);
  await core2.start();
  const after = await core2.request('market.plugin.get', {});
  check('a restart activates the persisted plugin',
    after.activePlugin === 'kalshi' && after.persisted === true
      && after.nextSessionPlugin === 'kalshi',
    JSON.stringify(after));

  // 6. reset clears the cell — the next session falls back to the boot
  //    resolution. This core booted ON the cell (kalshi), so the fallback IS
  //    kalshi: clearing removes the cell, not the value it last armed.
  const reset = await core2.request('market.plugin.set', { reset: true, reason: 'e2e gate' });
  check('reset clears the cell (the reply names the fallback)',
    reset.reset === true && reset.nextSessionPlugin === 'kalshi', JSON.stringify(reset));
  const afterReset = await core2.request('market.plugin.get', {});
  check('after a reset the readout reports nothing persisted',
    afterReset.persisted === false && afterReset.nextSessionPlugin === 'kalshi',
    JSON.stringify(afterReset));
  await core2.stop();

  // 4. An explicit flag outranks a persisted cell — the bit-exact replay
  //    baselines must never be hijacked by the plugin card.
  const core3 = makeCore('flagged', ['--market-plugin', 'predictfun']);
  await core3.start();
  const preFlag = await core3.request('market.plugin.set', { name: 'kalshi' });
  check('the flagged core accepts a write (the cell lands)',
    preFlag.nextSessionPlugin === 'kalshi', JSON.stringify(preFlag));
  await core3.stop();
  const core4 = makeCore('flagged2', ['--market-plugin', 'predictfun']);
  await core4.start();
  const flagged = await core4.request('market.plugin.get', {});
  check('an explicit --market-plugin outranks the persisted cell',
    flagged.activePlugin === 'predictfun' && flagged.flagPinned === true,
    JSON.stringify(flagged));
  await core4.stop();

  // 8. Extension enable/disable: the SAME verbs the panel buttons drive,
  //    run for real (on_load / on_unload).
  const core5 = makeCore('extension');
  await core5.start();
  const list0 = await core5.request('extension.list', {});
  const spot = (list0.extensions ?? []).find((e) => e.name === 'binance_spot');
  check('the builtin extension ships installed (not enabled)',
    spot && spot.state === 'installed', JSON.stringify(list0));

  const en = await core5.request('extension.enable', { name: 'binance_spot' });
  check('extension.enable runs on_load and reports the new state',
    en.enabled === true, JSON.stringify(en));
  const list1 = await core5.request('extension.list', {});
  check('the registry reflects enabled',
    (list1.extensions ?? []).find((e) => e.name === 'binance_spot')?.state === 'enabled',
    JSON.stringify(list1));

  const dis = await core5.request('extension.disable', { name: 'binance_spot' });
  check('extension.disable runs on_unload and reports the new state',
    dis.enabled === false, JSON.stringify(dis));
  const list2 = await core5.request('extension.list', {});
  check('the registry reflects disabled',
    (list2.extensions ?? []).find((e) => e.name === 'binance_spot')?.state === 'disabled',
    JSON.stringify(list2));

  let missing = null;
  try {
    missing = await core5.request('extension.enable', { name: 'not_installed' });
  } catch (e) {
    missing = { message: e?.message ?? String(e) };
  }
  check('enabling a non-installed extension is refused by name',
    missing?.message?.includes('not installed'), JSON.stringify(missing));
  await core5.stop();

  console.log(failures === 0
    ? '\nmarket:plugin — set/restart/flag precedence + extension lifecycle all hold over the real wire.'
    : `\nmarket:plugin — ${failures} assertion(s) failed.`);
  process.exit(failures === 0 ? 0 : 1);
} catch (e) {
  check('harness error', false, e?.stack || e);
  console.log(`market:plugin — harness died, ${failures} assertion(s) had failed before it.`);
  process.exit(1);
} finally {
  for (const dir of cleanup) {
    try { rmSync(dir, { recursive: true, force: true }); } catch { /* scratch */ }
  }
}
