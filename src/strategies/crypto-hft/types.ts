/**
 * Crypto HFT — Types for 15-minute Polymarket crypto market trading
 *
 * Ported from firstorder.rs with all real thresholds and execution patterns.
 */

// ── Order Execution ─────────────────────────────────────────────────────────

/** How to execute an entry or exit order */
export type OrderMode =
  | 'maker'        // GTC postOnly — 0% fee, rejected if would cross
  | 'taker'        // GTC — crosses spread, pays taker fee
  | 'fok'          // Fill-or-Kill — immediate full fill or cancel
  | 'maker_then_taker';  // Try maker first, escalate to taker on timeout

export interface OrderExecution {
  mode: OrderMode;
  /** Maker timeout before escalating to taker (ms). Only for maker_then_taker. */
  makerTimeoutMs: number;
  /** Price buffer for taker orders: +/- this many cents (default 0.01) */
  takerBufferCents: number;
  /** For maker exits: buffer below ask to post in spread */
  makerExitBufferCents: number;
}

// ── Taker Fee (Polymarket formula) ──────────────────────────────────────────

/** fee_per_share = 0.125 * (price * (1 - price))^2 */
export function takerFee(price: number): number {
  return 0.125 * Math.pow(price * (1 - price), 2);
}

export function takerFeePct(price: number): number {
  if (price === 0) return 0;
  return (takerFee(price) / price) * 100;
}

// ── Config ──────────────────────────────────────────────────────────────────

export interface CryptoHftConfig {
  /** Assets to trade */
  assets: string[];

  // ── Sizing ──
  sizeUsd: number;
  /** Min shares to survive taker fee round-trip */
  minShares: number;
  maxShares: number;
  maxPositionUsd: number;
  maxPositions: number;

  // ── Round timing ──
  roundDurationSec: number;
  /** Don't enter if fewer than this many seconds left */
  minTimeLeftSec: number;
  /** Stop resting entry bids this many seconds before minTimeLeftSec (avoid fills too close to expiry) */
  entryCutoffSec?: number;
  /** Don't enter in the first N seconds (spreads unstable) */
  minRoundAgeSec: number;
  /** Force exit at this many seconds before expiry */
  forceExitSec: number;
  /** In the final N seconds of a round, flush stale resting orders */
  roundEndClearSec?: number;
  /** Resting orders older than this (seconds) are cancelled during the round-end window */
  staleOrderAgeSec?: number;
  /** Warmup: don't trade for N seconds after engine start */
  warmupSec: number;

  // ── Entry execution ──
  entryOrder: OrderExecution;
  /** Max orderbook staleness before skipping entry (ms) */
  maxOrderbookStaleMs: number;

  // ── Exit execution ──
  exitOrder: OrderExecution;
  /** Use maker exits only for TP and TIME exits (not SL — speed matters) */
  makerExitsForTpOnly: boolean;
  /** Non-urgent exits (take_profit) post a maker offer first, then cross after
   *  exitOrder.makerTimeoutMs. Urgent exits (stop/force/trailing) always taker. */
  makerFirstExitEnabled?: boolean;
  /** A protective stop only fires when the executable bid is within this % of the
   *  mid — guards against selling into a one-tick pulled bid wick (default 8) */
  maxBidWickPct?: number;
  /** Cooldown between sell attempts (ms) */
  sellCooldownMs: number;
  /** Share buffer subtracted from exit size for rounding (e.g. 0.02) */
  exitShareBuffer: number;

  // ── Take Profit / Stop Loss ──
  takeProfitPct: number;
  stopLossPct: number;

  // ── Quick scalp exit ──
  quickProfitMinHoldSec: number;
  quickProfitMinPct: number;

  // ── Spot reversal exit ──
  spotReversalThresholdPct: number;

  // ── Ratchet floor (progressive giveback from confirmed high) ──
  ratchetEnabled: boolean;
  /** Number of consecutive ticks near high to confirm HWM */
  ratchetConfirmTicks: number;
  /** Tolerance % for HWM confirmation (within this % of high = "near") */
  ratchetConfirmTolerancePct: number;

  // ── Trailing stop ──
  trailingEnabled: boolean;

  // ── Time-aware trailing (tightens as expiry approaches) ──
  trailingLatePct: number;    // <3 min left
  trailingMidPct: number;     // 3-7 min left
  trailingWidePct: number;    // >7 min left

