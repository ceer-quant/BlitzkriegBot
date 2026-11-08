#!/usr/bin/env node
/**
 * ab-exit-964 — stone 回测 A/B 对照实验（issue #964，Exit-First 判决）。
 *
 * 问题（#964）：出场机制对胜率/盈亏比有没有质变作用？三个臂，同一份语料、
 * 同一组窗口、同一笔资金，只换出场栈：
 *
 *   A 臂  lua_momentum         — 原默认出场（manifest 出厂 tunables），
 *                                入场为动量信号 + 纯 maker（never-escalate）。
 *   B 臂  coin_flip_exit_lab   — 抛硬币随机入场（剥离入场 alpha）+ 动态出场
 *                                栈（硬止损/时间加权止盈带/移动止盈/截止线/
 *                                thesis_death），coin_seed 固定。
 *   C 臂  coin_flip_exit_lab   — 对照组：同包同 seed，出场栈经 --backtest-knob
 *                                全部清零（#962 声明的对照组口径），只剩
 *                                exit_time_left_sec —— 度量「出场机制的孤立
 *                                贡献」= B − C。
 *
 * A vs B 回答 issue 的原题（好入场+呆板出场 vs 随机入场+动态出场）；
 * B vs C 把「出场」从「入场」里剥出来单独归因；两个随机入场臂（B/C）的
 * seed 相同、窗口相同、资金相同，差异只能来自出场栈。
 *
 * 语料坐标（预声明，不挑窗口）：
 *   data/onchain/ 的 15m-act 单文件（4,766 轮，2026-08-05 → 2026-10-01，
 *   BTC/BNB/DOGE/ETH/SOL/XRP）。跑批前门禁：manifest sha256 对文件实哈希、
 *   scripts/corpus/fix_resolutions.py --check（label-form settlement 行 =
 *   立即拒绝，#890）。窗口 = 轮数等分的连续切片（默认每窗 ≥200 轮），
 *   每窗校验「本窗各轮的 resolution 行都在切片内」——不完整就硬错，绝不
 *   静默丢结算。全部窗口进附录（#964 验收 3）。
 *
 * 写实档（主坐标，#964 审计结论）：
 *   --fee-model official --slippage-ticks 1 --backtest-tick-ms 50 --backtest-fast
 *   identity 档（引擎默认恒等成交模型）只作附录上界，用 --profile both 顺带跑。
 *
 * 确定性（#351 / #964 验收 1；#1004 加「非空」前置）：每个臂 × 每个窗口默认
 * 跑两遍，报告文件 sha256 必须逐字节一致、tradesSha256 相同 —— 不一致 =
 * 确定性 bug，判实验无效而不是当统计噪音。0 tradeLines 的臂（closed==0）
 * 两份空报告必然逐字节相等（「空==空」），记 vacuous（UNJUDGEABLE_EMPTY_RUN）
 * 而不是 PASS：空跑不构成确定性证据，带着 vacuous 臂的实验判不可用；
 * arm-starved 的结构化归因声明路径不受影响。--single-run 可跳过第二遍
 * （快速过框架用）。
 *
 * 平均持仓时长（预先声明的近似口径）：backtest 报告的 tradeLines 不带逐笔
 * 时间戳（引擎 schema 限制，backtest.rs TradeLine），逐笔秒数不可测。本脚本
 * 用 round_slot 近似：每笔交易按 conditionId 映射回语料轮次，持仓只可能
 * 存在于该轮窗 [expiresAt−roundDurationSec, expiresAt] 内（15m 语料 =
 * 900s），于是报三个量：
 *   - 到期结算率 settlementRatePct（精确，按出场 reason 分桶）；
 *   - avgHoldSecFloor = 结算笔记满轮、提前出场笔记 0（下界口径）；
 *   - avgHoldSecUpper = 全部笔记满轮（上界口径，15m 语料恒为 900s）。
 *   真实平均持仓 ∈ [floor, upper]。跨臂比较持仓结构请看 settlementRatePct
 *   与 reason 直方图（精确可测）；逐笔时长需要引擎给 tradeLines 加时间戳，
 *   另开 issue，本脚本不改引擎（#964 禁止触碰项）。
 *
 * 每臂只 stage 目标策略包到临时目录（#302 兄弟包耦合：默认 lua 目录会加载
 * 全部兄弟包，同场交易会污染记账与归因）。--no-strategy-dir 关 cdylib，
 * --lua-strategy-dir 指向只含本臂包的临时目录；报告里 strategies[] 出现
 * 任何别的名字 = 污染，实验判无效。
 *
 * 复跑 / 断点：所有中间产物落盘（窗口切片、双份报告、逐窗 tradeLines 导出、
 * run manifest），重跑时已解析的报告直接跳过（walk-forward-sweep.mjs 的
 * resume 语义）。--report-only <dir> 只从既有 run 目录重算汇总，不碰内核。
 *
 * 用法：
 *   node scripts/ab-exit-964.mjs                       # 全实验（A/B/C × 全窗口 × 双跑）
 *   node scripts/ab-exit-964.mjs --dry-run             # 只出计划：臂×窗口×命令行，不执行
 *   node scripts/ab-exit-964.mjs --arm A               # 只跑 A 臂
 *   node scripts/ab-exit-964.mjs --profile both        # 写实档 + identity 档（identity 只进附录）
 *   node scripts/ab-exit-964.mjs --report-only <runDir> # 从既有产物重算汇总
 *   node scripts/ab-exit-964.mjs --self-test           # 框架自测（离线，合成夹具，不碰内核/语料）
 *
 * 退出码：0 = 实验有效（结论另看报告）；1 = 实验无效（确定性破裂/覆盖不足/
 * 污染/内核失败）；2 = 用法或环境错误（语料缺失、B 包未就绪、二进制缺失）。
 */

import { spawn } from './lib/child-guard.mjs';
import { createHash } from 'crypto';
import {
  cpSync, createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync,
  readdirSync, renameSync, writeFileSync,
} from 'fs';
import { createInterface } from 'readline';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(join(__dirname, '..'));

// ── 预声明的实验（数字存在之前先钉死）───────────────────────────────────────

/** B/C 臂的抛硬币种子（#962 约定：manifest 默认 coin_seed=962；显式下发并记录）。 */
const DEFAULT_SEED = 962;

/**
 * 对照组（C 臂）的出场清零口径 —— #962 包 README 的声明，逐字对应：
 * hard 0 / tp 0（含 tight）/ trail 0（含 tight）/ death 带惰性化
 * reversal=100, tight=0 / stale=0，只剩 exit_time_left_sec。
 * 注意（如实声明）：stdlib thesis_death 的「盘口消失仍判死」语义不受
 * stale=0 影响（那是超时阈值，不是存在性判死的开关）——对照臂保留这
 * 一条 runtime 注入路径，与 #962 的对照臂口径一致，两随机臂同享。
 */
const CONTROL_KNOBS = [
  ['hard_stop_loss_pct', '0'],
  ['take_profit_pct', '0'],
  ['take_profit_pct_tight', '0'],
  ['trail_arm_pct', '0'],
  ['trail_giveback_pct', '0'],
  ['trail_giveback_pct_tight', '0'],
  ['thesis_death_reversal_pct', '100'],
  ['thesis_death_reversal_pct_tight', '0'],
  ['thesis_death_stale_ms', '0'],
];

const ARMS = {
  A: {
    key: 'A',
    pkg: 'lua_momentum',
    strategy: 'lua_momentum',
    // lua_momentum 声明入场纯 maker、never-escalate（manifest entry_maker）：
    // 负值 = 永不升级，逐字实现包声明。这是包契约，不是处理变量。
    entryMakerTimeoutMs: '-1',
    knobs: null, // 原默认出场 = manifest 出厂 tunables，一个旋钮都不动
    seed: null,
  },
  B: {
    key: 'B',
    pkg: 'coin_flip_exit_lab',
    strategy: 'coin_flip_exit_lab',
    // coin_flip 声明 taker 风格入场（manifest entry_maker=false）：回放里
    // 1ms 升级 = 抬所选 token 自己的 best ask（#962 包 README 的约定）。
    entryMakerTimeoutMs: '1',
    knobs: null, // 动态出场栈 = manifest 默认；seed 经 --backtest-knob 显式下发
    seed: 'seed',
  },
  C: {
    key: 'C',
    pkg: 'coin_flip_exit_lab',
    strategy: 'coin_flip_exit_lab',
    entryMakerTimeoutMs: '1',
    knobs: CONTROL_KNOBS,
    seed: 'seed',
  },
};

/** 写实档（主坐标）与 identity 档（附录上界）——#964 审计钉死的两档。 */
const PROFILES = {
  realistic: {
    key: 'realistic',
    feeModel: 'official',
    slippageTicks: '1',
    tickMs: '50',
    fast: true,
  },
  identity: {
    key: 'identity',
    feeModel: null,
    slippageTicks: null,
    tickMs: '50',
    fast: false,
  },
};

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
const armKeys = has('--arm') ? String(flag('--arm')).toUpperCase().split(',').map((s) => s.trim()).filter(Boolean) : ['A', 'B', 'C'];
const profileKey = flag('--profile') ?? 'realistic';
const seed = Number(flag('--seed') ?? DEFAULT_SEED);
const seedBalance = flag('--seed-balance') ?? '1000';
const maxOrderNotional = flag('--max-order-notional') ?? '12';
// 份额 = 1：top-only 语料的簿深度是名义 1 手（marketdata.rs update_top 的
// nominal size；语料 top 行不带量）。3 手单对着 1 手名义深度永远吃不满
// —— 962 档案书语料（真 L2 三档）才用 3/3。这是语料坐标决定的 sizing，
// 不是处理变量；两臂同值。--min-shares/--max-shares 可显式覆盖。
const minShares = flag('--min-shares') ?? '1';
const maxShares = flag('--max-shares') ?? '1';
const maxPositions = flag('--max-positions') ?? '10';
const minRoundsPerWindow = Number(flag('--rounds-per-window') ?? 200);
const windowsOverride = flag('--windows') ? Number(flag('--windows')) : null;
const runTimeoutMin = Number(flag('--run-timeout-min') ?? 30);
const CORE_BIN = flag('--core-bin') ?? process.env.BK_CORE_BIN ?? join(ROOT, 'target', 'release', 'blitzkrieg-core');
const BPkgOverride = flag('--b-pkg-dir') ?? process.env.BK_964_B_PKG ?? null;
const corpusArg = flag('--corpus') ?? null;
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-') + 'Z';
const OUT_DIR = REPORT_ONLY ? resolve(REPORT_ONLY) : resolve(flag('--out') ?? join(ROOT, 'data', 'evolution', 'ab-exit-964', stamp));

