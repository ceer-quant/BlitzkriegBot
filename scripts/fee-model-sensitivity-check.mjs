#!/usr/bin/env node
/**
 * #203: what the taker-fee schedule COSTS the strategies — measured, not argued.
 *
 * The issue: `official` (Polymarket's published `0.07 * p * (1-p)`) is 2.3x-5.3x
 * the schedule the deployment line charges (`0.125*(p*(1-p))^2`) over the prices
 * the strategies actually trade, and fees are already 31.5% of gross PnL. So
 * "switch the fee model" is not a units fix — it is a cost increase of the same
 * order as the strategy's whole edge, and it can move the book from profitable
 * to unprofitable. #182 made the gates AGREE with the kernel about the fee; this
 * gate asks the other question, which nothing was asking: is the strategy still
 * viable at that fee?
 *
 * How it answers: the frozen corpus (#176's four sha256-pinned hours, shared via
 * `lib/frozen-corpus.mjs`) replayed with the SHIPPED binary under each schedule,
 * one arm per (window, strategy, schedule). Same events, same strategy config,
 * same fill model — the fee schedule is the only difference, because
 * `--fee-model` is the only knob that moves (#203, replay-only by construction).
 * Each report states the schedule it charged, so an arm whose report does not
 * name the schedule it was asked for is a broken counterfactual, not a result.
 *
 * What it decides: the DECISION record below, checked against the measurement in
 * both directions. That is the guard #203 asks for, and it is deliberately
 * two-sided: the gate is red if a strategy becomes unprofitable under the
 * candidate schedule AND red if the recorded reason for not switching stops
 * being true (every measured strategy would be profitable under it). A pin that
 * only fires one way would either be a permanent red or a note nobody re-reads.
 *
 * Modes:
 *   node scripts/fee-model-sensitivity-check.mjs                  # corpus, both schedules
 *   node scripts/fee-model-sensitivity-check.mjs --out <path>     # also write JSON
 *   node scripts/fee-model-sensitivity-check.mjs --self-test      # fixtures, no binary
 *   node scripts/fee-model-sensitivity-check.mjs --trades <jsonl> # re-price a real trade log
 *   node scripts/fee-model-sensitivity-check.mjs --build-corpus   # regenerate the corpus
 *
 * Needs: cargo build --release --workspace --locked
 *        (cd user_layer/strategies && cargo build --release)
 */
import { spawn } from './lib/child-guard.mjs';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import {
  TAKER_FEE_MODELS,
  PINNED_DEFAULT_MODEL,
  feeModelTableProblems,
  feePerShareAt,
} from './lib/fee-model.mjs';
import { WINDOWS, buildCorpus, materialize } from './lib/frozen-corpus.mjs';
import { requireFreshStrategyDylibs } from './lib/strategy-dylib-freshness.mjs';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const STRATEGY_DIR = join(ROOT, 'user_layer', 'strategies', 'target', 'release');
const ARCHIVE_DIR = join(ROOT, 'data', 'archive');

// ---------------------------------------------------------------------------
// The recorded decision (#203, 2026-09-21). Change it only with a measurement.
//
// `hold` means: the kernel keeps charging `legacy_quadratic` and the switch to
// `official` is NOT made, because on the frozen corpus every measured strategy
// is at or below zero expectancy under the published schedule (the table this
// gate prints). `heldFor` names the condition that must therefore still hold —
// the gate re-checks it every run, so the day that stops being true this goes
// red and the decision gets re-made instead of fossilising.
// ---------------------------------------------------------------------------
const DECISION = Object.freeze({
  outcome: 'hold',
  candidate: 'official',
  // Spelled out rather than read from PINNED_DEFAULT_MODEL: the point of the
  // check below is to notice that the PIN moved away from the schedule this
  // decision was made about. Reading it here would make the check vacuous.
  shipped: 'legacy_quadratic',
  heldFor: 'candidate-not-viable',
  decided: '2026-09-21',
  issue: 'https://github.com/ceer-quant/BlitzkriegBot/issues/203',
});

/** The schedules every run measures, in the order it reports them. Deduped: if the
 *  shipped schedule ever becomes the candidate, the two collapse to one and the
 *  verdict says the shipped arm was never measured (plus the pin guard fires)
 *  rather than silently measuring the same schedule twice. */
