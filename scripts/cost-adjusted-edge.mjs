#!/usr/bin/env node
/**
 * Cost-adjusted edge on the frozen one-hour windows (#865).
 *
 * This is the replacement the retired `scripts/backtest-check.mjs` does not
 * have: that gate replayed deleted cdylibs. This one does not pretend to be a
 * byte-identical kernel replay. It prices the ONLY committed order-book
 * corpus (`docs/reports/data/mean-reversion-gate/`, hashes pinned in
 * `scripts/lib/frozen-corpus.mjs`) under explicit fees and one tick of
 * slippage, then refuses to enable a strategy whose current code path is not
 * what that number measures.
 *
 * One observation = one (condition, round slot). The sample is the book pair
 * whose ask-sum is the median of the fresh pairs in that round (both asks
 * present, timestamps within 3500 ms). A completed merge returns $1.00. That
 * ignores naked-leg loss, which the windows cannot measure: they contain no
 * trade prints and no resolution rows.
 *
 *   taker_official_slip1   buy one tick through each ask, Polymarket crypto
 *                          taker fee 0.07*p*(1-p). Conservative cost ceiling.
 *                          Not pair_discount_arb v1.3's path (that package is
 *                          maker and quotes under the ask).
 *   taker_legacy_slip1     same prices, kernel default legacy_quadratic fee.
 *   maker_at_ask           fee 0, fill at the asks. A maker cannot rest AT
 *                          the ask (#464); this is a price ceiling, not a fill.
 *   maker_quote_minus_1    fee 0, fill at ask-1 tick. OPTIMISTIC: assumes
 *                          both legs fill at the v1.3 quote. Cannot GO.
 *
 * Usage:
 *   node scripts/cost-adjusted-edge.mjs --self-test
 *   node scripts/cost-adjusted-edge.mjs --write
 *   node scripts/cost-adjusted-edge.mjs --check
 *   node scripts/cost-adjusted-edge.mjs --dig [--top N]
 *
 * --dig (issue 889, 挖掘档): prints the candidate pool as JSON. Each pricing
 * mode is one arm; the pool is sorted by EV (net per round) descending and
 * carries per-arm n / EV / PF / CI. This is mining semantics: a pool to dig
 * through, with NO GO/NO-GO verdicts — verdicts stay with the adoption gate
 * (--check) alone, per the 2026-11-02 ruling (挖掘宽、采用严). The pool reads
 * the same frozen corpus; `top` defaults to all arms.
 */
import { readFileSync, writeFileSync, rmSync, createReadStream } from 'fs';
import { createInterface } from 'readline';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const JSON_PATH = join(ROOT, 'docs', 'reports', 'cost-adjusted-edge-72h.json');
const MD_PATH = join(ROOT, 'docs', 'reports', 'cost-adjusted-edge-72h.md');
const STALE_MS = 3500;
const MICRO = 1_000_000n;
const TICK = 10_000n; // 0.01
const MAX_PX = 990_000n; // 0.99
const MIN_PX = 10_000n; // 0.01
const ONE = MICRO;
const DIG_Z = 1.959963984540054; // two-sided 95% normal / Wilson deviate

const argv = process.argv.slice(2);
const selfTest = argv.includes('--self-test');
const write = argv.includes('--write');
const dig = argv.includes('--dig');
const topIdx = argv.indexOf('--top');
const topArg = topIdx === -1 ? Infinity : Number(argv[topIdx + 1]);
if (topIdx !== -1 && (!Number.isInteger(topArg) || topArg < 1)) {
  console.error('cost-adjusted-edge: --top needs a positive integer');
  process.exit(1);
}
const check = !selfTest && !write && !dig;

function parseMicro(raw) {
  const s = String(raw).trim();
  if (!s || s === 'NaN') return null;
  const neg = s.startsWith('-');
  const t = neg ? s.slice(1) : s;
  if (!/^\d+(\.\d+)?$/.test(t)) return null;
  const [w, f = ''] = t.split('.');
  const frac = (f + '000000').slice(0, 6);
  const v = BigInt(w) * MICRO + BigInt(frac);
  return neg ? -v : v;
}

function formatMicro(n) {
  const neg = n < 0n;
  const v = neg ? -n : n;
  const whole = v / MICRO;
  let frac = (v % MICRO).toString().padStart(6, '0').replace(/0+$/, '');
  const body = frac.length ? `${whole}.${frac}` : `${whole}`;
  return neg ? `-${body}` : body;
}

