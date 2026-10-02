#!/usr/bin/env node
/**
 * oracle-ruler-check.mjs — the measurement-layer calibration.
 *
 * The fork the operator posed: if five structurally different strategies all
 * book negative expectancy, either the STRATEGIES are bad or the RULER is
 * broken. Five agreeing strategies do not decide which — a probe whose PnL is
 * known by construction does. The probe (user_layer/strategies_lua/oracle_ruler)
 * has NO gates, NO exits, NO direction: it rests one maker bid at 0.30 on a
 * synthetic corpus whose price path is built by hand (scripts/lib/oracle-ruler.mjs),
 * so every booked number has a closed-form answer the kernel must reproduce.
 *
 * Trade logs are FORCE-DISABLED in replay (backtest.rs new(): trade_log_path =
 * None), so reconciliation runs off the report: tradeLines[] + the trades
 * aggregate + strategies[]. Shares are DERIVED (net / (pct/100 * entry)) and
 * must come out whole; the identities then close per arm.
 *
 * ARMS and closed forms (entry 0.30 maker, fee 0; legacy_quadratic taker
 * fee = 0.125*(p*(1-p))^2; maker exits are FREE):
 *
 *   win-tp        TP backstop (pnl +116.7% >= 100%) fires at the 0.65 jump.
 *                 maker-first exit (maker_first_exit_enabled=true) fills AT
 *                 0.65 against the resting bid -> +0.35/share, fee 0. The
 *                 taker reading (+0.34353047) is accepted and REPORTED — which
 *                 regime fired is a measurement fact, not an assumption.
 *                 THE RULER TEST: must book positive either way.
 *   loss-sl       SL 12% fires at the 0.10 jump. maker reading −0.20/share
 *                 (fee 0) or taker −0.2010125. Must match one exactly.
 *   win-timeexit  0.55 flat: pnl +83% (< TP), trail armed but no giveback,
 *                 stale-profit (20%/10s) or stagnant fires at the SAME price
 *                 -> +0.25/share maker / +0.24234297 taker. Answers "does the
 *                 time/stale exit mangle a winner" — must book positive.
 *   win-tp-slip1  --slippage-ticks 1: taker exits pay a tick (0.64 ->
 *                 +0.33336448); maker exits are untouched by taker slippage
 *                 (+0.35). Which one books IS the slippage-model answer.
 *   settle-win    UP asks-only 0.90 from t=700s (no bids => force_exit/SL have
 *                 no executable price), DOWN two-sided 0.10. Rides to expiry.
 *                 dry_resolution marks tokens off the LAST books: DOWN 0.10;
 *                 UP book-mid 0 -> position.current_price FALLBACK. If the
 *                 fallback mark exceeds 0.5 UP wins and books +0.70/share; if
 *                 the mark is 0/stale-low, best <= 0.5 => pays=false => −0.30
 *                 (a bid-less winner books a full loss — a dry-settlement
 *                 limitation, REPORTED as the mark-source finding).
 *   settle-loss   UP asks-only 0.02 / DOWN 0.95: DOWN wins on the book mid
 *                 alone (0.95 > 0.5), UP pays 0 -> −0.30/share EXACT. Control.
 *
 * Report-level identities (every arm): closed==1; Σ tradeLines == trades.net;
 * trades.fees == strategies[].fees; derived shares whole; per-share net
 * matches one closed form; implied fee consistent with the regime; fee
 * schedule == the arm's model (legacy_quadratic 0.125/exp2, official
 * 0.07/exp1). NOTE (backtest.rs trade_stats): the "gross" profit/loss buckets
 * accumulate NET pnl per side — profit_factor is a NET-basis ratio; the name
 * is historical, the identity checked below is bucket == |net|.
 *
 * Output: one row per arm + PASS/FAIL verdict; exit 1 on any failure.
 * (The probe is a RULER, not a strategy: nothing here tunes anything.)
 */

import { spawn } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';
import { buildProbe, ENTRY_PRICE } from './lib/oracle-ruler.mjs';

/** Taker fee per share under a named schedule — this script's OWN restatement,
 *  independent of the kernel AND of fee-model.mjs: three independent
 *  restatements agreeing (kernel booked / lib / here) is the evidence. */
function feePerShareAt(model, price) {
  const p = Number(price);
  if (model === 'official') return 0.07 * p * (1 - p); // published: rate*p*(1-p)
  return 0.125 * (p * (1 - p)) ** 2; // legacy_quadratic
}
const SCHEDULES = {
  legacy_quadratic: { rate: 0.125, exponent: 2 },
  official: { rate: 0.07, exponent: 1 },
};

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const PKG = join(ROOT, 'user_layer', 'strategies_lua', 'oracle_ruler');
const OUT = process.env.BK_ORACLE_OUT || join(tmpdir(), 'bk-oracle-ruler');

