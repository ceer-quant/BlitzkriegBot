#!/usr/bin/env node
/**
 * Read-only strategy analysis: trade outcomes by bucket + near-miss (blocked
 * signal) counterfactuals. Prints evidence only; changes nothing.
 */
import { readFileSync } from 'fs';

const trades = readFileSync('data/trades/trades.jsonl', 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
const misses = readFileSync('data/shadow/near-miss.jsonl', 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);

const num = (x) => Number(x);
const pct = (n, d) => (d > 0 ? (100 * n / d).toFixed(0) + '%' : '-');
const money = (x) => (x >= 0 ? '+' : '-') + '$' + Math.abs(x).toFixed(2);

console.log('════════ TRADES: ' + trades.length + ' ════════');
const wins = trades.filter((t) => num(t.netPnlUsd) > 0);
const losses = trades.filter((t) => num(t.netPnlUsd) <= 0);
const grossWin = wins.reduce((a, t) => a + num(t.netPnlUsd), 0);
const grossLoss = Math.abs(losses.reduce((a, t) => a + num(t.netPnlUsd), 0));
const net = trades.reduce((a, t) => a + num(t.netPnlUsd), 0);
const fees = trades.reduce((a, t) => a + num(t.feesUsd), 0);
console.log(`WR ${pct(wins.length, trades.length)} (${wins.length}W/${losses.length}L)  net ${money(net)}  fees $${fees.toFixed(2)}`);
console.log(`avg win ${money(grossWin / (wins.length || 1))}  avg loss ${money(-grossLoss / (losses.length || 1))}  profit factor ${(grossWin / (grossLoss || 1)).toFixed(2)}`);

const group = (rows, keyFn) => {
  const m = new Map();
  for (const t of rows) {
    const k = keyFn(t);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(t);
  }
  return m;
};
const table = (title, rows, keyFn, sortKeys) => {
  console.log(`\n── ${title} ──`);
  const m = group(rows, keyFn);
  const keys = sortKeys ? [...m.keys()].sort(sortKeys) : [...m.keys()];
  console.log('  ' + 'bucket'.padEnd(16) + 'n'.padStart(4) + 'win%'.padStart(7) + 'net'.padStart(10) + 'avgNet'.padStart(10));
  for (const k of keys) {
    const g = m.get(k);
    const w = g.filter((t) => num(t.netPnlUsd) > 0).length;
    const n = g.reduce((a, t) => a + num(t.netPnlUsd), 0);
    console.log('  ' + String(k).padEnd(16) + String(g.length).padStart(4) + pct(w, g.length).padStart(7) + money(n).padStart(10) + money(n / g.length).padStart(10));
  }
};

table('by exitReason', trades, (t) => t.exitReason);
table('by asset', trades, (t) => t.asset);
table('by direction', trades, (t) => t.direction);
table('by entryPrice bucket', trades, (t) => {
  const p = num(t.entryPrice);
  return p < 0.42 ? '<0.42' : p < 0.44 ? '0.42-0.44' : p < 0.45 ? '0.44-0.45' : '>=0.45';
}, (a, b) => (a < b ? -1 : 1));
table('by hold time', trades, (t) => {
  const h = num(t.holdTimeSec);
  return h < 10 ? '<10s' : h < 30 ? '10-30s' : h < 120 ? '30-120s' : '>=120s';
}, (a, b) => (a < b ? -1 : 1));
table('by wasMakerEntry', trades, (t) => String(t.wasMakerEntry));

// Giveback: how much profit was given back (highPnlPct vs final)
const withHigh = trades.filter((t) => num(t.highPnlPct) > 0);
const giveback = withHigh.map((t) => ({ h: num(t.highPnlPct), f: num(t.netPnlPct), g: num(t.highPnlPct) - num(t.netPnlPct) }));
console.log(`\n── profit giveback (n=${withHigh.length}) ──`);
console.log(`  avg high ${giveback.reduce((a, x) => a + x.h, 0) / (giveback.length || 1)}%  avg final ${giveback.reduce((a, x) => a + x.f, 0) / (giveback.length || 1)}%`);
const bigGive = giveback.filter((x) => x.h > 10 && x.f < 0);
console.log(`  peaked >10% then closed negative: ${bigGive.length}/${withHigh.length}`);

console.log('\n════════ NEAR-MISS (blocked signals): ' + misses.length + ' ════════');
const byAsset = group(misses, (m) => m.asset);
for (const [k, g] of byAsset) console.log(`  ${k}: ${g.length}`);
console.log('  sample keys: ' + Object.keys(misses[0] || {}).join(','));
console.log('  blockedAt range: ' + new Date(Math.min(...misses.map(m=>m.blockedAt))).toLocaleString() + ' → ' + new Date(Math.max(...misses.map(m=>m.blockedAt))).toLocaleString());