function divRound(num, den) {
  return (num + den / 2n) / den;
}

/** Official crypto taker: 0.07 * p * (1-p), in micro-USD per share. */
function officialFee(px) {
  return divRound(7n * px * (MICRO - px), 100n * MICRO);
}

/** Kernel default legacy_quadratic: 0.125 * (p*(1-p))^2. */
function legacyFee(px) {
  const spread = px * (MICRO - px);
  return divRound(spread * spread * 125_000n, 10n ** 24n);
}

function bestAsk(levels) {
  if (!Array.isArray(levels)) return null;
  let best = null;
  for (const level of levels) {
    if (!Array.isArray(level) || level.length < 2) continue;
    const px = parseMicro(level[0]);
    const sz = parseMicro(level[1]);
    if (px === null || sz === null || sz <= 0n) continue;
    if (px <= 0n || px >= MICRO) continue;
    if (best === null || px < best) best = px;
  }
  return best;
}

/**
 * Price one share of each leg. Returns null when a slipped quote would leave
 * the (0.01, 0.99] grid — that round is unfillable under this model, not a loss.
 */
function leg(px, mode) {
  if (mode === 'taker_official_slip1' || mode === 'taker_legacy_slip1') {
    const q = px + TICK;
    if (q > MAX_PX) return null;
    return { px: q, fee: mode === 'taker_official_slip1' ? officialFee(q) : legacyFee(q) };
  }
  if (mode === 'maker_quote_minus_1') {
    const q = px - TICK;
    if (q < MIN_PX) return null;
    return { px: q, fee: 0n };
  }
  if (mode === 'maker_at_ask') return { px, fee: 0n };
  throw new Error(`unknown mode ${mode}`);
}

const MODES = [
  'taker_official_slip1',
  'taker_legacy_slip1',
  'maker_at_ask',
  'maker_quote_minus_1',
];

const OPTIMISTIC = new Set(['maker_quote_minus_1', 'maker_at_ask']);

function pnlOf(up, down, mode) {
  const a = leg(up, mode);
  const b = leg(down, mode);
  if (!a || !b) return null;
  return ONE - (a.px + a.fee + b.px + b.fee);
}

function summarize(pnls) {
  let profit = 0n;
  let loss = 0n;
  let net = 0n;
  let equity = 0n;
  let peak = 0n;
  let maxDd = 0n;
  let wins = 0;
  let losses = 0;
  for (const p of pnls) {
    net += p;
    equity += p;
    if (equity > peak) peak = equity;
    const dd = peak - equity;
    if (dd > maxDd) maxDd = dd;
    if (p > 0n) {
      profit += p;
      wins += 1;
    } else if (p < 0n) {
      loss += -p;
      losses += 1;
    }
  }
  let pf;
  if (loss === 0n && profit > 0n) pf = 'inf';
  else if (loss === 0n) pf = 'na';
  else pf = formatMicro(divRound(profit * MICRO, loss));
  return {
    n: pnls.length,
    wins,
    losses,
    netUsd: formatMicro(net),
    grossProfitUsd: formatMicro(profit),
    grossLossUsd: formatMicro(loss),
    profitFactor: pf,
    maxDrawdownUsd: formatMicro(maxDd),
  };
}

function pfNumber(pf) {
  if (pf === 'inf') return Infinity;
  if (pf === 'na') return 0;
  return Number(pf);
}

/**
 * A scenario can be GO only when it is not an optimistic fill assumption,
 * the sample is at least 30 rounds, PF >= 1.1 and max drawdown <= $5
 * (the shipped daily cap). No current strategy's code path is this scenario:
 * see `decisions`.
 */
function scenarioPasses(summary, mode) {
  if (OPTIMISTIC.has(mode)) return false;
  if (summary.n < 30) return false;
  if (pfNumber(summary.profitFactor) < 1.1) return false;
  if (Number(summary.maxDrawdownUsd) > 5) return false;
  return true;
}

/**
 * One mining arm (issue 889): n / EV / PF / CI for one pricing mode. EV is
 * the integer-rounded mean per-round pnl; the CI is the conservative
 * normal-approx lower bound EV − z·s/√n (sample sd, n<2 → null) alongside the
 * win-rate Wilson lower bound. Money stays in the report's micro-USD string
 * convention; ranks sort on the exact integer mean.
 */
