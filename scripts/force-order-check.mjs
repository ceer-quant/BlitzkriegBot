#!/usr/bin/env node
/**
 * 挂单强平的端到端验收（orders.force_close，issue 456）。真内核 spawn，
 * 真 UDS wire，dry 模式，零网络。
 *
 * 钉住的事实（对应 Issue #456 的验收/反向验收行）：
 *
 *   1. 反向验收「强平作用于活跃订单 → 红」：轮次仍活着的挂单被内核拒绝，
 *      错误点名 still live，且订单状态原样（仍是 LIVE）。
 *   2. 正向闭环：60s 短轮内核里等轮次真过期 → 过期扫描自动 ForceClosed
 *      （run_expiry_sweep），已终态订单再强平被拒（already terminal）。
 *   3. 审计：reconcile.jsonl（派生在 order log 父目录 data/orders/）追加
 *      kind=force_close 行，含订单/策略/轮次/原因；拒绝路径不写行。
 *   4. 已终态订单再强平 → 拒绝（already terminal）。
 *
 * 一切写入都在脚本自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * Usage: node scripts/force-order-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, readFileSync, rmSync } from 'fs';

import { join } from 'path';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const gate = createChecks();
const check = gate.check;
const cleanup = [];

// Scratch 根：本机铁律只写 /Volumes/Hard Disk；CI（Linux runner）没有该卷，
// 回退 os.tmpdir()。两边都要 mkdir -p，mkdtemp 只造末段。
import { mkdirSync } from 'fs';
import { tmpdir } from 'os';
const SCRATCH = process.env.BK_SCRATCH_DIR
  ?? (process.platform === 'darwin' ? '/Volumes/Hard Disk/bk-scratch' : tmpdir());
mkdirSync(SCRATCH, { recursive: true });

function makeCore(label, extraArgs = []) {
  const cwd = mkdtempSync(join(SCRATCH, `forceorder-${label}-${process.pid}`));
  cleanup.push(cwd);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`forceorder-${label}`),
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

const auditLines = (cwd) => {
  try {
    return readFileSync(join(cwd, 'data', 'orders', 'reconcile.jsonl'), 'utf8')
      .split('\n').filter((l) => l.trim() !== '')
      .map((l) => { try { return JSON.parse(l); } catch { return null; } })
      .filter((r) => r && r.kind === 'force_close');
  } catch {
    return [];
  }
};

try {
  const core = makeCore('main');
  await core.start();

  // 铺状态：一张盘口 + 一张 resting maker 买单（本轮 slot）。
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
    internalKey: 'force-e2e',
    strategy: 's',
    asset: 'BTC_UP',
    direction: 'up',
    roundSlot: slot,
  });
  check('the fixture resting order is live', String(place?.status).toUpperCase() === 'LIVE',
    JSON.stringify(place));
  const orderId = place?.orderId ?? place?.order_id;
  check('the reply names the order id', typeof orderId === 'string' && orderId.length > 0,
    JSON.stringify(place));

  // 1. 反向验收：轮次仍活着 → 强平拒绝，状态原样，审计不写行。
  const live = await refused(core, 'orders.force_close', { orderId });
  check('a live-round force-close is refused by name',
    !live.ok && live.message.includes('still live'), JSON.stringify(live));
  const afterRefusal = await core.request('orders.list', {});
  const stillLive = (afterRefusal.orders ?? []).find((o) => o.orderId === orderId);
  check('the refused order is untouched (still LIVE)',
    stillLive && String(stillLive.status).toUpperCase() === 'LIVE', JSON.stringify(stillLive));
  check('a refusal writes no audit line', auditLines(core.cwd).length === 0,
    JSON.stringify(auditLines(core.cwd)));

  // 2. 正向闭环：900s 轮等不起 —— 换 --round-sec 60 的短轮内核，等轮次
  //    真过期后过期扫描自动收走（run_expiry_sweep 的行为）。
  await core.stop();

  const core2 = makeCore('short', ['--round-sec', '60']);
  await core2.start();
  const feed2 = await core2.request('books.snapshot', {
    tokenId: 'tok',
    bids: [{ price: 0.39, size: 100 }],
    asks: [{ price: 0.4, size: 100 }],
  });
  check('the short-round fixture book lands', !!feed2 && !feed2.error, JSON.stringify(feed2));
  const slot2 = Math.floor(Date.now() / 1000 / 60);
  const place2 = await core2.request('orders.place', {
    tokenId: 'tok',
    conditionId: 'cond',
    side: 'buy',
    mode: 'maker',
    price: 0.39,
    size: 5,
    internalKey: 'force-e2e-short',
    strategy: 's',
    asset: 'BTC_UP',
    direction: 'up',
    roundSlot: slot2,
  });
  check('the short-round fixture order is live', String(place2?.status).toUpperCase() === 'LIVE',
    JSON.stringify(place2));
  const orderId2 = place2?.orderId ?? place2?.order_id;

  // 等轮次过期（60s 轮 + 余量），过期扫描会先收走它 —— 这正是根因修复
  // 的行为：订单在该 tick 被 ForceClosed。强平动词此时应拒绝（已终态）。
  const deadline = Date.now() + 130_000;
  let swept = null;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 500));
    const rows = await core2.request('orders.list', {});
    swept = (rows.orders ?? []).find((o) => o.orderId === orderId2);
    if (swept && String(swept.status).toUpperCase() === 'FORCE_CLOSED') break;
  }
  check('the expiry sweep force-closes the order on its own',
    swept && String(swept.status).toUpperCase() === 'FORCE_CLOSED', JSON.stringify(swept));

  // 3. 对已终态订单强平 → 拒绝（already terminal）。
  const again = await refused(core2, 'orders.force_close', { orderId: orderId2 });
  check('a terminal order refuses a second force-close',
    !again.ok && again.message.includes('terminal'), JSON.stringify(again));

  // 4. 审计：sweep 的那一行在 reconcile.jsonl（该文件还含 #189 的周期会计
  //    行，auditLines 已按 kind=force_close 过滤）。拒绝路径不写 force_close 行。
  const lines = auditLines(core2.cwd);
  const fc = lines.filter((r) => r.order_id === orderId2);
  check('the audit trail names the force_close with order and reason',
    fc.length === 1 && typeof fc[0].reason === 'string' && fc[0].round_slot === slot2,
    JSON.stringify(lines));

  // 5. 手动正向路径（轮次已过、扫描未跑的窗口）抖动难控：判定与 sweep 完全
  //    同函数（service.rs force_close_order），拒绝路径 + sweep + 审计三点
  //    已把该函数的行为钉住。

  await core2.stop();

  console.log(gate.failures === 0
    ? '\norders:force_close — live-round refusal, terminal refusal, sweep closure, audit line all hold.'
    : `\norders:force_close — ${gate.failures} assertion(s) failed.`);
  process.exit(gate.failures === 0 ? 0 : 1);
} catch (e) {
  check('harness error', false, e?.stack || e);
  console.log(`orders:force_close — harness died, ${gate.failures} assertion(s) had failed before it.`);
  process.exit(1);
} finally {
  for (const dir of cleanup) {
    try { rmSync(dir, { recursive: true, force: true }); } catch { /* scratch */ }
  }
}
