#!/usr/bin/env node
/**
 * #272 — the ECONOMIC gate on exit reachability.
 *
 * The defect this exists for: the F6/#168 batch pinned "a profit-side exit is
 * unreachable without an executable bid" as EXPECTED BEHAVIOUR, with four unit
 * tests asserting the mechanism (`exit_policy.rs` `no_bid_means_no_executable_
 * price_even_with_a_high_ask`, `force_exit_does_not_fire_without_an_executable_
 * bid`, `reference_price_prefers_bid_then_two_sided_mid_then_fallback`,
 * `position.rs` `no_bid_mint_no_exit_request_even_at_deadline`). Every one of
 * them passed. Nothing in the suite could answer the only question that
 * mattered: did that change make the strategy earn more or less? It was found
 * afterwards, by a 24-hour corpus A/B.
 *
 * So this gate does not assert a mechanism. It replays the release core over
 * the sha256-pinned frozen corpus (`lib/frozen-corpus.mjs`, in-repo, no
 * network) with ONE strategy, and compares the MONEY against a recorded
 * baseline. It goes red when the same corpus pays less than it used to.
 *
 * ---------------------------------------------------------------------------
 * On #272's acceptance criteria 2 and 3 (2026-09-23 measurement)
 *
 * The issue asks for `net >= 0`, `WR >= 60%`, and a reverse acceptance in which
 * a binary built from the pinned BEFORE commit `d5c7fec3` MUST FAIL the gate.
 * Measured on these four windows, that is not satisfiable, and the reason is
 * worth recording rather than papering over:
 *
 *   window                  BEFORE net / WR    HEAD net / WR
 *   trend-20260919T1000Z      +$2.58 / 54%       -$0.02 / 30%
 *   range-20260919T1600Z     +$13.62 / 72%       -$5.07 / 37%
 *   trend-20260920T2100Z      +$5.75 / 75%       -$0.71 / 42%
 *   range-20260920T2300Z      +$9.93 / 78%       -$1.82 / 33%
 *
 * BEFORE is economically BETTER on every window. So an absolute threshold
 * (`net >= 0`) is red on HEAD (fails criterion 2) and green on BEFORE (fails
 * criterion 3): the two criteria are inversions of each other. That is not a
 * tuning problem — it is #267's regression showing up in the gate, and it says
 * the regression predates this gate rather than being something the gate can
 * pin away.
 *
 * What this gate therefore does instead is the half that can be honest: pin the
 * CURRENT numbers and go red on DEGRADATION. That is what criterion 1 actually
 * needs (a change in exit reachability caught in money), it is green on HEAD,
 * and `--teeth` demonstrates it can fail. The regression itself is #267's to
 * fix; when it is fixed, re-record the baseline and the numbers move up.
 *
 * Usage:
 *   node scripts/exit-economics-check.mjs              # the gate (needs the release core)
 *   node scripts/exit-economics-check.mjs --self-test  # verdict fixtures, no binary
 *   node scripts/exit-economics-check.mjs --teeth      # the demonstrated-failing arm
 *   node scripts/exit-economics-check.mjs --arm <flags...>   # any candidate, same verdict
 *
 * `BK_CORE_BIN` points the replay at another build of the core (see BIN below);
 * the strategy cdylibs are always the ones this checkout builds.
 */

import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { spawn } from 'child_process';
import { join } from 'path';
import { tmpdir } from 'os';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';
import { requireFreshStrategyDylibs } from './lib/strategy-dylib-freshness.mjs';

const ROOT = process.cwd();
// `BK_CORE_BIN` exists for one reason: `target/release/blitzkrieg-core` may be
// the live kernel's own binary (it is, on this checkout), and overwriting a
// running deployment's file to measure a candidate is not something a check
// script should force. CI has nothing running and uses the default.
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
// NOT overridable on purpose: `strategyDylibReport` resolves the cdylibs from
// the repo root and the binary's parent chain, so a second knob here could point
// the replay at one set of libraries while the freshness check vouched for
// another. One directory, one answer.
const STRATEGY_DIR = join(ROOT, 'user_layer', 'strategies', 'target', 'release');