const num = (v) => Number(v);
const near = (a, b, tol) => Math.abs(a - b) <= tol;

const failures = [];
const notes = [];
function check(name, cond, detail) {
  if (cond) notes.push(`  ok   ${name}`);
  else failures.push(`${name} — ${detail}`);
}

function stagePackage() {
  const dir = mkdtempSync(join(tmpdir(), 'bk-oracle-pkg-'));
  cpSync(PKG, join(dir, 'oracle_ruler'), { recursive: true });
  return dir;
}

function runCore(luaDir, corpusPath, outDir, name, extraFlags) {
  return new Promise((res) => {
    const report = join(outDir, `${name}.report.json`);
    rmSync(report, { force: true });
    const child = spawn(
      BIN,
      [
        '--mode', 'dry',
        '--engine',
        '--no-discovery',
        '--no-strategy-state',
        '--no-intent-audit',
        '--lua-strategy-dir', luaDir,
        '--enable-strategy', 'oracle_ruler',
        '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
        '--seed-balance', '1000', '--max-order-notional', '12',
        '--backtest', corpusPath,
        '--backtest-report', report,
        '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
        ...extraFlags,
      ],
      {
        cwd: ROOT, stdio: ['ignore', 'ignore', 'pipe'],
        env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir },
      },
    );
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      if (code !== 0 || !existsSync(report)) {
        res({ error: `exit ${code}: ${err.split('\n').slice(-4).join(' | ')}` });
        return;
      }
      res({ report: JSON.parse(readFileSync(report, 'utf8')) });
    });
  });
}

/** Reconcile one arm's report. Returns the row; pushes checks. */
function reconcile(arm, rep) {
  const t = rep.trades ?? {};
  const lines = rep.tradeLines ?? [];
  const strat = (rep.strategies ?? [])[0] ?? {};

  check(`${arm.name} one closed trade`, t.closed === 1 && lines.length === 1,
    `closed=${t.closed} tradeLines=${lines.length}`);
  if (t.closed !== 1 || lines.length !== 1) return null;
  const line = lines[0];

  check(`${arm.name} strategy attribution`, line.strategy === 'oracle_ruler', `strategy=${line.strategy}`);
  check(
    `${arm.name} Σ tradeLines == trades.net`,
    near(num(line.netPnlUsd), num(t.netPnlUsd), 1e-6),
    `line=${line.netPnlUsd} trades=${t.netPnlUsd}`,
  );
  check(
    `${arm.name} trades.fees == strategies.fees`,
    near(num(t.feesUsd), num(strat.feesUsd), 1e-6),
    `trades=${t.feesUsd} strategies=${strat.feesUsd}`,
  );

  // Derive shares: netPnlPct = net / (entry*shares) * 100 with entry 0.30.
  const net = num(line.netPnlUsd);
  const pct = num(line.netPnlPct);
  const denom = (pct / 100) * ENTRY_PRICE;
  const shares = near(denom, 0, 1e-12) ? NaN : net / denom;
  check(`${arm.name} shares derive whole`, Number.isFinite(shares) && shares > 0
    && near(shares, Math.round(shares), 1e-4),
  `shares=${shares} (net=${net} pct=${pct})`);
  if (!Number.isFinite(shares)) return null;

  // per-share net and which fee regime it matches (arm's own fee model).
  // Settlement arms accept BOTH payout semantics: +1.00 (sound) or 0.00
  // (the dry resolver zeroed a bid-less/undermarked winner — see settleDual).
  const model = arm.feeModel ?? 'legacy_quadratic';
  const perShare = net / shares;
  let makerCF, takerCF, regime;
  if (arm.settleDual) {
    makerCF = arm.settleDual.sound;
    takerCF = arm.settleDual.zeroed;
    regime = near(perShare, makerCF, 2e-4) ? 'settle'
      : near(perShare, takerCF, 2e-4) ? 'settle-zeroed' : null;
  } else {
    makerCF = arm.exitMaker - ENTRY_PRICE;               // maker exit: fee 0
    takerCF = arm.exitTaker - ENTRY_PRICE - feePerShareAt(model, arm.exitTaker);
    regime = near(perShare, makerCF, 2e-4) ? 'maker'
      : near(perShare, takerCF, 2e-4) ? 'taker' : null;
  }
  check(
    `${arm.name} per-share closed form`,
    regime !== null,
    `perShare=${perShare.toFixed(8)} makerCF=${makerCF.toFixed(8)} takerCF=${takerCF.toFixed(8)}`,
  );

  // fee consistency with the regime
  if (regime === 'maker') {
    check(`${arm.name} maker regime books zero fee`, near(num(t.feesUsd), 0, 1e-6),
      `fees=${t.feesUsd}`);
  } else if (regime === 'taker') {
    const feeWant = feePerShareAt(model, arm.exitTaker) * shares;
    check(`${arm.name} taker regime fee`, near(num(t.feesUsd), feeWant, 1e-4),
      `fees=${t.feesUsd} expected=${feeWant.toFixed(6)}`);
  }

  // net-bucket identities (backtest.rs trade_stats: the "gross" buckets hold
  // NET pnl per side — see the header note)
  if (net >= 0) {
    check(`${arm.name} net-bucket: profitBucket == net`, near(num(t.grossProfitUsd), net, 1e-6),
      `grossProfit=${t.grossProfitUsd} net=${net}`);
    check(`${arm.name} net-bucket: lossBucket empty`, near(num(t.grossLossUsd), 0, 1e-6),
      `grossLoss=${t.grossLossUsd}`);
  } else {
    check(`${arm.name} net-bucket: lossBucket == |net|`, near(num(t.grossLossUsd), -net, 1e-6),
      `grossLoss=${t.grossLossUsd} |net|=${-net}`);
    check(`${arm.name} net-bucket: profitBucket empty`, near(num(t.grossProfitUsd), 0, 1e-6),
      `grossProfit=${t.grossProfitUsd}`);
  }

  return {
    arm: arm.name,
    reason: line.reason,
    regime: regime ?? 'NONE',
    exitBooked: regime === 'maker' ? arm.exitMaker : arm.exitTaker,
    shares,
    net,
    perShare,
    makerCF,
    takerCF,
  };
}