const utc = (ms) => (ms == null ? '—' : new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z'));
const r4 = (n) => (n == null ? '—' : (Math.round(n * 10000) / 10000).toString());
// 千分位分组，手写不用 toLocaleString —— 报告文本必须跨环境逐字节稳定。
const fmtInt = (n) => String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');
const num = (v) => {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};

function usage(msg) {
  if (msg) console.error(`error: ${msg}`);
  console.error(`usage: node scripts/ab-exit-964.mjs [--arm A,B,C] [--profile realistic|identity|both]
       [--corpus <15m-act.jsonl>] [--rounds-per-window 200] [--windows N] [--seed 962]
       [--seed-balance 1000] [--single-run] [--out dir] [--dry-run] [--report-only dir]
       [--core-bin path] [--b-pkg-dir path] [--self-test]`);
  process.exit(2);
}

// ── 尾部分位（线性插值，numpy 默认口径）─────────────────────────────────────

export function percentile(sortedAsc, p) {
  if (!sortedAsc.length) return null;
  if (sortedAsc.length === 1) return sortedAsc[0];
  const idx = (p / 100) * (sortedAsc.length - 1);
  const lo = Math.floor(idx), hi = Math.ceil(idx);
  if (lo === hi) return sortedAsc[lo];
  return sortedAsc[lo] + (sortedAsc[hi] - sortedAsc[lo]) * (idx - lo);
}

// ── 语料预检 ────────────────────────────────────────────────────────────────

function discoverCorpus() {
  if (corpusArg) {
    const p = resolve(ROOT, corpusArg);
    if (!existsSync(p)) usage(`corpus not found: ${p}`);
    return p;
  }
  const dir = join(ROOT, 'data', 'onchain');
  if (!existsSync(dir)) usage('data/onchain 不存在 —— 15m-act 语料是本实验的主坐标（--corpus 可指定别处）');
  // 单文件 15m-act 语料：排除 sidecar（trades/cashflows/redemptions/manifest）。
  const found = readdirSync(dir)
    .filter((f) => /-15m-act-[^.]*\.jsonl$/.test(f) && !/\.(trades|cashflows|redemptions)\.jsonl$/.test(f))
    .sort();
  if (found.length === 0) usage(`data/onchain 下没有 15m-act 语料（找 *-15m-act-*.jsonl）`);
  if (found.length > 1) usage(`data/onchain 下有多份 15m-act 语料，请用 --corpus 指定：\n  ${found.join('\n  ')}`);
  return join(dir, found[0]);
}

function runFixResolutionsCheck(corpus) {
  // scripts/corpus/README.md 的约定：每个消费语料的脚本都要把
  // fix_resolutions.py --check 接成门禁（label-form settlement 行 ⇒ 不许回放）。
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
    console.error('先修语料（python3 scripts/corpus/fix_resolutions.py --apply <corpus>；正式结算标签走 relabel_official.py --apply-corpus，需网络），再跑实验。');
    process.exit(2);
  }
  return {
    corpus,
    sha256: sha,
    manifestPath,
    manifest: {
      market: manifest.market ?? null,
      startMs: manifest.startMs ?? null,
      endMs: manifest.endMs ?? null,
      counts: manifest.counts ?? null,
      wallet: manifest.wallet ?? null,
      sha256Events: manifest.sha256?.events ?? null,
    },
    fixResolutions: { lines: m ? Number(m[1]) : null, ok: m ? Number(m[2]) : null, bad: resBad },
  };
}

// ── 语料扫描：轮次 / 结算行（一次流式过）────────────────────────────────────

async function scanCorpus(corpus) {
  const rounds = []; // { line, at, slot, expiresAtMs, durationSec, assets[], conds[] }
  const resLineOf = new Map(); // conditionId → resolution 行号
  const resAtLine = new Map(); // 行号 → 该行 resolution 的 conditionId[]（边界回拉用）
  const assetsAll = new Set();
  let totalLines = 0;
  let firstAtMs = null, lastAtMs = null;
  // top 报价锁价统计（#1004：报告里的结构归因数字必须从本次语料实算，
  // 禁止写死「实测」值）。k=top 行的 ba/bb 是报价字符串；等价 = 同一数值
  // 表示（"0.44" 与 "0.440" 都算锁价 —— 归因关心的是数值本身，不是字面）。
  let topQuotes = 0, topLocked = 0, topUnparsed = 0;
  const rl = createInterface({ input: createReadStream(corpus), crlfDelay: Infinity });
  for await (const line of rl) {
    totalLines++;
    if (!line) continue;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (Number.isFinite(at)) {
      if (firstAtMs == null) firstAtMs = at;
      lastAtMs = at;
    }
    if (line.includes('"k":"top"')) {
      let d;
      try { d = JSON.parse(line); } catch { d = null; }
      const ba = num(d?.ba), bb = num(d?.bb);
      if (ba == null || bb == null) topUnparsed++;
      else { topQuotes++; if (ba === bb) topLocked++; }
    }
    if (line.includes('"k":"round"')) {
      let d;
      try { d = JSON.parse(line); } catch { continue; }
      const slot = d.m?.[0]?.roundSlot ?? null;
      const assets = [], conds = [];
      let expiresAtMs = null, durationSec = null;
      for (const mk of d.m ?? []) {
        if (mk.conditionId) conds.push(mk.conditionId);
        if (mk.asset) { assets.push(mk.asset); assetsAll.add(mk.asset); }
        if (mk.expiresAtMs) expiresAtMs = mk.expiresAtMs;
        if (mk.roundDurationSec) durationSec = mk.roundDurationSec;
      }
      rounds.push({ line: totalLines, at: d.at, slot, expiresAtMs, durationSec, assets, conds });
    } else if (line.includes('"k":"resolution"')) {
      const c = line.match(/"c":"(0x[0-9a-f]+)"/)?.[1];
      if (c) {
        if (!resLineOf.has(c)) resLineOf.set(c, totalLines);
        if (!resAtLine.has(totalLines)) resAtLine.set(totalLines, []);
        resAtLine.get(totalLines).push(c);
      }
    }
  }
  return {
    rounds, resLineOf, resAtLine, totalLines, firstAtMs, lastAtMs,
    assetsAll: [...assetsAll].sort(),
    topStats: { quotes: topQuotes, locked: topLocked, unparsed: topUnparsed },
  };
}

// ── 窗口规划（轮数等分 + 结算完整性校验，不挑窗口）──────────────────────────

function planWindows(scan, minRounds, windowsN) {
  const { rounds, resLineOf, totalLines } = scan;
  const total = rounds.length;
  if (total === 0) usage('语料里没有 k=round 行 —— 这不是 15m-act 语料？');
  let n;
  if (windowsN) {
    n = windowsN;
    if (!Number.isInteger(n) || n < 1 || n > 48) usage('--windows must be an integer in 1..48');
    if (n * minRounds > total) {
      console.error(`--windows ${n} × --rounds-per-window ${minRounds} = ${n * minRounds} > 语料总轮数 ${total} —— 覆盖不了 ≥${minRounds} 轮/窗的验收下限`);
      process.exit(2);
    }
  } else {
    if (!Number.isInteger(minRounds) || minRounds < 1) usage('--rounds-per-window must be a positive integer');
    n = Math.max(1, Math.floor(total / minRounds));
  }
  const base = Math.floor(total / n);
  const rem = total - base * n;
  // 结算行与下一轮的 round 行在同一毫秒交错（语料实测：resolution 的 at ==
  // 下一轮 round 的 at，但行序可以落在 round 行之后）。窗口尾边界 = 边界
  // round 行向前回拉到「本窗条件集的最后一行 resolution」——回拉段里若夹着
  // 下一轮的 round/top 行也无害：内核按事件推进虚拟钟，同毫秒内不再触发
  // maintenance grid（tick-ms 50），被提前注册的下一轮市场拿不到任何一次
  // engine_evaluate，不可能产生跨窗仓位；窗口完整性校验在回拉后强制成立。
  const RES_TAIL_SCAN = 64;
  const windows = [];
  let i = 0;
  for (let w = 0; w < n; w++) {
    const count = base + (w < rem ? 1 : 0);
    const first = rounds[i], last = rounds[i + count - 1];
    const nextFirst = rounds[i + count];
    const startLine = first.line;
    let endLine = nextFirst ? nextFirst.line - 1 : totalLines;
    const conds = new Set(), slots = new Set(), assets = new Set();
    for (let k = i; k < i + count; k++) {
      rounds[k].conds.forEach((c) => conds.add(c));
      slots.add(rounds[k].slot);
      rounds[k].assets.forEach((a) => assets.add(a));
    }
    if (nextFirst) {
      // 在边界 round 行之后的小段里找本窗条件的 resolution 尾巴
      const upto = Math.min(totalLines, nextFirst.line + RES_TAIL_SCAN);
      for (let li = nextFirst.line; li <= upto; li++) {
        const condsHere = scan.resAtLine.get(li);
        if (condsHere && condsHere.some((c) => conds.has(c))) endLine = Math.max(endLine, li);
      }
    }
    // 结算完整性：本窗每一轮的 resolution 行必须落在本窗切片内。
    // 这条校验是防回归，不是摆设 —— 一旦违例，切片会静默丢结算，
    // 仓位悬空、closed 数缺一口，A/B 却还"对称"，最阴的一种数据丢失。
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
      firstSlot: first.slot, lastSlot: last.slot,
      firstAtMs: first.at, lastRoundAtMs: last.at,
      expiryAtMs: last.expiresAtMs,
      assets: [...assets].sort(),
      conds: conds.size,
      missingResolutions, missingExample,
      // 持仓时长近似的映射表（floor/upper 口径用）
      condSlot: Object.fromEntries(rounds.slice(i, i + count).flatMap((r) => r.conds.map((c) => [c, r.slot]))),
      slotDuration: Object.fromEntries(rounds.slice(i, i + count).map((r) => [r.slot, r.durationSec ?? 900])),
    });
    i += count;
  }
  const bad = windows.filter((w) => w.missingResolutions > 0);
  if (bad.length) {
    console.error('窗口结算完整性校验失败 —— 以下窗口的 resolution 行落在切片外（静默丢结算 = 数据丢失，拒绝回放）：');
    for (const w of bad.slice(0, 5)) {
      console.error(`  ${w.name}: missing ${w.missingResolutions} resolution row(s), e.g. ${w.missingExample.join(', ')}`);
    }
    process.exit(2);
  }
  const small = windows.filter((w) => w.rounds < minRounds);
  if (small.length) {
    console.error(`规划出的窗口有 ${small.length} 个不足 ${minRounds} 轮（#964 验收 1 要求 ≥200 轮/臂）—— 调小 --rounds-per-window 或 --windows。`);
    process.exit(2);
  }
  return windows;
}