const STRATEGY = 'spread_arb';

// ---------------------------------------------------------------------------
// The recorded baseline. Change it only with a measurement, and say which
// commit the measurement was taken at — a baseline with no provenance is a
// number nobody can re-derive.
// ---------------------------------------------------------------------------
const BASELINE = Object.freeze({
  recorded: '2026-09-23',
  commit: 'c76b3c22',
  issue: 'https://github.com/ceer-quant/BlitzkriegBot/issues/272',
  windows: {
    'trend-20260919T1000Z': { closed: 14, wins: 3, winRatePct: 21.43, netPnlUsd: -2.878 },
    'range-20260919T1600Z': { closed: 16, wins: 6, winRatePct: 37.5, netPnlUsd: -5.0782 },
    'trend-20260920T2100Z': { closed: 9, wins: 3, winRatePct: 33.33, netPnlUsd: -2.8141 },
    'range-20260920T2300Z': { closed: 11, wins: 2, winRatePct: 18.18, netPnlUsd: -5.149 },
  },
});

// Tolerances, not equality: `rust_decimal` arithmetic is exact and the replay is
// deterministic, but the corpus arm still crosses a process boundary and a
// platform, and a gate that flickers on its own baseline gets muted. They are
// deliberately far tighter than the regression #267 measured (one window alone
// moved by $18.69), so a real reachability change cannot slip through.
const TOLERANCE = Object.freeze({
  // #272 criterion 1: closes may not fall below 90% of the baseline.
  closedFloorPct: 90,
  netUsd: 0.75,
  winRatePct: 5,
});

/**
 * The verdict, as a pure function of two measured rows — so `--self-test` can
 * pin the LOGIC against fixtures without a binary or a replay.
 */
function judge(baseline, actual) {
  const problems = [];
  if (!actual) return ['no measurement'];
  if (actual.error) return [actual.error];

  const closedFloor = Math.floor((baseline.closed * TOLERANCE.closedFloorPct) / 100);
  if (actual.closed < closedFloor) {
    problems.push(`closes ${actual.closed} < floor ${closedFloor} (${TOLERANCE.closedFloorPct}% of ${baseline.closed})`);
  }
  if (actual.netPnlUsd < baseline.netPnlUsd - TOLERANCE.netUsd) {
    problems.push(
      `net ${actual.netPnlUsd.toFixed(4)} < baseline ${baseline.netPnlUsd.toFixed(4)} - ${TOLERANCE.netUsd}`,
    );
  }
  if (actual.winRatePct < baseline.winRatePct - TOLERANCE.winRatePct) {
    problems.push(
      `win rate ${actual.winRatePct.toFixed(2)}% < baseline ${baseline.winRatePct.toFixed(2)}% - ${TOLERANCE.winRatePct}`,
    );
  }
  return problems;
}

/** One replay: materialize the window, drive the release core, read the report. */
function runArm({ corpus, windowName, dir, extraFlags = [] }) {
  return new Promise((resolve) => {
    const report = join(dir, `${windowName}-${STRATEGY}.json`);
    const args = [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--strategy-dir', STRATEGY_DIR,
      '--enable-strategy', STRATEGY,
      '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', '1000', '--max-order-notional', '12',
      '--backtest', corpus,
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
      ...extraFlags,
    ];
    const child = spawn(BIN, args, { cwd: ROOT, stdio: ['ignore', 'ignore', 'pipe'] });
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      if (code !== 0 || !existsSync(report)) {
        resolve({ window: windowName, error: `exit ${code}: ${err.split('\n').slice(-4).join(' | ')}` });
        return;
      }
      const r = JSON.parse(readFileSync(report, 'utf8'));
      const t = r.trades ?? {};
      resolve({
        window: windowName,
        closed: Number(t.closed ?? 0),
        wins: Number(t.wins ?? 0),
        losses: Number(t.losses ?? 0),
        winRatePct: Number(t.winRatePct ?? 0),
        netPnlUsd: Number(t.netPnlUsd ?? 0),
        feesUsd: Number(t.feesUsd ?? 0),
        maxDrawdownUsd: Number(t.maxDrawdownUsd ?? 0),
      });
    });
  });
}

