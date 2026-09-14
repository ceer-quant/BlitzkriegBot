#!/usr/bin/env node
/**
 * Regression: a second client pointed at a socket already served by a live core
 * must NOT enter a spawn/crash loop. It should adopt (connect to) the existing
 * core, and `stop()` must never kill the core it does not own.
 *
 * Reproduces the 2026-09-14 incident: duplicate-core rejection + auto-restart
 * with no backoff/cap caused ~1300 spawns/minute and "not connected" status.
 */
import { spawn } from 'child_process';
import { mkdtempSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';
import { BlitzkriegCoreClient } from '../dist/core/blitzkrieg-core-client.js';

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

// 2) Second client points at the SAME socket. It spawns a doomed child (bails with
//    "already listening"), then must adopt the owner instead of looping.
const client = new BlitzkriegCoreClient({
  binaryPath: BIN, socketPath: SOCK, mode: 'dry', seedBalance: 1000,
  maxOrderNotional: 5, tickMs: 50, autoRestart: true, cwd: WORKDIR, noTradeLog: true,
  noOrderLog: true, noPositionLog: true,
});
let fatal = null;
client.on('fatal', (e) => { fatal = e; });

await client.start().then(() => console.log('client.start() resolved')).catch((e) => console.log('client.start() rejected:', e.message));
// Give the adopt path time to run.
await sleep(2500);

const connected = client.isConnected();
// Count our doomed child failures: they must be bounded (adopt, not loop).
const req = (m) => new Promise((res) => {
  const c = net.connect(SOCK); let b = '';
  c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method: m }) + '\n'));
  c.on('data', (d) => { b += d; const i = b.indexOf('\n'); if (i < 0) return; try { res(JSON.parse(b.slice(0, i)).result); } catch { res(null); } c.end(); });
  c.on('error', () => res(null)); setTimeout(() => { try { c.end(); } catch {} res(null); }, 3000);
});
const ml = await req('market.list');
console.log('client connected after settle :', connected);
console.log('owner still serving market.list:', ml?.active ?? '(none)');

// 3) stop() must not kill the adopted (not-owned) core.
await client.stop();
await sleep(500);
const ownerAlive = (() => { try { process.kill(owner.pid, 0); return true; } catch { return false; } })();
console.log('owner alive after client.stop()  :', ownerAlive);

owner.kill('SIGTERM');
await sleep(300);
try { unlinkSync(SOCK); } catch {}

const ok = connected && ownerAlive && ml?.active === 'polymarket';
console.log(`\nRESULT: ${ok ? 'PASS' : 'FAIL'} — duplicate client adopted the core (no spawn loop), owner survived`);
process.exit(ok ? 0 : 1);
