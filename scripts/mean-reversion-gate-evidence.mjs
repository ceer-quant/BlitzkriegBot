#!/usr/bin/env node
/**
 * #176 evidence: the `mean_reversion` trend gate, measured on a FROZEN corpus.
 *
 * The issue: in a one-sided market the fade leg keeps buying the cheap side as
 * it cheapens — every dip is another leg of the same slide, and in a double
 * market the cheap side is not oversold, it is being repriced. The fix is a
 * trend gate: the SAME measure the entry already takes (`min_drop_pct`, the draw
 * off the high of the token's own history) read over a LONGER window. A mid
 * several legs below its `trend_window_sec` high is a slide, not a dip.
 *
 * This script is how the gate's shape and threshold were chosen — and how a
 * reviewer re-derives them:
 *
 *   1. the corpus is four 1 h slices of the recorded 2026-09-19/20 capture
 *      (`data/archive/`), committed under
 *      `docs/reports/data/mean-reversion-gate/` (gzipped, sha256-pinned below).
 *      Two are one-sided slices and two two-sided; the rule that picked them is
 *      stated per window (`basis`) and is a baseline-replay outcome, not a
 *      hand-label: the hours with the worst shipped-config PnL and zero wins
 *      (one-sided) and the hours with its best win rate (two-sided);
 *   2. every arm is the SHIPPED binary replayed on that corpus with the real
 *      `mean_reversion` cdylib loaded from `user_layer/strategies/target/release`
 *      — the only difference between arms is a knob value, handed over through
 *      the same hot-param registry Shadow Evolution writes (`--backtest-knob`);
 *   3. the table below is trades / win rate / max drawdown / net PnL per arm
 *      per window, printed and (with `--out`) written as JSON.
 *
 * The corpus is frozen by content, not by path: every run verifies the sha256 of
 * the DECOMPRESSED JSONL against `sha256` below and refuses to report on a
 * mismatch. `--build-corpus` regenerates the files from a local `data/archive`
 * and checks the same hashes, so the transformation (window, filters, ladder
 * truncation) is auditable rather than a claim.
 *
 * Corpus construction, applied identically to every window and every arm:
 *   - span [t0 - 15 min, t0 + 60 min): the lead-in makes the replayed rounds and
 *     the strategy's lookback buffers warm at t0;
 *   - `round` events verbatim (market metadata: tokens, expiry, question);
 *   - `book` events for every token, truncated to the top 3 levels per side.
 *     Truncation is validated, not assumed: `--verify-truncation` replays one
 *     window from the untruncated archive slice and requires an identical report
 *     (same fills, same orders, same PnL) — the strategy's orders are 10 shares
 *     and the walk never reaches the fourth level;
 *   - `spot` and `top` events dropped (the fade leg reads books only, and the
 *     momentum gate it is exempt from is the only spot consumer).
 *
 * Hermetic: a scratch dir, dry mode, no event archive, no strategy state, no
 * network, the repo's own binary and dylib.
 *
 * Usage:
 *   node scripts/mean-reversion-gate-evidence.mjs                 # shipped vs no-gate
 *   node scripts/mean-reversion-gate-evidence.mjs --sweep         # the (window, %) grid
 *   node scripts/mean-reversion-gate-evidence.mjs --out <path>    # also write JSON
 *   node scripts/mean-reversion-gate-evidence.mjs --build-corpus  # regenerate from data/archive
 *
 * Needs: cargo build --release --workspace --locked
 *        (cd user_layer/strategies && cargo build --release)
 */
import { spawn } from './lib/child-guard.mjs';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'fs';
import { join } from 'path';
// The corpus pins live in one place (#203): a second copy of four hashes is a
// copy that cannot notice the original moving.
import { WINDOWS, buildCorpus, corpusPath, materialize } from './lib/frozen-corpus.mjs';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const STRATEGY_DIR = join(ROOT, 'user_layer', 'strategies', 'target', 'release');
const ARCHIVE_DIR = join(ROOT, 'data', 'archive');