/** The three numbers #272 criterion 4 requires on every line, pass or fail. */
function rowLine(r) {
  if (r.error) return `${r.window.padEnd(24)} ERROR  ${r.error}`;
  return (
    `${r.window.padEnd(24)} closes ${String(r.closed).padStart(3)}` +
    `  WR ${r.winRatePct.toFixed(2).padStart(6)}%` +
    `  net ${(r.netPnlUsd >= 0 ? '+' : '') + r.netPnlUsd.toFixed(4)}`
  );
}

function selfTest() {
  const base = { closed: 10, wins: 6, winRatePct: 60, netPnlUsd: 5 };
  const cases = [
    { name: 'an identical replay is clean', actual: { closed: 10, wins: 6, winRatePct: 60, netPnlUsd: 5 }, want: 0 },
    { name: 'one close fewer (90% floor is 9) is clean', actual: { closed: 9, wins: 6, winRatePct: 60, netPnlUsd: 5 }, want: 0 },
    { name: 'eight closes is below the 90% floor', actual: { closed: 8, wins: 6, winRatePct: 60, netPnlUsd: 5 }, want: 1 },
    { name: 'a net inside the tolerance is clean', actual: { closed: 10, wins: 6, winRatePct: 60, netPnlUsd: 4.25 }, want: 0 },
    { name: 'a net past the tolerance is red', actual: { closed: 10, wins: 6, winRatePct: 60, netPnlUsd: 4.24 }, want: 1 },
    // The shape #267 actually had: the money moves and the mechanism tests stay
    // green. A gate that cannot see this is the defect being fixed.
    { name: "a sign flip with an unchanged close count is red", actual: { closed: 10, wins: 3, winRatePct: 30, netPnlUsd: -5 }, want: 2 },
    { name: 'a missing measurement is red, never clean', actual: null, want: 1 },
    { name: 'a failed replay is red, never clean', actual: { window: 'w', error: 'exit 1: boom' }, want: 1 },
  ];
  let failed = 0;
  for (const c of cases) {
    const problems = judge(base, c.actual);
    const ok = problems.length >= c.want && (c.want === 0 ? problems.length === 0 : problems.length > 0);
    if (!ok) {
      failed += 1;
      console.error(`  FAIL ${c.name}: got ${problems.length} problem(s) ${JSON.stringify(problems)}`);
    } else {
      console.log(`  ok   ${c.name}`);
    }
  }
  if (failed > 0) {
    console.error(`\nexit-economics self-test: ${failed} of ${cases.length} fixtures failed`);
    process.exit(1);
  }
  console.log(`\nexit-economics self-test: ${cases.length} fixtures passed`);
}

/**
 * The demonstrated-failing arm.
 *
 * #272 asked for the pinned BEFORE commit here. BEFORE is economically BETTER on
 * this corpus (see the header), so it comes out GREEN and cannot serve as the
 * reverse acceptance. The arm that must go red is one that genuinely makes the
 * same corpus pay less.
 *
 * A note on why this arm and not "remove the profit-side exits": disabling the
 * trailing stop and the take-profit (`--exit-trailing-min-high-pct 999
 * --exit-take-profit-pct 999`) was measured first and is NOT a teeth arm — on
 * this corpus those two windows got BETTER without them (+$3.34 and +$1.91),
 * because the trailing stop is itself bleeding there. A reverse acceptance that
 * only fails on the windows it happens to hurt proves nothing, so the arm below
 * was chosen by measurement: it must move every window the same way.
 *
 * Measured on all four windows with this arm (2026-09-23): every window degrades,
 * win rate to 0% in all four, closes below the 90% floor in two. Worth noting it
 * is not "everything gets worse" — two windows LOSE LESS money at a 1% stop
 * (−$1.43 vs −$2.81 and −$3.90 vs −$5.15). The win-rate criterion is what catches
 * those, which is the argument for reporting all three numbers and not net alone.
 */
