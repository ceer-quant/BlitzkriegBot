/**
 * Regression check for the panel's theme preference (`src/lib/theme.ts`).
 *
 * Runs the REAL module against a controllable `matchMedia`, which covers what a
 * browser pass cannot: the automation backend exposes no media emulation, so an
 * OS appearance flip is simulated by firing `change` on the same
 * `MediaQueryList` the module subscribed to.
 *
 *   cd ui/webapp/webui && npm run check:theme
 */
import assert from 'node:assert/strict'

/** Minimal MediaQueryList stand-in that records its subscribers. */
function fakeMql(initial) {
  const listeners = new Set()
  const legacy = new Set()
  return {
    matches: initial,
    media: '(prefers-color-scheme: dark)',
    addEventListener: (_type, fn) => void listeners.add(fn),
    removeEventListener: (_type, fn) => void listeners.delete(fn),
    // Safari < 14 path, present so the module's else-branch is exercised.
    addListener: (fn) => void legacy.add(fn),
    /** Simulate the OS switching appearance. */
    fire(matches) {
      this.matches = matches
      for (const fn of listeners) fn({ matches })
      for (const fn of legacy) fn({ matches })
    },
    listenerCount: () => listeners.size + legacy.size,
  }
}

const store = new Map()
const classes = new Set()
const style = {}
const mql = fakeMql(true)

// Globals must exist before the module evaluates: it reads localStorage and
// applies a theme at import time. Vue's runtime-dom also probes `document` at
// import time, so a few no-op element factories are needed.
globalThis.localStorage = {
  getItem: (k) => store.get(k) ?? null,
  setItem: (k, v) => void store.set(k, v),
  removeItem: (k) => void store.delete(k),
}
globalThis.window = { matchMedia: () => mql }
globalThis.document = {
  documentElement: {
    classList: {
      toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)),
      contains: (c) => classes.has(c),
    },
    style,
  },
  createElement: () => ({ style: {}, setAttribute() {}, appendChild() {} }),
  createElementNS: () => ({ style: {}, setAttribute() {}, appendChild() {} }),
  createTextNode: () => ({}),
  createComment: () => ({}),
  querySelector: () => null,
}
globalThis.SVGElement = class {}
globalThis.Element = class {}

let failures = 0
const check = async (label, fn) => {
  try {
    await fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const { useTheme, applyTheme } = await import('../src/lib/theme.ts')
const t = useTheme()
const resolved = () => t.resolved.value

console.log('theme-follow-system')

// A fresh install has no stored preference and must default to 跟随系统.
await check('fresh install defaults to 跟随系统', () => {
  assert.equal(t.theme.value, 'system')
  assert.equal(resolved(), 'dark', 'system resolves to the OS value (dark here)')
  assert.ok(classes.has('dark'))
})

await check('subscribes to prefers-color-scheme exactly once', () => {
  assert.equal(mql.listenerCount(), 1)
})

await check('system mode live-tracks an OS flip to light', () => {
  mql.fire(false)
  assert.equal(t.isDark.value, false)
  assert.ok(classes.has('light') && !classes.has('dark'))
})

await check('an explicit override ignores the OS', () => {
  t.setTheme('dark')
  assert.equal(resolved(), 'dark')
  mql.fire(false)
  assert.ok(classes.has('dark') && !classes.has('light'))
  assert.equal(store.get('blitzkrieg-panel-theme'), 'dark', 'override persisted')
})

await check('re-entering 跟随系统 re-reads the OS immediately', () => {
  t.setTheme('system')
  assert.equal(resolved(), 'light')
})

await check('cycle order 跟随系统 -> 浅色 -> 深色 -> 跟随系统', () => {
  t.setTheme('system')
  const seen = []
  for (let i = 0; i < 3; i++) {
    t.cycleTheme()
    seen.push(t.theme.value)
  }
  assert.deepEqual(seen, ['light', 'dark', 'system'])
})

await check('applyTheme sets color-scheme for native chrome', () => {
  applyTheme('light')
  assert.equal(style.colorScheme, 'light')
})

// A stored 'dark' came from the era when dark was a hardcoded default rather
// than a choice, so it migrates to 跟随系统 once. 'light' could only be a click.
console.log('\nstorage migration')
for (const [stored, expected, label] of [
  ['dark', 'system', "legacy stored 'dark' migrates to 跟随系统"],
  [null, 'system', 'no stored value defaults to 跟随系统'],
  ['light', 'light', "deliberate 'light' is preserved"],
  ['system', 'system', "stored 'system' is preserved"],
]) {
  await check(label, async () => {
    const s = new Map()
    if (stored !== null) s.set('blitzkrieg-panel-theme', stored)
    globalThis.localStorage = {
      getItem: (k) => s.get(k) ?? null,
      setItem: (k, v) => void s.set(k, v),
      removeItem: (k) => void s.delete(k),
    }
    // Fresh query string busts the module cache so import-time init runs again.
    const m = await import(`../src/lib/theme.ts?probe=${encodeURIComponent(label)}`)
    assert.equal(m.useTheme().theme.value, expected)
    assert.equal(s.get('blitzkrieg-panel-theme-follows-system'), '1', 'migration marker set')
  })
}

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
