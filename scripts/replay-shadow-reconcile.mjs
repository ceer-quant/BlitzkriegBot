#!/usr/bin/env node
/**
 * #862 — 影子盘 vs 回放 逐笔成交价对账（Up/Down/日/regime 分桶）。
 *
 * 输入：
 *   --replay <out.json>   一次 --backtest 的报告（可重复传多份，多资产/多窗口）。
 *                         报告的 tradeLines 携带 conditionId/direction/entryPrice/regime。
 *   --shadow <trades.jsonl> 影子台账（trades-live.jsonl 形状：conditionId/direction/
 *                         entryPrice/entryTime/accountId）。全程只读。
 *   --from-ms/--to-ms     影子侧冻结切片窗口（epoch ms，可省）。
 *   --out <dir>           输出目录：reconcile-report.json + manifest（sha256）。
 *
 * 配对：仅同 venue 同 instrument —— key = conditionId + direction。跨 venue 的
 * 同 slot（polymarket 0x… vs kalshi KX…）不配对：那是两个市场的价差（venue
 * basis），不是回放执行模型的对账对象。同 key 多笔按时间序（影子按 entryTime、
 * 回放按 trade id）逐一配对。
 *
 * 判据（issue #862）：配对笔的平均 |回放入场价 − 影子入场价| ≤ 1¢ → PASS；
 * 超差 → FAIL（回测不可用）。不许调参凑差：脚本只算差值并如实输出分布，
 * 不提供任何容差以外的"修正"。
 *
 * 证据等级：本工具的输出永远是 synthetic-replay 证据 —— 回放是合成执行，
 * 不得作为真实盘证据引用；报告内强制携带该标注（issue #862 红线）。
 *
 * 用法示例：
 *   node scripts/replay-shadow-reconcile.mjs \
 *     --replay out-BTC.json --replay out-ETH.json \
 *     --shadow data/trades/trades-live.jsonl \
 *     --from-ms 1793109351349 --to-ms 1793117104034 \
 *     --out /tmp/reconcile-862
 */

import { createHash } from 'crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync, statSync } from 'fs';
import { join, resolve, basename } from 'path';

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const flags = (name) => {
  const out = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] !== name) continue;
    // Variadic: every consecutive value after the flag belongs to it, so
    // `--replay a b c` and `--replay a --replay b` are the same.
    for (let j = i + 1; j < args.length && !args[j].startsWith('--'); j++) out.push(args[j]);
  }
  return out;
};

const replayPaths = flags('--replay');
const shadowPath = flag('--shadow');
const fromMs = flag('--from-ms') ? Number(flag('--from-ms')) : null;
const toMs = flag('--to-ms') ? Number(flag('--to-ms')) : null;
const outDir = resolve(flag('--out') || '/tmp/reconcile-862');

if (!replayPaths.length || !shadowPath) {
  console.error('usage: --replay <report.json>... --shadow <trades.jsonl> [--from-ms N] [--to-ms N] --out <dir>');
  process.exit(2);
}

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');
const cents = (p) => Number(p) * 100;

// ── 影子侧（只读 + 冻结切片） ────────────────────────────────────────────────
const shadowRaw = readFileSync(shadowPath, 'utf8');
const shadowAll = shadowRaw.split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
const shadow = shadowAll.filter((r) => {
  if (r.entryTime == null || r.conditionId == null || r.direction == null) return false;
  if (fromMs != null && r.entryTime < fromMs) return false;
  if (toMs != null && r.entryTime > toMs) return false;
  return true;
});

// ── 回放侧（报告 tradeLines） ────────────────────────────────────────────────
const replays = replayPaths.map((p) => {
  const raw = readFileSync(p, 'utf8');
  const report = JSON.parse(raw);
  const lines = report.tradeLines || [];
  return { path: p, report, lines };
});
const replayTrades = replays.flatMap((r) =>
  r.lines.map((t) => ({ ...t, _source: basename(r.path) }))
);

