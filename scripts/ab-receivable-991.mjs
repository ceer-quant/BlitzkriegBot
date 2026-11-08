#!/usr/bin/env node
/**
 * ab-receivable-991 — 不可核验到期回退的记账策略 A/B（issue #991/#1002）。
 *
 * 问题（#991 判决）：到期回退平仓的这笔"估算钱"该不该立刻进可花现金？
 *   cash 臂（现状，#684 字节等价）——回退名义额直接 credit 现金；
 *   receivable 臂（本 issue）——名义额记为 settlement receivable 池里的
 *     ExpiryFallbackClaim，现金不动；等 venue truth（resolution 行或
 *     reconcile 的场地成交） redemption 进现金，差额按 #998 双行模式补
 *     correction 行并修正 trade record 的净 PnL。
 *
 * 两个臂的唯一处理差异是启动环境变量 BK_UNVERIFIED_EXPIRY_POLICY（#991：
 * 读于 boot，非热更新）。argv 逐字节相同、语料相同、窗口相同、资金相同
 * —— 差异只能来自记账策略。没有也不许有"阈值快速确认"旋钮（边界 #3）。
 *
 * 预声明对比维度（数字存在之前先钉死）：
 *   1. day-budget 消耗/每窗：dailyLoss.openingEquityUsd / realizedPnlUsd /
 *      limitUsd / tripped（估算钱不许进预算基数 → receivable 臂的
 *      opening equity 系统性低出回退名义额，损失预算按更小的基数算）。
 *   2. breaker 触发次数：risk_alerts 里 consecutive-loss / trading frozen /
 *      daily-loss / expiry-fallback / receivable 相关计数。
 *   3. 下单→成交吞吐：orders / fillStats / fills（receivable 臂现金更紧，
 *      equity 相对仓位帽下的下单能力是实测对象）。
 *   4. 每臂净 PnL：trades.netPnlUsd 与 tradeLines 分解（含
 *      UnverifiableExpiry 平仓计数）。
 *
 * 语料坐标（预声明，不挑窗口）：data/onchain 的 15m-act 单文件。跑批前
 * 门禁：manifest sha256 对文件实哈希、scripts/corpus/fix_resolutions.py
 * --check（#890）。窗口 = 轮数等分的连续切片，每窗校验 resolution 行
 * 完整性（与 ab-exit-964 同一套纪律）。--limit-windows K 只跑前 K 窗
 * （冒烟用；manifest 里 coverage.limited 如实标注）。
 *
 * 确定性（#351 / #964 同款）：每臂 × 每窗默认跑两遍，报告 sha256 必须逐
 * 字节一致；--single-run 跳过第二遍（冒烟可用）。不一致 = 确定性 bug，
 * 实验判无效。
 *
 * 附加门禁（本实验特有）：报告 feed.settlement.unverifiedExpiryPolicy 必须
 * 等于该臂声明的策略 —— 防止 env 没被内核读到、两臂实际同策略的假对照。
 *
 * 用法：
 *   node scripts/ab-receivable-991.mjs                     # 全实验（两臂 × 全窗口 × 双跑）
 *   node scripts/ab-receivable-991.mjs --dry-run           # 只出计划，不执行
 *   node scripts/ab-receivable-991.mjs --only receivable   # 只跑一臂（cash|receivable）
 *   node scripts/ab-receivable-991.mjs --limit-windows 1 --single-run   # 冒烟
 *   node scripts/ab-receivable-991.mjs --report-only <dir> # 从既有产物重算汇总
 *   node scripts/ab-receivable-991.mjs --self-test         # 框架自测（离线，不碰内核/语料）
 *
 * 退出码：0 = 实验有效（结论另看报告）；1 = 实验无效（确定性/策略完整性/
 * 覆盖破裂）；2 = 用法或环境错误（语料缺失、二进制缺失）。
 */

import { spawn } from './lib/child-guard.mjs';
import { createHash } from 'crypto';
import {
  cpSync, createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync,
  readdirSync, renameSync, writeFileSync,
} from 'fs';
import { createInterface } from 'readline';
import { tmpdir } from 'os';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(join(__dirname, '..'));

// ── 预声明的实验 ────────────────────────────────────────────────────────────

const ARMS = {
  cash: {
    key: 'cash',
    policy: 'cash',
    // #684/#686 现状：回退名义额即时进现金。无任何旋钮（边界 #3）。
    env: { BK_UNVERIFIED_EXPIRY_POLICY: 'cash' },
  },
  receivable: {
    key: 'receivable',
    policy: 'receivable',
    // #991：估算钱进 receivable 池，venue truth 来了才 redemption。
    env: { BK_UNVERIFIED_EXPIRY_POLICY: 'receivable' },
  },
};
const ARM_KEYS = Object.keys(ARMS);

// ── args ────────────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const has = (name) => args.includes(name);

const SELF_TEST = has('--self-test');
const DRY_RUN = has('--dry-run');
const REPORT_ONLY = flag('--report-only');
const SINGLE_RUN = has('--single-run');
const onlyArg = flag('--only') ? String(flag('--only')).toLowerCase().split(',').map((s) => s.trim()).filter(Boolean) : null;
const runArms = onlyArg ?? ARM_KEYS;
const seedBalance = flag('--seed-balance') ?? '1000';
const maxOrderNotional = flag('--max-order-notional') ?? '12';
const minShares = flag('--min-shares') ?? '1';
const maxShares = flag('--max-shares') ?? '1';
const maxPositions = flag('--max-positions') ?? '10';
const minRoundsPerWindow = Number(flag('--rounds-per-window') ?? 200);
const windowsOverride = flag('--windows') ? Number(flag('--windows')) : null;
const limitWindows = flag('--limit-windows') ? Number(flag('--limit-windows')) : null;
const maxAgeMs = flag('--max-age-ms') ?? null; // BK_RECEIVABLE_MAX_AGE_MS（默认 48h，不传就不钉）
const runTimeoutMin = Number(flag('--run-timeout-min') ?? 30);
const CORE_BIN = flag('--core-bin') ?? process.env.BK_CORE_BIN ?? join(ROOT, 'target', 'release', 'blitzkrieg-core');
const corpusArg = flag('--corpus') ?? null;
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-') + 'Z';
const OUT_DIR = REPORT_ONLY ? resolve(REPORT_ONLY) : resolve(flag('--out') ?? join(ROOT, 'data', 'evolution', 'ab-receivable-991', stamp));

