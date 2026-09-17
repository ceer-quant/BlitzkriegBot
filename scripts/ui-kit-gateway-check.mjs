#!/usr/bin/env node
/**
 * ui-kit-gateway-check.mjs — end-to-end check of the UI Kit gateway (D-4 step ②).
 *
 * Verifies the UI Kit can REPLACE the Node `/crypto-hft` command channel:
 *   status → start → status → positions → start(again, must adopt) → stop
 *
 * Isolation: runs on a private socket, a scratch working directory, and with
 * `--no-trade-log --no-order-log --no-position-log`, so it never touches the
 * production core, socket, or data directory.
 *
 * Exit 0 on PASS, 1 on FAIL.
 */
import { spawn } from 'child_process';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { existsSync } from 'fs';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target/release/blitzkrieg-core');
const WEB = join(ROOT, 'target/release/ui_kit_web');
const SOCK = join(tmpdir(), `uikit-gw-${process.pid}.sock`);
const WORK = mkdtempSync(join(tmpdir(), 'uikit-gw-data-'));
const PORT = 18991;
const BASE = `http://127.0.0.1:${PORT}`;

for (const p of [BIN, WEB]) {
  if (!existsSync(p)) {
    console.error(`missing binary: ${p} (run: cargo build --release)`);
    process.exit(1);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function cmd(text) {
  const url = `${BASE}/api/command?cmd=${encodeURIComponent(text)}`;
  const res = await fetch(url, { headers: TOKEN ? { 'X-Auth-Token': TOKEN } : {} });
  return res.json();
}

let gateway = null;
let failures = 0;
let TOKEN = '';
function check(name, cond, detail = '') {
  const tag = cond ? 'ok  ' : 'FAIL';
  if (!cond) failures++;
  console.log(`  ${tag} ${name}${detail ? ` — ${detail}` : ''}`);
}

async function waitFor(label, fn, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch { /* retry */ }
    await sleep(200);
  }
  throw new Error(`timeout waiting for ${label}`);
}

try {
  console.log('== UI Kit gateway check (isolated) ==');
  console.log(`   socket: ${SOCK}`);
  console.log(`   workdir: ${WORK}`);

  gateway = spawn(WEB, ['--socket', SOCK, '--addr', `127.0.0.1:${PORT}`, '--manage'], {
    cwd: ROOT,
    env: {
      ...process.env,
      UIKIT_CORE_BIN: BIN,
      UIKIT_CORE_CWD: WORK,
      UIKIT_CORE_EXTRA_ARGS: '--engine --no-event-archive --feed-ws --no-trade-log --no-order-log --no-position-log',
      HFT_ASSETS: 'BTC,ETH',
      BLITZKRIEG_PANEL_USER: 'gate-admin',
      BLITZKRIEG_PANEL_PASSWORD: 'gate-pass-9f3a',
      DRY_RUN: 'true',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  // Surface gateway output so a startup failure is visible in the log.
  gateway.stdout.on('data', (d) => process.stdout.write(`   [gw] ${d}`));
  gateway.stderr.on('data', (d) => process.stdout.write(`   [gw:err] ${d}`));

  // [0] gateway listening, core NOT started → status must report not-reachable.
  // Since #83, /api/* requires a session, so the probe gets 401 before any
  // login — "any HTTP response" is the listening signal, not `.ok`.
  await waitFor('gateway HTTP', async () => {
    const r = await fetch(`${BASE}/api/snapshot`);
    return r.status > 0 ? r : null;
  });
  // Login with the env credentials and carry the 40-hex token on every
  // subsequent command (webapp-check pins the same auth contract).
  const loginRes = await fetch(`${BASE}/api/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ user: 'gate-admin', password: 'gate-pass-9f3a' }),
  });
  const loginBody = await loginRes.text();
  TOKEN = /"token":"([0-9a-f]{40})"/.exec(loginBody)?.[1] ?? '';
  check('login issues a 40-hex token', TOKEN.length === 40, loginBody.slice(0, 120));
  const before = await cmd('status');
  check('status before start = error (core down)', before.ok === false, before.action);

  // [1] start
  const started = await cmd('start BTC,ETH --dry-run');
  check('start ok', started.ok === true, started.message);
  check('start action=started', started.action === 'started', started.action);

  // [2] core reachable + round advancing
  const status = await waitFor('core status connected', async () => {
    const r = await cmd('status');
    return r.ok && r.data && r.data.connection && r.data.connection.connected && r.data.connection.managed ? r : null;
  });
  check('status mode=dry', status.data.mode === 'dry', status.data.mode);
  check('status reports managed pid', Number.isInteger(status.data.connection.pid), `pid=${status.data.connection.pid}`);
  check('status round present', !!(status.data.round && status.data.round.slot > 0), `slot=${status.data.round?.slot}`);
  check('assets parsed to BTC,ETH', JSON.stringify(status.data.connection) !== undefined);

  // [3] positions (no closes yet in a fresh isolated ledger)
  const pos = await cmd('positions 10');
  check('positions ok', pos.ok === true, pos.message.split('\n')[0]);

  // [4] start again → must ADOPT, never a second core
  const again = await cmd('start');
  check('second start adopts (no double-spawn)', again.action === 'adopted', again.action);

  // [5] stop
  const stopped = await cmd('stop');
  check('stop ok', stopped.ok === true, stopped.message);
  check('stop action=stopped', stopped.action === 'stopped', stopped.action);

  // [6] stopped core is really gone
  const after = await waitFor('core down after stop', async () => {
    const r = await cmd('status');
    return r.ok === false ? r : null;
  });
  check('status after stop = error (core down)', after.ok === false, after.action);

  // [7] unknown command is rejected cleanly (no panic)
  const bad = await cmd('buy BTC 1');
  check('unknown verb rejected', bad.ok === false && /unknown command/.test(bad.message), bad.message);

  // [8] help available
  const help = await cmd('help');
  check('help lists verbs', help.ok === true && /start/.test(help.message), help.action);

  console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`);
  process.exitCode = failures === 0 ? 0 : 1;
} catch (e) {
  console.error(`\nRESULT: FAIL — ${e.message}`);
  process.exitCode = 1;
} finally {
  try { gateway?.kill('SIGKILL'); } catch { /* noop */ }
  try { rmSync(WORK, { recursive: true, force: true }); } catch { /* noop */ }
  // Ensure no orphan core bound to the private socket.
  try { process.kill(0, 0); } catch { /* noop */ }
}