  // ── Advanced exits ──
  /** Exit if up >= this % and bid unchanged for staleSeconds */
  staleProfitPct: number;
  staleProfitBidUnchangedSec: number;
  /** Exit if at +N% for M seconds without progress */
  stagnantProfitPct: number;
  stagnantDurationSec: number;
  /** Exit on depth collapse: depth dropped this % while price dropping */
  depthCollapseThresholdPct: number;
  /** spread_arb maker limit = currentPrice * this factor (legacy) */
  spreadArbEntryFactor?: number;
  /** spread_arb trend confirmation price (token must hold above this) */
  trendMinPrice?: number;
  /** spread_arb: if a confirmed trend falls below this price it is treated as a
   *  regime change (reversal), not a pullback — bids cancelled + position exited */
  trendBrokenPrice?: number;
  /** Require Binance spot short-term momentum to agree with the token direction */
  momentumFilterEnabled?: boolean;
  /** Spot momentum lookback window (seconds) */
  momentumFilterWindowSec?: number;
  /** Max adverse spot move % tolerated (0 = must not be moving against at all) */
  momentumFilterMinPct?: number;
  /** Never fill a buy more than this % above the current mid (stale-price guard) */
  maxFillVsMidPct?: number;
  /** spread_arb seconds the token must hold above trendMinPrice */
  trendConfirmSec?: number;
  /** Fraction of the confirm window that must be above trendMinPrice (default 0.8) */
  trendRatio?: number;
  /** spread_arb fixed resting bid price once the trend is confirmed (0 = use factor) */
  trendEntryPrice?: number;
  /** spread_arb resting bid = currentPrice * factor (when trendEntryPrice is 0) */
  trendEntryFactor?: number;
  /** spread_arb: never rest a bid above this price (default 0.70) */
  trendMaxEntryPrice?: number;
  /** Cancel a resting bid when mid has fallen this % below it (default on) */
  cancelStaleBids?: boolean;
  /** Stale-bid threshold in % (default 5) */
  staleBidPct?: number;
  /** Trail high-profit positions as a percentage of the high instead of fixed points */
  proportionalTrailEnabled?: boolean;
  /** Giveback as % of high when proportional trailing applies (default 10) */
  proportionalTrailPct?: number;
  /** Only apply proportional trailing once high >= this % (default 15) */
  proportionalTrailMinPct?: number;
  /** Minimum giveback (points) for proportional trailing (default 3) */
  proportionalTrailMinGivebackPct?: number;
  /** Trailing only arms once high PnL reaches this % (default 5) — avoids noise exits */
  trailingMinHighPct?: number;
  /** Absolute minimum trailing giveback in points (default 5) */
  minTrailPct?: number;
  /** Grace period after open before any signal-based exit can fire (default 3s) */
  exitGraceSec?: number;
  /** Simple exit mode: only TP / wide SL / expiry. Disables breakeven, trailing,
   *  stale, stagnant, depth, spot-reversal and quick-profit exits. */
  simpleExitEnabled?: boolean;
  /** Dynamic stop: tighten the stop as expiry approaches (default on) */
  dynamicStopEnabled?: boolean;
  /** Begin tightening the stop when time left drops below this (default 300s = 5 min) */
  stopTightenStartSec?: number;
  /** Tightest stop (points) applied at the force-exit horizon (default 10) */
  stopMinPct?: number;

  // ── Risk controls (toggleable for A/B testing) ──
  /** Use a tighter hard stop instead of stopLossPct (default on) */
  tightStopEnabled?: boolean;
  /** Hard stop percent used when tightStopEnabled (default 12) */
  tightStopPct?: number;
  /** Reject entries right after a violent move (falling-knife filter) */
  crashFilterEnabled?: boolean;
  /** Require the ≥0.55 high reading to be at least this old before entering */
  crashFilterMinHighAgeSec?: number;
  /** Max adverse Binance spot move (%) over the lookback to allow entry */
  crashFilterMaxSpotMovePct?: number;
  /** Lookback seconds for the crash spot-move check */
  crashFilterLookbackSec?: number;
  /** Skip non-mandatory exits when the book gapped below the last tick */
  slippageGuardEnabled?: boolean;
  /** One-tick adverse move (%) that triggers the slippage guard */
  slippageGuardMaxPct?: number;

  // ── Risk ──
  maxDailyLossUsd: number;
  /** Cooldown after stop loss hit (seconds) */
  stopLossCooldownSec: number;
  /** Cooldown after the consecutive-loss circuit breaker trips (seconds) */
  breakerCooldownSec?: number;
  /** Cooldown after any exit before re-entering same coin+direction (seconds) */
  exitCooldownSec: number;
  /** Cooldown (seconds) before touching the SAME asset again after any exit, regardless of direction */
  assetCooldownSec?: number;
  /** Cooldown (seconds) after a LOSING exit before touching the same asset again */
  lossCooldownSec?: number;
  negRisk: boolean;
  dryRun: boolean;
}

// ── Orderbook ───────────────────────────────────────────────────────────────

