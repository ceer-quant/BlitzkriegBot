#!/usr/bin/env node
/**
 * soak-monitor gate — proves the sampler's lifetime modes are distinguishable.
 *
 * D-30 turned `soak-monitor.mjs` into a process that can run forever. That is a
 * deliberate change to a property the health check depends on: KI-30 found that
 * judging "is sampling happening?" by process name is wrong *because* the
 * sampler had a bounded lifetime and exiting was its normal end. A resident
 * sampler removes that specific reason — but it also introduces the opposite
 * hazard, and this gate exists to pin both down:
 *
 *   * the resident mode must be ASKED FOR. A typo'd `--hours abc` parses to NaN;
 *     `Math.round(NaN)` would make the cycle loop run zero times, so the monitor
 *     would sample nothing and exit 0 — a check that can never fire, wearing a
 *     scheduler's clothes. Any unparseable horizon must be a loud exit 2.
 *   * the STOP SWITCH must work and be VISIBLE. SIGTERM has to end the process
 *     promptly (not after the current 600s interval) and the closing log line
 *     must say it was stopped, not that the soak completed — otherwise a
 *     deliberately stopped monitor is indistinguishable from a finished soak in
 *     the only artifact anyone reads afterwards.
 *   * the bounded mode must still be the default and must still say COMPLETE.
 *     Widening one mode must not quietly change the other.
 *
 * Nothing here touches the real deployment: the socket comes from `TMPDIR` +
 * `USER` (see `lib/core-socket.mjs`), and output goes to a throwaway directory
 * via `BK_SOAK_DIR`, the same seam `soak-health.sh` reads. Spawned processes go
 * through `lib/child-guard.mjs` so a crashed run cannot leave orphans (KI-29).
 *
 * Run: node scripts/soak-monitor-check.mjs
 */
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, mkdirSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import net from 'node:net';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, '..');
const MONITOR = join(ROOT, 'scripts', 'soak-monitor.mjs');

const WORK = mkdtempSync(join(tmpdir(), 'soak-monitor-check-'));
const SOAK_DIR = join(WORK, 'soak');
// NOT under WORK. `TMPDIR` becomes the literal socket directory, and a UDS path
// is capped around 104 bytes — macOS's default temp dir already spends most of
// that, so `<long tmpdir>/sock/blitzkrieg-core-<user>.sock` fails with EINVAL.
// The socket dir is therefore the shortest thing that is still per-run unique,
// which is also what `lib/core-socket.mjs` warns about.
const SOCK_DIR = mkdtempSync('/tmp/bksm-');
const noop = () => {};

let failures = 0;
let liveServers = new Set();

// Same reasoned bound as the soak-health gate: a gate that can hang is a gate
// that cannot fail on time.
const WATCHDOG_MS = Number(process.env.BK_GATE_WATCHDOG_MS ?? 180000);
const watchdog = setTimeout(() => {
  console.error(`  FAIL gate did not exit within ${WATCHDOG_MS / 1000}s — a fixture server is still listening:`);
  for (const srv of liveServers) console.error(`         ${JSON.stringify(srv.address?.())}`);
  if (liveServers.size === 0) console.error('         (no tracked server — some other handle is held)');
  console.error('RESULT: FAIL — gate wedged (the assertions above may all have passed)');
  process.exit(1);
}, WATCHDOG_MS);
watchdog.unref?.();

function ok(msg) { console.log(`  ok   ${msg}`); }
function bad(msg) { console.error(`  FAIL ${msg}`); failures++; }
function assert(cond, msg) { cond ? ok(msg) : bad(msg); }

/**
 * A fixture core: a UDS server that answers every JSON-RPC call the monitor
 * makes with a shape its `cycle()` can read. The monitor is a *client* — it must
 * never spawn a core — so this is all a "live" deployment looks like to it.
 */
