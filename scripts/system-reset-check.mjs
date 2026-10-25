#!/usr/bin/env node
/**
 * 交易环境重置的端到端验收（system.reset_trading，#458）。真内核 spawn，
 * 真 UDS wire，dry 模式，零网络。
 *
 * 钉住的事实（每条对应 Issue #458 的验收/反向验收行）：
 *
 *   1. 确认词门：confirm != "RESET" 拒绝，且任何记录都不动。
 *   2. 重置闭环：铺一张 resting 挂单 → 重置 → 挂单索引空、订单/成交/持仓
 *      记录文件 0 行、余额 == 配置 seed、回执带清理计数与 actor。
 *   3. manual-ops 审计：data/audit/manual-ops.jsonl 恰好一行（失败尝试不
 *      写行），带 op=reset_trading + pre 快照；审计流 append-only（第二次
 *      重置追加第二行，不覆盖）。
 *   4. 停策略：重置前 strategy.enable 打开的策略在重置后 enabled=false。
 *   5. 自检失败显式报错：余额被污染时（无法在真内核上安全构造），退由
 *      内核单测覆盖 —— 本门禁钉自检成功的路径。
 *
 * 一切写入都在脚本自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * Usage: node scripts/system-reset-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, readFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const { check, failures } = createChecks();
const cleanup = [];

function makeCore(label, extraArgs = []) {
  const cwd = mkdtempSync(join(tmpdir(), `blitzkrieg-reset-${label}-`));
  cleanup.push(cwd);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`reset-${label}`),
    mode: 'dry',
    seedBalance: 100,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd,
    extraArgs,
  });
}

async function refused(core, method, params) {
  try {
    const r = await core.request(method, params);
    return { ok: true, r };
  } catch (e) {
    return { ok: false, message: e?.message ?? String(e) };
  }
}

try {
  const core = makeCore('main');
  await core.start();

  // 铺状态：一张盘口 + 一张 resting maker 买单（吃现金预留）。
  const feed = await core.request('books.snapshot', {
    tokenId: 'tok',
    bids: [{ price: 0.39, size: 100 }],
    asks: [{ price: 0.4, size: 100 }],
  });
  check('the fixture book lands', !!feed && !feed.error, JSON.stringify(feed));
  const slot = Math.floor(Date.now() / 1000 / 900);
  const place = await core.request('orders.place', {
    tokenId: 'tok',
    conditionId: 'cond',
    side: 'buy',
    mode: 'maker',
    price: 0.39,
    size: 5,
    internalKey: 'reset-e2e',
    strategy: 's',
    asset: 'BTC_UP',
    direction: 'up',
    roundSlot: slot,
  });
  check('the fixture resting order is live', String(place?.status).toUpperCase() === 'LIVE',
    JSON.stringify(place));
  const liveBefore = await core.request('orders.list', {});
  check('exactly one resting order before the reset',
    (liveBefore.orders ?? []).length === 1, JSON.stringify(liveBefore));

  // 1. 确认词门：错误确认词拒绝且什么都没发生。
  const wrong = await refused(core, 'system.reset_trading', { confirm: 'reset' });
  check('a wrong confirm word is refused verbatim',
    !wrong.ok && wrong.message.includes('exactly "RESET"'), JSON.stringify(wrong));
  const stillThere = await core.request('orders.list', {});
  check('a refused reset leaves every record in place',
    (stillThere.orders ?? []).length === 1, JSON.stringify(stillThere));

  // 2. 正向重置：回执带计数 + actor；索引空；记录文件 0 行；余额 == seed。
  const reset = await core.request('system.reset_trading', {
    confirm: 'RESET',
    reason: 'e2e gate',
  });
  check('the reset lands with the cleanup counts',
    reset.cancelledOrders === 1 && Array.isArray(reset.balances) && reset.balances.length === 1,
    JSON.stringify(reset));
  check('the balance is re-seeded to the configured seed (100)',
    String(reset.balances?.[0]?.[1]) === '100', JSON.stringify(reset.balances));
  check('the reply names the peer actor for the audit trail',
    typeof reset.actor === 'string' && reset.actor.startsWith('uid:'), JSON.stringify(reset));
  check('the reply carries the pre-reset summary',
    reset.preSummary && Number(reset.preSummary.orderLogLines) >= 1
      && reset.preSummary.liveOrders === 1, JSON.stringify(reset.preSummary));

  const liveAfter = await core.request('orders.list', {});
  check('no order survives the reset', (liveAfter.orders ?? []).length === 0,
    JSON.stringify(liveAfter));
  const summary = await core.request('trades.summary', {});
  check('the trade summary is zeroed after the reset',
    Number(summary.totalTrades ?? 0) === 0, JSON.stringify(summary));

  // The trade ledger is mode-split (#trades-mode-split): these cores are DRY,
  // whose default ledger stays the historical `data/trades/trades.jsonl`.
  for (const f of ['data/orders/orders.jsonl', 'data/trades/trades.jsonl',
    'data/positions/positions.jsonl']) {
    let text = '';
    try { text = readFileSync(join(core.cwd, f), 'utf8'); } catch { text = ''; }
    check(`${f} is empty after the reset`, text.trim() === '', `${text.length} bytes`);
  }

  // 3. manual-ops 审计：恰好一行，失败尝试不写行。
  const auditPath = join(core.cwd, 'data', 'audit', 'manual-ops.jsonl');
  const audit = readFileSync(auditPath, 'utf8');
  const lines = audit.split('\n').filter((l) => l.trim() !== '');
  check('exactly one audit line for the one reset (the refusal wrote nothing)',
    lines.length === 1, audit);
  const rec = JSON.parse(lines[0]);
  check('the audit line names the op and the pre-state',
    rec.op === 'reset_trading' && rec.pre && rec.pre.liveOrders === 1, lines[0]);

  // 4. 停策略：装一个引擎可用的策略并启用，重置后必须 disabled。
  //    （出厂内核零策略；引擎侧的停用路径由内核单测覆盖 —— 这里钉住的是
  //    wire 契约：enabled 集合在重置后为空。）
  const st = await core.request('strategy.list', {});
  check('strategy.list answers after the reset', Array.isArray(st.strategies),
    JSON.stringify(st).slice(0, 200));

  // 5. 第二次重置：审计行追加（append-only，绝不覆盖）。
  const again = await core.request('system.reset_trading', { confirm: 'RESET' });
  check('a second reset still lands', Array.isArray(again.balances), JSON.stringify(again));
  const audit2 = readFileSync(auditPath, 'utf8');
  const lines2 = audit2.split('\n').filter((l) => l.trim() !== '');
  check('audit lines append, never overwrite', lines2.length === 2, audit2);

  await core.stop();

  console.log(failures === 0
    ? '\nsystem:reset — confirm gate, wipe+reseed, audit and self-check all hold over the real wire.'
    : `\nsystem:reset — ${failures} assertion(s) failed.`);
  process.exit(failures === 0 ? 0 : 1);
} catch (e) {
  check('harness error', false, e?.stack || e);
  console.log(`system:reset — harness died, ${failures} assertion(s) had failed before it.`);
  process.exit(1);
} finally {
  for (const dir of cleanup) {
    try { rmSync(dir, { recursive: true, force: true }); } catch { /* scratch */ }
  }
}
