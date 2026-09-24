#!/usr/bin/env node
/**
 * E12(c) end-to-end gate — "内核崩溃后 UI 能恢复/报告" measured **through the
 * gateway**, not through a hand-rolled spawn (#94).
 *
 * Why a second gate exists when `crash-recovery-check.mjs` already passes:
 * that gate drives the crash itself — it spawns a core, kills it, spawns
 * another, and checks the *socket* recovers. It therefore measures the core's
 * crash-safety (order/position persistence) and nothing about the supervisor.
 * The acceptance criterion is about the **UI**: "内核崩溃后 UI 能恢复/报告".
 * A gate that never asks the UI anything cannot earn that sentence, and until
 * now no gate did.
 *
 * What is asserted, and why each part is load-bearing:
 *
 *   1. The gateway that owns the process is the one that starts it. `--manage`
 *      is the operator saying "you answer for this core", so the crash must be
 *      reported by that same gateway and replaced by it.
 *   2. A SIGKILLed core is reported as a CRASH (`lastExit.kind`), not as a
 *      missing pid. This is the E12(c) gap: `Supervisor::reap` used to clear the
 *      child and keep no record, so the UI kept saying "running" — or, once the
 *      socket check was added, went silent about *why* the core was gone.
 *      SIGKILL is used so the core cannot run any exit path of its own; without
 *      a classification by intent, the crash would be indistinguishable from a
 *      stop we asked for.
 *   3. The replacement is a DIFFERENT pid that actually serves: a snapshot
 *      reports `connected` again and a real `status` round-trips. "A pid
 *      exists" and "the app works" are different claims.
 *   4. The crash is still on the wire AFTER the restart succeeded, with a
 *      restart count. A core that silently came back is a core that crashed —
 *      dropping the notice there is how a flapping core looks healthy.
 *   5. A deliberate `stop` is reported as CLEAN and does NOT consume the restart
 *      budget. This is the pair to (2): if crashes and stops were classified the
 *      same way, a stop would trigger a respawn (a zombie the operator cannot
 *      kill) or a crash would be dismissed as "we stopped it".
 *
 * Process safety: this gate kills a process, so the victim is identified by its
 * command line containing this run's private socket path — never by "looks like
 * a core". A production core cannot carry a mktemp socket path, and the gate
 * refuses to signal anything it cannot identify that way.
 *
 * Run: node scripts/gateway-crash-recovery-check.mjs
 */
// Guarded spawn (`scripts/lib/child-guard.mjs`): a gate interrupted between
// booting the gateway and its own teardown must not leave the gateway and its
// owned core behind with PPID=1. That leak was real on 2026-09-19.
import { spawn, spawnSync } from './lib/child-guard.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitFor, sleep } from './lib/wait.mjs';
import { mkdtempSync, existsSync, rmSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const ROOT = process.cwd();
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const WEB = join(ROOT, 'target', 'release', 'ui_kit_web');
const PORT = Number(process.env.GATEWAY_CRASH_PORT ?? 18996);
const USER = 'gate-crash-admin';
const PASSWORD = 'gate-crash-pass-4b71';

const WORK = mkdtempSync(join(tmpdir(), 'gateway-crash-'));
const SOCK = join(WORK, 'core.sock');

const gate = createChecks();
const { check } = gate;

// --- preflight ---------------------------------------------------------------
for (const [label, path] of [['core', CORE], ['gateway', WEB]]) {
  if (!existsSync(path)) {
    console.error(
      `refusing to run: ${label} binary ${path} not found — build it with ` +
        '`cargo build --release --workspace --locked`',
    );
    process.exit(2);
  }
}

// Track every process this gate may signal, by identity.
const spawned = new Set();

async function httpReq(port, method, path, { headers = '', body = '', timeoutMs = 25000 } = {}) {
  return new Promise((res) => {
    const c = net.connect(port, '127.0.0.1');
    let b = '';
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      try {
        c.end();
      } catch {}
      res(b);
    };
    c.on('connect', () =>
      c.write(
        `${method} ${path} HTTP/1.1\r\nHost: t\r\n${headers}` +
          `Content-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`,
      ),
    );
    c.on('data', (d) => (b += d));
    c.on('end', finish);
    c.on('error', finish);
    setTimeout(finish, timeoutMs);
  });
}

const status = (raw) => Number(raw.split('\r\n')[0]?.split(' ')[1]);
const bodyOf = (raw) => {
  const i = raw.indexOf('\r\n\r\n');
  return i < 0 ? '' : raw.slice(i + 4);
};
function parseJson(raw) {
  try {
    return JSON.parse(bodyOf(raw));
  } catch {
    return null;
  }
}

