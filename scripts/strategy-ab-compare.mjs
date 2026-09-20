#!/usr/bin/env node
/**
 * Strategy A/B compare (E15 / #97): two frozen backtest reports, one verdict.
 *
 * The acceptance is explicit: a new configuration may only be called an
 * improvement when it is better on AT LEAST TWO metrics against the old one,
 * measured on the SAME archive through the same replay engine. This tool
 * takes the two report JSONs `blitzkrieg-core --backtest --backtest-report`
 * wrote, extracts the ledger metrics, computes the deltas and emits a
 * pass/fail verdict with the evidence:
 *
 *   - better: win rate, profit factor, payoff, net PnL, max drawdown (lower
 *     is better), fees (lower is better)
 *   - pass: candidate wins on ≥ `--min-better` (default 2) metrics
 *
 * Exit code 0 = pass, 1 = fail, 2 = usage — so a gate or CI step can consume
 * the verdict directly.
 *
 * Usage:
 *   node scripts/strategy-ab-compare.mjs --baseline <report.json> \
 *     --candidate <report.json> [--min-better 2] [--out report-ab.{json,md} dir]
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const baselinePath = flag('--baseline');
const candidatePath = flag('--candidate');
const minBetter = Number(flag('--min-better') ?? 2);
if (!baselinePath || !candidatePath) {
  console.error('usage: --baseline <report.json> --candidate <report.json> [--min-better 2] [--out dir]');
  process.exit(2);
}
for (const p of [baselinePath, candidatePath]) {
  if (!existsSync(p)) { console.error(`report not found: ${p}`); process.exit(2); }
}

const num = (v) => {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};

function metricsOf(rep, label) {
  const t = rep.trades ?? {};
  const wins = num(t.wins) ?? 0, losses = num(t.losses) ?? 0;
  const gp = num(t.grossProfitUsd) ?? 0, gl = num(t.grossLossUsd) ?? 0;
  const payoff = wins > 0 && losses > 0 ? (gp / wins) / (gl / losses) : null;
  return {
    label,
    source: rep.source ?? null,
    events: num(rep.sourceStats?.events),
    startAtMs: num(rep.startAtMs),
    endAtMs: num(rep.endAtMs),
    closed: num(t.closed),
    winRatePct: num(t.winRatePct),
    payoff,
    profitFactor: num(t.profitFactor),
    netPnlUsd: num(t.netPnlUsd),
    maxDrawdownUsd: num(t.maxDrawdownUsd),
    feesUsd: num(t.feesUsd),
    fills: num(rep.fills),
  };
}

const A = metricsOf(JSON.parse(readFileSync(baselinePath, 'utf8')), 'baseline');
const B = metricsOf(JSON.parse(readFileSync(candidatePath, 'utf8')), 'candidate');

// higher-is-better unless stated. The kernel report stores maxDrawdownUsd as
// a positive magnitude (peak − equity), so smaller is better.
const METRICS = [
  ['winRatePct', 'win rate %', 'higher'],
  ['payoff', 'payoff', 'higher'],
  ['profitFactor', 'profit factor', 'higher'],
  ['netPnlUsd', 'net PnL USD', 'higher'],
  ['maxDrawdownUsd', 'max drawdown USD', 'lower'],
  ['feesUsd', 'fees USD', 'lower'],
  ['closed', 'closed trades', 'info'],
  ['fills', 'fills', 'info'],
];

const rows = [];
let betterCount = 0, worseCount = 0;
for (const [key, label, dir] of METRICS) {
  const a = A[key], b = B[key];
  let verdict;
  if (a == null || b == null || dir === 'info') verdict = a == null || b == null ? 'n/a' : 'info';
  else if (dir === 'lower') {
    if (b < a) { verdict = 'better'; betterCount++; }
    else if (b > a) { verdict = 'worse'; worseCount++; }
    else verdict = 'equal';
  } else {
    if (b > a) { verdict = 'better'; betterCount++; }
    else if (b < a) { verdict = 'worse'; worseCount++; }
    else verdict = 'equal';
  }
  rows.push({ key, label, baseline: a, candidate: b, delta: a != null && b != null ? b - a : null, verdict });
}

const passed = betterCount >= minBetter;
const verdictText = passed
  ? `PASS — candidate better on ${betterCount} metric(s) (≥ ${minBetter} required)`
  : `FAIL — candidate better on ${betterCount} metric(s) (< ${minBetter} required)`;

const r4 = (n) => (n == null ? '—' : (Math.round(n * 10000) / 10000).toString());
const out = flag('--out') ? resolve(flag('--out')) : null;

const result = {
  generatedAtMs: Date.now(),
  baseline: { path: baselinePath, ...A },
  candidate: { path: candidatePath, ...B },
  minBetterRequired: minBetter,
  metricsBetter: betterCount,
  metricsWorse: worseCount,
  verdict: passed ? 'pass' : 'fail',
  verdictText,
  rows,
};
if (out) {
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, 'ab-compare.json'), JSON.stringify(result, null, 2) + '\n');
}

const md = [];
md.push(`# Strategy A/B — ${new Date(result.generatedAtMs).toISOString()}`);
md.push('');
md.push(`**${verdictText}**`);
md.push('');
md.push(`| metric | baseline (${A.label}) | candidate (${B.label}) | delta | verdict |`);
md.push('|---|---|---|---|---|');
for (const r of rows) {
  md.push(`| ${r.label} | ${r4(r.baseline)} | ${r4(r.candidate)} | ${r.delta == null ? '—' : r4(r.delta)} | ${r.verdict} |`);
}
md.push('');
md.push(`Windows: baseline ${r4(A.startAtMs) != null ? new Date(A.startAtMs).toISOString() : '—'} → ${A.endAtMs != null ? new Date(A.endAtMs).toISOString() : '—'}; candidate must cover the SAME archive (check \`events\`).`);
md.push('');
if (out) writeFileSync(join(out, 'ab-compare.md'), md.join('\n') + '\n');
else console.log(md.join('\n'));

console.log(`A/B: ${verdictText}`);
process.exit(passed ? 0 : 1);
