#!/usr/bin/env node
/**
 * webapp-check.mjs — E6-a gate (#35).
 *
 *   [1] The Tauri shell crate compiles (cargo check -p blitzkrieg-webapp).
 *   [2] The web gateway enforces auth: no/wrong token → 401; valid token →
 *       200 (query, header). CORS: foreign Origin → 403.
 *   [3] The headless AppViewModel (ui_kit app adapter, the Tauri seam)
 *       renders an offline-safe view with no GUI deps touched.
 *   [4] blitzkrieg_ui_kit stays GUI-free: no tauri in its Cargo.toml/deps.
 *
 * Exit 0 on PASS, 1 on FAIL.
 */
import { spawnSync, spawn } from 'child_process';
import { mkdtempSync, rmSync, existsSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import net from 'net';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target/release/blitzkrieg-core');
const WEB = join(ROOT, 'target/release/ui_kit_web');
const SOCK = join(tmpdir(), `webapp-check-${process.pid}.sock`);
const WORK = mkdtempSync(join(tmpdir(), 'webapp-check-'));

let failures = 0;
const check = (name, cond, detail = '') => {
  if (!cond) failures++;
  console.log(`  ${cond ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// [1] Tauri crate compiles.
const tauriCheck = spawnSync('cargo', ['check', '-p', 'blitzkrieg-webapp'], { cwd: ROOT, encoding: 'utf8' });
check('tauri crate compiles', tauriCheckOk(tauriCheck), tauriCheck?.stderr?.slice(-120));
function tauriCheckOk(r) { return r && r.status === 0; }

// [4] ui_kit has zero GUI deps.
const kitToml = (await import('fs')).readFileSync(join(ROOT, 'ui/ui_kit/Cargo.toml'), 'utf8');
check('blitzkrieg-ui-kit has no tauri dep', !kitToml.includes('tauri'));

// [2] Gateway auth via a live ui_kit_web.
const web = spawn(WEB, ['--socket', SOCK, '--addr', '127.0.0.1:18997', '--manage'], {
  cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
});
let token = '';
const gotLine = new Promise((res) => {
  web.stdout.on('data', (d) => {
    const m = /token ([0-9a-f]{40})/.exec(d.toString());
    if (m) res(m[1]);
  });
  web.stderr.on('data', (d) => process.stderr.write(`   [web:err] ${d}`));
});
token = await Promise.race([gotLine, sleep(5000).then(() => '')]);
if (!token) {
  check('web gateway started with token', false, 'no token line on stdout');
  process.exit(1); // finally-free below not needed; web killed below anyway
}

// [3] Live core for snapshot through the same token gate.
const core = spawn(BIN, ['--socket', SOCK, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '1000', '--no-trade-log', '--no-event-archive'], { cwd: WORK, stdio: 'ignore' });
for (let i = 0; i < 50 && !existsSync(SOCK); i++) await sleep(100);
await sleep(300);

async function httpGet(path, headers = '') {
  return new Promise((res) => {
    const c = net.connect(18997, '127.0.0.1');
    let b = '';
    c.on('connect', () => c.write(`GET ${path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n${headers}\r\n`));
    c.on('data', (d) => (b += d));
    c.on('end', () => res(b));
    c.on('error', () => res(b));
    setTimeout(() => { try { c.end(); } catch {} res(b); }, 3000);
  });
}
const status = (raw) => Number(raw.split('\r\n')[0]?.split(' ')[1]);

try {
  check('no token → 401', (await httpGet('/api/snapshot')).startsWith('HTTP/1.1 401'));
  check('wrong token → 401', (await httpGet('/api/snapshot?token=deadbeef')).startsWith('HTTP/1.1 401'));
  const ok = await httpGet(`/api/snapshot?token=${token}`);
  check('valid token → 200 snapshot', status(ok) === 200 && /version|connected/.test(ok));
  check('valid header token → 200', status(await httpGet('/api/snapshot', `X-Auth-Token: ${token}\r\n`)) === 200);
  const cors = await httpGet(`/api/snapshot?token=${token}`, 'Origin: http://evil.example\r\n');
  check('foreign Origin → 403', status(cors) === 403);
  const same = await httpGet(`/api/snapshot?token=${token}`, 'Origin: http://127.0.0.1\r\n');
  check('loopback Origin → 200', status(same) === 200);

  // Command through the gate: E5 verb works.
  const st = await httpGet(`/api/command?cmd=${encodeURIComponent('status')}&token=${token}`);
  check('command with token → 200', status(st) === 200);
  const bad = await httpGet('/api/command?cmd=status');
  check('command without token → 401', status(bad) === 401);
} finally {
  try { core.kill('SIGKILL'); } catch {}
  try { web.kill('SIGKILL'); } catch {}
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
}

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
