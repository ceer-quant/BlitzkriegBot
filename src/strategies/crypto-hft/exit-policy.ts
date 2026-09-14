/**
 * Exit Policy — pure, replayable exit decision logic.
 *
 * Both the live engine (positions.ts) and the offline shadow replay
 * (scripts/analyze-shadow.mjs via the built dist) call THIS module, so the
 * strategy that is backtested is byte-for-byte the strategy that trades.
 *
 * Valuation / trigger price discipline:
 *  - A long position can only ever be SOLD at the live best bid. Pricing exits
 *    off the mid overstates realizable PnL on thin books (a token can show a
 *    mid of 0.82 while the best bid is 0.59) — it produced "TP +100%" exits
 *    that actually filled at +44%. We therefore value the position, track the
 *    high-water mark and fire triggers off the EXECUTABLE bid.
 *  - A single resting bid can be pulled for one tick and flash far below the
 *    mid. Stop-loss is a protective, adverse-direction trigger, so it requires
 *    the mid to roughly confirm (bid not dislocated beyond maxBidWickPct);
 *    profit-side triggers need no such guard — a favourable wick only ever
 *    gives us a better fill.
 *
 * Everything here is a pure function of (state, book, clock, config); it never
 * places orders or touches I/O.
 */

import { takerFeePct } from './types.js';
import type { CryptoHftConfig, ExitReason, OrderbookSnapshot } from './types.js';

// ── Ratchet Floor Table (from firstorder.rs Jan 19 2026) ────────────────────
// confirmedHighPct → floorPct
const RATCHET_TABLE: Array<[number, number]> = [
  [100, 94],
  [50, 44],
  [40, 35],
  [30, 25],
  [25, 20],
  [20, 15],
  [15, 10],
  [10, 6],
  [8, 4],
  [6, 3],
  [5, 2],
  [4, 1],
  [3, 0],
  [2, -2],
  [1, -4],
];
const RATCHET_DEFAULT_FLOOR = -12;

export function getRatchetFloor(confirmedHighPct: number): number {
  for (const [threshold, floor] of RATCHET_TABLE) {
    if (confirmedHighPct >= threshold) return floor;
  }
  return RATCHET_DEFAULT_FLOOR;
}

// ── Trailing giveback tables ────────────────────────────────────────────────

export function getProfitTrailPct(highPnlPct: number, cfg?: CryptoHftConfig): number {
  let table: number;
  if (highPnlPct >= 50) table = 15;
  else if (highPnlPct >= 30) table = 12;
  else if (highPnlPct >= 20) table = 9;
  else if (highPnlPct >= 10) table = 6;
  else if (highPnlPct >= 5) table = 4;
  else if (highPnlPct >= 3) table = 3;
  else table = 2;

  if (cfg?.proportionalTrailEnabled !== false && highPnlPct >= (cfg?.proportionalTrailMinPct ?? 15)) {
    const prop = highPnlPct * ((cfg?.proportionalTrailPct ?? 10) / 100);
    const floor = cfg?.proportionalTrailMinGivebackPct ?? 3;
    return Math.max(floor, Math.min(table, prop));
  }
  return table;
}

export function getTimeTrailPct(timeLeftSec: number): number {
  if (timeLeftSec > 420) return 12;
  if (timeLeftSec > 180) return 8;
  return 6;
}

/**
 * Dynamic stop: full base stop while there is time for the trade to repair,
 * tightening linearly to stopMinPct over the final stopTightenStartSec.
 */
export function effectiveStopPct(base: number, timeLeftSec: number, cfg: CryptoHftConfig): number {
  if (cfg.dynamicStopEnabled === false) return base;
  const start = cfg.stopTightenStartSec ?? 300;
  const floorT = Math.min(cfg.forceExitSec ?? 120, Math.max(1, start - 1));
  const minPct = Math.min(base, cfg.stopMinPct ?? 10);
  if (timeLeftSec >= start) return base;
  if (timeLeftSec <= floorT) return minPct;
  const frac = (timeLeftSec - floorT) / (start - floorT);
  return minPct + (base - minPct) * frac;
}

const BREAKEVEN_LOCK_TRIGGER_PCT = 3;
const BREAKEVEN_LOCK_MIN_FLOOR_PCT = 0.5;

// ── Mutable per-position state the policy needs ─────────────────────────────

export interface ExitState {
  highPnlPct: number;
  lowPnlPct: number;
  wasEverPositive: boolean;
  highWaterMark: number;
  hwmConfirmCount: number;
  confirmedHigh: number;
  lastBidPrice: number;
  bidUnchangedSince: number;
  lastProgressAt: number;
  lastProgressPct: number;
  initialDepth: number;
}

export function createExitState(entryPrice: number, now: number): ExitState {
  return {
    highPnlPct: 0,
    lowPnlPct: 0,
    wasEverPositive: false,
    highWaterMark: entryPrice,
    hwmConfirmCount: 0,
    confirmedHigh: entryPrice,
    lastBidPrice: entryPrice,
    bidUnchangedSince: now,
    lastProgressAt: now,
    lastProgressPct: 0,
    initialDepth: 0,
  };
}

