/**
 * Regression check for the 启动 / 停止 control gating (`src/lib/lifecycle.ts`).
 *
 * The bug this pins down: the panel offered two live-looking buttons that
 * answered `lifecycle control disabled; start the gateway with --manage to
 * enable start/stop`. Two independent causes, and the check must keep them
 * distinguishable — a gateway that was never granted the verbs is a different
 * situation from one that was granted them but cannot act on this particular
 * core (an adopted core is deliberately left running by `Supervisor::stop`).
 *
 * The safety property that matters most: absent information must read as
 * "cannot". A missing `gateway` block is an old or read-only gateway, and
 * defaulting it to "enabled" would put the old dead button back.
 *
 *   cd ui/webapp/webui && npm run check:lifecycle
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

const { controlState } = await import('../src/lib/lifecycle.ts')

const gw = (over) => ({ lifecycleEnabled: false, managed: false, corePid: null, socket: '/tmp/x.sock', ...over })

console.log('lifecycle control gating')

check('a missing gateway block never grants control', () => {
  // Read-only adapter, or a core/gateway older than the field. The panel must
  // not fall back to "there is no flag saying no, so yes".
  for (const block of [undefined, null]) {
    const c = controlState(block, true)
    assert.equal(c.enabled, false)
    assert.equal(c.canStop, false)
    assert.equal(c.canStart, false)
    assert.equal(c.usable, false)
    assert.ok(c.blockedReason, 'an inert pair must carry the reason')
  }
})

check('without --manage both verbs are refused and the panel says why', () => {
  // The reproduced live case: gateway pid 10257 was started with no --manage.
  const c = controlState(gw({ lifecycleEnabled: false }), true)
  assert.equal(c.canStart, false)
  assert.equal(c.canStop, false)
  assert.match(c.blockedReason, /--manage/, 'the reason names the actual fix')
})

check('--manage with an adopted core enables start but not stop', () => {
  // The second, independent cause: core pid 15022 is a child of pid 72966, not
  // of the gateway, so `Supervisor::stop` returns NotOwned and leaves it
  // running. Offering a live 停止 here would be a promise the core cannot keep.
  const c = controlState(gw({ lifecycleEnabled: true, managed: false }), true)
  assert.equal(c.canStart, false, 'a core is already answering — start is pointless')
  assert.equal(c.canStop, false, 'an adopted core is left running by design')
  assert.equal(c.usable, false)
  assert.match(c.blockedReason, /其他进程/, 'the reason distinguishes adoption from a missing flag')
})

check('--manage with a spawned core enables stop and not start', () => {
  const c = controlState(gw({ lifecycleEnabled: true, managed: true, corePid: 4242 }), true)
  assert.equal(c.canStop, true)
  assert.equal(c.canStart, false, 'no point spawning a second core')
  assert.equal(c.usable, true)
  assert.equal(c.blockedReason, null)
})

check('--manage with no core running enables start', () => {
  const c = controlState(gw({ lifecycleEnabled: true, managed: false }), false)
  assert.equal(c.canStart, true)
  assert.equal(c.canStop, false)
  assert.equal(c.usable, true)
  assert.equal(c.blockedReason, null, 'nothing is blocked when start can act')
})

check('the two refusal reasons are never conflated', () => {
  // A missing flag and an adopted core are fixed in different ways, so the
  // panel must not show one message for both.
  const noFlag = controlState(gw({ lifecycleEnabled: false }), true).blockedReason
  const adopted = controlState(gw({ lifecycleEnabled: true, managed: false }), true).blockedReason
  assert.notEqual(noFlag, adopted)
  assert.match(noFlag, /--manage/)
  assert.doesNotMatch(adopted, /--manage/, '--manage is already on in this case')
})

check('canStop and canStart are never both true', () => {
  // Exactly one control carries the next move; both live at once would be a
  // contradiction the UI cannot render honestly.
  for (const connected of [true, false]) {
    for (const managed of [true, false]) {
      for (const enabled of [true, false]) {
        const c = controlState(gw({ lifecycleEnabled: enabled, managed }), connected)
        assert.ok(!(c.canStop && c.canStart), `both true: enabled=${enabled} managed=${managed} connected=${connected}`)
      }
    }
  }
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