async function sliceWindow(corpus, win, outPath) {
  // 断点续跑：切片是语料 + 行区间的确定函数。切片清单记录行区间，行区间
  // 一致且文件哈希对得上才复用 —— 换了 --rounds-per-window 重用旧目录也不会
  // 拿错切片（win-00 名字相同、内容不同的经典陷阱）。
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

// ── 包 staging（#302：每臂只带自己的包）────────────────────────────────────

function stagePackage(pkgDir, strategy, label) {
  if (!existsSync(pkgDir) || !existsSync(join(pkgDir, 'manifest.json'))) {
    console.error(`[${label}] 策略包未就绪：${pkgDir} 不存在或缺 manifest.json`);
    if (strategy === 'coin_flip_exit_lab') {
      console.error('B/C 臂依赖 #962 的 coin_flip_exit_lab 包 —— 未合并前本实验显式拒绝（非静默降级）。');
      console.error('#962 合并后重跑；或用 --b-pkg-dir <path> 显式指向一个已就绪的包目录（会被记录进 run manifest）。');
    }
    process.exit(2);
  }
  const stage = join(tmpdir(), `bk-964-${strategy}-${process.pid}-${Math.random().toString(36).slice(2, 8)}`);
  cpSync(pkgDir, join(stage, strategy), { recursive: true });
  const manifest = JSON.parse(readFileSync(join(stage, strategy, 'manifest.json'), 'utf8'));
  if (manifest.name !== strategy) {
    console.error(`[${label}] staged manifest name ${manifest.name} != ${strategy} —— staging 错包了`);
    process.exit(2);
  }
  return {
    strategy,
    stagedDir: stage,
    source: pkgDir,
    sourceOverridden: resolve(pkgDir) !== resolve(join(ROOT, 'user_layer', 'strategies_lua', strategy)),
    manifestSha256: sha256File(join(stage, strategy, 'manifest.json')),
    entrySha256: sha256File(join(stage, strategy, manifest.entry ?? 'strategy.lua')),
    version: manifest.version ?? null,
  };
}

// ── 回放命令（写实档 / identity 档）─────────────────────────────────────────

function buildArgs(arm, stagedDir, windowFile, reportPath, profile) {
  const p = PROFILES[profile];
  const cmd = [
    '--mode', 'dry',
    '--engine',
    '--no-discovery',
    '--no-strategy-state',
    '--no-strategy-dir',
    '--lua-strategy-dir', stagedDir,
    '--enable-strategy', arm.strategy,
    '--no-trade-log', '--no-order-log', '--no-position-log', '--no-intent-audit',
    '--no-event-archive',
    '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
    '--seed-balance', String(seedBalance),
    '--max-order-notional', String(maxOrderNotional),
    '--min-shares', String(minShares), '--max-shares', String(maxShares),
    '--max-positions', String(maxPositions),
    '--entry-maker-timeout-ms', arm.entryMakerTimeoutMs,
    '--backtest', windowFile,
    '--backtest-report', reportPath,
    '--backtest-tick-ms', p.tickMs,
    '--backtest-tail-ms', '0',
  ];
  if (p.feeModel) cmd.push('--fee-model', p.feeModel);
  if (p.slippageTicks) cmd.push('--slippage-ticks', p.slippageTicks);
  if (p.fast) cmd.push('--backtest-fast');
  if (arm.seed === 'seed') cmd.push('--backtest-knob', `${arm.strategy}:coin_seed=${seed}`);
  for (const [k, v] of arm.knobs ?? []) {
    cmd.push('--backtest-knob', `${arm.strategy}:${k}=${v}`);
  }
  return cmd;
}

// ── 跑一遍回放（#265 自检 + 断点续跑 + watchdog）────────────────────────────

function parseSelfCheck(stderr) {
  const m = stderr.match(/strategy startup self-check: requested=(\d+) resolved=(\d+) enabled=\[([^\]]*)\]/);
  if (!m) return null;
  return { requested: Number(m[1]), resolved: Number(m[2]), enabled: m[3].split(',').map((s) => s.trim()).filter(Boolean) };
}

function invalidReason(stderr, selfCheck, arm) {
  if (/unknown strategy requested/.test(stderr)) return 'kernel log: unknown strategy requested (包没加载)';
  if (/refusing to start: none of the \d+ explicitly requested/.test(stderr)) return 'kernel log: 内核拒绝启动（requested 策略未解析）';
  if (/counterfactual knobs refused/.test(stderr)) {
    const first = stderr.split('\n').find((l) => l.includes('--backtest-knob'));
    return `kernel log: --backtest-knob 被拒（undeclared/wrong-shape）: ${first ?? ''}`;
  }
  if (!selfCheck) return 'kernel log: 无 strategy startup self-check 行（二进制过旧？）';
  if (selfCheck.resolved < 1) return `kernel log: requested ${selfCheck.requested} resolved ${selfCheck.resolved}`;
  if (!selfCheck.enabled.includes(arm.strategy)) {
    return `kernel log: 启用集 ${selfCheck.enabled.join(', ')} 不含 ${arm.strategy}`;
  }
  return null;
}

function runBacktest(cmd, reportPath, label) {
  return new Promise((res) => {
    // 断点续跑带防陈旧：报告旁边存一份生成它的命令行，参数变过（换 seed、
    // 换窗口、换档位）就重跑 —— 缓存只对逐字节相同的命令有效。
    const cmdFile = reportPath.replace(/\.json$/, '.cmd.txt');
    if (existsSync(reportPath) && existsSync(cmdFile) && readFileSync(cmdFile, 'utf8') === cmd.join(' ') + '\n') {
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), cached: true, invalid: null });
        return;
      } catch { /* 残缺文件 —— 重跑 */ }
    }
    console.log(`    $ blitzkrieg-core ${cmd.join(' ')}   # ${label}`);
    let stderr = '';
    const child = spawn(CORE_BIN, cmd, { stdio: ['ignore', 'ignore', 'pipe'], cwd: OUT_DIR });
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    const watchdog = setTimeout(() => {
      console.error(`    TIMEOUT after ${runTimeoutMin} min — stopping the run group`);
      try { process.kill(-child.pid, 'SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
    }, runTimeoutMin * 60_000);
    child.on('exit', (code, signal) => {
      clearTimeout(watchdog);
      const sc = parseSelfCheck(stderr);
      const invalid = invalidReason(stderr, sc, { strategy: cmd[cmd.indexOf('--enable-strategy') + 1] });
      if (invalid) {
        console.error(`    INVALID (${label}): ${invalid}`);
        res({ report: null, cached: false, invalid });
        return;
      }
      if (code !== 0) {
        console.error(`    backtest exited code=${code} signal=${signal ?? child.signalCode ?? 'none'} (${label}); 拒绝从失败运行读报告`);
        console.error(stderr.split('\n').slice(-8).join('\n'));
        process.exit(1);
      }
      writeFileSync(cmdFile, cmd.join(' ') + '\n');
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), cached: false, invalid: null });
      } catch (e) {
        console.error(`    cannot parse report ${reportPath}: ${e.message}`);
        process.exit(1);
      }
    });
  });
}

// ── 指标抽取（报告 × 窗口映射）──────────────────────────────────────────────

const SETTLED_REASONS = new Set(['Settlement']);

/**
 * 双跑确定性判定（#1004 加非空前置）。
 *
 * 旧逻辑只比整份 report 的 sha256：0 tradeLines 的臂两份空报告必然逐字节
 * 相等（「空==空」），于是被记成 match 并充当确定性 PASS —— 这是外推造假：
 * 空跑不构成引擎确定性的证据。本判定加前置：双跑同 hash 且 tradeLines 非空
 * 才记 'match'；同 hash 但为空记 'vacuous-empty-runs'（= UNJUDGEABLE_EMPTY_RUN，
 * 未判定而非通过）；不同 hash 恒记 'mismatch'（空报告也不会 hash 相撞）。
 * 'skipped'（--single-run）与 'missing'（断点续跑缺第二份报告）语义不变。
 *
 * @param {string} h1 run1 报告文件 sha256
 * @param {string} h2 run2 报告文件 sha256
 * @param {number} tradeLineCount run1 报告的 tradeLines 条数
 * @returns {'match'|'vacuous-empty-runs'|'mismatch'|'skipped'|'missing'}
 */
function determinismVerdict(h1, h2, tradeLineCount) {
  if (h1 !== h2) return 'mismatch';
  return tradeLineCount > 0 ? 'match' : 'vacuous-empty-runs';
}

function extractMetrics(rep, win, profile) {
  const t = rep.trades ?? {};
  const closed = num(t.closed) ?? 0;
  const wins = num(t.wins) ?? 0, losses = num(t.losses) ?? 0;
  const gp = num(t.grossProfitUsd) ?? 0, gl = num(t.grossLossUsd) ?? 0;
  const payoff = wins > 0 && losses > 0 ? (gp / wins) / (gl / losses) : null;
  const lines = Array.isArray(rep.tradeLines) ? rep.tradeLines : [];
  const strategyNames = [...new Set(lines.map((l) => l.strategy))];
  const statsNames = (rep.strategies ?? []).map((s) => s.name).filter(Boolean);
  // 入场 starvation 证据（#964 归因分支 3「市场结构」的实measure）：
  // 锁价语料 × 纯 maker 入场声明（entry_maker=true，宿主强制 never-escalate）
  // → WouldCross 在提交瞬间 100% 拒单 → 5 连拒触发账户冻结 → 整窗无法入场。
  const ownStats = (rep.strategies ?? []).find((s) => s.name != null) ?? {};
  const feed = rep.feed ?? {};
  const reasons = {};
  let settled = 0, holdFloorSum = 0, holdUpperSum = 0;
  const lossMags = [];
  let worst = null;
  for (const l of lines) {
    const pnl = num(l.netPnlUsd);
    const reason = String(l.reason ?? '');
    reasons[reason] = (reasons[reason] ?? 0) + 1;
    const slot = win.condSlot[l.conditionId];
    const dur = (slot != null ? win.slotDuration[slot] : null) ?? 900;
    holdUpperSum += dur;
    if (SETTLED_REASONS.has(reason)) { settled++; holdFloorSum += dur; }
    if (pnl != null && pnl < 0) lossMags.push(-pnl);
    if (pnl != null && (worst == null || pnl < worst)) worst = pnl;
  }
  lossMags.sort((a, b) => a - b);
  return {
    profile,
    window: win.name,
    windowRounds: win.rounds,
    closed,
    wins, losses, flat: num(t.flat) ?? 0,
    winRatePct: num(t.winRatePct),
    payoff,
    profitFactor: num(t.profitFactor),
    netPnlUsd: num(t.netPnlUsd),
    avgPnlUsd: num(t.avgPnlUsd),
    expectancyUsd: closed > 0 ? (num(t.netPnlUsd) ?? 0) / closed : null,
    grossProfitUsd: gp, grossLossUsd: gl,
    maxDrawdownUsd: num(t.maxDrawdownUsd),
    maxDrawdownPct: num(t.maxDrawdownPct),
    feesUsd: num(t.feesUsd),
    worstTradeUsd: worst,
    tailLoss: {
      n: lossMags.length,
      p95: percentile(lossMags, 95),
      p99: percentile(lossMags, 99),
    },
    settlementRatePct: closed > 0 ? (settled / closed) * 100 : null,
    avgHoldSecFloor: closed > 0 ? holdFloorSum / closed : null,
    avgHoldSecUpper: closed > 0 ? holdUpperSum / closed : null,
    reasonHistogram: reasons,
    tradesSha256: rep.tradesSha256 ?? null,
    orders: rep.orders ?? null,
    fills: num(rep.fills),
    openPositions: num(rep.openPositions),
    openNotionalUsd: num(rep.openNotionalUsd),
    feeSchedule: rep.feeSchedule ?? null,
    slippageTicks: rep.fillModel?.takerSlippageTicks ?? null,
    events: num(rep.sourceStats?.events),
    strategyNames,
    statsNames,
    entry: {
      ordersPlaced: num(ownStats.ordersPlaced),
      ordersRejected: num(ownStats.ordersRejected),
      rejectionCauses: ownStats.rejectionCauses ?? null,
      accountFreezes: feed.accountFreezes ?? null,
      venueRejected: num(feed.venueRejected),
    },
  };
}

function mergeTradeLines(rep, arm, win, profile, tradesDir) {
  const lines = Array.isArray(rep.tradeLines) ? rep.tradeLines : [];
  const dir = join(OUT_DIR, tradesDir, arm);
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `${win.name}.trades.jsonl`);
  const body = lines.map((l) => JSON.stringify({ profile, window: win.name, arm, ...l })).join('\n');
  writeFileSync(path, body ? body + '\n' : '');
  return { path, count: lines.length };
}

// ── 汇总 + 对照判决（结论模板的骨架，数字只从产物里来）──────────────────────

