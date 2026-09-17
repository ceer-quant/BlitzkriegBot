#!/usr/bin/env node
/**
 * E12 parent-monitor gate — "父进程退出后不留僵尸子进程".
 *
 * The shell (`src/index.ts`) used to exit via `process.exit(0)` after closing
 * only the HTTP server. The Rust core is a separate process, so it survived the
 * shell's death holding the socket and its resting orders.
 *
 * This gate exercises the REAL exit path rather than a simulation: it starts the
 * core through the same runner the shell uses, inside a child process, then
 * SIGTERMs the child and inspects the process table. It asserts on the observable
 * outcome (no surviving core) rather than on any internal call, so it stays
 * honest if the shutdown internals are refactored.
 *
 * Run: node scripts/parent-monitor-check.mjs
 */
import { spawn, execSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, unlinkSync, writeFileSync, existsSync } from 'node:fs';
import { join, relative } from 'node:path';
import { tmpdir } from 'node:os';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const WORK = mkdtempSync(join(tmpdir(), 'parent-monitor-'));

// ── Safety interlock ────────────────────────────────────────────────────────
// The gate spawns a core through the real runner. Isolation is explicit (via
// BZK_CORE_ISOLATION), so a live production core is not itself a blocker — but
// it IS worth stating loudly, because a regression in the isolation hook would
// mean this gate writes into the live ledger.
const PROD_SOCK = join(
  (process.env.TMPDIR || tmpdir()).replace(/\/$/, ''),
  `blitzkrieg-core-${process.env.USER || 'user'}.sock`,
);
if (existsSync(PROD_SOCK)) {
  console.log(`note: a production core is live on ${PROD_SOCK}`);
  console.log('      this gate isolates itself via BZK_CORE_ISOLATION; it must not touch it.');
}

// Isolate the spawned core: TMPDIR moves the socket, cwd moves the data files.
const SOCK = join(WORK, 'core.sock');

// The driver must live INSIDE the project so Node can resolve `tsx` and the
// project's own module graph; `target/` is gitignored and already scratch space.
const DRIVER_DIR = join(process.cwd(), 'target', 'parent-monitor');
mkdirSync(DRIVER_DIR, { recursive: true });

/** Every live `blitzkrieg-core` process, as {pid, ppid, cmd}. */
function corePids() {
  return execSync('ps -Ao pid=,ppid=,command=', { encoding: 'utf8' })
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => /target\/release\/blitzkrieg-core/.test(l))
    .map((l) => {
      const [pid, ppid] = l.split(/\s+/);
      return { pid: Number(pid), ppid: Number(ppid), cmd: l };
    });
}

// A driver that starts the core the same way the shell does, then idles so the
// parent's SIGTERM is what ends it.
//
// It loads the COMPILED runner (`dist/`) and runs under plain `node`, matching
// production (`npm start` → `node dist/index.js`). Two traps this avoids:
//
//   1. A `.ts` driver run through the `tsx` wrapper leaves the wrapper alive
//      after SIGTERM, so the driver never dies and the exit guard never fires —
//      the FAIL/PASS says nothing about the real signal path. Plain node has no
//      wrapper, so the signal reaches the process that owns the guards.
//   2. An absolute import path baked into the generated file points at the
//      developer's checkout and fails with ERR_MODULE_NOT_FOUND elsewhere (CI
//      caught exactly that). The specifier is therefore relative.
const DIST_RUNNER = join(process.cwd(), 'dist', 'core', 'blitzkrieg-core-runner.js');
if (!existsSync(DIST_RUNNER)) {
  console.error(`refusing to run: ${DIST_RUNNER} not found.`);
  console.error('  build the shell first: npm run build');
  process.exit(2);
}

const DRIVER = join(DRIVER_DIR, 'driver.mjs');
const REL = relative(DRIVER_DIR, DIST_RUNNER).replace(/\\/g, '/');
const IMPORT_SPEC = REL.startsWith('.') ? REL : './' + REL;
writeFileSync(
  DRIVER,
  `
import { getBlitzkriegCoreRunner } from ${JSON.stringify(IMPORT_SPEC)};

async function main() {
  const runner = getBlitzkriegCoreRunner();
  await runner.start({
    assets: ['BTC'], roundSec: 900, dryRun: true, sizeUsd: 5,
    minShares: 1, maxShares: 1, maxPositions: 1, maxDailyLossUsd: 10,
    minRoundAgeSec: 0, minTimeLeftSec: 0, eventArchive: null,
  });
  console.log('DRIVER_READY');
  setInterval(() => {}, 1000);
}

main().catch((e) => { console.error('DRIVER_FAILED', e); process.exit(1); });
`,
);

