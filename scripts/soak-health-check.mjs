#!/usr/bin/env node
/**
 * soak-health gate — proves `scripts/soak-health.sh`'s checks are able to fail.
 *
 * KI-30's finding was that the ops-inspection layer's guards could not fail:
 * the panel check probed a route that has never existed (permanently red), two
 * scans read a file the Node purge had deleted (silently skipped, forever
 * green), and "is monitoring running?" grepped for a sampler with a *bounded*
 * lifetime. An always-on alarm and a never-firing check are the same defect:
 * no signal. So this gate is built the way KI-24's was — it leads with the
 * REFUSAL branches and injects each failure, rather than only walking the happy
 * path and declaring the script good.
 *
 * What it does NOT do: touch the real deployment. Every path is redirected to a
 * throwaway fixture via the script's own env seams (`BK_*`), and the real
 * `data/`, the real core, and the real panel are never read, written, or
 * signalled. The fixture processes it spawns go through `lib/child-guard.mjs`,
 * so a crashed run cannot leave them behind (KI-29).
 *
 *   1. Static: every `./scripts/<name>` a shell script invokes actually exists.
 *   2. A healthy fixture exits 0 — the anti-false-positive direction. A gate
 *      that only checks refusals would pass a script that always says ANOMALY.
 *   3. Each guard FIRES on the failure it exists to catch:
 *        core absent / two cores / round-sec=300 / no UDS answer /
 *        panel HTML fallback (the historical trap) / panel not-ok / panel bad
 *        body / panel unreachable / sampling stale / no samples /
 *        configured-but-missing log / shipped log with a panic / no trade
 *        ledger / slow-hold force_exit / late-entry force_exit /
 *        archive stale / archive empty / not a repo root.
 *   4. The sub-statuses the loop reads from `--quiet` output are all present,
 *      since a status that only appears in a non-quiet `note` is invisible in
 *      `data/soak/health.log` — the file that is actually read.
 *
 * The gate also has to EXIT. Its first version verified all of the above, printed
 * `RESULT: PASS`, and then never terminated: a fixture server opened by a later
 * control was never closed, so a listener held the event loop open forever. No
 * step in CI was bounded, so the run sat there for ~40 minutes looking like a
 * slow gate. Every fixture server is now owned by a registry and closed at
 * teardown, and `BK_GATE_WATCHDOG_MS` (default 5 min) converts any future wedge
 * into a fast failure that names the handles still listening.
 *
 * Run: node scripts/soak-health-check.mjs
 *      BK_GATE_WATCHDOG_MS=2500 node scripts/soak-health-check.mjs   # prove it fires
 */
import { spawn } from './lib/child-guard.mjs';
import { execFileSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  readdirSync,
  existsSync,
  rmSync,
  utimesSync,
} from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import http from 'node:http';
import net from 'node:net';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, '..');
const HEALTH = join(ROOT, 'scripts', 'soak-health.sh');

const WORK = mkdtempSync(join(tmpdir(), 'soak-health-check-'));
const noop = () => {};

/**
 * Every fixture server this gate opens, so teardown closes them from a registry
 * instead of from hand-held variables. The hand-held version was wrong: the
 * wedged-core control replaces the live socket server with a new one, and the
 * replacement's handle was never kept, so its listener kept the event loop alive
 * after the final `RESULT: PASS` had already been printed. The gate verified
 * everything correctly and then never exited — CI sat on the step for 40 minutes.
 */
const liveServers = new Set();

let failures = 0;

/**
 * A gate that can hang is a gate that cannot fail on time. Nothing bounded the
 * CI step, so the hang above cost 40 minutes of wall clock and looked like a
 * slow gate rather than a broken one. The watchdog turns any future hang — the
 * usual cause being a dropped fixture-server handle — into a fast, named
 * failure, and it names the handles so the cause needs no re-diagnosis.
 */