function digArm(mode, pnls) {
  const s = summarize(pnls);
  const n = pnls.length;
  let sum = 0n;
  for (const p of pnls) sum += p;
  const evMicro = divRound(sum, BigInt(n));
  let evLb = null;
  if (n >= 2) {
    const mean = Number(sum) / n;
    let s2 = 0;
    for (const p of pnls) s2 += (Number(p) - mean) ** 2;
    evLb = mean - (DIG_Z * Math.sqrt(s2 / (n - 1))) / Math.sqrt(n);
  }
  const wr = s.wins / n;
  const denom = 1 + (DIG_Z * DIG_Z) / n;
  const centre = wr + (DIG_Z * DIG_Z) / (2 * n);
  const rad = DIG_Z * Math.sqrt((wr * (1 - wr)) / n + (DIG_Z * DIG_Z) / (4 * n * n));
  const wrWilsonLb = Math.max(0, (centre - rad) / denom);
  return {
    mode,
    n,
    evMicro,
    evUsd: formatMicro(evMicro),
    evLowerUsd: evLb === null ? null : formatMicro(BigInt(Math.round(evLb))),
    wrWilsonLb,
    profitFactor: s.profitFactor,
  };
}

/** The --dig pool: every mode as an arm, EV-descending, top-N. No verdicts. */
function digPool(rounds, top) {
  const pool = [];
  for (const mode of MODES) {
    const pnls = [];
    for (const r of rounds) {
      const p = pnlOf(r.up, r.down, mode);
      if (p !== null) pnls.push(p);
    }
    if (pnls.length) pool.push(digArm(mode, pnls));
  }
  pool.sort((a, b) => (b.evMicro > a.evMicro ? 1 : b.evMicro < a.evMicro ? -1 : a.mode.localeCompare(b.mode)));
  return pool.slice(0, top).map(({ evMicro, ...rest }) => rest);
}

function readManifest(name) {
  const p = join(ROOT, 'user_layer', 'strategies_lua', name, 'manifest.json');
  const m = JSON.parse(readFileSync(p, 'utf8'));
  return { name: m.name, version: m.version };
}

function decisions(pairManifest, singleManifest) {
  return {
    pair_discount_arb: {
      version: pairManifest.version,
      decision: 'NO-GO',
      reason:
        'v1.3.0 是 entry_maker，报价在 ask 之下。这些窗口没有成交打印，测不到 maker 成交率，也测不到裸腿归零。taker_official_slip1 是一条当前版本根本不走的路径的成本上限。docs/strategy-review-0.4.1.md 的 PF 早于 v1.3。策略自己的折扣测试看的是 ask 之和（再加费）是否低于 0.985；本语料的命中轮数见文末，中位书上并不开火。',
    },
    single_leg_pair: {
      version: singleManifest.version,
      decision: 'NO-GO',
      reason:
        '方向性单腿要有结算或 resolution 才能打分。这些窗口既没有 resolution，也没有 spot。0.4.1 的 PF 不是 1.1.0 在这份语料上的重跑。',
    },
    lua_momentum: {
      version: readManifest('lua_momentum').version,
      decision: 'NO-GO',
      reason: '0.4.1 重测约 -$17.6k，PF 0.433。不重新启用。',
    },
    spread_arb: {
      version: readManifest('spread_arb').version,
      decision: 'NO-GO',
      reason: '0.4.1 重测约 -$475，胜率约 10%。不重新启用。',
    },
    mad_dog: {
      version: readManifest('mad_dog').version,
      decision: 'NO-GO',
      reason: '带 spot 的样本是负的。这份语料没有 spot。不重新启用。',
    },
    market_maker: {
      version: readManifest('market_maker').version,
      decision: 'NO-GO',
      reason: '没有扣费后为正的样本。不重新启用。',
    },
  };
}

