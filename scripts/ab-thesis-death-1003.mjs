#!/usr/bin/env node
/**
 * ab-thesis-death-1003 — thesis_death 误杀 A/B 对照装置（issue #1003）。
 *
 * 问题（#1003，P1）：#994 给 6 个 LEGACY 包统一接入的 thesis_death 钩子在
 * spread_arb / single_leg_pair 上实测误杀（death 臂净利劣于 nodeath 臂）。
 * workbuddy 独立复测（2026-11-08，#1003 W5 评论，内核 31d4850e）：
 *
 *   spread_arb       death −568.971  vs nodeath −495.867  Δ −$73.10（比原报告 −$62.64 更差）
 *   single_leg_pair  death   −6.735  vs nodeath   −6.665  Δ −$0.07（旧轮 +0.663 方向翻转）
 *
 * 费率隔离对照（--fee-model legacy_quadratic）证明数字变化全部来自 #998 的
 * 官方费率档切换，判决未被翻案：spread_arb 在两种费率下死亡臂都显著更差。
 *
 * ── 装置（沿用 #994/#1298 验收同款口径，跑前钉死） ─────────────────────────
 * 两臂，同一份语料、同一资金、同一条内核、同一 tick 参数，唯一差异是
 * 「论点死亡是否生效」：
 *
 *   death 臂   = 出厂包（BK_THESIS_DEATH_ENABLE=true / 形态 A 的 bk_thesis_death 原样）
 *   nodeath 臂 = staged 副本，仅把死亡判据关掉（形态 B：开关翻 false；形态 A：
 *                bk_thesis_death 首行插入 return false）—— 入场逻辑零改动，
 *                两臂入场决策相同，差异只能来自死亡平仓。副本 manifest 按
 *                副本 strategy.lua 字节重签（old→new 哈希打印进 run manifest）。
 *                注意：--backtest-knob 做不到这件事——标准库「book 消失仍判死」
 *                不受 stale=0 影响（那是超时阈值不是存在性判死开关），只有
 *                改包才能完全关闭钩子；所以 nodeath 臂必须是 staged 副本。
 *
 *   分母 denominator = death 臂 exit_reason == ThesisDeathExit 的平仓 token 数
 *   误杀 misfire     = 其中该 token 在 nodeath 臂净利 > 0 的笔数
 *   误杀率           = misfire / denominator（分母 0 = UNJUDGEABLE，不写 0%）
 *
 * 分母为 0 的包（0 intent）= UNJUDGEABLE_ZERO_ORDERS：不写 0%、不判可接受
 * （#1003 要求的诚实边界）。
 *
 * 语料（预声明，不挑窗）：data/onchain 的 15m-act 单文件全窗（sha256 必须
 * 等于 manifest sha256.events；fix_resolutions --check 门禁；字节不符 exit 2）。
 *
 * 确定性（#351）：每臂 × 每包默认跑两遍，报告 sha256 必须逐字节一致；
 * 不一致 = 确定性 bug，实验判无效而不是统计噪音。--single-run 可跳过。
 *
 * 用法：
 *   node scripts/ab-thesis-death-1003.mjs                        # 全实验（death vs nodeath × 双跑）
 *   node scripts/ab-thesis-death-1003.mjs --packages spread_arb  # 只跑指定包
 *   node scripts/ab-thesis-death-1003.mjs --single-run           # 跳过第二遍（快速过框架）
 *   node scripts/ab-thesis-death-1003.mjs --fee-model legacy_quadratic   # 费率隔离对照
 *   node scripts/ab-thesis-death-1003.mjs --dry-run              # 只出计划
 *   node scripts/ab-thesis-death-1003.mjs --report-only <dir>    # 从既有产物重算汇总
 *   node scripts/ab-thesis-death-1003.mjs --self-test            # 框架自测（离线，不碰内核/语料）
 *
 * 退出码：0 = 实验有效（结论另看报告）；1 = 实验无效（确定性破裂/污染/内核失败）；
 * 2 = 用法或环境错误（语料缺失、包缺失、二进制缺失）。
 */

import { spawn } from './lib/child-guard.mjs';
import { createHash } from 'crypto';
import {
  cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync,
} from 'fs';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(join(__dirname, '..'));

// ── 预声明的实验（数字存在之前先钉死）───────────────────────────────────────

