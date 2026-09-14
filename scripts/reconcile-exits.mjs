#!/usr/bin/env node
/**
 * CANONICAL exit simulator over path-recorded shadow trades. One implementation,
 * used to reconcile earlier inconsistent numbers. Mirrors Rust `decide_exit`
 * (simple mode) ordering: force -> TP -> stop -> trailing -> time.
 */
import { readFileSync } from 'fs';
const recs = readFileSync(process.argv[2] || 'data/backup-20260914-001139/shadow/positions.jsonl', 'utf8')
  .trim().split('\n').map(JSON.parse);
const n = (x) => Number(x);
const pnlPct = (p, e) => (e > 0 ? ((p - e) / e) * 100 : 0);
const profitTrail = (h) => {
  const t = h >= 50 ? 15 : h >= 30 ? 12 : h >= 20 ? 9 : h >= 10 ? 6 : h >= 5 ? 4 : h >= 3 ? 3 : 2;
  return Math.max(3, Math.min(t, h * 0.15)); // proportionalTrailMinGiveback=3, prop=15%, minPct=15
};
const timeTrail = (tl) => (tl > 420 ? 12 : tl > 180 ? 8 : 6);
const FEE_RT = 0.017; // round-trip fee fraction of notional (from observed data)

function simulate(rec, c) {
  const path = rec.own.samples.map((s) => ({ t: s.t, p: n(s.p) }));
  const e = n(rec.entryPrice);
  const tl0 = n(rec.context?.timeLeftSec ?? 700);
  let high = 0;
  let r = null;
  for (const s of path) {
    const pct = pnlPct(s.p, e);
    if (pct > high) high = pct;
    const tl = tl0 - s.t;
    // 1. force exit
    if (tl <= c.forceExit) { r = pct; break; }
    // 2. take profit (backstop) — cap at target
    if (c.tp > 0 && c.tp < 9999 && pct >= c.tp) { r = c.tp; break; }
    // 3. stop loss
    if (pct <= -c.stop) { r = -c.stop; break; }
    // 4. trailing stop
    if (c.trailing && high >= c.arm) {
      const trail = Math.max(c.minTrail, Math.min(profitTrail(high), timeTrail(tl)));
      if (high - pct >= trail) { r = pct; break; }
    }
    // 5. time exit
    if (tl <= c.minTimeLeft) { r = pct; break; }
  }
  if (r == null) r = pnlPct(path[path.length - 1].p, e);
  const sh = n(rec.shares) || 10;
  return (r / 100) * e * sh - sh * (e + e * (1 + r / 100)) * FEE_RT;
}

function agg(rows, c) {
  const o = rows.map((r) => simulate(r, c));
  const w = o.filter((x) => x > 0), l = o.filter((x) => x <= 0);
  const gw = w.reduce((a, b) => a + b, 0), gl = -l.reduce((a, b) => a + b, 0);
  return {
    n: o.length, wr: (100 * w.length / o.length), pf: gw / (gl || 1),
    net: gw - gl, avgWin: gw / (w.length || 1), avgLoss: -gl / (l.length || 1),
  };
}
const fmt = (a) => `WR${a.wr.toFixed(0)}% PF${a.pf.toFixed(2)} net$${a.net.toFixed(2)} (W$${a.avgWin.toFixed(2)}/L$${a.avgLoss.toFixed(2)})`;

const configs = [
  ['旧 SL50/tr10/TP100     ', { stop: 50, minTrail: 10, arm: 15, tp: 100, trailing: true, forceExit: 120, minTimeLeft: 180 }],
  ['§26 TP20/SL15/tr8     ', { stop: 15, minTrail: 8, arm: 15, tp: 20, trailing: true, forceExit: 120, minTimeLeft: 180 }],
  ['§26 TP20/SL15/tr10    ', { stop: 15, minTrail: 10, arm: 15, tp: 20, trailing: true, forceExit: 120, minTimeLeft: 180 }],
  ['现部署 TP100/SL15/tr10 ', { stop: 15, minTrail: 10, arm: 15, tp: 100, trailing: true, forceExit: 120, minTimeLeft: 180 }],
];
console.log('=== 关键配置对照 (n=' + recs.length + ') ===');
for (const [name, c] of configs) console.log('  ' + name + ': ' + fmt(agg(recs, c)));

// Max-NET search over stop x trail x tp
console.log('\n=== 按“净额最大”搜索 (trailing + TP兜底) ===');
let best = null;
for (const stop of [8, 10, 12, 15, 20]) for (const minTrail of [6, 8, 10, 12]) for (const tp of [15, 20, 25, 30, 40, 100]) {
  const c = { stop, minTrail, arm: 15, tp, trailing: true, forceExit: 120, minTimeLeft: 180 };
  const a = agg(recs, c);
  if (!best || a.net > best.a.net) best = { c, a };
}
console.log('  最大净额: SL' + best.c.stop + '/tr' + best.c.minTrail + '/TP' + best.c.tp + ' -> ' + fmt(best.a));

console.log('\n=== 净额 Top8 ===');
const all = [];
for (const stop of [8, 10, 12, 15, 20]) for (const minTrail of [6, 8, 10, 12]) for (const tp of [15, 20, 25, 30, 40, 100]) {
  const c = { stop, minTrail, arm: 15, tp, trailing: true, forceExit: 120, minTimeLeft: 180 };
  all.push({ c, a: agg(recs, c) });
}
all.sort((x, y) => y.a.net - x.a.net);
for (const { c, a } of all.slice(0, 8)) console.log(`  SL${String(c.stop).padEnd(2)}/tr${String(c.minTrail).padEnd(2)}/TP${String(c.tp).padEnd(3)}: ${fmt(a)}`);