const WATCHDOG_MS = Number(process.env.BK_GATE_WATCHDOG_MS ?? 300000);
const watchdog = setTimeout(() => {
  console.error(`  FAIL gate did not exit within ${WATCHDOG_MS / 1000}s — a fixture server is still listening:`);
  for (const srv of liveServers) {
    console.error(`         ${srv.constructor?.name} ${JSON.stringify(srv.address?.())}`);
  }
  if (liveServers.size === 0) console.error('         (no tracked server — some other handle is held)');
  console.error('RESULT: FAIL — gate wedged (the assertions above may all have passed)');
  process.exit(1);
}, WATCHDOG_MS);
watchdog.unref?.();

function ok(msg) {
  console.log(`  ok   ${msg}`);
}
function bad(msg) {
  console.error(`  FAIL ${msg}`);
  failures++;
}
function assert(cond, msg) {
  cond ? ok(msg) : bad(msg);
}

// ── fixture layout ──────────────────────────────────────────────────────────
const FIX = join(WORK, 'fix');
const SOAK_DIR = join(FIX, 'soak');
const ARCH_DIR = join(FIX, 'archive');
const TRADES = join(FIX, 'trades', 'trades.jsonl');
const LOG_OK = join(FIX, 'clean.log');
const LOG_PANIC = join(FIX, 'panicked.log');
const LOG_MISSING = join(FIX, 'never-written.log');
const SOCK_OK = join(FIX, 'core.sock');

mkdirSync(SOAK_DIR, { recursive: true });
mkdirSync(ARCH_DIR, { recursive: true });
mkdirSync(dirname(TRADES), { recursive: true });
writeFileSync(LOG_OK, 'INFO engine up\nINFO round 1\n');
writeFileSync(LOG_PANIC, 'INFO engine up\nthread panicked at src/ome.rs:1\n');

const nowMs = () => Date.now();
function writeSoak(ageSec) {
  writeFileSync(
    join(SOAK_DIR, 'soak.jsonl'),
    JSON.stringify({ cycle: 1, ts: nowMs() - ageSec * 1000, pingOk: true }) + '\n',
  );
}
function writeArchive(ageSec) {
  const p = join(ARCH_DIR, '20260101T000000Z.jsonl');
  writeFileSync(p, '{"event":"book"}\n');
  // utimesSync, not `touch -t`: BSD touch rejects the 14-digit CCYYMMDDhhmmss
  // form that GNU accepts, and this gate has to run on both (macOS locally,
  // Ubuntu on CI).
  const t = nowMs() - ageSec * 1000;
  utimesSync(p, t / 1000, t / 1000);
}
function writeTrades(rows) {
  writeFileSync(TRADES, rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
}
/** A round-clock entryTime whose remaining time is `leftSec`. */
const entryWithLeft = (leftSec) => 900 * 1000 * 1000 + (900 - leftSec) * 1000;

// ── fixture servers ─────────────────────────────────────────────────────────
/** A unix-socket JSON-RPC server that answers core.ping; `answer:false` wedges. */
function startCoreSock(path, { answer = true } = {}) {
  return new Promise((res) => {
    try {
      rmSync(path, { force: true });
    } catch (e) {
      noop(e);
    }
    const srv = net.createServer((sock) => {
      if (!answer) return; // accept, never reply — the "alive but wedged" core
      let buf = '';
      sock.on('data', (d) => {
        buf += d.toString();
        let i;
        while ((i = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          if (!line.trim()) continue;
          let msg = {};
          try {
            msg = JSON.parse(line);
          } catch (e) {
            noop(e);
          }
          if (msg.id != null) {
            sock.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: { pong: true, ts: Date.now() } }) + '\n');
          }
        }
      });
      sock.on('error', noop);
    });
    srv.on('close', () => liveServers.delete(srv));
    srv.listen(path, () => {
      liveServers.add(srv);
      res(srv);
    });
  });
}

