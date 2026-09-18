#!/usr/bin/env node
/**
 * child-guard gate — a spawned child must not outlive the process that spawned it.
 *
 * WHY
 *   On 2026-09-19 the process table held two orphans (PPID=1), one of them 33
 *   hours old, left by gate runs that were interrupted between spawning a core
 *   and killing it. A leaked core is not cosmetic: it keeps its socket, its
 *   engine loop and its feed subscriptions alive with nobody watching, and a
 *   later `blitzkrieg-core` on the same socket path fails to boot.
 *
 *   `scripts/lib/child-guard.mjs` is the fix. This gate tests the FIX, not the
 *   twenty gates that import it: it spawns a driver that spawns something real,
 *   then ends the driver in a specific way, and asserts the descendant is gone.
 *   Testing it this way is the point — a guard that merely "looks installed" is
 *   worth nothing, and the failure it prevents is only visible in the process
 *   table.
 *
 * What it pins:
 *   1. A child spawned through the guard is reaped when its parent exits normally.
 *   2. …when the parent exits via a thrown error.
 *   3. …when the parent is signalled (SIGTERM), which does not fire 'exit'.
 *   4. …including a GRANDCHILD the child started. Several gates spawn a launcher
 *      (`scripts/tui-demo.sh`, `blitzkrieg run`, `ui_kit_web --manage`) that owns
 *      the core; reaping only the direct child would leave the core behind — the
 *      same leak, one level down.
 *   5. The child gets SIGTERM first, so it can run its own teardown, rather than
 *      being SIGKILLed outright (a core stripped of its teardown leaves its
 *      socket file behind and breaks the next boot on that path).
 *   6. The driver really did start a live descendant first, so a PASS cannot be
 *      vacuous — "no leak" is trivially true if nothing was ever spawned.
 *
 * Run: node scripts/child-guard-check.mjs
 */
import { spawn as rawSpawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

const LF = String.fromCharCode(10);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'child-guard-'));
const GUARD = join(process.cwd(), 'scripts', 'lib', 'child-guard.mjs');

let failures = 0;
const ok = (m) => console.log(`  ok   ${m}`);
const bad = (m) => { console.error(`  FAIL ${m}`); failures++; };
const assert = (c, m) => (c ? ok(m) : bad(m));

if (!existsSync(GUARD)) {
  console.error(`FAIL: ${GUARD} not found`);
  process.exit(1);
}

/** Is `pid` alive (not a zombie)? */
function alive(pid) {
  try { process.kill(pid, 0); } catch { return false; }
  return true;
}

/**
 * Write a driver that starts a real long-lived descendant through the guard,
 * reports its pid(s), then ends in the requested way.
 *
 * The descendant is `sleep` rather than a core: this gate tests the guard's
 * bookkeeping, and pulling a 10 MB core into it would make the gate slow and its
 * failures ambiguous. `parent-monitor-check.mjs` covers the real core.
 *
 * Modes: 'exit' | 'throw' | 'signal'  — end the driver that way.
 *        'subtree'                   — the child starts a background grandchild.
 *        'grace'                     — the child traps SIGTERM and must get to run it.
 */
function driver(mode, marker) {
  const dir = mkdtempSync(join(WORK, 'driver-'));
  mkdirSync(dir, { recursive: true });
  const file = join(dir, 'driver.mjs');

  // The child. In 'subtree' mode bash backgrounds a grandchild and waits; a
  // non-interactive bash has no job control, so the grandchild stays in bash's
  // process group — which is exactly the escape this case exists to rule out.
  // In 'grace' mode the child traps SIGTERM and leaves a marker if it got to run
  // the trap before being killed outright.
  const script =
    mode === 'subtree'
      ? 'sleep 600 & echo GRANDCHILD_PID=$!; wait'
      : mode === 'grace'
        ? `trap 'echo trapped > ${marker}; exit 0' TERM; while true; do sleep 0.05; done`
        : 'exec sleep 600';

  const end =
    mode === 'throw'
      ? `throw new Error('deliberate failure after spawning');`
      : mode === 'signal'
        ? `process.kill(process.pid, 'SIGTERM');`
        : `process.exit(0);`;

  const body = [
    `import { spawn, liveChildCount } from ${JSON.stringify(GUARD)};`,
    ``,
    `const c = spawn('bash', ['-c', ${JSON.stringify(script)}], { stdio: ['ignore', 'pipe', 'pipe'] });`,
    `c.stdout.on('data', (d) => process.stdout.write(d));`,
    `console.log('CHILD_PID=' + c.pid);`,
    `console.log('TRACKED=' + liveChildCount());`,
    `setTimeout(() => {`,
    `  ${end}`,
    `}, 500);`,
    ``,
  ].join(LF);
  writeFileSync(file, body);
  return { file, dir };
}

