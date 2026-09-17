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
import { join } from 'node:path';
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

/** Every live `blitzkrieg-core` process, as {pid, ppid}. */
function corePids() {
  return execSync('ps -Ao pid=,ppid=,command=', { encoding: 'utf8' })
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => /target\/release\/blitzkrieg-core/.test(l))
    .map((l) => {
      const [pid, ppid] = l.split(/\s+/);
      return { pid: Number(pid), ppid: Number(ppid) };
    });
}

// A driver that starts the core the same way the shell does, then idles so the
// parent's SIGTERM is what ends it. Written to a file so it imports the real
// runner rather than reimplementing the call.
const DRIVER = join(DRIVER_DIR, 'driver.ts');
// No top-level await: the file lives outside `src/` so it is treated as CJS and
// esbuild rejects TLA there. The IIFE keeps the same shape and stays portable.
writeFileSync(
  DRIVER,
  `
import { getBlitzkriegCoreRunner } from ${JSON.stringify(
    join(process.cwd(), 'src/core/blitzkrieg-core-runner.ts'),
  )};

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

// Match the project's own TS entry convention (`node --import tsx <file>.ts`).
// `cwd` stays at the repo root so `tsx` resolves; isolation comes from the
// runner's BZK_CORE_ISOLATION hook, which redirects the socket AND the data log
// paths away from the production ledger.
const child = spawn(process.execPath, ['--import', 'tsx', DRIVER], {
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

// Our core is the one descended from the shell we spawned. Matching on ancestry
// (not on "some core survived") is what keeps this gate from ever touching an
// unrelated live core — the operator's panel-managed instance is in the process
// table for the whole run, and killing it would destroy an unreconstructable
// ledger.
const oursBefore = before.filter((p) => p.ppid === child.pid);
console.log('before exit: blitzkrieg-core processes =', before.length);
console.log('  ours (descended from shell pid ' + child.pid + ') =', oursBefore.length);
for (const p of oursBefore) console.log(`  pid=${p.pid} ppid=${p.ppid}`);
const otherCount = before.length - oursBefore.length;
if (otherCount > 0) console.log(`  (${otherCount} unrelated core(s) in the table — ignored)`);

// Signal the shell exactly as a user's Ctrl-C would.
child.kill('SIGTERM');
for (let i = 0; i < 60 && child.exitCode === null; i++) await sleep(250);
console.log('shell exited: code =', child.exitCode, '| signal =', child.signalCode);

// Give an asynchronous shutdown a moment to finish reaping.
await sleep(1000);

const after = corePids();
// A leak is one of OUR pids still present. An orphan reparents to pid 1, so
// match on identity, not on ppid.
const survivors = after.filter((p) => oursBefore.some((b) => b.pid === p.pid));
console.log('after exit : surviving core processes (ours) =', survivors.length);
for (const p of survivors) console.log(`  LEAKED pid=${p.pid} ppid=${p.ppid}`);

// Clean up ONLY what this gate spawned.
for (const p of survivors) { try { process.kill(p.pid, 'SIGKILL'); } catch {} }
try { unlinkSync(SOCK); } catch {}

const ok = oursBefore.length >= 1 && survivors.length === 0;
console.log(
  `\nRESULT: ${ok ? 'PASS' : 'FAIL'} — ` +
    (ok
      ? 'the core died with its parent (no zombie)'
      : 'the core outlived its parent and is now orphaned'),
);
process.exit(ok ? 0 : 1);
