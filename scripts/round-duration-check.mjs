#!/usr/bin/env node
/**
 * 轮次时间周期设置卡的端到端验收（round_duration.get / round_duration.set）。
 * 真内核 spawn，真 UDS wire，dry 模式，零网络。
 *
 * 钉住的事实（#1140 per-account 契约，每条对应一个 user-facing 承诺）：
 *
 *   1. readout：effectiveSec 来自出厂默认（900），source=default，perAccount
 *      为空，可切换档位（插件声明并集）带 scanner 的 5m/15m/1h/4h/daily 标签。
 *   2. 写路径：round_duration.set {accountId:'default', durationSec:300} 把
 *      ACCOUNT_DEFAULT_ROUND_DURATION_SEC 落进 SQLite `strategy_assignments`
 *      行（issue #1189；回执点名账户与 nextSessionSec=300；审计回执 actor
 *      带 uid:）。
 *   3. 立即生效：本会话 perAccount.default=300（内存 map 即时更新）；全局
 *      effectiveSec 一个字节不动（per-account 覆盖与默认网格分离）。
 *   4. 重启生效：重启后的内核从 SQLite 读回 perAccount.default=300 —— 持久
 *      覆盖 outranks 出厂默认。
 *   5. 声明并集外拒绝：durationSec:60 拒绝且 perAccount 不动。
 *   6. 不存在的账户按名拒绝：`no such account` 报错，绝不崩核。
 *   7. reset 清该账户的覆盖：回落默认网格。
 *   8. 显式旗标只管默认网格：--round-sec 3600 启动时 effectiveSec=3600 而
 *      账户覆盖 300 原样保留 —— 逐位复现的回放基线永远不被 UI 设置劫持。
 *
 * 一切写入都在脚本自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * Usage: node scripts/round-duration-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { DatabaseSync } from 'node:sqlite';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const gate = createChecks();
const check = gate.check;
const cleanup = [];

function makeCore(label, extraArgs = [], cwdOverride = null) {
  // The override lives in the cwd's SQLite (`strategy_assignments`,
  // issue #1189), so a "restart" MUST reuse the SAME cwd the previous core
  // ran in — a real deployment restarts in place; a fresh scratch dir would
  // be a different deployment with nothing to read back.
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
    // 门禁要断言“引擎侧实际变化”，必须让核安装扫描引擎（--engine），
    // 否则 round_duration.set 的 engine 同步无对象可写。
    extraArgs: [...(extraArgs ?? []), '--engine'],
  });
}

// ── the SQLite read face (issue #1189): the write's landing row ─────────────
// The `round_duration.set` override now lands in `strategy_assignments`
// (account, 'default', round_duration_sec), NOT in the `.env` text — the
// old gates grepped the file, these read the row. A scratch cwd has no
// `BLITZKRIEG_DB_PATH`, so the launcher's default (`<cwd>/data/blitzkrieg.sqlite`)
// is where the core wrote. Reading the DB file is safe while the core runs
// (WAL readers never block the writer).
function roundDurationRow(cwd) {
  const dbPath = join(cwd, 'data', 'blitzkrieg.sqlite');
  const db = new DatabaseSync(dbPath, { readOnly: true });
  try {
    return db.prepare(
      'SELECT strategy, round_duration_sec FROM strategy_assignments \
       WHERE account_id = ?1 AND strategy = ?2',
    ).get('default', 'default');
  } finally {
    db.close();
  }
}

try {
  const core = makeCore('main');
  await core.start();

  // 1. The pre-write readout: compiled default, no per-account overrides,
  //    the declared union (the market plugins' supported_round_secs) with its
  //    labels.
  const before = await core.request('round_duration.get', {});
  check('readout answers the effective value from the compiled default',
    before.effectiveSec === 900 && before.source === 'default', JSON.stringify(before));
  check('no override means the readout reports an empty per-account map',
    before.nextSessionSec === 900 && JSON.stringify(before.perAccount ?? {}) === '{}',
    JSON.stringify(before));
  const labels = Object.fromEntries((before.allowed ?? []).map((o) => [o.sec, o.label]));
  check('the declared union carries the scanner labels',
    labels[300] === '5m' && labels[900] === '15m' && labels[86400] === 'daily',
    JSON.stringify(labels));

  // 2. The write path: per-account, names the account and the actor.
  const set = await core.request('round_duration.set',
    { accountId: 'default', durationSec: 300, reason: 'e2e gate' });
  check('the set replies naming the account and its new grid',
    set.accountId === 'default' && set.nextSessionSec === 300 && set.reset === false,
    JSON.stringify(set));
  check('the audit trail names the actor',
    typeof set.actor === 'string' && set.actor.startsWith('uid:'), JSON.stringify(set));
  // issue #1189: the write lands as a DB row, not .env text — read the row
  // back and pin it (reverse acceptance: a neutered write leaves no row).
  const row = roundDurationRow(core.cwd);
  check('the write landed as a strategy_assignments row (not .env text)',
    Number(row?.round_duration_sec) === 300, JSON.stringify(row ?? null));

  // 3. Live semantics: the per-account map reflects the write immediately;
  //    the global default grid is untouched (the override is a separate tier),
  //    and crucially the ENGINE scanner clock is synced so the live countdown
  //    matches the account grid — no split-brain (#1151).
  const mid = await core.request('round_duration.get', {});
  check('the write lands in the running session immediately and the engine clock synced',
    mid.perAccount?.default === 300 && mid.effectiveSec === 900 && mid.engineActiveSec === 300,
    JSON.stringify(mid));

  // 5. A value outside the plugins' declared union is refused and the
  //    override is untouched.
  let refused = null;
  try {
    refused = await core.request('round_duration.set', { accountId: 'default', durationSec: 60 });
  } catch (e) {
    refused = { message: e?.message ?? String(e) };
  }
  check('a value no plugin declared is refused by name',
    refused?.message?.includes('not declared by any market plugin'), JSON.stringify(refused));
  const afterBad = await core.request('round_duration.get', {});
  check('a refused write leaves the override exactly as it was',
    afterBad.perAccount?.default === 300 && afterBad.effectiveSec === 900, JSON.stringify(afterBad));

  // 6. An account the book does not know is refused by name — never a crash.
  let ghost = null;
  try {
    ghost = await core.request('round_duration.set',
      { accountId: 'no-such-account', durationSec: 300 });
  } catch (e) {
    ghost = { message: e?.message ?? String(e) };
  }
  check('an unknown account is refused by name',
    ghost?.message?.includes('no such account'), JSON.stringify(ghost));

  // Ambiguity guard: durationSec + reset together is refused.
  let both = null;
  try {
    both = await core.request('round_duration.set',
      { accountId: 'default', durationSec: 300, reset: true });
  } catch (e) {
    both = { message: e?.message ?? String(e) };
  }
  check('an ambiguous write (value + reset) is refused',
    both?.message?.includes('not both'), JSON.stringify(both));

  // 秒切秒换：每次 round_duration.set 都立即撤单→市价强平→切网格，
  // fail-closed；成交按真实账簿价入账，台账写 operator_grid_switch。
  await core.request('books.snapshot', {
    tokenId: 'tok',
    bids: [{ price: 0.39, size: 100 }],
    asks: [{ price: 0.40, size: 100 }],
  });
  await core.request('orders.place', {
    tokenId: 'tok',
    conditionId: 'cond',
    side: 'buy',
    mode: 'taker',
    price: 0.40,
    size: 5,
    internalKey: 'switch-probe',
    strategy: 's',
    asset: 'BTC',
    direction: 'up',
    roundSlot: Math.floor(Date.now() / 1000 / 900),
  });
  const beforePos = await core.request('positions.list', {});
  check('切换前有一笔在途仓位',
    beforePos.positions?.length === 1, JSON.stringify(beforePos));

  const switched = await core.request('round_duration.set',
    { accountId: 'default', durationSec: 300, reason: 'gate switch' });
  check('set 回执新网格（秒切秒换）',
    switched.nextSessionSec === 300 && switched.reset === false, JSON.stringify(switched));

  const afterPos = await core.request('positions.list', {});
  check('秒切换平掉在途仓位',
    afterPos.positions?.length === 0, JSON.stringify(afterPos));

  const mid2 = await core.request('round_duration.get', {});
  check('引擎扫描器立即跟入新网格（无 split-brain）',
    mid2.engineActiveSec === 300 && mid2.perAccount?.default === 300, JSON.stringify(mid2));

  const trades = await core.request('trades.history', { limit: 10 });
  const earlyExit = trades.trades?.some((t) => t.exitReason === 'operator_grid_switch'
    && Number(t.exitPrice) === 0.39 && t.accountId === 'default');
  check('台账写 operator_grid_switch 且出场价是真实 bid 0.39',
    earlyExit === true, JSON.stringify(trades));

  // flagLocked：显式 --round-sec 在管时，立即切换是破坏性的，所有
  // round_duration.set 写入都被如实拒绝（不再区分 immediate 字段）。
  const core5 = makeCore('flaglocked', ['--round-sec', '3600']);
  await core5.start();
  let flaglocked = null;
  try {
    await core5.request('round_duration.set',
      { accountId: 'default', durationSec: 300 });
  } catch (e) {
    flaglocked = { message: e?.message ?? String(e) };
  }
  check('flagLocked 拒绝所有即时切换写入',
    flaglocked && (flaglocked.message.includes('锁定') || flaglocked.message.includes('拒绝')),
    JSON.stringify(flaglocked));
  await core5.stop();

  // 秒切秒换：每次写入都立即平掉在途仓位（与「非 immediate 不平仓」的
  // 旧 next_session 语义不同，切换本身即原子动作，无单独按钮）。
  await core.request('books.snapshot', {
    tokenId: 'tok2', bids: [{ price: 0.39, size: 100 }], asks: [{ price: 0.40, size: 100 }],
  });
  await core.request('orders.place', {
    tokenId: 'tok2', conditionId: 'cond2', side: 'buy', mode: 'taker',
    price: 0.40, size: 5, internalKey: 'switch-probe-2', strategy: 's', asset: 'ETH',
    direction: 'up', roundSlot: Math.floor(Date.now() / 1000 / 900),
  });
  const stillPos = await core.request('positions.list', {});
  check('写入前有一笔在途仓位',
    stillPos.positions?.length === 1, JSON.stringify(stillPos));
  await core.request('round_duration.set', { accountId: 'default', durationSec: 300 });
  const stillPos2 = await core.request('positions.list', {});
  check('秒切秒换：写入后在途仓位被平',
    stillPos2.positions?.length === 0, JSON.stringify(stillPos2));

  // 4. The restart resolution: the new core reads the .env override back
  //    (same deployment cwd — a restart re-runs in place) and the account
  //    runs 5m while the default grid stays 900.
  const core2 = makeCore('restart', [], core.cwd);
  await core2.start();
  const after = await core2.request('round_duration.get', {});
  check('a restart runs the persisted per-account override',
    after.perAccount?.default === 300 && after.effectiveSec === 900, JSON.stringify(after));

  // 7. reset clears the account's override — that account falls back to the
  //    default grid; the reply names the fallback.
  const reset = await core2.request('round_duration.set',
    { accountId: 'default', reset: true, reason: 'e2e gate' });
  check('reset clears the override (the reply names the fallback)',
    reset.reset === true && reset.accountId === 'default' && reset.nextSessionSec === 900,
    JSON.stringify(reset));
  const clearedRow = roundDurationRow(core2.cwd);
  check('the reset cleared the DB row (not just the reply)',
    clearedRow === undefined || clearedRow?.round_duration_sec === null,
    JSON.stringify(clearedRow ?? null));
  const afterReset = await core2.request('round_duration.get', {});
  check('after a reset the account falls back to the default grid',
    !('default' in (afterReset.perAccount ?? {})) && afterReset.effectiveSec === 900,
    JSON.stringify(afterReset));
  await core2.stop();

  // 8. An explicit --round-sec governs the DEFAULT grid only — the persisted
  //    per-account override survives the flag untouched, and the readout
  //    names its source so the card can explain itself.
  //
  //    新语义：flagLocked（--round-sec）时 round_duration.set 被拒绝
  //    （立即切换是破坏性的）。覆盖须在非 flagLocked 核上设置，再
  //    用 flagged 核（同一 cwd）读取验证覆盖保留 + 默认网格由 flag 控制。
  const preFlag = await core.request('round_duration.set',
    { accountId: 'default', durationSec: 300, reason: 'e2e gate' });
  check('非 flagLocked 核接受 per-account 写入',
    preFlag.accountId === 'default' && preFlag.nextSessionSec === 300, JSON.stringify(preFlag));
  await core.stop();

  const core3 = makeCore('flagged', ['--round-sec', '3600'], core.cwd);
  await core3.start();
  const flagged = await core3.request('round_duration.get', {});
  check('an explicit --round-sec runs the default grid while the override survives',
    flagged.effectiveSec === 3600 && flagged.perAccount?.default === 300, JSON.stringify(flagged));
  check('the flagged readout names its source so the card can explain itself',
    // #499 renamed the source tier: TOML is a default FOR the card, not a
    // veto over it, so a flag readout is exactly "cli_or_env".
    flagged.source === 'cli_or_env', JSON.stringify(flagged));
  await core3.stop();

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