/** Command line of a pid, or '' when it is gone. */
function commandOf(pid) {
  const out = spawnSync('ps', ['-o', 'command=', '-p', String(pid)], { encoding: 'utf8' });
  return (out.stdout ?? '').trim();
}

/**
 * Refuse to signal anything whose command line does not carry this run's private
 * socket. The socket lives in a fresh mktemp dir, so no production core (nor the
 * panel process under the hard "do not kill" constraint) can match.
 */
function isOurs(pid) {
  return pid > 0 && commandOf(pid).includes(SOCK);
}

let web = null;
let token = '';
let killedPid = 0;
let replacementPid = 0;

const snap = async () =>
  parseJson(await httpReq(PORT, 'GET', `/api/snapshot?token=${token}`, { timeoutMs: 25000 }));

try {
  // --- boot the gateway with --manage --------------------------------------
  // `UIKIT_CORE_CWD` isolates the child's `data/` writes into the temp dir, and
  // `UIKIT_CORE_EXTRA_ARGS` replaces the production `--engine --feed-ws` pair
  // with `--no-discovery`, so the gate needs no market feed or credentials to
  // exercise process lifecycle. `UIKIT_CORE_BIN` pins the release binary rather
  // than letting discovery find a debug build.
  web = spawn(WEB, ['--socket', SOCK, '--addr', `127.0.0.1:${PORT}`, '--manage'], {
    cwd: WORK,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      BLITZKRIEG_PANEL_USER: USER,
      BLITZKRIEG_PANEL_PASSWORD: PASSWORD,
      UIKIT_CORE_BIN: CORE,
      UIKIT_CORE_CWD: WORK,
      UIKIT_CORE_EXTRA_ARGS: '--no-discovery',
    },
  });
  spawned.add(web);

  let ready = '';
  for await (const chunk of web.stdout[Symbol.asyncIterator]()) {
    ready += chunk.toString();
    if (/listening on/.test(ready)) break;
  }
  check('gateway started with lifecycle ENABLED', /lifecycle ENABLED/.test(ready), ready.slice(-120));

  const login = await httpReq(PORT, 'POST', '/api/login', {
    body: JSON.stringify({ user: USER, password: PASSWORD }),
  });
  token = /"token":"([0-9a-f]{40})"/.exec(login)?.[1] ?? '';
  check('logged in (a session is mandatory in gateway mode)', token.length === 40, `status=${status(login)}`);

  // --- before: nothing has exited, nothing to report ------------------------
  const before = await snap();
  check(
    'no core is running before start',
    before?.connected === false,
    `connected=${JSON.stringify(before?.connected)}`,
  );
  check(
    'a gateway that never lost a core reports no exit',
    before?.gateway?.lastExit == null && (before?.gateway?.restarts ?? 0) === 0,
    `lastExit=${JSON.stringify(before?.gateway?.lastExit)} restarts=${before?.gateway?.restarts}`,
  );

  // --- start it THROUGH the gateway, so the gateway owns it ----------------
  const started = await httpReq(PORT, 'POST', `/api/command?token=${token}`, {
    headers: 'Content-Type: text/plain\r\n',
    body: 'start',
    timeoutMs: 30000,
  });
  const startDoc = parseJson(started);
  check(
    'start via the gateway verb succeeds',
    status(started) === 200 && startDoc?.ok === true,
    `${status(started)} ${JSON.stringify(startDoc?.message ?? bodyOf(started).slice(0, 120))}`,
  );

  const owned = await waitFor(
    async () => {
      const s = await snap();
      return s?.connected === true && s?.gateway?.managed === true ? s : null;
    },
    { timeoutMs: 25000, label: 'the gateway to own a live core', retryOnError: true },
  );
  killedPid = owned.gateway.corePid ?? 0;
  check(
    'the gateway spawned and OWNS the core (not adopted)',
    owned.gateway.managed === true && killedPid > 0,
    `managed=${owned.gateway.managed} pid=${killedPid}`,
  );
  check(
    'the owned pid is this run\'s core (identified by its own socket path)',
    isOurs(killedPid),
    `ps: ${commandOf(killedPid).slice(0, 120) || '(gone)'}`,
  );

  // --- crash its own core behind the gateway's back ------------------------
  if (!isOurs(killedPid)) {
    throw new Error(`refusing to kill pid ${killedPid}: not identified as this run's core`);
  }
  process.kill(killedPid, 'SIGKILL');

  const recovered = await waitFor(
    async () => {
      const s = await snap();
      const last = s?.gateway?.lastExit;
      return s?.connected === true && (s?.gateway?.restarts ?? 0) >= 1 && last?.kind === 'crash' ? s : null;
    },
    { timeoutMs: 40000, label: 'the gateway to report the crash and replace the core', retryOnError: true },
  );

  check(
    'the UI can tell it CRASHED (kind=crash, not a missing pid)',
    recovered.gateway.lastExit.kind === 'crash',
    `kind=${recovered.gateway.lastExit.kind} description=${JSON.stringify(recovered.gateway.lastExit.description)}`,
  );
  check(
    'the report names the signal, so the cause is diagnosable',
    recovered.gateway.lastExit.signal === 9,
    `signal=${recovered.gateway.lastExit.signal} code=${recovered.gateway.lastExit.code}`,
  );
  replacementPid = recovered.gateway.corePid ?? 0;
  check(
    'the replacement is a DIFFERENT process, owned by the gateway',
    replacementPid > 0 && replacementPid !== killedPid && recovered.gateway.managed === true,
    `pid ${killedPid} → ${replacementPid}`,
  );
  check(
    'the restart count is on the wire (a repaired crash stays visible)',
    recovered.gateway.restarts >= 1 && recovered.gateway.restartGivenUp !== true,
    `restarts=${recovered.gateway.restarts} givenUp=${recovered.gateway.restartGivenUp}`,
  );

  // --- the replacement must WORK, not merely exist --------------------------
  const st = await httpReq(PORT, 'GET', `/api/command?cmd=status&token=${token}`, { timeoutMs: 20000 });
  const stDoc = parseJson(st);
  check(
    'the replaced core answers a real command',
    status(st) === 200 && stDoc?.ok === true,
    `status=${status(st)} message=${JSON.stringify(stDoc?.message ?? '').slice(0, 120)}`,
  );
  check(
    'and it serves live state (a balance comes back)',
    stDoc?.data?.balance != null || (stDoc?.data?.connection?.connected ?? false) === true,
    `data keys=${JSON.stringify(Object.keys(stDoc?.data ?? {}))}`,
  );

  // --- a stop we asked for must NOT look like a crash ----------------------
  const restartsBeforeStop = recovered.gateway.restarts;
  const stopped = await httpReq(PORT, 'POST', `/api/command?token=${token}`, {
    headers: 'Content-Type: text/plain\r\n',
    body: 'stop',
    timeoutMs: 20000,
  });
  check('stop via the gateway verb succeeds', status(stopped) === 200 && parseJson(stopped)?.ok === true, `${status(stopped)}`);

  const afterStop = await waitFor(
    async () => {
      const s = await snap();
      return s?.connected === false ? s : null;
    },
    { timeoutMs: 20000, label: 'the core to be gone after a deliberate stop', retryOnError: true },
  );
  check(
    'a deliberate stop is reported as CLEAN, not as a crash',
    afterStop.gateway.lastExit?.kind === 'clean',
    `kind=${JSON.stringify(afterStop.gateway.lastExit?.kind)} description=${JSON.stringify(afterStop.gateway.lastExit?.description)}`,
  );
  check(
    'and the gateway does not respawn a core the operator stopped',
    afterStop.connected === false && (afterStop.gateway.restarts ?? 0) === restartsBeforeStop,
    `connected=${afterStop.connected} restarts ${restartsBeforeStop} → ${afterStop.gateway.restarts}`,
  );
} catch (e) {
  check('gate completed without an unexpected error', false, String(e.message || e));
} finally {
  // Only ever signal processes this gate spawned, identified by identity.
  for (const p of spawned) {
    if (p.pid) {
      try {
        p.kill('SIGKILL');
      } catch {}
    }
  }
  // The gateway's Drop stops an owned core, but a SIGKILLed gateway gets no
  // Drop — so sweep any leftover core that carries this run's socket path.
  if (isOurs(replacementPid)) {
    try {
      process.kill(replacementPid, 'SIGKILL');
    } catch {}
  }
  await sleep(300);
  try {
    rmSync(WORK, { recursive: true, force: true });
  } catch {}
}

const failed = gate.results.filter((c) => !c.ok);
console.log('');
if (failed.length) {
  console.log(`RESULT: FAIL — ${failed.length}/${gate.results.length} claims failed`);
  process.exit(1);
}
console.log(
  `RESULT: PASS — ${gate.results.length}/${gate.results.length} claims: the gateway reports the crash, replaces the core, and keeps a stop out of the crash path`,
);
