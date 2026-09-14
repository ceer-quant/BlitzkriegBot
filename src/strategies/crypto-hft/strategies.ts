/**
 * 5 Strategies for Polymarket Crypto Markets
 *
 * Each strategy:
 *  - Uses real orderbook data (OBI, spread, depth)
 *  - Specifies its preferred order mode (maker/taker/fok/maker_then_taker)
 *  - Logs features for post-trade analysis
 *  - Has individually tunable config
 *
 * 1. Momentum    — Spot moved, poly lagging → maker_then_taker entry
 * 2. Reversion   — Poly overshot on noise   → maker entry (patient, cheap)
 * 3. Penny Clip  — Oscillating in zone, buy dips → maker entry (V4 from firstorder)
 * 4. Expiry Fade — Near expiry, no trend    → taker entry (speed, last chance)
 * 5. Poly Momentum — Pure Polymarket price momentum → maker entry
 */

import type {
  CryptoMarket,
  TradeSignal,
  SignalDirection,
  OrderMode,
  OrderbookSnapshot,
} from './types.js';

// ── Helpers ─────────────────────────────────────────────────────────────────

/** Rolling price buffer for oscillation detection */
export interface PriceBuffer {
  prices: Array<{ price: number; ts: number }>;
  push(price: number, ts?: number): void;
  /** Count direction reversals in last N seconds */
  reversals(windowSec: number, minStep: number): number;
  /** Price range in last N seconds */
  range(windowSec: number): number;
  /** Mean price in last N seconds */
  mean(windowSec: number): number;
  /** Movement in last N seconds as pct */
  movePct(windowSec: number): number;
}

export function createPriceBuffer(maxAgeSec = 180): PriceBuffer {
  const prices: Array<{ price: number; ts: number }> = [];

  function prune() {
    const cutoff = Date.now() - maxAgeSec * 1000;
    while (prices.length > 0 && prices[prices.length - 1].ts < cutoff) {
      prices.pop();
    }
    if (prices.length > 2000) {
      prices.length = 2000;
    }
  }

  function inWindow(windowSec: number): Array<{ price: number; ts: number }> {
    const cutoff = Date.now() - windowSec * 1000;
    return prices.filter((p) => p.ts >= cutoff);
  }

  return {
    prices,

    push(price, ts = Date.now()) {
      prices.unshift({ price, ts });
      prune();
    },

    reversals(windowSec, minStep) {
      const window = inWindow(windowSec);
      if (window.length < 3) return 0;
      let count = 0;
      let lastDir: 'up' | 'down' | null = null;

      for (let i = 1; i < window.length; i++) {
        const diff = window[i - 1].price - window[i].price;
        if (Math.abs(diff) < minStep) continue;
        const dir = diff > 0 ? 'up' : 'down';
        if (lastDir && dir !== lastDir) count++;
        lastDir = dir;
      }
      return count;
    },

    range(windowSec) {
      const window = inWindow(windowSec);
      if (window.length === 0) return 0;
      const vals = window.map((p) => p.price);
      return Math.max(...vals) - Math.min(...vals);
    },

    mean(windowSec) {
      const window = inWindow(windowSec);
      if (window.length === 0) return 0;
      return window.reduce((s, p) => s + p.price, 0) / window.length;
    },

    movePct(windowSec) {
      const window = inWindow(windowSec);
      if (window.length < 2) return 0;
      const newest = window[0].price;
      const oldest = window[window.length - 1].price;
      if (oldest === 0) return 0;
      return ((newest - oldest) / oldest) * 100;
    },
  };
}

// =============================================================================
// STRATEGY 1: MOMENTUM
// =============================================================================

export interface MomentumConfig {
  /** Min spot move % to trigger (default 0.15) */
  minSpotMovePct: number;
  /** Max poly price staleness (seconds, default 5) */
  maxPolyStaleSec: number;
  /** Min lag between expected fair value and current poly price (cents, default 0.02) */
  minLagCents: number;
  /** Max spread to enter (pct, default 2.0) */
  maxSpreadPct: number;
  /** Spot move window (seconds, default 30) */
  spotWindowSec: number;
}