/** 全部接入包（#994 的 6 个 LEGACY 接入者）。0-intent 包照样跑，出数后判 UNJUDGEABLE。 */
const ALL_PACKAGES = [
  'flash_arb',
  'hot_side_momentum',
  'mad_dog',
  'single_leg_pair',
  'spread_arb',
  'whale_follow',
];

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
const FEE_MODEL = flag('--fee-model') ?? ''; // '' = 内核默认（main 上 = official，#998）
const PACKAGES = (flag('--packages') ?? ALL_PACKAGES.join(','))
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);
const seedBalance = flag('--seed-balance') ?? '1000';
const maxOrderNotional = flag('--max-order-notional') ?? '12';
const runTimeoutMin = Number(flag('--run-timeout-min') ?? 30);
const CORE_BIN = flag('--core-bin') ?? process.env.BK_CORE_BIN ?? join(ROOT, 'target', 'release', 'blitzkrieg-core');
const corpusArg = flag('--corpus') ?? null;
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-') + 'Z';
const OUT_DIR = REPORT_ONLY ? resolve(REPORT_ONLY) : resolve(flag('--out') ?? join(ROOT, 'data', 'evolution', 'ab-thesis-death-1003', stamp));

const r3 = (n) => (n == null ? '—' : (Math.round(n * 1000) / 1000).toString());
const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');

function usage(msg) {
  if (msg) console.error(`error: ${msg}`);
  console.error(`usage: node scripts/ab-thesis-death-1003.mjs [--packages flash_arb,...] [--corpus <15m-act.jsonl>]
       [--seed-balance 1000] [--max-order-notional 12] [--fee-model legacy_quadratic]
       [--single-run] [--out dir] [--dry-run] [--report-only dir] [--core-bin path] [--self-test]`);
  process.exit(2);
}

for (const p of PACKAGES) {
  if (!ALL_PACKAGES.includes(p)) usage(`--packages 只接受 ${ALL_PACKAGES.join('/')}，收到: ${p}`);
}

// ── 语料预检（manifest sha256 门禁 + fix_resolutions --check 门禁）────────────

function sha256Corpus(corpus) {
  return createHash('sha256').update(readFileSync(corpus)).digest('hex');
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
  const sha = sha256Corpus(corpus);
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
    manifestPath,
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

function discoverCorpus() {
  if (corpusArg) {
    const p = resolve(ROOT, corpusArg);
    if (!existsSync(p)) usage(`corpus not found: ${p}`);
    return p;
  }
  const dir = join(ROOT, 'data', 'onchain');
  if (!existsSync(dir)) usage('data/onchain 不存在 —— 15m-act 语料是本装置的预声明坐标（--corpus 可指定别处）');
  const found = readdirSync(dir)
    .filter((f) => /-15m-act-[^.]*\.jsonl$/.test(f) && !/\.(trades|cashflows|redemptions)\.jsonl$/.test(f))
    .sort();
  if (found.length === 0) usage('data/onchain 下没有 15m-act 语料（找 *-15m-act-*.jsonl）');
  if (found.length > 1) usage(`data/onchain 下有多份 15m-act 语料，请用 --corpus 指定：\n  ${found.join('\n  ')}`);
  return join(dir, found[0]);
}

// ── 包 staging：death 出厂原样；nodeath 是重签 manifest 的反事实副本 ──────────

function stage(pkg, mode) {
  const src = join(ROOT, 'user_layer', 'strategies_lua', pkg);
  if (!existsSync(src) || !existsSync(join(src, 'manifest.json'))) {
    usage(`package missing: ${src}`);
  }
  // /tmp（不是 os.tmpdir()）：内核的 strategy-root allow-list 在 macOS 上拒绝
  // /var/folders 沙箱路径（#1298 装置实测），/tmp 可加载。
  const root = mkdtempSync(join('/tmp', `bk-1003-${pkg}-${mode}-`));
  const dir = join(root, pkg);
  cpSync(src, dir, { recursive: true });
  const staging = { mode, source: src, manifestSha256Old: null, manifestSha256New: sha256File(join(dir, 'manifest.json')), mutations: [] };
  if (mode === 'nodeath') {
    const luaPath = join(dir, 'strategy.lua');
    const before = readFileSync(luaPath, 'utf8');
    let after = before;
    // 形态 B：翻开关。形态 A：谓词首行插入 return false。
    after = after.replace(/BK_THESIS_DEATH_ENABLE\s*=\s*true/g, 'BK_THESIS_DEATH_ENABLE = false');
    after = after.replace(
      /(function\s+bk_thesis_death\s*\([^)]*\))/,
      '$1\n  if true then return false end -- #1003 measurement: death arm disabled',
    );
    if (after === before) {
      // 包已不声明论点死亡（#1003 按包关闭后的形状）：nodeath 臂 = 原样副本，
      // 两臂逐字节同包。报告若仍出现 ThesisDeathExit 平仓 = 残留强制路径；
      // 两臂报告逐字节一致 = Δ≡0 的最强形式。
      staging.mutations.push('none (package declares no thesis death — per-package closed, arms identical)');
      console.log(`  staged ${pkg}/nodeath (factory copy verbatim — no declaration to disable; arms identical by construction)`);
    } else {
      writeFileSync(luaPath, after);
      // staged 副本的 manifest 按副本字节重签（loader 拒收旧收据）；旧→新哈希
      // 全程打印并记录 —— 这是测量用的反事实臂，永远不是出厂包。
      const manifestPath = join(dir, 'manifest.json');
      const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
      const oldSha = manifest.sha256;
      const newSha = createHash('sha256').update(readFileSync(luaPath)).digest('hex');
      manifest.sha256 = newSha;
      writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
      staging.manifestSha256Old = oldSha;
      staging.manifestSha256New = newSha;
      if (/BK_THESIS_DEATH_ENABLE\s*=\s*false/.test(after)) staging.mutations.push('form-B flag');
      if (/#1003 measurement: death arm disabled/.test(after)) staging.mutations.push('form-A predicate');
      console.log(`  staged ${pkg}/nodeath (${staging.mutations.join(' + ')}); re-signed receipt ${oldSha.slice(0, 12)}… -> ${newSha.slice(0, 12)}…`);
    }
  } else {
    console.log(`  staged ${pkg}/death (factory package verbatim)`);
  }
  return { root, dir, luaRoot: root, staging };
}

// ── 回放（spawn 形状 = #994 验收装置逐项相同，只多确定性双跑）────────────────

function parseSelfCheck(stderr) {
  const m = stderr.match(/strategy startup self-check: requested=(\d+) resolved=(\d+) enabled=\[([^\]]*)\]/);
  if (!m) return null;
  return { requested: Number(m[1]), resolved: Number(m[2]), enabled: m[3].split(',').map((s) => s.trim()).filter(Boolean) };
}

