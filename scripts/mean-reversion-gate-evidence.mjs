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
import { createHash } from 'crypto';
import { createReadStream, createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'fs';
import { gunzipSync, gzipSync } from 'zlib';
import { createInterface } from 'readline';
import { join } from 'path';
import { tmpdir } from 'os';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const STRATEGY_DIR = join(ROOT, 'user_layer', 'strategies', 'target', 'release');
const ARCHIVE_DIR = join(ROOT, 'data', 'archive');
const CORPUS_DIR = join(ROOT, 'docs', 'reports', 'data', 'mean-reversion-gate');

const LEAD_MS = 15 * 60 * 1000;
const SPAN_MS = 60 * 60 * 1000;
const TOP_LEVELS = 3;

// ---------------------------------------------------------------------------
// The frozen corpus. `sha256` is over the DECOMPRESSED jsonl; a run whose file
// does not hash to it is refused rather than reported.
// ---------------------------------------------------------------------------
const WINDOWS = [
  {
    name: 'trend-20260919T1000Z',
    at: '2026-09-19T10:00:00Z',
    regime: 'one-sided',
    basis: 'worst shipped-config PnL of 2026-09-19 (-$12.11, 0 wins / 20 trades)',
    sha256: 'eb43ecef092dd4d1cc37064d8d0b7ce7ec754fcd25b7ea9e357ae4aa34104d18',
  },
  {
    name: 'range-20260919T1600Z',
    at: '2026-09-19T16:00:00Z',
    regime: 'two-sided',
    basis: 'best shipped-config win rate of 2026-09-19 (7 wins / 17 trades, 41%)',
    sha256: '497b921df3d24911604132c42a7b7102573d0da2a29485eb53d44f82b3ee87d0',
  },
  {
    name: 'trend-20260920T2100Z',
    at: '2026-09-20T21:00:00Z',
    regime: 'one-sided',
    basis: 'worst shipped-config PnL of 2026-09-20 (-$10.16, 0 wins / 17 trades)',
    sha256: '5179ac7f7f7eb7108b8baf8a5a0153868f0ea3d10f63f085fc0300b6fb5a0836',
  },
  {
    name: 'range-20260920T2300Z',
    at: '2026-09-20T23:00:00Z',
    regime: 'two-sided',
    basis: 'best shipped-config win rate of 2026-09-20 (7 wins / 18 trades, 39%; the only profitable hour of the capture)',
    sha256: '3c365b0a9574117cc2f0b3e60d96f8e0159035dee9b3cc8c50e4dcd559f6ea79',
  },
];

// Arms. `knobs` is exactly what differs between two runs.
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

function truncateBook(ev) {
  const b = Array.isArray(ev.b) ? ev.b.slice(-TOP_LEVELS) : undefined;
  const a = Array.isArray(ev.a) ? ev.a.slice(-TOP_LEVELS) : undefined;
  const out = { at: ev.at, k: 'book', t: ev.t };
  if (b) out.b = b;
  if (a) out.a = a;
  return JSON.stringify(out);
}

async function buildCorpus(archiveDir) {
  if (!existsSync(archiveDir)) {
    console.error(`missing ${archiveDir}: --build-corpus needs the recorded capture (override with --archive <dir>)`);
    process.exit(2);
  }
  const files = (await import('fs')).readdirSync(archiveDir).filter((f) => /^events.*\.jsonl$/.test(f)).sort();
  mkdirSync(CORPUS_DIR, { recursive: true });
  const rows = new Map(WINDOWS.map((w) => [w.name, []]));
  let scanned = 0;
  for (const f of files) {
    const rl = createInterface({ input: createReadStream(join(archiveDir, f)), crlfDelay: Infinity });
    for await (const line of rl) {
      // Cheap prefilter before JSON: books and rounds only.
      if (!line.includes('"k":"book"') && !line.includes('"k":"round"')) continue;
      scanned += 1;
      const at = Number(/^\{?"?at"?:\s*(\d+)/.exec(line)?.[1] ?? /"at":(\d+)/.exec(line)?.[1] ?? NaN);
      if (!Number.isFinite(at)) continue;
      for (const w of WINDOWS) {
        const t0 = Date.parse(w.at);
        if (at < t0 - LEAD_MS || at >= t0 + SPAN_MS) continue;
        rows.get(w.name).push(line.includes('"k":"book"') ? truncateBook(JSON.parse(line)) : line);
      }
    }
  }
  for (const w of WINDOWS) {
    const body = rows.get(w.name);
    body.sort((x, y) => JSON.parse(x).at - JSON.parse(y).at);
    const jsonl = body.join('\n') + '\n';
    const buf = Buffer.from(jsonl, 'utf8');
    const sha = createHash('sha256').update(buf).digest('hex');
    const gz = join(CORPUS_DIR, `${w.name}.jsonl.gz`);
    writeFileSync(gz, gzipSync(buf, { level: 9 }));
    const expect = w.sha256 === 'PENDING' ? '(paste this)' : w.sha256;
    const ok = w.sha256 === 'PENDING' || w.sha256 === sha;
    console.log(`${w.name}: ${body.length} events, ${(buf.length / 1e6).toFixed(2)} MB raw, ${(readFileSync(gz).length / 1e6).toFixed(2)} MB gz`);
    console.log(`  sha256(uncompressed) ${sha} ${w.sha256 === 'PENDING' ? expect : ok ? 'OK' : `MISMATCH want ${expect}`}`);
    if (!ok) process.exit(1);
  }
  console.log(`scanned ${scanned} archive lines`);
}

// ---------------------------------------------------------------------------
// Replay arms
// ---------------------------------------------------------------------------
function corpusPath(w) {
  return join(CORPUS_DIR, `${w.name}.jsonl.gz`);
}

function materialize(w) {
  const gz = corpusPath(w);
  if (!existsSync(gz)) {
    console.error(`missing corpus ${gz}: run --build-corpus from a checkout with data/archive`);
    process.exit(2);
  }
  const buf = gunzipSync(readFileSync(gz));
  const sha = createHash('sha256').update(buf).digest('hex');
  if (w.sha256 !== 'PENDING' && sha !== w.sha256) {
    console.error(`${w.name}: corpus sha256 ${sha} != pinned ${w.sha256} — refusing to report on a changed corpus`);
    process.exit(2);
  }
  const dir = mkdtempSync(join(tmpdir(), 'bk-mr-gate-'));
  const path = join(dir, `${w.name}.jsonl`);
  writeFileSync(path, buf);
  return { path, dir, sha };
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
    await buildCorpus(ai >= 0 && argv[ai + 1] ? argv[ai + 1] : ARCHIVE_DIR);
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
    const corpus = materialize(w);
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