// ── 配对：conditionId + direction，双侧按序配 ────────────────────────────────
const keyOf = (cond, dir) => `${cond}|${dir}`;
// Natural order on trade ids ("hft-2" before "hft-10"): replay tradeLines carry
// no timestamps, so within one (condition, direction) key the pairing order is
// the id order — deterministic, and the overwhelmingly common case is 1:1.
const natural = (a, b) => String(a.id).localeCompare(String(b.id), undefined, { numeric: true });
const group = (rows, tsOf) => {
  const m = new Map();
  for (const r of rows) {
    const k = keyOf(r.conditionId, r.direction);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(r);
  }
  for (const v of m.values()) v.sort((a, b) => (tsOf(a) - tsOf(b)) || natural(a, b));
  return m;
};
const shadowBy = group(shadow, (r) => r.entryTime);
const replayBy = group(replayTrades, () => 0);

const pairs = [];
const shadowOnly = [];
for (const [k, rows] of shadowBy) {
  const rp = replayBy.get(k) || [];
  const n = Math.min(rows.length, rp.length);
  for (let i = 0; i < n; i++) {
    pairs.push({
      key: k,
      shadow: {
        id: rows[i].id, asset: rows[i].asset, entryPrice: rows[i].entryPrice,
        entryTime: rows[i].entryTime, accountId: rows[i].accountId, venue: rows[i].venue,
      },
      replay: {
        id: rp[i].id, asset: rp[i].asset, entryPrice: rp[i].entryPrice,
        regime: rp[i].regime ?? 'none', source: rp[i]._source,
      },
      diffCents: +(cents(rp[i].entryPrice) - cents(rows[i].entryPrice)).toFixed(4),
      absDiffCents: +Math.abs(cents(rp[i].entryPrice) - cents(rows[i].entryPrice)).toFixed(4),
    });
  }
  for (let i = n; i < rows.length; i++) shadowOnly.push({ key: k, id: rows[i].id, asset: rows[i].asset, entryPrice: rows[i].entryPrice });
}
const replayOnly = [];
for (const [k, rows] of replayBy) {
  const n = (shadowBy.get(k) || []).length;
  for (let i = n; i < rows.length; i++) replayOnly.push({ key: k, id: rows[i].id, asset: rows[i].asset, entryPrice: rows[i].entryPrice, source: rows[i]._source });
}

// ── 分桶：Up/Down、日（UTC）、regime；指标：均价差 / 平均绝对差 / 最大绝对差 ──
const bucketOf = (p) => ({
  direction: p.key.split('|')[1],
  day: new Date(p.shadow.entryTime).toISOString().slice(0, 10),
  regime: p.replay.regime,
});
const stats = (rows) => {
  if (!rows.length) return { n: 0, avgDiffCents: null, avgAbsDiffCents: null, maxAbsDiffCents: null };
  const avg = (xs) => +(xs.reduce((a, b) => a + b, 0) / xs.length).toFixed(4);
  return {
    n: rows.length,
    avgDiffCents: avg(rows.map((p) => p.diffCents)),
    avgAbsDiffCents: avg(rows.map((p) => p.absDiffCents)),
    maxAbsDiffCents: +Math.max(...rows.map((p) => p.absDiffCents)).toFixed(4),
  };
};
const buckets = { direction: {}, day: {}, regime: {} };
for (const p of pairs) {
  const b = bucketOf(p);
  for (const [dim, val] of Object.entries(b)) {
    const key = String(val);
    (buckets[dim][key] ||= []).push(p);
  }
}
const bucketReport = {};
for (const [dim, groups] of Object.entries(buckets)) {
  bucketReport[dim] = {};
  for (const [val, rows] of Object.entries(groups).sort()) bucketReport[dim][val] = stats(rows);
}