export interface OrderbookSnapshot {
  tokenId: string;
  bids: Array<[number, number]>; // [price, size]
  asks: Array<[number, number]>;
  bidDepth: number;
  askDepth: number;
  obi: number;               // (bidDepth - askDepth) / (bidDepth + askDepth)
  spread: number;             // bestAsk - bestBid
  spreadPct: number;
  bestBid: number;
  bestAsk: number;
  midPrice: number;
  timestamp: number;
}

export type ObiCategory = 'bid_heavy' | 'bid_lean' | 'balanced' | 'ask_lean' | 'ask_heavy';

export function categorizeObi(obi: number): ObiCategory {
  if (obi > 0.3) return 'bid_heavy';
  if (obi > 0) return 'bid_lean';
  if (obi > -0.3) return 'balanced';
  if (obi > -0.6) return 'ask_lean';
  return 'ask_heavy';
}

// ── Market ──────────────────────────────────────────────────────────────────

export interface CryptoMarket {
  asset: string;
  conditionId: string;
  questionId: string;
  upTokenId: string;
  downTokenId: string;
  upPrice: number;
  downPrice: number;
  expiresAt: number;
  /** Current round slot (expiresAt / roundDuration) */
  roundSlot: number;
  negRisk: boolean;
  question: string;
}

export interface RoundState {
  slot: number;
  expiresAt: number;
  markets: CryptoMarket[];
  /** Seconds since round started */
  ageSec: number;
  /** Seconds until round expires */
  timeLeftSec: number;
}

// ── Signal ──────────────────────────────────────────────────────────────────

export type SignalDirection = 'up' | 'down';

export interface TradeSignal {
  strategy: string;
  asset: string;
  direction: SignalDirection;
  tokenId: string;
  conditionId: string;
  price: number;
  confidence: number;
  reason: string;
  /** Which order mode this strategy recommends */
  orderMode: OrderMode;
  /** Features that triggered the signal (for logging/analysis) */
  features: Record<string, number>;
  timestamp: number;
}

// ── Position ────────────────────────────────────────────────────────────────

export interface OpenPosition {
  id: string;
  strategy: string;
  asset: string;
  direction: SignalDirection;
  tokenId: string;
  conditionId: string;
  entryPrice: number;
  currentPrice: number;
  /** Price at the previous tick (used to detect one-tick flash gaps on exit) */
  prevPrice?: number;
  shares: number;
  costUsd: number;
  wasMakerEntry: boolean;
  entryFeePct: number;
  /** Fixed exit price for strategies like sharp_reversal (optional) */
  targetExitPrice?: number;

  // HWM tracking
  highWaterMark: number;
  /** Consecutive ticks near HWM for confirmation */
  hwmConfirmCount: number;
  confirmedHigh: number;

  // Timing
  enteredAt: number;
  expiresAt: number;

  // Bid staleness tracking (for stale profit exit)
  lastBidPrice: number;
  bidUnchangedSince: number;

  // Stagnant tracking
  lastProgressAt: number;
  lastProgressPct: number;

  // Depth tracking
  initialDepth: number;

  // PnL timeline
  highPnlPct: number;
  lowPnlPct: number;
  wasEverPositive: boolean;
}

export type ExitReason =
  | 'take_profit'
  | 'stop_loss'
  | 'ratchet_floor'
  | 'trailing_stop'
  | 'breakeven_lock'
  | 'depth_collapse'
  | 'stale_profit'
  | 'stagnant_profit'
  | 'time_exit'
  | 'force_exit'
  | 'spot_reversal'
  | 'quick_profit'
  | 'manual';

export interface ClosedPosition extends OpenPosition {
  exitPrice: number;
  exitReason: ExitReason;
  exitedAt: number;
  wasMakerExit: boolean;
  exitFeePct: number;
  pnlUsd: number;
  pnlPct: number;
  /** Net PnL after fees */
  netPnlUsd: number;
  netPnlPct: number;
  holdTimeSec: number;
}

// ── Stats ───────────────────────────────────────────────────────────────────

export interface HftStats {
  totalTrades: number;
  wins: number;
  losses: number;
  winRate: number;
  grossPnlUsd: number;
  feesUsd: number;
  netPnlUsd: number;
  dailyPnlUsd: number;
  openPositions: number;
  bestTradePct: number;
  worstTradePct: number;
  avgHoldTimeSec: number;
  makerEntryRate: number;
  makerExitRate: number;
  exitReasons: Record<string, number>;
}

// ── Presets ──────────────────────────────────────────────────────────────────

export interface StrategyPreset {
  name: string;
  description: string;
  config: Partial<CryptoHftConfig>;
  /** Which strategies to enable */
  strategies: Record<string, boolean>;
  createdAt: number;
}
