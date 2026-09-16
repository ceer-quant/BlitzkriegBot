/**
 * Regression check for the panel's balance accounting (`src/lib/balance.ts`,
 * `src/lib/trades.ts`).
 *
 * These two modules are what turned "模拟余额 1070 with 净利润 40" from a
 * contradiction into an explained number, and then (rewritten) into the right
 * question being asked in the first place. The properties pinned here:
 *
 *   - the headline balance is 本金 ＋ 净利润, so a fee can never move the
 *     principal — it is an expense already deducted from net profit;
 *   - fees are reported as a cost, and gross profit is `net + fees` — the figure
 *     the cost is charged against;
 *   - the core's cash ledger is kept as a separate, operative number, and any gap
 *     between it and 本金 ＋ 净利润 is labelled as the kernel's cash basis rather
 *     than as a contradiction in the balance;
 *   - an unknown principal yields NO equity and NO gap, so a core that does not
 *     report its seed is never accused of drifting against a principal of zero;
 *   - closed-trade rows collapse by id, because `hft-N` restarts every boot
 *     against an append-only log and summing it raw inflates both the trade
 *     count and the net PnL.
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
const { dedupeTrades } = await import('../src/lib/trades.ts')

const balance = (over) => ({ balance: 0, reserved: 0, available: 0, seed: null, ...over })

console.log('balance basis — 本金 ＋ 净利润')

check('the headline is 本金 ＋ 净利润, not the core cash ledger', () => {
  // The case that started this: cash 1073.29, principal 1000, net 39.44. The
  // balance shown is 1039.44; cash is a separate number, not the balance.
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: 1000 }), 39.44, 34.96)
  assert.equal(Number(v.equity.toFixed(2)), 1039.44)
  assert.equal(v.cash, 1073.29)
  assert.notEqual(v.equity, v.cash, 'an unexplained cash surplus is not the balance')
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

check('equity reconciles exactly when the core charges fees to cash', () => {
  // A core whose Ledger deducts the fees lands cash on the equity; then the gap
  // is zero and nothing is flagged.
  const v = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 34.96)
  assert.equal(Number(v.cashGap.toFixed(6)), 0)
  assert.equal(v.gapMaterial, false)
})

check('reserved cash is not counted as profit', () => {
  // 1050 cash with 10.56 tied up in resting buys is 1039.44 of free cash. The
  // gap must look past the reservation.
  const v = balanceView(balance({ balance: 1050, reserved: 10.56, available: 1039.44, seed: 1000 }), 39.44, 0)
  assert.equal(Number(v.cashGap.toFixed(6)), 0)
  assert.equal(v.gapMaterial, false)
})

check('cash above equity is reported as a kernel cash-basis gap', () => {
  // The live dry books measured on 2026-09-16: seed 1000, cash 1073.29,
  // realized net 39.438323, fees 34.961677. The gap is the taker fee the running
  // core never charged to cash (34.96) less the per-share rounding shortfall
  // (1.11). Crucially the headline stays 1039.44 — the gap does NOT inflate it.
  const net = 39.438323
  const fees = 34.961677
  const v = balanceView(balance({ balance: 1073.2887, available: 1073.2887, seed: 1000 }), net, fees)
  assert.equal(Number(v.equity.toFixed(2)), 1039.44)
  assert.equal(Number(v.cashGap.toFixed(2)), 33.85)
  assert.equal(v.gapMaterial, true)
  assert.equal(Number((fees - 1.11).toFixed(2)), 33.85, 'gap is the uncharged fee less the shortfall')
})

check('an unknown principal yields no equity and no gap, not a fabricated one', () => {
  // LIVE, or a core older than the `seed` field (the core running here started
  // before it). Treating the missing principal as 0 would report the entire
  // balance as a gap.
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: null }), 39.44, 34.96)
  assert.equal(v.seed, null)
  assert.equal(v.equity, null)
  assert.equal(v.cashGap, null)
  assert.equal(v.gapMaterial, false, 'absence of evidence is not a gap')
  // The cash ledger and the cost are still known and still reported.
  assert.equal(v.cash, 1073.29)
  assert.equal(Number(v.gross.toFixed(2)), 74.40)
})

check('a missing balance object degrades to zeros, not NaN', () => {
  const v = balanceView(undefined, 0, 0)
  assert.deepEqual([v.cash, v.reserved, v.available], [0, 0, 0])
  assert.equal(v.equity, null)
  assert.equal(v.cashGap, null)
  assert.ok(Number.isFinite(v.gross), 'gross must stay a number')
})

check('sub-cent gaps are rounding, not a basis difference', () => {
  const v = balanceView(balance({ balance: 1039.442, available: 1039.442, seed: 1000 }), 39.44, 0)
  assert.equal(v.gapMaterial, false)
})

check('a loss keeps its sign through equity and gap', () => {
  const v = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), -50, 0)
  assert.equal(Number(v.equity.toFixed(6)), 950)
  assert.equal(Number(v.cashGap.toFixed(6)), -50)
  assert.equal(v.gapMaterial, true)
})

console.log('\nclosed-trade dedupe')

check('cross-run id collisions collapse to the last write', () => {
  const rows = [
    { id: 'hft-1', netPnlUsd: 5 },
    { id: 'hft-1', netPnlUsd: -1 },
    { id: 'hft-2', netPnlUsd: 2 },
  ]
  const out = dedupeTrades(rows)
  assert.deepEqual(out.map((r) => r.id), ['hft-1', 'hft-2'], 'insertion order preserved')
  assert.equal(out[0].netPnlUsd, -1, 'the surviving record is the current run')
})

check('the raw list double-counts what the deduped list does not', () => {
  // Shape of the live log: 256 rows over 240 unique ids.
  const rows = []
  for (let i = 1; i <= 240; i++) rows.push({ id: `hft-${i}`, netPnlUsd: 0.1 })
  for (let i = 1; i <= 16; i++) rows.push({ id: `hft-${i}`, netPnlUsd: 0.1 })
  const sum = (xs) => Number(xs.reduce((a, r) => a + r.netPnlUsd, 0).toFixed(6))
  const deduped = dedupeTrades(rows)
  assert.equal(rows.length, 256)
  assert.equal(deduped.length, 240, 'matches the kernel\'s own closedTrades counter')
  assert.equal(sum(deduped), 24)
  assert.ok(sum(rows) > sum(deduped), 'the raw sum inflates net PnL')
})

check('an empty or absent list stays empty', () => {
  assert.deepEqual(dedupeTrades([]), [])
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
