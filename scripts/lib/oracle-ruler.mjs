/**
 * The oracle-ruler calibration set: ONE definition of the synthetic corpora,
 * the probe's behaviour and the closed-form expectations, so the check script
 * and the corpus builder cannot drift apart.
 *
 * WHY A SYNTHETIC CORPUS (and not the frozen capture): the naive instrument —
 * "buy 0.30, sell 0.50" — is NOT a logical winner on a martingale binary
 * market (P(reach 0.50 first) = 0.6 from 0.30, EV = 0.6*0.20 − 0.4*0.30 = 0
 * before fees), and this market is measured calibrated. A real ruler needs a
 * price PATH that is CONSTRUCTED, so every entry price, exit price and payout
 * is known ahead of the run and the ledger can be reconciled to the cent. The
 * frozen capture has no settlement lines and unknown forward paths — it can
 * never yield a closed-form expectation.
 *
 * THE ARMS (all 900s rounds, the same spawn shape as the strategy replays,
 * kernel exit ladder at DEFAULTS in every arm — no --exit-* overrides):
 *
 *   win-tp       entry 0.30 (maker) → flat → jump 0.65 at t=30s.
 *                PnL at 0.65 = +116.7% ≥ fixed TP backstop (100%) → taker
 *                exit at 0.65. Expectation: 0.65−0.30−fee(0.65) =
 *                +0.34353046875/share. MUST book positive — the user's ruler
 *                test ("if a logical winner can't profit, the ruler is broken").
 *   loss-sl      entry 0.30 → jump 0.10 at t=30s → SL 12% fires → taker exit
 *                0.10. Expectation: −0.20−fee(0.10) = −0.2010125/share.
 *   win-timeexit entry 0.30 → jump 0.55 at t=30s (pnl +83.3% < TP 100%,
 *                trailing armed but the book is flat — no giveback) → held to
 *                force_exit (T−120s) → taker exit at 0.55. Expectation:
 *                0.25−fee(0.55) = +0.24234296875/share. Answers checklist #6:
 *                does the time exit mangle a winner? (A stale-profit rule may
 *                also fire on the flat book — at the SAME price, so the
 *                identity holds either way; the reason is printed, not
 *                asserted.)
 *   win-tp-slip1 same corpus as win-tp, replayed with --slippage-ticks 1:
 *                the taker exit pays one tick (0.64). Expectation:
 *                0.34−fee(0.64) = +0.33336448/share. Checklist #2.
 *   settle-win   entry 0.30 → flat → at t=700s the UP book goes ASKS-ONLY at
 *                0.90 (no bids ⇒ force exit and SL have no executable price —
 *                the exit-economics mechanism) and DOWN jumps to a two-sided
 *                0.10. The position rides to expiry; dry_resolution prices
 *                DOWN at 0.10 (book mid) and UP at book-mid-0 → the
 *                position.current_price FALLBACK. If the fallback carries any
 *                mark > 0.10, UP wins and the payout is $1: expectation
 *                +0.70/share. Booking anything else is the settlement
 *                double-source discrepancy made visible (checklist #4).
 *   settle-loss  same construction, UP asks-only at 0.02, DOWN two-sided at
 *                0.95 → DOWN wins on the BOOK MID alone (no fallback needed),
 *                UP pays 0: expectation −0.30/share. Airtight control.
 *
 * FEE MATH: legacy_quadratic, fee_per_share = 0.125*(p*(1-p))^2 — restated
 * here DELIBERATELY independently of scripts/lib/fee-model.mjs: agreement
 * between two independent restatements and the kernel's booked fees is the
 * evidence; an import would be circularity. Maker fills are free; the formula
 * self-zeroes at p ∈ {0,1}, so settlement payouts carry no fee under either
 * reading.
 *
 * FILL MECHANICS: the probe rests a maker bid at the CONFIG price 0.30 while
 * the book quotes bid 0.30 / ask 0.31 for the first 3s (no cross at submit —
 * safe under a reject-if-crossing post-only reading), then the ask drops to
 * 0.30 and the resting bid crosses and fills AT ITS OWN PRICE (sim.rs: maker
 * BUY fills when best ask <= limit). Immune to either post-only semantic.
 */