export const DEFAULT_MOMENTUM: MomentumConfig = {
  minSpotMovePct: 0.03,
  maxPolyStaleSec: 15,
  minLagCents: 0.01,
  maxSpreadPct: 3.0,
  spotWindowSec: 15,
};

export function evaluateMomentum(
  market: CryptoMarket,
  spotMovePct: number,
  spotWindowSec: number,
  book: OrderbookSnapshot | null,
  polyAgeSec: number,
  cfg: MomentumConfig = DEFAULT_MOMENTUM
): TradeSignal | null {
  if (Math.abs(spotMovePct) < cfg.minSpotMovePct) return null;
  if (polyAgeSec > cfg.maxPolyStaleSec) return null;
  if (book && book.spreadPct > cfg.maxSpreadPct) return null;

  const direction: SignalDirection = spotMovePct > 0 ? 'up' : 'down';
  const tokenId = direction === 'up' ? market.upTokenId : market.downTokenId;
  const price = direction === 'up' ? market.upPrice : market.downPrice;

  // Lag calculation: expected poly price based on spot move
  // For5-min binary: 0.10% spot move should push poly ~5 cents
  const expectedPolyPrice = 0.50 + Math.abs(spotMovePct) / 100 * 50;
  const lagCents = expectedPolyPrice - price;
  if (lagCents < cfg.minLagCents) return null;

  const confidence = Math.min(1, Math.abs(spotMovePct) / 0.15);
  const obi = book?.obi ?? 0;

  return {
    strategy: 'momentum',
    asset: market.asset,
    direction,
    tokenId,
    conditionId: market.conditionId,
    price,
    confidence,
    reason: `Spot ${spotMovePct > 0 ? '+' : ''}${spotMovePct.toFixed(3)}% in ${spotWindowSec}s, lag ${lagCents.toFixed(3)}`,
    orderMode: 'maker',
    features: {
      spotMovePct,
      spotWindowSec,
      polyAgeSec,
      obi,
      lagCents,
      spread: book?.spreadPct ?? 0,
      price,
    },
    timestamp: Date.now(),
  };
}

// =============================================================================
// STRATEGY 2: MEAN REVERSION
// =============================================================================

export interface MeanReversionConfig {
  /** Buy when token is this cheap or less (default 0.30) */
  cheapThreshold: number;
  /** Fade when token is this expensive (default 0.72) */
  expensiveThreshold: number;
  /** Min seconds into round (default 120 — spreads stabilized) */
  minRoundAgeSec: number;
  /** Only revert if spot is calm (max spot move %, default 0.08) */
  maxSpotMovePct: number;
  /** Min OBI in our favor (default -0.1 — don't fight order flow) */
  minObi: number;
}

export const DEFAULT_MEAN_REVERSION: MeanReversionConfig = {
  cheapThreshold: 0.35,
  expensiveThreshold: 0.65,
  minRoundAgeSec: 30,
  maxSpotMovePct: 0.15,
  minObi: -0.2,
};

export function evaluateMeanReversion(
  market: CryptoMarket,
  spotMovePct: number,
  roundAgeSec: number,
  book: OrderbookSnapshot | null,
  cfg: MeanReversionConfig = DEFAULT_MEAN_REVERSION
): TradeSignal | null {
  if (roundAgeSec < cfg.minRoundAgeSec) return null;
  if (Math.abs(spotMovePct) > cfg.maxSpotMovePct) return null;

  let direction: SignalDirection;
  let tokenId: string;
  let price: number;

  if (market.upPrice <= cfg.cheapThreshold) {
    direction = 'up';
    tokenId = market.upTokenId;
    price = market.upPrice;
  } else if (market.downPrice <= cfg.cheapThreshold) {
    direction = 'down';
    tokenId = market.downTokenId;
    price = market.downPrice;
  } else if (market.upPrice >= cfg.expensiveThreshold) {
    direction = 'down';
    tokenId = market.downTokenId;
    price = market.downPrice;
  } else if (market.downPrice >= cfg.expensiveThreshold) {
    direction = 'up';
    tokenId = market.upTokenId;
    price = market.upPrice;
  } else {
    return null;
  }

  // Don't fight order flow — check OBI
  const obi = book?.obi ?? 0;
  if (obi < cfg.minObi) return null;

  const confidence = Math.min(1, (1 - price) * 1.5);

  return {
    strategy: 'mean_reversion',
    asset: market.asset,
    direction,
    tokenId,
    conditionId: market.conditionId,
    price,
    confidence,
    reason: `${direction.toUpperCase()} at ${price.toFixed(2)}, spot calm (${spotMovePct.toFixed(3)}%), OBI ${obi.toFixed(2)}`,
    orderMode: 'maker', // Patient — post in spread, 0% fee
    features: {
      spotMovePct,
      roundAgeSec,
      obi,
      spread: book?.spreadPct ?? 0,
      price,
      upPrice: market.upPrice,
      downPrice: market.downPrice,
    },
    timestamp: Date.now(),
  };
}