const overall = stats(pairs);
const TOLERANCE_CENTS = 1.0;
let verdict;
if (!pairs.length) {
  verdict = 'NO_MATCHES';
} else if (overall.avgAbsDiffCents <= TOLERANCE_CENTS) {
  verdict = 'PASS';
} else {
  verdict = 'FAIL';
}

const report = {
  issue: 862,
  evidenceClass: 'synthetic-replay-not-real-evidence',
  evidenceNote:
    '回放是合成执行（sim fill model），本对账输出不得作为真实盘证据引用；判据仅回答「回测执行模型与影子盘的成交价是否一致（≤1¢）」。',
  acceptance: {
    metric: 'avg |replay entry − shadow entry| (cents), 同 condition 同 direction 配对',
    toleranceCents: TOLERANCE_CENTS,
    verdict,
    verdictNote:
      verdict === 'PASS'
        ? '平均成交价差 ≤1¢：该语料上 l2-delay 回放的成交价与影子盘对齐。'
        : verdict === 'FAIL'
          ? '平均成交价差 >1¢：按 issue #862 判「回测不可用」（如实输出，不调参凑差）。'
          : '同 condition 配对为 0：无法认证成交价一致性（回放与影子未成交同一 instrument）。先核对语料 family 切片与窗口。',
  },
  shadow: {
    path: resolve(shadowPath),
    slice: { fromMs, toMs, note: '冻结切片：台账是活动文件，聚合仅对本切片负责' },
    rowsTotal: shadowAll.length,
    rowsInSlice: shadow.length,
  },
  replays: replays.map((r) => ({
    path: resolve(r.path),
    fillModel: r.report.fillModel,
    tradesSha256: r.report.tradesSha256,
    tradeLines: r.lines.length,
  })),
  matched: pairs,
  unmatched: { shadowOnly, replayOnly },
  overall,
  buckets: bucketReport,
  generatedAt: new Date().toISOString(),
};

// ── 输出 + sha256 清单 ───────────────────────────────────────────────────────
mkdirSync(outDir, { recursive: true });
const reportPath = join(outDir, 'reconcile-report.json');
writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
const manifest = {
  generatedAt: report.generatedAt,
  inputs: {},
  outputs: {},
};
for (const p of [shadowPath, ...replayPaths]) {
  const rp = resolve(p);
  manifest.inputs[rp] = { sha256: sha256(readFileSync(rp)), bytes: statSync(rp).size };
}
for (const f of ['reconcile-report.json']) {
  const fp = join(outDir, f);
  manifest.outputs[f] = { sha256: sha256(readFileSync(fp)), bytes: statSync(fp).size };
}
writeFileSync(join(outDir, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');

// ── 人读摘要 ────────────────────────────────────────────────────────────────
const fmt = (s) => (s.avgAbsDiffCents == null ? '—' : `${s.avgDiffCents >= 0 ? '+' : ''}${s.avgDiffCents}¢ avg / |${s.avgAbsDiffCents}¢| max ${s.maxAbsDiffCents}¢ (n=${s.n})`);
console.log(`#862 影子盘 vs 回放 逐笔对账 — verdict: ${verdict}`);
console.log(`  证据等级: synthetic-replay-not-real-evidence（回放≠真实盘证据）`);
console.log(`  影子切片: ${shadow.length}/${shadowAll.length} 笔 in [${fromMs ?? '…'}, ${toMs ?? '…'}]`);
console.log(`  配对: ${pairs.length}（影子独有 ${shadowOnly.length}，回放独有 ${replayOnly.length}）`);
console.log(`  总体: ${fmt(overall)}  判据 ≤${TOLERANCE_CENTS}¢`);
for (const [dim, groups] of Object.entries(bucketReport)) {
  for (const [val, s] of Object.entries(groups)) console.log(`  ${dim}/${val}: ${fmt(s)}`);
}
console.log(`  报告: ${reportPath}（含 sha256 清单 manifest.json）`);
process.exit(verdict === 'FAIL' ? 1 : 0);
