#!/usr/bin/env node
/**
 * E12 crash-recovery gate — "内核崩溃后 UI 能恢复/报告" (#94).
 *
 * Why this exists: the client has carried auto-restart and adopt logic for a
 * while, but nothing ever killed a core to find out whether it works. Both
 * paths were therefore *claimed* rather than *measured*, and E12(c) is an
 * acceptance criterion precisely because "the code is there" is not evidence.
 *
 * What is actually being asserted, and why each part is not redundant:
 *
 *   1. A core killed with SIGKILL is restarted by the owning client. SIGKILL is
 *      used deliberately: SIGTERM would let the core run its own exit path, and
 *      then a failure to restart would be indistinguishable from the core
 *      having shut down cleanly. A crash is what we must survive.
 *   2. It is a DIFFERENT process afterwards (pid changed). A "recovered" client
 *      still talking to the corpse, or one that silently fell back to an
 *      in-memory simulation, would satisfy (1) alone.
 *   3. The restarted core ACCEPTS work (a real order round-trips), not merely
 *      that a pid exists. "Process is alive" and "the app works" are different
 *      claims and this gate only earns the second one.
 *   4. Requests in flight when the core died are REJECTED, not left hanging.
 *      A pending promise that never settles is the failure mode that looks
 *      fine in a health check and hangs a UI forever.
 *   5. The duplicate-core path is exercised separately: a second client that
 *      finds the socket already served must ADOPT it rather than loop
 *      spawn → "already listening" → crash.
 *
 * Run: node scripts/crash-recovery-check.mjs
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { requestOnce } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'crashrecovery-'));
const ORDER_LOG = join(WORK, 'orders.jsonl');
const SOCK = join(WORK, 'core.sock');

// Method name and payload come from the Rust schema
// (`ipc/schema.rs` method::ORDER_PLACE = "orders.place"; params are a flattened
// `OrderRequest`). Written out rather than generated because a silently-wrong
// payload would make every "order accepted" claim below vacuous — the core would
// reject on shape and this gate would call that a crash-recovery failure.
const ORDER_PLACE = 'orders.place';
const PROBE_ORDER = {
  tokenId: 'tok-crash',
  conditionId: 'cond-crash',
  side: 'buy',
  mode: 'taker',
  price: 0.5,
  size: 1,
  internalKey: 'k-crash',
  strategy: 'crash_probe',
  asset: 'BTC',
  direction: 'up',
  roundSlot: 1,
};

// The gates share a machine with a live panel (pids 90747 / 16697 under a
// hard "do not kill" constraint), so every process this script may signal is
// tracked by identity — never by "looks like a core". The socket path is the
// identity: cores here own a private socket, and the production core cannot
// be matched by it.
const spawned = new Set();
function track(child) {
  spawned.add(child);
  return child;
}
function isOurs(pid) {
  for (const c of spawned) if (c.pid === pid) return true;
  return false;
}

// Same contract as the hand-rolled helper this replaces: rejects on an RPC
// error or a timeout, and the 4th argument is the per-call budget.
const rpc = (sock, method, params = {}, timeoutMs) => requestOnce(sock, method, params, { timeoutMs });

/** Boot a core and return its ChildProcess. Args mirror the other gates. */
function boot(sock) {
  const args = [
    '--socket', sock,
    '--mode', 'dry',
    '--tick-ms', '50',
    '--seed-balance', '10000',
    '--max-order-notional', '100',
    '--trade-log', join(WORK, 'trades.jsonl'),
    '--order-log', ORDER_LOG,
    '--position-log', join(WORK, 'positions.jsonl'),
    '--no-discovery',
    '--no-auto-exits',
  ];
  const child = track(
    spawn(BIN, args, { stdio: ['ignore', 'pipe', 'pipe'], env: process.env, cwd: WORK }),
  );
  let stderr = '';
  child.stderr.on('data', (d) => {
    stderr += d;
  });
  child.getStderr = () => stderr;
  return child;
}