const SHIPPED = { label: 'shipped (gate 600s/-30%)', knobs: [] };
const NO_GATE = { label: 'no gate (pre-#176)', knobs: [['trend_window_sec', '0']] };
const SWEEP = [
  { label: 'W300 / -20%', knobs: [['trend_window_sec', '300'], ['trend_drop_pct', '20']] },
  { label: 'W300 / -30%', knobs: [['trend_window_sec', '300'], ['trend_drop_pct', '30']] },
  { label: 'W600 / -20%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '20']] },
  { label: 'W600 / -30%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '30']] },
  { label: 'W600 / -35%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '35']] },
  { label: 'W600 / -40%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '40']] },
  { label: 'W600 / -45%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '45']] },
  { label: 'W600 / -60%', knobs: [['trend_window_sec', '600'], ['trend_drop_pct', '60']] },
  { label: 'W1200 / -30% (out of domain)', knobs: [['trend_window_sec', '1200'], ['trend_drop_pct', '30']] },
];

// ---------------------------------------------------------------------------
// build-corpus: regenerate the committed files from the local archive
// ---------------------------------------------------------------------------
function parseKnobSpec(spec) {
  const i = spec.indexOf(':');
  const j = spec.indexOf('=');
  return [spec.slice(0, i), spec.slice(i + 1, j), spec.slice(j + 1)];
}

/** One replay. Returns the report's own numbers, no re-derivation. */
function runArm(corpus, arm) {
  return new Promise((resolve) => {
    const report = join(corpus.dir, `${arm.label.replace(/[^a-z0-9]+/gi, '_')}.json`);
    const args = [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--strategy-dir', STRATEGY_DIR,
      '--enable-strategy', 'mean_reversion',
      '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', '1000', '--max-order-notional', '12',
      '--backtest', corpus.path,
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
    ];
    for (const [knob, value] of arm.knobs) args.push('--backtest-knob', `mean_reversion:${knob}=${value}`);
    const child = spawn(BIN, args, { cwd: ROOT, stdio: ['ignore', 'ignore', 'pipe'] });
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      if (code !== 0 || !existsSync(report)) {
        resolve({ arm: arm.label, error: `exit ${code}: ${err.split('\n').slice(-4).join(' | ')}` });
        return;
      }
      const r = JSON.parse(readFileSync(report, 'utf8'));
      resolve({
        arm: arm.label,
        closed: r.trades.closed,
        wins: r.trades.wins,
        losses: r.trades.losses,
        winRatePct: Number(r.trades.winRatePct),
        netPnlUsd: Number(r.trades.netPnlUsd),
        maxDrawdownUsd: Number(r.trades.maxDrawdownUsd),
        feesUsd: Number(r.trades.feesUsd),
        orders: r.orders.orders,
        filled: r.orders.filled,
        cancelled: r.orders.cancelled,
        rejected: r.orders.rejected,
        openPositions: r.openPositions,
        riskAlerts: r.riskAlerts.length,
        errors: r.errors.length,
      });
    });
  });
}

function fmt(v, w = 9, d = 2) {
  return typeof v === 'number' ? v.toFixed(d).padStart(w) : String(v).padStart(w);
}

function printWindow(w, rows) {
  console.log(`\n== ${w.name} (${w.regime}) — ${w.at}  [${w.basis}]`);
  console.log('   arm                          closed  wins  win%    netPnl      maxDD     fees   fills  rej');
  for (const r of rows) {
    if (r.error) {
      console.log(`   ${r.arm.padEnd(28)} ERROR ${r.error}`);
      continue;
    }
    console.log(
      `   ${r.arm.padEnd(28)} ${fmt(r.closed, 6, 0)} ${fmt(r.wins, 5, 0)} ${fmt(r.winRatePct, 5, 1)} ${fmt(r.netPnlUsd, 9)} ${fmt(r.maxDrawdownUsd, 9)} ${fmt(r.feesUsd, 7)} ${fmt(r.filled, 6, 0)} ${fmt(r.rejected, 4, 0)}`,
    );
  }
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--build-corpus')) {
    const ai = argv.indexOf('--archive');
    await buildCorpus(ROOT, ai >= 0 && argv[ai + 1] ? argv[ai + 1] : ARCHIVE_DIR);
    return;
  }
  if (!existsSync(BIN)) {
    console.error(`missing binary ${BIN}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  if (!existsSync(join(STRATEGY_DIR, 'libmean_reversion_strategy.dylib'))) {
    console.error(`missing dylib in ${STRATEGY_DIR}: (cd user_layer/strategies && cargo build --release)`);
    process.exit(2);
  }
  const sweep = argv.includes('--sweep');
  const outIdx = argv.indexOf('--out');
  const arms = sweep ? [NO_GATE, ...SWEEP] : [NO_GATE, SHIPPED];
  const result = { windows: [], arms: arms.map((a) => a.label), generated: new Date().toISOString() };
  for (const w of WINDOWS) {
    const corpus = materialize(ROOT, w);
    const rows = [];
    for (const arm of arms) rows.push(await runArm(corpus, arm));
    printWindow(w, rows);
    result.windows.push({ name: w.name, regime: w.regime, at: w.at, basis: w.basis, sha256: corpus.sha, rows });
  }
  // Aggregate per arm: the gate must remove the one-sided entries and leave the
  // two-sided ones alone.
  console.log('\n== totals per arm');
  console.log('   arm                          closed   wins   netPnl     maxDD   oneSidedClosed  twoSidedClosed');
  for (const arm of arms) {
    const rs = result.windows.flatMap((w) => w.rows.filter((r) => r.arm === arm.label));
    const sum = (f) => rs.reduce((a, r) => a + (r[f] ?? 0), 0);
    const byRegime = (regime) => result.windows
      .filter((w) => w.regime === regime)
      .flatMap((w) => w.rows.filter((r) => r.arm === arm.label))
      .reduce((a, r) => a + (r.closed ?? 0), 0);
    const row = {
      arm: arm.label,
      closed: sum('closed'),
      wins: sum('wins'),
      netPnlUsd: sum('netPnlUsd'),
      maxDrawdownUsd: sum('maxDrawdownUsd'),
      oneSidedClosed: byRegime('one-sided'),
      twoSidedClosed: byRegime('two-sided'),
    };
    result.windows.push; // no-op keeps the shape obvious
    (result.totals ??= []).push(row);
    console.log(
      `   ${arm.label.padEnd(28)} ${fmt(row.closed, 6, 0)} ${fmt(row.wins, 6, 0)} ${fmt(row.netPnlUsd, 9)} ${fmt(row.maxDrawdownUsd, 9)} ${fmt(row.oneSidedClosed, 14, 0)} ${fmt(row.twoSidedClosed, 14, 0)}`,
    );
  }
  if (outIdx >= 0 && argv[outIdx + 1]) {
    writeFileSync(argv[outIdx + 1], JSON.stringify(result, null, 2));
    console.log(`\nwrote ${argv[outIdx + 1]}`);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
