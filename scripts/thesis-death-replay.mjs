#!/usr/bin/env node
/**
 * thesis-death replay arms (#961) — the A/B the acceptance asks for: the
 * SAME frozen windows through TWO cores with the matching lua_momentum
 * package, before/after the thesis-death enforcement:
 *
 *   baseline  = core built at f3753e11 (pre-#961) + the f3753e11 package
 *   candidate = core built on the #961 branch      + the #961 package
 *
 * Same spawn shape the mad-dog/exit-economics gates use (`--backtest` replay,
 * seed 1000, notional cap 12). The candidate arm keeps its trade log so the
 * report can count `thesis_death_exit` closes and prove no re-entry on a
 * token after its death close (命中后不再出现加仓/续持 tick).
 *
 * Environment:
 *   BK_BASE_CORE_BIN  baseline core binary (default: the sibling
 *                     baseline-961 worktree's release build — the two-binary
 *                     A/B is a LOCAL measurement rig, not a CI gate)
 *   BK_CORE_BIN       candidate core binary (default: this worktree's release build)
 *   BK_THESIS_DEATH_OUT  output dir (default: /tmp/bk-thesis-death-replay)
 *
 * The frozen corpus is materialized + hash-verified by scripts/lib/frozen-corpus.mjs.
 */

import { execFileSync, spawn } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BASE_REF = 'f3753e11';
const BASE_BIN =
  process.env.BK_BASE_CORE_BIN ||
  join(dirname(ROOT), 'baseline-961', 'target', 'release', 'blitzkrieg-core');
const CAND_BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const OUT_DIR = process.env.BK_THESIS_DEATH_OUT || join(tmpdir(), 'bk-thesis-death-replay');

if (!existsSync(BASE_BIN)) {
  console.error(`baseline core binary missing: ${BASE_BIN}`);
  console.error('build it: git worktree add ../baseline-961 --detach f3753e11 && (cd ../baseline-961 && cargo build --release -p blitzkrieg-core)');
  process.exit(2);
}
if (!existsSync(CAND_BIN)) {
  console.error(`candidate core binary missing: ${CAND_BIN}`);
  process.exit(2);
}

/** Stage the PRE-#961 package out of git history (byte-exact f3753e11). */
function stageBaselinePackage() {
  const root = mkdtempSync(join(tmpdir(), 'bk-thesis-base-pkg-'));
  const pkgDir = join(root, 'lua_momentum');
  mkdirSync(pkgDir, { recursive: true });
  for (const f of ['strategy.lua', 'manifest.json']) {
    const bytes = execFileSync('git', ['-C', ROOT, 'show', `${BASE_REF}:user_layer/strategies_lua/lua_momentum/${f}`], {
      maxBuffer: 1 << 24,
    });
    writeFileSync(join(pkgDir, f), bytes);
  }
  return root;
}

function stageCandidatePackage() {
  const root = mkdtempSync(join(tmpdir(), 'bk-thesis-cand-pkg-'));
  cpSync(join(ROOT, 'user_layer', 'strategies_lua', 'lua_momentum'), join(root, 'lua_momentum'), {
    recursive: true,
  });
  return root;
}