import { mkdirSync, writeFileSync } from 'fs';
import { join } from 'path';

/** Taker fee per share, legacy_quadratic — the INDEPENDENT restatement (see
 *  the fee-math note above). */
export function feePerShareLegacy(price) {
  const p = Number(price);
  return 0.125 * (p * (1 - p)) ** 2;
}

/** The entry price every arm uses (probe manifest tunable default). */
export const ENTRY_PRICE = 0.3;
/** Seconds the UP book quotes ask=0.31 before dropping to 0.30 (the fill trigger). */
export const FILL_TRIGGER_SEC = 4;
/** Round length in seconds — one grid slot, declared expiry authoritative. */
export const ROUND_SEC = 900;

export const PROBES = {
  'win-tp': { jump: 0.65, jumpAtSec: 30 },
  'loss-sl': { jump: 0.1, jumpAtSec: 30 },
  'win-timeexit': { jump: 0.55, jumpAtSec: 30 },
  'settle-win': { asksOnlyAtSec: 700, upAsk: 0.9, downJump: 0.1 },
  'settle-loss': { asksOnlyAtSec: 700, upAsk: 0.02, downJump: 0.95 },
  // The settlement-booking probe: UP jumps to a PHANTOM two-sided book — bids
  // quoted at 0.95 with ZERO size, asks 500 @ 0.95. mid = 0.95 > 0.5, so the
  // dry resolver can PRICE and PAY the winner; but a zero-depth bid side
  // means no exit (maker sell crosses with nothing to fill against; taker
  // walk finds no size; force_exit likewise) — the position must ride to
  // expiry and book the $1 payout. Expected +0.70/share if settlement books
  // winners correctly (checklist #4); anything else is a booking defect.
  'settle-win-phantom': { phantomAtSec: 700, upMark: 0.95, downJump: 0.05 },
};

/** Shared ladder geometry: size per level, levels per side. */
export const LEVEL_SIZE = 500;
export const LEVELS = 3;

/** Two-sided flat ladder at `price`: bids [p, p−.01, p−.02], asks [p, p+.01,
 *  p+.02]. best_bid == best_ask == price ⇒ mid = price exactly (F6 needs both
 *  sides; a crossed book would poison every downstream number). */
export function bookTwoSided(atMs, token, price) {
  const b = [];
  const a = [];
  for (let i = 0; i < LEVELS; i++) {
    const bp = Math.max(0.01, price - i * 0.01).toFixed(2);
    const ap = Math.min(0.99, price + i * 0.01).toFixed(2);
    b.push([bp, String(LEVEL_SIZE)]);
    a.push([ap, String(LEVEL_SIZE)]);
  }
  return JSON.stringify({ at: atMs, k: 'book', t: token, b, a });
}

/** Asks-only book: no bids ⇒ no executable sell price ⇒ force exit and SL
 *  cannot fire (exit-economics mechanism), and the mid drops to 0 (F6). */
export function bookAsksOnly(atMs, token, price) {
  const a = [];
  for (let i = 0; i < LEVELS; i++) {
    const ap = Math.min(0.99, price + i * 0.01).toFixed(2);
    a.push([ap, String(LEVEL_SIZE)]);
  }
  return JSON.stringify({ at: atMs, k: 'book', t: token, b: [], a });
}

/** Phantom two-sided book: bids quoted AT `price` with ZERO size, asks with
 *  real size. mid = price (> 0.5 for the winner probe) so the dry resolver
 *  can price and PAY it; the zero-depth bid side means no sell can execute
 *  (a maker sell crosses but marketable_depth = 0; a taker walk finds no
 *  size — sim.rs crossing_depth documents exactly this zero-size case), so
 *  TP / stale / force_exit all fail to fire and the position must ride to
 *  expiry and book the $1 payout. */
export function bookPhantom(atMs, token, price) {
  const b = [[price.toFixed(2), '0'], [Math.max(0.01, price - 0.01).toFixed(2), '0']];
  const a = [];
  for (let i = 0; i < LEVELS; i++) {
    a.push([Math.min(0.99, price + i * 0.01).toFixed(2), String(LEVEL_SIZE)]);
  }
  return JSON.stringify({ at: atMs, k: 'book', t: token, b, a });
}

