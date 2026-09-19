#!/usr/bin/env node
/**
 * E14 memory baseline / re-measure — peak RSS of a dry core under a
 * sustained synthetic book load.
 *
 * Reproduces the load shape the live feed produces, WITHOUT a market plugin:
 *   - one round with 40 markets (`engine.markets`),
 *   - 40 tokens, each refreshed at ~20 Hz with a full L2 snapshot of
 *     20 levels per side (`engine.book` — the same choke point the
 *     Rust-native feed drives),
 *   - the IPC maintenance loop ticking at the production cadence (50 ms).
 *
 * Peak and final RSS are sampled from `ps` every 250 ms and archived as JSON,
 * so a later E14 run is directly comparable (acceptance: "内存从基线到目标").
 *
 * Usage:  node scripts/e14-memory-baseline.mjs [--seconds 30] [--tokens 40]
 *                                              [--depth 20] [--out docs/perf/e14-mem-<tag>.json]
 */
import { spawn } from './lib/child-guard.mjs';
import net from 'node:net';
import { execSync } from 'node:child_process';
import { join, resolve, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');

const arg = (name, dflt) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? Number(process.argv[i + 1]) || dflt : dflt;
};
// String-valued flags: only use the value when the flag was actually passed
// AND a value follows it — falling back to argv[0] (the node binary itself)
// is exactly how the first draft overwrote the interpreter with JSON.
const argStr = (name, dflt) => {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
};
const SECONDS = arg('--seconds', 30);
const TOKENS = arg('--tokens', 40);
const DEPTH = arg('--depth', 20);
const OUT = resolve(ROOT, argStr('--out', join('docs', 'perf', 'e14-mem-baseline.json')));
if (OUT === ROOT || !OUT.startsWith(ROOT + '/')) {
  console.error(`FAIL: --out must resolve inside the repo, got ${OUT}`);
  process.exit(1);
}

const WORK = mkdtempSync(join(tmpdir(), 'e14-mem-'));
const SOCK = join(WORK, 'mem.sock');
const LF = String.fromCharCode(10);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function rpcLine(c, obj) {
  return new Promise((res) => {
    c.write(JSON.stringify(obj) + LF, () => res());
  });
}

const rssOf = (pid) => {
  try {
    return Number(execSync(`ps -o rss= -p ${pid}`, { encoding: 'utf8' }).trim());
  } catch {
    return null;
  }
};

async function waitSocket() {
  for (let i = 0; i < 100; i++) {
    const ok = await new Promise((res) => {
      const c = net.connect(SOCK);
      c.on('connect', () => { c.end(); res(true); });
      c.on('error', () => res(false));
    });
    if (ok) return true;
    await sleep(100);
  }
  return false;
}

async function main() {
  const core = spawn(CORE,
    ['--socket', SOCK, '--mode', 'dry', '--engine', '--tick-ms', '50',
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    { cwd: WORK, stdio: ['ignore', 'ignore', 'inherit'] });
  const pid = core.pid;
  if (!await waitSocket()) {
    console.error('FAIL: core socket never appeared');
    process.exit(1);
  }

  const c = net.connect(SOCK);
  await new Promise((res) => c.on('connect', res));
  let buf = '';
  c.on('data', (d) => { buf += d.toString(); });

  const notify = (method, params) =>
    rpcLine(c, { jsonrpc: '2.0', id: 1, method, params });

  // One round, 40 markets, horizon far beyond the measurement window.
  const nowMs = Date.now();
  const mkts = Array.from({ length: TOKENS }, (_, i) => ({
    asset: `A${i}`,
    conditionId: `cond-${i}`,
    questionId: `q-${i}`,
    upTokenId: `tok-${i}`,
    downTokenId: `dn-${i}`,
    upPrice: '0.60',
    downPrice: '0.40',
    expiresAtMs: nowMs + 10 * 3600 * 1000,
    roundSlot: Math.floor(nowMs / 1000 / 900),
    negRisk: true,
    question: '?',
  }));
  await notify('engine.markets', { markets: mkts });

  const mid = 0.45;
  const level = (i) => ({
    price: (mid + (i + 1) * 0.01).toFixed(2),
    size: '100',
  });
  const bookBody = (i) => ({
    tokenId: `tok-${i}`,
    bids: Array.from({ length: DEPTH }, (_, l) => ({ price: (mid - (l + 1) * 0.01).toFixed(2), size: '100' })),
    asks: Array.from({ length: DEPTH }, (_, l) => ({ price: (mid + (l + 1) * 0.01).toFixed(2), size: '100' })),
    tsMs: nowMs,
  });

  // ~20 Hz refresh of every token, for SECONDS seconds.
  const t0 = Date.now();
  const samples = [];
  let peak = 0;
  let peakAt = 0;
  while (Date.now() - t0 < SECONDS * 1000) {
    const b = bookBody(Math.floor(Math.random() * TOKENS));
    b.tsMs = Date.now();
    await notify('engine.book', b);
    if (Date.now() - t0 > 250 * samples.length) {
      const r = rssOf(pid);
      if (r !== null) {
        samples.push(r);
        if (r > peak) { peak = r; peakAt = Math.round((Date.now() - t0) / 1000); }
      }
    }
    await sleep(50);
  }
  const final = rssOf(pid);
  const result = {
    tokens: TOKENS, depth: DEPTH, seconds: SECONDS,
    rssKbPeak: peak, rssKbPeakAtSec: peakAt, rssKbFinal: final,
    samples,
    at: new Date().toISOString(),
  };
  console.log(JSON.stringify(result, null, 2));
  writeFileSync(OUT, JSON.stringify(result, null, 2));
  console.log(`archived → ${OUT}`);
  c.end();
  core.kill('SIGTERM');
  await sleep(300);
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
  process.exit(0);
}

main().catch((e) => {
  console.error('FAIL', e);
  process.exit(1);
});