// =============================================================================
// STRATEGY 3: PENNY CLIPPER (ported from firstorder V4)
// =============================================================================

export interface PennyClipperConfig {
  /** Price zone: min (default 0.08) */
  priceMin: number;
  /** Price zone: max (default 0.50) */
  priceMax: number;
  /** Max spread to trade (cents, default 0.02) */
  maxSpread: number;
  /** Min oscillation range in window (cents, default 0.03) */
  minOscRange: number;
  /** Min reversals in window (default 3) */
  minReversals: number;
  /** Min step to count as reversal (cents, default 0.01) */
  reversalMinStep: number;
  /** Entry discount: must be this many cents below mean (default 0.01) */
  entryDiscount: number;
  /** Lookback window for oscillation (seconds, default 30) */
  oscWindowSec: number;
  /** Confirmation: spot moving toward our direction in last N sec (default 10) */
  confirmWindowSec: number;
}

export const DEFAULT_PENNY_CLIPPER: PennyClipperConfig = {
  priceMin: 0.05,
  priceMax: 0.60,
  maxSpread: 0.03,
  minOscRange: 0.02,
  minReversals: 2,
  reversalMinStep: 0.01,
  entryDiscount: 0.005,
  oscWindowSec: 30,
  confirmWindowSec: 5,
};

export function evaluatePennyClipper(
  market: CryptoMarket,
  spotBuffer: PriceBuffer,
  polyBuffer: PriceBuffer,
  book: OrderbookSnapshot | null,
  cfg: PennyClipperConfig = DEFAULT_PENNY_CLIPPER
): TradeSignal | null {
  if (!book) return null;
  if (book.spread > cfg.maxSpread) return null;

  // Check both sides for price-zone candidates
  const candidates: Array<{ dir: SignalDirection; tokenId: string; price: number }> = [];
  if (market.upPrice >= cfg.priceMin && market.upPrice <= cfg.priceMax) {
    candidates.push({ dir: 'up', tokenId: market.upTokenId, price: market.upPrice });
  }
  if (market.downPrice >= cfg.priceMin && market.downPrice <= cfg.priceMax) {
    candidates.push({ dir: 'down', tokenId: market.downTokenId, price: market.downPrice });
  }
  if (candidates.length === 0) return null;

  // Check oscillation in poly price buffer
  const oscRange = polyBuffer.range(cfg.oscWindowSec);
  if (oscRange < cfg.minOscRange) return null;

  const reversals = polyBuffer.reversals(cfg.oscWindowSec, cfg.reversalMinStep);
  if (reversals < cfg.minReversals) return null;

  // Pick the candidate with the best discount below mean
  const mean = polyBuffer.mean(cfg.oscWindowSec);
  let best: (typeof candidates)[0] | null = null;
  let bestDiscount = 0;

  for (const c of candidates) {
    const discount = mean - c.price;
    if (discount >= cfg.entryDiscount && discount > bestDiscount) {
      best = c;
      bestDiscount = discount;
    }
  }
  if (!best) return null;

  // Confirm: spot should be moving in our direction recently
  const spotMoveRecent = spotBuffer.movePct(cfg.confirmWindowSec);
  const spotConfirms = best.dir === 'up' ? spotMoveRecent > 0 : spotMoveRecent < 0;
  if (!spotConfirms) return null;

  const confidence = Math.min(1, (reversals / 5) * (oscRange / 0.05));

  return {
    strategy: 'penny_clipper',
    asset: market.asset,
    direction: best.dir,
    tokenId: best.tokenId,
    conditionId: market.conditionId,
    price: best.price,
    confidence,
    reason: `${best.dir.toUpperCase()} at ${best.price.toFixed(2)}, ${reversals} reversals, ${(oscRange * 100).toFixed(0)}c range, ${(bestDiscount * 100).toFixed(0)}c below mean`,
    orderMode: 'maker', // Post at best bid, 0% fee — this IS the edge
    features: {
      oscRange,
      reversals,
      discount: bestDiscount,
      mean,
      spotMoveRecent,
      spread: book.spread,
      obi: book.obi,
      price: best.price,
    },
    timestamp: Date.now(),
  };
}

