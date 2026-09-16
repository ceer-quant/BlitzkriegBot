/**
 * Regression check for the panel's feed-liveness signal (`src/lib/feed.ts`).
 *
 * The defect this pins: a dead market-data feed was invisible. The core keeps
 * serving the last orderbook it holds, so every price on screen stayed plausible
 * while being hours old, and the counters that look like activity climb anyway
 * (the tick loop drives `evaluations`; a round's `ageSec` and `slot` come from the
 * local clock). The panel showed four frozen pairs as if the market were merely
 * quiet.
 *
 * Properties asserted here:
 *
 *   - liveness is judged on the orderbook counters alone, and their sum is what
 *     the store records, so either one moving counts as progress;
 *   - a round's `slot` is NOT part of the progress value, because it advances every
 *     15m off the wall clock and would therefore reset the staleness clock twice an
 *     hour — hiding a feed that is down all day;
 *   - nothing observed yet is "unknown", not "stale", so the banner does not fire
 *     on a cold page load before the first poll returns;
 *   - staleness trips only past the threshold, and the reported age is monotone.
 *
 *   cd ui/webapp/webui && npm run check:feed
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

const { feedProgress, feedStaleness, FEED_STALE_MS } = await import('../src/lib/feed.ts')

console.log('feed progress — only market-data counters count')

check('either book counter moving is progress', () => {
  const base = feedProgress(100, 200)
  assert.equal(feedProgress(101, 200), base + 1, 'books alone must advance')
  assert.equal(feedProgress(100, 250), base + 50, 'tops alone must advance')
  assert.equal(feedProgress(100, 200), base, 'no movement must not advance')
})

check('a round rollover is not progress', () => {
  // The failure this prevents: with `slot` folded in, each 15m round boundary
  // looks like the feed came back, so an all-day outage reads as healthy twice
  // an hour. The progress value is a function of the two book counters only —
  // there is no parameter through which `slot` could enter.
  assert.equal(feedProgress.length, 2, 'progress must take books and tops, nothing else')
  const frozen = feedProgress(5000, 9000)
  assert.equal(feedProgress(5000, 9000), frozen, 'unchanged books must not advance progress')
  // A fresh slot carries no books movement, so progress is identical across it.
  assert.equal(feedProgress(5000, 9000), frozen)
})

check('missing counters are zero, not NaN', () => {
  assert.equal(feedProgress(undefined, undefined), 0)
  assert.equal(feedProgress(null, 150), 150)
  assert.equal(feedProgress('12', 3), 15, 'numeric strings from JSON are coerced')
  assert.equal(Number.isFinite(feedProgress(undefined, Number.NaN)), true)
})

console.log('feed staleness — silence past the threshold')

check('not yet observed is unknown, never stale', () => {
  // Crying "stale" here would flash the outage banner on every cold load.
  const s = feedStaleness(null, Date.now())
  assert.equal(s.stale, false)
  assert.equal(s.ageMs, null)
  assert.equal(s.label, '行情状态未知')
})

check('a moving feed is never stale', () => {
  // Bases chosen above the threshold so `now - age` stays positive throughout.
  const now = 1_000_000
  for (const age of [0, 1, 1_000, FEED_STALE_MS - 1]) {
    const s = feedStaleness(now - age, now)
    assert.equal(s.stale, false, `age ${age}ms must not be stale`)
    assert.equal(s.ageMs, age)
  }
})

check('staleness trips just past the threshold', () => {
  const now = 1_000_000
  assert.equal(feedStaleness(now - FEED_STALE_MS, now).stale, false, 'boundary is not yet stale')
  assert.equal(feedStaleness(now - FEED_STALE_MS - 1, now).stale, true)
})

check('age is monotone and never negative', () => {
  const now = 5_000_000
  let prev = -1
  for (const age of [0, 500, 10_000, 120_000, 9_843_900]) {
    const s = feedStaleness(now - age, now)
    assert.ok(s.ageMs >= prev, 'age must not go backwards as the feed stays dead')
    prev = s.ageMs
  }
  // A clock that went backwards must not render a negative age.
  assert.equal(feedStaleness(now + 5_000, now).ageMs, 0)
})

check('the live outage is reported as stale', () => {
  // As measured: archive last event 2026-09-16T05:38:45.697Z, panel opened well
  // after. This is the case the banner exists for.
  const now = 1789541000000
  const lastChange = Date.parse('2026-09-16T05:38:45.697Z')
  const s = feedStaleness(lastChange, now)
  assert.equal(s.stale, true)
  assert.match(s.label, /无行情更新/, `label must name the outage, got ${s.label}`)
  assert.match(s.label, /≥/, 'the age is a lower bound (no last-event time on the wire)')
})

check('the threshold sits just above the engine\'s own staleness tolerance', () => {
  // The core refuses to trade an orderbook older than max_orderbook_stale_ms
  // (8000ms, core/blitzkrieg_core/src/engine.rs). The panel must not present as
  // live what the engine has already stopped trusting, but must not trip before
  // the engine does either — otherwise it flags healthy data the core still acts on.
  assert.equal(FEED_STALE_MS, 10_000)
  assert.ok(FEED_STALE_MS > 8_000, 'must not flag data the engine would still trade')
})

check('a fresh reading renders an age even while healthy', () => {
  // The always-on indicator: a climbing number is what makes the next outage
  // self-evident before anyone thinks to ask about it.
  const now = 10_000_000
  const healthy = feedStaleness(now - 3_000, now)
  assert.equal(healthy.stale, false)
  assert.equal(healthy.ageLabel, '行情 3s 前更新')

  const dead = feedStaleness(now - 9_843_900, now)
  assert.equal(dead.ageLabel, '行情已停更 ≥ 2h44m')
  assert.match(dead.ageLabel, /停更/)
})

check('the indicator does not merely repeat the banner', () => {
  // Both are on screen together, so identical text reads as a rendering fault.
  const s = feedStaleness(0, 4 * 60 * 60 * 1000)
  assert.notEqual(s.ageLabel, s.label)
  assert.match(s.label, /无行情更新/, 'the banner keeps the explanatory headline')
})

check('the unknown state is labelled as unknown in both fields', () => {
  const s = feedStaleness(null, 0)
  assert.equal(s.ageLabel, '行情状态未知')
  assert.equal(s.stale, false)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
