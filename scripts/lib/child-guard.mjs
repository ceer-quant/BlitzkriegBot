/**
 * Guarded `spawn` for the gate drivers.
 *
 * WHY THIS EXISTS
 *   Every gate that drives a real core does `spawn(BIN, …)` and then relies on
 *   reaching its own teardown to kill it. If the gate throws, is interrupted, or
 *   times out between the spawn and that teardown, the core is reparented to pid
 *   1 and keeps running — holding its socket, its engine loop and its feed
 *   subscriptions with nobody watching. On 2026-09-19 the process table held a
 *   **33-hour** orphan (`adopt-48644.sock`) from an interrupted run, which is what
 *   this module prevents.
 *
 *   The E12 design is sound — `CoreClient` owns an exit guard for the core it
 *   spawns — but gates that spawn a core directly (to test adoption, crash
 *   recovery, scale …) bypassed it. Rather than re-implement an exit guard in
 *   twenty files, this module exports a `spawn` that is drop-in compatible:
 *
 *     import { spawn } from './lib/child-guard.mjs';
 *
 * Two properties matter, and neither is free:
 *
 *   1. THE WHOLE SUBTREE DIES, not just the direct child. Several gates spawn a
 *      launcher, not a core: `scripts/tui-demo.sh` starts a core in the
 *      background, `blitzkrieg run` and `ui_kit_web --manage` own the core they
 *      shrank into a child process. Killing only the direct child would leave the
 *      core it started behind — the exact leak, one level down. So children are
 *      spawned `detached` (each leads its own process group) and reaped *by
 *      group*; a child's descendants cannot escape it.
 *
 *   2. THEY GET A CHANCE TO LEAVE CLEANLY. SIGKILL alone would strip a core of
 *      its own teardown: the socket file would stay behind, so the next
 *      `blitzkrieg-core` on that path fails to boot — a test failure that looks
 *      like an unrelated bug. So reap sends SIGTERM, waits a short grace period,
 *      and only then SIGKILLs whatever is still there.
 *
 * Non-child exports of `child_process` are re-exported untouched, so a gate can
 * consolidate its imports here without changing behaviour elsewhere.
 */
import { spawn as rawSpawn } from 'node:child_process';

export { execSync, execFileSync, spawnSync, exec } from 'node:child_process';

/** Live children spawned through this module. */
const live = new Set();

let installed = false;

/** How long a child gets to run its own teardown before it is killed outright. */
const GRACE_MS = 250;

/**
 * Synchronous nap. Signal and 'exit' handlers cannot await, so the grace period
 * has to block the thread — a promise would never be resumed. `Atomics.wait` is
 * the portable way to block without burning CPU (Node permits it on the main
 * thread, unlike a browser).
 */
function nap(ms) {
  try {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  } catch {
    // No SharedArrayBuffer (locked-down runtime): skip the grace. The SIGKILL
    // below still lands, so the guard degrades to "the child always dies" rather
    // than to "the child may live".
  }
}

/** Signal the child's whole process group, falling back to the child alone. */
function killTree(child, signal) {
  // Do not signal a child we know has exited: its pid may already have been
  // recycled, and a negative-pid kill would then hit an unrelated group.
  if (child.exitCode !== null || child.signalCode !== null) return;
  try {
    process.kill(-child.pid, signal);
  } catch {
    // Negative pid means "the group led by <pid>"; it fails if that group is
    // already gone, which is success, and on a runtime that did not detach the
    // child, which is why the fallback exists.
    try { child.kill(signal); } catch {}
  }
}

function reapAll() {
  if (live.size === 0) return;
  for (const child of live) killTree(child, 'SIGTERM');
  nap(GRACE_MS);
  for (const child of live) killTree(child, 'SIGKILL');
  live.clear();
}

function install() {
  if (installed) return;
  installed = true;
  // 'exit' covers a normal return, a thrown error and an explicit process.exit().
  // A signal terminates without firing 'exit', so those are handled separately.
  process.on('exit', () => reapAll());
  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(sig, () => {
      reapAll();
      // Exit non-zero: a gate stopped by a signal did not pass.
      process.exit(1);
    });
  }
}

/**
 * Drop-in `spawn` whose child — and everything the child starts — cannot outlive
 * this process.
 *
 * Same arguments and same return value as `child_process.spawn`. The differences
 * are that the child leads its own process group (`detached`), so it can be
 * reaped as a tree, and that it is registered for reaping on our exit.
 */
export function spawn(command, args, options) {
  install();
  // `spawn(command, options)` is legal in Node; normalise before merging.
  let childArgs = args;
  let childOpts = options;
  if (childArgs && !Array.isArray(childArgs)) {
    childOpts = childArgs;
    childArgs = [];
  }
  const child = rawSpawn(command, childArgs, {
    ...childOpts,
    detached: childOpts?.detached ?? true,
  });
  live.add(child);
  // Untrack as soon as it is gone, so reapAll() never walks a dead handle list.
  child.on('exit', () => live.delete(child));
  child.on('error', () => live.delete(child));
  return child;
}

/** Reap every tracked child immediately. Exported for tests and explicit teardown. */
export function reapAllChildren() {
  reapAll();
}

/** How many tracked children are still running. Exported for tests. */
export function liveChildCount() {
  return live.size;
}