const DEFAULT_MODELS = [...new Set([PINNED_DEFAULT_MODEL, 'official'])];
/** The strategies every run measures. Kernel ships none: these are the cdylibs. */
const DEFAULT_STRATEGIES = ['mean_reversion', 'spread_arb'];

/** Prices the structural table in the decision record quotes (#203's own rows). */
const STRUCTURAL_PRICES = [0.12, 0.33, 0.39, 0.4, 0.45];

// ---------------------------------------------------------------------------
// The verdict — a pure function of the decision and the measurements, so the
// self-test can exercise it against hand-computed fixtures without a replay.
//
// `arms` is one entry per (strategy, schedule): { strategy, model, closed, wins,
// feesUsd, netPnlUsd, grossPnlUsd, expectancyUsd }.
// ---------------------------------------------------------------------------
export function verdictProblems({ decision, arms, pinnedDefault = PINNED_DEFAULT_MODEL }) {
  const problems = [];
  const byModel = (m) => arms.filter((a) => a.model === m);
  const traded = (a) => Number(a.closed) > 0;

  // 0. THE PIN. This decision is a decision about WHICH schedule the kernel
  //    charges, so the first thing to check is that the kernel still charges the
  //    one the decision was made about. `PINNED_DEFAULT_MODEL` is the single
  //    place the shipped schedule is declared (#182) — the moment it moves off
  //    `decision.shipped`, somebody has made the switch this record deferred,
  //    whatever the corpus says. Red here means: re-decide, or revert the pin.
  //
  //    `decision.pinned` lets a caller state the pin it is assuming; the
  //    self-test uses it so the fixtures pin the LOGIC and do not all go red
  //    merely because this repository's own pin moved. The shipped record omits
  //    it, so a real run reads the kernel's actual pin.
  const pinned = decision.pinned ?? pinnedDefault;
  if (pinned !== decision.shipped) {
    problems.push(
      `the kernel's pinned default is '${pinned}' but the #203 decision record is a ` +
      `'${decision.outcome}' on '${decision.shipped}' as of ${decision.decided}: the switch this decision ` +
      `deferred has been made without a re-decision. Update DECISION here (with the corpus numbers) and the ` +
      `decision record in docs/CAPACITY_AND_EQUITY.md, or revert the pin`,
    );
  }

  const shipped = byModel(decision.shipped);
  const candidate = byModel(decision.candidate);
  if (shipped.length === 0 || candidate.length === 0) {
    problems.push(
      `measured nothing for the decision's own schedules (${decision.shipped} / ${decision.candidate}) — ` +
      `the decision record names a schedule this run did not measure`,
    );
    return problems;
  }

  // 1. WIRING. If the candidate arm did not actually pay more fees than the
  //    shipped arm on the same events, `--fee-model` did not take effect and the
  //    rest of the table would be a comparison of two identical runs. This is a
  //    broken-instrument failure, reported before any economic verdict.
  for (const c of candidate.filter(traded)) {
    const s = shipped.find((x) => x.strategy === c.strategy);
    if (s === undefined || !traded(s)) continue;
    if (!(Number(c.feesUsd) > Number(s.feesUsd))) {
      problems.push(
        `wiring: ${c.strategy} paid $${Number(c.feesUsd).toFixed(4)} under ${decision.candidate} but ` +
        `$${Number(s.feesUsd).toFixed(4)} under ${decision.shipped} — the candidate schedule did not ` +
        `charge more, so the counterfactual did not take effect and no verdict follows from it`,
      );
    }
  }

  // 2. The economic question. Expectancy is net PnL per closed trade: the number
  //    that decides whether a strategy is worth running, and the number #203
  //    showed could change sign. A strategy that never traded is not evidence of
  //    anything and is excluded from both sets.
  const unviable = candidate.filter((a) => traded(a) && Number(a.expectancyUsd) <= 0);
  const viable = candidate.filter((a) => traded(a) && Number(a.expectancyUsd) > 0);

  if (decision.outcome === 'switch') {
    for (const a of unviable) {
      const s = shipped.find((x) => x.strategy === a.strategy);
      const was = s !== undefined && traded(s) ? `$${Number(s.netPnlUsd).toFixed(4)} (${decision.shipped})` : 'not measured';
      problems.push(
        `${a.strategy}: under '${a.model}' expectancy is $${Number(a.expectancyUsd).toFixed(4)}/trade ` +
        `(${a.closed} closed, net $${Number(a.netPnlUsd).toFixed(4)}, shipped schedule ${was}) — the decision ` +
        `record says switch, and this strategy would not survive the switch`,
      );
    }
    if (viable.length === 0) {
      problems.push(
        `the decision record says switch but NO measured strategy is viable under ${decision.candidate}; ` +
        `a switch decision needs at least one strategy with positive expectancy on the frozen corpus`,
      );
    }
  } else if (decision.outcome === 'hold') {
    // The recorded reason, re-checked: this is the half that keeps the decision
    // honest in the other direction. The day the candidate schedule is affordable
    // for every measured strategy, "hold" has no reason left and this goes red.
    if (decision.heldFor === 'candidate-not-viable' && unviable.length === 0) {
      problems.push(
        `the recorded reason for NOT switching no longer holds: every measured strategy has positive ` +
        `expectancy under '${decision.candidate}' (${viable.map((a) => `${a.strategy} $${Number(a.expectancyUsd).toFixed(4)}`).join(', ')}). ` +
        `Re-decide #203 against these numbers and update DECISION in this script — a hold whose reason ` +
        `has expired is a decision nobody is making any more`,
      );
    }
    for (const a of unviable) {
      const s = shipped.find((x) => x.strategy === a.strategy);
      const flip = s !== undefined && traded(s) && Number(s.netPnlUsd) > 0;
      console.log(
        `   [decision] ${a.strategy}: ${flip ? 'SIGN FLIP — ' : ''}net $${Number(a.netPnlUsd).toFixed(4)} under ` +
        `'${a.model}' (expectancy $${Number(a.expectancyUsd).toFixed(4)}/trade) — the reason '${decision.outcome}' holds`,
      );
    }
  } else {
    problems.push(`decision record: unknown outcome '${decision.outcome}' (expected 'hold' or 'switch')`);
  }

  return problems;
}