function startCoreSock(path) {
  return new Promise((res) => {
    rmSync(path, { force: true });
    const srv = net.createServer((sock) => {
      let buf = '';
      sock.on('data', (d) => {
        buf += d.toString();
        let i;
        while ((i = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          if (!line.trim()) continue;
          let msg = {};
          try { msg = JSON.parse(line); } catch (e) { noop(e); }
          if (msg.id == null) continue;
          const results = {
            'core.ping': { pong: true, ts: Date.now() },
            'engine.stats': { books: 10, spots: 5, rounds: 1, evaluations: 1, signals: 1, confirmed: [], blocked: { timing: 0, momentum: 0 } },
            'engine.round': { slot: 12345, timeLeftSec: 600, markets: 2, canTrade: true },
            'orders.list': { orders: [] },
            'positions.list': { positions: [] },
          };
          const result = results[msg.method] ?? {};
          sock.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
        }
      });
      sock.on('error', noop);
    });
    srv.on('close', () => liveServers.delete(srv));
    srv.listen(path, () => { liveServers.add(srv); res(srv); });
  });
}

/** Spawn the monitor under a fixture environment; resolves on exit. */
function runMonitor(args, { env = {}, killAfterMs = 0, signal = 'SIGTERM' } = {}) {
  return new Promise((res) => {
    const fullEnv = {
      ...process.env,
      TMPDIR: SOCK_DIR,
      USER: 'soakmon',
      BK_SOAK_DIR: SOAK_DIR,
      BK_RUN_LOG: join(WORK, 'run.log'),
      ...env,
    };
    const child = spawn(process.execPath, [MONITOR, ...args], { cwd: ROOT, env: fullEnv, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    child.stdout.on('data', (d) => { out += d.toString(); });
    child.stderr.on('data', (d) => { out += d.toString(); });
    let killed = false;
    if (killAfterMs > 0) {
      setTimeout(() => {
        killed = true;
        child.kill(signal);
      }, killAfterMs);
    }
    const hardStop = setTimeout(() => child.kill('SIGKILL'), killAfterMs > 0 ? killAfterMs + 20000 : 60000);
    child.on('close', (code, sig) => {
      clearTimeout(hardStop);
      res({ code, sig, out, killed });
    });
  });
}

const soakLog = () => {
  const p = join(SOAK_DIR, 'soak.log');
  return existsSync(p) ? readFileSync(p, 'utf8') : '';
};
/**
 * The LAST summary banner in soak.log. soak.log is appended to across runs (that
 * is its purpose), so "the log contains SOAK COMPLETE" is true forever after the
 * first bounded run and says nothing about the run just finished. The property
 * that matters is which banner this run left as the file's final word.
 */
const lastSummary = () => {
  const lines = soakLog().split('\n').filter((l) => /^=== SOAK (COMPLETE|STOPPED)/.test(l));
  return lines[lines.length - 1] || '';
};
const soakLines = () => {
  const p = join(SOAK_DIR, 'soak.jsonl');
  return existsSync(p) ? readFileSync(p, 'utf8').split('\n').filter(Boolean) : [];
};

mkdirSync(SOAK_DIR, { recursive: true });
mkdirSync(SOCK_DIR, { recursive: true });

// ── 1. argument validation: a bad horizon must never become a silent no-op ──
console.log('1. a mistyped horizon is a loud error, not a monitor that samples nothing');
{
  const r = await runMonitor(['--hours', 'abc']);
  assert(r.code === 2, `--hours abc → exit 2 (got ${r.code})`);
  assert(r.out.includes('invalid --hours'), 'the bad value is named in the error');

  const r2 = await runMonitor(['--interval-sec', '0']);
  assert(r2.code === 2, `--interval-sec 0 → exit 2 (got ${r2.code})`);
  assert(r2.out.includes('invalid --interval-sec'), 'the bad interval is named in the error');

  const r3 = await runMonitor(['--hours', '-1']);
  assert(r3.code === 2, `--hours -1 → exit 2 (got ${r3.code})`);
}

// ── 2. bounded mode stays the default and still reports COMPLETE ────────────
console.log('2. bounded mode (the default) still completes on its own');
{
  const sock = await startCoreSock(join(SOCK_DIR, 'blitzkrieg-core-soakmon.sock'));
  // cycles = round(hours*3600/interval) = round(0.0006*3600/1) = 2, so this run
  // ends by itself in ~1s. `--hours 1 --interval-sec 1` would be 3600 cycles and
  // the hard stop would kill it, turning a lifetime test into a timeout test.
  const r = await runMonitor(['--hours', '0.0006', '--interval-sec', '1']);
  assert(r.code === 0, `bounded run exits 0 (got ${r.code})`);
  assert(r.out.includes('SOAK COMPLETE'), 'bounded run reports SOAK COMPLETE');
  assert(!r.out.includes('SOAK STOPPED'), 'bounded run is NOT reported as stopped');
  assert(soakLines().length >= 1, 'bounded run appended at least one sample');
  sock.close();
  liveServers.delete(sock);
}
{
  // Explicit --forever is the ONLY way to get a resident run.
  const before = soakLines().length;
  const sock = await startCoreSock(join(SOCK_DIR, 'blitzkrieg-core-soakmon.sock'));
  const r = await runMonitor(['--forever', '--interval-sec', '1'], { killAfterMs: 2500 });
  assert(r.killed, 'the resident run was still alive when the stop signal was sent (anti-vacuous)');
  assert(r.code === 0, `resident run exits 0 after SIGTERM (got ${r.code})`);
  assert(r.out.includes('resident (--forever)'), 'the resident mode announces itself');
  assert(soakLines().length > before, 'resident run kept sampling before the stop');
  assert(lastSummary().startsWith('=== SOAK STOPPED'), 'the resident run left the STOPPED banner, not COMPLETE');
  sock.close();
  liveServers.delete(sock);
}

// ── 3. the stop switch is prompt AND visible in the artifact ────────────────
console.log('3. SIGTERM stops promptly and the log says WHY it ended');
{
  const sock = await startCoreSock(join(SOCK_DIR, 'blitzkrieg-core-soakmon.sock'));
  // A long interval is the point: if TERM were only honoured between cycles,
  // this run would need 600s to end. 3s of patience proves the 1s slicing works.
  const t0 = Date.now();
  const r = await runMonitor(['--forever', '--interval-sec', '600'], { killAfterMs: 1200 });
  const elapsed = Date.now() - t0;
  assert(r.code === 0, `resident run with a 600s interval still exits 0 (got ${r.code})`);
  assert(elapsed < 15000, `SIGTERM is honoured during the interval (took ${elapsed}ms, interval was 600000ms)`);
  const stoppedLine = lastSummary();
  assert(stoppedLine.includes('SOAK STOPPED (SIGTERM)'), 'the closing banner names the stop signal');
  assert(!stoppedLine.includes('COMPLETE'), 'the closing banner is NOT a completion');
  assert(/after cycle \d+/.test(stoppedLine), 'the closing banner reports how many cycles actually ran');
  sock.close();
  liveServers.delete(sock);
}

// ── 4. SIGINT is the same switch (Ctrl-C on a foreground run) ───────────────
console.log('4. SIGINT is an equivalent stop switch');
{
  const sock = await startCoreSock(join(SOCK_DIR, 'blitzkrieg-core-soakmon.sock'));
  const r = await runMonitor(['--forever', '--interval-sec', '600'], { killAfterMs: 1200, signal: 'SIGINT' });
  assert(r.code === 0, `SIGINT → exit 0 (got ${r.code})`);
  assert(lastSummary().includes('SOAK STOPPED (SIGINT)'), 'the closing banner names SIGINT specifically');
  sock.close();
  liveServers.delete(sock);
}

// ── 5. the resident pair wrapper: stop switch + no double sampler ───────────
console.log('5. soak-resident.sh owns the pair: idempotent start, real stop');
{
  const RESIDENT = join(ROOT, 'scripts', 'soak-resident.sh');
  const env = {
    ...process.env,
    TMPDIR: SOCK_DIR,
    USER: 'soakmon',
    BK_SOAK_DIR: SOAK_DIR,
    BK_RUN_LOG: join(WORK, 'run.log'),
    // Point the health check's probes away from the real deployment: this gate
    // spawns the REAL loop, and a loop that read the real data/ would make the
    // gate a consumer of production state (it stays read-only, but the whole
    // point of the seams is that fixtures never depend on it).
    BK_CORE_PGREP: 'bk-soakmon-nonexistent-core',
    BK_TRADES: join(WORK, 'no-trades.jsonl'),
    BK_ARCH_DIR: SOAK_DIR,
    BK_PANEL_URL: 'http://127.0.0.1:1',
  };
  const sh = (args) => new Promise((res) => {
    const c = spawn('bash', [RESIDENT, ...args], { cwd: ROOT, env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    c.stdout.on('data', (d) => { out += d.toString(); });
    c.stderr.on('data', (d) => { out += d.toString(); });
    c.on('close', (code) => res({ code, out }));
  });

  let r = await sh(['status']);
  assert(r.out.includes('health loop: not running'), 'status before start: loop not running');

  r = await sh(['start', '--interval-sec', '3600', '--sample-sec', '3600']);
  assert(r.code === 0, `start exits 0 (got ${r.code})`);
  assert(r.out.includes('started: health loop'), 'start reports the health loop');
  assert(r.out.includes('started: sampler'), 'start reports the sampler');

  const pidLoop = Number(readFileSync(join(SOAK_DIR, 'resident-loop.pid'), 'utf8').trim());
  const pidSample = Number(readFileSync(join(SOAK_DIR, 'resident-sampler.pid'), 'utf8').trim());
  assert(Number.isInteger(pidLoop) && pidLoop > 0, 'the loop pidfile holds a pid');
  assert(Number.isInteger(pidSample) && pidSample > 0, 'the sampler pidfile holds a pid');

  // Starting again must NOT stack a second sampler. Two samplers on one socket
  // would double every figure in soak.jsonl — a silent corruption that looks
  // like double the market activity.
  r = await sh(['start', '--interval-sec', '3600', '--sample-sec', '3600']);
  assert(r.out.includes('already running: health loop'), 'a second start refuses the loop');
  assert(r.out.includes('already running: sampler'), 'a second start refuses the sampler');
  assert(!r.out.includes('started: sampler'), 'a second start starts no second sampler');
  const pidSample2 = Number(readFileSync(join(SOAK_DIR, 'resident-sampler.pid'), 'utf8').trim());
  assert(pidSample2 === pidSample, 'the sampler pid is unchanged by the second start');

  r = await sh(['status']);
  assert(r.out.includes('health loop: RUNNING'), 'status after start: loop RUNNING');
  assert(r.out.includes('sampler:     RUNNING'), 'status after start: sampler RUNNING');
  assert(/newest sample: \d+s ago/.test(r.out), 'status reports a fresh sample age, not "none"');

  r = await sh(['stop']);
  assert(r.code === 0, `stop exits 0 (got ${r.code})`);
  assert(r.out.includes('stopped: health loop'), 'stop reports stopping the loop');
  assert(r.out.includes('stopped: sampler'), 'stop reports stopping the sampler');
  assert(!r.out.includes('did not exit'), 'both processes exited within the grace period');
  assert(!existsSync(join(SOAK_DIR, 'resident-loop.pid')), 'stop removes the loop pidfile');
  assert(!existsSync(join(SOAK_DIR, 'resident-sampler.pid')), 'stop removes the sampler pidfile');

  // Anti-vacuous: actually verify the pids are gone, not just that stop said so.
  const gone = (pid) => { try { process.kill(pid, 0); return false; } catch { return true; } };
  assert(gone(pidLoop), `loop pid ${pidLoop} is really gone`);
  assert(gone(pidSample), `sampler pid ${pidSample} is really gone`);

  r = await sh(['status']);
  assert(r.out.includes('health loop: not running'), 'status after stop: loop not running');
  assert(r.out.includes('sampler:     not running'), 'status after stop: sampler not running');
}

// ── teardown ────────────────────────────────────────────────────────────────
clearTimeout(watchdog);
for (const srv of [...liveServers]) {
  srv.closeAllConnections?.();
  await Promise.race([
    new Promise((res) => srv.close(() => res())),
    new Promise((res) => setTimeout(res, 2000)),
  ]);
  liveServers.delete(srv);
}
rmSync(WORK, { recursive: true, force: true });

console.log('─'.repeat(72));
if (failures > 0) {
  console.error(`RESULT: FAIL — ${failures} assertion(s) failed`);
  process.exit(1);
}
console.log('RESULT: PASS — the sampler\'s two lifetimes are distinguishable in');
console.log('        both directions: bounded runs complete, resident runs stop on');
console.log('        request and say so, and a mistyped horizon cannot silently');
console.log('        become a monitor that samples nothing.');
console.log('─'.repeat(72));