export function pnlPct(price: number, entryPrice: number): number {
  return entryPrice > 0 ? ((price - entryPrice) / entryPrice) * 100 : 0;
}

/**
 * The price at which a long can realistically be sold right now: the live best
 * bid. Falls back to mid only when there is no bid at all (then the position is
 * untradeable this tick). Returns 0 when no usable price exists.
 */
export function executableBid(book: OrderbookSnapshot | null): number {
  if (!book) return 0;
  if (book.bestBid > 0) return book.bestBid;
  return book.midPrice > 0 ? book.midPrice : 0;
}

/**
 * Update HWM / staleness / depth tracking from a fresh book. Called once per
 * tick BEFORE decideExit, using the same executable price the triggers use, so
 * recorded highPnlPct matches what we could actually have sold for.
 */
export function updateExitState(
  state: ExitState,
  entryPrice: number,
  book: OrderbookSnapshot | null,
  now: number,
  cfg: CryptoHftConfig
): void {
  if (!book || entryPrice <= 0) return;
  const val = executableBid(book);
  if (val <= 0) return;
  const pct = pnlPct(val, entryPrice);

  if (pct > state.highPnlPct) state.highPnlPct = pct;
  if (pct < state.lowPnlPct) state.lowPnlPct = pct;
  if (pct > 0) state.wasEverPositive = true;

  if (val > state.highWaterMark) {
    state.highWaterMark = val;
    state.hwmConfirmCount = 1;
  } else {
    const nearHigh = state.highWaterMark > 0
      && (Math.abs(val - state.highWaterMark) / state.highWaterMark * 100 < cfg.ratchetConfirmTolerancePct);
    if (nearHigh) {
      state.hwmConfirmCount++;
      if (state.hwmConfirmCount >= cfg.ratchetConfirmTicks) state.confirmedHigh = state.highWaterMark;
    } else {
      state.hwmConfirmCount = 0;
    }
  }

  if (state.initialDepth === 0) state.initialDepth = book.bidDepth + book.askDepth;
  if (book.bestBid !== state.lastBidPrice) {
    state.lastBidPrice = book.bestBid;
    state.bidUnchangedSince = now;
  }
  if (Math.abs(pct - state.lastProgressPct) > 1) {
    state.lastProgressAt = now;
    state.lastProgressPct = pct;
  }
}

export interface ExitDecision {
  reason: ExitReason;
  /** Post a maker offer (non-urgent) vs cross immediately (urgent / mandatory). */
  useMaker: boolean;
}

export interface ExitTickInput {
  entryPrice: number;
  book: OrderbookSnapshot | null;
  /** Last known valuation when no fresh book is available (mandatory exits only). */
  fallbackPrice?: number;
  timeLeftSec: number;
  holdSec: number;
  state: ExitState;
  now: number;
  cfg: CryptoHftConfig;
}

/**
 * True when the executable bid is a plausible price rather than a one-tick
 * pulled-quote wick: the bid/mid discount must not exceed maxBidWickPct.
 * Protective (stop) exits use this; profit exits don't need to.
 */
function bidConfirmedByMid(book: OrderbookSnapshot, cfg: CryptoHftConfig): boolean {
  if (book.bestBid <= 0 || book.midPrice <= 0) return false;
  const wick = (book.midPrice - book.bestBid) / book.midPrice;
  const maxWick = (cfg.maxBidWickPct ?? 8) / 100;
  return wick <= maxWick;
}

/**
 * Evaluate every exit rule for one position at one instant. Pure: returns a
 * decision (or null) and mutates nothing. Priority order is documented in
 * positions.ts; mandatory exits (force/stop/time) return useMaker=false.
 */