const utc = (ms) => (ms == null ? '—' : new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z'));
const r4 = (n) => (n == null ? '—' : (Math.round(n * 10000) / 10000).toString());
const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');
const num = (v) => {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};

function usage(msg) {
  if (msg) console.error(`error: ${msg}`);
  console.error(`usage: node scripts/ab-receivable-991.mjs [--only cash,receivable] [--corpus <15m-act.jsonl>]
       [--rounds-per-window 200] [--windows N] [--limit-windows K] [--seed-balance 1000]
       [--max-order-notional 12] [--max-age-ms <ms>] [--single-run] [--out dir] [--dry-run]
       [--report-only dir] [--core-bin path] [--self-test]`);
  process.exit(2);
}

// 臂环境：唯一允许的差异就是策略键本身（+ 显式钉住的老化预算）。任何别的
// BK_* 注入都是处理变量，这里拒绝。
function armEnv(arm, maxAge) {
  const env = { ...arm.env };
  if (maxAge != null) env.BK_RECEIVABLE_MAX_AGE_MS = String(maxAge);
  return env;
}

// ── 回放命令（两臂 argv 逐字节相同 —— 差异只在 env）────────────────────────

function buildArgs(stagedDir, windowFile, reportPath) {
  return [
    '--mode', 'dry',
    '--engine',
    '--no-discovery',
    '--no-strategy-state',
    '--no-strategy-dir',
    '--lua-strategy-dir', stagedDir,
    '--enable-strategy', 'lua_momentum',
    '--no-trade-log', '--no-order-log', '--no-position-log', '--no-intent-audit',
    '--no-event-archive',
    '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
    '--seed-balance', String(seedBalance),
    '--max-order-notional', String(maxOrderNotional),
    '--min-shares', String(minShares), '--max-shares', String(maxShares),
    '--max-positions', String(maxPositions),
    '--backtest', windowFile,
    '--backtest-report', reportPath,
    '--backtest-tick-ms', '50',
    '--backtest-tail-ms', '0',
    '--backtest-fast',
  ];
}

// ── 语料预检（manifest sha256 + #890 门禁，与 ab-exit-964 同纪律）───────────

function discoverCorpus() {
  if (corpusArg) {
    const p = resolve(ROOT, corpusArg);
    if (!existsSync(p)) usage(`corpus not found: ${p}`);
    return p;
  }
  const dir = join(ROOT, 'data', 'onchain');
  if (!existsSync(dir)) usage('data/onchain 不存在 —— 15m-act 语料是本实验的主坐标（--corpus 可指定别处）');
  const found = readdirSync(dir)
    .filter((f) => /-15m-act-[^.]*\.jsonl$/.test(f) && !/\.(trades|cashflows|redemptions)\.jsonl$/.test(f))
    .sort();
  if (found.length === 0) usage('data/onchain 下没有 15m-act 语料（找 *-15m-act-*.jsonl）');
  if (found.length > 1) usage(`data/onchain 下有多份 15m-act 语料，请用 --corpus 指定：\n  ${found.join('\n  ')}`);
  return join(dir, found[0]);
}

function runFixResolutionsCheck(corpus) {
  return new Promise((res) => {
    const child = spawn('python3', [join(ROOT, 'scripts', 'corpus', 'fix_resolutions.py'), '--check', corpus], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (d) => { out += d.toString(); });
    child.stderr.on('data', (d) => { out += d.toString(); });
    child.on('exit', (code) => res({ code, out: out.trim() }));
  });
}

async function preflightCorpus(corpus) {
  if (!existsSync(corpus)) usage(`corpus not found: ${corpus}`);
  const manifestPath = corpus.replace(/\.jsonl$/, '.manifest.json');
  if (!existsSync(manifestPath)) usage(`corpus manifest not found: ${manifestPath}`);
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const sha = sha256File(corpus);
  if (manifest.sha256?.events && manifest.sha256.events !== sha) {
    console.error(`corpus sha256 mismatch: file ${sha} != manifest ${manifest.sha256.events}`);
    console.error('语料在 manifest 之后被改动过 —— 先重跑 relabel/构建链，或用 --corpus 指向钉死的那份。');
    process.exit(2);
  }
  const check = await runFixResolutionsCheck(corpus);
  const m = check.out.match(/lines=(\d+) resolutions_ok=(\d+) resolutions_bad=(\d+)/);
  const resBad = m ? Number(m[3]) : null;
  if (check.code !== 0 || resBad !== 0) {
    console.error(`fix_resolutions --check FAILED (exit ${check.code}):`);
    console.error(check.out);
    process.exit(2);
  }
  return {
    corpus,
    sha256: sha,
    manifest: {
      market: manifest.market ?? null,
      startMs: manifest.startMs ?? null,
      endMs: manifest.endMs ?? null,
      counts: manifest.counts ?? null,
      sha256Events: manifest.sha256?.events ?? null,
    },
    fixResolutions: { lines: m ? Number(m[1]) : null, ok: m ? Number(m[2]) : null, bad: resBad },
  };
}

// ── 语料扫描：轮次 / 结算行（一次流式过）────────────────────────────────────

async function scanCorpus(corpus) {
  const rounds = [];
  const resLineOf = new Map();
  const resAtLine = new Map();
  let totalLines = 0;
  let firstAtMs = null, lastAtMs = null;
  const rl = createInterface({ input: createReadStream(corpus), crlfDelay: Infinity });
  for await (const line of rl) {
    totalLines++;
    if (!line) continue;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (Number.isFinite(at)) {
      if (firstAtMs == null) firstAtMs = at;
      lastAtMs = at;
    }
    if (line.includes('"k":"round"')) {
      let d;
      try { d = JSON.parse(line); } catch { continue; }
      const slot = d.m?.[0]?.roundSlot ?? null;
      const assets = [], conds = [];
      let expiresAtMs = null;
      for (const mk of d.m ?? []) {
        if (mk.conditionId) conds.push(mk.conditionId);
        if (mk.asset) assets.push(mk.asset);
        if (mk.expiresAtMs) expiresAtMs = mk.expiresAtMs;
      }
      rounds.push({ line: totalLines, at: d.at, slot, expiresAtMs, assets, conds });
    } else if (line.includes('"k":"resolution"')) {
      const c = line.match(/"c":"(0x[0-9a-f]+)"/)?.[1];
      if (c) {
        if (!resLineOf.has(c)) resLineOf.set(c, totalLines);
        if (!resAtLine.has(totalLines)) resAtLine.set(totalLines, []);
        resAtLine.get(totalLines).push(c);
      }
    }
  }
  return { rounds, resLineOf, resAtLine, totalLines, firstAtMs, lastAtMs };
}

// ── 窗口规划（轮数等分 + 结算完整性校验，不挑窗口）──────────────────────────

const RES_TAIL_SCAN = 64;

function planWindows(scan, minRounds, windowsN, limitK) {
  const { rounds, resLineOf, resAtLine, totalLines } = scan;
  const total = rounds.length;
  if (total === 0) return { error: '语料里没有 k=round 行 —— 这不是 15m-act 语料？' };
  let n;
  if (windowsN) {
    n = windowsN;
    if (!Number.isInteger(n) || n < 1 || n > 48) return { error: '--windows must be an integer in 1..48' };
    if (n * minRounds > total) return { error: `--windows ${n} × --rounds-per-window ${minRounds} > 语料总轮数 ${total}` };
  } else {
    if (!Number.isInteger(minRounds) || minRounds < 1) return { error: '--rounds-per-window must be a positive integer' };
    n = Math.max(1, Math.floor(total / minRounds));
  }
  const base = Math.floor(total / n);
  const rem = total - base * n;
  const windows = [];
  let i = 0;
  for (let w = 0; w < n; w++) {
    const count = base + (w < rem ? 1 : 0);
    const first = rounds[i], last = rounds[i + count - 1];
    const nextFirst = rounds[i + count];
    const startLine = first.line;
    let endLine = nextFirst ? nextFirst.line - 1 : totalLines;
    const conds = new Set(), assets = new Set();
    for (let k = i; k < i + count; k++) {
      rounds[k].conds.forEach((c) => conds.add(c));
      rounds[k].assets.forEach((a) => assets.add(a));
    }
    if (nextFirst) {
      const upto = Math.min(totalLines, nextFirst.line + RES_TAIL_SCAN);
      for (let li = nextFirst.line; li <= upto; li++) {
        const condsHere = resAtLine.get(li);
        if (condsHere && condsHere.some((c) => conds.has(c))) endLine = Math.max(endLine, li);
      }
    }
    // 结算完整性：本窗每一轮的 resolution 行必须落在本窗切片内（防静默丢结算）。
    let missingResolutions = 0;
    const missingExample = [];
    for (const c of conds) {
      const rl2 = resLineOf.get(c);
      if (rl2 != null && (rl2 < startLine || rl2 > endLine)) {
        missingResolutions++;
        if (missingExample.length < 3) missingExample.push(c);
      }
    }
    windows.push({
      index: w,
      name: `win-${String(w).padStart(2, '0')}`,
      startLine, endLine,
      lines: endLine - startLine + 1,
      rounds: count,
      firstAtMs: first.at, lastRoundAtMs: last.at,
      expiryAtMs: last.expiresAtMs,
      assets: [...assets].sort(),
      conds: conds.size,
      missingResolutions, missingExample,
    });
    i += count;
  }
  const bad = windows.filter((w) => w.missingResolutions > 0);
  if (bad.length) {
    return { error: `窗口结算完整性校验失败 —— resolution 行落在切片外（静默丢结算），e.g. ${bad[0].name}: ${bad[0].missingExample.join(',')}` };
  }
  const small = windows.filter((w) => w.rounds < minRounds);
  if (small.length) {
    return { error: `规划出的窗口有 ${small.length} 个不足 ${minRounds} 轮 —— 调小 --rounds-per-window 或 --windows` };
  }
  const taken = limitK != null ? windows.slice(0, Math.max(1, limitK)) : windows;
  return { windows: taken, plannedTotal: windows.length, limited: limitK != null && limitK < windows.length };
}

async function sliceWindow(corpus, win, outPath) {
  const manifestPath = join(dirname(outPath), 'slices-manifest.json');
  let slices = {};
  if (existsSync(manifestPath)) {
    try { slices = JSON.parse(readFileSync(manifestPath, 'utf8')); } catch { slices = {}; }
  }
  const prev = slices[win.name];
  if (existsSync(outPath) && prev && prev.startLine === win.startLine && prev.endLine === win.endLine) {
    const sha = sha256File(outPath);
    if (sha === prev.sha256) return { path: outPath, sha256: sha, cached: true };
  }
  const tmp = outPath + '.part';
  const ws = createWriteStream(tmp);
  const rl = createInterface({ input: createReadStream(corpus), crlfDelay: Infinity });
  let n = 0;
  for await (const line of rl) {
    n++;
    if (n < win.startLine) continue;
    if (n > win.endLine) break;
    await new Promise((res) => ws.write(line + '\n', res));
  }
  await new Promise((res) => ws.end(res));
  renameSync(tmp, outPath);
  const sha = sha256File(outPath);
  slices[win.name] = { startLine: win.startLine, endLine: win.endLine, sha256: sha };
  writeFileSync(manifestPath, JSON.stringify(slices, null, 2) + '\n');
  return { path: outPath, sha256: sha, cached: false };
}

// ── 跑一遍回放（#265 自检 + 断点续跑 + watchdog）────────────────────────────

function parseSelfCheck(stderr) {
  const m = stderr.match(/strategy startup self-check: requested=(\d+) resolved=(\d+) enabled=\[([^\]]*)\]/);
  if (!m) return null;
  return { requested: Number(m[1]), resolved: Number(m[2]), enabled: m[3].split(',').map((s) => s.trim()).filter(Boolean) };
}

