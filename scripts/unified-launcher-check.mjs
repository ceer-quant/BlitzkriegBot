#!/usr/bin/env node
/**
 * E12 unified launcher gate — "单二进制一体化启动门禁".
 *
 * Exercises the unified `blitzkrieg` binary across its primary subcommands and invariants:
 *   1. `blitzkrieg --help` discloses `core`, `tui`, `web`, `run` subcommands.
 *   2. `blitzkrieg run` launches core + UI in one command with `lifecycle: on` by default.
 *   3. Child core process is owned by the launcher (PPID equals launcher PID).
 *   4. Core answers real JSON-RPC over the isolated UDS socket.
 *   5. `blitzkrieg run --readonly` structurally passes `--readonly` to core (mode = "readonly").
 *   6. Graceful shutdown: on SIGTERM, parent reaps child core, unbinds socket, leaving no zombies.
 *
 * Run: node scripts/unified-launcher-check.mjs
 */

import { execSync } from 'node:child_process';
// Guarded spawn: `blitzkrieg run` starts a core as its own child, so a failure or
// an interrupt between here and the launcher's SIGTERM leaves the launcher AND a
// core behind. The guard reaps the launcher's whole process group, which includes
// that core, and does it with SIGTERM first so the core still unlinks its socket.
import { spawn, reapAllChildren } from './lib/child-guard.mjs';
import { mkdtempSync, rmSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import net from 'node:net';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg');
const CORE_BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const WORK = mkdtempSync(join(tmpdir(), 'unified-launcher-'));
const SOCK = join(WORK, 'unified.sock');
const READONLY_SOCK = join(WORK, 'readonly.sock');
const PORT1 = 52000 + Math.floor(Math.random() * 2000);
const PORT2 = 54000 + Math.floor(Math.random() * 2000);

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

function rpc(sock, method, params = {}) {
  const LF = String.fromCharCode(10);
  return new Promise((res) => {
    let resolved = false;
    const c = net.connect(sock);
    let b = '';
    c.on('connect', () =>
      c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + LF)
    );
    c.on('data', (d) => {
      b += d.toString('utf8');
      let idx;
      while ((idx = b.indexOf(LF)) >= 0) {
        const line = b.slice(0, idx).trim();
        b = b.slice(idx + 1);
        if (!line) continue;
        try {
          const parsed = JSON.parse(line);
          if (parsed && parsed.id === 1) {
            resolved = true;
            c.end();
            return res(parsed.error ? { __error: parsed.error } : (parsed.result ?? null));
          }
        } catch {}
      }
    });
    c.on('error', () => {
      if (!resolved) {
        resolved = true;
        res(null);
      }
    });
    setTimeout(() => {
      if (!resolved) {
        resolved = true;
        try { c.end(); } catch {}
        res(null);
      }
    }, 4000);
  });
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

  // ── 2. Unified Launch (core + UI in one command) ───────────────────────────
  console.log('');
  console.log('[2] Unified launch (blitzkrieg run / default)');
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
      env: { ...process.env, TMPDIR: WORK },
      stdio: ['ignore', 'pipe', 'pipe'],
    }
  );
  child.stderr.on('data', (d) => {
    if (process.env.DEBUG) process.stderr.write(d);
  });

  let launcherPid = child.pid;
  assert(Boolean(launcherPid), `launcher spawned with PID ${launcherPid}`);

  // Wait for socket to become ready
  let connected = false;
  for (let i = 0; i < 40; i++) {
    await sleep(100);
    if (existsSync(SOCK)) {
      const ping = await rpc(SOCK, 'core.ping');
      if (ping !== null) {
        connected = true;
        break;
      }
    }
  }
  assert(connected, `unified launcher started core and serves socket on ${SOCK}`);

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
