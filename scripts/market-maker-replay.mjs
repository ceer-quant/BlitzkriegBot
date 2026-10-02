#!/usr/bin/env node
/**
 * market_maker replay arms — the 4 frozen windows through the release core with
 * the market_maker package enabled, the same spawn shape the exit-economics gate
 * uses (`--backtest` replay, `--no-*-log`, seed 1000, notional cap 12).
 *
 * Unlike that gate there is NO baseline to judge against (market_maker is new), so
 * this prints the measured rows and exits 0; the READING is the delivery
 * report's job. Arms isolate gates by difference:
 *
 *   node scripts/market-maker-replay.mjs                        # defaults (spot gate fail-closed)
 *   node scripts/market-maker-replay.mjs --param spot_missing=pass   # book-only diagnostic
 *   node scripts/market-maker-replay.mjs --param spot_missing=pass \
 *        --param min_bid_depth=1 --param max_abs_obi=1 --param min_time_left_sec=0
 *                                                        # widest diagnostic (thesis ceiling)
 *
 * `--param k=v` rewrites the tunable DEFAULT in a throwaway copy of the
 * package (the entry file's sha256 is untouched — only the manifest moves),
 * so an arm is a pure manifest change, reproducible from this file.
 */

import { spawn } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const PACKAGE = join(ROOT, 'user_layer', 'strategies_lua', 'market_maker');
const OUT_DIR = process.env.BK_MARKET_MAKER_OUT || join(tmpdir(), 'bk-market-maker-replay');

const args = process.argv.slice(2);
const params = {};
const extraFlags = [];
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--param') {
    const [k, v] = args[i + 1].split('=');
    params[k] = v;
    i++;
  } else if (args[i] === '--extra') {
    extraFlags.push(...args[i + 1].split(' ').filter(Boolean));
    i++;
  }
}

function stagePackage() {
  const root = mkdtempSync(join(tmpdir(), 'bk-market-maker-pkg-'));
  cpSync(PACKAGE, join(root, 'market_maker'), { recursive: true });
  if (Object.keys(params).length > 0) {
    const manifestPath = join(root, 'market_maker', 'manifest.json');
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    for (const [k, v] of Object.entries(params)) {
      if (!manifest.tunables[k]) throw new Error(`unknown tunable ${k}`);
      manifest.tunables[k].default = v;
    }
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
  }
  return root;
}

function runOne(luaDir, windowName, corpusPath, outDir) {
  return new Promise((res) => {
    const report = join(outDir, `${windowName}.json`);
    const child = spawn(
      BIN,
      [
        '--mode', 'dry',
        '--engine',
        '--no-discovery',
        '--no-strategy-state',
        '--no-intent-audit',
        '--lua-strategy-dir', luaDir,
        '--enable-strategy', 'market_maker',
        '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
        '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
        '--seed-balance', '1000', '--max-order-notional', '12',
        '--backtest', corpusPath,
        '--backtest-report', report,
        '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
        ...extraFlags,
      ],
      { cwd: ROOT, stdio: ['ignore', 'ignore', 'pipe'],
        env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir } },
    );
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      if (code !== 0 || !existsSync(report)) {
        res({ window: windowName, error: `exit ${code}: ${err.split('\n').slice(-3).join(' | ')}` });
        return;
      }
      const r = JSON.parse(readFileSync(report, 'utf8'));
      const t = r.trades ?? {};
      res({
        window: windowName,
        closed: t.closed ?? 0,
        wins: t.wins ?? 0,
        winRatePct: t.winRatePct ?? 0,
        netPnlUsd: t.netPnlUsd ?? 0,
        report,
      });
    });
  });
}

const luaDir = stagePackage();
mkdirSync(OUT_DIR, { recursive: true });
const armDir = join(OUT_DIR, Object.keys(params).length ? 'arm-' + Object.entries(params).map(([k, v]) => `${k}-${v}`).join('_') : 'arm-default');
rmSync(armDir, { recursive: true, force: true });
mkdirSync(armDir, { recursive: true });

// --corpus-dir <dir>: pre-built <name>.jsonl files (the spot builder's
// output) instead of the pinned book-only corpus.
const CORPUS_DIR = flag('--corpus-dir');
const only = flag('--only');

const rows = [];
for (const w of WINDOWS) {
  if (only && w.name !== only) continue;
  const path = CORPUS_DIR
    ? (() => {
        const p = join(CORPUS_DIR, `${w.name}.jsonl`);
        if (!existsSync(p)) throw new Error(`missing ${p} (run market-maker-spot-corpus.mjs first)`);
        return p;
      })()
    : materialize(ROOT, w).path;
  rows.push(await runOne(luaDir, w.name, path, armDir));
}
console.log(`# arm ${JSON.stringify(params)}`);
for (const r of rows) {
  if (r.error) console.log(`${r.window}  ERROR  ${r.error}`);
  else
    console.log(
      `${r.window}  closed=${r.closed}  wins=${r.wins}  WR=${Number(r.winRatePct).toFixed(2)}%  net=${Number(r.netPnlUsd).toFixed(4)}`,
    );
}
console.log(`# reports in ${armDir}`);
