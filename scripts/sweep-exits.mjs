#!/usr/bin/env node
/**
 * Read-only exit-parameter sweep over path-recorded shadow trades.
 * Reports the achievable win-rate / profit-factor frontier and checks
 * robustness by trimming the best and worst trades.
 */
import { readFileSync } from 'fs';
const recs = readFileSync('data/backup-20260914-001139/shadow/positions.jsonl', 'utf8').trim().split('\n').map(JSON.parse);
const n = (x) => Number(x);
const pnlPct = (p, e) => (e > 0 ? ((p - e) / e) * 100 : 0);
const profitTrail = (h) => {
  const t = h >= 50 ? 15 : h >= 30 ? 12 : h >= 20 ? 9 : h >= 10 ? 6 : h >= 5 ? 4 : h >= 3 ? 3 : 2;
  return Math.max(3, Math.min(t, h * 0.15));
};
const timeTrail = (tl) => (tl > 420 ? 12 : tl > 180 ? 8 : 6);
const FEE_RT = 0.017;

function tradeNet(rec, c) {
  const path = rec.own.samples.map((s) => ({ t: s.t, p: n(s.p) }));
  const e = n(rec.entryPrice);
  const tl0 = n(rec.context?.timeLeftSec ?? 700);
  let high = 0, lock = null, r = null;
  for (const s of path) {
    const pct = pnlPct(s.p, e);
    if (pct > high) high = pct;
    const tl = tl0 - s.t;
    if (tl <= c.forceExit) { r = pct; break; }
    if (c.beArm != null && high >= c.beArm) lock = c.beFloor;
    const stop = lock != null ? Math.max(-c.stop, lock) : -c.stop;
    if (pct <= stop) { r = lock != null && lock > -c.stop ? lock : -c.stop; break; }
    if (c.tp < 9999 && pct >= c.tp) { r = c.tp; break; }
    if (c.arm != null && c.trailing && high >= c.arm) {
      const tr = Math.max(c.minTrail, Math.min(profitTrail(high), timeTrail(tl)));
      if (high - pct >= tr) { r = pct; break; }
    }
    if (tl <= c.minTimeLeft) { r = pct; break; }
  }
  if (r == null) r = pnlPct(path[path.length - 1].p, e);
  const sh = n(rec.shares) || 10;
  const gross = (r / 100) * e * sh;
  const fee = sh * (e + e * (1 + r / 100)) * FEE_RT;
  return gross - fee;
}

const agg = (rows, c) => {
  const o = rows.map((r) => tradeNet(r, c));
  const w = o.filter((x) => x > 0), l = o.filter((x) => x <= 0);
  const gw = w.reduce((a, b) => a + b, 0), gl = -l.reduce((a, b) => a + b, 0);
  return { wr: 100 * w.length / o.length, pf: gw / (gl || 1), net: gw - gl, n: o.length };
};

const base = { stop: 15, minTrail: 10, arm: 15, tp: 100, trailing: true, forceExit: 120, minTimeLeft: 180 };

// Sort by base net for trimming.
const ranked = [...recs].sort((a, b) => tradeNet(b, base) - tradeNet(a, base));
const noTop2 = recs.filter((r) => !ranked.slice(0, 2).includes(r));
const noTop5 = recs.filter((r) => !ranked.slice(0, 5).includes(r));

console.log('══ 止盈 × 止损 扫描 (trail10, arm15, 无breakeven) — 全样本 ══');
console.log('  TP\\SL       ' + [10, 12, 15, 20, 30].map((s) => `SL${s}`.padStart(16)).join(''));
for (const tp of [8, 10, 12, 15, 20, 30, 100]) {
  const cells = [10, 12, 15, 20, 30].map((sl) => {
    const a = agg(recs, { ...base, tp, stop: sl });
    return `W${a.wr.toFixed(0)}/P${a.pf.toFixed(1)}`.padStart(16);
  });
  console.log(`  TP${String(tp).padEnd(4)}` + cells.join(''));
}

console.log('\n══ 只用止盈(无移动止盈)能否到高胜率 ══');
for (const tp of [8, 10, 12, 15]) {
  const a = agg(recs, { ...base, tp, arm: null, trailing: false });
  console.log(`  TP${tp}% 纯固定止盈: WR=${a.wr.toFixed(0)}% PF=${a.pf.toFixed(2)} net=$${a.net.toFixed(2)}`);
}

console.log('\n══ 高胜率候选的稳健性(去掉最赚的2/5笔) ══');
const candidates = [
  ['TP12/SL15/tr10', { ...base, tp: 12 }],
  ['TP10/SL15/tr10', { ...base, tp: 10 }],
  ['TP12/SL12/tr10', { ...base, tp: 12, stop: 12 }],
  ['TP12/SL15/tr6 ', { ...base, tp: 12, minTrail: 6 }],
];
for (const [name, c] of candidates) {
  const a = agg(recs, c), b = agg(noTop2, c), d = agg(noTop5, c);
  console.log(`  ${name}: 全 W${a.wr.toFixed(0)}/P${a.pf.toFixed(2)}/$${a.net.toFixed(2)} | 去top2 W${b.wr.toFixed(0)}/P${b.pf.toFixed(2)} | 去top5 W${d.wr.toFixed(0)}/P${d.pf.toFixed(2)}`);
}

console.log('\n══ breakeven 止损 + 高止盈(抬盈亏比) ══');
for (const be of [5, 8]) for (const tp of [20, 30, 100]) {
  const a = agg(recs, { ...base, tp, beArm: be, beFloor: 0 });
  console.log(`  BE@${be}%/TP${tp}: W${a.wr.toFixed(0)}/P${a.pf.toFixed(2)}/$${a.net.toFixed(2)}`);
}