function runBacktest(cmd, reportPath, armEnvObj, label) {
  return new Promise((res) => {
    const envFile = reportPath.replace(/\.json$/, '.env.txt');
    const cmdFile = reportPath.replace(/\.json$/, '.cmd.txt');
    const envLine = JSON.stringify(armEnvObj);
    if (existsSync(reportPath) && existsSync(cmdFile) && existsSync(envFile)
      && readFileSync(cmdFile, 'utf8') === cmd.join(' ') + '\n'
      && readFileSync(envFile, 'utf8') === envLine + '\n') {
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), cached: true, invalid: null });
        return;
      } catch { /* 残缺文件 —— 重跑 */ }
    }
    console.log(`    $ BK_UNVERIFIED_EXPIRY_POLICY=${armEnvObj.BK_UNVERIFIED_EXPIRY_POLICY} blitzkrieg-core …   # ${label}`);
    let stderr = '';
    const child = spawn(CORE_BIN, cmd, {
      stdio: ['ignore', 'ignore', 'pipe'],
      cwd: OUT_DIR,
      env: { ...process.env, ...armEnvObj },
    });
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    const watchdog = setTimeout(() => {
      console.error(`    TIMEOUT after ${runTimeoutMin} min — stopping the run group`);
      try { process.kill(-child.pid, 'SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
    }, runTimeoutMin * 60_000);
    child.on('exit', (code, signal) => {
      clearTimeout(watchdog);
      const sc = parseSelfCheck(stderr);
      if (/unknown strategy requested/.test(stderr)) {
        res({ report: null, cached: false, invalid: 'kernel log: unknown strategy requested (包没加载)' });
        return;
      }
      if (!sc) {
        res({ report: null, cached: false, invalid: 'kernel log: 无 strategy startup self-check 行（二进制过旧？）' });
        return;
      }
      if (sc.resolved < 1 || !sc.enabled.includes('lua_momentum')) {
        res({ report: null, cached: false, invalid: `kernel log: 启用集 [${sc.enabled.join(', ')}] 不含 lua_momentum` });
        return;
      }
      if (code !== 0) {
        console.error(`    backtest exited code=${code} signal=${signal ?? child.signalCode ?? 'none'} (${label}); 拒绝从失败运行读报告`);
        console.error(stderr.split('\n').slice(-8).join('\n'));
        process.exit(1);
      }
      writeFileSync(cmdFile, cmd.join(' ') + '\n');
      writeFileSync(envFile, envLine + '\n');
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), cached: false, invalid: null });
      } catch (e) {
        console.error(`    cannot parse report ${reportPath}: ${e.message}`);
        process.exit(1);
      }
    });
  });
}

// ── 指标抽取（预声明四维度的每窗实现）───────────────────────────────────────

const FALLBACK_REASON = 'UnverifiableExpiry';
const BREAKER_RE = /consecutive-loss breaker tripped|trading frozen|daily-loss/;
const FALLBACK_CLOSE_RE = /expiry fallback: closed (\d+) position/;
const REDEEM_RE = /fallback receivable redeemed: position ([^ ]+) booked ([-\d.]+) USDC, venue truth ([-\d.]+) USDC \(Δ([-\d.]+)\)/;
const AGING_RE = /receivable aging: (\d+) fallback claim/;

