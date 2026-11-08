#!/usr/bin/env node
/**
 * lua_momentum trailing-band sweep — the 「提利润」 lever, measured on the
 * frozen corpus exactly the way `market-maker-replay.mjs` measures arms:
 * every cell stages a THROWAWAY copy of the package whose manifest defaults
 * are rewritten (`--param`-style), spawns the release core in `--backtest`
 * mode over the pinned windows, and prints closed/wins/WR/net per cell.
 *
 * Why this sweep: the live ledger says the trailing class exits with +$0.084
 * per fill while the TP class gets +$0.347 (400 vs 333 fills) — the give-back
 * band (arm 20 / giveback 15) is leaving roughly $105 on the table across
 * the recorded book. The grid walks arm/giveback around the shipped values;
 * nothing here is declared a winner — the READING (and any manifest bump) is
 * the operator's call, per the pre-declared-experiment discipline.
 *
 *   node scripts/momentum-trailing-sweep.mjs                 # shipped band only
 *   node scripts/momentum-trailing-sweep.mjs --sweep         # the full grid
 *   node scripts/momentum-trailing-sweep.mjs --sweep --only trend-20260919T1000Z
 *   node scripts/momentum-trailing-sweep.mjs --param trail_giveback_pct=30
 *
 * Corpus: the pinned book-only windows (lib/frozen-corpus.mjs). KNOWN LIMIT
 * (#964): lua_momentum enters as a PURE MAKER and a book-only corpus with no
 * bid/ask gap starves it — a cell that closes ~0 trades is the corpus's
 * property, not the band's. Cells report `closed` so a starved cell is
 * visible instead of averaged in.
 */
import { spawn } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const PACKAGE = join(ROOT, 'user_layer', 'strategies_lua', 'lua_momentum');
const STRATEGY = 'lua_momentum';
const OUT_DIR = process.env.BK_MOMENTUM_SWEEP_OUT || join(tmpdir(), 'bk-momentum-sweep');

const args = process.argv.slice(2);
const has = (n) => args.includes(n);
const flag = (n, d) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : d;
};
const explicitParams = {};
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--param') {
    const [k, v] = args[i + 1].split('=');
    explicitParams[k] = v;
    i++;
  }
}

/** The shipped band, then the grid around it (arm/giveback pairs only). */
const SHIPPED = { trail_arm_pct: '20', trail_giveback_pct: '15' };
const GRID = [];
for (const arm of ['20', '25', '30']) {
  for (const give of ['10', '15', '20', '25']) {
    GRID.push({ trail_arm_pct: arm, trail_giveback_pct: give });
  }
}
const cells = has('--sweep')
  ? GRID
  : Object.keys(explicitParams).length
    ? [explicitParams]
    : [SHIPPED];

function stagePackage(params) {
  const root = mkdtempSync(join(tmpdir(), 'bk-momentum-pkg-'));
  cpSync(PACKAGE, join(root, STRATEGY), { recursive: true });
  const manifestPath = join(root, STRATEGY, 'manifest.json');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  for (const [k, v] of Object.entries(params)) {
    if (!manifest.tunables[k]) throw new Error(`unknown tunable ${k}`);
    manifest.tunables[k].default = v;
  }
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
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
        '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
        '--seed-balance', '1000', '--max-order-notional', '12',
        '--backtest', corpusPath,
        '--backtest-report', report,
        '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
      ],
      {
        cwd: ROOT,
        stdio: ['ignore', 'ignore', 'pipe'],
        env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir },
      },
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
      });
    });
  });
}

if (!existsSync(BIN)) {
  console.error(`missing kernel binary ${BIN} — cargo build --release --workspace --locked first`);
  process.exit(2);
}

mkdirSync(OUT_DIR, { recursive: true });
const only = flag('--only');
const summary = [];
for (const params of cells) {
  const luaDir = stagePackage(params);
  const cellDir = join(
    OUT_DIR,
    'cell-' + Object.entries(params).map(([k, v]) => `${k}-${v}`).join('_'),
  );
  rmSync(cellDir, { recursive: true, force: true });
  mkdirSync(cellDir, { recursive: true });
  const rows = [];
  for (const w of WINDOWS) {
    if (only && w.name !== only) continue;
    rows.push(await runOne(luaDir, w.name, materialize(ROOT, w).path, cellDir));
  }
  rmSync(luaDir, { recursive: true, force: true });
  const net = rows.reduce((a, r) => a + (r.netPnlUsd ?? 0), 0);
  const closed = rows.reduce((a, r) => a + (r.closed ?? 0), 0);
  const wins = rows.reduce((a, r) => a + (r.wins ?? 0), 0);
  summary.push({ params, net, closed, wins });
  console.log(`# ${JSON.stringify(params)}`);
  for (const r of rows) {
    if (r.error) console.log(`  ${r.window}  ERROR  ${r.error}`);
    else
      console.log(
        `  ${r.window}  closed=${r.closed}  wins=${r.wins}  WR=${Number(r.winRatePct).toFixed(2)}%  net=${Number(r.netPnlUsd).toFixed(4)}`,
      );
  }
  console.log(
    `  == total closed=${closed} wins=${wins} WR=${closed ? ((wins / closed) * 100).toFixed(1) : 'n/a'}% net=${net.toFixed(4)}`,
  );
}

if (summary.length > 1) {
  console.log('\n# summary (sorted by net)');
  for (const s of [...summary].sort((a, b) => b.net - a.net)) {
    console.log(
      `  ${JSON.stringify(s.params).padEnd(48)} closed=${String(s.closed).padStart(3)} ` +
        `WR=${s.closed ? ((s.wins / s.closed) * 100).toFixed(1) : 'n/a'}% net=${s.net.toFixed(4)}`,
    );
  }
  console.log(
    '\nNOTE: a cell with closed≈0 is the corpus starving the pure-maker entry (#964) — read `closed` before any net.',
  );
}