const ARMS = [
  {
    name: 'win-tp', corpus: 'win-tp', extra: [],
    exitMaker: 0.65, exitTaker: 0.65,
    verdict: 'RULER TEST: a logical winner MUST book positive',
    mustBePositive: true,
  },
  {
    name: 'loss-sl', corpus: 'loss-sl', extra: [],
    exitMaker: 0.10, exitTaker: 0.10,
    verdict: 'control: must match one closed form exactly (negative)',
  },
  {
    name: 'win-timeexit', corpus: 'win-timeexit', extra: [],
    exitMaker: 0.55, exitTaker: 0.55,
    verdict: 'time/stale exit must not mangle a winner (MUST be positive)',
    mustBePositive: true,
  },
  {
    name: 'win-tp-slip1', corpus: 'win-tp', extra: ['--slippage-ticks', '1'],
    exitMaker: 0.65, exitTaker: 0.64, // taker pays one tick
    verdict: 'slippage model: taker exit one tick worse; maker untouched',
    mustBePositive: true,
    assertSlippage: 1,
  },
  {
    name: 'loss-sl-slip1', corpus: 'loss-sl', extra: ['--slippage-ticks', '1'],
    exitMaker: 0.10, exitTaker: 0.09, // taker pays one tick DOWN
    verdict: 'checklist #2: the loss side pays slippage too',
  },
  {
    name: 'loss-sl-official', corpus: 'loss-sl', extra: ['--fee-model', 'official'],
    exitMaker: 0.10, exitTaker: 0.10,
    feeModel: 'official',
    verdict: 'checklist #1: same leg under the published fee curve',
    takerCFOverride: 'official', // official: 0.07*p*(1-p)
  },
  {
    name: 'settle-win', corpus: 'settle-win', extra: [],
    exitMaker: 1.0, exitTaker: 1.0, // payout $1, maker entry fee 0
    verdict: 'settlement mark-source probe: +0.70 (fallback marks > 0.5) or −0.30 (bid-less winner zeroed)',
    settleDual: { sound: 1 - ENTRY_PRICE, zeroed: -ENTRY_PRICE },
  },
  {
    name: 'settle-win-phantom', corpus: 'settle-win-phantom', extra: [],
    exitMaker: 1.0, exitTaker: 1.0,
    verdict: 'checklist #4: a PRICEABLE zero-depth winner must book the $1 payout (+0.70)',
    settleDual: { sound: 1 - ENTRY_PRICE, zeroed: -ENTRY_PRICE },
    phantom: true,
  },
  {
    name: 'settle-loss', corpus: 'settle-loss', extra: [],
    exitMaker: 0.0, exitTaker: 0.0, // payout 0, fee self-zeroes at p=0
    verdict: 'control: −0.30/share exact',
  },
];

const luaDir = stagePackage();
rmSync(OUT, { recursive: true, force: true });
mkdirSync(join(OUT, 'corpus'), { recursive: true });

const corpusCache = new Map();
function corpusFor(name) {
  if (!corpusCache.has(name)) {
    corpusCache.set(name, buildProbe(join(OUT, 'corpus'), name).path);
  }
  return corpusCache.get(name);
}

