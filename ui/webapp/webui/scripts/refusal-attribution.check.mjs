/**
 * Regression check for the panel's refusal accounting (`src/lib/rejections.ts`).
 *
 * The defect this pins is a *reading* defect, not an arithmetic one. The panel
 * showed "下单 270 / 拒单 34,480" with "限额/时机/动量挡 0" beneath it and labelled
 * the 34,480 "笔被风控拒绝". The natural reading — that the four named buckets
 * should account for the big number, and that their being zero means the risk layer
 * reported nothing — is exactly wrong. The two families are disjoint:
 *
 *   - `ordersRejected` counts refusals *inside* `Core::place()`: the risk gate, kill
 *     switch, loss breaker, position capacity, "already in asset", the SL/exit/loss/
 *     asset cooldowns, ledger reservation, OME duplicate suppression.
 *   - `limitRejected` / `blockedTiming` / `blockedMomentum` count refusals that
 *     happen *before* an order exists, and `continue` before `place()` is reached.
 *
 * Properties asserted here:
 *
 *   - `rejected` is NOT the sum of the named buckets, in either direction;
 *   - `placed + rejected` is the admitted-signal count and the denominator of the
 *     refusal rate, which is what makes 34,480/34,750 legible as 99.2%;
 *   - refusals with no cause buckets are flagged as unattributed rather than
 *     reported as a clean zero or an all-clear shield (the live core predates
 *     `rejectionCauses`, #65, so its map arrives null while its count is 34,480);
 *   - fleet totals add the counters and deliberately drop the cause breakdown,
 *     which is per-strategy and cannot be summed into a fleet figure.
 *
 *   cd ui/webapp/webui && npm run check:rejections
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

const { refusalProfile, refusalTotals } = await import('../src/lib/rejections.ts')

console.log('refusal attribution — two disjoint families')

check('the live case: 270 placed, 34,480 refused, all named buckets zero', () => {
  // Exactly what the panel was rendering when the question came up.
  const p = refusalProfile({
    ordersPlaced: 270, ordersRejected: 34480,
    limitRejected: 0, blockedTiming: 0, blockedMomentum: 0,
    rejectionCauses: null,
  })
  assert.equal(p.admitted, 34750, 'admitted = placed + rejected')
  assert.equal(p.preGate, 0, 'nothing was stopped before an order existed')
  assert.equal(p.refuseRate.toFixed(1), '99.2')
  // The load-bearing assertion: the big number does not decompose into the zeros.
  // No arrangement of the named buckets reproduces it — they are a disjoint family.
  assert.notEqual(p.rejected, p.preGate, 'the count must not equal the sum of the zeros')
  assert.equal(p.rejected + p.preGate, 34480, 'the two families are added, never nested')
})

check('no arrangement of the named buckets reproduces the refusal count', () => {
  // Guards the exact misreading the page invited: that the buckets are a
  // breakdown of `ordersRejected`. Whatever their values, they are separate.
  const p = refusalProfile({
    ordersPlaced: 10, ordersRejected: 5,
    limitRejected: 2, blockedTiming: 3, blockedMomentum: 4,
  })
  assert.equal(p.preGate, 9)
  assert.notEqual(p.rejected, p.preGate, 'a page must never present these as a breakdown')
  for (const bucket of [p.limitRejected, p.timing, p.momentum]) {
    assert.notEqual(bucket, p.rejected, 'no single named bucket equals the refusal count')
  }
})

check('admitted counts signals that became orders, preGate counts those that did not', () => {
  const p = refusalProfile({
    ordersPlaced: 100, ordersRejected: 20,
    limitRejected: 7, blockedTiming: 1, blockedMomentum: 2,
  })
  assert.equal(p.admitted, 120)
  assert.equal(p.preGate, 10)
  assert.equal(p.refuseRate, (20 / 120) * 100)
})

check('a zero-refusal strategy has a zero rate, not a NaN', () => {
  const p = refusalProfile({ ordersPlaced: 0, ordersRejected: 0 })
  assert.equal(p.refuseRate, 0)
  assert.equal(Number.isFinite(p.refuseRate), true)
})

check('refusals with no buckets are flagged unattributed', () => {
  // The live core: rejects 34,480 but reports no causes at all.
  const missing = refusalProfile({ ordersRejected: 34480, rejectionCauses: null })
  assert.equal(missing.causesMissing, true, 'must not read as "all clear"')
  assert.deepEqual(missing.causes, [])

  const empty = refusalProfile({ ordersRejected: 12, rejectionCauses: {} })
  assert.equal(empty.causesMissing, true, 'an empty map is equally unattributed')

  // No refusals at all is genuinely nothing to attribute.
  assert.equal(refusalProfile({ ordersRejected: 0, rejectionCauses: null }).causesMissing, false)
  // A reported cause is attributed, even if sparse relative to the total.
  assert.equal(
    refusalProfile({ ordersRejected: 12, rejectionCauses: { 'risk.breaker': 3 } }).causesMissing,
    false,
  )
})

check('cause buckets are sorted, positive-only, and numerically coerced', () => {
  const p = refusalProfile({
    ordersRejected: 100,
    rejectionCauses: { 'risk.breaker': '60', 'positions.already_in': 30, 'limit.other': 0, 'x': null },
  })
  assert.deepEqual(p.causes.map((c) => c.name), ['risk.breaker', 'positions.already_in'])
  assert.deepEqual(p.causes.map((c) => c.n), [60, 30])
})

check('the buckets that classify_rejection actually emits are all representable', () => {
  // The vocabulary from `classify_rejection` (core/blitzkrieg_core/src/service.rs):
  // a page must be able to render any of these without inventing a label.
  const live = {
    'risk:breaker': 5, 'positions.max_positions': 4, 'positions.already_in': 3,
    'positions.daily_loss': 2, 'positions.exitCooldown': 1, 'positions.lossCooldown': 1,
    'limit.positionCap': 1, 'limit.notionalCap': 1, 'limit.other': 1,
    'risk.perOrderCap': 1, 'risk.priceBand': 1, 'risk.other': 1,
    'ledger.reserve.other': 1, 'ome.other': 1, 'other.other': 1, 'killswitch.other': 1,
  }
  const p = refusalProfile({ ordersRejected: 100, rejectionCauses: live })
  assert.equal(p.causes.length, Object.keys(live).length)
  assert.equal(p.causesMissing, false)
})

console.log('fleet totals — counters add, causes do not')

check('fleet totals sum the counters', () => {
  const t = refusalTotals([
    { ordersPlaced: 270, ordersRejected: 34480, limitRejected: 0, blockedTiming: 0, blockedMomentum: 0 },
    { ordersPlaced: 30, ordersRejected: 44, limitRejected: 5, blockedTiming: 2, blockedMomentum: 1 },
  ])
  assert.equal(t.placed, 300)
  assert.equal(t.rejected, 34524)
  assert.equal(t.preGate, 8)
  assert.equal(t.admitted, 34824)
})

check('fleet totals carry no cause breakdown', () => {
  // A per-strategy bucket attributed to the whole fleet would be a misattribution.
  const t = refusalTotals([
    { ordersRejected: 10, rejectionCauses: { 'risk.breaker': 10 } },
    { ordersRejected: 5, rejectionCauses: { 'ome.other': 5 } },
  ])
  assert.deepEqual(t.causes, [], 'the fleet row must not claim a cause split')
  assert.equal(t.causesMissing, true, 'and must say the attribution is per-strategy')
})

check('totals over an empty fleet are zero, not undefined', () => {
  const t = refusalTotals([])
  assert.equal(t.placed, 0)
  assert.equal(t.rejected, 0)
  assert.equal(t.refuseRate, 0)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
