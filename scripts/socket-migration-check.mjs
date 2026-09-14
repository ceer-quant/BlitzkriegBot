#!/usr/bin/env node
/**
 * Regression for the socket rename migration window (MIGRATION_LOG §38).
 *
 * A node shell started BEFORE the rename passes `--socket <legacy path>` to its
 * core, so the running core legitimately binds `clodds-core-<user>.sock` until
 * that shell restarts. A freshly started client must ADOPT that core, not spawn a
 * rival on the new name: two cores on one cwd interleave their order/position
 * logs, and the archive's single-writer lock silently stops one of them.
 *
 * Hermetic: runs in its own TMPDIR + USER under a scratch cwd, so the production
 * core/socket/data dir are untouched.
 */
import { spawn } from 'child_process';
import { mkdtempSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';

const TMP = mkdtempSync(join(tmpdir(), 'bzk-migrate-'));
const WORKDIR = mkdtempSync(join(TMP, 'cwd-'));
// Private namespace: the child core and the client must agree on both vars.
process.env.TMPDIR = TMP;
process.env.USER = 'migrateprobe';

const { defaultSocketPath, legacySocketPath, socketServed } = await import('../scripts/lib/core-socket.mjs');
const { BlitzkriegCoreClient } = await import('../dist/core/blitzkrieg-core-client.js');

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const CANON = defaultSocketPath();
const LEGACY = legacySocketPath();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// 1) An "old" core: bound to the pre-rename name, exactly as a pre-rename shell
//    would have spawned it.
const old = spawn(BIN, [
  '--socket', LEGACY, '--mode', 'dry', '--tick-ms', '100',
  '--seed-balance', '1000', '--no-trade-log', '--no-order-log', '--no-position-log',
  '--no-event-archive',
], { stdio: 'ignore', cwd: WORKDIR });
for (let i = 0; i < 100 && !(await socketServed(LEGACY)); i++) await sleep(50);
console.log('old core pid', old.pid, 'bound to the legacy name');
console.log('  legacy :', LEGACY);
console.log('  canon  :', CANON);

// 2) A new client with no explicit socketPath must resolve to the legacy path and
//    adopt, NOT spawn on the canonical one.
const client = new BlitzkriegCoreClient({
  binaryPath: BIN, mode: 'dry', seedBalance: 1000, maxOrderNotional: 5,
  tickMs: 100, autoRestart: true, cwd: WORKDIR,
  noTradeLog: true, noOrderLog: true, noPositionLog: true,
  extraArgs: ['--no-event-archive'],
});
let spawned = 0;
client.on('disconnect', () => { spawned += 1; });
await client.start().catch((e) => console.log('client.start() rejected:', e.message));
await sleep(1200);

const connected = client.isConnected();
const adoptedSocket = client.resolvedSocketPath();
const canonServed = await socketServed(CANON);
const legacyStillServed = await socketServed(LEGACY);
// The old core must be the only one alive for this namespace.
const { execSync } = await import('child_process');
const cores = execSync(`pgrep -f "blitzkrieg-core --socket ${LEGACY}" || true`).toString().trim().split('\n').filter(Boolean);

console.log('\nclient connected          :', connected);
console.log('client socket             :', adoptedSocket);
console.log('canonical now served      :', canonServed, '(must be false — no rival)');
console.log('legacy still served       :', legacyStillServed);
console.log('cores on the legacy socket:', cores.length);

await client.stop();
await sleep(300);
const oldAlive = (() => { try { process.kill(old.pid, 0); return true; } catch { return false; } })();
console.log('old core survived stop()  :', oldAlive);

try { old.kill('SIGTERM'); } catch {}
await sleep(300);
for (const p of [CANON, LEGACY]) { try { unlinkSync(p); } catch {} }

const adoptedOk = connected
  && adoptedSocket === LEGACY
  && !canonServed
  && legacyStillServed
  && cores.length === 1
  && oldAlive;

// 3) Complementary case: with no core running at all, a fresh client must spawn
//    its OWN core on the canonical name (otherwise the rename never takes effect).
const fresh = new BlitzkriegCoreClient({
  binaryPath: BIN, mode: 'dry', seedBalance: 1000, maxOrderNotional: 5,
  tickMs: 100, autoRestart: false, cwd: WORKDIR,
  noTradeLog: true, noOrderLog: true, noPositionLog: true,
  extraArgs: ['--no-event-archive'],
});
await fresh.start().catch((e) => console.log('fresh client start rejected:', e.message));
await sleep(800);
const freshSocket = fresh.resolvedSocketPath();
const freshCanonServed = await socketServed(CANON);
console.log('\nfresh client socket       :', freshSocket);
console.log('fresh spawned canonical   :', freshCanonServed);
const freshOk = freshSocket === CANON && freshCanonServed;
await fresh.stop();
await sleep(400);
for (const p of [CANON, LEGACY]) { try { unlinkSync(p); } catch {} }

const ok = adoptedOk && freshOk;
console.log(
  `\nRESULT: ${ok ? 'PASS' : 'FAIL'} — adopt the pre-rename core (${adoptedOk ? 'ok' : 'FAIL'}), ` +
  `spawn canonical when none is running (${freshOk ? 'ok' : 'FAIL'})`
);
process.exit(ok ? 0 : 1);
