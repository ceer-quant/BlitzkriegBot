/**
 * Regression check for the panel's balance accounting (`src/lib/balance.ts`,
 * `src/lib/trades.ts`).
 *
 * `balance.ts` is what turned "模拟余额 1070 with 净利润 40" from a
 * contradiction into an explained number. The properties pinned here:
 *
 *   - the headline balance is 本金 ＋ 净利润, so a fee can never move the
 *     principal — it is an expense already deducted from net profit;
 *   - fees are reported as a cost, and gross profit is `net + fees` — the figure
 *     the cost is charged against;
 *   - the core's cash ledger is never the DRY headline, however far it diverges:
 *     it reseeds from the principal every boot and moves only on that run's
 *     fills, so it is 本金 ＋ 本次会话净利 — a strict subset of what the trade log
 *     already records. The panel shows it only where it is the only figure that
 *     exists, i.e. when the core reports no principal;
 *   - an unknown principal yields NO equity, so a core that does not report its
 *     seed is never accused of drifting against a principal of zero;
 *   - closed-trade rows collapse only when they describe the SAME trade: `hft-N`
 *     is a per-boot counter, so 267 distinct trades can carry 240 ids and keying
 *     the dedupe on the id silently deletes real trades from every row-derived
 *     aggregate (the equity curve ended $9.24 short of the summary above it);
 *
 *   cd ui/webapp/webui && npm run check:balance
 */
import assert from 'node:assert/strict'

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const { balanceView } = await import('../src/lib/balance.ts')
const { dedupeTrades, tradeIdentity } = await import('../src/lib/trades.ts')

const balance = (over) => ({ balance: 0, reserved: 0, available: 0, seed: null, ...over })

console.log('balance basis — 本金 ＋ 净利润')

check('the headline is 本金 ＋ 净利润, not the core cash ledger', () => {
  // The case that started this: cash 1073.29, principal 1000, net 39.44. The
  // balance shown is 1039.44; cash is a separate figure, not the balance.
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: 1000 }), 39.44, 34.96)
  assert.equal(Number(v.equity.toFixed(2)), 1039.44)
  assert.equal(v.cash, 1073.29)
  assert.notEqual(v.equity, v.cash, 'an unexplained cash surplus is not the balance')
})

check('the headline is unmoved by a diverging cash ledger', () => {
  // The book as measured on 2026-09-16 23:35: seed 1000, core cash 1002.06 (the
  // ledger had been reseeded at that boot and covered only that run), all-time
  // net 48.21. The DRY headline is 1048.21 and must stay exactly that: the
  // ledger's value never enters it, and the panel no longer prints the ledger
  // beside it — the difference against the log is 本次启动前净利, which the trade
  // history already shows in full.
  const v = balanceView(balance({ balance: 1002.06, available: 1002.06, seed: 1000 }), 48.21, 38.69)
  assert.equal(Number(v.equity.toFixed(2)), 1048.21)
  assert.equal(Number(v.cash.toFixed(2)), 1002.06)
  assert.equal(v.cashGap, undefined, 'no reconciliation against the equity is computed anymore')
})

check('equity is the principal plus the net, whatever the ledger says', () => {
  // The same trade with wildly different cash figures: the equity is untouched,
  // which is the point — a basis difference cannot move the balance.
  const a = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 0)
  const b = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), 39.44, 0)
  assert.equal(a.equity, b.equity, 'a different cash ledger is not a different balance')
  assert.equal(Number(a.equity.toFixed(2)), 1039.44)
})

check('a fee is an expense: it reduces net profit, never the principal', () => {
  // Same trade with and without fees. The principal is untouched; the net profit
  // absorbs the cost; gross is the pre-cost figure fees are charged against.
  const withFees = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 34.96)
  assert.equal(withFees.seed, 1000, 'fees do not move the principal')
  assert.equal(Number(withFees.gross.toFixed(2)), 74.40, 'gross = net + fees')
  assert.equal(Number(withFees.net.toFixed(2)), 39.44, 'net is already after fees')
  assert.ok(withFees.gross > withFees.net, 'the expense is deducted from gross, not added')
})

check('reserved cash is reported, never folded into the equity', () => {
  // 1050 cash with 10.56 tied up in resting buys is 1039.44 of free cash. The
  // reservation is surfaced on its own for LIVE; it is not profit either way.
  const v = balanceView(balance({ balance: 1050, reserved: 10.56, available: 1039.44, seed: 1000 }), 39.44, 0)
  assert.equal(v.reserved, 10.56)
  assert.equal(v.available, 1039.44)
  assert.equal(Number(v.equity.toFixed(2)), 1039.44, 'a reservation is not profit')
})

check('an unknown principal yields no equity, not a fabricated one', () => {
  // LIVE, or a core older than the `seed` field. Treating the missing principal
  // as 0 would report the whole balance as a drift against a principal of zero,
  // which is why `equity` is null rather than the cash figure.
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: null }), 39.44, 34.96)
  assert.equal(v.seed, null)
  assert.equal(v.equity, null)
  // The cash ledger and the cost are still known and still reported — this is
  // the case where the ledger is the figure the panel shows.
  assert.equal(v.cash, 1073.29)
  assert.equal(Number(v.gross.toFixed(2)), 74.40)
})

