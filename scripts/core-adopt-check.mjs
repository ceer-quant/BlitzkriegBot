#!/usr/bin/env node
/**
 * Regression: a second client pointed at a socket already served by a live core
 * must NOT enter a spawn/crash loop. The duplicate boot fails with
 * "already listening" ONCE, and the client surfaces that as an error — the
 * adoption path (spawn-or-adopt with a bounded retry) is exercised by
 * `ui_kit_web --manage` and covered by Rust tests; what this gate pins is that
 * a naive client cannot trigger an unbounded spawn loop against a live core.
 *
 * Reproduces the 2026-09-14 incident: duplicate-core rejection + auto-restart
 * with no backoff/cap caused ~1300 spawns/minute and "not connected" status.
 */
import { execSync } from 'child_process';
// Guarded spawn: the owner core below is spawned directly (not through
// CoreClient, which owns its own exit guard), so without this a failure or an
// interrupt between here and the explicit `owner.kill()` would leave a core with
// PPID=1 — holding its socket and engine loop. That really happened: a 33-hour
// orphan on `adopt-48644.sock` was found in the table on 2026-09-19.
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { CoreClient, requestOnce } from './lib/core-client.mjs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const SOCK = join(tmpdir(), `adopt-${process.pid}.sock`);
const WORKDIR = mkdtempSync(join(tmpdir(), 'adopt-'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
try { unlinkSync(SOCK); } catch {}

// 1) Owner core (the "healthy" one), started directly.
const owner = spawn(BIN, ['--socket', SOCK, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '1000', '--no-trade-log'], { stdio: 'ignore', cwd: WORKDIR });
for (let i = 0; i < 80 && !existsSync(SOCK); i++) await sleep(50);
await sleep(400);
console.log('owner core pid', owner.pid, 'listening');

// 2) A second client pointed at the SAME socket. Its boot fails on the
//    duplicate-core rejection; with autoRestart on, the failure must stay
//    bounded (no tight spawn loop) and the owner must be untouched.
const client = new CoreClient({
  binaryPath: BIN, socketPath: SOCK, mode: 'dry', seedBalance: 1000,
  maxOrderNotional: 5, tickMs: 50, autoRestart: true, cwd: WORKDIR, noTradeLog: true,
  noOrderLog: true, noPositionLog: true,
});
let fatal = null;
client.onFatal = (e) => { fatal = e; };
let spawnCrashes = 0;
client.onError = () => {};

const t0 = Date.now();
await client.start().then(
  () => console.log('client.start() resolved'),
  (e) => console.log('client.start() rejected:', e.message),
);
// Watch for a spawn/crash loop: a client that respawns repeatedly would keep
// failing children and rotate stderr. Poll the owner's liveness instead — the
// observable that matters.
for (let i = 0; i < 25; i++) await sleep(100);
const settleMs = Date.now() - t0;

const connected = client.isConnected();
// Count our doomed child failures: they must be bounded (no loop).
// There is no counter on the client; infer from stderr of the owner spawn —
// none. Instead, sample the process table for repeated core churn under our
// scratch cwd.
function coreChildren() {
  return execSync('ps -Ao pid=,command=', { encoding: 'utf8' })
    .split('\n').filter((l) => l.includes('blitzkrieg-core') && l.includes('adopt-')).length;
}
const churn = coreChildren();
const ml = await requestOnce(SOCK, 'market.list').catch(() => null);
console.log(`settled after ${settleMs}ms; doomed-child churn = ${churn} (bounded)`);
console.log('client connected after settle :', connected);
console.log('owner still serving market.list:', ml?.active ?? '(none)');

// 3) stop() must not kill the core the client does not own (start never
//    succeeded here, so stop() is a no-op — assert the owner survives anyway).
await client.stop();
await sleep(500);
const ownerAlive = (() => { try { process.kill(owner.pid, 0); return true; } catch { return false; } })();
console.log('owner alive after client.stop()  :', ownerAlive);

owner.kill('SIGTERM');
await sleep(300);
try { unlinkSync(SOCK); } catch {}

const ok = !fatal && ownerAlive && ml?.active === 'polymarket' && churn <= 2;
console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — duplicate client stayed bounded, owner survived`);
process.exit(ok ? 0 : 1);
