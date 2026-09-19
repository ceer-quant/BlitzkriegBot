/**
 * Regression check for the panel's strategy provenance label
 * (`src/lib/strategy-source.ts`).
 *
 * The defect this pins is a *fabricated* label. The strategies page has three
 * data paths, and the lowest-fidelity one — `/api/plugins`, i.e. `strategy.list`,
 * which returns only `{name, enabled}` — was filling the missing provenance with
 * `row.kind ?? 'builtin'`. Both halves of that expression are wrong:
 *
 *   - `kind` is the market class on a plugin row (spot/futures/options); it is
 *     not a strategy field at all, so it was always undefined here; and
 *   - `'builtin'` is a category that no longer exists. PR-B (#114) made the
 *     kernel ship ZERO strategies, so every strategy is an external cdylib
 *     reporting `source = "dylib:<path>"`.
 *
 * The live consequence, observed on a running deployment: the three strategies
 * that had been migrated to cdylibs were still shown as 内建 (builtin), because
 * the running core predated the migration AND the fallback would have said
 * `builtin` regardless. The second half is what this check locks down — a label
 * must never claim a provenance the data does not carry.
 *
 *   cd ui/webapp/webui && npm run check:source
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

const { strategySource, registryStrategyRows, UNKNOWN_SOURCE } =
  await import('../src/lib/strategy-source.ts')

console.log('strategy provenance — never invent a source')

check('a real source passes through unchanged', () => {
  assert.equal(
    strategySource('dylib:user_layer/strategies/target/release/libspread_arb_strategy.dylib'),
    'dylib:user_layer/strategies/target/release/libspread_arb_strategy.dylib',
  )
  assert.equal(strategySource('test'), 'test')
})

check('a missing source is unknown, NOT builtin', () => {
  // The load-bearing assertion: 'builtin' is the string the defect produced.
  for (const missing of [undefined, null, '', '   ']) {
    const got = strategySource(missing)
    assert.notEqual(got, 'builtin', `must not fabricate 'builtin' (input ${JSON.stringify(missing)})`)
    assert.equal(got, UNKNOWN_SOURCE)
  }
})

check('registry rows (strategy.list) report unknown provenance', () => {
  // Exactly what /api/plugins serves for the three migrated strategies:
  // name + enabled, nothing else.
  const rows = registryStrategyRows([
    { name: 'spread_arb', enabled: true },
    { name: 'trend_follow', enabled: false },
    { name: 'mean_reversion', enabled: false },
  ])
  assert.equal(rows.length, 3)
  for (const r of rows) {
    assert.notEqual(r.source, 'builtin', `${r.name} must not be labelled builtin`)
    assert.equal(r.source, UNKNOWN_SOURCE)
  }
  assert.equal(rows[0].enabled, true)
  assert.equal(rows[1].enabled, false)
})

check('a market-class `kind` is not mistaken for provenance', () => {
  // The old code read `kind`. Even when present it is a market class, so it must
  // not become the source — that would be the same defect in a new costume.
  const rows = registryStrategyRows([{ name: 'x', kind: 'spot', enabled: true }])
  assert.equal(rows[0].source, UNKNOWN_SOURCE)
  assert.notEqual(rows[0].source, 'spot')
})

check('registry rows carry honest zero counters, not fabricated ones', () => {
  const [r] = registryStrategyRows([{ name: 'a', enabled: true }])
  for (const k of ['ordersPlaced', 'ordersRejected', 'limitRejected', 'blockedTiming', 'blockedMomentum', 'closedTrades', 'wins', 'losses']) {
    assert.equal(r[k], 0, `${k} must be zero`)
  }
  assert.deepEqual(r.gateExemptions, [])
  assert.equal(r.rejectionCauses, null)
})

check('an empty registry yields no rows', () => {
  assert.deepEqual(registryStrategyRows([]), [])
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