function extractMetrics(rep, win) {
  const t = rep.trades ?? {};
  const feed = rep.feed ?? {};
  const settlement = feed.settlement ?? {};
  const daily = feed.dailyLoss ?? {};
  const lines = Array.isArray(rep.tradeLines) ? rep.tradeLines : [];
  const strategyNames = [...new Set(lines.map((l) => l.strategy))];
  const statsNames = (rep.strategies ?? []).map((s) => s.name).filter(Boolean);

  let fallbackCloses = 0, fallbackNetUsd = 0;
  const fallbackNets = [];
  for (const l of lines) {
    if (String(l.reason ?? '') === FALLBACK_REASON) {
      fallbackCloses++;
      const pnl = num(l.netPnlUsd);
      if (pnl != null) { fallbackNetUsd += pnl; fallbackNets.push(pnl); }
    }
  }
  fallbackNets.sort((a, b) => a - b);

  const alerts = Array.isArray(rep.riskAlerts) ? rep.riskAlerts : [];
  let breakerAlerts = 0, fallbackCloseAlerts = 0, redeemAlerts = 0, agingAlerts = 0;
  const redeemedDeltas = [];
  for (const a of alerts) {
    if (BREAKER_RE.test(a)) breakerAlerts++;
    const fc = a.match(FALLBACK_CLOSE_RE);
    if (fc) fallbackCloseAlerts += Number(fc[1]);
    const rd = a.match(REDEEM_RE);
    if (rd) { redeemAlerts++; const d = num(rd[4]); if (d != null) redeemedDeltas.push(d); }
    if (AGING_RE.test(a)) agingAlerts++;
  }
  const orders = rep.orders ?? {};
  const fillStats = rep.fillStats ?? {};

  return {
    window: win.name,
    windowRounds: win.rounds,
    // 策略完整性：报告必须自证它跑在哪个策略下
    policyObserved: settlement.unverifiedExpiryPolicy ?? null,
    fallbackReceivableUsd: num(settlement.fallbackReceivableUsd),
    pendingFallbackClaims: settlement.pendingFallbackClaims ?? null,
    oldestFallbackClaimAgeMs: num(settlement.oldestFallbackClaimAgeMs),
    // 维度 1：day-budget
    dayBudget: {
      dayIndex: daily.dayIndex ?? null,
      openingEquityUsd: num(daily.openingEquityUsd),
      realizedPnlUsd: num(daily.realizedPnlUsd),
      limitUsd: num(daily.limitUsd),
      tripped: daily.tripped ?? null,
      anyTripped: daily.anyTripped ?? null,
    },
    // 维度 2：breaker / 回退事件计数
    alerts: {
      breaker: breakerAlerts,
      fallbackClosePositions: fallbackCloseAlerts,
      redemptions: redeemAlerts,
      redeemDeltaSum: redeemedDeltas.reduce((s, d) => s + d, 0),
      aging: agingAlerts,
      total: alerts.length,
    },
    // 维度 3：下单→成交吞吐
    throughput: {
      orders: num(orders.orders) ?? 0,
      filled: num(orders.filled) ?? 0,
      rejected: num(orders.rejected) ?? 0,
      fillRatePct: num(fillStats.fillRatePct),
      partialRatePct: num(fillStats.partialRatePct),
      fills: num(rep.fills) ?? 0,
    },
    // 维度 4：每臂净
    closed: num(t.closed) ?? 0,
    wins: num(t.wins) ?? 0,
    losses: num(t.losses) ?? 0,
    netPnlUsd: num(t.netPnlUsd),
    grossProfitUsd: num(t.grossProfitUsd),
    grossLossUsd: num(t.grossLossUsd),
    feesUsd: num(t.feesUsd),
    maxDrawdownUsd: num(t.maxDrawdownUsd),
    fallbackCloses,
    fallbackNetUsd,
    tradesSha256: rep.tradesSha256 ?? null,
    strategyNames,
    statsNames,
  };
}

// ── 汇总 + 判决 ─────────────────────────────────────────────────────────────

function aggregate(armKey, perWindow) {
  const all = perWindow.filter(Boolean);
  const sum = (f) => all.reduce((s, m) => s + (f(m) ?? 0), 0);
  const agg = {
    arm: armKey,
    windowsRun: all.length,
    rounds: sum((m) => m.windowRounds),
    closed: sum((m) => m.closed),
    wins: sum((m) => m.wins),
    losses: sum((m) => m.losses),
    netPnlUsd: sum((m) => m.netPnlUsd),
    grossProfitUsd: sum((m) => m.grossProfitUsd),
    grossLossUsd: sum((m) => m.grossLossUsd),
    feesUsd: sum((m) => m.feesUsd),
    maxDrawdownUsd: Math.max(0, ...all.map((m) => m.maxDrawdownUsd ?? 0)),
    fallbackCloses: sum((m) => m.fallbackCloses),
    fallbackNetUsd: sum((m) => m.fallbackNetUsd),
    fallbackReceivableUsdEnd: sum((m) => m.fallbackReceivableUsd),
    pendingFallbackClaimsEnd: sum((m) => m.pendingFallbackClaims),
    alerts: {
      breaker: sum((m) => m.alerts.breaker),
      fallbackClosePositions: sum((m) => m.alerts.fallbackClosePositions),
      redemptions: sum((m) => m.alerts.redemptions),
      redeemDeltaSum: sum((m) => m.alerts.redeemDeltaSum),
      aging: sum((m) => m.alerts.aging),
    },
    throughput: {
      orders: sum((m) => m.throughput.orders),
      filled: sum((m) => m.throughput.filled),
      rejected: sum((m) => m.throughput.rejected),
      fills: sum((m) => m.throughput.fills),
    },
    dayBudget: {
      openingEquityUsdEnd: all.length ? all[all.length - 1].dayBudget.openingEquityUsd : null,
      realizedPnlUsdEnd: all.length ? all[all.length - 1].dayBudget.realizedPnlUsd : null,
      limitUsdEnd: all.length ? all[all.length - 1].dayBudget.limitUsd : null,
      trippedAnyWindow: all.some((m) => m.dayBudget.tripped === true || m.dayBudget.anyTripped === true),
    },
    determinism: all.every((m) => m.determinism === 'match') ? 'match'
      : all.some((m) => m.determinism === 'mismatch') ? 'mismatch'
        : all.every((m) => m.determinism === 'skipped') ? 'skipped' : 'mixed',
    perWindow: all,
  };
  agg.policyConsistent = all.every((m) => m.policyObserved === ARMS[armKey].policy);
  agg.contaminated = agg.perWindow.some((w) => (w.strategyNames ?? []).some((s) => s !== 'lua_momentum') || (w.statsNames ?? []).some((s) => s !== 'lua_momentum'));
  return agg;
}

// 对照表：cash = base（现状），receivable = cand（候选）。delta = receivable − cash。
const VERDICT_METRICS = [
  ['netPnlUsd', '净 PnL USD', 'info'],
  ['closed', '平仓笔数', 'info'],
  ['fallbackCloses', '回退平仓数', 'info'],
  ['fallbackNetUsd', '回退平仓净额 USD', 'info'],
  ['alerts.breaker', 'breaker 触发次数', 'lower'],
  ['alerts.redemptions', '窗内 redemption 数', 'info'],
  ['alerts.redeemDeltaSum', 'redemption 修正合计 USD', 'info'],
  ['alerts.aging', '超龄告警数', 'info'],
  ['throughput.orders', '下单数', 'info'],
  ['throughput.filled', '成交单数', 'info'],
  ['throughput.fillRatePct', '成交率 %', 'higher'],
  ['dayBudget.openingEquityUsdEnd', '末日预算基数 USD', 'info'],
  ['dayBudget.realizedPnlUsdEnd', '末日已实现 PnL USD', 'info'],
  ['dayBudget.trippedAnyWindow', '日内损失熔断触发过', 'lower'],
  ['fallbackReceivableUsdEnd', '期末挂账应收 USD', 'info'],
  ['maxDrawdownUsd', '最大回撤 USD', 'lower'],
  ['feesUsd', '费用 USD', 'lower'],
];

function metricGet(agg, key) {
  if (agg == null) return null;
  if (key.includes('.')) {
    const [a, b] = key.split('.');
    return agg[a]?.[b] ?? null;
  }
  return agg[key] ?? null;
}

function compareRows(base, cand) {
  return VERDICT_METRICS.map(([key, label, dir]) => {
    const a = metricGet(base, key), b = metricGet(cand, key);
    let verdict = 'n/a';
    if (a != null && b != null && dir !== 'info') {
      const better = dir === 'lower' ? b < a : b > a;
      const worse = dir === 'lower' ? b > a : b < a;
      verdict = better ? 'better' : worse ? 'worse' : 'equal';
    } else if (dir === 'info') verdict = 'info';
    return { key, label, dir, a, b, delta: a != null && b != null ? b - a : null, verdict };
  });
}

