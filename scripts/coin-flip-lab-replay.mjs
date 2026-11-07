#!/usr/bin/env node
/**
 * coin-flip-exit-lab replay arms (#962) — the Exit-First experiment the issue
 * asks for: entry direction is a COIN FLIP (a deterministic hash of
 * seed × round_slot × condition_id), so the measured expectancy belongs to
 * the EXIT mechanism alone.
 *
 *   control   = the same package with the dynamic exit stack zeroed via
 *               --backtest-knob (#916 hot-param counterfactuals) — the
 *               "固定呆板出场" arm: kernel environment (holder 50% containment,
 *               +100% backstop) + the declared time_exit deadline, nothing else.
 *   grid      = 16 pre-declared treatment points over the stack (hard stop ×
 *               TP band × trail × time-weighted death band), ALL reported —
 *               no cherry-picking.
 *   determinism = the primary point run TWICE on the same corpus/seed; the
 *               two reports must be byte-identical (#351 trades_sha256 and a
 *               byte diff). The strategy pins its own exit-emission order
 *               (token-sorted) so the Lua VM's per-process string-hash seed
 *               cannot reorder closes.
 *
 * Everything is declared here BEFORE the numbers exist (禁止调参 cherry-pick):
 *   seed       : 962 (the issue number; manifest default `coin_seed`)
 *   corpus     : data/onchain/ 15m-act SINGLE FILE — the onchain converter's
 *                activity corpus for wallet 0x3725…08ad88, 2026-08-05 → 
 *                2026-10-31 (88 days), 4,766 rounds / 9,780 round-markets
 *                (BTC/ETH/SOL/BNB/DOGE/XRP 15-min up/down), sha256-pinned
 *                below. Processed BEFORE replay by the two corpus gates:
 *                  python3 scripts/corpus/fix_resolutions.py --check <file>
 *                    → resolutions_ok=8034 resolutions_bad=0
 *                  python3 scripts/corpus/relabel_official.py \
 *                    --apply-corpus <file>   # official CLOB settlement truth
 *                The processed copy lives (gitignored) at
 *                data/corpus/coin-flip-962/onchain-15m-act-official.jsonl.
 *                ⚠️ ACT-CORPUS CAVEAT (declared): the converter synthesizes
 *                top-of-book from the trade tape with bid == ask at ~60s
 *                cadence and nominal depth 1.0 — entries are TAKER at the
 *                ask (manifest entry_maker=false; 1-share sizing because a
 *                depth-1.0 book FOK-kills anything larger), maker-exit fills
 *                (take_profit / trailing_stop arms) are OPTIMISTIC on this
 *                corpus and their conclusions must be re-derived on the
 *                production archive before production use.
 *   realism    : the 写实结论档 runs every arm with
 *                --fee-model official --slippage-ticks 1 --backtest-tick-ms 50
 *                --backtest-fast — the published taker schedule, the 1-tick
 *                honest-friction floor on every taker fill, the live 50ms
 *                cadence, and the deadline-jump replay scheduler.
 *   rounds     : every round-market whose entry window the replay observed
 *                (`round_min_seen_time_left` 885/900 — the converter's first
 *                top row must land ≤15s into the round; 7,193 of 9,780 do).
 *                One flip + at most one entry per (slot, condition).
 *   ⚠️ stale-cadence finding (measured, then designed around): the stdlib
 *                death's quoting timeout (thesis_death_stale_ms 30000) fires
 *                on a position's SECOND ~60s top arrival — the shipped-shape
 *                run collapses the entire grid into one column (5,428/5,429
 *                stale deaths, wr 0.0%): no band, stop or trail can ever
 *                express itself. The grid therefore runs
 *                thesis_death_stale_ms=0 (timeout OFF; a vanished book/quote
 *                still fires — that leg is shared by every arm, control
 *                included) and the `shipped-default` arm keeps the stdlib
 *                shape on the record for the report's attribution section.
 *
 * The script re-implements the Lua flip hash (FNV-1a-64 + splitmix64
 * finalizer, BigInt) and cross-checks EVERY closed trade's realized side
 * against the prediction — a mismatch is a determinism bug, not a statistic.
 *
 * Holding duration: backtest reports carry no per-trade timestamps, so the
 * arms run with the intent audit ENABLED (data/audit/intents.jsonl, virtual
 * replay clock) and the lab derives entry→exit deltas per token by
 * re-simulating each trade's exit off the corpus's own top-of-book timeline.
 * Approximation (declared): fill ≈ the last entry-intent tick (1ms maker
 * escalation), close ≈ the re-simulated exit tick (± 1 evaluate tick);
 * kernel-side closes the resim cannot reproduce are counted and excluded
 * from the duration histogram, never silently averaged in.
 *
 * Usage:
 *   node scripts/coin-flip-lab-replay.mjs --replay          # control + grid + determinism
 *   node scripts/coin-flip-lab-replay.mjs --replay --only control-rigid
 *
 * Environment:
 *   BK_CORE_BIN  core binary (default: this worktree's release build)
 *   BK_LAB_OUT   output dir (default: /tmp/bk-coin-flip-lab)
 *   BK_LAB_CORPUS  corpus override (default: the pinned onchain file below)
 */