/** Per (strategy, model) aggregate over the measured windows. */
export function aggregate(rows) {
  const key = (r) => `${r.strategy}\u0000${r.model}`;
  const out = new Map();
  for (const r of rows) {
    const k = key(r);
    const a = out.get(k) ?? {
      strategy: r.strategy,
      model: r.model,
      closed: 0,
      wins: 0,
      losses: 0,
      feesUsd: 0,
      netPnlUsd: 0,
    };
    a.closed += Number(r.closed ?? 0);
    a.wins += Number(r.wins ?? 0);
    a.losses += Number(r.losses ?? 0);
    a.feesUsd += Number(r.feesUsd ?? 0);
    a.netPnlUsd += Number(r.netPnlUsd ?? 0);
    out.set(k, a);
  }
  return [...out.values()].map((a) => ({
    ...a,
    // Gross PnL = net + fees: the report's net already has the fee taken out, so
    // adding it back is the strategy's result before the cost that is at issue.
    grossPnlUsd: a.netPnlUsd + a.feesUsd,
    feesPctOfGross: a.netPnlUsd + a.feesUsd > 0 ? (a.feesUsd / (a.netPnlUsd + a.feesUsd)) * 100 : null,
    expectancyUsd: a.closed > 0 ? a.netPnlUsd / a.closed : 0,
    winRatePct: a.closed > 0 ? (a.wins / a.closed) * 100 : 0,
  }));
}

