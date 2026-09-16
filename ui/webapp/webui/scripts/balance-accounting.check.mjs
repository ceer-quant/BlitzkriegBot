/**
 * Regression check for the panel's balance accounting (`src/lib/balance.ts`,
 * `src/lib/trades.ts`).
 *
 * These two modules are what turned "模拟余额 1070 with 净利润 40" from a
 * contradiction into an explained number, so the properties that make it
 * explained are pinned here:
 *
 *   - the reconciliation residual is `cash − reserved − seed − net`;
 *   - an unknown principal yields NO residual, so a core that does not report
 *     its seed is never accused of drift against a principal of zero;
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

const { reconcileBalance } = await import('../src/lib/balance.ts')
const { dedupeTrades } = await import('../src/lib/trades.ts')

const balance = (over) => ({ balance: 0, reserved: 0, available: 0, seed: null, ...over })

console.log('balance reconciliation')

check('books that agree reconcile to zero', () => {
  const r = reconcileBalance(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44)
  assert.equal(r.seed, 1000)
  assert.equal(Number(r.residual.toFixed(6)), 0)
  assert.equal(r.drifted, false)
})

check('reserved cash is not counted as profit', () => {
  // 1039.44 cash with 10.56 tied up in resting buys is 1028.88 of free cash;
  // the residual must look past the reservation to the principal.
  const r = reconcileBalance(balance({ balance: 1050, reserved: 10.56, available: 1039.44, seed: 1000 }), 39.44)
  assert.equal(Number(r.residual.toFixed(6)), 0)
  assert.equal(r.drifted, false)
})

check('an unknown principal reports no residual rather than a fabricated one', () => {
  // LIVE, or a core older than the `seed` field. Treating the missing principal
  // as 0 would report the entire balance as drift.
  const r = reconcileBalance(balance({ balance: 1073.29, available: 1073.29, seed: null }), 39.44)
  assert.equal(r.seed, null)
  assert.equal(r.residual, null)
  assert.equal(r.drifted, false, 'absence of evidence is not drift')
})

check('a missing balance object degrades to zeros, not NaN', () => {
  const r = reconcileBalance(undefined, 0)
  assert.deepEqual([r.balance, r.reserved, r.available], [0, 0, 0])
  assert.equal(r.residual, null)
})

check('a real cash-vs-record divergence is surfaced with its sign', () => {
  // The live dry books measured on 2026-09-16: seed 1000, cash 1073.29,
  // realized net 39.438323. The gap is the taker fee the running core never
  // charged to cash (34.96) less the per-share rounding shortfall (1.11).
  const net = 39.438323
  const r = reconcileBalance(balance({ balance: 1073.29, available: 1073.29, seed: 1000 }), net)
  assert.equal(Number(r.residual.toFixed(2)), 33.85)
  assert.equal(r.drifted, true)
  assert.equal(Number((34.96 - 1.11).toFixed(2)), 33.85, 'residual is the uncharged fee less the shortfall')
})

check('sub-cent residuals are rounding, not drift', () => {
  const r = reconcileBalance(balance({ balance: 1039.442, available: 1039.442, seed: 1000 }), 39.44)
  assert.equal(r.drifted, false)
})

check('a loss side residual keeps its sign', () => {
  const r = reconcileBalance(balance({ balance: 900, available: 900, seed: 1000 }), -50)
  assert.equal(Number(r.residual.toFixed(6)), -50)
  assert.equal(r.drifted, true)
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