function invalidReason(stderr, selfCheck, pkg) {
  if (/unknown strategy requested/.test(stderr)) return 'kernel log: unknown strategy requested (包没加载)';
  if (/refusing to start: none of the \d+ explicitly requested/.test(stderr)) return 'kernel log: 内核拒绝启动（requested 策略未解析）';
  if (/counterfactual knobs refused/.test(stderr)) return 'kernel log: --backtest-knob 被拒';
  if (!selfCheck) return 'kernel log: 无 strategy startup self-check 行（二进制过旧？）';
  if (selfCheck.resolved < 1) return `kernel log: requested ${selfCheck.requested} resolved ${selfCheck.resolved}`;
  if (!selfCheck.enabled.includes(pkg)) return `kernel log: 启用集 ${selfCheck.enabled.join(', ')} 不含 ${pkg}`;
  return null;
}

function buildArgs(pkg, luaDir, reportPath) {
  return [
    '--mode', 'dry',
    '--engine',
    '--no-discovery',
    '--no-strategy-state',
    '--no-intent-audit',
    '--lua-strategy-dir', luaDir,
    '--enable-strategy', pkg,
    '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
    '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
    '--seed-balance', String(seedBalance), '--max-order-notional', String(maxOrderNotional),
    '--backtest', corpusPath,
    ...(FEE_MODEL ? ['--fee-model', FEE_MODEL] : []),
    '--backtest-report', reportPath,
    '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
  ];
}