// ---------------------------------------------------------------------------
// Replays
// ---------------------------------------------------------------------------
function runArm({ corpus, windowName, strategy, model, dir }) {
  return new Promise((resolve) => {
    const report = join(dir, `${windowName}-${strategy}-${model}.json`);
    const args = [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--strategy-dir', STRATEGY_DIR,
      '--enable-strategy', strategy,
      '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', '1000', '--max-order-notional', '12',
      '--backtest', corpus,
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
      // The counterfactual (#203). Replay-only: the core refuses it without
      // `--backtest`, so a live run cannot be charged a schedule by accident.
      '--fee-model', model,
    ];
    const child = spawn(BIN, args, { cwd: ROOT, stdio: ['ignore', 'ignore', 'pipe'] });
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      if (code !== 0 || !existsSync(report)) {
        resolve({ window: windowName, strategy, model, error: `exit ${code}: ${err.split('\n').slice(-4).join(' | ')}` });
        return;
      }
      const r = JSON.parse(readFileSync(report, 'utf8'));
      // The report states the schedule it charged. An arm asked for one schedule
      // and reporting another is the instrument failing, not a result.
      const charged = r.feeSchedule?.name;
      if (charged !== model) {
        resolve({
          window: windowName,
          strategy,
          model,
          error: `the replay reported feeSchedule='${charged}' after being asked for '${model}' — the fee knob did not reach the charge path`,
        });
        return;
      }
      resolve({
        window: windowName,
        strategy,
        model,
        chargedRate: Number(r.feeSchedule.rate),
        chargedExponent: r.feeSchedule.exponent,
        feeSource: r.feeSchedule.source,
        closed: r.trades.closed,
        wins: r.trades.wins,
        losses: r.trades.losses,
        winRatePct: Number(r.trades.winRatePct),
        feesUsd: Number(r.trades.feesUsd),
        netPnlUsd: Number(r.trades.netPnlUsd),
        maxDrawdownUsd: Number(r.trades.maxDrawdownUsd),
        fills: Number(r.fills ?? 0),
      });
    });
  });
}

/** Structural table: fee per share (and % of price) at each price, both models. */
function structuralTable(models) {
  return STRUCTURAL_PRICES.map((p) => {
    const row = { price: p };
    for (const m of models) {
      const perShare = feePerShareAt(m, p);
      row[m] = { perShare, pctOfPrice: (perShare / p) * 100 };
    }
    if (models.length === 2) {
      row.ratio = row[models[1]].perShare / row[models[0]].perShare;
    }
    return row;
  });
}

function fmt(v, w = 10, d = 4) {
  return typeof v === 'number' && Number.isFinite(v) ? v.toFixed(d).padStart(w) : String(v).padStart(w);
}

function printAggregates(arms) {
  console.log('\n== measured on the frozen corpus (per strategy x schedule)');
  console.log('   strategy          model               closed  wins  win%      gross        fees     fees/gross%        net    exp/trade');
  for (const a of arms) {
    console.log(
      `   ${a.strategy.padEnd(17)} ${a.model.padEnd(18)} ${fmt(a.closed, 6, 0)} ${fmt(a.wins, 5, 0)} ` +
      `${fmt(a.winRatePct, 5, 1)} ${fmt(a.grossPnlUsd)} ${fmt(a.feesUsd)} ${fmt(a.feesPctOfGross, 11, 1)} ` +
      `${fmt(a.netPnlUsd)} ${fmt(a.expectancyUsd)}`,
    );
  }
  console.log('\n== structural: fee per share as a percentage of price (exact arithmetic)');
  console.log('   price   legacy_quadratic        official              ratio');
  for (const r of structuralTable([PINNED_DEFAULT_MODEL, 'official'])) {
    console.log(
      `   ${r.price.toFixed(2)}   ${fmt(r[PINNED_DEFAULT_MODEL].pctOfPrice, 6, 3)}%  ` +
      `$${fmt(r[PINNED_DEFAULT_MODEL].perShare, 8, 5)}   ${fmt(r.official.pctOfPrice, 6, 3)}%  ` +
      `$${fmt(r.official.perShare, 8, 5)}   ${r.ratio.toFixed(2)}x`,
    );
  }
}

// ---------------------------------------------------------------------------
// --trades: re-price an existing trade log under each schedule.
//
// The corpus replay says what the SAME strategy would do under a schedule. A
// trade log says what the strategy ALREADY did — production prices, production
// maker/taker mix — and re-pricing it shows where the fee dollars would land by
// price band. It holds the trades FIXED (the entries a higher fee would have
// changed are not re-decided here), so it is an accounting view of the real
// book, clearly labelled as such, not a replay.
// ---------------------------------------------------------------------------
function takerFeePctAt(model, price) {
  const perShare = feePerShareAt(model, price);
  return price > 0 ? (perShare / price) * 100 : 0;
}