/** Deterministic opaque tokens per probe (no collisions across corpora). */
export function tokensFor(probe) {
  const h = (s) => {
    let x = 2166136261;
    for (const c of s) {
      x ^= c.charCodeAt(0);
      x = Math.imul(x, 16777619);
    }
    return (x >>> 0).toString(16).padStart(8, '0');
  };
  return { up: `tok_${h(probe + '-up')}`, down: `tok_${h(probe + '-down')}` };
}

/** Build one probe's jsonl at `dir/<probe>.jsonl`. Returns the schedule. */
export function buildProbe(dir, probe) {
  const cfg = PROBES[probe];
  const { up, down } = tokensFor(probe);
  const startMs = 1789811100000; // fixed epoch; the engine is corpus-relative
  const expiryMs = startMs + ROUND_SEC * 1000;
  const roundSlot = Math.floor(startMs / 1000 / 900);
  // Books run 5s PAST expiry so the clock definitively crosses it with events
  // on the wire (settlement ticks need a live now_ms past expiresAtMs).
  const endMs = expiryMs + 5000;
  const lines = [];
  lines.push(JSON.stringify({
    at: startMs,
    k: 'round',
    m: [{
      asset: 'BTC',
      conditionId: `cond_${probe}`,
      downPrice: (1 - ENTRY_PRICE).toFixed(2),
      downTokenId: down,
      expiresAtMs: expiryMs,
      negRisk: false,
      question: `oracle ruler ${probe}`,
      questionId: `q_${probe}`,
      roundSlot,
      upPrice: ENTRY_PRICE.toFixed(2),
      upTokenId: up,
    }],
  }));
  const settle = cfg.asksOnlyAtSec != null;
  const phantom = cfg.phantomAtSec != null;
  const downJumped = (sec) =>
    (settle && sec >= cfg.asksOnlyAtSec) || (phantom && sec >= cfg.phantomAtSec);
  for (let t = 1000; t <= endMs - startMs; t += 1000) {
    const at = startMs + t;
    const sec = t / 1000;
    // UP token:
    if (settle && sec >= cfg.asksOnlyAtSec) {
      lines.push(bookAsksOnly(at, up, cfg.upAsk));
    } else if (phantom && sec >= cfg.phantomAtSec) {
      lines.push(bookPhantom(at, up, cfg.upMark));
    } else if (sec < FILL_TRIGGER_SEC) {
      // Entry window: bid 0.30 / ask 0.31 — the probe's resting bid does NOT
      // cross at submit; it fills when the ask drops (next branch).
      const b = [];
      const a = [];
      for (let i = 0; i < LEVELS; i++) {
        b.push([(ENTRY_PRICE - i * 0.01).toFixed(2), String(LEVEL_SIZE)]);
        a.push([(ENTRY_PRICE + 0.01 + i * 0.01).toFixed(2), String(LEVEL_SIZE)]);
      }
      lines.push(JSON.stringify({ at, k: 'book', t: up, b, a }));
    } else if (!settle && !phantom && cfg.jumpAtSec != null && sec >= cfg.jumpAtSec) {
      lines.push(bookTwoSided(at, up, cfg.jump));
    } else {
      lines.push(bookTwoSided(at, up, ENTRY_PRICE));
    }
    // DOWN token (mirror, two-sided throughout so the settlement engine can
    // always price it; its jump decides the winner in the settle arms):
    if (downJumped(sec)) {
      lines.push(bookTwoSided(at, down, cfg.downJump));
    } else {
      lines.push(bookTwoSided(at, down, 1 - ENTRY_PRICE));
    }
  }
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `${probe}.jsonl`);
  writeFileSync(path, lines.join('\n') + '\n');
  return { path, startMs, expiryMs, up, down };
}

/** Closed-form per-share net for a taker exit at `exitPrice` after a maker
 *  entry at ENTRY_PRICE (fee 0). Settlement payouts (exit 1 / 0) carry zero
 *  fee under the formula, so one expression covers every arm. */
export function expectedNetPerShare(exitPrice) {
  return Number(exitPrice) - ENTRY_PRICE - feePerShareLegacy(exitPrice);
}