function renderMarkdown(report) {
  const lines = [];
  lines.push('# 72 小时启用集：扣费与 1 tick 滑点后的证据');
  lines.push('');
  lines.push(`- 日期：${report.generatedOn}`);
  lines.push(`- 问题：#865`);
  lines.push(`- 语料：\`docs/reports/data/mean-reversion-gate/\` 四个一小时窗口（sha256 与 \`scripts/lib/frozen-corpus.mjs\` 一致）`);
  lines.push(`- 策略版本：pair_discount_arb ${report.decisions.pair_discount_arb.version}，single_leg_pair ${report.decisions.single_leg_pair.version}`);
  lines.push(`- 脚本：\`node scripts/cost-adjusted-edge.mjs --check\``);
  lines.push('');
  lines.push('## 结论');
  lines.push('');
  lines.push('**NO-GO。** 启用集为空。没有任何当前策略在「非乐观成交假设 + 手续费 + 1 tick」下被证明 PF ≥ 1.1。');
  lines.push('');
  lines.push('这不是内核 `--backtest` 的逐笔回放。被删的 `scripts/backtest-check.mjs` 回放的是已经不在树里的 cdylib。本报告只给冻结窗口上的成本模型，并拒绝把乐观的「两条腿都在报价上成交」写成已实现优势。窗口里没有成交打印，测不到 maker 成交率，也测不到裸腿归零。');
  lines.push('');
  lines.push('| 策略 | 版本 | 判定 |');
  lines.push('| --- | --- | --- |');
  for (const [name, d] of Object.entries(report.decisions)) {
    lines.push(`| \`${name}\` | ${d.version} | ${d.decision} |`);
  }
  lines.push('');
  lines.push('## 每个二元轮的合并盈亏（1 手，合并拿回 $1）');
  lines.push('');
  lines.push(`样本：${report.rounds} 个 (condition, roundSlot)。两边 ask 都在，且时间戳相差不超过 ${report.staleMs} ms。每个轮取 ask 之和的中位数那一对。滑点把买价推离中间：taker 加 1 tick，maker 报价减 1 tick。超出 0.01–0.99 网格的一轮记为不可成交，不记成亏损。`);
  lines.push('');
  lines.push('| 模型 | 成交轮 | 胜 / 负 | 净盈亏 USD | PF | 最大回撤 USD | 能否单独 GO |');
  lines.push('| --- | ---: | ---: | ---: | ---: | ---: | --- |');
  for (const mode of MODES) {
    const s = report.scenarios[mode];
    lines.push(
      `| \`${mode}\` | ${s.n} | ${s.wins} / ${s.losses} | ${s.netUsd} | ${s.profitFactor} | ${s.maxDrawdownUsd} | ${s.passesGate ? '是' : '否'} |`,
    );
  }
  lines.push('');
  lines.push('`passesGate` 只表示该模型自己达到了 PF ≥ 1.1、回撤 ≤ $5、样本 ≥ 30，且模型不是乐观成交。它不会把策略放进启用集。maker 两行被标成乐观，门禁直接否决。');
  lines.push('');
  lines.push('## 为什么不启用 pair_discount_arb 1.3.0');
  lines.push('');
  lines.push(report.decisions.pair_discount_arb.reason);
  lines.push('');
  lines.push('## 为什么不启用 single_leg_pair 1.1.0');
  lines.push('');
  lines.push(report.decisions.single_leg_pair.reason);
  lines.push('');
  lines.push('## 分窗口样本数');
  lines.push('');
  lines.push('| 窗口 | 轮数 |');
  lines.push('| --- | ---: |');
  for (const w of report.perWindow) {
    lines.push(`| \`${w.name}\` | ${w.rounds} |`);
  }
  lines.push('');
  lines.push('折扣测试（两边 ask 之和 < 0.985，尚未加费）命中的轮数：' + report.discountUnder985 + '。');
  lines.push('');
  return lines.join('\n');
}