function runOne(pkg, luaDir, reportPath, label) {
  return new Promise((res) => {
    console.log(`    $ blitzkrieg-core … --enable-strategy ${pkg} --backtest …   # ${label}`);
    const child = spawn(CORE_BIN, buildArgs(pkg, luaDir, reportPath), {
      cwd: OUT_DIR,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir },
    });
    let stderr = '';
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    const watchdog = setTimeout(() => {
      console.error(`    TIMEOUT after ${runTimeoutMin} min — stopping the run group`);
      try { process.kill(-child.pid, 'SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
    }, runTimeoutMin * 60_000);
    child.on('exit', (code, signal) => {
      clearTimeout(watchdog);
      const sc = parseSelfCheck(stderr);
      const invalid = invalidReason(stderr, sc, pkg);
      if (invalid) {
        console.error(`    INVALID (${label}): ${invalid}`);
        res({ report: null, invalid });
        return;
      }
      if (code !== 0) {
        console.error(`    backtest exited code=${code} signal=${signal ?? child.signalCode ?? 'none'} (${label}); 拒绝从失败运行读报告`);
        console.error(stderr.split('\n').slice(-8).join('\n'));
        process.exit(1);
      }
      try {
        const r = JSON.parse(readFileSync(reportPath, 'utf8'));
        res({
          closed: r.trades?.closed ?? 0,
          wins: r.trades?.wins ?? 0,
          orders: r.orders?.orders ?? null,
          fills: r.orders?.filled ?? null,
          netPnlUsd: Number(r.trades?.netPnlUsd ?? 0),
          // 归因污染守卫（#302）：报告里只许出现本包名。
          strategies: (r.strategies ?? []).map((s) => s.name ?? s.strategy ?? String(s)),
          blocked: r.blocked ?? null,
          errors: r.errors ?? null,
          tradeLines: r.tradeLines ?? [],
          report: reportPath,
          invalid: null,
        });
      } catch (e) {
        console.error(`    cannot parse report ${reportPath}: ${e.message}`);
        process.exit(1);
      }
    });
  });
}

// ── 误杀配对（#994 验收口径：token 维度，nodeath 净利 > 0 = 误杀）────────────

export function pair(deathArm, nodeathArm) {
  const dLines = deathArm.tradeLines ?? [];
  const nLines = nodeathArm.tradeLines ?? [];
  const deaths = dLines.filter((l) => l.reason === 'ThesisDeathExit');
  const deathTokens = new Set(deaths.map((l) => l.tokenId).filter((x) => x != null && x !== ''));
  const nodeathByToken = new Map();
  for (const l of nLines) {
    if (l.tokenId == null || l.tokenId === '') continue;
    nodeathByToken.set(l.tokenId, (nodeathByToken.get(l.tokenId) ?? 0) + Number(l.netPnlUsd ?? 0));
  }
  let matched = 0;
  let misfire = 0;
  let unmatched = 0;
  const misfireDetail = [];
  for (const tok of deathTokens) {
    if (!nodeathByToken.has(tok)) {
      unmatched += 1;
      continue;
    }
    matched += 1;
    const alt = nodeathByToken.get(tok);
    if (alt > 0) {
      misfire += 1;
      misfireDetail.push({ tokenId: tok, nodeathNetPnlUsd: Number(alt.toFixed(6)) });
    }
  }
  return {
    deathCloses: deaths.length,
    deathTokens: deathTokens.size,
    matched,
    unmatched,
    misfire,
    misfireDetail,
  };
}

// ── 汇总判决 ────────────────────────────────────────────────────────────────

export function verdictFor(pkgRow) {
  // 通过线（#1003，逐包）：death 净利 ≥ nodeath 净利 且 误杀显著下降。
  // 这里只产出判定材料；「显著下降」是相对旧轮误杀率的运营判断，报告文本写。
  const { death, nodeath, pairing } = pkgRow;
  if ((death?.orders ?? 0) === 0 && (nodeath?.orders ?? 0) === 0) {
    return { judgeable: false, why: 'UNJUDGEABLE_ZERO_ORDERS（两臂各 0 单，不是被闸门拦就是没产生 intent；按 #1003 不写 0%、不判可接受）' };
  }
  if (!death || !nodeath) return { judgeable: false, why: 'arm missing' };
  const delta = death.netPnlUsd - nodeath.netPnlUsd;
  const rate = pairing.deathTokens > 0 ? (pairing.misfire / pairing.deathTokens) * 100 : null;
  return {
    judgeable: true,
    delta,
    misfireRatePct: rate,
    deathNetNotWorse: delta >= 0,
    verdict: delta >= 0 ? 'PASS-candidate (death not worse)' : 'FAIL (death worse)',
  };
}

// ── 报告渲染 ────────────────────────────────────────────────────────────────

function renderSummary(runManifest, pkgRows, packages) {
  const lines = [];
  lines.push(`# ab-thesis-death-1003 — issue #1003 thesis_death 误杀 A/B（${runManifest.feeModel || 'kernel-default'} 费率档）`);
  lines.push('');
  lines.push(`生成：${new Date(runManifest.generatedAtMs).toISOString()}；内核：\`${runManifest.coreBin}\``);
  lines.push(`语料：\`${runManifest.corpus.corpus}\`（sha256 \`${runManifest.corpus.sha256.slice(0, 16)}…\` PIN_OK）`);
  lines.push(`资金：seed-balance ${runManifest.seedBalance} / max-order-notional ${runManifest.maxOrderNotional}；tick 50ms；确定性：${runManifest.singleRun ? 'single-run' : '双跑逐字节一致'}`);
  lines.push('');
  lines.push('| 包 | 分母/误杀/误杀率/unmatched | death 净利 | nodeath 净利 | Δ(death−nodeath) | 判定 |');
  lines.push('|---|---|---|---|---|---|');
  for (const p of packages) {
    const row = pkgRows[p];
    const v = row.verdict;
    if (!v.judgeable) {
      lines.push(`| ${p} | — | — | — | — | ${v.why} |`);
      continue;
    }
    const rate = v.misfireRatePct == null ? '—' : `${v.misfireRatePct.toFixed(2)}%`;
    lines.push(`| ${p} | ${row.pairing.deathTokens}/${row.pairing.misfire}/${rate}/${row.pairing.unmatched} | ${r3(row.death.netPnlUsd)} | ${r3(row.nodeath.netPnlUsd)} | ${r3(v.delta)} | ${v.verdict} |`);
  }
  lines.push('');
  return lines.join('\n') + '\n';
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

  // 1) 配对：death 臂 ThesisDeathExit token × nodeath 臂逐 token 净利。
  ok &= P('pair/误杀分母与去重', () => {
    const death = { tradeLines: [
      { tokenId: 't1', reason: 'ThesisDeathExit', netPnlUsd: '-0.5' },
      { tokenId: 't1', reason: 'ThesisDeathExit', netPnlUsd: '-0.2' }, // 同 token 两笔 → 分母去重计 1
      { tokenId: 't2', reason: 'ThesisDeathExit', netPnlUsd: '-1.0' },
      { tokenId: 't3', reason: 'Settlement', netPnlUsd: '0.3' }, // 非 death 平仓不进分母
      { tokenId: '', reason: 'ThesisDeathExit', netPnlUsd: '-0.1' }, // 空 token 不可配对 → 不进分母
    ] };
    const nodeath = { tradeLines: [
      { tokenId: 't1', reason: 'Settlement', netPnlUsd: '0.4' },
      { tokenId: 't2', reason: 'Settlement', netPnlUsd: '-0.6' },
      { tokenId: 't4', reason: 'Settlement', netPnlUsd: '9' }, // nodeath 独有 → 无配对
    ] };
    const p = pair(death, nodeath);
    eq(p.deathCloses, 4);
    eq(p.deathTokens, 2);
    eq(p.matched, 2);
    eq(p.unmatched, 0);
    eq(p.misfire, 1);
    eq(p.misfireDetail, [{ tokenId: 't1', nodeathNetPnlUsd: 0.4 }]);
  });

  // 2) 判定：spread_arb 形状（death 更差 → FAIL）、single_leg_pair 形状（略差 → FAIL）、
  //    0-intent 包（UNJUDGEABLE，绝不写 0%）。
  ok &= P('verdict/spread_arb-death-worse', () => {
    const v = verdictFor({
      death: { netPnlUsd: -568.971, orders: 10422 },
      nodeath: { netPnlUsd: -495.867, orders: 113423 },
      pairing: { deathTokens: 764, misfire: 309 },
    });
    eq(v.judgeable, true);
    eq(Math.round(v.delta * 1000) / 1000, -73.104);
    eq(v.deathNetNotWorse, false);
    eq(v.verdict, 'FAIL (death worse)');
  });
  ok &= P('verdict/single_leg_pair-slight-worse', () => {
    const v = verdictFor({
      death: { netPnlUsd: -6.735246, orders: 719 },
      nodeath: { netPnlUsd: -6.66497375, orders: 791 },
      pairing: { deathTokens: 238, misfire: 121 },
    });
    eq(v.deathNetNotWorse, false);
    eq(v.verdict, 'FAIL (death worse)');
    eq(Math.round(v.misfireRatePct * 100) / 100, 50.84);
  });
  ok &= P('verdict/death-not-worse-passes', () => {
    const v = verdictFor({
      death: { netPnlUsd: -3.0, orders: 10 },
      nodeath: { netPnlUsd: -4.0, orders: 10 },
      pairing: { deathTokens: 5, misfire: 0 },
    });
    eq(v.deathNetNotWorse, true);
    eq(v.verdict, 'PASS-candidate (death not worse)');
  });
  ok &= P('verdict/zero-intent-unjudgeable', () => {
    const v = verdictFor({
      death: { netPnlUsd: 0, orders: 0 },
      nodeath: { netPnlUsd: 0, orders: 0 },
      pairing: { deathTokens: 0, misfire: 0 },
    });
    eq(v.judgeable, false);
    eq(v.why.includes('UNJUDGEABLE_ZERO_ORDERS'), true);
  });

  // 3) buildArgs：两臂除 enable/lua-dir/report 外逐项相同；无 knob（nodeath 靠改包）。
  ok &= P('buildArgs/两臂对称', () => {
    const built = (luaDir, report) => [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--no-intent-audit',
      '--lua-strategy-dir', luaDir,
      '--enable-strategy', 'spread_arb',
      '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', String(seedBalance), '--max-order-notional', String(maxOrderNotional),
      '--backtest', 'corpus.jsonl',
      ...(FEE_MODEL ? ['--fee-model', FEE_MODEL] : []),
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
    ];
    const a = built('/tmp/A', 'r1.json');
    const b = built('/tmp/B', 'r2.json');
    const strip = (arr) => arr.filter((x, i) => !(x === '--lua-strategy-dir' || arr[i - 1] === '--lua-strategy-dir' || x === '--enable-strategy' || arr[i - 1] === '--enable-strategy' || x === '--backtest-report' || arr[i - 1] === '--backtest-report'));
    eq(strip(a), strip(b));
    eq(a[a.indexOf('--enable-strategy') + 1], 'spread_arb');
    eq(a.includes('--backtest-knob'), false, 'nodeath 臂靠改包关闭，不用 knob（book-gone 判死 knob 关不掉）');
  });

  // 4) staging 变异器：形态 B 翻开关、形态 A 插谓词短路（纯文本，不落盘）。
  ok &= P('mutate/两种声明形态', () => {
    const formB = 'BK_THESIS_DEATH_ENABLE = true\nfunction bk_evaluate() return {} end';
    let after = formB.replace(/BK_THESIS_DEATH_ENABLE\s*=\s*true/g, 'BK_THESIS_DEATH_ENABLE = false');
    after = after.replace(/(function\s+bk_thesis_death\s*\([^)]*\))/, '$1\n  if true then return false end -- #1003 measurement: death arm disabled');
    eq(after.startsWith('BK_THESIS_DEATH_ENABLE = false'), true);
    eq(after.includes('function bk_thesis_death'), false, 'form B 没有谓词，替换是无害 no-op');
    const formA = 'function bk_thesis_death(h)\n  return h.x == 1\nend';
    let afterA = formA.replace(/BK_THESIS_DEATH_ENABLE\s*=\s*true/g, 'BK_THESIS_DEATH_ENABLE = false');
    afterA = afterA.replace(/(function\s+bk_thesis_death\s*\([^)]*\))/, '$1\n  if true then return false end -- #1003 measurement: death arm disabled');
    eq(afterA.startsWith('function bk_thesis_death(h)\n  if true then return false end'), true);
    eq(afterA.includes('return h.x == 1'), true, '形态 A 的原谓词体保留（死代码），首行短路生效');
  });

  // 5) 汇总渲染：UNJUDGEABLE 包不产出 0% 也不产出误杀率。
  ok &= P('render/UNJUDGEABLE-不写0%', () => {
    const md = renderSummary(
      { generatedAtMs: 0, coreBin: 'x', feeModel: '', corpus: { corpus: 'c', sha256: '0'.repeat(64) }, seedBalance: '1000', maxOrderNotional: '12', singleRun: false },
      {
        flash_arb: { death: { netPnlUsd: 0, orders: 0 }, nodeath: { netPnlUsd: 0, orders: 0 }, pairing: { deathTokens: 0, misfire: 0, unmatched: 0 }, verdict: verdictFor({ death: { orders: 0, netPnlUsd: 0 }, nodeath: { orders: 0, netPnlUsd: 0 }, pairing: { deathTokens: 0, misfire: 0 } }) },
        spread_arb: { death: { netPnlUsd: -1, orders: 5 }, nodeath: { netPnlUsd: -2, orders: 5 }, pairing: { deathTokens: 10, misfire: 3, unmatched: 1 }, verdict: verdictFor({ death: { netPnlUsd: -1, orders: 5 }, nodeath: { netPnlUsd: -2, orders: 5 }, pairing: { deathTokens: 10, misfire: 3 } }) },
      },
      ['flash_arb', 'spread_arb'],
    );
    eq(md.includes('flash_arb | — | — | — | — | UNJUDGEABLE_ZERO_ORDERS'), true);
    eq(md.includes('30.00%'), true);
  });

  console.log(ok ? '\nself-test: ALL PASS' : '\nself-test: FAILURES');
  process.exit(ok ? 0 : 1);
}

