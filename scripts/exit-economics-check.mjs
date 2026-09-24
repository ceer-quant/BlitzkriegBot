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
 * BEFORE reads better on every window — with a caveat that changes what it means:
 * `d5c7fec3` predates `96fcf51c` (#171, "honest dry FOK walk + escalation
 * repricing"), so its maker-timeout escalation leg is booked at the MAKER price
 * where a real venue fills it as a taker at the deepest ask. #262 says that rule
 * inflates exactly these numbers and that "历史数字不能当基线，更不能当验收依据".
 * BEFORE also predates #261's escalation-abort fix. So BEFORE is not a clean
 * baseline: part of the gap is the booking model becoming honest, and "BEFORE
 * earned more" is not on its own proof that HEAD lost money.
 *
 * That does not change what this gate can do. An absolute threshold (`net >= 0`)
 * is red on HEAD (fails criterion 2) and green on BEFORE (fails criterion 3), so
 * the two criteria are inversions of each other either way — and that is not a
 * tuning problem, it is the economic question #267 is still open on.
 *
 * What this gate therefore does is the half that can be honest: pin the CURRENT
 * numbers and go red on DEGRADATION. That is what criterion 1 actually needs (a
 * change in exit reachability caught in money), it is green on HEAD, and
 * `--teeth` demonstrates it can fail. When #267 re-records a baseline on the
 * honest booking, the numbers move — the mechanism does not.
 *
 * Usage:
 *   node scripts/exit-economics-check.mjs              # the gate (needs the release core)
 *   node scripts/exit-economics-check.mjs --self-test  # verdict fixtures, no binary
 *   node scripts/exit-economics-check.mjs --teeth      # the demonstrated-failing arms
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
//
// RE-RECORDED 2026-09-24, and the measurement that moved it is the point.
//
// The 2026-09-23 recording (c76b3c22) was taken with FIVE cdylibs in
// STRATEGY_DIR. #296 deleted four of them along with the kernel's built-in
// strategies, so the directory this gate measures holds one library now and
// the old numbers are unreachable by construction — not because anything got
// worse. The kernel did not change: the same HEAD core, handed the old
// five-library directory, reproduces the old row bit-for-bit
// (14/21.43%/-2.878, 16/37.50%/-5.0782, 9/33.33%/-2.8141, 11/18.18%/-5.149).
//
// The delta is the sibling set, and it is not benign: a registered-but-DISABLED
// `trend_follow` still cancels this strategy's resting bids through
// `drain_breaks` (engine.rs:516), which is why the two range windows move and
// the two trend windows do not. That coupling is #302. Until it is fixed this
// baseline is only meaningful for the one-library directory named above — the
// number this gate produces depends on what is sitting in the directory, not
// only on what is enabled.
// ---------------------------------------------------------------------------
const BASELINE = Object.freeze({
  recorded: '2026-09-24',
  commit: '75fd8140',
  issue: 'https://github.com/ceer-quant/BlitzkriegBot/issues/272',
  windows: {
    'trend-20260919T1000Z': { closed: 14, wins: 3, winRatePct: 21.43, netPnlUsd: -2.878 },
    'range-20260919T1600Z': { closed: 17, wins: 7, winRatePct: 41.18, netPnlUsd: -4.7336 },
    'trend-20260920T2100Z': { closed: 9, wins: 3, winRatePct: 33.33, netPnlUsd: -2.8141 },
    'range-20260920T2300Z': { closed: 14, wins: 0, winRatePct: 0, netPnlUsd: -10.9323 },
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
 * The demonstrated-failing arms.
 *
 * #272 asked for the pinned BEFORE commit here. BEFORE is economically BETTER on
 * this corpus (see the header), so it comes GREEN and cannot serve as the
 * reverse acceptance. The arms that must go red are ones that genuinely make the
 * same corpus pay less.
 *
 * A note on why these and not "remove the profit-side exits": disabling the
 * trailing stop and the take-profit (`--exit-trailing-min-high-pct 999
 * --exit-take-profit-pct 999`) was measured first and is NOT a teeth arm — on
 * this corpus those two windows got BETTER without them, because the trailing
 * stop is itself bleeding there.
 *
 * WHY TWO ARMS AND NOT ONE. Re-measured 2026-09-24 against the baseline recorded
 * above (the one-library directory). A single `--exit-stop-loss-pct 1` used to
 * degrade all four windows because the WIN-RATE criterion caught the ones where
 * net improved. That criterion is dead on window 4: at the shipped stop its
 * fixture closes 14 trades and wins none, so `winRatePct` is already 0 and cannot
 * fall 5pp below itself. Net is the only lever left there, and a 1% stop makes
 * window 4 lose LESS (-4.9109 against -10.9323) — it is a tighter cap on a window
 * that is already all-losers. The arm that does move window 4 is the opposite
 * sabotage: no stop at all, so losers ride to resolution.
 *
 *   window                  stop 1%                  no stop
 *   trend-20260919T1000Z    net -4.6234    CATCH     net -15.7398   CATCH
 *   range-20260919T1600Z    closes 13 < 15 CATCH     net  +4.7000   miss
 *   trend-20260920T2100Z    closes  6 <  8 CATCH     net  -7.5369   CATCH
 *   range-20260920T2300Z    net -4.9109    miss      net -12.6494   CATCH
 *
 * Window 2's miss under "no stop" is the same fact from the other side: its
 * entries genuinely win (21 closes, 80.95%), so removing the loss cap makes that
 * window PROFITABLE. No single exit knob moves all four, and that is a property
 * of the fixture, not of the gate.
 *
 * So the property this mode proves is the one the gate actually needs: every
 * window's fixture is sensitive to exit damage, each demonstrated by a measured
 * arm. "One arm degrades all four" was the stronger claim and it is no longer
 * available. A window NO arm can move is the real failure, and there is none — if
 * a future edit produces one, this mode goes red and names it.
 */
const TEETH_ARMS = [
  {
    label: 'profit side unreachable (stop out at 1%)',
    flags: ['--exit-stop-loss-pct', '1'],
  },
  {
    label: 'loss side unreachable (no stop, ride to resolution)',
    flags: ['--exit-stop-loss-pct', '100'],
  },
];

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  const teethAt = argv.indexOf('--teeth');
  const armAt = argv.indexOf('--arm');
  // `--arm <flags...>`: everything after it goes to the replay, so an operator
  // can put any candidate configuration under the same verdict. The gate still
  // expects the arm to DEGRADE — that is the whole point of the mode.
  const arm = armAt >= 0 ? argv.slice(armAt + 1) : [];
  const teeth = teethAt >= 0 || armAt >= 0;
  // `--teeth` sweeps every recorded arm (see TEETH_ARMS); `--arm` puts one
  // operator-supplied candidate under the same verdict.
  const arms = teethAt >= 0 ? TEETH_ARMS : [{ label: arm.join(' '), flags: arm }];

  if (!existsSync(BIN)) {
    console.error(`missing binary ${BIN}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  requireFreshStrategyDylibs({ gate: 'exit-economics-check', require: [`${STRATEGY}_strategy`] });

  const dir = mkdtempSync(join(tmpdir(), 'bk-exit-economics-'));
  const measured = [];
  try {
    for (const w of WINDOWS) {
      const runs = [];
      for (const a of arms) {
        const { path, dir: corpusDir } = materialize(ROOT, w);
        const row = await runArm({ corpus: path, windowName: w.name, dir, extraFlags: a.flags });
        rmSync(corpusDir, { recursive: true, force: true });
        runs.push({ arm: a, row, problems: judge(BASELINE.windows[w.name], row) });
      }
      measured.push({ runs });
    }

    if (teeth) {
      console.log('exit-economics: TEETH ARMS — this run MUST be red on every window\n');
      for (const a of arms) console.log(`  ${a.label}: ${a.flags.join(' ')}`);
      console.log('');
    } else {
      console.log(`exit-economics: ${STRATEGY} over ${WINDOWS.length} sha256-pinned hours\n`);
    }

    let bad = 0;
    for (const { runs } of measured) {
      // The expectation INVERTS under --teeth: there, a window the gate would
      // have PASSED is the failure. A replay that could not run is red in both
      // modes — an error is not a measurement of anything.
      if (!teeth) {
        const { row, problems } = runs[0];
        const ok = !row.error && problems.length === 0;
        console.log(`${ok ? '  ok  ' : '  RED '} ${rowLine(row)}`);
        if (row.error) console.log(`         ${row.error}`);
        for (const p of problems) console.log(`         ${p}`);
        if (!ok) bad += 1;
        continue;
      }
      const caught = runs.filter((r) => !r.row.error && r.problems.length > 0);
      const errored = runs.filter((r) => r.row.error);
      const ok = caught.length > 0;
      console.log(`${ok ? '  ok  ' : '  RED '} ${rowLine((caught[0] ?? runs[0]).row)}`);
      if (ok) {
        for (const c of caught) {
          console.log(`         caught by ${c.arm.label}: ${c.problems.join('; ')}`);
        }
      } else if (errored.length === runs.length) {
        console.log(`         ${errored[0].row.error}`);
      } else {
        console.log('         no arm moved this window — the gate is blind to exit damage here');
      }
      if (!ok) bad += 1;
    }
    console.log(`\nbaseline recorded ${BASELINE.recorded} at ${BASELINE.commit} (${BASELINE.issue})`);
    if (teeth) {
      if (bad === 0) {
        console.log('exit-economics --teeth: every window was moved by at least one arm — the gate has teeth');
        process.exit(0);
      }
      console.error(
        `exit-economics --teeth: ${bad}/${measured.length} windows were moved by NO arm — the gate is blind there.\n` +
          'Add an arm that moves the window, or the reverse acceptance proves nothing.',
      );
      process.exit(1);
    }
    if (bad > 0) {
      console.error(
        `exit-economics: ${bad}/${measured.length} windows degraded against the recorded baseline.\n` +
          'Either the change made the strategy earn less, or the baseline needs re-recording WITH a measurement\n' +
          'and a commit hash. Do not widen the tolerance to make this pass.',
      );
      process.exit(1);
    }
    console.log(`exit-economics: ${measured.length} windows at or above the recorded baseline`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

main();