// =============================================================================
// STRATEGY 4: EXPIRY FADE
// =============================================================================

export interface ExpiryFadeConfig {
  /** Max seconds before expiry to trigger (default 300 = 5 min) */
  windowSec: number;
  /** Min seconds before expiry (default 60 — don't enter too late) */
  minSecLeft: number;
  /** Min distance from 0.50 to consider fading (default 0.15) */
  minSkewFromMid: number;
  /** Max recent spot move % (default 0.06 — only when spot is flat) */
  maxRecentSpotMovePct: number;
  /** Max spread to enter (default 2.5%) */
  maxSpreadPct: number;
}

export const DEFAULT_EXPIRY_FADE: ExpiryFadeConfig = {
  windowSec: 300,
  minSecLeft: 60,
  minSkewFromMid: 0.10,
  maxRecentSpotMovePct: 0.10,
  maxSpreadPct: 3.0,
};

export function evaluateExpiryFade(
  market: CryptoMarket,
  spotMovePct: number,
  book: OrderbookSnapshot | null,
  cfg: ExpiryFadeConfig = DEFAULT_EXPIRY_FADE
): TradeSignal | null {
  const secsToExpiry = (market.expiresAt - Date.now()) / 1000;
  if (secsToExpiry > cfg.windowSec || secsToExpiry < cfg.minSecLeft) return null;
  if (Math.abs(spotMovePct) > cfg.maxRecentSpotMovePct) return null;
  if (book && book.spreadPct > cfg.maxSpreadPct) return null;

  const upSkew = Math.abs(market.upPrice - 0.50);
  const downSkew = Math.abs(market.downPrice - 0.50);
  const maxSkew = Math.max(upSkew, downSkew);
  if (maxSkew < cfg.minSkewFromMid) return null;

  // Buy the cheap (underpriced) side
  let direction: SignalDirection;
  let tokenId: string;
  let price: number;

  if (market.upPrice < market.downPrice) {
    direction = 'up';
    tokenId = market.upTokenId;
    price = market.upPrice;
  } else {
    direction = 'down';
    tokenId = market.downTokenId;
    price = market.downPrice;
  }

  const minsLeft = (secsToExpiry / 60).toFixed(1);
  const confidence = Math.min(1, maxSkew * 3);

  return {
    strategy: 'expiry_fade',
    asset: market.asset,
    direction,
    tokenId,
    conditionId: market.conditionId,
    price,
    confidence,
    reason: `${minsLeft}min left, ${direction.toUpperCase()} at ${price.toFixed(2)}, skew ${(maxSkew * 100).toFixed(0)}c`,
    orderMode: 'taker', // Speed — limited time to get filled
    features: {
      secsToExpiry,
      skew: maxSkew,
      spotMovePct,
      obi: book?.obi ?? 0,
      spread: book?.spreadPct ?? 0,
      price,
    },
    timestamp: Date.now(),
  };
}

// =============================================================================
// STRATEGY 5: POLY MOMENTUM (pure Polymarket data)
// =============================================================================

