#!/usr/bin/env node
/**
 * Corpus-level ask-sum statistics for pair_discount_arb (task 3.1 acceptance
 * "replay shows ask_sum<1 frequency/edge") — the frequency a complete
 * UP+DOWN pair is offered below par, plus the median discount, straight
 * from the corpus bytes.
 *
 * What counts as an opportunity (the strategy's own trigger, replayed):
 *
 *   ask_UP + ask_DOWN + entry_taker_fees < 0.995
 *
 * with the fee curve read from the kernel's ONE schedule
 * (scripts/lib/fee-model.mjs: rate*(p*(1-p))^exponent, legacy_quadratic by
 * default, --fee-model to compare). Per (condition, book event): both legs'
 * top asks must exist, sit in [0.01, 0.99], and the pair-attempt latch
 * (one per condition per round, the package's own rule) applies per round
 * slot derived from the round rows.
 *
 * Usage:
 *   node scripts/pair-discount-stats.mjs                       # the 4 frozen windows
 *   node scripts/pair-discount-stats.mjs --dir data/corpus/spot
 *   node scripts/pair-discount-stats.mjs --dir data/corpus/spot --fee-model official
 *
 * Output: per-window rows + a totals row; JSON with --json for the report.
 */

import { createReadStream, existsSync, readdirSync } from 'fs';
import { createInterface } from 'readline';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import { feePerShareAt } from './lib/fee-model.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const flag = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};
const DIR = flag('--dir') || join(ROOT, 'data', 'corpus', 'spot');
const AS_JSON = args.includes('--json');

// The kernel's ONE fee curve (scripts/lib/fee-model.mjs is the same source
// the gates read; feePerShareAt is its independent per-share implementation).
// Legacy quadratic unless --fee-model says else.
const FEE_MODEL = flag('--fee-model') || 'legacy_quadratic';

const MAX_PAIR_COST = 0.995; // the package's tunable default
const LEG_MIN = 0.01;
const LEG_MAX = 0.99;

// ── corpus scan ─────────────────────────────────────────────────────────────

async function scanWindow(path) {
  // condition -> { roundSlot, upToken, downToken }
  const conditions = new Map();
  // token -> best ask
  const asks = new Map();
  // per round-slot: did a trigger fire for this condition already?
  const fired = new Map();

  let bookEvents = 0;
  let pairedEvents = 0;
  let triggers = 0;
  const edges = [];

  const rl = createInterface({ input: createReadStream(path), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    let row;
    try {
      row = JSON.parse(line);
    } catch {
      continue;
    }
    const at = row.at ?? 0;

    if (row.k === 'round' && Array.isArray(row.m)) {
      for (const m of row.m) {
        if (m.conditionId && m.upTokenId && m.downTokenId) {
          conditions.set(m.conditionId, {
            upToken: m.upTokenId,
            downToken: m.downTokenId,
            expiresAtMs: m.expiresAtMs ?? 0,
          });
        }
      }
      continue;
    }

    if (row.k === 'book' && row.t && Array.isArray(row.a) && row.a.length) {
      bookEvents++;
      // Best ask = the LOWEST price offered. The corpus stores levels
      // worst-first (0.99 → 0.002, descending), so the best level is the
      // LAST element — but min() is order-agnostic and mirrors the core's
      // `Book::best_ask` query exactly.
      let best = Infinity;
      for (const lvl of row.a) {
        const p = Number(lvl[0]);
        if (p > 0 && p < best) best = p;
      }
      if (Number.isFinite(best) && best > 0) asks.set(row.t, best);
    }

    // Evaluate pair triggers lazily: a book event on either leg of a known
    // condition refreshes that condition's pair check.
    if (row.k === 'book' && row.t) {
      for (const [cond, c] of conditions) {
        if (row.t !== c.upToken && row.t !== c.downToken) continue;
        const au = asks.get(c.upToken);
        const ad = asks.get(c.downToken);
        if (au === undefined || ad === undefined) continue;
        if (au < LEG_MIN || au > LEG_MAX || ad < LEG_MIN || ad > LEG_MAX) continue;
        pairedEvents++;
        const slot = Math.floor(at / 900_000);
        const key = cond + ':' + slot;
        if (fired.get(key)) continue;
        const feeU = feePerShareAt(FEE_MODEL, au);
        const feeD = feePerShareAt(FEE_MODEL, ad);
        if (!Number.isFinite(feeU) || !Number.isFinite(feeD)) continue;
        const total = au + ad + feeU + feeD;
        if (total < MAX_PAIR_COST) {
          fired.set(key, true);
          triggers++;
          edges.push(1 - total);
        }
      }
    }
  }
  return { bookEvents, pairedEvents, triggers, edges };
}

function median(a) {
  if (!a.length) return 0;
  const s = [...a].sort((x, y) => x - y);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

// ── windows ─────────────────────────────────────────────────────────────────

const files = existsSync(DIR)
  ? readdirSync(DIR).filter((f) => f.endsWith('.jsonl')).sort()
  : [];
if (!files.length) {
  console.error(`no .jsonl corpus files under ${DIR}`);
  process.exit(1);
}

const rows = [];
let tTriggers = 0;
let tPaired = 0;
let tBooks = 0;
const allEdges = [];
for (const f of files) {
  const { bookEvents, pairedEvents, triggers, edges } = await scanWindow(join(DIR, f));
  const rate = pairedEvents ? (triggers / pairedEvents) * 100 : 0;
  rows.push({
    window: f.replace(/\.jsonl$/, ''),
    bookEvents,
    pairedEvents,
    triggers,
    ratePct: rate,
    medianEdgePct: median(edges) * 100,
    maxEdgePct: edges.length ? Math.max(...edges) * 100 : 0,
  });
  tTriggers += triggers;
  tPaired += pairedEvents;
  tBooks += bookEvents;
  allEdges.push(...edges);
}

const totals = {
  bookEvents: tBooks,
  pairedEvents: tPaired,
  triggers: tTriggers,
  ratePct: tPaired ? (tTriggers / tPaired) * 100 : 0,
  medianEdgePct: median(allEdges) * 100,
  maxEdgePct: allEdges.length ? Math.max(...allEdges) * 100 : 0,
};

if (AS_JSON) {
  console.log(JSON.stringify({ windows: rows, totals }, null, 2));
} else {
  console.log(`# pair-discount stats  corpus ${DIR}  fee ${FEE_MODEL}  cap 0.995`);
  console.log('# window  books  paired-checks  triggers  rate%  median-edge%  max-edge%');
  for (const r of rows) {
    console.log(
      `${r.window}  ${r.bookEvents}  ${r.pairedEvents}  ${r.triggers}  ${r.ratePct.toFixed(3)}  ${r.medianEdgePct.toFixed(3)}  ${r.maxEdgePct.toFixed(3)}`,
    );
  }
  console.log(
    `TOTAL  ${totals.bookEvents}  ${totals.pairedEvents}  ${totals.triggers}  ${totals.ratePct.toFixed(3)}  ${totals.medianEdgePct.toFixed(3)}  ${totals.maxEdgePct.toFixed(3)}`,
  );
}
