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

const { controlState, exitNotice } = await import('../src/lib/lifecycle.ts')

const gw = (over) => ({ lifecycleEnabled: false, managed: false, corePid: null, socket: '/tmp/x.sock', ...over })
const crash = { pid: 111, kind: 'crash', code: null, signal: 9, description: 'core pid 111 CRASHED (killed by signal 9)' }

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

// ── E12-c: the crash notice ─────────────────────────────────────────────────
//
// The gap this covers: a core that crashed used to leave no trace on the panel.
// `Supervisor::reap` cleared the child and kept no record, so the page showed a
// missing pid and the operator had to guess whether that meant a crash, a stop,
// or a gateway that had never started anything. The Rust side now classifies the
// exit; these cases pin the *display* rules that decide when it is said out loud.

console.log('\nexit notices (E12-c)')

check('a gateway that never reported an exit says nothing', () => {
  // Absence is the normal case for a healthy gateway and for one older than the
  // field. Inventing a notice from it would put a permanent alarm on every panel.
  assert.equal(exitNotice(undefined, true), null)
  assert.equal(exitNotice(gw({ lastExit: null }), true), null)
})

check('a crash is reported while the core is down', () => {
  const n = exitNotice(gw({ managed: true, lastExit: crash }), false)
  assert.ok(n, 'a crash with no core running is the case the operator must see')
  assert.equal(n.kind, 'crash')
  assert.equal(n.description, crash.description)
})

check('a crash is still reported after the restart brought the core back', () => {
  // The subtle one. A successful auto-restart makes the core answer again, and
  // dropping the notice here is how a core that crashes every few minutes looks
  // perfectly healthy — the panel would only ever show the current, running
  // state. The notice must survive, with the restart count, so a flapping core
  // is a visible pattern instead of a silent one.
  const n = exitNotice(gw({ managed: true, restarts: 3, lastExit: crash }), true)
  assert.ok(n, 'a crash that was repaired is still a crash')
  assert.equal(n.restarts, 3, 'the operator needs the count to judge flapping')
  assert.equal(n.givenUp, false)
})

check('a spent restart budget is flagged, not hidden behind "crashed"', () => {
  const n = exitNotice(gw({ managed: true, restarts: 5, restartGivenUp: true, lastExit: crash }), false)
  assert.equal(n.givenUp, true, '"will not come back" is a different action from "crashed"')
})

check('a clean stop is only mentioned while the core is down', () => {
  const stopped = { pid: 7, kind: 'clean', code: 0, signal: null, description: 'core pid 7 stopped (exit code 0)' }
  const down = exitNotice(gw({ managed: true, lastExit: stopped }), false)
  assert.equal(down?.kind, 'clean', 'a deliberate stop is worth stating once')
  // Once a core is up again the stop is history; keeping it would read as a
  // current fault on a panel whose whole job is to show the present.
  assert.equal(exitNotice(gw({ managed: true, lastExit: stopped }), true), null)
})

check('a missing restarts count reads as zero, never as NaN', () => {
  // Older gateway: `lastExit` present, counters absent. The panel must render,
  // so the fallback has to be a number rather than undefined.
  const n = exitNotice(gw({ managed: true, lastExit: crash }), false)
  assert.equal(n.restarts, 0)
  assert.equal(n.givenUp, false)
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