function verdictOf(aggs, coverage, opts) {
  const gates = {
    coverage: coverage.ok,
    coverageDetail: coverage.detail,
    limited: coverage.limited ?? false,
    determinism: Object.fromEntries(Object.entries(aggs).map(([k, v]) => [k, v.determinism])),
    policyIntegrity: Object.fromEntries(Object.entries(aggs).map(([k, v]) => [k, v.policyConsistent])),
    contamination: Object.fromEntries(Object.entries(aggs).map(([k, v]) => [k, v.contaminated ?? false])),
  };
  const usable = gates.coverage
    && Object.values(gates.determinism).every((d) => d === 'match' || d === 'skipped')
    && Object.values(gates.policyIntegrity).every((p) => p === true)
    && Object.values(gates.contamination).every((c) => !c);

  const totalFallback = Object.values(aggs).reduce((s, a) => s + (a?.fallbackCloses ?? 0), 0);
  let conclusion;
  if (Object.keys(aggs).length < 2 || !aggs.cash || !aggs.receivable) conclusion = 'pending';
  else if (!usable) conclusion = 'invalid';
  else if (totalFallback === 0) conclusion = 'no-fallback-observed';
  else conclusion = 'fallback-observed';
  const cmpRows = (aggs.cash && aggs.receivable) ? compareRows(aggs.cash, aggs.receivable) : [];
  return { usable, gates, totalFallback, rows: cmpRows, conclusion };
}

// ── 报告渲染 ────────────────────────────────────────────────────────────────

function renderMd(manifest, aggs, verdict) {
  const md = [];
  md.push(`# 回退应收账款 A/B — issue #991/#1002（cash vs receivable）`);
  md.push('');
  md.push(`生成：${utc(manifest.generatedAtMs)}；脚本：\`scripts/ab-receivable-991.mjs\`；run 目录：\`${manifest.outDir}\``);
  md.push('');
  md.push('## 预声明坐标');
  md.push('');
  md.push(`- 语料：\`${manifest.corpus.corpus}\`（sha256 \`${manifest.corpus.sha256.slice(0, 16)}…\`）；门禁 fix_resolutions --check ${manifest.corpus.fixResolutions.ok} ok / ${manifest.corpus.fixResolutions.bad} bad（#890）。`);
  md.push(`- 窗口：${manifest.windows.length} 窗（规划 ${manifest.plannedTotal}，轮数等分连续切片 + resolution 完整性校验${manifest.coverage.limited ? '；**--limit-windows 部分运行，非全量实验**' : ''}）。`);
  md.push(`- 资金：seed-balance ${manifest.seedBalance} USD；max-order-notional ${manifest.maxOrderNotional}；min/max shares ${manifest.minShares}/${manifest.maxShares}；max-positions ${manifest.maxPositions}；tick-ms ${manifest.tickMs}；--backtest-fast。`);
  md.push(`- 臂：唯一差异 = 启动 env \`BK_UNVERIFIED_EXPIRY_POLICY\`（cash = #684 现状；receivable = #991 应收账款模型）。argv 逐字节相同、无任何回退阈值旋钮（边界 #3）。`);
  md.push(`- 确定性：${manifest.singleRun ? 'single-run（--single-run，双跑校验未执行）' : '每臂×每窗双跑，报告 sha256 必须一致'}。`);
  md.push('');
  md.push('## 门禁');
  md.push('');
  md.push(`- 覆盖：${verdict.gates.coverage ? 'PASS' : 'FAIL'} — ${verdict.gates.coverageDetail}`);
  for (const [arm, d] of Object.entries(verdict.gates.determinism)) {
    md.push(`- 确定性 ${arm}：${d === 'match' ? 'PASS（双跑逐字节一致）' : d === 'skipped' ? 'SKIPPED（--single-run）' : `**FAIL（${d}）**`}`);
  }
  for (const [arm, p] of Object.entries(verdict.gates.policyIntegrity)) {
    md.push(`- 策略完整性 ${arm}：${p ? 'PASS（报告自证跑了本臂策略）' : `**FAIL（报告的 unverifiedExpiryPolicy ≠ ${arm}）**`}`);
  }
  for (const [arm, c] of Object.entries(verdict.gates.contamination)) {
    md.push(`- 归因污染 ${arm}：${c ? '**FAIL（报告里出现了 lua_momentum 之外的策略名）**' : 'PASS'}`);
  }
  md.push('');
  if (!verdict.usable) {
    md.push('> **本报告判无效** —— 上面至少一条门禁没过，结论区不给出可执行判决。');
    md.push('');
  }
  md.push('## receivable vs cash（Δ = receivable − cash）');
  md.push('');
  md.push('| 指标 | cash（现状） | receivable（#991） | Δ | 判定 |');
  md.push('|---|---|---|---|---|');
  for (const r of verdict.rows) {
    md.push(`| ${r.label} | ${r4(r.a)} | ${r4(r.b)} | ${r.delta == null ? '—' : r4(r.delta)} | ${r.verdict} |`);
  }
  md.push('');
  md.push('## 结论（模板，判定由脚本按门禁给出）');
  md.push('');
  if (verdict.conclusion === 'no-fallback-observed') {
    md.push('**本语料/窗口坐标下没有发生任何到期回退平仓**（`UnverifiableExpiry` 平仓数为 0）—— 回退路径未武装，两臂行为一致是预期而非证据。要观察回退，需要语料里存在「到期时 settlement query 仍未被 venue 回答」的轮次（resolution 行晚于到期 + 静默界），或另选坐标。');
  } else if (verdict.conclusion === 'fallback-observed') {
    md.push('**回退事件已发生** —— 上表即两策略在同一语料/窗口/资金下的实测差异。重点读：dayBudget.openingEquityUsdEnd（receivable 臂的预算基数应系统性低出挂账额——估算钱不进基数）、alerts.breaker（更小的基数 → 更早触发）、throughput（估算钱不可交易的下单能力差）、netPnlUsd（窗内 redemption 到位的净差）。');
  } else if (verdict.conclusion === 'invalid') {
    md.push('**门禁未过，本报告不可作为证据。**');
  } else {
    md.push('**pending** —— 臂数据缺失（未跑或跑失败）。');
  }
  md.push('');
  md.push('## 可追溯性');
  md.push('');
  md.push('| 产物 | 路径 |');
  md.push('|---|---|');
  md.push(`| run manifest（本报告的机器版） | \`${manifest.outDir}/ab-receivable-991.json\` |`);
  md.push(`| 窗口切片（复跑输入） | \`${manifest.outDir}/windows/\` |`);
  md.push(`| 逐窗双份回放报告 | \`${manifest.outDir}/runs/<arm>/<win>/{report-1,report-2}.json\` |`);
  md.push('');
  md.push('每个数字 = run manifest 里对应字段的直出或四则组合；重新渲染：`node scripts/ab-receivable-991.mjs --report-only <runDir>`。');
  md.push('');
  md.push('## 附录：逐窗指标');
  md.push('');
  for (const armKey of Object.keys(aggs)) {
    md.push(`### ${armKey} 臂`);
    md.push('');
    md.push('| 窗口 | 轮数 | policy(自证) | 回退平仓 | 回退净额 | 挂账应收(末) | breaker | redemption | 下单/成交 | 净PnL | 预算基数(末) | tradesSha256(前8) | 确定性 |');
    md.push('|---|---|---|---|---|---|---|---|---|---|---|---|---|');
    for (const w of aggs[armKey].perWindow) {
      md.push(`| ${w.window} | ${w.windowRounds} | ${w.policyObserved ?? '—'} | ${w.fallbackCloses} | ${r4(w.fallbackNetUsd)} | ${r4(w.fallbackReceivableUsd)} | ${w.alerts.breaker} | ${w.alerts.redemptions} | ${w.throughput.orders}/${w.throughput.filled} | ${r4(w.netPnlUsd)} | ${r4(w.dayBudget.openingEquityUsd)} | ${String(w.tradesSha256 ?? '').slice(0, 8)} | ${w.determinism} |`);
    }
    md.push('');
  }
  for (const w of manifest.windows) {
    md.push(`- ${w.name}: 行 [${w.startLine}..${w.endLine}]（${w.lines} 行），${w.rounds} 轮，${utc(w.firstAtMs)} → ${utc(w.expiryAtMs)}，资产 ${w.assets.join('/')}，切片 sha256 \`${w.sha256?.slice(0, 16) ?? '—'}…\``);
  }
  md.push('');
  return md.join('\n') + '\n';
}