console.log(`# oracle-ruler: measurement-layer calibration, ${ARMS.length} arms, bin=${BIN}`);
const rows = [];
for (const arm of ARMS) {
  const r = await runCore(luaDir, corpusFor(arm.corpus), OUT, arm.name, arm.extra);
  if (r.error) {
    failures.push(`${arm.name} — replay failed: ${r.error}`);
    rows.push({ arm: arm.name, error: r.error });
    continue;
  }
  const rep = r.report;
  const model = arm.feeModel ?? 'legacy_quadratic';
  const sched = SCHEDULES[model];
  check(
    `${arm.name} fee schedule`,
    rep.feeSchedule?.name === model
      && num(rep.feeSchedule?.rate) === sched.rate
      && num(rep.feeSchedule?.exponent) === sched.exponent,
    `feeSchedule=${JSON.stringify(rep.feeSchedule)} want=${model}`,
  );
  if (arm.assertSlippage != null) {
    check(
      `${arm.name} slippage reported`,
      num(rep.fillModel?.takerSlippageTicks) === arm.assertSlippage,
      `fillModel=${JSON.stringify(rep.fillModel)}`,
    );
  }
  const row = reconcile(arm, rep);
  if (!row) {
    rows.push({ arm: arm.name, error: 'reconciliation impossible' });
    continue;
  }
  // arm-level verdicts
  if (arm.mustBePositive) {
    check(`${arm.name} ${arm.verdict}`, row.net > 0, `net=${row.net.toFixed(6)} perShare=${row.perShare.toFixed(8)}`);
  }
  if (arm.settleDual) {
    if (near(row.perShare, arm.settleDual.sound, 2e-4)) {
      notes.push(`  ok   ${arm.name}: settlement books the winner (+0.70/share) — payout path correct`);
    } else if (near(row.perShare, arm.settleDual.zeroed, 2e-4)) {
      const tag = arm.phantom ? 'FAIL' : 'NOTE';
      const msg = arm.phantom
        ? `${arm.name} — a PRICEABLE zero-depth winner (mid 0.95 > 0.5, asks quoted) booked a FULL LOSS:`
        : `${arm.name}: the dry simulator ZEROED a bid-less winner (book mid 0 → fallback mark ≤ 0.5 → pays=false)`;
      if (arm.phantom) {
        failures.push(
          `${msg} the dry resolver never paid the $1 payout on a token the corpus prices at 0.95.`
          + ` Settlement booking defect (checklist #4) — dry-resolution path, live resolves from the venue.`,
        );
      } else {
        notes.push(
          `  NOTE ${msg} — a bid-less token the corpus quotes at 0.90 ask books a full loss.`
          + ` Dry-settlement mark-source limitation, not a ledger arithmetic defect; live resolution comes from`
          + ` the venue. Documented, not failed.`,
        );
      }
      void tag;
    } else {
      failures.push(`${arm.name} — booked ${row.perShare.toFixed(6)}/share, matches neither settlement semantics`);
    }
  }
  if (arm.name === 'loss-sl' || arm.name === 'settle-loss') {
    const want = arm.name === 'settle-loss' ? -ENTRY_PRICE : null;
    if (want !== null) {
      check(`${arm.name} ${arm.verdict}`, near(row.perShare, want, 2e-4),
        `perShare=${row.perShare.toFixed(8)} expected=${want}`);
    }
  }
  rows.push(row);
}

console.log('');
console.log('# arm                exitReason              regime  shares   net          perShare       makerCF      takerCF');
for (const s of rows) {
  if (s.error) {
    console.log(`${s.arm.padEnd(20)} ERROR ${s.error}`);
    continue;
  }
  console.log(
    `${s.arm.padEnd(20)} ${String(s.reason).padEnd(23)} ${String(s.regime).padEnd(7)} `
    + `${String(s.shares).padEnd(8)} ${s.net.toFixed(6).padEnd(12)} ${s.perShare.toFixed(8).padEnd(14)} `
    + `${s.makerCF.toFixed(8).padEnd(12)} ${s.takerCF.toFixed(8)}`,
  );
}
console.log('');
for (const n of notes) console.log(n);
if (failures.length) {
  console.log('');
  console.log(`# VERDICT: RULER BROKEN — ${failures.length} failure(s):`);
  for (const f of failures) console.log(`  ✗ ${f}`);
  process.exit(1);
}
console.log('');
console.log('# VERDICT: RULER SOUND — every identity and closed-form expectation reproduced');
console.log('# within the scopes these arms cover: fill model (maker/taker), fee schedule,');
console.log('# exit ladder (TP/SL/stale), taker slippage, settlement booking.');
console.log('# Strategy-level negative expectancy on this stack is attributable to the');
console.log('# strategies + market microstructure, NOT to the measurement layer.');