/** A panel fixture. `kind` selects the exact shapes the script must tell apart. */
function startPanel(kind) {
  return new Promise((res) => {
    const srv = http.createServer((req, r) => {
      if (req.url !== '/api/ping') {
        r.writeHead(404);
        return r.end('nope');
      }
      if (kind === 'html') {
        // The historical trap: the panel's catch-all returns 200 + the HTML
        // bundle for ANY unknown path, so a substring probe never matches.
        r.writeHead(200, { 'content-type': 'text/html' });
        return r.end('<!doctype html><html><body>Blitzkrieg UI Kit</body></html>');
      }
      if (kind === 'notok') {
        r.writeHead(200, { 'content-type': 'application/json' });
        return r.end(JSON.stringify({ authRequired: true, ok: false, service: 'blitzkrieg-panel' }));
      }
      if (kind === 'badbody') {
        r.writeHead(200, { 'content-type': 'application/json' });
        return r.end(JSON.stringify({ hello: 'world' }));
      }
      r.writeHead(200, { 'content-type': 'application/json' });
      r.end(JSON.stringify({ authRequired: true, ok: true, service: 'blitzkrieg-panel' }));
    });
    srv.on('close', () => liveServers.delete(srv));
    srv.listen(0, '127.0.0.1', () => {
      liveServers.add(srv);
      res(srv);
    });
  });
}
const panelUrl = (srv) => `http://127.0.0.1:${srv.address().port}`;

/** A process whose argv carries the flags the script reads from `ps`. */
function startFakeCore({ token, roundSec = '900', socket = '' }) {
  const code = 'setTimeout(() => {}, 600000);';
  const args = ['-e', code, token, '--round-sec', roundSec];
  if (socket) args.push('--socket', socket);
  return spawn(process.execPath, args, { stdio: 'ignore' });
}

// ── run the script under a given fixture environment ────────────────────────
const BASE_ENV = {
  BK_CORE_PGREP: 'bk-soak-health-nonexistent-token',
  BK_SOCKET: SOCK_OK,
  BK_SOAK_DIR: SOAK_DIR,
  BK_SOAK_STALE_SEC: '1800',
  BK_RUN_LOG: LOG_OK,
  BK_ARCH_DIR: ARCH_DIR,
  BK_TRADES: TRADES,
};

/**
 * Invoke the script and collect its output. Async on purpose: the fixture
 * servers live in THIS process, and `execFileSync` would block the event loop
 * so they could never accept a connection — the script would then see an
 * unreachable panel and a dead socket, and the gate would "verify" nothing.
 */