// ── main ────────────────────────────────────────────────────────────────────

if (SELF_TEST) selfTest();

if (!existsSync(CORE_BIN)) {
  console.error(`core binary not found: ${CORE_BIN}`);
  console.error('先构建：cargo build --release -p blitzkrieg-launcher --bin blitzkrieg-core --locked（或用 --core-bin / BK_CORE_BIN）');
  process.exit(2);
}

console.log(`ab-thesis-death-1003 — issue #1003 thesis_death 误杀 A/B（包 ${PACKAGES.join('/')}，费率档 ${FEE_MODEL || 'kernel-default'}）`);
console.log(`run dir: ${OUT_DIR}`);
const corpusPath = discoverCorpus();
const pre = await preflightCorpus(corpusPath);
console.log(`corpus: ${corpusPath}`);
console.log(`  sha256 ${pre.sha256} PIN_OK`);
console.log(`  fix_resolutions --check: ${pre.fixResolutions.lines} lines, ${pre.fixResolutions.ok} ok, ${pre.fixResolutions.bad} bad`);

if (DRY_RUN) {
  console.log('\ndry-run: 计划的回放矩阵（不执行）——');
  let n = 0;
  for (const pkg of PACKAGES) {
    for (const mode of ['death', 'nodeath']) {
      const runs = SINGLE_RUN ? 1 : 2;
      n += runs;
      console.log(`  [${mode}] ${pkg} × ${runs} 跑`);
    }
  }
  console.log(`\n总计 ${n} 次回放。`);
  process.exit(0);
}