function runOne(bin, luaDir, windowName, corpusPath, outDir) {
  return new Promise((res) => {
    const report = join(outDir, `${windowName}.json`);
    const args = [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--no-intent-audit',
      '--lua-strategy-dir', luaDir,
      '--enable-strategy', 'lua_momentum',
      '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', '1000', '--max-order-notional', '12',
      '--backtest', corpusPath,
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
    ];
    const child = spawn(bin, args, {
      cwd: ROOT,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir },
    });
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

/**
 * Per-trade analysis off the report's own tradeLines (backtest mode persists
 * no logs — the report IS the record). The report spells reasons in the
 * ExitReason Debug form (`ThesisDeathExit`).
 */
function analyzeReport(path) {
  const r = JSON.parse(readFileSync(path, 'utf8'));
  const lines = r.tradeLines ?? [];
  const deaths = lines.filter((l) => l.reason === 'ThesisDeathExit');
  const byToken = new Map();
  for (const l of lines) byToken.set(l.tokenId, (byToken.get(l.tokenId) ?? 0) + 1);
  return {
    closedTotal: lines.length,
    deaths: deaths.length,
    deathNetPnl: deaths.reduce((a, l) => a + Number(l.netPnlUsd), 0),
    deathAvgNetPct:
      deaths.length
        ? deaths.reduce((a, l) => a + Number(l.netPnlPct), 0) / deaths.length
        : 0,
    // A token traded more than twice in one window is re-entry pressure —
    // informational (rounds are not in the lines; the no-reentry law is
    // pinned by the lua_momentum_thesis_check integration tests).
    tokensTradedMoreThanTwice: [...byToken.values()].filter((n) => n > 2).length,
    reasons: lines.reduce((m, l) => {
      m[l.reason] = (m[l.reason] ?? 0) + 1;
      return m;
    }, {}),
  };
}

const basePkg = stageBaselinePackage();
const candPkg = stageCandidatePackage();
mkdirSync(OUT_DIR, { recursive: true });
const baseDir = join(OUT_DIR, 'baseline');
const candDir = join(OUT_DIR, 'candidate');
rmSync(baseDir, { recursive: true, force: true });
rmSync(candDir, { recursive: true, force: true });
mkdirSync(baseDir, { recursive: true });
mkdirSync(candDir, { recursive: true });

const rows = [];
for (const w of WINDOWS) {
  const corpus = materialize(ROOT, w).path;
  const b = await runOne(BASE_BIN, basePkg, w.name, corpus, baseDir);
  const c = await runOne(CAND_BIN, candPkg, w.name, corpus, candDir);
  rows.push({ window: w.name, baseline: b, candidate: c });
}

const sum = (arm, k) => rows.reduce((a, r) => a + (r[arm].error ? 0 : Number(r[arm][k] ?? 0)), 0);
console.log(`# thesis-death A/B  base_ref=${BASE_REF}  windows=${rows.length}`);
console.log('# window                          baseline_closed  baseline_net   candidate_closed  candidate_net');
for (const r of rows) {
  const f = (x) => (x.error ? `ERROR ${x.error}` : `${String(x.closed).padStart(4)}  net=${Number(x.netPnlUsd).toFixed(4)}`);
  console.log(
    `${r.window.padEnd(32)}  ${f(r.baseline).padEnd(28)}  ${f(r.candidate)}`,
  );
}
const analysis = [];
for (const r of rows) {
  if (!r.candidate.error && existsSync(r.candidate.report)) {
    analysis.push({ window: r.window, ...analyzeReport(r.candidate.report) });
  }
}
const out = {
  baseRef: BASE_REF,
  baseBin: BASE_BIN,
  candBin: CAND_BIN,
  windows: rows.map(({ window, baseline, candidate }) => ({
    window,
    baseline: baseline.error ? { error: baseline.error } : { closed: baseline.closed, wins: baseline.wins, winRatePct: baseline.winRatePct, netPnlUsd: baseline.netPnlUsd },
    candidate: candidate.error ? { error: candidate.error } : { closed: candidate.closed, wins: candidate.wins, winRatePct: candidate.winRatePct, netPnlUsd: candidate.netPnlUsd },
  })),
  totals: {
    baseline: { closed: sum('baseline', 'closed'), netPnlUsd: sum('baseline', 'netPnlUsd') },
    candidate: { closed: sum('candidate', 'closed'), netPnlUsd: sum('candidate', 'netPnlUsd') },
  },
  candidateTradeLogAnalysis: analysis,
};
const outPath = join(OUT_DIR, 'summary.json');
writeFileSync(outPath, JSON.stringify(out, null, 2) + '\n');
console.log(`# TOTAL baseline closed=${out.totals.baseline.closed} net=${out.totals.baseline.netPnlUsd.toFixed(4)}`);
console.log(`# TOTAL candidate closed=${out.totals.candidate.closed} net=${out.totals.candidate.netPnlUsd.toFixed(4)}`);
for (const a of analysis) {
  console.log(
    `# ${a.window}: ThesisDeathExit closes=${a.deaths} (net=${a.deathNetPnl.toFixed(4)}, avg ${a.deathAvgNetPct.toFixed(2)}%) reasons=${JSON.stringify(a.reasons)}`,
  );
}
console.log(`# summary written to ${outPath}`);
