#!/usr/bin/env node
/**
 * Gateway signal shutdown gate: SIGTERM/SIGINT on a managed web gateway must
 * stop the core it owns, rather than orphaning it.
 */
import { spawn, spawnSync } from './lib/child-guard.mjs';
import { existsSync, mkdtempSync, rmSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const ROOT = process.cwd();
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const WEB = join(ROOT, 'target', 'release', 'ui_kit_web');
const PORT = Number(process.env.GATEWAY_SIGNAL_PORT ?? 18997);
const USER = 'gate-signal-admin';
const PASSWORD = 'gate-signal-pass-4b71';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const WORK = mkdtempSync(join(tmpdir(), 'gateway-signal-'));
const SOCK = join(WORK, 'core.sock');
const claims = [];
const spawned = new Set();

function check(name, ok, detail = '') {
  claims.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
}
function commandOf(pid) {
  const out = spawnSync('ps', ['-o', 'command=', '-p', String(pid)], { encoding: 'utf8' });
  return (out.stdout ?? '').trim();
}
function isOurs(pid) {
  return pid > 0 && commandOf(pid).includes(SOCK);
}
function status(raw) { return Number(raw.split('\r\n')[0]?.split(' ')[1]); }
function bodyOf(raw) { const i = raw.indexOf('\r\n\r\n'); return i < 0 ? '' : raw.slice(i + 4); }
function json(raw) { try { return JSON.parse(bodyOf(raw)); } catch { return null; } }
function httpReq(method, path, { body = '', timeoutMs = 20000 } = {}) {
  return new Promise((resolve) => {
    const socket = net.connect(PORT, '127.0.0.1');
    let raw = ''; let done = false;
    const finish = () => { if (done) return; done = true; try { socket.end(); } catch {} resolve(raw); };
    socket.on('connect', () => socket.write(`${method} ${path} HTTP/1.1\r\nHost: t\r\nContent-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`));
    socket.on('data', (chunk) => { raw += chunk.toString(); });
    socket.on('end', finish); socket.on('error', finish); setTimeout(finish, timeoutMs);
  });
}
async function waitFor(fn, timeoutMs, label) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) { if (await fn()) return true; await sleep(250); }
  throw new Error(`timed out waiting for ${label}`);
}

for (const [label, path] of [['core', CORE], ['gateway', WEB]]) {
  if (!existsSync(path)) { console.error(`${label} binary missing: ${path}`); process.exit(2); }
}
let web = null; let corePid = 0; let token = '';
try {
  web = spawn(WEB, ['--socket', SOCK, '--addr', `127.0.0.1:${PORT}`, '--manage'], {
    cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, BLITZKRIEG_PANEL_USER: USER, BLITZKRIEG_PANEL_PASSWORD: PASSWORD, UIKIT_CORE_BIN: CORE, UIKIT_CORE_CWD: WORK, UIKIT_CORE_EXTRA_ARGS: '--no-discovery' },
  });
  spawned.add(web);
  let ready = '';
  for await (const chunk of web.stdout[Symbol.asyncIterator]()) { ready += chunk.toString(); if (/listening on/.test(ready)) break; }
  check('gateway starts with lifecycle enabled', /lifecycle ENABLED/.test(ready), ready.slice(-120));
  const login = await httpReq('POST', '/api/login', { body: JSON.stringify({ user: USER, password: PASSWORD }) });
  token = /"token":"([0-9a-f]{40})"/.exec(login)?.[1] ?? '';
  check('gateway login succeeds', status(login) === 200 && token.length === 40, `status=${status(login)}`);
  const started = await httpReq('POST', `/api/command?token=${token}`, { body: 'start' });
  check('core starts through the gateway', status(started) === 200 && json(started)?.ok === true, bodyOf(started).slice(0, 120));
  await waitFor(async () => {
    const snap = json(await httpReq('GET', `/api/snapshot?token=${token}`));
    corePid = snap?.gateway?.corePid ?? 0;
    return snap?.connected === true && snap?.gateway?.managed === true && isOurs(corePid);
  }, 25000, 'owned core');
  check('gateway owns a core identified by the private socket', corePid > 0 && isOurs(corePid), `pid=${corePid}`);
  const gatewayPid = web.pid;
  check('gateway pid is distinct from core pid', gatewayPid > 0 && gatewayPid !== corePid, `gateway=${gatewayPid} core=${corePid}`);
  process.kill(gatewayPid, 'SIGTERM');
  await waitFor(() => web.exitCode !== null || web.signalCode !== null, 10000, 'gateway exit');
  check('gateway exits after SIGTERM', web.exitCode === 0, `exit=${web.exitCode} signal=${web.signalCode}`);
  await waitFor(() => !isOurs(corePid), 10000, 'owned core shutdown');
  check('SIGTERM does not orphan the managed core', !isOurs(corePid), `ps=${commandOf(corePid) || '(gone)'}`);
} catch (error) {
  check('gate completes without an unexpected error', false, String(error?.message ?? error));
} finally {
  for (const p of spawned) { try { p.kill('SIGKILL'); } catch {} }
  if (isOurs(corePid)) { try { process.kill(corePid, 'SIGKILL'); } catch {} }
  await sleep(300);
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
}
const failed = claims.filter((c) => !c.ok);
console.log(failed.length ? `RESULT: FAIL — ${failed.length}/${claims.length} claims failed` : `RESULT: PASS — ${claims.length}/${claims.length} claims`);
if (failed.length) process.exit(1);