export function repricedTrade(trade, models) {
  const out = {};
  const shares = Number(trade.shares ?? 0);
  const entry = Number(trade.entryPrice ?? 0);
  const exit = Number(trade.exitPrice ?? 0);
  for (const m of models) {
    const entryFee = trade.wasMakerEntry ? 0 : feePerShareAt(m, entry) * shares;
    const exitFee = trade.wasMakerExit ? 0 : feePerShareAt(m, exit) * shares;
    out[m] = { feesUsd: entryFee + exitFee };
  }
  return out;
}

function tradesReport(path, models) {
  const lines = readFileSync(path, 'utf8').split('\n').filter((l) => l.trim() !== '');
  const trades = lines.map((l) => JSON.parse(l));
  const bands = [
    { label: '0.05-0.20', lo: 0.0, hi: 0.2 },
    { label: '0.20-0.30', lo: 0.2, hi: 0.3 },
    { label: '0.30-0.40', lo: 0.3, hi: 0.4 },
    { label: '0.40-0.55', lo: 0.4, hi: 0.55 },
    { label: '0.55-1.00', lo: 0.55, hi: 1.01 },
  ];
  const rows = trades.map((t) => ({ t, re: repricedTrade(t, models) }));
  console.log(`\n== re-pricing ${trades.length} real trades from ${path} (trades held fixed)`);
  console.log('   entry band   trades  notionalUsd   grossPnl     fees(legacy)    fees(official)     net(legacy)    net(official)');
  const out = { trades: trades.length, bands: [] };
  for (const b of bands) {
    const inBand = rows.filter((r) => {
      const p = Number(r.t.entryPrice ?? 0);
      return p >= b.lo && p < b.hi;
    });
    if (inBand.length === 0) continue;
    const sum = (f) => inBand.reduce((a, r) => a + f(r), 0);
    const legacy = sum((r) => r.re[models[0]].feesUsd);
    const official = sum((r) => r.re[models[1]].feesUsd);
    const gross = sum((r) => Number(r.t.grossPnlUsd ?? 0));
    const notional = sum((r) => Number(r.t.costUsd ?? 0));
    const row = {
      band: b.label,
      trades: inBand.length,
      notionalUsd: notional,
      grossPnlUsd: gross,
      [models[0]]: legacy,
      [models[1]]: official,
      [`net_${models[0]}`]: gross - legacy,
      [`net_${models[1]}`]: gross - official,
    };
    out.bands.push(row);
    console.log(
      `   ${row.band.padEnd(12)} ${fmt(row.trades, 5, 0)} ${fmt(row.notionalUsd, 11, 2)} ${fmt(row.grossPnlUsd, 10, 2)} ` +
      `${fmt(row[models[0]], 13, 4)} ${fmt(row[models[1]], 17, 4)} ${fmt(row[`net_${models[0]}`], 14, 4)} ${fmt(row[`net_${models[1]}`], 15, 4)}`,
    );
  }
  const tot = (f) => out.bands.reduce((a, r) => a + f(r), 0);
  const total = {
    trades: out.trades,
    notionalUsd: tot((r) => r.notionalUsd),
    grossPnlUsd: tot((r) => r.grossPnlUsd),
    [models[0]]: tot((r) => r[models[0]]),
    [models[1]]: tot((r) => r[models[1]]),
  };
  total[`net_${models[0]}`] = total.grossPnlUsd - total[models[0]];
  total[`net_${models[1]}`] = total.grossPnlUsd - total[models[1]];
  console.log(
    `   TOTAL        ${fmt(total.trades, 5, 0)} ${fmt(total.notionalUsd, 11, 2)} ${fmt(total.grossPnlUsd, 10, 2)} ` +
    `${fmt(total[models[0]], 13, 4)} ${fmt(total[models[1]], 17, 4)} ${fmt(total[`net_${models[0]}`], 14, 4)} ${fmt(total[`net_${models[1]}`], 15, 4)}`,
  );
  out.total = total;
  return out;
}

