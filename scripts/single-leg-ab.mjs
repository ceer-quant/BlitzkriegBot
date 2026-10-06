#!/usr/bin/env node
/**
 * #387 — single-leg round participation A/B: the off/on pair through ONE
 * binary, ONE corpus, ONE cwd, then the overlay verdict.
 *
 * The three metrics the issue's acceptance names (coverage / ROI / PF) plus
 * closed trades, computed by the repo's own calibers:
 *   - OFF run : pair_discount_arb only  (the 28% pair-ceiling baseline)
 *   - ON run  : pair_discount_arb + single_leg_pair (same binary, same cwd)
 *   - overlay : scripts/almach-overlay.mjs vs the @almach /activity ledger,
 *               once per report — its `signals.coverageOfActualPairsPct`,
 *               `roi.*FinalPct` and the report's own `trades.profitFactor`
 *               are the acceptance numbers;
 *   - odds    : the single-leg settlement odds table by VWAP band, derived
 *               from the ledger itself (the honesty row the issue asks for),
 *               and the ON report's per-strategy split (tradeLines carry the
 *               strategy name) — what the new strategy actually added.
 *
 * Orchestration only: every number comes from the repo's analyzers or the
 * kernel's own report. Zero new dependencies (node:child_process, node:fs).
 *
 * Usage:
 *   node scripts/single-leg-ab.mjs --binary <blitzkrieg-core> \
 *     --corpus <corpus.jsonl> --ledger <activity-full.jsonl> \
 *     --start 2026-08-05 --end 2026-09-30 --dur 15m \
 *     [--worktree <repo root>] [--out-dir <dir>] [--skip-off] [--skip-replay]
 *
 *   --skip-replay  reuse existing ab-off.json/ab-on.json in --out-dir
 *                  (they were produced by the same binary+corpus before).
 */
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};

const binary = flag('--binary');
const corpus = flag('--corpus');
const ledger = flag('--ledger');
const start = flag('--start');
const end = flag('--end');
const dur = flag('--dur') ?? '15m';
const outDir = resolve(flag('--out-dir') ?? '/Volumes/Hard Disk/bk-agent387');
const skipReplay = args.includes('--skip-replay');
const skipOff = args.includes('--skip-off');

const missing = [
  ['--binary', binary], ['--corpus', corpus], ['--ledger', ledger],
  ['--start', start], ['--end', end],
].filter(([n, v]) => !v);
if (missing.length || !existsSync(binary) || !existsSync(corpus) || !existsSync(ledger)) {
  console.error(`usage: single-leg-ab.mjs --binary <core> --corpus <jsonl> --ledger <activity.jsonl> \\
  --start YYYY-MM-DD --end YYYY-MM-DD [--dur 15m] [--out-dir dir] [--skip-replay] [--skip-off]`);
  for (const [n, v] of missing) console.error(`  missing ${n}`);
  process.exit(2);
}
mkdirSync(outDir, { recursive: true });

// ── the replay command (identical cwd + flags on both sides) ────────────────
// cwd is the session dir on purpose: execution_policy resolves against it
// (#386) — the same dir for both runs keeps the NEUTRAL kernel NEUTRAL.
const LUA_DIR = process.env.BK_LUA_STRATEGY_DIR ?? join(ROOT, 'user_layer', 'strategies_lua');
const cwd = process.env.BK_SESSION_CWD ?? dirname(corpus);

const reportPath = (name) => join(outDir, name);
function runReplay(enable, outName) {
  const out = reportPath(outName);
  if (skipReplay && existsSync(out)) {
    console.error(`[replay] ${outName}: reuse (exists)`);
    return out;
  }
  const argv = [
    '--no-config', '--engine', '--fee-model', 'official',
    '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
    '--slippage-ticks', '1', '--seed-balance', '100000',
    '--enable-strategy', 'pair_discount_arb',
    ...(enable ? ['--enable-strategy', 'single_leg_pair'] : []),
    '--max-positions', '10000', '--max-positions-per-asset', '10000',
    '--lua-strategy-dir', LUA_DIR,
    '--backtest', corpus, '--backtest-report', out,
    '--backtest-fast', '--backtest-tick-ms', '50', '--backtest-tail-ms', '100',
  ];
  console.error(`[replay] ${outName}: ${enable ? 'OFF+ON' : 'OFF'} → ${out}`);
  const r = spawnSync(binary, argv, { cwd, stdio: ['ignore', 'pipe', 'inherit'] });
  if (r.status !== 0 || !existsSync(out)) {
    console.error(`replay failed (${r.status ?? 'signal'}): ${outName}`);
    process.exit(1);
  }
  return out;
}