export interface PolyMomentumConfig {
  /** Min price change in poly buffer to trigger (cents, default 0.02) */
  minPolyMoveCents: number;
  /** Window to measure poly momentum (seconds, default 10) */
  polyMomentumWindowSec: number;
  /** Max spread to enter (cents, default 0.03) */
  maxSpread: number;
  /** Min price to enter (don't buy very cheap tokens, default 0.10) */
  minPrice: number;
  /** Max price to enter (don't buy very expensive tokens, default 0.60) */
  maxPrice: number;
}

export const DEFAULT_POLY_MOMENTUM: PolyMomentumConfig = {
  minPolyMoveCents: 0.02,
  polyMomentumWindowSec: 10,
  maxSpread: 0.03,
  minPrice: 0.10,
  maxPrice: 0.60,
};

export function evaluatePolyMomentum(
  market: CryptoMarket,
  polyBuffer: PriceBuffer,
  book: OrderbookSnapshot | null,
  cfg: PolyMomentumConfig = DEFAULT_POLY_MOMENTUM
): TradeSignal | null {
  if (!book) return null;
  if (book.spread > cfg.maxSpread) return null;

  // Measure Polymarket price momentum
  const polyMove = polyBuffer.movePct(cfg.polyMomentumWindowSec);
  const polyMoveCents = Math.abs(polyMove) * market.upPrice; // approximate cents

  if (polyMoveCents < cfg.minPolyMoveCents) return null;

  // Direction follows poly momentum
  let direction: SignalDirection;
  let tokenId: string;
  let price: number;

  if (polyMove > 0) {
    direction = 'up';
    tokenId = market.upTokenId;
    price = market.upPrice;
  } else {
    direction = 'down';
    tokenId = market.downTokenId;
    price = market.downPrice;
  }

  // Filter by price range
  if (price < cfg.minPrice || price > cfg.maxPrice) return null;

  const confidence = Math.min(1, polyMoveCents / 0.05);

  return {
    strategy: 'poly_momentum',
    asset: market.asset,
    direction,
    tokenId,
    conditionId: market.conditionId,
    price,
    confidence,
    reason: `Poly ${direction} ${(polyMoveCents * 100).toFixed(1)}c in ${cfg.polyMomentumWindowSec}s, price ${price.toFixed(2)}`,
    orderMode: 'maker',
    features: {
      polyMove: polyMoveCents,
      polyWindow: cfg.polyMomentumWindowSec,
      obi: book.obi,
      spread: book.spreadPct,
      price,
      upPrice: market.upPrice,
      downPrice: market.downPrice,
    },
    timestamp: Date.now(),
  };
}

// =============================================================================
// STRATEGY: SHARP REVERSAL
// =============================================================================
// Core idea: When a token that was trading high (>0.55) suddenly drops to ≤0.35,
// it's likely an overreaction. Buy at a deep discount and sell on recovery.
// - Track how long price stayed above 0.55
// - When it drops to ≤0.35 → place limit buy at 0.25
// - If filled → place limit sell at 0.70

export interface SharpReversalConfig {
  /** Min price to be considered "high" (default 0.55) */
  highThreshold: number;
  /** Min time price must stay above highThreshold (ms, default 300_000 = 5 min) */
  minHighDurationMs: number;
  /** Price must drop to this level or below to trigger (default 0.35) */
  dropThreshold: number;
  /** Entry price for limit buy (default 0.25) */
  entryPrice: number;
  /** Exit price for limit sell (default 0.70) */
  exitPrice: number;
  /** Max spread to enter (default 0.05) */
  maxSpread: number;
}

export const DEFAULT_SHARP_REVERSAL: SharpReversalConfig = {
  highThreshold: 0.55,
  minHighDurationMs: 300_000, // 5 minutes
  dropThreshold: 0.35,
  entryPrice: 0.25,
  exitPrice: 0.70,
  maxSpread: 0.05,
};

