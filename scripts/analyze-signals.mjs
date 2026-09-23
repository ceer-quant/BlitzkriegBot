#!/usr/bin/env node
/**
 * Analyze shadow signal records (data/signals/signals.jsonl).
 *
 * Answers: which signals reached the engine, and under which conditions they
 * turned into entries. The records are whatever strategy produced them — the
 * kernel ships none of its own, so the file is the operator's to interpret.
 *
 * Usage:
 *   node scripts/analyze-signals.mjs
 *   node scripts/analyze-signals.mjs --file data/signals/signals.jsonl
 */

import { readFileSync, existsSync } from 'fs';
import { resolve } from 'path';

const args = process.argv.slice(2);
const fileArgIdx = args.indexOf('--file');
const FILE = fileArgIdx >= 0 ? args[fileArgIdx + 1] : 'data/signals/signals.jsonl';

if (!existsSync(FILE)) {
  console.error(`No signal file at ${resolve(FILE)} — run the engine in dry-run first.`);
  process.exit(1);
}

const records = readFileSync(FILE, 'utf-8')
  .split('\n')
  .filter(Boolean)
  .map((l) => { try { return JSON.parse(l); } catch { return null; } })
  .filter(Boolean);

const HORIZONS = ['pct5s', 'pct15s', 'pct30s', 'pct60s', 'maxPct', 'minPct'];

function summarize(rows) {
  const out = { n: rows.length };
  for (const h of HORIZONS) {
    const vals = rows.map((r) => r.pct?.[h]).filter((v) => typeof v === 'number');
    if (vals.length === 0) { out[h] = null; continue; }
    const mean = vals.reduce((a, b) => a + b, 0) / vals.length;
    const win = (vals.filter((v) => v > 0).length / vals.length) * 100;
    out[h] = { mean: round(mean), win: round(win), n: vals.length };
  }
  const touched = rows.filter((r) => r.touched);
  out.touchRate = rows.length ? round((touched.length / rows.length) * 100) : 0;
  const pmax = touched.map((r) => r.postMaxPct).filter((v) => typeof v === 'number');
  const pmin = touched.map((r) => r.postMinPct).filter((v) => typeof v === 'number');
  const delay = touched.map((r) => r.touchDelaySec).filter((v) => typeof v === 'number');
  out.postMaxAvg = pmax.length ? round(pmax.reduce((a, b) => a + b, 0) / pmax.length) : null;
  out.postMinAvg = pmin.length ? round(pmin.reduce((a, b) => a + b, 0) / pmin.length) : null;
  // % of filled signals whose best exit after fill was >= +20% (favourable)
  out.postWin20 = pmax.length ? round((pmax.filter((v) => v >= 20).length / pmax.length) * 100) : null;
  out.touchDelayAvg = delay.length ? round(delay.reduce((a, b) => a + b, 0) / delay.length) : null;
  // Of fills, did the best price come before the worst? (i.e. could we exit up first)
  const seq = touched.filter((r) => typeof r.timeToPostMaxSec === 'number' && typeof r.timeToPostMinSec === 'number');
  out.upFirstPct = seq.length ? round((seq.filter((r) => r.timeToPostMaxSec <= r.timeToPostMinSec).length / seq.length) * 100) : null;
  return out;
}
const round = (n) => Math.round(n * 100) / 100;

function printTable(title, buckets) {
  console.log(`\n=== ${title} ===`);
  console.log('bucket                      n   touch%  fillMax fillMin  win20%  upFirst%  avg30s  WR30s');
  for (const [name, rows] of buckets) {
    if (rows.length === 0) continue;
    const s = summarize(rows);
    const g = (v, w = 6) => (v === null || v === undefined ? '-'.padStart(w) : `${v}`.padStart(w));
    console.log(
      `${name.padEnd(26)} ${String(s.n).padStart(4)}  ${g(s.touchRate)}  ${g(s.postMaxAvg)} ${g(s.postMinAvg)}  ${g(s.postWin20)}  ${g(s.upFirstPct, 7)}  ${g(s.pct30s?.mean)}  ${g(s.pct30s?.win)}`
    );
  }
}

function groupBy(rows, keyFn) {
  const m = new Map();
  for (const r of rows) {
    const k = keyFn(r);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(r);
  }
  return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
}

console.log(`Loaded ${records.length} signals from ${FILE}`);

// Hypothetical entry-factor sensitivity (uses the recorded max/min path, so it
// works even if the live factor differed when the signal was logged).
(function factorSensitivity() {
  const rows = records.filter((r) => typeof r.minPct === 'number' && typeof r.marketPrice === 'number' && r.marketPrice > 0);
  if (rows.length === 0) return;
  console.log('\n=== Entry-factor sensitivity (dip needed to fill) ===');
  console.log('factor  need-dip  touchRate  postMaxAvg  best>=20%  postMinAvg');
  for (const f of [0.7, 0.8, 0.85, 0.9, 0.95]) {
    const thr = (f - 1) * 100;
    const touched = rows.filter((r) => r.minPct <= thr);
    const pm = touched.map((r) => ((1 + (r.maxPct || 0) / 100) / f - 1) * 100);
    const pn = touched.map((r) => ((1 + (r.minPct || 0) / 100) / f - 1) * 100);
    const avg = (x) => (x.length ? x.reduce((a, b) => a + b, 0) / x.length : 0);
    const ge20 = pm.length ? (pm.filter((v) => v >= 20).length / pm.length) * 100 : 0;
    console.log(
      `${f.toFixed(2)}   ${thr.toFixed(0).padStart(6)}%   ${((touched.length / rows.length) * 100).toFixed(1).padStart(7)}%   ${avg(pm).toFixed(1).padStart(8)}%   ${ge20.toFixed(1).padStart(7)}%   ${avg(pn).toFixed(1).padStart(8)}%`
    );
  }
})();

printTable('Overall', [['all', records]]);
printTable('By direction', groupBy(records, (r) => r.direction));
printTable('By drop depth (marketPrice at signal)', groupBy(records, (r) => {
  const p = r.marketPrice ?? 0;
  if (p < 0.30) return '<0.30';
  if (p < 0.35) return '0.30-0.35';
  if (p < 0.40) return '0.35-0.40';
  return '>=0.40';
}));
printTable('By trend age (held above threshold)', groupBy(records, (r) => {
  const a = r.trendAgeSec ?? r.highAgeSec ?? 0;
  if (a < 10) return '<10s';
  if (a < 30) return '10-30s';
  if (a < 60) return '30-60s';
  return '>=60s';
}));
printTable('By spotMove30 at signal', groupBy(records, (r) => {
  const m = r.spotMove30 ?? 0;
  if (m < -0.1) return '<-0.10%';
  if (m < -0.03) return '-0.10~-0.03%';
  if (m <= 0.03) return '-0.03~0.03%';
  if (m <= 0.1) return '0.03~0.10%';
  return '>0.10%';
}));
printTable('By timeLeftSec', groupBy(records, (r) => {
  const t = r.timeLeftSec ?? 0;
  if (t < 180) return '<180s';
  if (t < 300) return '180-300s';
  if (t < 600) return '300-600s';
  return '>=600s';
}));

console.log('\nReading: "avgNs" = mean % move from signal price at horizon N; "WR" = share positive.');
console.log('Edge exists if avg > 0 after costs; look for buckets with consistently positive avg and WR.');
