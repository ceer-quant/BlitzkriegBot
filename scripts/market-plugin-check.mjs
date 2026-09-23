#!/usr/bin/env node
/** Verify --market-plugin selection + market.list active flag. */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { requestOnce as rpc } from './lib/core-client.mjs';
import { waitForSocket } from './lib/wait.mjs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function run(args) {
  const sock = join(tmpdir(), `mp-${Math.random().toString(36).slice(2)}.sock`);
  try { unlinkSync(sock); } catch {}
  const p = spawn(BIN, ['--socket', sock, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '100', '--no-trade-log', ...args], { stdio: 'ignore', cwd: mkdtempSync(join(tmpdir(), 'mp-')) });
  await waitForSocket(sock, { timeoutMs: 3000 });
  await sleep(300);
  const ml = await rpc(sock, 'market.list');
  p.kill('SIGTERM'); await sleep(200);
  return ml;
}

const a = await run([]);
const b = await run(['--market-plugin', 'polymarket']);
const c = await run(['--market-plugin', 'nonexistent']);
const j = (x) => JSON.stringify(x);
const okA = a?.active === 'polymarket';
const okB = b?.active === 'polymarket' && b.plugins[0].active === true;
const okC = c?.active === 'polymarket'; // unknown name falls back to first
console.log('default        :', j(a));
console.log('--market-plugin polymarket :', j(b));
console.log('--market-plugin bogus      :', j(c));
console.log(`\nRESULT: ${okA && okB && okC ? 'PASS' : 'FAIL'}`);
process.exit(okA && okB && okC ? 0 : 1);