// ---------------------------------------------------------------------------
// --self-test: the verdict and the table rules against hand-computed fixtures.
// No binary, no corpus — so the LOGIC is pinned even where a replay cannot run.
// ---------------------------------------------------------------------------
function selfTest() {
  const problems = [];
  const check = (name, ok, detail) => {
    if (!ok) problems.push(`${name}: ${detail}`);
    console.log(`   ${ok ? 'ok  ' : 'FAIL'} ${name}`);
  };
  const arm = (strategy, model, o = {}) => ({
    strategy,
    model,
    closed: o.closed ?? 10,
    wins: o.wins ?? 5,
    losses: o.losses ?? 5,
    feesUsd: o.feesUsd ?? 1,
    netPnlUsd: o.netPnlUsd ?? 1,
    grossPnlUsd: (o.netPnlUsd ?? 1) + (o.feesUsd ?? 1),
    expectancyUsd: ((o.netPnlUsd ?? 1) / (o.closed ?? 10)),
  });
  // `pinned` is stated per fixture: the self-test pins the LOGIC (does a moved
  // pin make it red?) and must not go red for the repository's own pin.
  const hold = { ...DECISION, outcome: 'hold', shipped: 'legacy_quadratic', candidate: 'official', heldFor: 'candidate-not-viable', pinned: 'legacy_quadratic' };
  const switchDec = { ...hold, outcome: 'switch' };

  // The pin guard: the decision says the kernel charges `legacy_quadratic`; if the
  // kernel has been repinned to the candidate, that IS the switch happening.
  const repinned = verdictProblems({
    decision: { ...hold, pinned: 'official' },
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16 }),
      arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -0.8, closed: 16 }),
    ],
    pinnedDefault: 'official',
  });
  check(
    'hold + pin moved to the candidate => RED (switch made without a re-decision)',
    repinned.some((p) => p.includes('has been made without a re-decision')),
    `got ${JSON.stringify(repinned)}`,
  );
  // ...and the same measurements with the pin still where the decision left it
  // are green, so the guard is red for the pin and not for the numbers.
  check(
    'hold + pin unchanged => green',
    verdictProblems({
      decision: hold,
      arms: [
        arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16 }),
        arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -0.8, closed: 16 }),
      ],
      pinnedDefault: 'legacy_quadratic',
    }).length === 0,
    'the pin guard must not fire on a matching pin',
  );

  // A hold whose reason holds: candidate unviable. The gate must be GREEN.
  check(
    'hold + candidate unviable => green',
    verdictProblems({
      decision: hold,
      arms: [
        arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16, wins: 5 }),
        arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -0.8, closed: 16, wins: 5 }),
      ],
    }).length === 0,
    'a hold with a live reason must not be red',
  );

  // The same measurements, but the candidate is now viable: the recorded reason
  // has expired and the gate must say so rather than stay quietly green.
  const expired = verdictProblems({
    decision: hold,
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16 }),
      arm('s', 'official', { feesUsd: 2.3, netPnlUsd: 2.0, closed: 16 }),
    ],
  });
  check('hold + candidate viable => RED (reason expired)', expired.some((p) => p.includes('no longer holds')), `got ${JSON.stringify(expired)}`);

  // A sign flip is named as such (it is the decision point), but a hold is not
  // made red BY it: the flip is the reason the hold exists. The red for a hold is
  // the reason expiring, checked in the next fixture.
  const flipped = verdictProblems({
    decision: hold,
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.82, closed: 16 }),
      arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -0.16, closed: 16 }),
    ],
  });
  check('hold + sign flip => green (the flip IS the reason)', flipped.length === 0, `got ${JSON.stringify(flipped)}`);

  // Holding is right for a second reason too: a strategy already losing money is
  // not made worse by a hold. Both negative => green.
  check(
    'hold + both schedules unviable => green',
    verdictProblems({
      decision: hold,
      arms: [
        arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: -0.5, closed: 16 }),
        arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -1.5, closed: 16 }),
      ],
    }).length === 0,
    'a hold on an already-unviable strategy has no expired reason',
  );

  // Wiring: a candidate arm that paid no more than the shipped one means the fee
  // knob never reached the charge path — reported as a broken instrument.
  const wiring = verdictProblems({
    decision: hold,
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 1.2, netPnlUsd: 0.8, closed: 16 }),
      arm('s', 'official', { feesUsd: 1.2, netPnlUsd: 0.8, closed: 16 }),
    ],
  });
  check('candidate charged no more => wiring red', wiring.some((p) => p.startsWith('wiring:')), `got ${JSON.stringify(wiring)}`);

  // A switch decision needs its own verification: the candidate must be viable.
  check(
    'switch + candidate viable => green',
    verdictProblems({
      decision: switchDec,
      arms: [
        arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16 }),
        arm('s', 'official', { feesUsd: 2.3, netPnlUsd: 0.3, closed: 16 }),
      ],
    }).length === 0,
    'a switch whose candidate is still profitable must be green',
  );
  const switchBad = verdictProblems({
    decision: switchDec,
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 1, netPnlUsd: 0.8, closed: 16 }),
      arm('s', 'official', { feesUsd: 2.3, netPnlUsd: -0.16, closed: 16 }),
    ],
  });
  check('switch + candidate unviable => red', switchBad.some((p) => p.includes('would not survive')), `got ${JSON.stringify(switchBad)}`);

  // A strategy that never traded cannot support any economic claim: it must not
  // be counted as evidence of viability (the fixture above has one arm with 0).
  const noTrades = verdictProblems({
    decision: switchDec,
    arms: [
      arm('s', 'legacy_quadratic', { feesUsd: 0, netPnlUsd: 0, closed: 0, wins: 0 }),
      arm('s', 'official', { feesUsd: 0, netPnlUsd: 0, closed: 0, wins: 0 }),
    ],
  });
  check('no trades => not viable evidence', noTrades.some((p) => p.includes('NO measured strategy is viable')), `got ${JSON.stringify(noTrades)}`);

  // The provenance requirement itself: a model with no source is a number
  // nobody can check, which is exactly how `0.07` arrived (#203).
  check('real model table has provenance', feeModelTableProblems().length === 0, JSON.stringify(feeModelTableProblems()));
  const stripped = { ghost: { rate: 0.07, exponent: 1 } };
  check(
    'a model without a source is rejected',
    feeModelTableProblems(stripped).some((p) => p.includes('no usable source')),
    JSON.stringify(feeModelTableProblems(stripped)),
  );
  check(
    'a model with a rate but no exponent is rejected',
    feeModelTableProblems({ ghost: { rate: 0.07, exponent: 0, source: 'https://example.invalid/x' } })
      .some((p) => p.includes('exponent')),
    'an unusable schedule must not pass the table check',
  );

  // Re-pricing: the arithmetic behind the per-band table, hand-computed.
  // 10 shares at 0.40, taker both legs, official: 2 * 10 * 0.07*0.4*0.6 = 0.336.
  const re = repricedTrade({ shares: 10, entryPrice: 0.4, exitPrice: 0.4, wasMakerEntry: false, wasMakerExit: false }, ['legacy_quadratic', 'official']);
  check('re-price: official 10sh@0.40 taker both legs = 0.336', Math.abs(re.official.feesUsd - 0.336) < 1e-9, JSON.stringify(re));
  // Maker legs are free: the same trade as a maker pays nothing under either.
  const reMaker = repricedTrade({ shares: 10, entryPrice: 0.4, exitPrice: 0.4, wasMakerEntry: true, wasMakerExit: true }, ['legacy_quadratic', 'official']);
  check('re-price: maker legs are free', reMaker.official.feesUsd === 0 && reMaker.legacy_quadratic.feesUsd === 0, JSON.stringify(reMaker));

  return problems;
}

