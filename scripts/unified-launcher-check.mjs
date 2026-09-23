#!/usr/bin/env node
/**
 * E12 unified launcher gate — "单二进制一体化启动门禁".
 *
 * Exercises the unified `blitzkrieg` binary across its primary subcommands and invariants:
 *   1. `blitzkrieg --help` discloses `core`, `tui`, `web`, `run` subcommands and
 *      the part-launch flags (`--tui`/`-tui`, `--web`, `--interval-ms`, `--manage`).
 *   2. `blitzkrieg run` launches core + UI in one command with `lifecycle: on` by default.
 *   3. Child core process is owned by the launcher (PPID equals launcher PID).
 *   4. Core answers real JSON-RPC over the isolated UDS socket.
 *   5. `blitzkrieg run --readonly` structurally passes `--readonly` to core (mode = "readonly").
 *   6. Graceful shutdown: on SIGTERM, parent reaps child core, unbinds socket, leaving no zombies.
 *   9. `run --tui` spawns the core but binds NO web address; the TUI refuses a
 *      headless stdout gracefully; the shared dispatcher reaps the core on exit.
 *  10. `tui --attach` watches an existing core and leaves it running (adopt-only).
 *
 * Run: node scripts/unified-launcher-check.mjs
 */

import { execSync, execFileSync } from 'node:child_process';
// Guarded spawn: `blitzkrieg run` starts a core as its own child, so a failure or
// an interrupt between here and the launcher's SIGTERM leaves the launcher AND a
// core behind. The guard reaps the launcher's whole process group, which includes
// that core, and does it with SIGTERM first so the core still unlinks its socket.
import { spawn, reapAllChildren } from './lib/child-guard.mjs';
import { mkdtempSync, rmSync, existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import net from 'node:net';
import { requestOnce } from './lib/core-client.mjs';
import { pollUntil } from './lib/wait.mjs';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg');
const CORE_BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const WORK = mkdtempSync(join(tmpdir(), 'unified-launcher-'));
const SOCK = join(WORK, 'unified.sock');
const READONLY_SOCK = join(WORK, 'readonly.sock');
const STOP_SOCK = join(WORK, 'stop.sock');
const ORPHAN_SOCK = join(WORK, 'orphan.sock');
const ADOPT_SOCK = join(WORK, 'adopt.sock');
const ENV_SOCK = join(WORK, 'env.sock');
const TUI_SOCK = join(WORK, 'tui.sock');
const ATTACH_SOCK = join(WORK, 'attach.sock');
const PORT1 = 52000 + Math.floor(Math.random() * 2000);
const PORT2 = 54000 + Math.floor(Math.random() * 2000);
const PORT3 = 56000 + Math.floor(Math.random() * 2000);
const PORT4 = 58000 + Math.floor(Math.random() * 2000);
const PORT5 = 61000 + Math.floor(Math.random() * 2000);
const PORT_TUI = 64000 + Math.floor(Math.random() * 1000);

function cleanupAll() {
  reapAllChildren();
}

function assert(ok, msg) {
  if (!ok) {
    console.error(`FAIL: ${msg}`);
    cleanupAll();
    try { rmSync(WORK, { recursive: true, force: true }); } catch {}
    process.exit(1);
  }
  console.log(`  ok   ${msg}`);
}

// A reachability probe, not a data path: this gate's assertions compare against
// `null` ("the core is gone") and retry in their own loops, so a refused or
// unanswered call reads as `null` here rather than throwing.
const rpc = (sock, method, params = {}) =>
  requestOnce(sock, method, params, { timeoutMs: 4000 }).catch(() => null);

/**
 * Log into the panel on `port` (gateway mode arms sessions on /api/*) and
 * fetch the authenticated snapshot's `gateway` block. Returns null when the
 * panel is unreachable or auth fails — callers assert on the difference.
 */
async function gatewaySnapshot(port, user, password) {
  try {
    const login = await fetch(`http://127.0.0.1:${port}/api/login`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ user, password }),
    });
    if (!login.ok) return null;
    const { token } = await login.json();
    const res = await fetch(`http://127.0.0.1:${port}/api/snapshot`, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

function findChildren(parentPid) {
  const LF = String.fromCharCode(10);
  try {
    const lines = execSync('ps -Ao pid=,ppid=,command=', { encoding: 'utf8' })
      .split(LF)
      .map((l) => l.trim())
      .filter(Boolean);
    return lines
      .map((l) => {
        const parts = l.split(/\s+/);
        return { pid: Number(parts[0]), ppid: Number(parts[1]), cmd: parts.slice(2).join(' ') };
      })
      .filter((p) => p.ppid === parentPid);
  } catch {
    return [];
  }
}

async function main() {
  console.log('─'.repeat(72));
  console.log('E12 Unified Launcher Gate (blitzkrieg single binary)');
  console.log('─'.repeat(72));

  assert(existsSync(BIN), `binary exists: ${BIN}`);
  assert(existsSync(CORE_BIN), `core binary exists: ${CORE_BIN}`);

  // ── 1. Help & Subcommands ──────────────────────────────────────────────────
  console.log('');
  console.log('[1] Subcommands and help output');
  const helpOut = execSync(`"${BIN}" --help`, { encoding: 'utf8' });
  assert(helpOut.includes('blitzkrieg <SUBCOMMAND>'), 'help defines subcommand syntax');
  assert(helpOut.includes('core'), 'help includes core subcommand');
  assert(helpOut.includes('tui [--attach]'), 'help includes tui --attach subcommand');
  assert(helpOut.includes('web'), 'help includes web subcommand');
  assert(helpOut.includes('run'), 'help includes run subcommand');
  assert(helpOut.includes('--readonly'), 'help includes --readonly flag');
  assert(helpOut.includes('--tui, -tui'), 'help documents --tui and the -tui alias');
  assert(helpOut.includes('--web'), 'help documents --web');
  assert(helpOut.includes('--interval-ms'), 'help documents --interval-ms');
  assert(helpOut.includes('--manage'), 'help documents --manage');
  assert(helpOut.includes('--round-sec'), 'help documents --round-sec');
  assert(helpOut.includes('Part launches'), 'help lists the part-launch recipes');

  // ── 2. Unified Launch (core + UI in one command) ───────────────────────────
  console.log('');
  console.log('[2] Unified launch (blitzkrieg run / default)');

  // The gate shell may carry panel credentials; strip them so the launcher's
  // stdout assertions below are deterministic (no .env exists in WORK yet, so
  // the read-only note MUST appear here).
  const credlessEnv = { ...process.env, TMPDIR: WORK };
  delete credlessEnv.BLITZKRIEG_PANEL_USER;
  delete credlessEnv.BLITZKRIEG_PANEL_PASSWORD;

  const child = spawn(
    BIN,
    [
      'run',
      '--socket', SOCK,
      '--mode', 'dry',
      '--tick-ms', '50',
      '--addr', `127.0.0.1:${PORT1}`,
      '--engine',
      '--no-event-archive',
      '--no-trade-log',
      '--no-order-log',
      '--no-position-log',
    ],
    {
      cwd: WORK,
      env: credlessEnv,
      stdio: ['ignore', 'pipe', 'pipe'],
    }
  );
  let phase2Stdout = '';
  child.stdout.on('data', (d) => { phase2Stdout += d.toString(); });
  child.stderr.on('data', (d) => {
    if (process.env.DEBUG) process.stderr.write(d);
  });

  let launcherPid = child.pid;
  assert(Boolean(launcherPid), `launcher spawned with PID ${launcherPid}`);

  // Wait for the socket to be SERVED, not merely to exist: the file appears
  // before the core reaches its accept loop, and a ping that never lands is not
  // readiness. `rpc` maps a refusal to null, so this probe cannot throw.
  const connected = Boolean(await pollUntil(
    async () => existsSync(SOCK) && (await rpc(SOCK, 'core.ping')) !== null,
    { timeoutMs: 4000 },
  ));
  assert(connected, `unified launcher started core and serves socket on ${SOCK}`);
  // The note prints after the core is up; without credentials (env stripped
  // and no .env in WORK yet) the panel must be announced as read-only. The
  // socket can answer BEFORE the launcher reaches the print, so poll briefly
  // instead of asserting a snapshot.
  let noteSeen = false;
  for (let i = 0; i < 30 && !noteSeen; i++) {
    await sleep(100);
    noteSeen = phase2Stdout.includes('read-only mode');
  }
  assert(
    noteSeen,
    'without credentials (env or .env) the launcher says the panel is read-only'
  );

  // Verify process table: core is child of launcher
  const children = findChildren(launcherPid);
  const coreChild = children.find((c) => c.cmd.includes('blitzkrieg-core'));
  assert(Boolean(coreChild), `core child process discovered (PID ${coreChild?.pid}) under parent ${launcherPid}`);

  // Verify IPC response from core
  const ready = await rpc(SOCK, 'core.ready');
  assert(ready !== null && typeof ready === 'object', 'core answers ready query over IPC');
  assert(ready.mode === 'dry', `core started in dry mode (got ${ready.mode})`);

  // ── 3. Clean Shutdown & No Zombie ─────────────────────────────────────────
  console.log('');
  console.log('[3] Clean shutdown and zombie prevention');
  child.kill('SIGTERM');
  const exitCode = await new Promise((resolve) => {
    child.on('close', (code, signal) => resolve({ code, signal }));
    setTimeout(() => resolve({ code: 'TIMEOUT', signal: null }), 5000);
  });
  assert(exitCode.code === 0 || exitCode.code === null, `launcher exited cleanly (${JSON.stringify(exitCode)})`);

  await sleep(200);
  // Ensure child core process is gone
  if (coreChild) {
    const surviving = findChildren(launcherPid);
    assert(!surviving.some((c) => c.pid === coreChild.pid), `child core PID ${coreChild.pid} was reaped (no zombie)`);
  }
  assert(!existsSync(SOCK), `socket ${SOCK} cleaned up`);

  // ── 4. Unified Readonly Mode ──────────────────────────────────────────────
  console.log('');
  console.log('[4] Unified launcher --readonly flag structural check');
  const roChild = spawn(
    BIN,
    [
      'run',
      '--socket', READONLY_SOCK,
      '--readonly',
      '--tick-ms', '50',
      '--addr', `127.0.0.1:${PORT2}`,
      '--no-event-archive',
      '--no-trade-log',
      '--no-order-log',
      '--no-position-log',
    ],
    {
      cwd: WORK,
      env: { ...process.env, TMPDIR: WORK },
      stdio: ['ignore', 'pipe', 'pipe'],
    }
  );

  let roReady = false;
  for (let i = 0; i < 40; i++) {
    await sleep(100);
    if (existsSync(READONLY_SOCK)) {
      const modeResult = await rpc(READONLY_SOCK, 'core.ready');
      if (modeResult && modeResult.mode === 'readonly') {
        roReady = true;
        break;
      }
    }
  }
  assert(roReady, '--readonly flag passed structurally through launcher to core (mode="readonly")');

  roChild.kill('SIGTERM');
  await new Promise((resolve) => roChild.on('close', resolve));
  await sleep(100);

  // ── 5. `blitzkrieg stop` stops the unified stack ───────────────────────────
  // The operator's switch: SIGTERM the launcher (which cascades to the core it
  // spawned), then nothing is left attached to the socket — and the second
  // stop is a no-op.
  console.log('');
  console.log('[5] blitzkrieg stop stops the unified stack, idempotently');
  const stopChild = spawn(
    BIN,
    [
      'run',
      '--socket', STOP_SOCK,
      '--mode', 'dry',
      '--tick-ms', '50',
      '--addr', `127.0.0.1:${PORT3}`,
      '--engine',
      '--no-event-archive',
      '--no-trade-log',
      '--no-order-log',
      '--no-position-log',
    ],
    { cwd: WORK, env: { ...process.env, TMPDIR: WORK }, stdio: ['ignore', 'pipe', 'pipe'] }
  );
  let stopStackReady = false;
  for (let i = 0; i < 40 && !stopStackReady; i++) {
    await sleep(100);
    if (existsSync(STOP_SOCK)) stopStackReady = (await rpc(STOP_SOCK, 'core.ping')) !== null;
  }
  assert(stopStackReady, `stack ready on ${STOP_SOCK} before stop`);
  // Attach the close listener BEFORE stop runs: `close` fires once, and a
  // listener attached after an already-dead child would wait forever.
  const stopChildClosed = new Promise((resolve) => stopChild.on('close', (code) => resolve(code)));

  // execFileSync, NOT execSync: a `sh -c` wrapper would itself carry the
  // binary name and socket path in its command line. `stop` excludes its own
  // ancestor chain, but the gate should not depend on that courtesy.
  const stopOut = execFileSync(BIN, ['stop', '--socket', STOP_SOCK, '--timeout', '10'], { encoding: 'utf8' });
  assert(stopOut.includes('owner pid'), `stop names the owner it signalled: ${stopOut.split('\n')[1]}`);
  const stopClosed = await Promise.race([stopChildClosed, sleep(10_000).then(() => 'TIMEOUT')]);
  assert(stopClosed === 0 || stopClosed === null, `launcher exited when stopped (${stopClosed})`);
  assert(!existsSync(STOP_SOCK), 'socket file removed after stop');
  const stopSecond = execFileSync(BIN, ['stop', '--socket', STOP_SOCK, '--timeout', '5'], { encoding: 'utf8' });
  assert(stopSecond.includes('nothing to stop'), 'a second stop is a no-op');

  // ── 6. `blitzkrieg stop` stops an orphan core and leaves strangers alone ───
  // The incident shape, part 1: a core whose parent is this gate driver (not a
  // blitzkrieg process). Stop must signal the CORE, never the parent, and say so.
  console.log('');
  console.log('[6] blitzkrieg stop stops an orphan core without touching its stranger parent');
  const orphan = spawn(
    CORE_BIN,
    ['--socket', ORPHAN_SOCK, '--mode', 'dry', '--tick-ms', '50',
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    { cwd: WORK, stdio: ['ignore', 'ignore', 'pipe'] }
  );
  // Listener FIRST: stop may reap this child while the gate is between steps,
  // and a close listener attached after that event would wait forever.
  const orphanClosed = new Promise((resolve) => orphan.on('close', resolve));
  let orphanReady = false;
  for (let i = 0; i < 40 && !orphanReady; i++) {
    await sleep(100);
    if (existsSync(ORPHAN_SOCK)) orphanReady = (await rpc(ORPHAN_SOCK, 'core.ping')) !== null;
  }
  assert(orphanReady, `standalone core ready on ${ORPHAN_SOCK}`);
  const orphanOut = execFileSync(BIN, ['stop', '--socket', ORPHAN_SOCK, '--timeout', '10'], { encoding: 'utf8' });
  assert(orphanOut.includes('core pid'), `stop names the core it signalled: ${orphanOut.split('\n')[1]}`);
  assert(
    orphanOut.includes('not a blitzkrieg process; left untouched'),
    'stop reports that the stranger parent was left alone'
  );
  await sleep(200);
  const orphanGone = (await rpc(ORPHAN_SOCK, 'core.ping')) === null;
  assert(orphanGone, 'orphan core stopped');
  assert(!existsSync(ORPHAN_SOCK), 'orphan socket file removed');
  assert(!orphan.killed, 'the gate driver itself was never signalled');
  await Promise.race([orphanClosed, sleep(5_000).then(() => 'TIMEOUT')]);

  // ── 7. `blitzkrieg stop` stops an ADOPTING launcher plus the orphan core ───
  // The incident shape, part 2 (what the operator actually hit): a standalone
  // core is adopted by a `blitzkrieg run` — the panel reads but cannot stop it.
  // One `stop` must take down BOTH the launcher and the core, and still leave
  // the stranger parent alone.
  console.log('');
  console.log('[7] blitzkrieg stop stops an adopting launcher AND the adopted core');
  const adoptCore = spawn(
    CORE_BIN,
    ['--socket', ADOPT_SOCK, '--mode', 'dry', '--tick-ms', '50',
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    { cwd: WORK, stdio: ['ignore', 'ignore', 'pipe'] }
  );
  // Listener first, for the same reason as in [6]: stop reaps this child.
  const adoptCoreClosed = new Promise((resolve) => adoptCore.on('close', resolve));
  let adoptCoreReady = false;
  for (let i = 0; i < 40 && !adoptCoreReady; i++) {
    await sleep(100);
    if (existsSync(ADOPT_SOCK)) adoptCoreReady = (await rpc(ADOPT_SOCK, 'core.ping')) !== null;
  }
  assert(adoptCoreReady, `standalone core ready on ${ADOPT_SOCK}`);
  const adoptLauncher = spawn(
    BIN,
    ['run', '--socket', ADOPT_SOCK, '--tick-ms', '50', '--addr', `127.0.0.1:${PORT4}`,
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    {
      cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
      // Credentials so the gateway block is ON THE WIRE: an adopted core must
      // be reported as `managed: false` — the panel then honestly says 停止 is
      // unavailable instead of offering a button that would be a lie.
      env: {
        ...process.env, TMPDIR: WORK,
        BLITZKRIEG_PANEL_USER: 'gate_user', BLITZKRIEG_PANEL_PASSWORD: 'gate_pass',
      },
    }
  );
  await sleep(500); // give the launcher a moment to boot and adopt
  const adoptSnap = await gatewaySnapshot(PORT4, 'gate_user', 'gate_pass');
  assert(adoptSnap !== null, 'adopting launcher serves an authenticated snapshot');
  assert(adoptSnap?.gateway?.lifecycleEnabled === true, 'lifecycle verbs are enabled for the adopting launcher');
  assert(
    adoptSnap?.gateway?.managed === false,
    `an ADOPTED core is honestly reported as not managed: ${JSON.stringify(adoptSnap?.gateway)}`
  );
  const adoptLauncherClosed = new Promise((resolve) => adoptLauncher.on('close', (code) => resolve(code)));
  const adoptOut = execFileSync(BIN, ['stop', '--socket', ADOPT_SOCK, '--timeout', '10'], { encoding: 'utf8' });
  assert(adoptOut.includes('owner pid'), `stop names the adopting launcher: ${adoptOut.split('\n').find((l) => l.includes('owner'))}`);
  assert(adoptOut.includes('core pid'), `stop names the adopted core: ${adoptOut.split('\n').find((l) => l.includes('core'))}`);
  const adoptClosed = await Promise.race([adoptLauncherClosed, sleep(10_000).then(() => 'TIMEOUT')]);
  assert(adoptClosed === 0 || adoptClosed === null, `adopting launcher exited (${adoptClosed})`);
  await sleep(200);
  assert((await rpc(ADOPT_SOCK, 'core.ping')) === null, 'adopted core stopped');
  assert(!existsSync(ADOPT_SOCK), 'adopted-stack socket file removed');
  assert(!adoptCore.killed, 'the gate driver still was never signalled');
  await Promise.race([adoptCoreClosed, sleep(5_000).then(() => 'TIMEOUT')]);

  // ── 8. `.env` self-load: the launcher reads <cwd>/.env itself ─────────────
  // The start recipe used to demand `set -a; source .env`. Now the launcher
  // loads the file before tokio starts (env wins over file, always). Proof:
  // with credentials present ONLY in a .env file, the read-only note disappears.
  console.log('');
  console.log('[8] .env self-load provides panel credentials without sourcing');
  writeFileSync(join(WORK, '.env'), 'BLITZKRIEG_PANEL_USER=gate_user\nBLITZKRIEG_PANEL_PASSWORD=gate_pass\n');
  const envChild = spawn(
    BIN,
    [
      'run',
      '--socket', ENV_SOCK,
      '--mode', 'dry',
      '--tick-ms', '50',
      '--addr', `127.0.0.1:${PORT5}`,
      '--no-event-archive',
      '--no-trade-log',
      '--no-order-log',
      '--no-position-log',
    ],
    {
      cwd: WORK,
      env: credlessEnv,
      stdio: ['ignore', 'pipe', 'pipe'],
    }
  );
  let envStdout = '';
  envChild.stdout.on('data', (d) => { envStdout += d.toString(); });
  const envChildClosed = new Promise((resolve) => envChild.on('close', (code) => resolve(code)));
  let envReady = false;
  for (let i = 0; i < 40 && !envReady; i++) {
    await sleep(100);
    if (existsSync(ENV_SOCK)) envReady = (await rpc(ENV_SOCK, 'core.ping')) !== null;
  }
  assert(envReady, `stack ready on ${ENV_SOCK}`);
  await sleep(200);
  assert(
    !envStdout.includes('read-only mode'),
    'credentials from .env alone put the panel into managed (non-read-only) mode'
  );
  // THE managed-ownership assertion (the bug this gate exists for): the core
  // this launcher spawned must be reported as MANAGED on the wire, so the
  // panel's 停止 button is live and no "started by another process" notice
  // can appear. The web server and the signal task share one dispatcher.
  const envSnap = await gatewaySnapshot(PORT5, 'gate_user', 'gate_pass');
  assert(envSnap?.gateway?.lifecycleEnabled === true, 'lifecycle verbs are enabled on the wire');
  assert(
    envSnap?.gateway?.managed === true,
    `a core SPAWNED by this launcher is reported as managed: ${JSON.stringify(envSnap?.gateway)}`
  );
  // The file must not leak into the log: a count, never a value.
  assert(!envStdout.includes('gate_pass'), 'the .env values are never printed');
  envChild.kill('SIGTERM');
  await Promise.race([envChildClosed, sleep(10_000).then(() => 'TIMEOUT')]);
  rmSync(join(WORK, '.env'));

  // ── 9. `run --tui`: core + TUI only, NO web listener, no orphan ────────────
  // Headless (piped stdout) the TUI deliberately refuses to start — but the
  // ownership chain runs to the end regardless: the launcher spawns the core
  // FIRST, then the TUI gives up, then the SAME dispatcher stops the core.
  // So a piped run still proves: core up under the launcher, no web port
  // bound, clean exit, core reaped.
  console.log('');
  console.log('[9] run --tui launches the TUI stack without the web listener');
  const tuiChild = spawn(
    BIN,
    [
      'run', '--tui',
      '--socket', TUI_SOCK,
      '--mode', 'dry',
      '--tick-ms', '50',
      '--interval-ms', '200',
      '--addr', `127.0.0.1:${PORT_TUI}`,
      '--no-event-archive',
      '--no-trade-log',
      '--no-order-log',
      '--no-position-log',
    ],
    { cwd: WORK, env: credlessEnv, stdio: ['ignore', 'pipe', 'pipe'] }
  );
  const tuiChildClosed = new Promise((resolve) => tuiChild.on('close', (code) => resolve(code)));
  let tuiStderr = '';
  tuiChild.stderr.on('data', (d) => { tuiStderr += d.toString(); });
  let tuiReady = false;
  for (let i = 0; i < 40 && !tuiReady; i++) {
    await sleep(100);
    if (existsSync(TUI_SOCK)) tuiReady = (await rpc(TUI_SOCK, 'core.ping')) !== null;
  }
  assert(tuiReady, `run --tui spawned the core on ${TUI_SOCK}`);
  // THE partial-launch assertion: the TUI path must never listen on the web
  // address, even though --addr was supplied.
  await sleep(700);
  const webUpOnTuiRun = await new Promise((res) => {
    const c = net.connect(PORT_TUI, '127.0.0.1');
    c.on('connect', () => { c.end(); res(true); });
    c.on('error', () => res(false));
  });
  assert(!webUpOnTuiRun, `run --tui does NOT bind the web address ${PORT_TUI}`);
  // Headless, the TUI says why it quits and the launcher exits 0 after
  // reaping the core through the shared dispatcher.
  const tuiExit = await Promise.race([tuiChildClosed, sleep(15_000).then(() => 'TIMEOUT')]);
  assert(tuiExit === 0 || tuiExit === null, `run --tui exits cleanly headless (${tuiExit})`);
  assert(
    tuiStderr.includes('interactive terminal'),
    'the TUI explains it needs a terminal instead of crashing'
  );
  await sleep(300);
  assert((await rpc(TUI_SOCK, 'core.ping')) === null, 'the TUI-run core was reaped after exit');
  assert(!existsSync(TUI_SOCK), 'TUI-run socket file removed');

  // ── 10. `tui --attach` is adopt-only: it watches, never kills ──────────────
  console.log('');
  console.log('[10] tui --attach watches an existing core and leaves it running');
  const attachCore = spawn(
    CORE_BIN,
    ['--socket', ATTACH_SOCK, '--mode', 'dry', '--tick-ms', '50',
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    { cwd: WORK, stdio: ['ignore', 'ignore', 'pipe'] }
  );
  const attachCoreClosed = new Promise((resolve) => attachCore.on('close', resolve));
  let attachReady = false;
  for (let i = 0; i < 40 && !attachReady; i++) {
    await sleep(100);
    if (existsSync(ATTACH_SOCK)) attachReady = (await rpc(ATTACH_SOCK, 'core.ping')) !== null;
  }
  assert(attachReady, `standalone core ready on ${ATTACH_SOCK}`);
  const attachTui = spawn(
    BIN,
    ['tui', '--attach', '--socket', ATTACH_SOCK],
    { cwd: WORK, env: credlessEnv, stdio: ['ignore', 'pipe', 'pipe'] }
  );
  const attachTuiClosed = new Promise((resolve) => attachTui.on('close', (code) => resolve(code)));
  const attachTuiExit = await Promise.race([attachTuiClosed, sleep(10_000).then(() => 'TIMEOUT')]);
  assert(attachTuiExit === 0 || attachTuiExit === null, `tui --attach exits cleanly headless (${attachTuiExit})`);
  await sleep(300);
  assert(
    (await rpc(ATTACH_SOCK, 'core.ping')) !== null,
    'the watched core is STILL alive after tui --attach exits'
  );
  assert(!attachTui.killed, 'tui --attach never signalled the core (or anyone else)');
  attachCore.kill('SIGTERM');
  await Promise.race([attachCoreClosed, sleep(5_000).then(() => 'TIMEOUT')]);
  assert((await rpc(ATTACH_SOCK, 'core.ping')) === null, 'watched core stops when explicitly signalled');

  // ── 11. Origin gate: same-origin passes, cross-origin is refused ────────────
  // The server-deployment semantics: a browser on the machine's real address
  // sends Referer/Origin == its Host, and that must pass WITHOUT allowlist
  // config, while a foreign origin stays 403. Driven over RAW sockets because
  // the interesting variable is the Host header itself (fetch forbids setting
  // it). The status ladder: origin-refused = 403 BEFORE auth; same-origin but
  // no session = 401 (the origin passed, auth did the rest). A `web` server
  // answers the statuses without needing a core.
  console.log('');
  console.log('[11] origin gate: same-origin allowed, cross-origin refused, allowlist honored');
  const rawGet = (port, hostHeader, extra) =>
    new Promise((resolve) => {
      const c = net.connect(port, '127.0.0.1');
      let buf = '';
      c.on('connect', () => {
        c.write(
          `GET /api/snapshot HTTP/1.1\r\nHost: ${hostHeader}\r\nConnection: close\r\n${extra || ''}\r\n`
        );
      });
      c.on('data', (d) => { buf += d.toString(); });
      c.on('close', () => resolve(Number((buf.match(/^HTTP\/1\.[01] (\d{3})/) || [])[1]) || 0));
      c.on('error', () => resolve(0));
    });
  const waitHttp = async (port) => {
    for (let i = 0; i < 60; i++) {
      await sleep(100);
      const up = await new Promise((res) => {
        const c = net.connect(port, '127.0.0.1');
        c.on('connect', () => { c.end(); res(true); });
        c.on('error', () => res(false));
      });
      if (up) return true;
    }
    return false;
  };
  const spawnWeb = (port, extraArgs) => {
    const child = spawn(
      BIN,
      ['web', '--socket', join(WORK, 'origin.sock'), '--addr', `127.0.0.1:${port}`,
       '--manage', ...extraArgs],
      {
        cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
        env: { ...process.env, BLITZKRIEG_PANEL_USER: 'gate_user', BLITZKRIEG_PANEL_PASSWORD: 'gate_pass' },
      }
    );
    return { child, closed: new Promise((resolve) => child.on('close', resolve)) };
  };

  // 9a. Same-origin / cross-origin with NO allowlist configured.
  const PORT6 = 63000 + Math.floor(Math.random() * 2000);
  const plain = spawnWeb(PORT6, []);
  assert(await waitHttp(PORT6), `web server serves on ${PORT6}`);
  assert(
    (await rawGet(PORT6, '192.168.0.153:51888', 'Origin: http://evil.example\r\n')) === 403,
    'a foreign Origin is refused'
  );
  const sameStatus = await rawGet(
    PORT6, '192.168.0.153:51888', 'Referer: http://192.168.0.153:51888/panel/\r\n'
  );
  assert(
    sameStatus !== 403,
    `same-origin (Referer authority == Host) passes the gate (${sameStatus}; 401 = origin passed, auth next)`
  );
  assert(
    (await rawGet(PORT6, '192.168.0.153:51888', 'Referer: http://192.168.0.153:9999/panel/\r\n')) === 403,
    'same host on another port stays refused'
  );
  plain.child.kill('SIGTERM');
  await Promise.race([plain.closed, sleep(5_000).then(() => 'TIMEOUT')]);

  // 9b. The allowlist wiring (--allowed-origin / BLITZKRIEG_ALLOWED_ORIGINS):
  // a proxy-fronted origin that is neither same-origin nor loopback passes
  // when listed and is refused when not.
  const PORT7 = 51000 + Math.floor(Math.random() * 1000);
  const listed = spawnWeb(PORT7, ['--allowed-origin', 'http://panel.example.com']);
  assert(await waitHttp(PORT7), `web server serves on ${PORT7}`);
  assert(
    (await rawGet(PORT7, 'internal.host:51888', 'Origin: http://panel.example.com\r\n')) !== 403,
    'the allowlisted origin passes'
  );
  assert(
    (await rawGet(PORT7, 'internal.host:51888', 'Origin: http://other.example\r\n')) === 403,
    'an unlisted origin stays refused'
  );
  listed.child.kill('SIGTERM');
  await Promise.race([listed.closed, sleep(5_000).then(() => 'TIMEOUT')]);

  // ── 12. No family process may survive the whole gate ───────────────────────
  // Every core and launcher this gate started must be gone — this is what turns
  // a stack that outlived its owner (the readonly orphan shape) into a caught
  // failure instead of a mystery process on the operator's machine.
  console.log('');
  console.log('[12] nothing the gate started is left behind');
  const leftover = execSync('ps -ww -axo pid=,ppid=,command=', { encoding: 'utf8' })
    .split('\n')
    .filter((l) => l.includes(WORK) && (l.includes('blitzkrieg-core') || l.includes('blitzkrieg run') || l.includes('blitzkrieg web') || l.includes('blitzkrieg tui')));
  assert(leftover.length === 0, `no family process left in ${WORK}: ${leftover.join(' | ')}`);

  // Clean up scratch dir
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}

  console.log('─'.repeat(72));
  console.log('RESULT: PASS — E12 single binary launcher verified end-to-end');
  console.log('─'.repeat(72));
}

main().catch((e) => {
  console.error('FAIL: uncaught error in unified-launcher-check', e);
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
  process.exit(1);
});