function runScript(args, { env, cwd, timeoutMs = 60000 } = {}) {
  return new Promise((res) => {
    const child = spawn('bash', args, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    child.stdout.on('data', (d) => {
      out += d.toString();
    });
    child.stderr.on('data', (d) => {
      out += d.toString();
    });
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.on('close', (code) => {
      clearTimeout(timer);
      res({ code: code ?? 1, out });
    });
    child.on('error', (e) => {
      clearTimeout(timer);
      res({ code: 1, out: out + String(e) });
    });
  });
}

function runHealth(overrides = {}) {
  const env = { ...process.env, ...BASE_ENV, ...overrides };
  for (const [k, v] of Object.entries(env)) if (v === undefined) delete env[k];
  return runScript([HEALTH, '--quiet'], { cwd: ROOT, env });
}
const has = (r, s) => r.out.includes(s);

// ── 1. static: invoked scripts exist ────────────────────────────────────────
console.log('1. static — every ./scripts/<name> invoked by a shell script exists');
{
  const shellScripts = readdirSync(join(ROOT, 'scripts')).filter((f) => f.endsWith('.sh'));
  let refs = 0;
  let missing = [];
  for (const f of shellScripts) {
    const lines = readFileSync(join(ROOT, 'scripts', f), 'utf8').split('\n');
    for (const line of lines) {
      // Comment lines are prose, not invocations: this very fix's explanation
      // names the script it removed, and matching that would be a false alarm.
      if (line.trimStart().startsWith('#')) continue;
      for (const m of line.matchAll(/(?:\.\/)?scripts\/([A-Za-z0-9._-]+\.(?:sh|mjs))/g)) {
        if (m[1] === f) continue; // a script naming itself (usage/help) is not a call
        refs++;
        if (!existsSync(join(ROOT, 'scripts', m[1]))) missing.push(`${f} → ${m[1]}`);
      }
    }
  }
  assert(shellScripts.length >= 3, `scanned ${shellScripts.length} shell script(s) (a rename cannot empty the scan)`);
  assert(refs >= 1, `found ${refs} cross-script reference(s) to check (anti-vacuous)`);
  assert(missing.length === 0, `all referenced scripts exist${missing.length ? `: ${missing.join(', ')}` : ''}`);
}

// ── 2. healthy fixture must exit 0 (anti-false-positive) ────────────────────
console.log('2. anti-false-positive — a healthy fixture exits 0');
const coreSock = await startCoreSock(SOCK_OK);
const panel = await startPanel('ok');
const healthEnv = {
  BK_CORE_PGREP: 'bk-soak-fixture-core',
  BK_PANEL_URL: panelUrl(panel),
  BK_SOCKET: SOCK_OK,
};
let core1 = null;
{
  core1 = startFakeCore({ token: 'bk-soak-fixture-core', socket: SOCK_OK });
  await new Promise((r) => setTimeout(r, 400));
  writeSoak(5);
  writeArchive(5);
  writeTrades([]);
  const r = await runHealth(healthEnv);
  assert(r.code === 0, `healthy fixture exits 0${r.code !== 0 ? ` (got ${r.code})` : ''}`);
  assert(has(r, 'panel=ok'), 'healthy fixture reports panel=ok');
  assert(has(r, 'core-ping=ok'), 'healthy fixture reports core-ping=ok');
  assert(has(r, 'soak=ok('), 'healthy fixture reports a fresh soak');
  assert(has(r, 'log=ok'), 'healthy fixture reports log=ok');
  assert(has(r, 'archive=ok('), 'healthy fixture reports a fresh archive');
  assert(!has(r, 'ANOMALY'), 'healthy fixture does not report ANOMALY');
}

// ── 3. each guard fires on the failure it exists to catch ───────────────────
console.log('3. negative controls — each guard fires');
{
  // core absent
  let r = await runHealth({ ...healthEnv, BK_CORE_PGREP: 'bk-soak-no-such-process' });
  assert(r.code !== 0 && has(r, 'core not running'), 'core absent → ANOMALY');
  assert(has(r, 'core-ping=skip'), 'core absent → core ping is skipped, not failed');

  // round-sec regression (300 = 5m rounds). Its own pgrep token, so the healthy
  // core above cannot shadow it — the script reads round-sec from the FIRST
  // matching pid, and two matches would make this check untestable.
  const core300 = startFakeCore({ token: 'bk-soak-fixture-round300', roundSec: '300', socket: SOCK_OK });
  await new Promise((res) => setTimeout(res, 400));
  r = await runHealth({ ...healthEnv, BK_CORE_PGREP: 'bk-soak-fixture-round300' });
  assert(has(r, 'round-sec=300'), 'round-sec=300 → ANOMALY (5m regression)');
  assert(has(r, 'round=300'), 'round-sec=300 is named in the summary');
  core300.kill('SIGKILL');
  await new Promise((res) => setTimeout(res, 200));

  // two cores
  const coreDup = startFakeCore({ token: 'bk-soak-fixture-core', roundSec: '900', socket: SOCK_OK });
  await new Promise((res) => setTimeout(res, 400));
  r = await runHealth(healthEnv);
  assert(has(r, 'core instances (expected 1)'), 'two cores → ANOMALY');
  coreDup.kill('SIGKILL');
  await new Promise((res) => setTimeout(res, 200));
}
// core alive but wedged: socket accepts, never answers
{
  rmSync(SOCK_OK, { force: true });
  coreSock.close();
  const wedged = await startCoreSock(SOCK_OK, { answer: false });
  writeSoak(5);
  const r = await runHealth(healthEnv);
  assert(has(r, 'core not answering core.ping'), 'core alive but wedged → ANOMALY (process check alone cannot catch this)');
  assert(has(r, 'core-ping=fail'), 'wedged core reports core-ping=fail in the summary');
  wedged.close();
  rmSync(SOCK_OK, { force: true });
  // The answering core comes back for the checks below. Its handle is
  // deliberately not held in a variable: `liveServers` owns every fixture server,
  // so teardown cannot miss this replacement the way the old hand-held list did.
  await startCoreSock(SOCK_OK);
}
{
  // panel served its HTML fallback for /api/ping — the exact historical defect
  const p = await startPanel('html');
  let r = await runHealth({ ...healthEnv, BK_PANEL_URL: panelUrl(p) });
  assert(has(r, 'HTML fallback'), 'panel HTML fallback → ANOMALY (the KI-30 trap)');
  assert(has(r, 'panel=html-fallback'), 'panel HTML fallback is named in the summary');
  p.close();

  const pn = await startPanel('notok');
  r = await runHealth({ ...healthEnv, BK_PANEL_URL: panelUrl(pn) });
  assert(has(r, 'reports not ok'), 'panel ok:false → ANOMALY');
  pn.close();

  const pb = await startPanel('badbody');
  r = await runHealth({ ...healthEnv, BK_PANEL_URL: panelUrl(pb) });
  assert(has(r, 'did not return'), 'panel returns non-panel JSON → ANOMALY');
  pb.close();

  // unreachable panel (nothing listening on a closed port)
  const dead = await startPanel('ok');
  const deadUrl = panelUrl(dead);
  await new Promise((res) => dead.close(res));
  r = await runHealth({ ...healthEnv, BK_PANEL_URL: deadUrl });
  assert(has(r, 'did not return the panel'), 'panel unreachable → ANOMALY');
}
{
  // sampling stale / absent — the judge is the newest sample, not a process name
  writeSoak(99999);
  let r = await runHealth(healthEnv);
  assert(has(r, 'soak sampling stale'), 'stale sampling → ANOMALY');
  assert(has(r, 'soak=stale('), 'stale sampling is named in the summary');

  writeSoak(5);
  rmSync(join(SOAK_DIR, 'soak.jsonl'), { force: true });
  r = await runHealth(healthEnv);
  assert(has(r, 'no soak samples'), 'no samples → ANOMALY');
  assert(has(r, 'soak=none'), 'no samples is named in the summary');
  writeSoak(5);
}
{
  // log: unset is reported, configured-but-missing is an anomaly, panic is caught
  let r = await runHealth({ ...healthEnv, BK_RUN_LOG: undefined });
  assert(has(r, 'log=off'), 'unset log is reported as log=off, not silently skipped');
  assert(!has(r, 'log=' + 'missing'), 'unset log is not reported as missing');

  r = await runHealth({ ...healthEnv, BK_RUN_LOG: LOG_MISSING });
  assert(has(r, 'does not exist'), 'configured-but-missing log → ANOMALY');
  assert(has(r, 'log=missing'), 'missing log is named in the summary');

  r = await runHealth({ ...healthEnv, BK_RUN_LOG: LOG_PANIC });
  assert(has(r, 'crash/orphan hits'), 'a panic in the log → ANOMALY');
}
{
  // trade ledger
  rmSync(TRADES, { force: true });
  let r = await runHealth(healthEnv);
  assert(has(r, 'no trade ledger'), 'no trade ledger → ANOMALY');

  // zero-hold force_exit WITH time left = the seconds-flatten bug
  writeTrades([{ holdTimeSec: 0, exitReason: 'force_exit', entryTime: entryWithLeft(600) }]);
  r = await runHealth(healthEnv);
  assert(has(r, 'seconds-flatten bug'), 'zero-hold force_exit with time left → the bug is named');

  // zero-hold force_exit inside the force-exit window = legal (timing exemption)
  writeTrades([{ holdTimeSec: 0, exitReason: 'force_exit', entryTime: entryWithLeft(30) }]);
  r = await runHealth(healthEnv);
  assert(has(r, 'force-exit window'), 'late entry is reported as a late entry');
  assert(!has(r, 'seconds-flatten bug'), 'late entry is NOT reported as the seconds-flatten bug');

  // both at once: the two causes must be distinguishable in one run
  writeTrades([
    { holdTimeSec: 0, exitReason: 'force_exit', entryTime: entryWithLeft(600) },
    { holdTimeSec: 0, exitReason: 'force_exit', entryTime: entryWithLeft(30) },
  ]);
  r = await runHealth(healthEnv);
  assert(has(r, '= 1 with time left'), 'the flattened count is 1 of 2 (per-cause counts, not a lump sum)');
  assert(has(r, '1 entry(ies) opened inside the force-exit window'), 'the late count is 1 of 2');

  writeTrades([]);
}
{
  // archive
  writeArchive(99999);
  let r = await runHealth(healthEnv);
  assert(has(r, 'archive stale'), 'stale archive → ANOMALY');
  assert(has(r, 'archive=stale('), 'stale archive is named in the summary');

  rmSync(join(ARCH_DIR, '20260101T000000Z.jsonl'), { force: true });
  r = await runHealth(healthEnv);
  assert(has(r, 'holds no segments'), 'archive dir with no segments → ANOMALY');

  writeArchive(5);
}
{
  // not a repo root: the script must refuse rather than report health
  const empty = join(WORK, 'not-a-repo');
  mkdirSync(join(empty, 'scripts'), { recursive: true });
  const copy = join(empty, 'scripts', 'soak-health.sh');
  writeFileSync(copy, readFileSync(HEALTH));
  let code = 0;
  let out = '';
  const rr = await runScript([copy, '--quiet'], { cwd: empty, timeoutMs: 30000 });
  code = rr.code;
  out = rr.out;
  assert(code === 2, `not a repo root → exit 2 (got ${code})`);
  assert(out.includes('not in BlitzkriegBot root'), 'not a repo root names the reason');
}

// ── 4. quiet output carries every sub-status the loop reads ─────────────────
console.log('4. the loop reads only --quiet output, so every sub-status must be in it');
{
  writeSoak(5);
  writeArchive(5);
  writeTrades([]);
  const r = await runHealth(healthEnv);
  for (const field of ['core=', 'round=', 'panel=', 'core-ping=', 'soak=', 'trades=', 'archive=', 'log=']) {
    assert(has(r, field), `--quiet summary carries ${field}`);
  }
}

// ── teardown ────────────────────────────────────────────────────────────────
// Close every fixture server the registry knows about, including any opened by a
// later control that replaced an earlier one on the same socket path. Closing
// only the first handle is what let this gate hang after printing its PASS.
//
// `close()` alone is not enough: it waits for existing connections to end, and
// the wedged-core fixture exists precisely to accept a connection and never
// answer, so its server would never emit 'close'. Force-drop the connections,
// then bound the wait so a fixture that still refuses to die is a fast, named
// failure instead of a CI step that eats an hour.
clearTimeout(watchdog);
for (const srv of [...liveServers]) {
  srv.closeAllConnections?.();
  await Promise.race([
    new Promise((res) => srv.close(() => res())),
    new Promise((res) => setTimeout(res, 2000)),
  ]);
  liveServers.delete(srv);
}
if (core1 && core1.exitCode === null) core1.kill('SIGKILL');
rmSync(WORK, { recursive: true, force: true });

console.log('─'.repeat(72));
if (failures > 0) {
  console.error(`RESULT: FAIL — ${failures} assertion(s) failed`);
  process.exit(1);
}
console.log('RESULT: PASS — soak-health separates healthy from broken in both');
console.log('        directions: a healthy fixture exits 0, and every guard fires');
console.log('        on the failure it exists to catch.');
console.log('─'.repeat(72));