export function evaluateSharpReversal(
  market: CryptoMarket,
  polyBuffer: PriceBuffer,
  book: OrderbookSnapshot | null,
  cfg: SharpReversalConfig = DEFAULT_SHARP_REVERSAL
): TradeSignal | null {
  if (!book) return null;
  if (book.spread > cfg.maxSpread) return null;

  // Check both sides
  const candidates: Array<{
    dir: SignalDirection;
    tokenId: string;
    price: number;
    currentPrice: number;
  }> = [];

  // Check UP token
  if (market.upPrice <= cfg.dropThreshold) {
    // Was it high before? Check poly buffer for history
    const highDuration = getHighDuration(polyBuffer, cfg.highThreshold);
    if (highDuration >= cfg.minHighDurationMs) {
      candidates.push({
        dir: 'up',
        tokenId: market.upTokenId,
        price: cfg.entryPrice,
        currentPrice: market.upPrice,
      });
    }
  }

  // Check DOWN token
  if (market.downPrice <= cfg.dropThreshold) {
    const highDuration = getHighDuration(polyBuffer, cfg.highThreshold);
    if (highDuration >= cfg.minHighDurationMs) {
      candidates.push({
        dir: 'down',
        tokenId: market.downTokenId,
        price: cfg.entryPrice,
        currentPrice: market.downPrice,
      });
    }
  }

  if (candidates.length === 0) return null;

  // Pick the one with the biggest drop (most oversold)
  const best = candidates.reduce((a, b) =>
    a.currentPrice < b.currentPrice ? a : b
  );

  const confidence = Math.min(1, (cfg.highThreshold - best.currentPrice) / 0.20);

  return {
    strategy: 'sharp_reversal',
    asset: market.asset,
    direction: best.dir,
    tokenId: best.tokenId,
    conditionId: market.conditionId,
    price: best.price,
    confidence,
    reason: `${best.dir.toUpperCase()} dropped to ${best.currentPrice.toFixed(2)} from ≥${cfg.highThreshold}, limit buy at ${cfg.entryPrice}`,
    orderMode: 'maker',
    features: {
      currentPrice: best.currentPrice,
      entryPrice: cfg.entryPrice,
      exitPrice: cfg.exitPrice,
      highThreshold: cfg.highThreshold,
      spread: book.spread,
    },
    timestamp: Date.now(),
  };
}

/** Get how long price stayed above threshold in the buffer (ms) */
function getHighDuration(buffer: PriceBuffer, threshold: number): number {
  const now = Date.now();
  const prices = buffer.prices;
  if (prices.length === 0) return 0;

  // Buffer stores newest first (index 0 = newest)
  // Check if current (newest) price is above threshold
  if (prices[0].price < threshold) return 0;

  // Walk backwards from newest to find when price first dropped below threshold
  for (let i = 1; i < prices.length; i++) {
    if (prices[i].price < threshold) {
      // Found the point where it dropped below, duration = now - that point's time
      return now - prices[i].ts;
    }
  }

  // All prices in buffer are ≥ threshold, duration = now - oldest price time
  return now - prices[prices.length - 1].ts;
}

// =============================================================================
// STRATEGY: SPREAD ARBITRAGE (mean reversion on Polymarket tokens)
// =============================================================================
// Core idea: When price rises, buy the CHEAP side. When it falls, sell.
// - Price goes UP → DOWN tokens become cheap → buy DOWN
// - Price goes DOWN → UP tokens become cheap → buy UP
// - Profit from oscillation, not from trend following
// - Watch 1-min K-line for entry/exit timing

export interface SpreadArbConfig {
  /** Min Binance spot move % to confirm trend (legacy) */
  minTrendPct: number;
  /** Min discount from 0.50 to enter (legacy) */
  minDiscount: number;
  /** Legacy maker limit factor (superseded by trendEntryPrice) */
  entryFactor: number;
  /** Price a token must hold above to confirm a trend */
  trendMinPrice: number;
  /** How long (seconds) the token must hold above trendMinPrice to confirm */
  trendConfirmSec: number;
  /** Resting bid price: if >0, a fixed absolute price; if 0, use trendEntryFactor */
  trendEntryPrice: number;
  /** Resting bid = currentPrice * this factor (used when trendEntryPrice == 0). Higher = fills more */
  trendEntryFactor: number;
  /** Never rest a bid above this price — buying near-certainties has terrible risk/reward */
  trendMaxEntryPrice: number;
  /** Below this price a confirmed trend is considered reversed (not a pullback) */
  trendBrokenPrice: number;
}