mkdirSync(OUT_DIR, { recursive: true });
mkdirSync(join(OUT_DIR, 'reports'), { recursive: true });

const runManifest = {
  generatedAtMs: Date.now(),
  issue: 1003,
  script: 'scripts/ab-thesis-death-1003.mjs',
  outDir: OUT_DIR,
  coreBin: CORE_BIN,
  coreBinSha256: sha256File(CORE_BIN),
  feeModel: FEE_MODEL,
  corpus: pre,
  packages: PACKAGES,
  seedBalance,
  maxOrderNotional,
  singleRun: SINGLE_RUN,
  arms: {},
};

const pkgRows = {};
const failures = [];
for (const pkg of PACKAGES) {
  console.log(`\n[${pkg}]`);
  const staged = { death: stage(pkg, 'death'), nodeath: stage(pkg, 'nodeath') };
  runManifest.arms[pkg] = {
    death: { source: staged.death.staging.source, manifestSha256: staged.death.staging.manifestSha256New },
    nodeath: { source: staged.nodeath.staging.source, mutations: staged.nodeath.staging.mutations, manifestSha256Old: staged.nodeath.staging.manifestSha256Old, manifestSha256New: staged.nodeath.staging.manifestSha256New },
  };
  const arms = {};
  for (const mode of ['death', 'nodeath']) {
    const s = staged[mode];
    const p1 = join(OUT_DIR, 'reports', `${pkg}.${mode}-1.json`);
    const r1 = await runOne(pkg, s.luaRoot, p1, `${pkg}/${mode} run1`);
    if (r1.invalid) {
      failures.push({ pkg, mode, reason: r1.invalid });
      arms[mode] = null;
      break;
    }
    let determinism = 'skipped';
    if (!SINGLE_RUN) {
      const p2 = join(OUT_DIR, 'reports', `${pkg}.${mode}-2.json`);
      const r2 = await runOne(pkg, s.luaRoot, p2, `${pkg}/${mode} run2`);
      if (r2.invalid) {
        failures.push({ pkg, mode, reason: r2.invalid });
        arms[mode] = null;
        break;
      }
      const h1 = sha256File(p1);
      const h2 = sha256File(p2);
      determinism = h1 === h2 ? 'match' : 'mismatch';
      if (determinism === 'mismatch') {
        console.error(`    DETERMINISM MISMATCH ${pkg}/${mode}: run1 ${h1.slice(0, 12)} vs run2 ${h2.slice(0, 12)} — 确定性 bug，实验无效`);
      }
    }
    arms[mode] = { ...r1, determinism };
    const contam = (r1.strategies ?? []).some((x) => x !== pkg);
    if (contam) console.error(`    CONTAMINATION ${pkg}/${mode}: 报告里出现了 ${pkg} 之外的策略名 ${(r1.strategies ?? []).join(', ')}`);
  }
  const row = {
    death: arms.death ? { closed: arms.death.closed, wins: arms.death.wins, orders: arms.death.orders, fills: arms.death.fills, netPnlUsd: arms.death.netPnlUsd, blocked: arms.death.blocked, errors: arms.death.errors, determinism: arms.death.determinism } : null,
    nodeath: arms.nodeath ? { closed: arms.nodeath.closed, wins: arms.nodeath.wins, orders: arms.nodeath.orders, fills: arms.nodeath.fills, netPnlUsd: arms.nodeath.netPnlUsd, blocked: arms.nodeath.blocked, errors: arms.nodeath.errors, determinism: arms.nodeath.determinism } : null,
    pairing: arms.death && arms.nodeath ? pair(arms.death, arms.nodeath) : { deathCloses: 0, deathTokens: 0, matched: 0, unmatched: 0, misfire: 0, misfireDetail: [] },
    contamination: arms.death ? (arms.death.strategies ?? []).some((x) => x !== pkg) || (arms.nodeath?.strategies ?? []).some((x) => x !== pkg) : false,
  };
  row.verdict = verdictFor(row);
  pkgRows[pkg] = row;
  console.log(`  death: closed=${row.death?.closed ?? '—'} orders=${row.death?.orders ?? '—'} net=${r3(row.death?.netPnlUsd)} | nodeath: closed=${row.nodeath?.closed ?? '—'} orders=${row.nodeath?.orders ?? '—'} net=${r3(row.nodeath?.netPnlUsd)}`);
  console.log(`  pairing: deathTokens=${row.pairing.deathTokens} misfire=${row.pairing.misfire} unmatched=${row.pairing.unmatched} rate=${row.verdict.misfireRatePct == null ? '—' : `${row.verdict.misfireRatePct.toFixed(2)}%`} | Δ=${row.verdict.judgeable ? r3(row.verdict.delta) : '—'} | ${row.verdict.judgeable ? row.verdict.verdict : row.verdict.why}`);
  // 逐包落 tradeLines 导出（审计可追溯，death 臂 ThesisDeathExit 明细）
  for (const mode of ['death', 'nodeath']) {
    if (arms[mode]) {
      const body = (arms[mode].tradeLines ?? []).map((l) => JSON.stringify({ pkg, mode, ...l })).join('\n');
      writeFileSync(join(OUT_DIR, 'reports', `${pkg}.${mode}.trades.jsonl`), body ? body + '\n' : '');
    }
  }
  rmSync(staged.death.root, { recursive: true, force: true });
  rmSync(staged.nodeath.root, { recursive: true, force: true });
}

