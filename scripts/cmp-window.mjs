#!/usr/bin/env node
/**
 * byte-compare the gate's spread_arb windows between two cores.
 * usage: node cmp-window.mjs <binA> <binB> [worktreeRoot]
 */
import { spawnSync } from 'child_process';
import { readFileSync, existsSync, mkdirSync, rmSync } from 'fs';
import { join } from 'path';
import { materialize, WINDOWS } from './lib/frozen-corpus.mjs';

const [binA, binB, root = '/Volumes/Hard Disk/bk-wt-390'] = process.argv.slice(2);
if (!binA || !binB || !existsSync(binA) || !existsSync(binB)) {
  console.error('usage: node cmp-window.mjs <binA> <binB> [worktreeRoot]');
  process.exit(2);
}

const LUA_DIR = join(root, 'user_layer', 'strategies_lua');
const outDir = '/Volumes/Hard Disk/bk-agent390/cmp-windows';
mkdirSync(outDir, { recursive: true });

function runCore(bin, corpus, report) {
  const args = [
    '--mode', 'dry', '--engine', '--no-discovery', '--no-strategy-state',
    '--no-strategy-dir', '--lua-strategy-dir', LUA_DIR, '--enable-strategy', 'spread_arb',
    '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
    '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
    '--seed-balance', '1000', '--max-order-notional', '12',
    '--backtest', corpus, '--backtest-report', report,
    '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
  ];
  const r = spawnSync(bin, args, { cwd: root, stdio: ['ignore', 'ignore', 'pipe'], timeout: 600_000, maxBuffer: 256 * 1024 * 1024 });
  if (r.status !== 0 || !existsSync(report)) {
    throw new Error(`core exited ${r.status}: ${String(r.stderr).split('\n').slice(-4).join(' | ')}`);
  }
}

let allIdentical = true;
for (const w of WINDOWS) {
  const { path, dir: corpusDir } = materialize(root, w);
  const repA = join(outDir, `${w.name}-A.json`);
  const repB = join(outDir, `${w.name}-B.json`);
  runCore(binA, path, repA);
  runCore(binB, path, repB);
  rmSync(corpusDir, { recursive: true, force: true });
  const a = readFileSync(repA);
  const b = readFileSync(repB);
  const identical = Buffer.compare(a, b) === 0;
  if (!identical) allIdentical = false;
  const ta = JSON.parse(a).trades ?? {};
  const tb = JSON.parse(b).trades ?? {};
  console.log(
    `${identical ? 'IDENTICAL' : 'DIFFER    '} ${w.name}  ` +
      `A(closed ${ta.closed}, WR ${Number(ta.winRatePct ?? 0).toFixed(2)}%, net ${Number(ta.netPnlUsd ?? 0).toFixed(4)}) | ` +
      `B(closed ${tb.closed}, WR ${Number(tb.winRatePct ?? 0).toFixed(2)}%, net ${Number(tb.netPnlUsd ?? 0).toFixed(4)})`,
  );
}
console.log(allIdentical ? '\nALL WINDOWS BYTE-IDENTICAL' : '\nDIFFERENCES PRESENT');
process.exit(allIdentical ? 0 : 1);
