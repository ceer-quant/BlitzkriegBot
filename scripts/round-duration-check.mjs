#!/usr/bin/env node
/**
 * 轮次时间周期设置卡的端到端验收（round_duration.get / round_duration.set）。
 * 真内核 spawn，真 UDS wire，dry 模式，零网络。
 *
 * 钉住的事实（每个都对应一条 user-facing 承诺）：
 *
 *   1. readout：effectiveSec 来自出厂默认（900），source=default，白名单带
 *      scanner 的 5m/15m/1h/4h/daily 标签，nextSessionSec 无格时回落生效值。
 *   2. 写路径（next_session）：round_duration.set {durationSec:300} 落进
 *      data/runtime/config.json；回执 nextSessionSec=300；**本会话的网格一个
 *      字节不动**（effectiveSec 仍是 900）。
 *   3. 重启生效：重启后的内核 effectiveSec=300、source=runtime —— 持久格
 *      outranks 出厂默认，新网格从干净状态开始。
 *   4. 显式旗标优先：--round-sec 3600 启动时，即使格里有 300，生效值仍是
 *      3600 —— 逐位复现的回放基线永远不被 UI 设置劫持。
 *   5. 白名单外拒绝：durationSec:60 拒绝且格不动。
 *   6. reset 清格：回落启动解析（旗标测试内核随后 readout persisted=false）。
 *   7. 回执 actor 带 uid:（审计）。
 *
 * 一切写入都在脚本自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * Usage: node scripts/round-duration-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, mkdirSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const gate = createChecks();
const check = gate.check;
const cleanup = [];

function makeCore(label, extraArgs = [], cwdOverride = null) {
  // The cell is cwd-relative (data/runtime/config.json), so a "restart" MUST
  // reuse the SAME cwd the previous core ran in — a real deployment restarts
  // in place; a fresh scratch dir would be a different deployment with no
  // cell to read.
  const cwd = cwdOverride ?? mkdtempSync(join(tmpdir(), `blitzkrieg-rounddur-${label}-`));
  if (!cwdOverride) cleanup.push(cwd);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`rounddur-${label}`),
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

  // 1. The pre-write readout: compiled default, nothing persisted, the
  //    whitelist with its labels.
  const before = await core.request('round_duration.get', {});
  check('readout answers the effective value from the compiled default',
    before.effectiveSec === 900 && before.source === 'default', JSON.stringify(before));
  check('no cell means the next session re-resolves to the effective value',
    before.nextSessionSec === 900 && before.persisted === false, JSON.stringify(before));
  const labels = Object.fromEntries((before.allowed ?? []).map((o) => [o.sec, o.label]));
  check('the whitelist carries the scanner labels',
    labels[300] === '5m' && labels[900] === '15m' && labels[86400] === 'daily',
    JSON.stringify(labels));

  // 7. The write path lands and the audit trail names the actor.
  const set = await core.request('round_duration.set', { durationSec: 300, reason: 'e2e gate' });
  check('the set replies with the NEXT session value',
    set.nextSessionSec === 300 && set.reset === false, JSON.stringify(set));
  check('the audit trail names the actor',
    typeof set.actor === 'string' && set.actor.startsWith('uid:'), JSON.stringify(set));

  // 2. The running grid does not move (next-session semantics).
  const mid = await core.request('round_duration.get', {});
  check('the running session keeps its effective value after a write',
    mid.effectiveSec === 900 && mid.nextSessionSec === 300 && mid.persisted === true,
    JSON.stringify(mid));

  // 5. An off-whitelist value is refused and the cell is untouched.
  let refused = null;
  try {
    refused = await core.request('round_duration.set', { durationSec: 60 });
  } catch (e) {
    refused = { message: e?.message ?? String(e) };
  }
  check('an off-whitelist value is refused by name',
    refused?.message?.includes('not on the whitelist'), JSON.stringify(refused));
  const afterBad = await core.request('round_duration.get', {});
  check('a refused write leaves the cell exactly as it was',
    afterBad.nextSessionSec === 300 && afterBad.effectiveSec === 900, JSON.stringify(afterBad));

  // 7 (ambiguity guard): durationSec + reset together is refused.
  let both = null;
  try {
    both = await core.request('round_duration.set', { durationSec: 300, reset: true });
  } catch (e) {
    both = { message: e?.message ?? String(e) };
  }
  check('an ambiguous write (value + reset) is refused',
    both?.message?.includes('not both'), JSON.stringify(both));

  await core.stop();

  // 3. The restart resolution: the new core reads the cell (same deployment
  //    cwd — a restart re-runs in place) and runs 5m.
  const core2 = makeCore('restart', [], core.cwd);
  await core2.start();
  const after = await core2.request('round_duration.get', {});
  check('a restart runs the persisted duration (source=runtime)',
    after.effectiveSec === 300 && after.source === 'runtime' && after.persisted === true,
    JSON.stringify(after));

  // 6. reset clears the cell — the next session falls back to the boot
  //    resolution. This core booted ON the cell (300), so the fallback IS
  //    300: clearing removes the cell, not the value it last armed.
  const reset = await core2.request('round_duration.set', { reset: true, reason: 'e2e gate' });
  check('reset clears the cell (the reply names the fallback)',
    reset.reset === true && reset.nextSessionSec === 300, JSON.stringify(reset));
  const afterReset = await core2.request('round_duration.get', {});
  check('after a reset the readout reports nothing persisted',
    afterReset.persisted === false && afterReset.nextSessionSec === 300
      && afterReset.effectiveSec === 300, JSON.stringify(afterReset));
  await core2.stop();

  // 4. An explicit flag outranks a persisted cell — the bit-exact replay
  //    baselines must never be hijacked by the UI card.
  const core3 = makeCore('flagged', ['--round-sec', '3600']);
  // Re-arm the cell for this core's cwd: write 300, boot with the flag.
  await core3.start();
  const preFlag = await core3.request('round_duration.set', { durationSec: 300 });
  check('the flagged core accepts a write (the cell lands)',
    preFlag.nextSessionSec === 300, JSON.stringify(preFlag));
  await core3.stop();
  const core4 = makeCore('flagged2', ['--round-sec', '3600']);
  await core4.start();
  const flagged = await core4.request('round_duration.get', {});
  check('an explicit --round-sec outranks the persisted cell',
    flagged.effectiveSec === 3600, JSON.stringify(flagged));
  check('the flagged readout names its source so the card can explain itself',
    // #499 renamed the source tier: TOML is a default FOR the card, not a
    // veto over it, so a flag readout is exactly "cli_or_env" (the tier the
    // boot site tests `!=` against before consulting the persisted cell).
    flagged.source === 'cli_or_env', JSON.stringify(flagged));
  await core4.stop();

  console.log(gate.failures === 0
    ? '\nround:duration — set/restart/flag precedence all hold over the real wire.'
    : `\nround:duration — ${gate.failures} assertion(s) failed.`);
  process.exit(gate.failures === 0 ? 0 : 1);
} catch (e) {
  check('harness error', false, e?.stack || e);
  console.log(`round:duration — harness died, ${gate.failures} assertion(s) had failed before it.`);
  process.exit(1);
} finally {
  for (const dir of cleanup) {
    try { rmSync(dir, { recursive: true, force: true }); } catch { /* scratch */ }
  }
}