// Run under plain `node`, matching production. No tsx wrapper: it would outlive
// SIGTERM, keep the driver alive, and make the result meaningless.
const child = spawn(process.execPath, [DRIVER], {
  cwd: process.cwd(),
  stdio: ['ignore', 'pipe', 'pipe'],
  env: { ...process.env, BZK_CORE_ISOLATION: WORK },
});

let out = '';
child.stdout.on('data', (d) => (out += d));
child.stderr.on('data', (d) => (out += d));

let ready = false;
for (let i = 0; i < 120; i++) {
  if (/DRIVER_READY/.test(out)) { ready = true; break; }
  if (child.exitCode !== null) break;
  await sleep(250);
}

if (!ready) {
  console.log('driver never became ready; tail of its output:');
  console.log(out.split('\n').slice(-15).join('\n'));
  try { child.kill('SIGKILL'); } catch {}
  process.exit(1);
}

const before = corePids();

// Identify OUR core by its isolated socket path — an exact, unique marker for
// this run. Ancestry is NOT usable here: `tsx` runs the driver in a subprocess,
// so the core's ppid is the tsx child rather than the wrapper we spawned. The
// socket path is both unique and impossible to collide with a production core
// (which lives under TMPDIR, never under our scratch dir).
const socketMarker = join(WORK, 'core.sock');
const oursBefore = before.filter((p) => p.cmd.includes(socketMarker));
console.log('before exit: blitzkrieg-core processes =', before.length);
console.log(`  ours (socket under ${WORK}) =`, oursBefore.length);
for (const p of oursBefore) console.log(`  pid=${p.pid} ppid=${p.ppid}`);
const otherCount = before.length - oursBefore.length;
if (otherCount > 0) console.log(`  (${otherCount} unrelated core(s) in the table — ignored)`);

if (oursBefore.length === 0) {
  console.log('\nthe driver reported READY but no core is using our isolated socket;');
  console.log('cannot attribute a core to this run, so the gate fails closed.');
  try { child.kill('SIGKILL'); } catch {}
  process.exit(1);
}

// Signal the shell exactly as a user's Ctrl-C would.
child.kill('SIGTERM');
for (let i = 0; i < 60 && child.exitCode === null; i++) await sleep(250);
console.log('shell exited: code =', child.exitCode, '| signal =', child.signalCode);

// Wait for the shell to be reaped, then give an asynchronous shutdown a moment
// to finish. Sampling too early produced a FALSE PASS once: the process table
// was read while the tree was still winding down, so nothing looked like a leak.
for (let i = 0; i < 40; i++) {
  if (child.exitCode !== null || child.signalCode !== null) break;
  await sleep(100);
}
await sleep(1500);

/** Is `pid` still a live (non-zombie) process? */
function isAlive(pid) {
  try { process.kill(pid, 0); } catch { return false; }
  try {
    const st = execSync(`ps -p ${pid} -o stat=`, { encoding: 'utf8' }).trim();
    return st.length > 0 && !st.startsWith('Z');
  } catch { return false; }
}

const after = corePids();
// A leak is one of OUR pids still present. Match on pid identity, not on ppid:
// an orphan is reparented to pid 1, which is precisely the failure being caught.
// A zombie is NOT a leak here — the core did exit; the shell simply has not
// reaped it yet — so require a live process.
const survivors = after.filter(
  (p) => oursBefore.some((b) => b.pid === p.pid) && isAlive(p.pid),
);
console.log('after exit : surviving core processes (ours) =', survivors.length);
for (const p of survivors) console.log(`  LEAKED pid=${p.pid} ppid=${p.ppid}`);

// Clean up ONLY what this gate spawned — that is, pids we saw on our own socket.
// Never touch a pid we did not observe there: it may be the operator's live core.
for (const p of survivors) { try { process.kill(p.pid, 'SIGKILL'); } catch {} }
try { child.kill('SIGKILL'); } catch {}
try { unlinkSync(SOCK); } catch {}

const ok = oursBefore.length >= 1 && survivors.length === 0;
console.log(
  `\nRESULT: ${ok ? 'PASS' : 'FAIL'} — ` +
    (ok
      ? 'the core died with its parent (no zombie)'
      : 'the core outlived its parent and is now orphaned'),
);

// A core that is gone by name can still be gone in the ONE way that matters —
// left holding the socket, i.e. replaced by a restart. Re-check by name and
// report any core still on our socket, resolving it to a pid so a leak cannot be
// reported as clean. This is a second, independent read of the same rule.
const reread = corePids().filter((p) => p.cmd.includes(socketMarker) && isAlive(p.pid));
if (reread.length > 0) {
  console.log('re-check: a core is STILL using our isolated socket after the tree exited:');
  for (const p of reread) console.log(`  pid=${p.pid} ppid=${p.ppid} — restart or leak`);
  for (const p of reread) { try { process.kill(p.pid, 'SIGKILL'); } catch {} }
  process.exit(1);
}

process.exit(ok ? 0 : 1);