/** Run a driver to completion (or signal-exit) and return its output. */
async function runDriver(mode, marker) {
  const { file } = driver(mode, marker);
  const child = rawSpawn(process.execPath, [file], { stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  child.stdout.on('data', (d) => (out += d));
  child.stderr.on('data', (d) => (out += d));
  await new Promise((res) => child.on('exit', res));
  return { out, code: child.exitCode, signal: child.signalCode };
}

const pidOf = (out, name) => {
  const m = out.match(new RegExp(`${name}=(\\d+)`));
  return m ? Number(m[1]) : null;
};

/** Wait for a pid to disappear, killing it only if it never does. */
async function expectGone(pid, label) {
  for (let i = 0; i < 60; i++) {
    if (!alive(pid)) { ok(`${label} (pid ${pid}) was reaped`); return true; }
    await sleep(50);
  }
  bad(`${label}: pid ${pid} STILL ALIVE — the guard leaked it`);
  try { process.kill(pid, 'SIGKILL'); } catch {}
  return false;
}

console.log('─'.repeat(72));
console.log('child-guard gate (a spawned child must not outlive its spawner)');
console.log('─'.repeat(72));

// ── 1..3: the three ways a driver ends ──────────────────────────────────────
for (const [label, mode] of [
  ['normal exit', 'exit'],
  ['thrown error', 'throw'],
  ['SIGTERM to the driver', 'signal'],
]) {
  console.log('');
  console.log(`[${label}]`);
  const { out } = await runDriver(mode);
  const pid = pidOf(out, 'CHILD_PID');
  const tracked = /TRACKED=(\d+)/.exec(out);
  // Guard against a vacuous pass: the driver must have reported a live child.
  assert(pid !== null && pid > 0, `driver started a real child (pid ${pid})`);
  assert(tracked && Number(tracked[1]) === 1, `the guard tracked exactly one child (${tracked?.[1]})`);
  if (pid === null) continue;
  // Reaping is synchronous inside the driver's exit handler, so by the time the
  // driver's own exit has been observed the signal is already delivered. A short
  // grace period covers bash finishing its own teardown.
  await expectGone(pid, 'the child did not outlive the driver');
}

// ── 4: a grandchild must not survive either ────────────────────────────────
console.log('');
console.log('[grandchild subtree]');
{
  const { out } = await runDriver('subtree');
  const childPid = pidOf(out, 'CHILD_PID');
  const grandPid = pidOf(out, 'GRANDCHILD_PID');
  assert(childPid !== null, `driver started the intermediate child (pid ${childPid})`);
  assert(grandPid !== null && grandPid > 0, `the child really started a grandchild (pid ${grandPid})`);
  if (grandPid !== null) await expectGone(grandPid, 'the GRANDCHILD did not outlive the driver');
  if (childPid !== null) await expectGone(childPid, 'the child did not outlive the driver');
}

// ── 5: SIGTERM first, so the child can run its own teardown ────────────────
console.log('');
console.log('[grace before SIGKILL]');
{
  const marker = join(WORK, 'trapped');
  const { out } = await runDriver('grace', marker);
  const pid = pidOf(out, 'CHILD_PID');
  assert(pid !== null, `driver started a child that traps SIGTERM (pid ${pid})`);
  assert(
    existsSync(marker),
    'the child received SIGTERM and ran its trap before being killed outright',
  );
  if (pid !== null) await expectGone(pid, 'the child did not outlive the driver');
}

console.log('');
console.log('─'.repeat(72));
if (failures > 0) {
  console.error(`RESULT: FAIL — ${failures} assertion(s) failed`);
  process.exit(1);
}
console.log('RESULT: PASS — a guarded child dies with its spawner on exit, error and signal;');
console.log('               descendants included, and only after a chance to shut down cleanly.');
console.log('─'.repeat(72));