// ── 框架自测（离线：合成夹具，不碰内核/语料/网络）───────────────────────────

export function selfTest() {
  const P = (name, fn) => {
    try {
      fn();
      console.log(`  PASS ${name}`);
      return true;
    } catch (e) {
      console.error(`  FAIL ${name}: ${e.message}`);
      return false;
    }
  };
  const eq = (a, b, msg) => {
    if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`${msg ?? 'assert'}: ${JSON.stringify(a)} != ${JSON.stringify(b)}`);
  };
  let ok = true;

  // 1) 指标抽取：合成报告（策略自证 / 回退平仓计数 / alert 分类 / 吞吐）
  const win = { name: 'win-00', rounds: 3 };
  const rep = {
    trades: { closed: 4, wins: 2, losses: 2, grossProfitUsd: 3, grossLossUsd: 1, netPnlUsd: 2, maxDrawdownUsd: 0.8, feesUsd: 0.4 },
    tradeLines: [
      { conditionId: 'c1', reason: 'UnverifiableExpiry', strategy: 'lua_momentum', netPnlUsd: 1 },
      { conditionId: 'c2', reason: 'Settlement', strategy: 'lua_momentum', netPnlUsd: 2 },
      { conditionId: 'c3', reason: 'UnverifiableExpiry', strategy: 'lua_momentum', netPnlUsd: -0.5 },
      { conditionId: 'c4', reason: 'StopLoss', strategy: 'lua_momentum', netPnlUsd: -0.5 },
    ],
    orders: { orders: 10, filled: 7, rejected: 2, cancelled: 1, failed: 0, liveAtEnd: 0 },
    fillStats: { fillRatePct: 70, partialRatePct: 0 },
    fills: 9,
    feed: {
      settlement: { unverifiedExpiryPolicy: 'receivable', fallbackReceivableUsd: '2.5', pendingFallbackClaims: 1, oldestFallbackClaimAgeMs: 1000 },
      dailyLoss: { dayIndex: 5, openingEquityUsd: '998.0', realizedPnlUsd: '-1.5', limitUsd: '99.8', tripped: false, anyTripped: false },
    },
    riskAlerts: [
      'expiry fallback: closed 2 position(s) on market(s) the venue never answered (unverifiable-expiry, booked per policy receivable)',
      'consecutive-loss breaker tripped for strategy lua_momentum: 4 losses, halting its new entries 300s',
      'fallback receivable redeemed: position hft-1 booked 2.0 USDC, venue truth 3.5 USDC (Δ1.5)',
      'receivable aging: 1 fallback claim(s) overdue (oldest 172800001ms > 172800000ms)',
    ],
    tradesSha256: 'abc',
    strategies: [{ name: 'lua_momentum' }],
  };
  ok &= P('extractMetrics/策略自证与回退计数', () => {
    const m = extractMetrics(rep, win);
    eq(m.policyObserved, 'receivable');
    eq(m.fallbackCloses, 2);
    eq(m.fallbackNetUsd, 0.5);
    eq(m.fallbackReceivableUsd, 2.5);
    eq(m.pendingFallbackClaims, 1);
  });
  ok &= P('extractMetrics/day-budget维度', () => {
    const m = extractMetrics(rep, win);
    eq(m.dayBudget.openingEquityUsd, 998.0);
    eq(m.dayBudget.realizedPnlUsd, -1.5);
    eq(m.dayBudget.limitUsd, 99.8);
    eq(m.dayBudget.tripped, false);
  });
  ok &= P('extractMetrics/breaker与redemption告警分类', () => {
    const m = extractMetrics(rep, win);
    eq(m.alerts.breaker, 1);
    eq(m.alerts.fallbackClosePositions, 2);
    eq(m.alerts.redemptions, 1);
    eq(m.alerts.redeemDeltaSum, 1.5);
    eq(m.alerts.aging, 1);
  });
  ok &= P('extractMetrics/吞吐', () => {
    const m = extractMetrics(rep, win);
    eq(m.throughput.orders, 10);
    eq(m.throughput.filled, 7);
    eq(m.throughput.fillRatePct, 70);
    eq(m.throughput.fills, 9);
  });

  // 2) 汇总 + 策略一致性
  ok &= P('aggregate/跨窗合并与策略一致性', () => {
    const m1 = extractMetrics(rep, win);
    m1.determinism = 'match';
    const rep2 = JSON.parse(JSON.stringify(rep));
    rep2.feed.settlement.unverifiedExpiryPolicy = 'cash'; // 污染：报告自证错误策略
    const m2 = extractMetrics(rep2, win);
    m2.determinism = 'match';
    const bad = aggregate('receivable', [m1, m2]);
    eq(bad.policyConsistent, false);
    eq(bad.fallbackCloses, 4);
    eq(bad.alerts.breaker, 2);
    const good = aggregate('receivable', [m1, JSON.parse(JSON.stringify(m1))]);
    eq(good.policyConsistent, true);
  });

  // 3) 判决：确定性破裂 / 策略不完整 → invalid；零回退 → no-fallback-observed
  ok &= P('verdict/门禁方向', () => {
    const mk = (over) => ({
      windowsRun: 2, closed: 10, fallbackCloses: 1, fallbackNetUsd: 0.5,
      determinism: 'match', policyConsistent: true, contaminated: false,
      alerts: { breaker: 1, redemptions: 1, redeemDeltaSum: 0.5, aging: 0 },
      throughput: { orders: 20, filled: 15, rejected: 2, fills: 18 },
      dayBudget: { openingEquityUsdEnd: 998, realizedPnlUsdEnd: -1, limitUsdEnd: 99.8, trippedAnyWindow: false },
      fallbackReceivableUsdEnd: 0, maxDrawdownUsd: 1, feesUsd: 0.4,
      perWindow: [],
      ...over,
    });
    const v1 = verdictOf({ cash: mk({}), receivable: mk({}) }, { ok: true, detail: 't' });
    eq(v1.conclusion, 'fallback-observed');
    eq(v1.usable, true);
    const v2 = verdictOf({ cash: mk({}), receivable: mk({ determinism: 'mismatch' }) }, { ok: true, detail: 't' });
    eq(v2.conclusion, 'invalid');
    const v3 = verdictOf({ cash: mk({}), receivable: mk({ policyConsistent: false }) }, { ok: true, detail: 't' });
    eq(v3.usable, false);
    const v4 = verdictOf({ cash: mk({ fallbackCloses: 0 }), receivable: mk({ fallbackCloses: 0 }) }, { ok: true, detail: 't' });
    eq(v4.conclusion, 'no-fallback-observed');
    eq(v4.usable, true);
    const v5 = verdictOf({ cash: mk({}) }, { ok: true, detail: 't' });
    eq(v5.conclusion, 'pending');
  });

  // 4) 对照表方向：cand=receivable，breaker 更少 = better
  ok &= P('compareRows/判定方向', () => {
    const rows = compareRows(
      { alerts: { breaker: 3 }, throughput: { fillRatePct: 70 }, dayBudget: { trippedAnyWindow: true } },
      { alerts: { breaker: 1 }, throughput: { fillRatePct: 68 }, dayBudget: { trippedAnyWindow: false } },
    );
    const br = rows.find((r) => r.key === 'alerts.breaker');
    eq(br.verdict, 'better');
    eq(br.delta, -2);
    const fr = rows.find((r) => r.key === 'throughput.fillRatePct');
    eq(fr.verdict, 'worse'); // 成交率是 higher-更好：68 < 70
  });

  // 5) 臂环境：唯一差异 = 策略键（边界 #3：不发明任何阈值旋钮）
  ok &= P('armEnv/唯一差异是策略键', () => {
    const a = armEnv(ARMS.cash, null);
    const b = armEnv(ARMS.receivable, null);
    eq(Object.keys(a).sort(), ['BK_UNVERIFIED_EXPIRY_POLICY']);
    eq(a.BK_UNVERIFIED_EXPIRY_POLICY, 'cash');
    eq(b.BK_UNVERIFIED_EXPIRY_POLICY, 'receivable');
    const a2 = armEnv(ARMS.cash, 480000), b2 = armEnv(ARMS.receivable, 480000);
    eq(a2.BK_RECEIVABLE_MAX_AGE_MS, '480000');
    eq(b2.BK_RECEIVABLE_MAX_AGE_MS, '480000');
  });

  // 6) 命令装配：两臂 argv 逐字节相同（策略只走 env）
  ok &= P('buildArgs/两臂argv逐字节相同', () => {
    const a = buildArgs('/tmp/stageA', 'win-00.jsonl', 'r1.json');
    const b = buildArgs('/tmp/stageB', 'win-00.jsonl', 'r2.json');
    eq(a.filter((x) => x !== '/tmp/stageA' && x !== 'r1.json'), b.filter((x) => x !== '/tmp/stageB' && x !== 'r2.json'));
    eq(a.includes('--backtest-knob'), false); // 边界 #3：没有快速确认旋钮
    eq(a[a.indexOf('--enable-strategy') + 1], 'lua_momentum');
  });

  // 7) 窗口规划：轮数等分 + resolution 完整性（合成扫描）
  ok &= P('planWindows/等分与完整性', () => {
    const scan = {
      rounds: [
        { line: 1, at: 1000, slot: 1, expiresAtMs: 1000, assets: ['BTC'], conds: ['c1'] },
        { line: 4, at: 2000, slot: 2, expiresAtMs: 2000, assets: ['BTC'], conds: ['c2'] },
        { line: 7, at: 3000, slot: 3, expiresAtMs: 3000, assets: ['ETH'], conds: ['c3'] },
        { line: 10, at: 4000, slot: 4, expiresAtMs: 4000, assets: ['ETH'], conds: ['c4'] },
      ],
      resLineOf: new Map([['c1', 3], ['c2', 6], ['c3', 9], ['c4', 12]]),
      resAtLine: new Map([[3, ['c1']], [6, ['c2']], [9, ['c3']], [12, ['c4']]]),
      totalLines: 12,
    };
    const p1 = planWindows(scan, 2, null, null);
    eq(p1.windows.length, 2);
    eq(p1.windows[0].rounds, 2);
    eq(p1.windows[0].startLine, 1);
    eq(p1.windows[0].endLine, 6); // 回拉吞掉 c1/c2 的 resolution 行
    eq(p1.limited, false);
    const p2 = planWindows(scan, 2, null, 1);
    eq(p2.windows.length, 1);
    eq(p2.limited, true);
    const badScan = { ...scan, resLineOf: new Map([['c1', 3], ['c2', 6], ['c3', 9], ['c4', 99]]) };
    const p3 = planWindows(badScan, 2, null, null);
    eq(!!p3.error, true);
  });

  console.log(ok ? '\nself-test: ALL PASS' : '\nself-test: FAILURES');
  process.exit(ok ? 0 : 1);
}