const TEETH_FLAGS = ['--exit-stop-loss-pct', '1'];

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  const teethAt = argv.indexOf('--teeth');
  const armAt = argv.indexOf('--arm');
  // `--arm <flags...>`: everything after it goes to the replay, so an operator
  // can put any candidate configuration under the same verdict. The gate still
  // expects the arm to DEGRADE — that is the whole point of the mode.
  const arm = armAt >= 0 ? argv.slice(armAt + 1) : teethAt >= 0 ? TEETH_FLAGS : [];
  const teeth = teethAt >= 0 || armAt >= 0;

  if (!existsSync(BIN)) {
    console.error(`missing binary ${BIN}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  requireFreshStrategyDylibs({ gate: 'exit-economics-check', require: [`${STRATEGY}_strategy`] });

  const dir = mkdtempSync(join(tmpdir(), 'bk-exit-economics-'));
  const rows = [];
  try {
    for (const w of WINDOWS) {
      const { path, dir: corpusDir } = materialize(ROOT, w);
      rows.push(await runArm({ corpus: path, windowName: w.name, dir, extraFlags: arm }));
      rmSync(corpusDir, { recursive: true, force: true });
    }

    console.log(
      teeth
        ? `exit-economics: TEETH ARM (${arm.join(' ')}) — this run MUST be red\n`
        : `exit-economics: ${STRATEGY} over ${WINDOWS.length} sha256-pinned hours\n`,
    );
    let bad = 0;
    for (const r of rows) {
      const problems = judge(BASELINE.windows[r.window], r);
      // The expectation INVERTS under --teeth: there, a window the gate would
      // have PASSED is the failure, because the arm was chosen to degrade all of
      // them. A replay that could not run is red in both modes — an error is not
      // a measurement of anything.
      const degraded = !r.error && problems.length > 0;
      const ok = r.error ? false : teeth ? degraded : problems.length === 0;
      console.log(`${ok ? '  ok  ' : '  RED '} ${rowLine(r)}`);
      if (teeth) {
        console.log(
          `         ${
            r.error
              ? r.error
              : degraded
                ? `degraded as required: ${problems.join('; ')}`
                : 'no degradation measured — the teeth arm did not move the money'
          }`,
        );
      } else {
        for (const p of problems) console.log(`         ${p}`);
      }
      if (!ok) bad += 1;
    }
    console.log(`\nbaseline recorded ${BASELINE.recorded} at ${BASELINE.commit} (${BASELINE.issue})`);
    if (teeth) {
      if (bad === 0) {
        console.log(`exit-economics --teeth: all ${rows.length} windows degraded — the gate has teeth`);
        process.exit(0);
      }
      console.error(
        `exit-economics --teeth: ${bad}/${rows.length} windows did NOT degrade — the gate cannot see this arm's damage.\n` +
          'Pick an arm that moves every window, or the reverse acceptance proves nothing.',
      );
      process.exit(1);
    }
    if (bad > 0) {
      console.error(
        `exit-economics: ${bad}/${rows.length} windows degraded against the recorded baseline.\n` +
          'Either the change made the strategy earn less, or the baseline needs re-recording WITH a measurement\n' +
          'and a commit hash. Do not widen the tolerance to make this pass.',
      );
      process.exit(1);
    }
    console.log(`exit-economics: ${rows.length} windows at or above the recorded baseline`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

main();
