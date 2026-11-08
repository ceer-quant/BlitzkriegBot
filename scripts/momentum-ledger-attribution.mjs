#!/usr/bin/env node
/**
 * lua_momentum ledger attribution — the read-only evidence behind the
 * 2026-11-08/09 "赚的太少" verdict, reproducible from the deployment's own
 * files (no binary, no replay corpus needed):
 *
 *   node scripts/momentum-ledger-attribution.mjs [--account polysimulator]
 *
 * The measured shape (2026-11-09, 1017 fills):
 *   all                n=1017 WR=71.4% net= +31.48
 *   ex unverifiable    n= 799 WR=90.9% net=+138.87   <- the strategy body
 *   unverifiable_expiry n= 218 WR= 0.0% net=-107.38   <- the only drain
 *
 * The conclusion this prints: the entry/exit PARAMETERS are healthy (body
 * WR 90.9%, both directions PF > 1); the headline shortfall is the dry
 * venue's `unverifiable_expiry` bookkeeping (expired with no verdict →
 * fallback price → straight loss booking). Whether that drain should move
 * to the `BK_UNVERIFIED_EXPIRY_POLICY=receivable` posture is an accounting
 * ruling, not a tuning knob — this script only measures.
 *
 * Read-only: opens trades.jsonl + the account journals for READING. No
 * writes, no kernel contact.
 */
import { readFileSync, existsSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const flag = (n, d) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : d;
};
const ACCOUNT = flag('--account', 'polysimulator');
const TRADES = flag('--trades', join(ROOT, 'data', 'trades', 'trades.jsonl'));

if (!existsSync(TRADES)) {
  console.error(`trades log not found: ${TRADES}`);
  process.exit(2);
}

const rows = readFileSync(TRADES, 'utf8')
  .split('\n')
  .filter((l) => l.trim())
  .map((l) => JSON.parse(l))
  .filter((t) => (t.accountId ?? '') === ACCOUNT);

const stats = (ts) => {
  if (!ts.length) return { n: 0, wr: 0, net: 0, pf: 0 };
  const wins = ts.filter((t) => t.netPnlUsd > 0);
  const gross = wins.reduce((a, t) => a + t.netPnlUsd, 0);
  const loss = -ts.filter((t) => t.netPnlUsd <= 0).reduce((a, t) => a + t.netPnlUsd, 0);
  return {
    n: ts.length,
    wr: (wins.length / ts.length) * 100,
    net: ts.reduce((a, t) => a + t.netPnlUsd, 0),
    pf: loss > 0 ? gross / loss : Infinity,
  };
};
const line = (label, s) =>
  `  ${label.padEnd(26)} n=${String(s.n).padStart(4)}  WR=${s.wr.toFixed(1).padStart(5)}%  ` +
  `net=${s.net >= 0 ? '+' : ''}${s.net.toFixed(2).padStart(8)}  PF=${Number.isFinite(s.pf) ? s.pf.toFixed(2) : ' inf'}`;

const band = (p) =>
  p < 0.2 ? '0.00-0.20' : p < 0.35 ? '0.20-0.35' : p < 0.5 ? '0.35-0.50' : '0.50+';

console.log(`lua_momentum ledger attribution — account=${ACCOUNT}, fills=${rows.length}\n`);
console.log('by scope');
console.log(line('all', stats(rows)));
const body = rows.filter((t) => t.exitReason !== 'unverifiable_expiry');
console.log(line('ex unverifiable_expiry', stats(body)));
console.log(line('  unverifiable_expiry', stats(rows.filter((t) => t.exitReason === 'unverifiable_expiry'))));

console.log('\nby direction');
for (const d of ['down', 'up']) console.log(line(d, stats(rows.filter((t) => t.direction === d))));

console.log('\nby entry price band');
for (const b of ['0.00-0.20', '0.20-0.35', '0.35-0.50', '0.50+']) {
  console.log(line(b, stats(rows.filter((t) => band(t.entryPrice) === b))));
}

console.log('\nby exit reason');
const reasons = [...new Set(rows.map((t) => t.exitReason))].sort(
  (a, b) => stats(rows.filter((t) => t.exitReason === b)).net - stats(rows.filter((t) => t.exitReason === a)).net,
);
for (const r of reasons) console.log(line(r, stats(rows.filter((t) => t.exitReason === r))));

console.log('\nby entry role');
for (const m of [true, false]) {
  console.log(line(m ? 'maker entry' : 'taker entry', stats(rows.filter((t) => t.wasMakerEntry === m))));
}

// The ledger cross-check: the cash-truth anchor (seededTotal) work means the
// fold above and the account's journal may disagree; print both so a reader
// sees which is which.
const journal = join(ROOT, 'data', 'ledger', `${ACCOUNT}.journal.dry.jsonl`);
if (existsSync(journal)) {
  const jrows = readFileSync(journal, 'utf8').split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
  const last = jrows[jrows.length - 1];
  const seeded = jrows.filter((r) => r.kind === 'seed').reduce((a, r) => a + r.amount, 0);
  if (last) {
    const ledgerNet = last.balanceAfter - seeded;
    console.log('\nledger口径 (cash journal — the single truth)');
    console.log(`  rows=${jrows.length}  balance=${last.balanceAfter}  seeded=${seeded}  账本净利=${ledgerNet.toFixed(2)}`);
    console.log(`  fold口径净利=${stats(rows).net.toFixed(2)}  Δ=${(stats(rows).net - ledgerNet).toFixed(2)} (fee-model vs cash) `);
  }
}