// ── main ────────────────────────────────────────────────────────────────────

if (SELF_TEST) selfTest();

if (runArms.some((k) => !ARMS[k])) usage(`--only 只接受 ${ARM_KEYS.join('/')} 的组合，收到: ${runArms.join(',')}`);
if (maxAgeMs != null && !(Number(maxAgeMs) >= 0)) usage('--max-age-ms 必须是非负毫秒数');
if (limitWindows != null && (!Number.isInteger(limitWindows) || limitWindows < 1)) usage('--limit-windows 必须是正整数');

if (REPORT_ONLY) {
  const manifestPath = join(OUT_DIR, 'ab-receivable-991.json');
  if (!existsSync(manifestPath)) usage(`run manifest not found: ${manifestPath}`);
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const aggs = {};
  for (const armKey of manifest.arms ?? ARM_KEYS) {
    const perWindow = [];
    for (const win of manifest.windows ?? []) {
      const runDir = join(OUT_DIR, 'runs', armKey, win.name);
      const p1 = join(runDir, 'report-1.json');
      if (!existsSync(p1)) { perWindow.push(null); continue; }
      const rep = JSON.parse(readFileSync(p1, 'utf8'));
      const m = extractMetrics(rep, win);
      const p2 = join(runDir, 'report-2.json');
      m.determinism = existsSync(p2)
        ? (sha256File(p1) === sha256File(p2) ? 'match' : 'mismatch')
        : (manifest.singleRun ? 'skipped' : 'missing');
      perWindow.push(m);
    }
    aggs[armKey] = aggregate(armKey, perWindow);
  }
  const verdict = verdictOf(aggs, manifest.coverage ?? { ok: false, detail: 'n/a' });
  writeFileSync(join(OUT_DIR, 'ab-receivable-991.json'), JSON.stringify({ ...manifest, aggregates: aggs, verdict }, null, 2) + '\n');
  writeFileSync(join(OUT_DIR, 'ab-receivable-991.md'), renderMd({ ...manifest }, aggs, verdict));
  console.log(`report-only: rewrote ${join(OUT_DIR, 'ab-receivable-991.md')}`);
  process.exit(verdict.usable ? 0 : 1);
}

// —— 正式实验路径 ——

if (!existsSync(CORE_BIN)) {
  console.error(`core binary not found: ${CORE_BIN}`);
  console.error('先构建：cargo build --release -p blitzkrieg-launcher --locked（或用 --core-bin / BK_CORE_BIN 指向已构建二进制）');
  process.exit(2);
}
const ARM_KEYS_RUN = runArms.filter((k) => ARMS[k]);
const STAGE_DIR = join(tmpdir(), `bk-991-lua_momentum-${process.pid}`);
{
  const pkgDir = join(ROOT, 'user_layer', 'strategies_lua', 'lua_momentum');
  if (!existsSync(join(pkgDir, 'manifest.json'))) {
    console.error(`策略包未就绪：${pkgDir} 不存在或缺 manifest.json`);
    process.exit(2);
  }
  cpSync(pkgDir, join(STAGE_DIR, 'lua_momentum'), { recursive: true });
}