export const DEFAULT_SPREAD_ARB: SpreadArbConfig = {
  minTrendPct: 0.005,
  minDiscount: 0.02,
  entryFactor: 0.95,
  trendMinPrice: 0.55,
  // A 30s streak is just noise on these tokens. Require the token to hold above
  // trendMinPrice for 60s.
  trendConfirmSec: 60,
  // 0 = use the relative factor below; set to e.g. 0.25 for a fixed deep bid.
  trendEntryPrice: 0,
  // 0.98: bid just below the market so it actually fills (near-taker). Deeper
  // bids need a 30-40% pullback that almost never happens.
  trendEntryFactor: 0.98,
  // Cap the bid to avoid buying near-certainties (terrible risk/reward).
  trendMaxEntryPrice: 0.45,
  trendBrokenPrice: 0.35,
};

/**
 * Trend-confirmed dip buyer.
 *
 * Thesis: while a token holds above ~0.55 for a sustained period it is the
 * favourite for the round. Instead of waiting for it to already collapse below
 * 0.40 (old behaviour), rest a deep bid (default 0.25) UNDER the favourite as
 * soon as the trend is confirmed, so a temporary wick/dip fills us cheaply.
 * A trailing take-profit then rides the rebound toward 0.40–1.00.
 *
 * `trendConfirmed` holds the tokenIds that have held >= trendMinPrice long
 * enough this round (streak tracking lives in the engine).
 */
export function evaluateSpreadArb(
  market: CryptoMarket,
  spotMovePct: number,
  polyBuffer: PriceBuffer,
  books: { up: OrderbookSnapshot | null; down: OrderbookSnapshot | null },
  cfg: SpreadArbConfig = DEFAULT_SPREAD_ARB,
  trendConfirmed?: Set<string>
): TradeSignal | null {
  if (!trendConfirmed || trendConfirmed.size === 0) return null;

  const now = Date.now();

  const consider = (tokenId: string, price: number, direction: SignalDirection): TradeSignal | null => {
    if (!tokenId || !trendConfirmed.has(tokenId)) return null;
    // Require a FRESH book on both sides. Pricing off the scanner's last price
    // produced fills far above the true mid (e.g. entry 0.41 while mid was 0.23).
    const book = direction === 'up' ? books.up : books.down;
    if (!book || book.bids.length === 0 || book.asks.length === 0 || book.midPrice <= 0) return null;
    const ref = book.midPrice;
    // Regime change: the trend has reversed, not pulled back — do not buy.
    if (cfg.trendBrokenPrice && ref < cfg.trendBrokenPrice) return null;
    // Bid = factor * mid, but never above the live best bid (maker discipline).
    const raw = cfg.trendEntryPrice > 0 ? cfg.trendEntryPrice : ref * (cfg.trendEntryFactor || 0.6);
    let entryPrice = Math.min(0.9, Math.max(0.05, Math.round(raw * 100) / 100));
    if (book.bestBid > 0 && entryPrice > book.bestBid) entryPrice = Math.round(book.bestBid * 100) / 100;
    // Must rest strictly below the mid.
    if (entryPrice >= ref) return null;
    // Cap: never buy near-certainties (terrible risk/reward if it reverses).
    if (cfg.trendMaxEntryPrice && entryPrice > cfg.trendMaxEntryPrice) return null;
    return {
      strategy: 'spread_arb',
      asset: market.asset,
      direction,
      tokenId,
      conditionId: market.conditionId,
      price: entryPrice,
      confidence: 1,
      reason: `${direction.toUpperCase()} trend confirmed (held >${cfg.trendMinPrice} for >=${cfg.trendConfirmSec}s), resting bid ${entryPrice} (${(entryPrice / ref * 100).toFixed(0)}% of mid ${ref.toFixed(2)})`,
      orderMode: 'maker_then_taker',
      features: {
        spotMovePct,
        trendEntryPrice: entryPrice,
        trendMinPrice: cfg.trendMinPrice,
        trendConfirmSec: cfg.trendConfirmSec,
      },
      timestamp: now,
    };
  };

  return consider(market.upTokenId, market.upPrice, 'up')
    || consider(market.downTokenId, market.downPrice, 'down');
}