function runOverlay(report) {
  const out = report.replace(/\.json$/, '.overlay.json');
  const r = spawnSync(
    process.execPath,
    [join(ROOT, 'scripts', 'almach-overlay.mjs'), ledger, report, start, end, dur],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 },
  );
  if (r.status !== 0) {
    console.error(`overlay failed for ${report}:\n${r.stderr}`);
    process.exit(1);
  }
  writeFileSync(out, r.stdout);
  return JSON.parse(r.stdout);
}

// ── the ledger's own single-leg odds table (the honesty row) ────────────────
// Single-leg round = exactly one leg bought, settled (≥1 REDEEM/MERGE row).
// Same caliber as almach-ground-truth.mjs: legVWAP per (conditionId, asset),
// duration-filtered, settlement booked only when a settle row exists.
function singleLegOdds() {
  const startSec = Date.parse(`${start}T00:00:00Z`) / 1000;
  const endSec = Date.parse(`${end}T00:00:00Z`) / 1000 + 86_400;
  const durOf = (slug) => {
    const p = (slug ?? '').split('-');
    return p.length >= 2 && ['5m', '15m', '1h', '4h'].includes(p.at(-2)) ? p.at(-2) : null;
  };
  const isDur = (slug) => durOf(slug) === dur;
  const legs = new Map(); // cid -> {usd, shares, tokens:Set}
  const settled = new Set();
  const lines = readFileSync(ledger, 'utf8').split('\n');
  for (const line of lines) {
    if (!line) continue;
    const r = JSON.parse(line);
    const ts = r.timestamp ?? 0;
    if (ts < startSec || ts >= endSec) continue;
    if (r.type === 'REDEEM' || r.type === 'MERGE') {
      if (!r.conditionId || !isDur(r.slug)) continue;
      settled.add(r.conditionId);
      continue;
    }
    if (r.type !== 'TRADE' || !isDur(r.slug)) continue;
    let leg = legs.get(r.conditionId);
    if (!leg) { leg = { usd: 0, shares: 0, tokens: new Set() }; legs.set(r.conditionId, leg); }
    leg.usd += r.usdcSize ?? 0;
    leg.shares += r.size ?? 0;
    leg.tokens.add(r.asset);
  }
  const bandOf = (p) =>
    p < 0.1 ? '<0.10' : p < 0.2 ? '0.10-0.20' : p < 0.3 ? '0.20-0.30'
      : p < 0.4 ? '0.30-0.40' : p < 0.5 ? '0.40-0.50' : p < 0.6 ? '0.50-0.60'
        : p < 0.7 ? '0.60-0.70' : p < 0.8 ? '0.70-0.80'
          : p < 0.9 ? '0.80-0.90' : '>=0.90';
  const bands = new Map();
  let rounds = 0;
  for (const [cid, leg] of legs) {
    if (leg.tokens.size !== 1 || leg.shares <= 0 || !settled.has(cid)) continue;
    rounds += 1;
    const vwap = leg.usd / leg.shares;
    const b = bandOf(vwap);
    let s = bands.get(b);
    if (!s) { s = { n: 0, paidUsd: 0 }; bands.set(b, s); }
    s.n += 1; s.paidUsd += leg.usd;
  }
  return { rounds, bands: Object.fromEntries([...bands.entries()].sort()) };
}

// per-strategy split from a report's tradeLines (net PnL per strategy)
function strategySplit(report) {
  const m = new Map();
  for (const tl of report.tradeLines ?? []) {
    const k = tl.strategy ?? 'unknown';
    const cur = m.get(k) ?? { closes: 0, netPnlUsd: 0 };
    cur.closes += 1;
    cur.netPnlUsd += tl.netPnlUsd ?? 0;
    m.set(k, cur);
  }
  return Object.fromEntries([...m.entries()].map(([k, v]) => [
    k, { closes: v.closes, netPnlUsd: +v.netPnlUsd.toFixed(2) },
  ]));
}

// ── run both sides ──────────────────────────────────────────────────────────
const offReport = skipOff ? reportPath('ab-off.json') : runReplay(false, 'ab-off.json');
const onReport = runReplay(true, 'ab-on.json');
const offRep = JSON.parse(readFileSync(offReport, 'utf8'));
const onRep = JSON.parse(readFileSync(onReport, 'utf8'));
const offOv = runOverlay(offReport);
const onOv = runOverlay(onReport);

const t = (rep) => rep.trades ?? {};
const r6 = (n) => (n == null ? null : +(n).toFixed(6));