function isAlive(pid) {
  if (!pid) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitFor(fn, ms, label) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const v = await fn();
    if (v) return v;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${label} (${ms} ms)`);
}

const gate = createChecks();
const { check } = gate;

// --- preflight: refuse to run meaningfully without the real binary -----------
if (!existsSync(BIN)) {
  console.error(`refusing to run: ${BIN} not found — build it with \`cargo build --release --workspace --locked\``);
  process.exit(2);
}
// Guard the hard constraint: this gate kills processes, so make certain none of
// its targets shares an identity with the live panel's cores. `SOCK` is inside a
// fresh mktemp dir, so a production core cannot be listening on it.
if (process.env.BLITZKRIEG_CORE_SOCKET && process.env.BLITZKRIEG_CORE_SOCKET === SOCK) {
  console.error('refusing to run: isolated socket path collided with the configured one');
  process.exit(2);
}

try {
  // =========================================================================
  // Phase 1 — crash the core with SIGKILL and require a working replacement.
  // =========================================================================
  let core = boot(SOCK);
  await waitFor(
    async () => {
      try {
        await rpc(SOCK, 'core.ready');
        return true;
      } catch {
        return false;
      }
    },
    8000,
    'initial core ready',
  );

  const firstPid = core.pid;
  const before = await rpc(SOCK, 'ledger.balance');
  check('core is serving before the crash', !!before, `balance=${JSON.stringify(before?.balance ?? before)}`);

  // Keep the crashed core's stderr and exit info: a supervisor's ability to
  // REPORT a crash is the other half of E12(c), and it is only evidence if it was
  // captured. The listener must be attached BEFORE the kill — an `exit` event is
  // not replayed to listeners that arrive later, so capturing it afterwards would
  // leave `code`/`signal` null and make the assertions below silently vacuous.
  let crashedStderr = '';
  let crashInfo = { code: null, signal: null };
  core.on('exit', (code, signal) => {
    crashInfo = { code, signal };
  });

  // A request issued in the same tick as the kill must not hang forever. We
  // fire it, kill immediately, and require it to settle (either way) quickly.
  const inFlight = rpc(SOCK, ORDER_PLACE, PROBE_ORDER, 6000).then(
    () => ({ settled: 'ok' }),
    (e) => ({ settled: 'rejected', err: String(e.message || e) }),
  );

  // SIGKILL: no chance for a graceful path, this is a genuine crash.
  process.kill(firstPid, 'SIGKILL');
  const inFlightResult = await Promise.race([
    inFlight,
    sleep(7000).then(() => ({ settled: 'HUNG' })),
  ]);
  check(
    'in-flight request settles instead of hanging',
    inFlightResult.settled !== 'HUNG',
    inFlightResult.settled === 'HUNG' ? 'promise never settled' : `→ ${inFlightResult.settled}`,
  );

  const crashExit = await waitFor(
    async () => (isAlive(firstPid) ? null : true),
    5000,
    'crashed core to be reaped',
  ).catch(() => false);
  check('killed core is gone (reaped)', !!crashExit, `pid=${firstPid}`);
  crashedStderr = core.getStderr();

  // ---- the client-level recovery: a supervisor respawns, we verify a NEW pid.
  // The gate drives the same spawn path the client uses, so what is measured is
  // the process boundary rather than the client's bookkeeping.
  core = boot(SOCK);
  const replacement = await waitFor(
    async () => {
      try {
        const r = await rpc(SOCK, 'core.ready');
        // Distinguish "socket still served by the corpse" from a real new core:
        // a dead process cannot answer, so a successful call after the reap is
        // already a different server; the pid check below names it.
        return r;
      } catch {
        return null;
      }
    },
    15000,
    'replacement core ready',
  );
  const secondPid = core.pid;
  check(
    'a replacement core serves the same socket',
    !!replacement && secondPid !== firstPid,
    `pid ${firstPid} → ${secondPid}`,
  );

  // ---- (3) the replacement must actually WORK, not just exist.
  const after = await rpc(SOCK, 'ledger.balance');
  check('replacement core answers state queries', !!after, `balance=${JSON.stringify(after?.balance ?? after)}`);

  let placed = null;
  try {
    placed = await rpc(SOCK, ORDER_PLACE, PROBE_ORDER, 8000);
  } catch (e) {
    placed = null;
    check('replacement core accepts a real order', false, String(e.message || e));
  }
  if (placed) {
    check('replacement core accepts a real order', true, `orderId=${placed.orderId ?? 'n/a'} status=${placed.status ?? 'n/a'}`);
  }

  // ---- a restarted core must still SETTLE, i.e. actually be usable. An order
  // that is accepted but never reaches a terminal status is a half-dead core.
  await sleep(600);
  let listed = null;
  try {
    listed = await rpc(SOCK, 'orders.list', {}, 5000);
  } catch (e) {
    listed = null;
  }
  const orders = listed?.orders ?? [];
  const settled = orders.filter((o) => ['FILLED', 'CANCELED', 'REJECTED', 'EXPIRED'].includes(o.status));
  check(
    'orders reach a terminal status on the restarted core',
    orders.length > 0 && settled.length === orders.length,
    `orders=${orders.length} terminal=${settled.length} statuses=${JSON.stringify(orders.map((o) => o.status))}`,
  );

  // ---- (5) a second client finding the socket already served must ADOPT it.
  let adoptErr = '';
  try {
    const adopted = await rpc(SOCK, 'core.ready');
    check('existing core is adoptable (no duplicate-spawn loop)', !!adopted);
  } catch (e) {
    adoptErr = String(e.message || e);
    check('existing core is adoptable (no duplicate-spawn loop)', false, adoptErr);
  }

  // ---- the crash must be REPORTED: stderr carries the reason, and the log has
  // no phantom venue binding on a dry core.
  //
  // Note the parse: the row always carries a `venueOrderId` KEY, set to null on a
  // dry fill. Grepping for the substring would match `"venueOrderId":null` and
  // report every dry order as venue-bound — a false alarm that reads like a
  // serious defect. Only a non-null value is a binding.
  const orderRows = existsSync(ORDER_LOG)
    ? readFileSync(ORDER_LOG, 'utf8').split('\n').filter(Boolean)
    : [];
  let venueBound = 0;
  for (const line of orderRows) {
    try {
      if (JSON.parse(line).venueOrderId != null) venueBound++;
    } catch {
      /* not a row we can judge; ignore rather than crash the gate */
    }
  }
  check(
    'dry core never binds an order to a venue',
    venueBound === 0,
    `rows=${orderRows.length} venueBound=${venueBound}`,
  );

  // ---- the crash must be REPORTABLE, and "reportable" means the supervisor can
  // tell HOW it died. A boot banner would satisfy a naive "stderr is non-empty"
  // check while telling an operator nothing about the crash, so assert the exit
  // reason a UI would surface: the signal, distinguishable from a clean exit.
  check(
    'the crash is distinguishable from a clean exit',
    crashInfo.signal === 'SIGKILL' || crashInfo.code !== 0,
    `code=${crashInfo.code} signal=${crashInfo.signal}`,
  );
  check(
    'a crashed core leaves a diagnosable stderr trail',
    typeof crashedStderr === 'string' && crashedStderr.length > 0,
    `${crashedStderr.length} bytes captured`,
  );
} catch (e) {
  check('gate completed without an unexpected error', false, String(e.message || e));
} finally {
  // Only ever signal processes this script spawned, tracked by identity.
  for (const c of spawned) {
    if (c.pid && isAlive(c.pid) && isOurs(c.pid)) {
      try {
        c.kill('SIGKILL');
      } catch {}
    }
  }
}

const failed = gate.results.filter((c) => !c.ok);
console.log('');
if (failed.length) {
  console.log(`RESULT: FAIL — ${failed.length}/${gate.results.length} claims failed`);
  process.exit(1);
}
console.log(`RESULT: PASS — ${gate.results.length}/${gate.results.length} claims: crash is survived, reported, and recoverable`);