console.log(`ab-receivable-991 — issue #991 回退记账策略 A/B（臂 ${ARM_KEYS_RUN.join('/')}）`);
console.log(`run dir: ${OUT_DIR}`);
const corpus = discoverCorpus();
const pre = await preflightCorpus(corpus);
console.log(`corpus: ${corpus}`);
console.log(`  sha256 ${pre.sha256}`);
console.log(`  market=${pre.corpus === corpus ? (pre.manifest.market ?? '—') : '—'} ${utc(pre.manifest.startMs)} → ${utc(pre.manifest.endMs)}`);
console.log(`  fix_resolutions --check: ${pre.fixResolutions.lines} lines, ${pre.fixResolutions.ok} ok, ${pre.fixResolutions.bad} bad`);

const scan = await scanCorpus(corpus);
console.log(`scan: ${scan.totalLines} lines, ${scan.rounds.length} rounds (slots), resolutions ${scan.resLineOf.size}`);

const planned = planWindows(scan, minRoundsPerWindow, windowsOverride, limitWindows);
if (planned.error) {
  console.error(planned.error);
  process.exit(2);
}
const windows = planned.windows;
const totalRounds = windows.reduce((s, w) => s + w.rounds, 0);
const minWinRounds = Math.min(...windows.map((w) => w.rounds));
const coverage = {
  ok: minWinRounds >= 1 && totalRounds >= 1,
  limited: planned.limited,
  detail: planned.limited
    ? `LIMITED: 前 ${windows.length}/${planned.plannedTotal} 窗（共 ${totalRounds} 轮；冒烟/部分运行）`
    : `${windows.length} 窗 × ${minWinRounds}..${Math.max(...windows.map((w) => w.rounds))} 轮（共 ${totalRounds}）`,
};
console.log(`windows: ${coverage.detail}`);

if (DRY_RUN) {
  console.log('\ndry-run: 计划的回放矩阵（不执行、不切片）——');
  let n = 0;
  for (const armKey of ARM_KEYS_RUN) {
    const runs = SINGLE_RUN ? 1 : 2;
    n += windows.length * runs;
    console.log(`\n[${armKey}] BK_UNVERIFIED_EXPIRY_POLICY=${ARMS[armKey].policy}（${runs} 跑/窗 × ${windows.length} 窗 = ${windows.length * runs} 次回放；argv 与另一臂逐字节相同）`);
  }
  console.log(`\n总计 ${n} 次回放 + ${windows.length} 次切片（正式跑时写入 ${join(OUT_DIR, 'windows')}）。`);
  process.exit(0);
}

mkdirSync(join(OUT_DIR, 'windows'), { recursive: true });
mkdirSync(join(OUT_DIR, 'runs'), { recursive: true });

for (const win of windows) {
  const s = await sliceWindow(corpus, win, join(OUT_DIR, 'windows', `${win.name}.jsonl`));
  win.sha256 = s.sha256;
  console.log(`  ${win.name}: lines [${win.startLine}..${win.endLine}] ${win.rounds} 轮 ${utc(win.firstAtMs)}→${utc(win.expiryAtMs)} sha ${s.sha256.slice(0, 12)}${s.cached ? ' (cached)' : ''}`);
}

// 正式跑批（顺序执行：确定性优先）
const aggs = {};
const failures = [];
let lastUsable = true;
let lastConclusion = 'pending';
for (const armKey of ARM_KEYS_RUN) {
  const arm = ARMS[armKey];
  const envObj = armEnv(arm, maxAgeMs != null ? Number(maxAgeMs) : null);
  const perWindow = [];
  for (const win of windows) {
    const runDir = join(OUT_DIR, 'runs', armKey, win.name);
    mkdirSync(runDir, { recursive: true });
    const p1 = join(runDir, 'report-1.json');
    const p2 = join(runDir, 'report-2.json');
    const label = `${armKey}/${win.name}`;
    const cmd = buildArgs(STAGE_DIR, join(OUT_DIR, 'windows', `${win.name}.jsonl`), p1);
    const r1 = await runBacktest(cmd, p1, envObj, `${label} run1`);
    if (r1.invalid) {
      failures.push({ arm: armKey, win: win.name, reason: r1.invalid });
      perWindow.push(null);
      continue;
    }
    let determinism = 'skipped';
    if (!SINGLE_RUN) {
      const r2 = await runBacktest(buildArgs(STAGE_DIR, join(OUT_DIR, 'windows', `${win.name}.jsonl`), p2), p2, envObj, `${label} run2`);
      if (r2.invalid) {
        failures.push({ arm: armKey, win: win.name, reason: r2.invalid });
        perWindow.push(null);
        continue;
      }
      const h1 = sha256File(p1), h2 = sha256File(p2);
      determinism = h1 === h2 ? 'match' : 'mismatch';
      if (determinism === 'mismatch') {
        console.error(`    DETERMINISM MISMATCH ${label}: run1 ${h1.slice(0, 12)} vs run2 ${h2.slice(0, 12)} — 确定性 bug，实验无效`);
      }
    }
    const m = extractMetrics(r1.report, win);
    m.determinism = determinism;
    perWindow.push(m);
    console.log(`    ${label}: policy=${m.policyObserved} fallbackCloses=${m.fallbackCloses} breaker=${m.alerts.breaker} orders=${m.throughput.orders}/${m.throughput.filled} net=${r4(m.netPnlUsd)} 确定性=${determinism}`);
  }
  const agg = aggregate(armKey, perWindow);
  aggs[armKey] = agg;
  console.log(`  [${armKey}] 合计: fallbackCloses=${agg.fallbackCloses} breaker=${agg.alerts.breaker} redemptions=${agg.alerts.redemptions} orders=${agg.throughput.orders}/${agg.throughput.filled} net=${r4(agg.netPnlUsd)} policyConsistent=${agg.policyConsistent}`);
}

const verdict = verdictOf(aggs, coverage);
const manifest = {
  generatedAtMs: Date.now(),
  issue: '991/1002',
  script: 'scripts/ab-receivable-991.mjs',
  outDir: OUT_DIR,
  coreBin: CORE_BIN,
  coreBinSha256: sha256File(CORE_BIN),
  corpus: pre,
  windows,
  plannedTotal: planned.plannedTotal,
  arms: ARM_KEYS_RUN,
  armEnvs: Object.fromEntries(ARM_KEYS_RUN.map((k) => [k, armEnv(ARMS[k], maxAgeMs != null ? Number(maxAgeMs) : null)])),
  seedBalance, maxOrderNotional, minShares, maxShares, maxPositions,
  minRounds: minRoundsPerWindow,
  tickMs: 50,
  singleRun: SINGLE_RUN,
  maxAgeMs: maxAgeMs != null ? Number(maxAgeMs) : null,
  coverage,
  failures,
  aggregates: aggs,
  verdict,
};
writeFileSync(join(OUT_DIR, 'ab-receivable-991.json'), JSON.stringify(manifest, null, 2) + '\n');
writeFileSync(join(OUT_DIR, 'ab-receivable-991.md'), renderMd(manifest, aggs, verdict));
console.log(`\nreport: ${join(OUT_DIR, 'ab-receivable-991.json')}`);
console.log(`report: ${join(OUT_DIR, 'ab-receivable-991.md')}`);
console.log(`verdict: ${verdict.conclusion}（usable=${verdict.usable}）`);
lastUsable = verdict.usable;
lastConclusion = verdict.conclusion;

if (failures.length) {
  console.error(`\nINVALID: ${failures.length} 个回放被内核自检判无效（详见 ab-receivable-991.json .failures）。`);
  process.exitCode = 1;
}
if (!lastUsable) {
  console.error(`\nVERDICT: ${lastConclusion} —— 门禁未全绿（覆盖/确定性/策略完整性/归因污染，见报告），实验判无效。`);
  process.exitCode = 1;
}
