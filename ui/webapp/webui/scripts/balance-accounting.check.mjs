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
 *     between it and 本金 ＋ 净利润 is labelled as a kernel basis difference rather
 *     than as a contradiction in the balance — the ledger reseeds from the
 *     principal every boot while the equity carries all persisted history, so
 *     the gap is signed in both directions and its copy must follow the sign;
 *   - an unknown principal yields NO equity and NO gap, so a core that does not
 *     report its seed is never accused of drifting against a principal of zero;
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

const { balanceView, cashGapDirection, cashGapShort, cashGapDetail } =
  await import('../src/lib/balance.ts')
const { dedupeTrades, tradeIdentity } = await import('../src/lib/trades.ts')

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
  // realized net 39.438323, fees 34.961677. Crucially the headline stays
  // 1039.44 — the gap does NOT inflate it.
  const net = 39.438323
  const fees = 34.961677
  const v = balanceView(balance({ balance: 1073.2887, available: 1073.2887, seed: 1000 }), net, fees)
  assert.equal(Number(v.equity.toFixed(2)), 1039.44)
  assert.equal(Number(v.cashGap.toFixed(2)), 33.85)
  assert.equal(v.gapMaterial, true)
  assert.equal(v.gapDir, 'above')
})

check('a gap below the equity is NOT described as 高于 (the reported bug)', () => {
  // The book as measured on 2026-09-16 23:35: seed 1000, core cash 1002.06,
  // all-time net 48.21 → equity 1048.21, so the gap is −46.15. The panel used
  // to print a hardcoded 「高于真实余额 −$46.15」 — a self-contradiction: it
  // claimed the cash sat above the equity while the figure beside it was
  // negative. The direction has to come off the sign.
  const v = balanceView(balance({ balance: 1002.06, available: 1002.06, seed: 1000 }), 48.21, 38.69)
  assert.equal(Number(v.equity.toFixed(2)), 1048.21)
  assert.equal(Number(v.cashGap.toFixed(2)), -46.15)
  assert.equal(v.gapMaterial, true)
  assert.equal(v.gapDir, 'below', 'cash 1002.06 is BELOW an equity of 1048.21')

  const label = cashGapShort(v)
  assert.match(label, /低于真实余额/, 'the copy must follow the sign')
  assert.doesNotMatch(label, /高于/, 'the exact wording that shipped broken')
  assert.match(label, /−\$46\.15/, 'and it keeps the signed figure')
})

check('the gap direction is the sign of the gap, at both ends', () => {
  const above = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: 1000 }), 39.44, 0)
  // Principal 1000 with a −60 net is an equity of 940; cash of 900 sits 40 below it.
  const below = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), -60, 0)
  const level = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 0)
  assert.equal(Number(above.cashGap.toFixed(2)), 33.85)
  assert.equal(Number(below.cashGap.toFixed(2)), -40)
  assert.equal(cashGapDirection(above.cashGap), 'above')
  assert.equal(cashGapDirection(below.cashGap), 'below')
  assert.equal(cashGapDirection(level.cashGap), 'level')
  assert.equal(cashGapDirection(null), null, 'no comparison, no direction')
  assert.match(cashGapShort(above), /^高于真实余额 \+/)
  assert.match(cashGapShort(below), /^低于真实余额 −/)
  assert.equal(cashGapShort(level), '与真实余额一致')
})

check('with no principal the gap copy is empty, not a fabricated direction', () => {
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: null }), 39.44, 34.96)
  assert.equal(v.gapDir, null)
  assert.equal(cashGapShort(v), '')
  assert.equal(cashGapDetail(v), '')
})

check('the gap explanation names the restart basis, not uncharged fees', () => {
  // The shipped tooltip asserted 「差额≈未入账的手续费 $38.69」 next to a −$46.15
  // gap — wrong in mechanism AND sign. The measured mechanism: the cash ledger
  // reseeds from --seed-balance every boot and covers only the fills since,
  // while the panel's equity carries all persisted history. Split the live log
  // by the core's boot: pre-boot net 46.12, post-boot net 2.09, and
  // 1000 + 2.09 = 1002.09 ≈ the reported 1002.06, so the −46.15 gap is the
  // pre-boot profit, not the 38.69 of fees.
  const preBoot = { n: 260, net: 46.12, fees: 37.88 }
  const postBoot = { n: 6, net: 2.09, fees: 0.81 }
  const allNet = preBoot.net + postBoot.net
  const v = balanceView(balance({ balance: 1002.06, available: 1002.06, seed: 1000 }), allNet, 38.69)

  // The arithmetic that proves the split, pinned so the story cannot drift.
  assert.equal(Number((1000 + postBoot.net).toFixed(2)), 1002.09, 'cash ≈ 本金 ＋ 本次启动后净利')
  assert.equal(Number(v.cashGap.toFixed(2)), -46.15)
  assert.equal(Number(preBoot.net.toFixed(2)), 46.12, 'gap ≈ 本次启动前净利, NOT the fee total')
  assert.notEqual(Math.abs(v.cashGap).toFixed(2), '38.69', 'the fee explanation does not hold')

  const detail = cashGapDetail(v)
  assert.match(detail, /每次启动都从本金重新累计/, 'states the actual basis split')
  assert.match(detail, /本次启动之前的历史净利/, 'points at the pre-boot profit')
  assert.match(detail, /手续费/, 'still mentions fees as a secondary contributor')
  assert.doesNotMatch(detail, /差额≈未入账的手续费/, 'the disproven claim is gone')
})

check('a sub-cent gap is level, so the copy does not invent a direction', () => {
  const v = balanceView(balance({ balance: 1039.442, available: 1039.442, seed: 1000 }), 39.44, 0)
  assert.equal(v.gapDir, 'level')
  assert.equal(cashGapShort(v), '与真实余额一致')
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
