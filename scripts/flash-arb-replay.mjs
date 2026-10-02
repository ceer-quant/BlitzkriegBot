#!/usr/bin/env node
/**
 * flash_arb replay arms — the 4 frozen windows through the release core with
 * the flash_arb package enabled, the same spawn shape the exit-economics gate
 * uses (`--backtest` replay, `--no-*-log`, seed 1000, notional cap 12).
 *
 * No baseline to judge against (flash_arb is new): this prints the measured
 * rows and exits 0; the READING is the delivery report's job. Arms isolate
 * gates by difference:
 *
 *   node scripts/flash-arb-replay.mjs --corpus-dir /tmp/bk-spot-corpus
 *       # spec defaults on the spot-bearing corpus (the trigger needs spot)
 *   node scripts/flash-arb-replay.mjs --corpus-dir /tmp/bk-spot-corpus \
 *        --param min_bid_depth=1 --param max_abs_obi=1 --param min_time_left_sec=0
 *       # widest diagnostic (gate ceiling)
 *   node scripts/flash-arb-replay.mjs --strategy spread_arb --corpus-dir /tmp/bk-spot-corpus
 *       # the OLD strategy on the SAME corpus (the comparison arm); --param
 *       keys then validate against that package's own manifest
 *
 * `--param k=v` rewrites the tunable DEFAULT in a throwaway copy of the
 * package (the entry file's sha256 is untouched — only the manifest moves),
 * so an arm is a pure manifest change, reproducible from this file.
 * `--corpus-dir <dir>` points at pre-built <name>.jsonl files (the
 * mad-dog-spot-corpus.mjs output); without it the pinned book-only corpus is
 * materialized, on which flash_arb cannot fire (no spot lines → no trigger).
 */

import { spawn } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const OUT_DIR = process.env.BK_FLASH_ARB_OUT || join(tmpdir(), 'bk-flash-arb-replay');

const args = process.argv.slice(2);
const params = {};
const extraFlags = [];
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const STRATEGY = flag('--strategy') || 'flash_arb';
const PACKAGE = join(ROOT, 'user_layer', 'strategies_lua', STRATEGY);
// Round length passthrough (5m integration, task 2.1): the driver no longer
// assumes 15m — `--round-sec 300` replays a 5m corpus; 900 stays the default
// so every existing arm is unchanged. (The hot_side_momentum package takes
// the same value via --param round_sec=300.)
const ROUND_SEC = flag('--round-sec') || '900';
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
  const root = mkdtempSync(join(tmpdir(), `bk-${STRATEGY}-pkg-`));
  cpSync(PACKAGE, join(root, STRATEGY), { recursive: true });
  if (Object.keys(params).length > 0) {
    const manifestPath = join(root, STRATEGY, 'manifest.json');
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
        '--enable-strategy', STRATEGY,
        '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
        '--round-sec', ROUND_SEC, '--min-round-age', '0', '--min-time-left', '0',
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
        profitFactor: t.profitFactor ?? null,
        feesUsd: t.feesUsd ?? 0,
        report,
      });
    });
  });
}

const luaDir = stagePackage();
mkdirSync(OUT_DIR, { recursive: true });
const armDir = join(OUT_DIR, `${STRATEGY}-` + (Object.keys(params).length ? 'arm-' + Object.entries(params).map(([k, v]) => `${k}-${v}`).join('_') : 'arm-default'));
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
        if (!existsSync(p)) throw new Error(`missing ${p} (run mad-dog-spot-corpus.mjs first)`);
        return p;
      })()
    : materialize(ROOT, w).path;
  rows.push(await runOne(luaDir, w.name, path, armDir));
}
console.log(`# strategy ${STRATEGY}  arm ${JSON.stringify(params)}${CORPUS_DIR ? `  corpus ${CORPUS_DIR}` : '  corpus book-only-pinned'}`);
for (const r of rows) {
  if (r.error) console.log(`${r.window}  ERROR  ${r.error}`);
  else
    console.log(
      `${r.window}  closed=${r.closed}  wins=${r.wins}  WR=${Number(r.winRatePct).toFixed(2)}%  net=${Number(r.netPnlUsd).toFixed(4)}  PF=${r.profitFactor ?? 'n/a'}  fees=${Number(r.feesUsd).toFixed(4)}`,
    );
}
console.log(`# reports in ${armDir}`);