// ---------------------------------------------------------------------------
async function main() {
  const argv = process.argv.slice(2);

  if (argv.includes('--self-test')) {
    console.log('== self-test: verdict logic, model table, re-pricing arithmetic (no binary)');
    const problems = selfTest();
    if (problems.length > 0) {
      console.error(`\nSELF-TEST FAILED (${problems.length})`);
      for (const p of problems) console.error(`  - ${p}`);
      process.exit(1);
    }
    console.log('\nself-test passed');
    return;
  }

  if (argv.includes('--build-corpus')) {
    const ai = argv.indexOf('--archive');
    await buildCorpus(ROOT, ai >= 0 && argv[ai + 1] ? argv[ai + 1] : ARCHIVE_DIR);
    return;
  }

  const models = (argv.includes('--models') ? argv[argv.indexOf('--models') + 1].split(',') : DEFAULT_MODELS);
  const strategies = (argv.includes('--strategies') ? argv[argv.indexOf('--strategies') + 1].split(',') : DEFAULT_STRATEGIES);
  const outIdx = argv.indexOf('--out');

  // The table's provenance is a precondition of every verdict below: a schedule
  // nobody can source is a schedule nobody can defend (#203 acceptance).
  const tableProblems = feeModelTableProblems();
  if (tableProblems.length > 0) {
    console.error('fee model table is not usable:');
    for (const p of tableProblems) console.error(`  - ${p}`);
    process.exit(2);
  }

  const tradePath = argv.includes('--trades') ? argv[argv.indexOf('--trades') + 1] : null;
  if (tradePath !== null) {
    const repriced = tradesReport(tradePath, models);
    if (outIdx >= 0 && argv[outIdx + 1]) {
      writeFileSync(argv[outIdx + 1], JSON.stringify({ decision: DECISION, repriced }, null, 2));
      console.log(`\nwrote ${argv[outIdx + 1]}`);
    }
    return;
  }

  if (!existsSync(BIN)) {
    console.error(`missing binary ${BIN}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  // #207: the strategies are cdylibs the kernel dlopens. A conclusion about
  // "the strategy" that ran a stale dylib is a conclusion about other code.
  requireFreshStrategyDylibs({
    gate: 'fee-model-sensitivity-check',
    require: strategies.map((s) => `${s}_strategy`),
  });

  const dir = mkdtempSync(join(tmpdir(), 'bk-fee-sensitivity-'));
  const rows = [];
  const structural = structuralTable(models);
  console.log(`fee schedules under test: ${models.join(', ')}  (decision: ${DECISION.outcome} on ${DECISION.candidate})`);
  console.log(`pinned default: ${PINNED_DEFAULT_MODEL}  models: ${Object.keys(TAKER_FEE_MODELS).join(', ')}`);

  for (const w of WINDOWS) {
    const corpus = materialize(ROOT, w);
    console.log(`\n== window ${w.name} (${w.regime}, sha256 ${corpus.sha.slice(0, 12)}…)`);
    for (const strategy of strategies) {
      for (const model of models) {
        const r = await runArm({ corpus: corpus.path, windowName: w.name, strategy, model, dir });
        if (r.error) {
          // A window that could not be replayed is a FAILURE, never a skip: the
          // aggregate would otherwise quietly be over fewer hours.
          console.error(`   ${strategy} / ${model}: ERROR ${r.error}`);
          process.exit(1);
        }
        rows.push({ ...r, regime: w.regime, sha256: corpus.sha });
        console.log(
          `   ${strategy.padEnd(15)} ${model.padEnd(16)} closed ${String(r.closed).padStart(3)} ` +
          `net ${r.netPnlUsd.toFixed(4).padStart(9)} fees ${r.feesUsd.toFixed(4).padStart(8)} fills ${String(r.fills).padStart(4)}`,
        );
      }
    }
  }

  const arms = aggregate(rows);
  printAggregates(arms);

  const problems = verdictProblems({ decision: DECISION, arms });
  const result = {
    decision: DECISION,
    generated: new Date().toISOString(),
    models,
    strategies,
    windows: WINDOWS.map((w) => ({ name: w.name, regime: w.regime, sha256: w.sha256 })),
    rows,
    arms,
    structural,
    problems,
  };
  if (outIdx >= 0 && argv[outIdx + 1]) {
    writeFileSync(argv[outIdx + 1], JSON.stringify(result, null, 2));
    console.log(`\nwrote ${argv[outIdx + 1]}`);
  }

  if (problems.length > 0) {
    console.error(`\nFEE-MODEL SENSITIVITY FAILED (${problems.length})`);
    for (const p of problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  const holding = arms.filter((a) => a.model === DECISION.candidate && a.closed > 0);
  console.log(
    `\nFEE-MODEL SENSITIVITY OK — decision '${DECISION.outcome}' on '${DECISION.candidate}' still holds: ` +
    `${holding.map((a) => `${a.strategy} expectancy $${a.expectancyUsd.toFixed(4)}`).join(', ')}`,
  );
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
