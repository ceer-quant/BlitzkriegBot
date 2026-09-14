#!/usr/bin/env node
/** Canonical sim + breakeven + robustness. Answers: max final profit & max PF. */
import { readFileSync } from 'fs';
const recs = readFileSync('data/backup-20260914-001139/shadow/positions.jsonl', 'utf8').trim().split('\n').map(JSON.parse);
const n = (x) => Number(x);
const pnlPct = (p, e) => (e > 0 ? ((p - e) / e) * 100 : 0);
const profitTrail = (h) => Math.max(3, Math.min(h >= 50 ? 15 : h >= 30 ? 12 : h >= 20 ? 9 : h >= 10 ? 6 : h >= 5 ? 4 : h >= 3 ? 3 : 2, h * 0.15));
const timeTrail = (tl) => (tl > 420 ? 12 : tl > 180 ? 8 : 6);
const FEE_RT = 0.017;

function simulate(rec, c) {
  const path = rec.own.samples.map((s) => ({ t: s.t, p: n(s.p) }));
  const e = n(rec.entryPrice), tl0 = n(rec.context?.timeLeftSec ?? 700);
  let high = 0, r = null;
  for (const s of path) {
    const pct = pnlPct(s.p, e);
    if (pct > high) high = pct;
    const tl = tl0 - s.t;
    if (tl <= c.forceExit) { r = pct; break; }
    if (c.tp > 0 && c.tp < 9999 && pct >= c.tp) { r = c.tp; break; }
    // hard stop
    if (pct <= -c.stop) { r = -c.stop; break; }
    // breakeven lock: was up >=be, back to ~flat -> exit at MARKET (r = pct),
    // mirroring Rust's breakeven branch (which returns a decision at the bid).
    if (c.be != null && high >= c.be && pct <= 0.5) { r = pct; break; }
    if (c.trailing && high >= c.arm) {
      const tr = Math.max(c.minTrail, Math.min(profitTrail(high), timeTrail(tl)));
      if (high - pct >= tr) { r = pct; break; }
    }
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
  return { wr: 100 * w.length / o.length, pf: gw / (gl || 1), net: gw - gl,
    gw, gl, nW: w.length, nL: l.length, sumW: gw, sumL: gl };
}

const mk = (o) => ({ stop: 15, minTrail: 10, arm: 15, tp: 100, trailing: true, forceExit: 120, minTimeLeft: 180, ...o });
const B = mk({});

// Robustness reference ordering uses the deployed base.
const ranked = [...recs].sort((a, b) => simulate(b, B) - simulate(a, B));
const noTop2 = recs.filter((r) => !ranked.slice(0, 2).includes(r));
const noTop5 = recs.filter((r) => !ranked.slice(0, 5).includes(r));
const worst5 = recs.filter((r) => ![...recs].sort((a, b) => simulate(a, B) - simulate(b, B)).slice(0, 5).includes(r));

const line = (name, c) => {
  const a = agg(recs, c), b = agg(noTop2, c), d = agg(noTop5, c), e = agg(worst5, c);
  return `${name} | 全 W${a.wr.toFixed(0)}/P${a.pf.toFixed(2)}/$${a.net.toFixed(2)} | 去top2 P${b.pf.toFixed(2)}/$${b.net.toFixed(2)} | 去top5 P${d.pf.toFixed(2)}/$${d.net.toFixed(2)} | 去最亏5 P${e.pf.toFixed(2)}/$${e.net.toFixed(2)}`;
};

console.log('=== 候选对照 (含稳健性) ===');
console.log(line('现部署 SL15/tr10/TP100      ', mk({})));
console.log(line('TP20/SL15/tr8 (§26误报过)   ', mk({ tp: 20, minTrail: 8 })));
console.log(line('SL12/tr10/TP100            ', mk({ stop: 12 })));
console.log(line('SL12/tr8 /TP100            ', mk({ stop: 12, minTrail: 8 })));
console.log(line('SL12/tr6 /TP100            ', mk({ stop: 12, minTrail: 6 })));
console.log(line('SL12/tr6 /TP100 +BE@8      ', mk({ stop: 12, minTrail: 6, be: 8, beFloor: 0 })));
console.log(line('SL10/tr6 /TP100 +BE@8      ', mk({ stop: 10, minTrail: 6, be: 8, beFloor: 0 })));

console.log('\n=== 全网格: 按净额 Top6 / 按PF Top6 (5x4x6 + BE) ===');
const all = [];
for (const stop of [8, 10, 12, 15, 20])
  for (const minTrail of [6, 8, 10, 12])
    for (const tp of [15, 20, 25, 30, 40, 100])
      for (const be of [null, 8]) {
        const c = mk({ stop, minTrail, tp, be, beFloor: be != null ? 0 : undefined });
        all.push({ c, a: agg(recs, c) });
      }
const key = (c) => `SL${c.stop}/tr${c.minTrail}/TP${c.tp}${c.be != null ? '+BE' + c.be : ''}`;
console.log('  按净额:');
[...all].sort((x, y) => y.a.net - x.a.net).slice(0, 6).forEach(({ c, a }) => console.log(`    ${key(c).padEnd(22)} ${line('', c).split('|')[1].trim()}`));
console.log('  按盈亏比:');
[...all].sort((x, y) => y.a.pf - x.a.pf).slice(0, 6).forEach(({ c, a }) => console.log(`    ${key(c).padEnd(22)} ${line('', c).split('|')[1].trim()}`));