runManifest.failures = failures;
runManifest.results = Object.fromEntries(Object.entries(pkgRows).map(([k, v]) => [k, {
  death: v.death,
  nodeath: v.nodeath,
  pairing: { ...v.pairing, misfireDetail: undefined },
  misfireDetail: v.pairing.misfireDetail,
  contamination: v.contamination,
  verdict: v.verdict,
}]));
writeFileSync(join(OUT_DIR, 'ab-thesis-death-1003.json'), JSON.stringify(runManifest, null, 2) + '\n');
writeFileSync(join(OUT_DIR, 'ab-thesis-death-1003.md'), renderSummary(runManifest, pkgRows, PACKAGES));
console.log(`\nreport: ${join(OUT_DIR, 'ab-thesis-death-1003.json')}`);
console.log(`report: ${join(OUT_DIR, 'ab-thesis-death-1003.md')}`);
console.log(renderSummary(runManifest, pkgRows, PACKAGES));

if (failures.length) {
  console.error(`INVALID: ${failures.length} 个回放被内核自检判无效（见 ab-thesis-death-1003.json .failures）—— 实验不成立，拒绝出结论。`);
  process.exitCode = 1;
}
const contaminated = Object.values(pkgRows).some((r) => r.contamination);
if (contaminated) {
  console.error('INVALID: 归因污染 —— 报告里出现了 enable 之外的策略名，实验判无效。');
  process.exitCode = 1;
}
const detMismatch = Object.values(pkgRows).some((r) => r.death?.determinism === 'mismatch' || r.nodeath?.determinism === 'mismatch');
if (detMismatch) {
  console.error('INVALID: 确定性破裂 —— 双跑报告哈希不一致，实验判无效。');
  process.exitCode = 1;
}