export function decideExit(input: ExitTickInput): ExitDecision | null {
  const { entryPrice, book, fallbackPrice, timeLeftSec, holdSec, state, now, cfg } = input;
  if (entryPrice <= 0) return null;

  const bid = executableBid(book);
  const usable = bid > 0 ? bid : (fallbackPrice && fallbackPrice > 0 ? fallbackPrice : 0);

  // 1. Force exit — absolute deadline. Must fire even with no fresh book so a
  // position is never carried into settlement; price uses the last known quote.
  if (timeLeftSec <= cfg.forceExitSec) {
    if (usable <= 0) return null;
    return { reason: 'force_exit', useMaker: false };
  }

  // Profit/protective triggers below all require a real executable bid.
  if (bid <= 0) return null;
  const pct = pnlPct(bid, entryPrice);

  // ── Simple mode: TP backstop / dynamic stop / trailing lock / expiry ──
  if (cfg.simpleExitEnabled !== false) {
    const tp = cfg.takeProfitPct;
    if (tp > 0 && tp < 9999 && pct >= tp) {
      // Profit target is non-urgent: try a zero-fee maker offer first, the engine
      // escalates to taker after exitOrder.makerTimeoutMs if it does not fill.
      return { reason: 'take_profit', useMaker: cfg.makerFirstExitEnabled !== false };
    }
    if (pct <= -effectiveStopPct(cfg.stopLossPct, timeLeftSec, cfg) && (!book || bidConfirmedByMid(book, cfg))) {
      return { reason: 'stop_loss', useMaker: false };
    }
    if (cfg.trailingEnabled && state.highPnlPct >= (cfg.trailingMinHighPct ?? 15)) {
      const profitTrail = getProfitTrailPct(state.highPnlPct, cfg);
      const timeTrail = getTimeTrailPct(timeLeftSec);
      const trail = Math.max(cfg.minTrailPct ?? 10, Math.min(profitTrail, timeTrail));
      if (state.highPnlPct - pct >= trail) {
        return { reason: 'trailing_stop', useMaker: false };
      }
    }
    // Time exit is mandatory near expiry: use the last known quote if the bid
    // momentarily disappears, rather than risking settlement exposure.
    if (timeLeftSec <= cfg.minTimeLeftSec) {
      if (usable <= 0) return null;
      return { reason: 'time_exit', useMaker: false };
    }
    return null;
  }

  // ── Full mode ──
  // Grace period right after a maker fill (book often temporarily dislocated).
  if (holdSec < (cfg.exitGraceSec ?? 3)) return null;

  // 2. Take profit — non-urgent: zero-fee maker offer first, taker on timeout.
  if (pct >= cfg.takeProfitPct) {
    return { reason: 'take_profit', useMaker: cfg.makerFirstExitEnabled !== false };
  }

  // 3. Stop loss — always taker, requires bid not to be an unconfirmed wick.
  const baseStopPct = cfg.tightStopEnabled !== false
    ? (cfg.tightStopPct ?? 12)
    : cfg.stopLossPct;
  const stopPct = effectiveStopPct(baseStopPct, timeLeftSec, cfg);
  if (pct <= -stopPct && (!book || bidConfirmedByMid(book, cfg))) {
    return { reason: 'stop_loss', useMaker: false };
  }

  // 4. Ratchet floor
  if (cfg.ratchetEnabled) {
    const confirmedHighPct = pnlPct(state.confirmedHigh, entryPrice);
    if (pct <= getRatchetFloor(confirmedHighPct)) {
      return { reason: 'ratchet_floor', useMaker: false };
    }
  }

  // 4b. Breakeven lock — fee-aware floor so the locked trade is net >= ~breakeven.
  if (state.highPnlPct >= BREAKEVEN_LOCK_TRIGGER_PCT) {
    const lockFloor = Math.max(BREAKEVEN_LOCK_MIN_FLOOR_PCT, takerFeePct(bid) + 0.2);
    if (pct <= lockFloor) {
      return { reason: 'breakeven_lock', useMaker: false };
    }
  }

  // 5. Trailing stop
  if (cfg.trailingEnabled && state.highPnlPct >= (cfg.trailingMinHighPct ?? 5)) {
    const profitTrail = getProfitTrailPct(state.highPnlPct, cfg);
    const timeTrail = getTimeTrailPct(timeLeftSec);
    const trail = Math.max(cfg.minTrailPct ?? 5, Math.min(profitTrail, timeTrail));
    if (state.highPnlPct - pct >= trail) {
      return { reason: 'trailing_stop', useMaker: false };
    }
  }

  // 6. Depth collapse
  if (book && state.initialDepth > 0) {
    const currentDepth = book.bidDepth + book.askDepth;
    const depthChangePct = ((currentDepth - state.initialDepth) / state.initialDepth) * 100;
    if (depthChangePct <= -cfg.depthCollapseThresholdPct && bid < state.highWaterMark && pct >= 2) {
      return { reason: 'depth_collapse', useMaker: false };
    }
  }

  // 7. Stale profit
  if (pct >= cfg.staleProfitPct) {
    const bidStaleSec = (now - state.bidUnchangedSince) / 1000;
    if (bidStaleSec >= cfg.staleProfitBidUnchangedSec) {
      return { reason: 'stale_profit', useMaker: true };
    }
  }

  // 8. Stagnant profit
  if (pct >= cfg.stagnantProfitPct && pct < cfg.takeProfitPct) {
    const stagnantSec = (now - state.lastProgressAt) / 1000;
    if (stagnantSec >= cfg.stagnantDurationSec) {
      return { reason: 'stagnant_profit', useMaker: true };
    }
  }

  // 9. Time exit (mandatory near expiry)
  if (timeLeftSec <= cfg.minTimeLeftSec) {
    if (usable <= 0) return null;
    return { reason: 'time_exit', useMaker: cfg.makerExitsForTpOnly === true };
  }

  return null;
}

/** Mid price only used as a reference (logs, target-price strategies). */
export function referenceMid(book: OrderbookSnapshot | null, fallback: number): number {
  return book && book.bids.length > 0 && book.asks.length > 0 && book.midPrice > 0 ? book.midPrice : fallback;
}