import { createHash } from 'crypto';
import { createReadStream, cpSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import { spawn } from 'child_process';
import { tmpdir } from 'os';
import { coinFlipJs } from './lib/coin-flip.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PKG = join(ROOT, 'user_layer', 'strategies_lua', 'coin_flip_exit_lab');
const CORE_BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const OUT_DIR = process.env.BK_LAB_OUT || join(tmpdir(), 'bk-coin-flip-lab');
const AUDIT_PATH = join(ROOT, 'data', 'audit', 'intents.jsonl');

// ── the pre-declared experiment ─────────────────────────────────────────────
export const SEED = 962;
const CORPUS_DIR = join(ROOT, 'data', 'corpus', 'coin-flip-962');
const CORPUS_FILE = process.env.BK_LAB_CORPUS || join(CORPUS_DIR, 'onchain-15m-act-official.jsonl');
/** sha256 over the processed corpus — pin AFTER the two corpus gates, before --replay.
 *  NOTE: the relabel pass rewrote all 8,034 resolution rows to exactly the bytes
 *  the onchain converter already carried (the output hashes identical to the
 *  pull manifest's `events` sha) — the apply step acted as a FULL-COVERAGE
 *  official-truth verification, not a correction. */
const CORPUS_SHA256 = '2787348f07c3f26504092e93217ddd0cf619e3e7927917ead9d2109737e305c1';

/** The pre-declared grid: 16 treatment points, every one reported. Each point
 *  rides the manifest defaults (= the PRIMARY point) and overrides ONLY the
 *  named knobs through the #916 hot-param path — one staged package, every
 *  difference a named knob.
 *
 *  thesis_death_stale_ms = 0 on every grid point, and why (measured, not
 *  assumed): the act corpus publishes a token's top-of-book on the wallet's
 *  own ~60s trade cadence, so the stdlib 30s quoting timeout fires on the
 *  position's SECOND top long before any band can judge a price — the
 *  shipped-default run below collapses the whole grid into one column
 *  (5,428/5,429 stale deaths, wr 0.0%). The timeout stays OFF for the stack
 *  measurement (a vanished book/quote still fires — that leg is shared by
 *  every arm); the shipped-default arm keeps the stdlib shape on the record. */
export const GRID = [];
for (const hard of [20, 35]) {
  for (const tp of [15, 30]) {
    for (const arm of [10, 25]) {
      for (const deathTight of [0, 10]) {
        GRID.push({
          name: `h${hard}_tp${tp}_tr${arm}_dt${deathTight}`,
          knobs: {
            hard_stop_loss_pct: String(hard),
            take_profit_pct: String(tp),
            trail_arm_pct: String(arm),
            thesis_death_reversal_pct_tight: String(deathTight),
            thesis_death_stale_ms: '0',
          },
        });
      }
    }
  }
}
/** The headline arm — the grid point whose knobs equal the manifest defaults
 *  except the corpus-cadence timeout (declared above). Declared before the
 *  numbers exist. */
export const PRIMARY = 'h20_tp30_tr25_dt0';
/** The shipped-shape arm: NO knobs — the manifest defaults, stdlib stale 30s.
 *  On this corpus it demonstrates the stale-arm dominance (measured once, on
 *  the record; not part of the A/B grid). `analysisKnobs` tells the resim the
 *  EFFECTIVE death config (the manifest default the engine actually ran) so
 *  the duration re-derivation judges the same rule the kernel did. */
export const SHIPPED = { name: 'shipped-default', knobs: {}, analysisKnobs: { thesis_death_stale_ms: '30000' } };
/** The rigid control: the same package, the whole dynamic stack zeroed via knobs. */
export const CONTROL = {
  name: 'control-rigid',
  knobs: {
    hard_stop_loss_pct: '0',
    take_profit_pct: '0',
    take_profit_pct_tight: '0',
    trail_arm_pct: '0',
    trail_giveback_pct: '0',
    trail_arm_pct_tight: '0',
    trail_giveback_pct_tight: '0',
    // a 100% drawdown cannot happen: reversal band inert
    thesis_death_reversal_pct: '100',
    thesis_death_reversal_pct_tight: '0',
    // timeout off (a vanished book/quote still fires — that leg is shared)
    thesis_death_stale_ms: '0',
  },
};
/** The exit-stack knobs a grid point may move (the analysis echoes the full bag). */
const STACK_KNOBS = {
  hard_stop_loss_pct: 20,
  take_profit_pct: 30,
  take_profit_pct_tight: 5,
  trail_arm_pct: 25,
  trail_giveback_pct: 10,
  trail_arm_pct_tight: 0,
  trail_giveback_pct_tight: 0,
  thesis_death_reversal_pct: 25,
  thesis_death_reversal_pct_tight: 0,
  thesis_death_stale_ms: 0,
  exit_time_left_sec: 45,
};

// ── corpus verification (the two python gates already ran; we pin the bytes) ─
function verifyCorpus() {
  if (!existsSync(CORPUS_FILE)) {
    console.error(`missing corpus ${CORPUS_FILE} — run the declared pipeline first:`);
    console.error('  python3 scripts/corpus/fix_resolutions.py --check <onchain 15m-act file>');
    console.error('  python3 scripts/corpus/relabel_official.py --apply-corpus <processed copy>');
    process.exit(2);
  }
  const buf = readFileSync(CORPUS_FILE);
  const sha = createHash('sha256').update(buf).digest('hex');
  if (CORPUS_SHA256 === 'CORPUS_SHA256_PENDING') {
    console.error(`CORPUS_SHA256 is PENDING — pin it first (sha256 ${sha})`);
    process.exit(2);
  }
  if (sha !== CORPUS_SHA256) {
    console.error(`corpus sha256 ${sha} != pinned ${CORPUS_SHA256} — refusing to report on a changed corpus`);
    process.exit(2);
  }
  return { path: CORPUS_FILE, sha };
}

/** Index what the analysis needs, one pass over the corpus:
 *   rounds(token) → { slot, conditionId, upTokenId, expiresAtMs }
 *   books(token)  → [[at, bestBid, bestAsk], …] (top rows; bid==ask on act corpora)
 * Also verifies every `k:resolution` row is TOKEN-form (the fix_resolutions
 * gate's contract) and refuses a label-form corpus. */
export async function corpusIndex(corpusPath) {
  const rounds = new Map();
  const books = new Map();
  const roundTokens = new Set();
  let resolutions = 0;
  let resolutionsLabelForm = 0;
  const rl = createInterface({ input: createReadStream(corpusPath), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    if (!line.includes('"k":')) continue;
    if (line.includes('"k":"round"')) {
      let rec;
      try {
        rec = JSON.parse(line);
      } catch {
        continue;
      }
      for (const m of rec.m ?? []) {
        if (m.upTokenId && m.conditionId) {
          const row = { slot: m.roundSlot, conditionId: m.conditionId, upTokenId: m.upTokenId, expiresAtMs: m.expiresAtMs };
          rounds.set(m.upTokenId, row);
          rounds.set(m.downTokenId, row);
          roundTokens.add(m.upTokenId);
          roundTokens.add(m.downTokenId);
        }
      }
    } else if (line.includes('"k":"top"')) {
      let rec;
      try {
        rec = JSON.parse(line);
      } catch {
        continue;
      }
      const bid = rec.bb === undefined || rec.bb === '' ? null : Number(rec.bb);
      const ask = rec.ba === undefined || rec.ba === '' ? null : Number(rec.ba);
      let arr = books.get(rec.t);
      if (!arr) {
        arr = [];
        books.set(rec.t, arr);
      }
      arr.push([rec.at, bid, ask]);
    } else if (line.includes('"k":"resolution"')) {
      let rec;
      try {
        rec = JSON.parse(line);
      } catch {
        continue;
      }
      resolutions += 1;
      for (const [tok] of rec.p ?? []) {
        if (!roundTokens.has(tok)) resolutionsLabelForm += 1;
      }
    }
  }
  if (resolutionsLabelForm > 0) {
    console.error(`corpus has ${resolutionsLabelForm} label-form payout rows across ${resolutions} resolutions — run fix_resolutions.py --apply first`);
    process.exit(2);
  }
  return { rounds, books, resolutions };
}

// ── package staging (ONE package per arm, temp dir — #302 sibling coupling) ──
function stagePackage() {
  const root = mkdtemp('bk-coin-flip-pkg-');
  const dir = join(root, 'coin_flip_exit_lab');
  cpSync(PKG, dir, { recursive: true });
  return root;
}

function mkdtemp(prefix) {
  return join(tmpdir(), `${prefix}${process.pid}-${Math.random().toString(36).slice(2, 8)}`);
}

// ── one replay ──────────────────────────────────────────────────────────────
function runOne(bin, luaDir, windowName, corpusPath, outDir, knobs) {
  return new Promise((res) => {
    const report = join(outDir, `${windowName}.json`);
    if (existsSync(AUDIT_PATH)) rmSync(AUDIT_PATH, { force: true });
    const args = [
      '--mode', 'dry',
      '--engine',
      '--no-discovery',
      '--no-strategy-state',
      '--lua-strategy-dir', luaDir,
      '--enable-strategy', 'coin_flip_exit_lab',
      '--no-order-log', '--no-position-log', '--no-event-archive',
      '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
      '--seed-balance', '1000', '--max-order-notional', '12',
      // act-corpus top-of-book carries nominal depth 1.0: a larger order is
      // FOK-killed on every fill — the size is the corpus's, not a tuned dial.
      '--min-shares', '1', '--max-shares', '1',
      '--max-positions', '10',
      '--entry-maker-timeout-ms', '1',
      // the 写实结论档 (declared): official schedule, honest-friction floor,
      // live cadence, deadline-jump scheduler.
      '--fee-model', 'official',
      '--slippage-ticks', '1',
      '--backtest', corpusPath,
      '--backtest-report', report,
      '--backtest-tick-ms', '50', '--backtest-tail-ms', '0',
      '--backtest-fast',
    ];
    for (const [k, v] of Object.entries(knobs ?? {})) {
      args.push('--backtest-knob', `coin_flip_exit_lab:${k}=${v}`);
    }
    const child = spawn(bin, args, {
      cwd: ROOT,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: luaDir },
    });
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    const t0 = Date.now();
    child.on('close', async (code) => {
      if (code !== 0 || !existsSync(report)) {
        res({ name: windowName, error: `exit ${code}: ${err.split('\n').slice(-3).join(' | ')}` });
        return;
      }
      const audit = await readAudit();
      res({ name: windowName, report, audit, wallSec: (Date.now() - t0) / 1000 });
    });
  });
}

/** The intent audit (strategy entry intents on the virtual clock). */
export async function readAudit() {
  if (!existsSync(AUDIT_PATH)) return { entries: new Map(), rejected: 0, lines: 0 };
  const entries = new Map(); // token -> { ts, slot, condition, direction }
  let rejected = 0;
  let lines = 0;
  const rl = createInterface({ input: createReadStream(AUDIT_PATH), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    lines += 1;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    if (rec.strategy !== 'coin_flip_exit_lab') continue;
    const intent = rec.intent ?? {};
    const token = intent.tokenId ?? intent.token_id ?? intent.token;
    if (!token) continue;
    const ts = rec.tsMs ?? rec.ts_ms;
    if (typeof ts !== 'number') continue;
    // Exit suggestions never reach the audit sink (they bypass arbitration),
    // so every audited lab intent is an entry. The entry RE-QUOTES until the
    // fill is observed, so keep the LAST intent ts per token — the fill lands
    // within one escalation tick after it (± 1 tick duration accuracy).
    entries.set(token, {
      ts,
      slot: intent.roundSlot,
      condition: intent.conditionId,
      direction: intent.direction,
    });
    const status = rec.decision?.status ?? '';
    if (status && status !== 'APPROVED') rejected += 1;
  }
  return { entries, rejected, lines };
}

// ── analysis ────────────────────────────────────────────────────────────────
export function analyze(runResult, index, knobs) {
  const r = JSON.parse(readFileSync(runResult.report, 'utf8'));
  const lines = r.tradeLines ?? [];
  const nets = lines.map((l) => Number(l.netPnlUsd));
  const losses = nets.filter((n) => n < 0).sort((a, b) => a - b);
  const q = (arr, p) => (arr.length ? arr[Math.min(arr.length - 1, Math.floor(p * arr.length))] : null);
  const p = { ...STACK_KNOBS };
  for (const [k, v] of Object.entries(knobs ?? {})) p[k] = Number(v);
  // Holding duration: re-simulate each trade's exit off the corpus timeline
  // (± 1 evaluate tick); the audit supplies the entry tick per token. TWO
  // estimators, by who owns the close:
  //   * strategy-side closes (ThesisDeathExit / StrategySignal / TakeProfit /
  //     TrailingStop / StopLoss) — the every-top resim judges the same rule
  //     math the strategy ran (validated per-arm by the reasonMatch stat);
  //   * kernel-side deadline closes (TimeExit / Settlement /
  //     UnverifiableExpiry) — derived DIRECTLY from the engine's own close
  //     reason (the deadline the rule names), because the fast replay's
  //     kernel ladder timing is not the resim's every-top model (measured:
  //     the rigid control's resim mismatches ~96% while its rule-timed
  //     derivation is exact to ±1 grid tick).
  const durations = [];
  let unmeasured = 0;
  let reasonMatch = 0;
  let reasonDiff = 0;
  let kernelDerived = 0;
  const resimReasons = {};
  for (const l of lines) {
    const e = runResult.audit.entries.get(l.tokenId);
    const round = index.rounds.get(l.tokenId);
    const timeline = index.books.get(l.tokenId);
    if (!e || !round) {
      unmeasured += 1;
      continue;
    }
    let at = null;
    let reason = null;
    if (l.reason === 'TimeExit' || l.reason === 'Settlement' || l.reason === 'UnverifiableExpiry') {
      at = l.reason === 'TimeExit'
        ? round.expiresAtMs - p.exit_time_left_sec * 1000
        : round.expiresAtMs;
      reason = l.reason;
      kernelDerived += 1;
    } else if (timeline) {
      const hit = resimExit({ price: Number(l.entryPrice), ts: e.ts }, timeline, round.expiresAtMs, p, 0.07);
      if (hit) {
        at = hit.at;
        reason = hit.reason;
        resimReasons[hit.reason] = (resimReasons[hit.reason] ?? 0) + 1;
        if (hit.reason === l.reason) reasonMatch += 1;
        else reasonDiff += 1;
      }
    }
    if (at === null) {
      unmeasured += 1;
      continue;
    }
    durations.push((at - e.ts) / 1000);
  }
  durations.sort((a, b) => a - b);
  // Realized side vs the predicted flip, per trade. The flip key's slot is the
  // KERNEL's round slot (the audit's intent.roundSlot); the corpus carries the
  // VENUE's roundSlot per token. The offset is DERIVED per run and must be
  // constant — any drift is a slot-identity bug, not a statistic.
  const sideCounts = { up: 0, down: 0 };
  let auditDirectionMismatch = 0;
  const offsets = new Map();
  for (const l of lines) {
    const round = index.rounds.get(l.tokenId);
    if (!round) continue;
    const actual = l.tokenId === round.upTokenId ? 'up' : 'down';
    sideCounts[actual] += 1;
    const e = runResult.audit.entries.get(l.tokenId);
    if (e && e.slot !== undefined) offsets.set(e.slot - round.slot, (offsets.get(e.slot - round.slot) ?? 0) + 1);
    // integrity: the audit's own direction must agree with the corpus's legs
    if (e && e.direction && e.direction !== actual) auditDirectionMismatch += 1;
  }
  const [[slotOffset, slotOffsetN] = [null, 0]] = [...offsets.entries()].sort((a, b) => b[1] - a[1]);
  let flipsChecked = 0;
  let flipsMismatched = 0;
  for (const l of lines) {
    const round = index.rounds.get(l.tokenId);
    if (!round) continue;
    const actual = l.tokenId === round.upTokenId ? 'up' : 'down';
    const predicted = coinFlipJs(SEED, round.slot + (slotOffset ?? 0), round.conditionId);
    flipsChecked += 1;
    if (actual !== predicted) flipsMismatched += 1;
  }
  let slotInvariantBroken = 0;
  if (offsets.size > 1) {
    console.error(`  RED  kernel roundSlot − venue roundSlot is not constant across trades: ${JSON.stringify([...offsets])} — the flip cross-check key is wrong`);
    slotInvariantBroken = 1;
  }
  if (auditDirectionMismatch > 0) {
    console.error(`  RED  audit direction disagrees with the corpus legs on ${auditDirectionMismatch} trades — integrity bug, not a statistic`);
    slotInvariantBroken = 1;
  }
  const worstIdx = nets.indexOf(Math.min(...nets));
  // One position per (slot, condition) is the flip's contract — a duplicate
  // would mean a re-entry through the entry window (a same-window exit plus a
  // fresh re-quote). Count them; the report flags any.
  const seenSlotCond = new Set();
  let duplicateSlotCond = 0;
  for (const l of lines) {
    const round = index.rounds.get(l.tokenId);
    if (!round) continue;
    const key = `${round.slot}:${round.conditionId}`;
    if (seenSlotCond.has(key)) duplicateSlotCond += 1;
    seenSlotCond.add(key);
  }
  return {
    duplicateSlotCond,
    slotOffset,
    closed: r.trades?.closed ?? lines.length,
    wins: r.trades?.wins ?? 0,
    winRatePct: Number(r.trades?.winRatePct ?? 0),
    netPnlUsd: Number(r.trades?.netPnlUsd ?? 0),
    avgPnlUsd: Number(r.trades?.avgPnlUsd ?? 0),
    profitFactor: r.trades?.profitFactor != null ? Number(r.trades.profitFactor) : null,
    grossProfitUsd: Number(r.trades?.grossProfitUsd ?? 0),
    grossLossUsd: Number(r.trades?.grossLossUsd ?? 0),
    maxDrawdownUsd: Number(r.trades?.maxDrawdownUsd ?? 0),
    maxDrawdownPct: Number(r.trades?.maxDrawdownPct ?? 0),
    feesUsd: Number(r.trades?.feesUsd ?? 0),
    openAtEnd: r.openPositions ?? 0,
    tradesSha256: r.tradesSha256 ?? '',
    reasons: lines.reduce((m, l) => ((m[l.reason] = (m[l.reason] ?? 0) + 1), m), {}),
    lossDist: {
      worst: losses.length ? losses[0] : 0,
      p50: q(losses, 0.5),
      p90: q(losses, 0.9),
      p99: q(losses, 0.99),
      beyondMinus10: losses.filter((n) => n <= -10).length,
      buckets: bucketLosses(losses),
    },
    durationSec: {
      n: durations.length,
      mean: durations.length ? durations.reduce((a, b) => a + b, 0) / durations.length : null,
      p50: q(durations, 0.5),
      p90: q(durations, 0.9),
      kernelDerived,
      unmeasured,
    },
    resim: { reasonMatch, reasonDiff, resimReasons },
    flips: { checked: flipsChecked, mismatched: flipsMismatched, sideCounts, slotInvariantBroken, slotOffsetN },
    audit: { attempted: runResult.audit.entries.size, rejected: runResult.audit.rejected, lines: runResult.audit.lines },
    worstTrade: worstIdx >= 0 ? lines[worstIdx] : null,
  };
}

function bucketLosses(losses) {
  const edges = [0, -0.2, -0.4, -0.6, -0.8, -1.0, -1.2, -Infinity];
  const labels = ['(0,-0.2]', '(-0.2,-0.4]', '(-0.4,-0.6]', '(-0.6,-0.8]', '(-0.8,-1.0]', '(-1.0,-1.2]', '(<-1.2)'];
  const buckets = {};
  for (let i = 0; i < labels.length; i++) buckets[labels[i]] = 0;
  for (const n of losses) {
    for (let i = 1; i < edges.length; i++) {
      if (n > edges[i] && n <= edges[i - 1]) {
        buckets[labels[i - 1]] += 1;
        break;
      }
    }
  }
  return buckets;
}

/**
 * Re-simulate ONE trade's exit off the corpus's own top-of-book timeline — the
 * same rule math the Lua strategy and the kernel ladder run, evaluated per top
 * event (± 1 evaluate tick). Backtest reports carry no per-trade timestamps,
 * so this re-derivation is how the lab measures holding duration (and it
 * cross-checks the close reason the report attributes).
 *
 * `trade` carries the two facts the report + audit supply: `price` (the fill
 * the position actually opened at, slippage included — the number every net
 * judgement in the engine compares against) and `ts` (the last entry intent
 * ≈ the fill tick). NOTE: the price judgements read `trade.price`, never the
 * audit row — an undefined price would NaN every comparison and silently
 * reduce the resim to the time exit (measured as such once; do not regress).
 *
 * Priority per tick: thesis death (stale → reversal band, runtime-injected) →
 * kernel holder containment stop 50% → kernel +100% backstop → hard stop →
 * take-profit band → trailing stop → time exit → expiry settlement.
 */
function resimExit(trade, timeline, expiryMs, p, feeRate) {
  const FEE = (bid) => feeRate * bid * (1 - bid);
  const tight = (base, tightPct, timeLeftSec) => {
    if (!(tightPct > 0) || !(tightPct < base)) return base;
    const frac = Math.min(1, Math.max(0, (900 - timeLeftSec) / 900));
    return Math.max(tightPct, base - (base - tightPct) * frac);
  };
  let lastBookTs = trade.ts;
  let lastAt = trade.ts;
  let peak = null;
  for (const [at, bid, ask] of timeline) {
    if (at <= trade.ts) continue;
    if (at >= expiryMs) return { at, reason: 'Settlement' }; // the round owns it now
    const timeLeftSec = (expiryMs - at) / 1000;
    // Condition 1 — the quoting fields vanished / timed out (stdlib death).
    if (bid === null && ask === null) return { at, reason: 'ThesisDeathExit' };
    if (p.thesis_death_stale_ms > 0 && at - lastBookTs > p.thesis_death_stale_ms) {
      return { at, reason: 'ThesisDeathExit' };
    }
    lastBookTs = at;
    lastAt = at;
    if (bid === null || bid <= 0) continue; // no sellable price: nothing to judge
    // Condition 2/3 — the reversal band (stdlib death, raw bid off entry).
    if (trade.price > 0 && bid < trade.price) {
      const adverse = ((trade.price - bid) / trade.price) * 100;
      const band = tight(p.thesis_death_reversal_pct, p.thesis_death_reversal_pct_tight, timeLeftSec);
      if (adverse >= band) return { at, reason: 'ThesisDeathExit' };
    }
    // The kernel's own ladder (identical environment both arms): the holder
    // containment stop (flat 50%) and the +100% backstop, judged on the raw bid.
    const pnlPct = ((bid - trade.price) / trade.price) * 100;
    if (pnlPct <= -50) return { at, reason: 'StopLoss' };
    if (pnlPct >= 100) return { at, reason: 'TakeProfit' };
    // The strategy's own stack, judged on NET proceeds (bid − taker fee).
    const proceeds = bid - FEE(bid);
    if (peak === null || proceeds > peak) peak = proceeds;
    if (p.hard_stop_loss_pct > 0 && proceeds <= trade.price * (1 - p.hard_stop_loss_pct / 100)) {
      return { at, reason: 'StrategySignal' }; // tag hard_stop_loss → StrategySignal
    }
    const tp = tight(p.take_profit_pct, p.take_profit_pct_tight, timeLeftSec);
    if (tp > 0 && ((proceeds - trade.price) / trade.price) * 100 >= tp) {
      return { at, reason: 'TakeProfit' };
    }
    const arm = tight(p.trail_arm_pct, p.trail_arm_pct_tight, timeLeftSec);
    const give = tight(p.trail_giveback_pct, p.trail_giveback_pct_tight, timeLeftSec);
    if (arm > 0 && give > 0 && peak !== null
      && ((peak - trade.price) / trade.price) * 100 >= arm
      && ((peak - proceeds) / trade.price) * 100 >= give) {
      return { at, reason: 'TrailingStop' };
    }
    if (p.exit_time_left_sec > 0 && timeLeftSec <= p.exit_time_left_sec) {
      return { at, reason: 'TimeExit' };
    }
  }
  // The timeline ended: a position still held rode to the round's expiry —
  // the kernel's settlement owns it (the corpus keeps publishing the token's
  // top until expiry, so a timeline that falls short by minutes means the
  // trade cannot be timed honestly).
  if (expiryMs - lastAt <= 120000) return { at: expiryMs, reason: 'Settlement' };
  return null;
}

// ── main ────────────────────────────────────────────────────────────────────
const REPLAY = process.argv.includes('--replay');
const ONLY = process.argv.includes('--only') ? process.argv[process.argv.indexOf('--only') + 1] : null;

if (!REPLAY) {
  console.error('usage: --replay [--only <arm>]   (corpus: see the header pipeline)');
  process.exit(2);
}
if (!existsSync(CORE_BIN)) {
  console.error(`core binary missing: ${CORE_BIN} (cargo build --release -p blitzkrieg-core)`);
  process.exit(2);
}

const corpus = verifyCorpus();
console.log(`corpus ok (sha ${corpus.sha.slice(0, 12)}…, ${statSync(CORPUS_FILE).size} bytes)`);
const index = await corpusIndex(corpus.path);
const roundsInWindow = [...new Set([...index.rounds.values()].map((r) => `${r.slot}:${r.conditionId}`))].length;
console.log(`round-markets: ${roundsInWindow}, resolutions: ${index.resolutions}, seed: ${SEED}`);

const arms = [SHIPPED, CONTROL, ...GRID].filter((a) => !ONLY || a.name === ONLY);
mkdirSync(OUT_DIR, { recursive: true });
const rows = [];
for (const arm of arms) {
  const pkgDir = stagePackage();
  const knobs = arm.knobs ?? {};
  const run = await runOne(CORE_BIN, pkgDir, arm.name, corpus.path, OUT_DIR, knobs);
  if (run.error) {
    console.error(`  RED  ${arm.name}: ${run.error}`);
    rows.push({ name: arm.name, error: run.error });
    continue;
  }
  const a = analyze(run, index, { ...knobs, ...(arm.analysisKnobs ?? {}) });
  rows.push({ name: arm.name, wallSec: run.wallSec, ...a });
  const pf = a.profitFactor === null ? 'n/a' : a.profitFactor.toFixed(3);
  console.log(
    `  ${arm.name.padEnd(22)} closed=${String(a.closed).padStart(4)} wr=${a.winRatePct.toFixed(1).padStart(5)}% net=${a.netPnlUsd.toFixed(2).padStart(8)} E[trade]=${a.avgPnlUsd.toFixed(4)} PF=${pf.padStart(6)} maxDD=${a.maxDrawdownUsd.toFixed(2)} worst=${a.lossDist.worst.toFixed(2)} hold=${a.durationSec.mean === null ? 'n/a' : a.durationSec.mean.toFixed(0) + 's'} flips✓=${a.flips.checked - a.flips.mismatched}/${a.flips.checked} sha=${a.tradesSha256.slice(0, 8)}`,
  );
  if (a.duplicateSlotCond > 0) {
    console.error(`  RED  ${arm.name}: ${a.duplicateSlotCond} duplicate (slot, condition) positions — the one-flip contract broke`);
  }
}

// Determinism: the primary point twice on the same corpus/seed must produce
// identical trade lists (#351 trades_sha256) and byte-identical reports —
// after normalizing the ONE provenance field that legitimately differs (the
// per-run staged-package temp dir embedded in `source`).
let determinism = null;
if (!ONLY || ONLY === PRIMARY) {
  const pkgDir = stagePackage();
  const run1 = await runOne(CORE_BIN, pkgDir, `${PRIMARY}-det1`, corpus.path, OUT_DIR, GRID.find((g) => g.name === PRIMARY).knobs);
  const pkgDir2 = stagePackage();
  const run2 = await runOne(CORE_BIN, pkgDir2, `${PRIMARY}-det2`, corpus.path, OUT_DIR, GRID.find((g) => g.name === PRIMARY).knobs);
  if (!run1.error && !run2.error) {
    const b1 = readFileSync(run1.report);
    const b2 = readFileSync(run2.report);
    const norm = (buf, root) => Buffer.from(buf.toString().split(root).join('<staged-pkg>'));
    const n1 = norm(b1, pkgDir);
    const n2 = norm(b2, pkgDir2);
    const identical = n1.equals(n2);
    determinism = {
      identical,
      sha1: JSON.parse(b1.toString()).tradesSha256,
      sha2: JSON.parse(b2.toString()).tradesSha256,
      bytes: n1.length,
      diffBytes: identical ? 0 : [...n1].filter((x, i) => x !== n2[i]).length,
    };
    console.log(`  determinism(${PRIMARY}): ${identical ? 'IDENTICAL' : 'MISMATCH'} (trades_sha256 ${determinism.sha1} vs ${determinism.sha2}, ${determinism.bytes} bytes)`);
  } else {
    determinism = { identical: false, error: run1.error ?? run2.error };
    console.error(`  RED  determinism run failed: ${determinism.error}`);
  }
}

const outPath = join(OUT_DIR, 'summary.json');
writeFileSync(outPath, JSON.stringify({ seed: SEED, corpus: corpus.path, corpusSha256: corpus.sha, roundsInWindow, primary: PRIMARY, determinism, rows }, replacer, 2) + '\n');
console.log(`# summary written to ${outPath}`);

function replacer(_k, v) {
  if (v instanceof Map) return Object.fromEntries(v);
  return v;
}