const verdict = {
  window: { start, end, dur, binary, corpus, cwd, luaDir: LUA_DIR },
  ab: {
    off: {
      closed: t(offRep).closed, wins: t(offRep).wins, losses: t(offRep).losses,
      winRatePct: r6(t(offRep).winRatePct),
      profitFactor: r6(t(offRep).profitFactor),
      netPnlUsd: r6(t(offRep).netPnlUsd),
      feesUsd: r6(t(offRep).feesUsd),
      coverageOfActualPairsPct: offOv.signals.coverageOfActualPairsPct,
      sharedRounds: offOv.signals.sharedRounds,
      backtestOnlyRounds: offOv.signals.backtestOnlyRounds,
      roiBacktestFinalPct: offOv.roi.backtestFinalPct,
      roiActualPairFinalPct: offOv.roi.actualPairFinalPct,
    },
    on: {
      closed: t(onRep).closed, wins: t(onRep).wins, losses: t(onRep).losses,
      winRatePct: r6(t(onRep).winRatePct),
      profitFactor: r6(t(onRep).profitFactor),
      netPnlUsd: r6(t(onRep).netPnlUsd),
      feesUsd: r6(t(onRep).feesUsd),
      coverageOfActualPairsPct: onOv.signals.coverageOfActualPairsPct,
      sharedRounds: onOv.signals.sharedRounds,
      backtestOnlyRounds: onOv.signals.backtestOnlyRounds,
      roiBacktestFinalPct: onOv.roi.backtestFinalPct,
      roiActualPairFinalPct: onOv.roi.actualPairFinalPct,
    },
    delta: {
      closed: t(onRep).closed - t(offRep).closed,
      profitFactor: r6(t(onRep).profitFactor - t(offRep).profitFactor),
      netPnlUsd: r6((t(onRep).netPnlUsd ?? 0) - (t(offRep).netPnlUsd ?? 0)),
      coverageOfActualPairsPct: r6(onOv.signals.coverageOfActualPairsPct - offOv.signals.coverageOfActualPairsPct),
    },
  },
  perStrategy: { off: strategySplit(offRep), on: strategySplit(onRep) },
  singleLegOdds: singleLegOdds(),
  honestyNotes: [
    'coverage = overlay signals.coverageOfActualPairsPct (shared / actual PAIR rounds) — the pair-ceiling caliber; single-leg ledger rounds are outside it by construction',
    'singleLegOdds = the ledger’s own single-leg settlement odds by VWAP band (rounds counted, not PnL — the paid/redeemed caliber lives in almach-ground-truth)',
    'both replays ran the SAME binary, corpus, cwd and flag set; only --enable-strategy single_leg_pair differs',
  ],
};

const outJson = join(outDir, 'single-leg-ab.json');
writeFileSync(outJson, JSON.stringify(verdict, null, 2) + '\n');

const md = [];
md.push(`# #387 single-leg A/B — ${start}..${end} (${dur})`);
md.push('');
md.push(`| metric | OFF (pair only) | ON (pair + single-leg) | delta |`);
md.push('|---|---|---|---|');
const o = verdict.ab.off, n = verdict.ab.on;
const row = (label, a, b, d) => md.push(`| ${label} | ${a} | ${b} | ${d} |`);
row('signal coverage (of actual pair rounds)', `${o.coverageOfActualPairsPct}%`, `${n.coverageOfActualPairsPct}%`, `+${verdict.ab.delta.coverageOfActualPairsPct}pp`);
row('ROI (backtest final, invested caliber)', `${o.roiBacktestFinalPct}%`, `${n.roiBacktestFinalPct}%`, `${r6(n.roiBacktestFinalPct - o.roiBacktestFinalPct)}pp`);
row('profit factor', o.profitFactor, n.profitFactor, verdict.ab.delta.profitFactor);
row('closed trades', o.closed, n.closed, verdict.ab.delta.closed);
row('win rate %', r6(o.winRatePct), r6(n.winRatePct), r6(n.winRatePct - o.winRatePct));
row('net PnL USD', o.netPnlUsd, n.netPnlUsd, verdict.ab.delta.netPnlUsd);
md.push('');
md.push(`Per-strategy (closes / net USD):`);
for (const [k, v] of Object.entries(verdict.perStrategy.on)) {
  md.push(`- ${k}: ${v.closes} / ${v.netPnlUsd}`);
}
md.push('');
md.push(`Single-leg ledger odds (rounds by VWAP band): ${JSON.stringify(verdict.singleLegOdds.bands)}`);
md.push('');
writeFileSync(join(outDir, 'single-leg-ab.md'), md.join('\n') + '\n');
console.log(md.join('\n'));
console.error(`\nverdict: ${outJson}`);