const VERDICT_METRICS = [
  ['winRatePct', '胜率 %', 'higher'],
  ['payoff', '盈亏比', 'higher'],
  ['expectancyUsd', '期望 USD/笔', 'higher'],
  ['netPnlUsd', '净 PnL USD', 'higher'],
  ['maxDrawdownUsd', '最大回撤 USD', 'lower'],
  ['worstTradeUsd', '单笔最大亏损 USD', 'higher'], // 更接近 0 = 更好
  ['tailLoss.p99', '尾亏 P99 USD', 'higher'],
  ['feesUsd', '费用 USD', 'lower'],
  ['settlementRatePct', '到期结算率 %', 'info'],
  ['avgHoldSecFloor', '持仓下界 s', 'info'],
  ['closed', '平仓笔数', 'info'],
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

function aggregate(arm, perWindow) {
  // 汇总 = 全窗口的并集：计数相加、率重算、尾亏对合并后的亏损分布重算。
  const all = perWindow.filter(Boolean);
  const closed = all.reduce((s, m) => s + (m.closed ?? 0), 0);
  const wins = all.reduce((s, m) => s + (m.wins ?? 0), 0);
  const losses = all.reduce((s, m) => s + (m.losses ?? 0), 0);
  const gp = all.reduce((s, m) => s + (m.grossProfitUsd ?? 0), 0);
  const gl = all.reduce((s, m) => s + (m.grossLossUsd ?? 0), 0);
  const net = all.reduce((s, m) => s + (m.netPnlUsd ?? 0), 0);
  const fees = all.reduce((s, m) => s + (m.feesUsd ?? 0), 0);
  const settled = all.reduce((s, m) => s + Math.round(((m.settlementRatePct ?? 0) / 100) * (m.closed ?? 0)), 0);
  const holdFloor = all.reduce((s, m) => s + (m.avgHoldSecFloor ?? 0) * (m.closed ?? 0), 0);
  const holdUpper = all.reduce((s, m) => s + (m.avgHoldSecUpper ?? 0) * (m.closed ?? 0), 0);
  const lossMags = [];
  let worst = null;
  const reasons = {};
  const rejectionCauses = {};
  let ordersPlaced = 0, ordersRejected = 0;
  const freezes = {};
  for (const m of all) {
    for (const [k, v] of Object.entries(m.reasonHistogram ?? {})) reasons[k] = (reasons[k] ?? 0) + v;
    for (const [k, v] of Object.entries(m.entry?.rejectionCauses ?? {})) rejectionCauses[k] = (rejectionCauses[k] ?? 0) + v;
    for (const [k, v] of Object.entries(m.entry?.accountFreezes ?? {})) freezes[k] = v;
    ordersPlaced += m.entry?.ordersPlaced ?? 0;
    ordersRejected += m.entry?.ordersRejected ?? 0;
    const arr = (m._lossMags ?? []);
    lossMags.push(...arr);
    if (m.worstTradeUsd != null && (worst == null || m.worstTradeUsd < worst)) worst = m.worstTradeUsd;
  }
  lossMags.sort((a, b) => a - b);
  return {
    arm,
    windowsRun: all.length,
    rounds: all.reduce((s, m) => s + (m.windowRounds ?? 0), 0),
    closed, wins, losses,
    winRatePct: closed > 0 ? (wins / closed) * 100 : null,
    payoff: wins > 0 && losses > 0 ? (gp / wins) / (gl / losses) : null,
    profitFactor: gl > 0 ? gp / gl : null,
    netPnlUsd: net,
    expectancyUsd: closed > 0 ? net / closed : null,
    grossProfitUsd: gp, grossLossUsd: gl,
    maxDrawdownUsd: Math.max(0, ...all.map((m) => m.maxDrawdownUsd ?? 0)),
    worstTradeUsd: worst,
    tailLoss: { n: lossMags.length, p95: percentile(lossMags, 95), p99: percentile(lossMags, 99) },
    feesUsd: fees,
    settlementRatePct: closed > 0 ? (settled / closed) * 100 : null,
    avgHoldSecFloor: closed > 0 ? holdFloor / closed : null,
    avgHoldSecUpper: closed > 0 ? holdUpper / closed : null,
    reasonHistogram: reasons,
    entry: { ordersPlaced, ordersRejected, rejectionCauses, accountFreezes: freezes },
    // 跨窗确定性合成（#1004 语义）：vacuous-empty-runs 是独立状态，不并入
    // match（空跑不是证据），也不并入 mismatch（没有破裂证据）。vacuous +
    // match 混合 = 'vacuous-mixed' —— 只有非空窗口的 match 算数，空窗不算。
    determinism: all.some((m) => m.determinism === 'mismatch') ? 'mismatch'
      : all.some((m) => m.determinism === 'missing') ? 'missing'
        : all.every((m) => m.determinism === 'match') ? 'match'
          : all.every((m) => m.determinism === 'vacuous-empty-runs') ? 'vacuous-empty-runs'
            : all.some((m) => m.determinism === 'vacuous-empty-runs') ? 'vacuous-mixed'
              : all.every((m) => m.determinism === 'skipped') ? 'skipped' : 'mixed',
    perWindow: all.map(({ _lossMags, ...rest }) => rest),
  };
}

function verdictOf(aggs, coverage, minBetter) {
  // gates 先行：任何一条不过，实验整体判无效 —— 结论区只做归因，不做遮掩。
  // #1004：确定性门禁区分「通过」与「证据」——match 是证据；skipped 是声明式
  // 跳过；vacuous-empty-runs / vacuous-mixed 是 UNJUDGEABLE（0 tradeLines 双跑
  // 「空==空」）不是 PASS。带 vacuous 臂的实验判不可用：该臂的确定性主张
  // 没有证据。mismatch/missing 照旧直接红。
  const DETERMINISM_PASS = new Set(['match', 'skipped']);
  const gates = {
    coverage: coverage.ok,
    coverageDetail: coverage.detail,
    determinism: Object.fromEntries(Object.entries(aggs).map(([k, v]) => [k, v.determinism])),
    determinismVacuous: Object.fromEntries(Object.entries(aggs)
      .filter(([, v]) => typeof v.determinism === 'string' && v.determinism.startsWith('vacuous'))
      .map(([k, v]) => [k, v.determinism])),
    contamination: Object.fromEntries(Object.entries(aggs).map(([k, v]) => [k, v.contaminated ?? false])),
  };
  const usable = gates.coverage
    && Object.values(gates.determinism).every((d) => DETERMINISM_PASS.has(d))
    && Object.values(gates.contamination).every((c) => !c);
  // 硬红 = 与「饿死臂必然 vacuous」无关的门禁破裂：覆盖不足、归因污染、
  // 确定性 mismatch/missing。这些无论臂饿死与否都直接判 invalid。
  const hardFail = !gates.coverage
    || Object.values(gates.contamination).some(Boolean)
    || Object.values(gates.determinism).some((d) => d === 'mismatch' || d === 'missing');

  // 行方向约定：compareRows(base, cand) → a=base 列, b=cand 列, delta=cand−base,
  // verdict 'better' = cand 优于 base。三张表都让 cand=更"动态"的那一臂。
  // 一侧 closed==0（臂饿死）时全部判定改 n/a —— 没上场的对手不产生
  // "worse/better"证据，防下游摘引假判定。
  const cmp = (base, cand) => {
    const rows = compareRows(aggs[base], aggs[cand]);
    if (((aggs[base]?.closed ?? 0) === 0) || ((aggs[cand]?.closed ?? 0) === 0)) {
      for (const r of rows) if (r.verdict !== 'info') r.verdict = 'n/a';
    }
    return rows;
  };
  const both = (x, y) => (aggs[x] != null && aggs[y] != null);
  const bVsA = both('A', 'B') ? cmp('A', 'B') : []; // Δ = B − A
  const bVsC = both('C', 'B') ? cmp('C', 'B') : []; // Δ = B − C
  const cVsA = both('A', 'C') ? cmp('A', 'C') : []; // Δ = C − A

  const countBetter = (rows) => rows.filter((r) => r.verdict === 'better').length;
  const expectBetter = (rows, key) => {
    const r = rows.find((x) => x.key === key);
    return r ? r.verdict : 'n/a';
  };
  const bBetterOn = countBetter(bVsA);
  // 数据充分性：参比臂（A、B）有一臂全程 0 平仓 = 对照不成立 —— 判
  // arm-starved，绝不把「对手没上场」读成「B 不优」或「B 更优」。归因
  // 证据（拒单原因、账户冻结）直接给在 gates.starvedDetail。
  const starvedDetail = {};
  for (const k of ['A', 'B']) {
    if (aggs[k] && (aggs[k].closed ?? 0) === 0 && aggs[k].windowsRun > 0) {
      starvedDetail[k] = {
        closed: 0,
        ordersPlaced: aggs[k].entry?.ordersPlaced ?? 0,
        ordersRejected: aggs[k].entry?.ordersRejected ?? 0,
        rejectionCauses: aggs[k].entry?.rejectionCauses ?? {},
        accountFreezes: aggs[k].entry?.accountFreezes ?? {},
      };
    }
  }
  const starved = Object.keys(starvedDetail).length > 0;
  // #1004 结论序：arm-starved 优先于 invalid。饿死臂（closed==0）的双跑必然
  // vacuous —— 那正是「臂没上场」的表现，不该把结构归因判决（issue 验收：
  // arm-starved 路径保持原样）降级成 invalid；vacuous 记录在
  // gates.determinismVacuous 里随报告显式可读。只有存在与饿死无关的硬红
  // （覆盖不足 / 污染 / mismatch / missing）才判 invalid —— 硬红优先于
  // starved，绝不给带真门禁破裂的跑批出结构归因。usable 仍按门禁原样：
  // vacuous 臂不充当确定性证据，带 vacuous 的跑批 usable=false。
  const vacuousArms = Object.keys(gates.determinismVacuous);
  const conclusion = !aggs.B || !aggs.A ? 'pending'
    : hardFail ? 'invalid'
      : starved ? 'arm-starved'
        : !usable ? 'invalid'
          : expectBetter(bVsA, 'expectancyUsd') === 'better' && expectBetter(bVsA, 'payoff') === 'better' ? 'B-better-on-both-target-metrics'
            : bBetterOn >= minBetter ? 'B-better-on-enough-metrics'
              : 'B-not-better';
  return { usable, gates, bVsA, bVsC, cVsA, bBetterOn, minBetter, conclusion, starved, starvedDetail, vacuousArms };
}

// ── 报告渲染 ────────────────────────────────────────────────────────────────

/**
 * arm-starved 段的结构归因文本（#1004：所有「实测」数字必须能追到本次语料，
 * 算不出来就印「未测」，禁止写死字面量）。
 *
 * 数字来源 = 本次 run manifest 的 corpusTopStats（scanCorpus 流式实算的
 * k=top 行统计）：locked/quotes 两个数与锁价占比都从这里派生；字段缺失
 * （旧 manifest --report-only 复渲染）时整段印「未测」，绝不回退到写死值。
 */
function armStarvedAttribution(manifest) {
  const ts = manifest.corpusTopStats;
  let measured = 'top 报价锁价占比未测（本次 run manifest 无 corpusTopStats —— 旧 manifest 复渲染不伪造实测值）';
  if (ts && Number.isFinite(ts.quotes) && Number.isFinite(ts.locked) && ts.quotes > 0) {
    const unparsedNote = ts.unparsed > 0 ? `，另有 ${fmtInt(ts.unparsed)} 行 top 未解析、不计入分母` : '';
    measured = `top 报价锁价 ${fmtInt(ts.locked)}/${fmtInt(ts.quotes)}（${((ts.locked / ts.quotes) * 100).toFixed(2)}%${unparsedNote}，ba==bb，单边成交价派生，本次语料实算）`;
  }
  return `结构性归因（issue 归因分支 3「市场结构」）：本语料（data/onchain 15m-act）的 ${measured}。A 臂（lua_momentum）的入场由 manifest \`entry_maker: true\` 声明为纯 maker（宿主强制 never-escalate，lua_loader.rs），提交瞬间即穿越（buy@best_bid == best_ask）→ WouldCross 拒单 → 5 连拒触发内核账户冻结。这是包学说 × 语料结构的交互，不是旋钮可修的：被饿死是「错失机会，从非损失」（包注释原话）在锁价簿上的必然结果。**真数据 A/B 需审计裁决坐标**：(a) 接受主坐标下的结构性结论；或 (b) A 臂换档案书语料（data/archive frozen-corpus 惯例，真 L2 有档差，962 同款）。本脚本不改策略实现、不改引擎（#964 禁止触碰项）。`;
}

function renderMd(manifest, windows, aggs, verdict, tradesIndex) {
  const md = [];
  md.push(`# stone A/B 出场对照 — issue #964（${manifest.profile} 档）`);
  md.push('');
  md.push(`生成：${utc(manifest.generatedAtMs)}；脚本：\`scripts/ab-exit-964.mjs\`；run 目录：\`${manifest.outDir}\``);
  md.push('');
  md.push('## 预声明坐标（跑之前钉死，不挑窗口）');
  md.push('');
  md.push(`- 语料：\`${manifest.corpus.corpus}\`（sha256 \`${manifest.corpus.sha256.slice(0, 16)}…\`，market=${manifest.corpus.manifest.market}，${manifest.corpus.manifest.startMs ? utc(manifest.corpus.manifest.startMs) : '—'} → ${manifest.corpus.manifest.endMs ? utc(manifest.corpus.manifest.endMs) : '—'}，资产 ${manifest.assetsAll.join('/')}`);
  md.push(`- 语料门禁：manifest sha256 一致；fix_resolutions --check ${manifest.corpus.fixResolutions.ok} ok / ${manifest.corpus.fixResolutions.bad} bad（#890 门禁）。`);
  md.push(`- 窗口：${windows.length} 窗 × ≥${manifest.minRounds} 轮，轮数等分连续切片；每窗校验 resolution 行完整性。全窗口清单见附录。`);
  md.push(`- 资金与费率：seed-balance ${manifest.seedBalance} USD；min/max shares ${manifest.minShares}/${manifest.maxShares}（top-only 语料名义深度 1 手 → sizing 固定 1，两臂同值）；max-order-notional ${manifest.maxOrderNotional}；max-positions ${manifest.maxPositions}；fee-model=${manifest.profile === 'realistic' ? 'official' : '(引擎默认 legacy)'}；slippage=${manifest.profile === 'realistic' ? '1 tick' : '0 (identity)'}；tick-ms=${manifest.tickMs}${manifest.profile === 'realistic' ? '；--backtest-fast' : ''}。`);
  md.push(`- seed：B/C 臂 coin_seed=${manifest.seed}（--backtest-knob 显式下发）；A 臂无 seed（确定性引擎）。`);
  md.push(`- 确定性：${manifest.singleRun ? 'single-run（--single-run，验收 1 的双跑校验未执行）' : '每臂×每窗双跑，报告 sha256 必须一致且 tradeLines 非空（0 tradeLines 双跑 = vacuous，不计证据，#1004）'}。`);
  md.push('');
  md.push('### 平均持仓时长 —— 预先声明的近似口径');
  md.push('');
  md.push('backtest 报告 tradeLines 不带逐笔时间戳（引擎 schema 限制）。持仓时长用 round_slot 近似：每笔按 conditionId 映射回语料轮次（持仓只可能存在于该轮窗内，15m=900s），报 settlementRatePct（精确）+ avgHoldSecFloor（结算记满轮/提前出场记 0，下界）+ avgHoldSecUpper（全记满轮，上界）。真实平均持仓 ∈ [floor, upper]；跨臂比较持仓结构以 settlementRatePct 与 reason 直方图为准。逐笔秒级时长需引擎侧加字段，另开 issue（#964 禁改引擎）。');
  md.push('');
  md.push('## 臂定义');
  md.push('');
  md.push('| 臂 | 包 | 入场 | 出场 | seed |');
  md.push('|---|---|---|---|---|');
  md.push('| A | `lua_momentum` | 动量信号 + 纯 maker（never-escalate） | 原默认（manifest 出厂 tunables） | — |');
  md.push(`| B | \`coin_flip_exit_lab\` | 抛硬币（coin_seed=${manifest.seed}）+ taker 升级 1ms | 动态栈默认（硬止损/加权止盈带/移动止盈/截止线/thesis_death） | ${manifest.seed} |`);
  md.push(`| C（对照） | \`coin_flip_exit_lab\` | 同 B 同 seed | 出场栈 --backtest-knob 全清零，只剩 exit_time_left_sec | ${manifest.seed} |`);
  md.push('');
  md.push('C 臂清零口径（#962 声明）：hard 0 / tp 0(+tight) / trail 0(+tight) / death reversal=100,tight=0 / stale=0。stdlib「盘口消失判死」不受 stale=0 影响（那是超时阈值），两随机臂同享该 runtime 路径。');
  md.push('');
  md.push('## 门禁');
  md.push('');
  md.push(`- 覆盖：${verdict.gates.coverage ? 'PASS' : 'FAIL'} — ${verdict.gates.coverageDetail}`);
  for (const [arm, d] of Object.entries(verdict.gates.determinism)) {
    md.push(`- 确定性 ${arm}：${d === 'match' ? 'PASS（双跑逐字节一致）'
      : d === 'skipped' ? 'SKIPPED（--single-run）'
        : d === 'vacuous-empty-runs' ? '**UNJUDGEABLE_EMPTY_RUN（双跑同 hash 但 0 tradeLines：空==空，不构成确定性证据）**'
          : d === 'vacuous-mixed' ? '**UNJUDGEABLE_EMPTY_RUN（部分窗口 0 tradeLines：vacuous 窗不计证据，非空窗 match 不救整臂）**'
            : `**FAIL（${d}）**`}`);
  }
  const vacuousGates = Object.entries(verdict.gates.determinismVacuous ?? {});
  if (vacuousGates.length) {
    md.push(`- 确定性 vacuous 臂（#1004）：${vacuousGates.map(([a, d]) => `${a}=${d}`).join('，')} —— vacuous 臂不计入确定性证据，整跑 usable=false。`);
  }
  for (const [arm, c] of Object.entries(verdict.gates.contamination)) {
    md.push(`- 归因污染 ${arm}：${c ? '**FAIL（报告里出现了别的策略名）**' : 'PASS'}`);
  }
  md.push('');
  if (!verdict.usable) {
    md.push('> **本报告判无效** —— 上面至少一条门禁没过。结论区只做归因，不给出「B 更优/不优」的可执行判决。');
    md.push('');
  }
  md.push('## B vs A（issue 原题：好入场+呆板出场 vs 随机入场+动态出场）');
  md.push('');
  md.push('| 指标 | A (lua_momentum) | B (coin_flip_exit_lab) | Δ(B−A) | 判定 |');
  md.push('|---|---|---|---|---|');
  for (const r of verdict.bVsA) {
    md.push(`| ${r.label} | ${r4(r.a)} | ${r4(r.b)} | ${r.delta == null ? '—' : r4(r.delta)} | ${r.verdict} |`);
  }
  md.push('');
  if (verdict.cVsA) {
    md.push('## C vs A（对照臂参考）');
    md.push('');
    md.push('| 指标 | A | C (清零对照) | Δ(C−A) | 判定 |');
    md.push('|---|---|---|---|---|');
    for (const r of verdict.cVsA) {
      md.push(`| ${r.label} | ${r4(r.a)} | ${r4(r.b)} | ${r.delta == null ? '—' : r4(r.delta)} | ${r.verdict} |`);
    }
    md.push('');
  }
  md.push('## B vs C（出场栈的孤立贡献 = B − C，同 seed 同窗口同资金）');
  md.push('');
  md.push('| 指标 | B (动态出场) | C (清零对照) | Δ(B−C) | 判定 |');
  md.push('|---|---|---|---|---|');
  for (const r of verdict.bVsC) {
    md.push(`| ${r.label} | ${r4(r.b)} | ${r4(r.a)} | ${r.delta == null ? '—' : r4(r.delta)} | ${r.verdict} |`);
  }
  md.push('');
  md.push('## 结论（模板，判定由脚本按上表给出）');
  md.push('');
  if (verdict.conclusion === 'B-better-on-both-target-metrics') {
    md.push('**B 在期望与盈亏比两项目标指标上同时优于 A** —— 在本语料/窗口/资金坐标下，出场机制的质变作用成立。孤立贡献看 B−C：动态栈相对清零对照的期望差即出场机制本身的期望贡献。');
  } else if (verdict.conclusion === 'B-better-on-enough-metrics') {
    md.push(`**B 在 ${verdict.bBetterOn} 项指标上优于 A（≥ ${verdict.minBetter} 的采用线），但期望与盈亏比未同时占优** —— 出场机制有可测改善，target metrics 未同时翻转；先看 B−C 的期望差是否为正，再走归因清单。`);
  } else if (verdict.conclusion === 'B-not-better') {
    md.push('**B 未在足够指标上优于 A** —— 按预声明归因清单走（见下），禁止直接改成「出场无用」。');
  } else if (verdict.conclusion === 'arm-starved') {
    for (const [arm, d] of Object.entries(verdict.starvedDetail ?? {})) {
      md.push(`**${arm} 臂全程 0 平仓（对照不成立）**：下单 ${d.ordersPlaced}，被拒 ${d.ordersRejected}，原因 ${JSON.stringify(d.rejectionCauses)}；账户冻结 ${JSON.stringify(d.accountFreezes)}。`);
    }
    md.push('');
    md.push(armStarvedAttribution(manifest));
  } else if (verdict.conclusion === 'invalid') {
    md.push('**门禁未过，本报告不可作为证据。**');
  } else {
    md.push('**pending** —— B 臂数据缺失（包未就绪或未跑）。');
  }
  md.push('');
  md.push('### 归因清单（B 不优时，按序排查，禁止含糊归因）');
  md.push('');
  md.push('1. **出场参数**：B 的出场栈是 #962 网格的默认点，不是调出来的最优。若 B−C ≤ 0（栈没挣到钱），跑 #962 的 16 点预声明网格（`scripts/coin-flip-lab-replay.mjs`）——栈内某参数（如 hard_stop 20 在 0.5-0.6 价带的过紧问题）可能吃掉正期望。');
  md.push('2. **样本**：看附录逐窗表——若 B 优于 A 的窗口集中在少数几窗（其余窗反向），样本不足以支持总判决；扩窗重跑（--rounds-per-window 或 --windows）。');
  md.push('3. **市场结构**：写实档（official 费 + 1 tick 滑点）与 identity 档（附录上界）结论若翻转，说明判决被摩擦成本主导而非出场机制；再对照语料资产结构（本语料 6 资产混合，B 臂每轮至多一仓）确认不是某单一资产拖尾。');
  md.push('');
  md.push('## 可追溯性');
  md.push('');
  md.push('| 产物 | 路径 |');
  md.push('|---|---|');
  md.push(`| run manifest（本报告的机器版） | \`${manifest.outDir}/ab-exit-964.json\` |`);
  for (const t of tradesIndex ?? []) md.push(`| tradeLines 导出：${t.arm} × ${t.profile} | \`${t.dir}/\`（逐窗 .trades.jsonl） |`);
  md.push(`| 窗口切片（复跑输入） | \`${manifest.outDir}/windows/\` |`);
  md.push(`| 逐窗双份回放报告 | \`${manifest.outDir}/runs/<profile>/<arm>/<win>/{report-1,report-2}.json\` |`);
  md.push('');
  md.push('每个数字 = run manifest 里对应字段的直出或四则组合；重新渲染：`node scripts/ab-exit-964.mjs --report-only <runDir>`。');
  md.push('');
  md.push('## 附录：全部窗口（#964 验收 3 —— 不挑窗口）');
  md.push('');
  for (const arm of Object.keys(aggs)) {
    md.push(`### ${arm} 臂 逐窗指标（${manifest.profile} 档）`);
    md.push('');
    md.push('| 窗口 | 轮数 | 窗口起（UTC） | closed | 胜率% | 盈亏比 | 期望 | 净PnL | 最大回撤 | 单笔最大亏损 | 尾亏P95 | 尾亏P99 | 结算率% | 下单/被拒 | tradesSha256(前8) |');
    md.push('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|');
    for (const w of aggs[arm].perWindow) {
      md.push(`| ${w.window} | ${w.windowRounds} | ${utc(w._firstAtMs)} | ${r4(w.closed)} | ${r4(w.winRatePct)} | ${r4(w.payoff)} | ${r4(w.expectancyUsd)} | ${r4(w.netPnlUsd)} | ${r4(w.maxDrawdownUsd)} | ${r4(w.worstTradeUsd)} | ${r4(w.tailLoss?.p95)} | ${r4(w.tailLoss?.p99)} | ${r4(w.settlementRatePct)} | ${w.entry?.ordersPlaced ?? '—'}/${w.entry?.ordersRejected ?? '—'} | ${String(w.tradesSha256 ?? '').slice(0, 8)} |`);
    }
    md.push('');
  }
  for (const w of windows) {
    md.push(`- ${w.name}: 行 [${w.startLine}..${w.endLine}]（${w.lines} 行），${w.rounds} 轮（slot ${w.firstSlot}→${w.lastSlot}），${utc(w.firstAtMs)} → ${utc(w.expiryAtMs)}，资产 ${w.assets.join('/')}，切片 sha256 \`${w.sha256?.slice(0, 16)}…\``);
  }
  md.push('');
  return md.join('\n') + '\n';
}

// ── 框架自测（离线：合成夹具，不碰内核/语料/网络）───────────────────────────

function selfTest() {
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

  // 1) 分位数：numpy 线性插值口径
  ok &= P('percentile/线性插值', () => {
    eq(percentile([1, 2, 3, 4], 50), 2.5);
    eq(percentile([1, 2, 3, 4], 0), 1);
    eq(percentile([1, 2, 3, 4], 100), 4);
    eq(percentile([10, 20], 95), 19.5);
    eq(percentile([], 95), null);
  });

  // 2) 指标抽取：合成报告 + 合成窗口映射
  const win = {
    name: 'win-00', rounds: 3, condSlot: { c1: 1, c2: 1, c3: 2 }, slotDuration: { 1: 900, 2: 900 },
  };
  const rep = {
    trades: { closed: 4, wins: 2, losses: 2, flat: 0, grossProfitUsd: 3, grossLossUsd: 1, netPnlUsd: 2, winRatePct: 50, maxDrawdownUsd: 0.8, feesUsd: 0.4 },
    tradeLines: [
      { conditionId: 'c1', reason: 'Settlement', strategy: 's', netPnlUsd: 1 },
      { conditionId: 'c2', reason: 'TakeProfit', strategy: 's', netPnlUsd: 2 },
      { conditionId: 'c3', reason: 'StopLoss', strategy: 's', netPnlUsd: -0.5 },
      { conditionId: 'c3', reason: 'Settlement', strategy: 's', netPnlUsd: -0.5 },
    ],
    tradesSha256: 'abc',
    strategies: [{ name: 's' }],
    feeSchedule: { name: 'official' },
    fillModel: { takerSlippageTicks: 1 },
  };
  ok &= P('extractMetrics/胜率盈亏比期望', () => {
    const m = extractMetrics(rep, win, 'realistic');
    eq(m.winRatePct, 50);
    eq(m.payoff, (3 / 2) / (1 / 2));
    eq(m.expectancyUsd, 0.5);
    eq(m.worstTradeUsd, -0.5);
  });
  ok &= P('extractMetrics/尾亏P95P99', () => {
    const m = extractMetrics(rep, win, 'realistic');
    eq(m.tailLoss.n, 2);
    eq(m.tailLoss.p95, 0.5);
    eq(m.tailLoss.p99, 0.5);
  });
  ok &= P('extractMetrics/持仓近似口径', () => {
    const m = extractMetrics(rep, win, 'realistic');
    // 结算 2 笔（Settlement）记满轮 900，其余记 0；上限 = 全部 900
    eq(m.settlementRatePct, 50);
    eq(m.avgHoldSecFloor, 450);
    eq(m.avgHoldSecUpper, 900);
  });
  ok &= P('extractMetrics/出场原因直方图', () => {
    const m = extractMetrics(rep, win, 'realistic');
    eq(m.reasonHistogram, { Settlement: 2, TakeProfit: 1, StopLoss: 1 });
  });

  // 3) 汇总：多窗合并重算
  ok &= P('aggregate/跨窗合并', () => {
    const m1 = extractMetrics(rep, win, 'realistic');
    const rep2 = JSON.parse(JSON.stringify(rep));
    rep2.trades.closed = 2; rep2.trades.wins = 1; rep2.trades.losses = 1;
    rep2.tradeLines = rep.tradeLines.slice(0, 2);
    rep2.trades.netPnlUsd = 3; rep2.trades.grossProfitUsd = 3; rep2.trades.grossLossUsd = 0;
    const m2 = extractMetrics(rep2, win, 'realistic');
    m1._lossMags = [0.5, 0.5]; m2._lossMags = [];
    m1.determinism = m2.determinism = 'match';
    const agg = aggregate('B', [m1, m2]);
    eq(agg.closed, 6);
    eq(agg.wins, 3);
    eq(agg.expectancyUsd, 5 / 6);
    eq(agg.worstTradeUsd, -0.5);
    eq(agg.determinism, 'match');
  });

  // 4) 对照判决行：B 全面占优 / 全面落败 / 参比臂饿死 三个方向
  ok &= P('verdict/判决行方向', () => {
    const mk = (over) => ({
      winRatePct: 50, payoff: 1, expectancyUsd: 0.1, netPnlUsd: 10, maxDrawdownUsd: 5,
      worstTradeUsd: -2, tailLoss: { p99: 2 }, feesUsd: 1, settlementRatePct: 50,
      avgHoldSecFloor: 100, closed: 100, windowsRun: 3, determinism: 'match', contaminated: false,
      entry: { ordersPlaced: 10, ordersRejected: 0, rejectionCauses: {}, accountFreezes: {} },
      ...over,
    });
    const aggsWin = { A: mk({}), B: mk({ winRatePct: 55, payoff: 1.2, expectancyUsd: 0.2, netPnlUsd: 20, maxDrawdownUsd: 4, worstTradeUsd: -1.5, tailLoss: { p99: 1.5 }, feesUsd: 0.9 }), C: mk({}) };
    const v = verdictOf(aggsWin, { ok: true, detail: 't' }, 2);
    eq(v.conclusion, 'B-better-on-both-target-metrics');
    const aggsLose = { A: mk({}), B: mk({ winRatePct: 45, payoff: 0.8, expectancyUsd: 0.05, netPnlUsd: 5, maxDrawdownUsd: 6, worstTradeUsd: -3, tailLoss: { p99: 3 }, feesUsd: 1.2 }), C: mk({}) };
    const v2 = verdictOf(aggsLose, { ok: true, detail: 't' }, 2);
    eq(v2.conclusion, 'B-not-better');
    const aggsBad = { A: mk({}), B: mk({ determinism: 'mismatch' }), C: mk({}) };
    const v3 = verdictOf(aggsBad, { ok: true, detail: 't' }, 2);
    eq(v3.usable, false);
    eq(v3.conclusion, 'invalid');
    // A 臂饿死（closed=0 且有拒单证据）→ arm-starved，绝不判 B-not-better
    const aggsStarved = {
      A: mk({ closed: 0, entry: { ordersPlaced: 115, ordersRejected: 115, rejectionCauses: { 'venue.WouldCross': 115 }, accountFreezes: { default: '5 consecutive venue rejections' } } }),
      B: mk({ winRatePct: 55, payoff: 1.2, expectancyUsd: 0.2 }),
      C: mk({}),
    };
    const v4 = verdictOf(aggsStarved, { ok: true, detail: 't' }, 2);
    eq(v4.conclusion, 'arm-starved');
    eq(v4.starvedDetail.A.rejectionCauses['venue.WouldCross'], 115);
  });

  // 4b) 空跑确定性（#1004）：0 tradeLines 双跑 = vacuous，不是 PASS。
  // 饿死臂的确定性必然 vacuous —— 它不构成引擎确定性证据；vacuous 臂
  // 把 usable 拉成 false，但 arm-starved 声明路径（结构化判决 + 拒单
  // 证据）保持原样，不降级成 invalid。
  ok &= P('verdict/空tradeLines双跑=vacuous非PASS', () => {
    // determinismVerdict：同 hash + 空行 = vacuous；同 hash + 非空 = match；异 hash = mismatch
    eq(determinismVerdict('h', 'h', 0), 'vacuous-empty-runs');
    eq(determinismVerdict('h', 'h', 3), 'match');
    eq(determinismVerdict('h1', 'h2', 0), 'mismatch');
    // 全 vacuous 臂：determinism 合成不是 match；门禁不是 PASS；usable=false
    const mk = (over) => ({
      winRatePct: null, payoff: null, expectancyUsd: null, netPnlUsd: 0, maxDrawdownUsd: 0,
      worstTradeUsd: null, tailLoss: { p99: null }, feesUsd: 0, settlementRatePct: null,
      avgHoldSecFloor: null, closed: 0, windowsRun: 3, determinism: 'match', contaminated: false,
      entry: { ordersPlaced: 0, ordersRejected: 0, rejectionCauses: {}, accountFreezes: {} },
      ...over,
    });
    const aggVac = aggregate('A', [mk({ determinism: 'vacuous-empty-runs' }), mk({ determinism: 'vacuous-empty-runs' })]);
    eq(aggVac.determinism, 'vacuous-empty-runs');
    const aggMix = aggregate('A', [mk({ determinism: 'vacuous-empty-runs' }), mk({ determinism: 'match', closed: 5 })]);
    eq(aggMix.determinism, 'vacuous-mixed');
    const aggsVac = {
      A: mk({ determinism: 'vacuous-empty-runs', entry: { ordersPlaced: 115, ordersRejected: 115, rejectionCauses: { 'venue.WouldCross': 115 }, accountFreezes: { default: '5 consecutive venue rejections' } } }),
      B: mk({ determinism: 'match', closed: 100, winRatePct: 55, payoff: 1.2, expectancyUsd: 0.2 }),
      C: mk({ determinism: 'match', closed: 90, winRatePct: 50, payoff: 1.1, expectancyUsd: 0.15 }),
    };
    const v5 = verdictOf(aggsVac, { ok: true, detail: 't' }, 2);
    // 空臂不充当确定性证据：usable 拉闸，gates 里 vacuous 显式记录
    eq(v5.usable, false);
    eq(v5.gates.determinism.A, 'vacuous-empty-runs');
    eq(v5.gates.determinismVacuous.A, 'vacuous-empty-runs');
    eq(v5.gates.determinismVacuous.B, undefined);
    // arm-starved 声明路径保持：结构化判决 + 拒单证据原样给出，不降级 invalid
    eq(v5.conclusion, 'arm-starved');
    eq(v5.starvedDetail.A.rejectionCauses['venue.WouldCross'], 115);
    eq(v5.starvedDetail.A.closed, 0);
    // B 臂空转（closed=0 无拒单）也走既定 starved 声明（closed==0 即 starved，
    // 证据如实为空），vacuous 只拉 usable，不改判决种类
    const aggsVacNoEvidence = {
      A: mk({ determinism: 'match', closed: 50, winRatePct: 50, payoff: 1, expectancyUsd: 0.1 }),
      B: mk({ determinism: 'vacuous-empty-runs' }),
      C: mk({ determinism: 'match', closed: 40, winRatePct: 50, payoff: 1, expectancyUsd: 0.1 }),
    };
    const v6 = verdictOf(aggsVacNoEvidence, { ok: true, detail: 't' }, 2);
    eq(v6.usable, false);
    eq(v6.conclusion, 'arm-starved');
    eq(v6.starvedDetail.B.closed, 0);
    eq(v6.starvedDetail.B.ordersPlaced, 0);
    // 真 mismatch 无论饿死与否都是 invalid（硬红优先于 starved）
    const aggsHard = {
      A: mk({ determinism: 'mismatch' }),
      B: mk({ determinism: 'match', closed: 100, winRatePct: 55, payoff: 1.2, expectancyUsd: 0.2 }),
      C: mk({ determinism: 'match', closed: 90, winRatePct: 50, payoff: 1.1, expectancyUsd: 0.15 }),
    };
    const v7 = verdictOf(aggsHard, { ok: true, detail: 't' }, 2);
    eq(v7.usable, false);
    eq(v7.conclusion, 'invalid');
    // 无 vacuous 时旧门禁语义不变：全 match → usable 且可出对照判决
    const v8 = verdictOf({
      A: mk({ closed: 100, winRatePct: 50, payoff: 1, expectancyUsd: 0.1, netPnlUsd: 10, maxDrawdownUsd: 5, worstTradeUsd: -2, tailLoss: { p99: 2 }, feesUsd: 1 }),
      B: mk({ closed: 100, winRatePct: 55, payoff: 1.2, expectancyUsd: 0.2, netPnlUsd: 20, maxDrawdownUsd: 4, worstTradeUsd: -1.5, tailLoss: { p99: 1.5 }, feesUsd: 0.9 }),
      C: mk({ closed: 90 }),
    }, { ok: true, detail: 't' }, 2);
    eq(v8.usable, true);
    eq(v8.conclusion, 'B-better-on-both-target-metrics');
    eq(v8.gates.determinismVacuous, {});
  });

  // 4c) arm-starved 归因段的数字可追溯（#1004）：报告文本里的「实测」数
  // 必须由 corpusTopStats 派生；旧 manifest（无该字段）必须印「未测」，
  // 绝不回退写死字面量。
  ok &= P('renderMd/arm-starved数字来自语料实算', () => {
    const baseManifest = {
      profile: 'realistic', seed: 962, seedBalance: '1000', maxOrderNotional: '12',
      minShares: '1', maxShares: '2', maxPositions: '10', minRounds: 200, tickMs: '50',
      generatedAtMs: 0, outDir: 'run', singleRun: false,
      corpus: { corpus: 'c.jsonl', sha256: 'a'.repeat(64), manifest: { market: 'm', startMs: null, endMs: null }, fixResolutions: { ok: 1, bad: 0 } },
      assetsAll: ['A'], windows: [],
      armDeclarations: {},
    };
    const starvedAgg = (determinism) => ({
      arm: 'A', windowsRun: 1, rounds: 200, closed: 0, wins: 0, losses: 0,
      winRatePct: null, payoff: null, expectancyUsd: null, netPnlUsd: 0,
      maxDrawdownUsd: 0, worstTradeUsd: null, tailLoss: { n: 0, p95: null, p99: null },
      feesUsd: 0, settlementRatePct: null, avgHoldSecFloor: null, avgHoldSecUpper: null,
      reasonHistogram: {}, entry: { ordersPlaced: 115, ordersRejected: 115, rejectionCauses: { 'venue.WouldCross': 115 }, accountFreezes: {} },
      determinism, contaminated: false, perWindow: [],
    });
    const okAgg = (determinism, closed) => ({
      arm: 'X', windowsRun: 1, rounds: 200, closed, wins: Math.floor(closed / 2), losses: closed - Math.floor(closed / 2),
      winRatePct: 50, payoff: 1, expectancyUsd: 0.1, netPnlUsd: closed * 0.1,
      maxDrawdownUsd: 1, worstTradeUsd: -1, tailLoss: { n: 1, p95: 1, p99: 1 },
      feesUsd: 0.5, settlementRatePct: 50, avgHoldSecFloor: 100, avgHoldSecUpper: 900,
      reasonHistogram: {}, entry: { ordersPlaced: 10, ordersRejected: 0, rejectionCauses: {}, accountFreezes: {} },
      determinism, contaminated: false, perWindow: [],
    });
    const verdictVac = verdictOf({ A: starvedAgg('vacuous-empty-runs'), B: okAgg('match', 10), C: okAgg('match', 9) }, { ok: true, detail: 't' }, 2);
    eq(verdictVac.conclusion, 'arm-starved');
    // 带 corpusTopStats：锁价数随输入变（3/4 ≠ 293,286），旧字面量不得再现
    const md1 = renderMd({ ...baseManifest, corpusTopStats: { quotes: 4, locked: 3, unparsed: 0 } }, [], { A: starvedAgg('vacuous-empty-runs'), B: okAgg('match', 10), C: okAgg('match', 9) }, verdictVac, []);
    if (!md1.includes('锁价 3/4（75.00%')) throw new Error(`锁价数未随输入实算: ${md1.match(/锁价[^）]*）?/)?.[0]}`);
    if (md1.includes('293,286')) throw new Error('报告仍含硬编码 293,286');
    // 旧 manifest 无 corpusTopStats：印「未测」，不伪造实测值
    const md2 = renderMd(baseManifest, [], { A: starvedAgg('match'), B: okAgg('match', 10), C: okAgg('match', 9) }, verdictVac, []);
    if (!md2.includes('未测')) throw new Error('旧 manifest 复渲染未印「未测」');
    if (md2.includes('293,286')) throw new Error('「未测」分支仍含硬编码 293,286');
    // 门禁区：vacuous 臂显式标 UNJUDGEABLE_EMPTY_RUN，不是 PASS
    if (!md1.includes('UNJUDGEABLE_EMPTY_RUN')) throw new Error('vacuous 臂未标 UNJUDGEABLE_EMPTY_RUN');
    if (/确定性 A：PASS/.test(md1)) throw new Error('vacuous 臂被判 PASS');
  });

  // 5) 对照组旋钮口径：与 #962 README 声明逐项对上
  ok &= P('CONTROL_KNOBS/对照口径', () => {
    const got = Object.fromEntries(CONTROL_KNOBS);
    eq(got.hard_stop_loss_pct, '0');
    eq(got.take_profit_pct, '0');
    eq(got.take_profit_pct_tight, '0');
    eq(got.trail_arm_pct, '0');
    eq(got.trail_giveback_pct, '0');
    eq(got.trail_giveback_pct_tight, '0');
    eq(got.thesis_death_reversal_pct, '100');
    eq(got.thesis_death_reversal_pct_tight, '0');
    eq(got.thesis_death_stale_ms, '0');
  });

  // 6) 命令装配：写实档旗标 + seed/knob 注入 + 臂间环境一致性
  ok &= P('buildArgs/写实档与knob', () => {
    const a = buildArgs(ARMS.A, '/tmp/stageA', 'win.jsonl', 'r.json', 'realistic');
    eq(a.includes('--fee-model'), true);
    eq(a[a.indexOf('--fee-model') + 1], 'official');
    eq(a.includes('--backtest-fast'), true);
    eq(a.includes('--entry-maker-timeout-ms'), true);
    eq(a[a.indexOf('--entry-maker-timeout-ms') + 1], '-1');
    const b = buildArgs(ARMS.B, '/tmp/stageB', 'win.jsonl', 'r.json', 'realistic');
    eq(b[b.indexOf('--entry-maker-timeout-ms') + 1], '1');
    const knobIdx = b.indexOf('--backtest-knob');
    eq(b[knobIdx + 1], `coin_flip_exit_lab:coin_seed=${seed}`);
    const c = buildArgs(ARMS.C, '/tmp/stageC', 'win.jsonl', 'r.json', 'realistic');
    const cKnobs = c.filter((x, i) => c[i - 1] === '--backtest-knob');
    eq(cKnobs.length, CONTROL_KNOBS.length + 1); // + coin_seed
    // A 臂不许有任何 knob（原默认出场 = 出厂 tunables）
    eq(a.includes('--backtest-knob'), false);
    // 两臂的语料/资金/费率段完全一致（除 enable/lua-dir/entry-timeout/knob）。
    // 注意命令里混有无值布尔旗标（--no-trade-log 等），不能按奇偶配对剥：
    // 只做"删掉臂差异旗标及其 value"的索引手术。
    const strip = (arr) => {
      const out = [];
      const SKIP = new Set(['--lua-strategy-dir', '--enable-strategy', '--entry-maker-timeout-ms', '--backtest-knob']);
      for (let i = 0; i < arr.length; i++) {
        if (SKIP.has(arr[i])) { i++; continue; } // 连同 value 一起删
        out.push(arr[i]);
      }
      return out;
    };
    eq(strip(a), strip(c));
  });

  console.log(ok ? '\nself-test: ALL PASS' : '\nself-test: FAILURES');
  process.exit(ok ? 0 : 1);
}

// ── main ────────────────────────────────────────────────────────────────────

if (SELF_TEST) selfTest();

if (armKeys.some((k) => !ARMS[k])) usage(`--arm 只接受 A/B/C 的组合，收到: ${armKeys.join(',')}`);
if (!['realistic', 'identity', 'both'].includes(profileKey)) usage(`--profile 只接受 realistic|identity|both，收到: ${profileKey}`);
const profiles = profileKey === 'both' ? ['realistic', 'identity'] : [profileKey];

if (REPORT_ONLY) {
  // 从既有 run 目录重算汇总（不碰内核、不碰语料）
  const manifestPath = join(OUT_DIR, 'ab-exit-964.json');
  if (!existsSync(manifestPath)) usage(`run manifest not found: ${manifestPath}`);
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const aggs = {};
  const tradesIndex = [];
  const coverage = { ok: manifest.coverage?.ok ?? false, detail: manifest.coverage?.detail ?? '' };
  for (const profile of manifest.profiles ?? ['realistic']) {
    for (const armKey of manifest.arms ?? ['A', 'B', 'C']) {
      const arm = ARMS[armKey];
      if (!arm) continue;
      const perWindow = [];
      for (const win of manifest.windows ?? []) {
        const runDir = join(OUT_DIR, 'runs', profile, armKey, win.name);
        const p1 = join(runDir, 'report-1.json');
        if (!existsSync(p1)) { perWindow.push(null); continue; }
        const rep = JSON.parse(readFileSync(p1, 'utf8'));
        const m = extractMetrics(rep, win, profile);
        m._firstAtMs = win.firstAtMs;
        const p2 = join(runDir, 'report-2.json');
        m.determinism = existsSync(p2)
          ? determinismVerdict(sha256File(p1), sha256File(p2), (Array.isArray(rep.tradeLines) ? rep.tradeLines : []).length)
          : (manifest.singleRun ? 'skipped' : 'missing');
        m._lossMags = (Array.isArray(rep.tradeLines) ? rep.tradeLines : [])
          .map((l) => num(l.netPnlUsd)).filter((v) => v != null && v < 0).map((v) => -v);
        perWindow.push(m);
      }
      const agg = aggregate(armKey, perWindow);
      agg.contaminated = agg.perWindow.some((w) => (w.strategyNames ?? []).some((s) => s !== arm.strategy) || (w.statsNames ?? []).some((s) => s !== arm.strategy));
      aggs[armKey] = agg;
      tradesIndex.push({ arm: armKey, profile, dir: join('trades', profile, armKey) });
    }
    const verdict = verdictOf(aggs, coverage, 2);
    writeFileSync(join(OUT_DIR, `ab-exit-964-${profile}.md`), renderMd({ ...manifest, profile }, manifest.windows, aggs, verdict, tradesIndex));
    console.log(`report-only: rewrote ${join(OUT_DIR, `ab-exit-964-${profile}.md`)}`);
  }
  process.exit(0);
}

// —— 正式实验路径 ——

if (!existsSync(CORE_BIN)) {
  console.error(`core binary not found: ${CORE_BIN}`);
  console.error('先构建：cargo build --release --workspace --locked（或用 --core-bin / BK_CORE_BIN 指向已构建二进制）');
  process.exit(2);
}
const ARM_KEYS = armKeys.filter((k) => ARMS[k]);
const needB = ARM_KEYS.includes('B') || ARM_KEYS.includes('C');
const B_PKG_DIR = BPkgOverride ?? join(ROOT, 'user_layer', 'strategies_lua', 'coin_flip_exit_lab');

console.log(`ab-exit-964 — issue #964 stone A/B 出场对照（臂 ${ARM_KEYS.join('/')}，profile ${profiles.join('+')}）`);
console.log(`run dir: ${OUT_DIR}`);
const corpus = discoverCorpus();
const pre = await preflightCorpus(corpus);
console.log(`corpus: ${corpus}`);
console.log(`  sha256 ${pre.sha256}`);
console.log(`  market=${pre.manifest.market} ${utc(pre.manifest.startMs)} → ${utc(pre.manifest.endMs)} wallet=${pre.manifest.wallet}`);
console.log(`  fix_resolutions --check: ${pre.fixResolutions.lines} lines, ${pre.fixResolutions.ok} ok, ${pre.fixResolutions.bad} bad`);

const scan = await scanCorpus(corpus);
console.log(`scan: ${scan.totalLines} lines, ${scan.rounds.length} rounds (slots), resolutions ${scan.resLineOf.size}, assets ${scan.assetsAll.join('/')}`);

const windows = planWindows(scan, minRoundsPerWindow, windowsOverride);
const totalRounds = windows.reduce((s, w) => s + w.rounds, 0);
const minWinRounds = Math.min(...windows.map((w) => w.rounds));
const coverage = {
  ok: minWinRounds >= minRoundsPerWindow && totalRounds >= 200,
  detail: `${windows.length} 窗 × ${minWinRounds}..${Math.max(...windows.map((w) => w.rounds))} 轮（共 ${totalRounds}；验收线 ≥200 轮/臂）`,
};
console.log(`windows: ${coverage.detail}`);

// staging：A 臂包在仓库里；B/C 需要包就绪（缺失 = 硬错，dry-run 也一样失败）
const staged = {};
if (ARM_KEYS.includes('A')) staged.A = stagePackage(join(ROOT, 'user_layer', 'strategies_lua', 'lua_momentum'), 'lua_momentum', 'A');
if (ARM_KEYS.includes('B')) staged.B = stagePackage(B_PKG_DIR, 'coin_flip_exit_lab', 'B');
if (ARM_KEYS.includes('C')) staged.C = stagePackage(B_PKG_DIR, 'coin_flip_exit_lab', 'C');

if (DRY_RUN) {
  console.log('\ndry-run: 计划的回放矩阵（不执行、不切片）——');
  let n = 0;
  for (const profile of profiles) {
    for (const armKey of ARM_KEYS) {
      const arm = ARMS[armKey];
      const ex = buildArgs(arm, `<staged:${arm.pkg}>`, join(OUT_DIR, 'windows', 'win-00.jsonl'), join(OUT_DIR, 'runs', profile, armKey, 'win-00', 'report-1.json'), profile);
      const runs = SINGLE_RUN ? 1 : 2;
      n += windows.length * runs;
      console.log(`\n[${profile}/${armKey}] ${arm.pkg}（${runs} 跑/窗 × ${windows.length} 窗 = ${windows.length * runs} 次回放）`);
      console.log(`  例：blitzkrieg-core ${ex.join(' ')}`);
      if (arm.knobs) console.log(`  对照清零 knobs: ${arm.knobs.map(([k, v]) => `${k}=${v}`).join(' ')}`);
    }
  }
  console.log(`\n总计 ${n} 次回放 + ${windows.length} 次切片（正式跑时写入 ${join(OUT_DIR, 'windows')}）。`);
  process.exit(0);
}

mkdirSync(join(OUT_DIR, 'windows'), { recursive: true });
mkdirSync(join(OUT_DIR, 'runs'), { recursive: true });

// 切片（跨臂共享，一次写入；断点续跑校验行区间+哈希后复用）
for (const win of windows) {
  const s = await sliceWindow(corpus, win, join(OUT_DIR, 'windows', `${win.name}.jsonl`));
  win.sha256 = s.sha256;
  console.log(`  ${win.name}: lines [${win.startLine}..${win.endLine}] ${win.rounds} 轮 ${utc(win.firstAtMs)}→${utc(win.expiryAtMs)} sha ${s.sha256.slice(0, 12)}${s.cached ? ' (cached)' : ''}`);
}

// 正式跑批（顺序执行：确定性优先，避免 #199 data-lock 与并发抖动）
const aggs = {};
const tradesIndex = [];
const failures = [];
let lastUsable = true;
let lastConclusion = 'pending';
for (const profile of profiles) {
  for (const armKey of ARM_KEYS) {
    const arm = ARMS[armKey];
    const perWindow = [];
    for (const win of windows) {
      const runDir = join(OUT_DIR, 'runs', profile, armKey, win.name);
      mkdirSync(runDir, { recursive: true });
      const p1 = join(runDir, 'report-1.json');
      const p2 = join(runDir, 'report-2.json');
      const label = `${profile}/${armKey}/${win.name}`;
      const r1 = await runBacktest(buildArgs(arm, staged[armKey].stagedDir, join(OUT_DIR, 'windows', `${win.name}.jsonl`), p1, profile), p1, `${label} run1`);
      if (r1.invalid) {
        failures.push({ profile, arm: armKey, win: win.name, reason: r1.invalid });
        perWindow.push(null);
        continue;
      }
      let determinism = 'skipped';
      if (!SINGLE_RUN) {
        const r2 = await runBacktest(buildArgs(arm, staged[armKey].stagedDir, join(OUT_DIR, 'windows', `${win.name}.jsonl`), p2, profile), p2, `${label} run2`);
        if (r2.invalid) {
          failures.push({ profile, arm: armKey, win: win.name, reason: r2.invalid });
          perWindow.push(null);
          continue;
        }
        const h1 = sha256File(p1), h2 = sha256File(p2);
        const run1Lines = Array.isArray(r1.report.tradeLines) ? r1.report.tradeLines : [];
        determinism = determinismVerdict(h1, h2, run1Lines.length);
        if (determinism === 'mismatch') {
          console.error(`    DETERMINISM MISMATCH ${label}: run1 ${h1.slice(0, 12)} vs run2 ${h2.slice(0, 12)} — 确定性 bug，实验无效`);
        } else if (determinism === 'vacuous-empty-runs') {
          console.error(`    DETERMINISM VACUOUS ${label}: 双跑 hash 相同但 0 tradeLines（空==空，UNJUDGEABLE_EMPTY_RUN）— 不计确定性证据`);
        }
      }
      const m = extractMetrics(r1.report, win, profile);
      m.determinism = determinism;
      m._firstAtMs = win.firstAtMs;
      m._lossMags = (Array.isArray(r1.report.tradeLines) ? r1.report.tradeLines : [])
        .map((l) => num(l.netPnlUsd)).filter((v) => v != null && v < 0).map((v) => -v);
      const tl = mergeTradeLines(r1.report, armKey, win, profile, join('trades', profile));
      m.tradesExport = tl.path;
      perWindow.push(m);
      console.log(`    ${label}: closed=${m.closed} WR=${r4(m.winRatePct)}% payoff=${r4(m.payoff)} net=${r4(m.netPnlUsd)} 确定性=${determinism} (${tl.count} tradeLines → ${tl.path})`);
    }
    const agg = aggregate(armKey, perWindow);
    // #302 污染检查：报告里只许出现本臂策略名
    agg.contaminated = agg.perWindow.some((w) => (w.strategyNames ?? []).some((s) => s !== arm.strategy) || (w.statsNames ?? []).some((s) => s !== arm.strategy));
    if (agg.contaminated) console.error(`    CONTAMINATION ${profile}/${armKey}: 报告里出现了 ${arm.strategy} 之外的策略名`);
    aggs[armKey] = agg;
    tradesIndex.push({ arm: armKey, profile, dir: join('trades', profile, armKey) });
  }

  const verdict = verdictOf(aggs, coverage, 2);
  const manifest = {
    generatedAtMs: Date.now(),
    issue: 964,
    script: 'scripts/ab-exit-964.mjs',
    outDir: OUT_DIR,
    coreBin: CORE_BIN,
    coreBinSha256: sha256File(CORE_BIN),
    corpus: pre,
    assetsAll: scan.assetsAll,
    corpusTopStats: scan.topStats,
    windows,
    profiles,
    profile,
    arms: ARM_KEYS,
    armDeclarations: Object.fromEntries(ARM_KEYS.map((k) => [k, {
      pkg: ARMS[k].pkg,
      strategy: ARMS[k].strategy,
      entryMakerTimeoutMs: ARMS[k].entryMakerTimeoutMs,
      knobs: ARMS[k].knobs,
      staged: staged[k] ? { source: staged[k].source, sourceOverridden: staged[k].sourceOverridden, manifestSha256: staged[k].manifestSha256, entrySha256: staged[k].entrySha256, version: staged[k].version } : null,
    }])),
    seed, seedBalance, maxOrderNotional, minShares, maxShares, maxPositions,
    minRounds: minRoundsPerWindow,
    tickMs: PROFILES[profile].tickMs,
    singleRun: SINGLE_RUN,
    coverage,
    failures,
    aggregates: aggs,
    verdict,
  };
  writeFileSync(join(OUT_DIR, `ab-exit-964.json`), JSON.stringify(manifest, null, 2) + '\n');
  writeFileSync(join(OUT_DIR, `ab-exit-964-${profile}.md`), renderMd(manifest, windows, aggs, verdict, tradesIndex));
  console.log(`\nreport: ${join(OUT_DIR, `ab-exit-964.json`)}`);
  console.log(`report: ${join(OUT_DIR, `ab-exit-964-${profile}.md`)}`);
  console.log(`verdict: ${verdict.conclusion}（usable=${verdict.usable}, B better on ${verdict.bBetterOn} metrics）`);
  lastUsable = verdict.usable;
  lastConclusion = verdict.conclusion;
}

if (failures.length) {
  console.error(`\nINVALID: ${failures.length} 个回放被内核自检判无效（详见 ab-exit-964.json .failures）—— 实验不成立，拒绝出结论。`);
  process.exitCode = 1;
}
if (!lastUsable) {
  console.error(`\nVERDICT: ${lastConclusion} —— 门禁未全绿（覆盖/确定性/归因污染，见报告），实验判无效。`);
  process.exitCode = 1;
}
if (!coverage.ok) {
  console.error(`\nCOVERAGE: ${coverage.detail} —— 不足 #964 验收线（≥200 轮/臂），报告已判 coverage FAIL。`);
  process.exitCode = 1;
}
