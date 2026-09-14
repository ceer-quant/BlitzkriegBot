#!/usr/bin/env node
/** Verify --market-plugin selection + market.list active flag. */
import { spawn } from 'child_process';
import { mkdtempSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import net from 'net';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rpc = (sock, method) => new Promise((res) => {
  const c = net.connect(sock); let b = '';
  c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method }) + '\n'));
  c.on('data', (d) => { b += d; const i = b.indexOf('\n'); if (i < 0) return; try { res(JSON.parse(b.slice(0, i)).result); } catch { res(null); } c.end(); });
  c.on('error', () => res(null)); setTimeout(() => { try { c.end(); } catch {} res(null); }, 3000);
});

async function run(args) {
  const sock = join(tmpdir(), `mp-${Math.random().toString(36).slice(2)}.sock`);
  try { unlinkSync(sock); } catch {}
  const p = spawn(BIN, ['--socket', sock, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '100', '--no-trade-log', ...args], { stdio: 'ignore', cwd: mkdtempSync(join(tmpdir(), 'mp-')) });
  for (let i = 0; i < 60 && !existsSync(sock); i++) await sleep(50);
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
