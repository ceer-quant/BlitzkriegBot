/**
 * E11 first-run tour — the「三步首次引导可完成」验收, executable.
 *
 * The tour's one rule: it must ask nothing the panel can already answer. Steps
 * complete from live state (active market feed, any enabled strategy), the wrap
 * step is always done, so the card retires itself the moment both real steps
 * are satisfied — without a click — and persists the retirement. This check
 * runs the REAL state machine (`src/lib/onboarding.ts`) over synthetic panel
 * states and the real localStorage contract, and statically pins the mounting.
 *
 *   cd ui/webapp/webui && npm run check:first-run
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

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

const {
  onboardSteps, currentStep, onboardDone, markOnboardDone, DONE_KEY,
} = await import('../src/lib/onboarding.ts')

console.log('step machine — exactly the two things a new operator must do')

check('three steps, fixed ids and order', () => {
  const steps = onboardSteps({ activeMarket: null, anyStrategyEnabled: false })
  assert.deepEqual(steps.map((s) => s.id), ['market', 'strategy', 'wrap'])
  assert.deepEqual(steps.map((s) => s.index), [0, 1, 2])
})

check('nothing done → the tour opens on 选定行情源', () => {
  const cur = currentStep(onboardSteps({ activeMarket: null, anyStrategyEnabled: false }))
  assert.equal(cur?.id, 'market')
  assert.equal(cur?.target, 'plugins')
  assert.equal(cur?.cta, '去插件页')
})

check('market active → next undone step is 启用策略', () => {
  const cur = currentStep(onboardSteps({ activeMarket: 'polymarket', anyStrategyEnabled: false }))
  assert.equal(cur?.id, 'strategy')
  assert.equal(cur?.target, 'strategies')
})

check('a feed that merely EXISTS but is not active is not done', () => {
  // The completion condition is an active flag, not a non-empty registry —
  // a stalled gateway listing its feed must not complete the step for the user.
  const steps = onboardSteps({ activeMarket: null, anyStrategyEnabled: false })
  assert.equal(steps[0].done, false)
})

check('both real steps done → no current step (tour retires itself)', () => {
  const cur = currentStep(onboardSteps({ activeMarket: 'polymarket', anyStrategyEnabled: true }))
  assert.equal(cur, null)
})

check('strategy-only done still opens on the market step', () => {
  const cur = currentStep(onboardSteps({ activeMarket: null, anyStrategyEnabled: true }))
  assert.equal(cur?.id, 'market')
})

check('wrap step is always done and carries no CTA', () => {
  const wrap = onboardSteps({ activeMarket: null, anyStrategyEnabled: false }).at(-1)
  assert.equal(wrap?.done, true)
  assert.equal(wrap?.cta, null)
  assert.equal(wrap?.target, null)
})

check('every undone step has a CTA that lands on its target tab', () => {
  for (const state of [
    { activeMarket: null, anyStrategyEnabled: false },
    { activeMarket: null, anyStrategyEnabled: true },
    { activeMarket: 'x', anyStrategyEnabled: false },
  ]) {
    for (const s of onboardSteps(state).filter((s) => !s.done)) {
      assert.ok(s.cta, `step ${s.id} has no CTA`)
      assert.ok(s.target === 'plugins' || s.target === 'strategies', `bad target ${s.target}`)
    }
  }
})

console.log('retirement — once done or skipped, it never comes back')

/** Map-backed localStorage stand-in (the module touches it lazily). */
function freshStorage(initial = {}) {
  const m = new Map(Object.entries(initial))
  globalThis.localStorage = {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => void m.set(k, String(v)),
    removeItem: (k) => void m.delete(k),
  }
  return m
}

check('a fresh browser has not onboarded', () => {
  freshStorage()
  assert.equal(onboardDone(), false)
})

check('markOnboardDone persists and onboardDone reads it back', () => {
  const m = freshStorage()
  markOnboardDone()
  assert.equal(onboardDone(), true)
  assert.equal(m.get(DONE_KEY), '1')
})

check('a missing-storage browser (private mode) is treated as done', () => {
  // The getter must throw like a blocked localStorage, not return undefined.
  globalThis.localStorage = {
    getItem() { throw new Error('blocked') },
    setItem() { throw new Error('blocked') },
    removeItem() { throw new Error('blocked') },
  }
  assert.equal(onboardDone(), true, 'stay out of the way when storage is unusable')
  // And the write must not throw through the caller.
  markOnboardDone()
})

console.log('mounting — the card exists and retires itself')

const shell = read('src', 'App.vue')
const tour = read('src', 'components', 'OnboardingTour.vue')

check('App mounts the tour and wires navigation', () => {
  assert.ok(shell.includes('<OnboardingTour'), 'tour not mounted in App.vue')
  assert.ok(/@navigate="goOnboard"/.test(shell), 'navigate event not wired')
})

check('the card retires from live state, not only from the skip button', () => {
  assert.ok(tour.includes('markOnboardDone()'), 'no self-retire path')
  assert.ok(/watch\(step/.test(tour), 'no watch on the live step')
})

check('the card waits for data before showing', () => {
  // Guard against the tour flashing over a still-loading panel on cold login.
  assert.ok(tour.includes('dataArrived'), 'no data-arrival gate')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