async function scanWindow(name) {
  const spec = WINDOWS.find((w) => w.name === name);
  const { path, dir } = materialize(ROOT, spec);
  const asks = new Map();
  const sideOf = new Map();
  const buckets = new Map();
  try {
    const rl = createInterface({ input: createReadStream(path), crlfDelay: Infinity });
    for await (const line of rl) {
      if (!line) continue;
      const ev = JSON.parse(line);
      if (ev.k === 'round' && Array.isArray(ev.m)) {
        for (const m of ev.m) {
          const slot = m.roundSlot;
          const condition = m.conditionId;
          if (!condition || slot === undefined || !m.upTokenId || !m.downTokenId) continue;
          const key = `${condition}|${slot}`;
          sideOf.set(m.upTokenId, { key, side: 'up', other: m.downTokenId });
          sideOf.set(m.downTokenId, { key, side: 'down', other: m.upTokenId });
        }
        continue;
      }
      if (ev.k !== 'book') continue;
      const info = sideOf.get(ev.t);
      if (!info) continue;
      const px = bestAsk(ev.a);
      if (px === null) continue;
      const mine = { px, at: ev.at };
      asks.set(ev.t, mine);
      const other = asks.get(info.other);
      if (!other) continue;
      if (Math.abs(mine.at - other.at) > STALE_MS) continue;
      const up = info.side === 'up' ? mine : other;
      const down = info.side === 'down' ? mine : other;
      let bucket = buckets.get(info.key);
      if (!bucket) {
        bucket = [];
        buckets.set(info.key, bucket);
      }
      bucket.push({ up: up.px, down: down.px, at: Math.max(up.at, down.at) });
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  const rounds = [];
  for (const [key, samples] of buckets) {
    samples.sort((a, b) => {
      const sa = a.up + a.down;
      const sb = b.up + b.down;
      if (sa < sb) return -1;
      if (sa > sb) return 1;
      return a.at - b.at;
    });
    const mid = samples[(samples.length - 1) >> 1];
    rounds.push({ key, ...mid });
  }
  rounds.sort((a, b) => a.at - b.at || (a.key < b.key ? -1 : 1));
  return rounds;
}

async function collectRounds() {
  const perWindow = [];
  const rounds = [];
  for (const w of WINDOWS) {
    const found = await scanWindow(w.name);
    perWindow.push({ name: w.name, rounds: found.length });
    for (const r of found) rounds.push({ window: w.name, ...r });
  }
  rounds.sort((a, b) => a.at - b.at || (a.key < b.key ? -1 : 1));
  return { perWindow, rounds };
}

async function buildReport() {
  const { perWindow, rounds } = await collectRounds();
  const scenarios = {};
  for (const mode of MODES) {
    const pnls = [];
    for (const r of rounds) {
      const p = pnlOf(r.up, r.down, mode);
      if (p !== null) pnls.push(p);
    }
    const summary = summarize(pnls);
    scenarios[mode] = { ...summary, optimistic: OPTIMISTIC.has(mode), passesGate: scenarioPasses(summary, mode) };
  }
  let discountUnder985 = 0;
  for (const r of rounds) {
    if (r.up + r.down < 985_000n) discountUnder985 += 1;
  }
  const pair = readManifest('pair_discount_arb');
  const single = readManifest('single_leg_pair');
  const report = {
    generatedOn: '2026-11-02',
    issue: 865,
    staleMs: STALE_MS,
    rounds: rounds.length,
    discountUnder985,
    perWindow,
    scenarios,
    decisions: decisions(pair, single),
    allow: [],
  };
  return report;
}

function stable(report) {
  return JSON.stringify(report, null, 2) + '\n';
}

function assertEq(name, got, want) {
  if (got !== want) {
    console.error(`SELF-TEST FAIL ${name}: got ${got} want ${want}`);
    process.exit(1);
  }
}

function atMost(name, got, eps) {
  if (!(got <= eps)) {
    console.error(`SELF-TEST FAIL ${name}: off by ${got} (±${eps})`);
    process.exit(1);
  }
}

function runSelfTest() {
  assertEq('official 0.40', officialFee(400_000n), 16_800n);
  assertEq('legacy 0.40', legacyFee(400_000n), 7_200n);
  assertEq('official 0.41', officialFee(410_000n), 16_933n);
  const slip = pnlOf(400_000n, 400_000n, 'taker_official_slip1');
  // 0.41+0.41+0.016933+0.016933 = 0.853866; pnl = 1 - that = 0.146134
  assertEq('pair pnl', slip, 146_134n);
  assertEq('unfillable', pnlOf(990_000n, 400_000n, 'taker_official_slip1'), null);
  const s = summarize([2_000_000n, -1_000_000n]);
  assertEq('pf', s.profitFactor, '2');
  assertEq('dd', s.maxDrawdownUsd, '1');
  const optimistic = summarize([1_000_000n, 1_000_000n, 1_000_000n]);
  // n=3 < 30, and the mode is optimistic
  if (scenarioPasses({ ...optimistic, n: 40, profitFactor: 'inf', maxDrawdownUsd: '0' }, 'maker_quote_minus_1')) {
    console.error('SELF-TEST FAIL: optimistic maker must not pass');
    process.exit(1);
  }
  if (!scenarioPasses(
    { n: 40, profitFactor: '1.2', maxDrawdownUsd: '4' },
    'taker_official_slip1',
  )) {
    console.error('SELF-TEST FAIL: a real taker sample at PF 1.2 must pass the scenario gate');
    process.exit(1);
  }
  const d = decisions(
    { version: '1.3.0' },
    { version: '1.1.0' },
  );
  if (d.pair_discount_arb.decision !== 'NO-GO' || d.single_leg_pair.decision !== 'NO-GO') {
    console.error('SELF-TEST FAIL: current packages must stay NO-GO without fill evidence');
    process.exit(1);
  }
  // --dig (issue 889): arms carry n / EV / CI and rank by exact EV; no verdicts.
  const mixed = digArm('mixed', [2_000_000n, -1_000_000n]);
  assertEq('dig n', mixed.n, 2);
  assertEq('dig ev', mixed.evUsd, '0.5');
  assertEq('dig pf', mixed.profitFactor, '2');
  // mean 0.5, sd 1.5e6 micro, se 1.5e6/sqrt(2): lb = 0.5 - 2.939945... = -2.439946
  assertEq('dig ev lower bound', mixed.evLowerUsd, '-2.439946');
  atMost('dig wilson lower bound', Math.abs(mixed.wrWilsonLb - 0.0945327), 1e-4);
  const clean = digArm('clean', [1_000_000n, 1_000_000n, 1_000_000n]);
  assertEq('dig all-win ev', clean.evUsd, '1');
  assertEq('dig all-win ev lb', clean.evLowerUsd, '1');
  assertEq('dig all-win pf', clean.profitFactor, 'inf');
  atMost('dig all-win wilson lb', Math.abs(clean.wrWilsonLb - 0.438531), 1e-4);
  assertEq('dig n=1 has no ev lb', digArm('single', [500_000n]).evLowerUsd, null);
  const pool = digPool([{ up: 400_000n, down: 400_000n }], 10);
  assertEq(
    'dig pool ranks by ev, best first',
    pool.map((a) => a.mode).join(','),
    'maker_quote_minus_1,maker_at_ask,taker_legacy_slip1,taker_official_slip1',
  );
  assertEq('dig pool drops the sort key', 'evMicro' in pool[0], false);
  assertEq('dig pool top', digPool([{ up: 400_000n, down: 400_000n }], 2).length, 2);
  console.log('cost-adjusted-edge self-test: PASS');
}

async function main() {
  if (selfTest) {
    runSelfTest();
    if (!check && !write && !dig) return;
  }
  if (dig) {
    const { rounds } = await collectRounds();
    const pool = digPool(rounds, topArg);
    console.log(
      JSON.stringify(
        {
          semantics: 'mining',
          note: '挖掘档候选池：按 EV 降序，无 GO/NO-GO 判决（判决只属于 --check 的采用语义）',
          pool,
        },
        null,
        2,
      ) + '\n',
    );
    return;
  }
  const report = await buildReport();
  const body = stable(report);
  const md = renderMarkdown(report);
  if (write) {
    writeFileSync(JSON_PATH, body);
    writeFileSync(MD_PATH, md);
    console.log(`wrote ${JSON_PATH}`);
    console.log(`wrote ${MD_PATH}`);
    console.log(`rounds=${report.rounds} allow=${JSON.stringify(report.allow)}`);
    for (const mode of MODES) {
      const s = report.scenarios[mode];
      console.log(`${mode} n=${s.n} net=${s.netUsd} pf=${s.profitFactor} dd=${s.maxDrawdownUsd}`);
    }
    return;
  }
  if (check) {
    const disk = readFileSync(JSON_PATH, 'utf8');
    if (disk !== body) {
      console.error('cost-adjusted-edge: committed JSON != fresh scan. Run --write and review the diff.');
      process.exit(1);
    }
    const diskMd = readFileSync(MD_PATH, 'utf8');
    if (diskMd !== md) {
      console.error('cost-adjusted-edge: committed markdown != fresh scan. Run --write and review the diff.');
      process.exit(1);
    }
    if (report.allow.length !== 0) {
      console.error('cost-adjusted-edge: allow list must stay empty until a non-optimistic rerun says GO');
      process.exit(1);
    }
    console.log(`cost-adjusted-edge check: PASS (${report.rounds} rounds, allow empty)`);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