check('a missing balance object degrades to zeros, not NaN', () => {
  const v = balanceView(undefined, 0, 0)
  assert.deepEqual([v.cash, v.reserved, v.available], [0, 0, 0])
  assert.equal(v.equity, null)
  assert.ok(Number.isFinite(v.gross), 'gross must stay a number')
})

check('a loss keeps its sign through equity and net', () => {
  const v = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), -50, 0)
  assert.equal(Number(v.equity.toFixed(6)), 950)
  assert.equal(Number(v.net.toFixed(6)), -50)
  assert.ok(v.equity < v.seed, 'a loss lands below the principal')
})

console.log('\nclosed-trade dedupe')

check('distinct trades that share a per-boot counter id are both kept', () => {
  // The live log: 267 rows, 240 distinct ids, `hft-1` standing for six different
  // trades. Keying on the id deleted 27 real trades and $9.24 of net PnL, so the
  // equity curve ended at $38.72 under a header reading $47.96.
  const rows = [
    { id: 'hft-1', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: -0.75 },
    { id: 'hft-1', asset: 'ETH', entryTime: 300, exitTime: 400, netPnlUsd: 0.85 },
    { id: 'hft-1', asset: 'SOL', entryTime: 500, exitTime: 600, netPnlUsd: 3.18 },
  ]
  const out = dedupeTrades(rows)
  assert.equal(out.length, 3, 'three distinct trades, one reused counter id')
  const net = Number(out.reduce((a, r) => a + r.netPnlUsd, 0).toFixed(2))
  assert.equal(net, 3.28, 'every trade counts toward the net')
})

check('a genuine re-append of the SAME trade still collapses', () => {
  // Same asset and same entry/exit timestamps: one trade written twice.
  const rows = [
    { id: 'hft-7', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: 5 },
    { id: 'hft-7', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: -1 },
  ]
  const out = dedupeTrades(rows)
  assert.equal(out.length, 1, 'the same trade is not counted twice')
  assert.equal(out[0].netPnlUsd, -1, 'the last write wins')
})

check('the identity key is timestamps+asset, not the counter id', () => {
  assert.equal(
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    tradeIdentity({ id: 'hft-9', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    'the same trade under different counter values is one trade',
  )
  assert.notEqual(
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 3 }),
    'a different exit is a different trade even at the same id',
  )
})

check('rows without timestamps fall back to the counter, never all collapse', () => {
  // An older core omits entryTime/exitTime; the counter is the only key left, so
  // two rows under one id collapse — a possible over-collapse, but the log's rows
  // are genuinely indistinguishable at that point.
  const rows = [
    { id: 'hft-1', asset: 'BTC', netPnlUsd: 1 },
    { id: 'hft-2', asset: 'ETH', netPnlUsd: 2 },
  ]
  assert.equal(dedupeTrades(rows).length, 2, 'distinct ids stay distinct')
  assert.equal(tradeIdentity(rows[0]), 'i:hft-1', 'falls back to the counter')
  const sameId = [{ id: 'hft-1', asset: 'BTC' }, { id: 'hft-1', asset: 'BTC' }]
  assert.equal(dedupeTrades(sameId).length, 1)
})

check('insertion order is preserved so the curve stays chronological', () => {
  const rows = [
    { id: 'hft-2', asset: 'A', entryTime: 2, exitTime: 3 },
    { id: 'hft-1', asset: 'B', entryTime: 1, exitTime: 2 },
    { id: 'hft-1', asset: 'C', entryTime: 5, exitTime: 6 },
  ]
  assert.deepEqual(dedupeTrades(rows).map((r) => r.asset), ['A', 'B', 'C'])
})

check('the deduped list no longer under-counts the summary', () => {
  // The live shape: 267 rows, 240 unique ids, all rows distinct trades. After
  // the fix the deduped sum must equal the raw sum — which is what the kernel's
  // own summary reports, so the curve and the header agree.
  const rows = []
  for (let i = 1; i <= 240; i++) {
    rows.push({ id: `hft-${i}`, asset: 'BTC', entryTime: i * 10, exitTime: i * 10 + 5, netPnlUsd: 0.1 })
  }
  for (let i = 1; i <= 27; i++) {
    rows.push({ id: `hft-${i}`, asset: 'ETH', entryTime: 1e6 + i, exitTime: 1e6 + i + 5, netPnlUsd: 0.2 })
  }
  const sum = (xs) => Number(xs.reduce((a, r) => a + r.netPnlUsd, 0).toFixed(6))
  const deduped = dedupeTrades(rows)
  assert.equal(rows.length, 267)
  assert.equal(deduped.length, 267, 'no distinct trade is dropped')
  assert.equal(sum(deduped), sum(rows), 'the curve total now matches the summary')
})

check('an empty or absent list stays empty', () => {
  assert.deepEqual(dedupeTrades([]), [])
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
