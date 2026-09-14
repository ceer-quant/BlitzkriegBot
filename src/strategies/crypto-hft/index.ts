/**
 * Crypto HFT Engine — Wires spot feed + poly orderbook → strategies → execution
 *
 * 4 strategies: momentum, mean_reversion, penny_clipper, expiry_fade
 * Real orderbook analysis, round-based market rotation, full exit logic from firstorder.rs
 */

import { logger } from '../../utils/logger.js';
import type { CryptoFeed, PriceUpdate } from '../../feeds/crypto/index.js';
import type { ExecutionService } from '../../execution/index.js';
import { createOrderManager, type OrderManager, type TrackedOrder, loadAllOrders } from './order-manager.js';
import { createSignalLog } from './signal-log.js';
import { createShadowEngine, type ShadowContext } from './shadow-engine.js';
import { createTrendTracker, type TrendTracker } from './trend-tracker.js';
import { createMarketScanner, type MarketScanner } from './market-scanner.js';
import { createPositionManager, type PositionManager } from './positions.js';
import {
  buildOrderbookSnapshot,
  createSpreadTracker,
  createDepthTracker,
  createBidTracker,
  type SpreadTracker,
  type DepthTracker,
  type BidTracker,
} from './orderbook.js';
import {
  createPriceBuffer,
  evaluateMomentum,
  evaluateMeanReversion,
  evaluatePennyClipper,
  evaluateExpiryFade,
  evaluatePolyMomentum,
  evaluateSpreadArb,
  evaluateSharpReversal,
  type PriceBuffer,
  type MomentumConfig,
  type MeanReversionConfig,
  type PennyClipperConfig,
  type ExpiryFadeConfig,
  type PolyMomentumConfig,
  type SpreadArbConfig,
  DEFAULT_MOMENTUM,
  DEFAULT_MEAN_REVERSION,
  DEFAULT_PENNY_CLIPPER,
  DEFAULT_EXPIRY_FADE,
  DEFAULT_POLY_MOMENTUM,
  DEFAULT_SPREAD_ARB,
} from './strategies.js';
import type {
  CryptoHftConfig,
  CryptoMarket,
  TradeSignal,
  HftStats,
  OpenPosition,
  OrderbookSnapshot,
  OrderMode,
  ClosedPosition,
  ExitReason,
} from './types.js';
import { createStateMachine, calculateSafetyMargin, canEnter, type StateMachine, type SafetyMargin } from './state-machine.js';

// ── Default Config (all real thresholds from firstorder.rs) ─────────────────

export const DEFAULT_CONFIG: CryptoHftConfig = {
  assets: ['BTC', 'ETH', 'SOL', 'XRP'],

  // Sizing
  sizeUsd: 2.5,
  minShares: 10,
  maxShares: 10,
  maxPositionUsd: 2.5,
  maxPositions: 2,

  // Round timing
  roundDurationSec: 900,
  minTimeLeftSec: 180,
  minRoundAgeSec: 30,
  /** Stop resting entry bids this many seconds before minTimeLeftSec (avoid late fills) */
  entryCutoffSec: 120,
  forceExitSec: 120,
  roundEndClearSec: 180,
  staleOrderAgeSec: 25,
  warmupSec: 0,

  // Entry execution
  entryOrder: {
    mode: 'taker',
    makerTimeoutMs: 5000,
    takerBufferCents: 0.01,
    makerExitBufferCents: 0.01,
  },
  maxOrderbookStaleMs: 8000,

  // Exit execution
  exitOrder: {
    mode: 'taker',
    makerTimeoutMs: 2000,
    takerBufferCents: 0.01,
    makerExitBufferCents: 0.01,
  },
  makerExitsForTpOnly: false,
  // Non-urgent exits (take profit) post a zero-fee maker offer first and only
  // cross the spread if it does not fill within exitOrder.makerTimeoutMs.
  makerFirstExitEnabled: true,
  // Protective stops require the executable bid to be within this % of mid, so
  // a one-tick pulled bid wick cannot trigger a sale into a fake low.
  maxBidWickPct: 8,
  sellCooldownMs: 1000,
  exitShareBuffer: 0.01,

  // TP/SL
  takeProfitPct: 100,
  stopLossPct: 50,

  // Quick scalp exit - 至少持有10秒，盈利>3%才出场
  quickProfitMinHoldSec: 10,
  quickProfitMinPct: 3,

  // Spot reversal exit
  spotReversalThresholdPct: 0.10,

  // Ratchet
  ratchetEnabled: false,
  ratchetConfirmTicks: 3,
  ratchetConfirmTolerancePct: 0.5,

  // Trailing
  trailingEnabled: true,
  trailingLatePct: 3,
  trailingMidPct: 5,
  trailingWidePct: 8,

  // Advanced exits
  staleProfitPct: 20,
  staleProfitBidUnchangedSec: 10,
  stagnantProfitPct: 5,
  stagnantDurationSec: 30,
  depthCollapseThresholdPct: 60,
  spreadArbEntryFactor: 0.95,
  trendMinPrice: 0.55,
  trendConfirmSec: 60,
  trendRatio: 0.8,
  trendBrokenPrice: 0.35,
  momentumFilterEnabled: true,
  momentumFilterWindowSec: 30,
  momentumFilterMinPct: 0.03,
  maxFillVsMidPct: 3,
  trendEntryPrice: 0,
  trendEntryFactor: 0.98,
  trendMaxEntryPrice: 0.45,
  /** Cancel a resting bid when the mid has fallen this % below it (stale/adverse fill guard) */
  cancelStaleBids: false,
  staleBidPct: 5,
  proportionalTrailEnabled: true,
  proportionalTrailPct: 15,
  proportionalTrailMinPct: 15,
  proportionalTrailMinGivebackPct: 3,
  trailingMinHighPct: 15,
  minTrailPct: 10,
  exitGraceSec: 3,
  simpleExitEnabled: true,
  dynamicStopEnabled: true,
  stopTightenStartSec: 300,
  stopMinPct: 10,
  // Risk controls (A/B toggles)
  tightStopEnabled: true,
  tightStopPct: 12,
  crashFilterEnabled: true,
  crashFilterMinHighAgeSec: 20,
  crashFilterMaxSpotMovePct: 3,
  crashFilterLookbackSec: 5,
  slippageGuardEnabled: true,
  slippageGuardMaxPct: 4,

  // Risk
  maxDailyLossUsd: 200,
  stopLossCooldownSec: 180,
  breakerCooldownSec: 300,
  exitCooldownSec: 60,
  assetCooldownSec: 90,
  lossCooldownSec: 180,
  negRisk: true,
  dryRun: true,
};

// ── Engine Interface ────────────────────────────────────────────────────────

export interface CryptoHftEngine {
  start(): Promise<void>;
  stop(): void;
  getStats(): HftStats;
  getPositions(): OpenPosition[];
  getClosed(): ClosedPosition[];
  getMarkets(): CryptoMarket[];
  getRoundInfo(): { slot: number; ageSec: number; timeLeftSec: number; canTrade: boolean };
  updateConfig(partial: Partial<CryptoHftConfig>): void;
  getConfig(): CryptoHftConfig;
  setStrategyEnabled(name: string, enabled: boolean): void;
  getEnabledStrategies(): Record<string, boolean>;
  /** Feed an orderbook update from poly WS */
  onOrderbook(tokenId: string, bids: Array<[number, number]>, asks: Array<[number, number]>): void;
  /** Update prices from Gamma API */
  updatePrices(conditionId: string, upPrice: number, downPrice: number): void;
  /** Get clock offset between local and Polymarket time */
  getClockOffset(): number;
  /** Get last poly price update timestamp for a market */
  getPolyLastTs(conditionId: string): number;
  /** Get state machine */
  getStateMachine(): StateMachine;
  /** Number of live (resting, unfilled) entry BUY orders — for UI order alerts. */
  getPendingEntryCount(): number;
  /** Risk/breaker state for the UI */
  getRiskState(): {
    halted: boolean;
    haltUntil: number;
    haltedAt: number;
    consecutiveLosses: number;
    lastEvent: string;
    lastEventAt: number;
  };
}

export function createCryptoHftEngine(
  cryptoFeed: CryptoFeed,
  execution: ExecutionService | null,
  initialConfig?: Partial<CryptoHftConfig>,
  strategyConfigs?: {
    momentum?: Partial<MomentumConfig>;
    meanReversion?: Partial<MeanReversionConfig>;
    pennyClipper?: Partial<PennyClipperConfig>;
    expiryFade?: Partial<ExpiryFadeConfig>;
    polyMomentum?: Partial<PolyMomentumConfig>;
  }
): CryptoHftEngine {
  let config: CryptoHftConfig = { ...DEFAULT_CONFIG, ...initialConfig };
  const getConfig = () => config;
  // Shadow engine (observation only): records each position's intra-hold price
  // path for offline counterfactual analysis.
  const shadowEngine = createShadowEngine();
  const lastSignalContext = new Map<string, ShadowContext>();
  const positionMgr: PositionManager = createPositionManager(getConfig, {
    onOpen: (pos) => {
      // Resolve the opposite token so the shadow engine can also record the
      // flipped (momentum) side for counterfactual comparison.
      let oppositeTokenId: string | undefined;
      for (const m of scanner.getRound().markets) {
        if (m.conditionId === pos.conditionId || m.upTokenId === pos.tokenId || m.downTokenId === pos.tokenId) {
          oppositeTokenId = m.upTokenId === pos.tokenId ? m.downTokenId : m.upTokenId;
          break;
        }
      }
      shadowEngine.onOpen(pos, lastSignalContext.get(pos.tokenId), oppositeTokenId);
    },
    onClose: (pos) => shadowEngine.onClose(pos),
  });
  const scanner: MarketScanner = createMarketScanner(getConfig);

  // Orderbook trackers
  const spreadTracker: SpreadTracker = createSpreadTracker();
  const depthTracker: DepthTracker = createDepthTracker();
  const bidTracker: BidTracker = createBidTracker();
  const books = new Map<string, OrderbookSnapshot>();

  // Per-asset price buffers (spot + poly)
  const spotBuffers = new Map<string, PriceBuffer>();
  const polyBuffers = new Map<string, PriceBuffer>();
  const polyLastTs = new Map<string, number>(); // freshness

  // Strategy configs
  const momCfg: MomentumConfig = { ...DEFAULT_MOMENTUM, ...strategyConfigs?.momentum };
  const revCfg: MeanReversionConfig = { ...DEFAULT_MEAN_REVERSION, ...strategyConfigs?.meanReversion };
  const clipCfg: PennyClipperConfig = { ...DEFAULT_PENNY_CLIPPER, ...strategyConfigs?.pennyClipper };
  const fadeCfg: ExpiryFadeConfig = { ...DEFAULT_EXPIRY_FADE, ...strategyConfigs?.expiryFade };
  const polyMomCfg: PolyMomentumConfig = { ...DEFAULT_POLY_MOMENTUM, ...strategyConfigs?.polyMomentum };

  const enabled: Record<string, boolean> = {
    momentum: false,
    mean_reversion: false,
    penny_clipper: false,
    expiry_fade: false,
    poly_momentum: false,
    spread_arb: true,
    sharp_reversal: true,
  };

  // State
  let running = false;
  let startedAt = 0;
  let exitCheckInterval: NodeJS.Timeout | null = null;
  let orderTimeoutInterval: NodeJS.Timeout | null = null;
  let signalTickInterval: NodeJS.Timeout | null = null;
  const unsubscribes: Array<() => void> = [];
  let orderInFlight = false;
  let lastSellAt = 0;
  let consecutiveLosses = 0;
  const MAX_CONSECUTIVE_LOSSES = 3;
  /** When true we stop opening NEW positions but keep managing exits (never strand positions). */
  let haltNewEntries = false;
  let haltNewEntriesUntil = 0;
  let lastClosedCount = 0;
  let riskLastEvent = '';
  let riskLastEventAt = 0;

  // Order lifecycle manager
  const orderMgr: OrderManager = createOrderManager();
  // Shadow signal logger (observation only) for spread_arb edge measurement
  const signalLog = createSignalLog();
  const lastSignalLoggedAt = new Map<string, number>();
  const SIGNAL_LOG_THROTTLE_MS = 15_000;
  const ORDER_TIMEOUT_MS = 120_000; // cancel GTC orders after 120s if unfilled

  // Per-trade fill ledger: tradeKey → amount already applied to positions/orders.
  // This makes fill handling idempotent across MATCHED→MINED→CONFIRMED and lets
  // us apply a later, larger reported size (e.g. size 0 at MATCHED) as a delta.
  const appliedFills = new Map<string, { appliedSize: number; price: number; side: 'BUY' | 'SELL'; orderId: string }>();
  // Fills that arrived before their order was recorded (or unknown maker orderid)
  // are buffered briefly and retried instead of being silently dropped.
  const pendingFills: Array<{ fill: any; ts: number }> = [];
  const PENDING_FILL_TTL_MS = 30_000;

  // State machine
  const stateMachine = createStateMachine();
  let windowOpeningPrices = new Map<string, number>(); // Binance price at start of observation

  // Robust trend confirmation (rolling-window above-threshold ratio). Decoupled
  // from evaluateAll: fed on every book/price update.
  const trendTracker: TrendTracker = createTrendTracker(getConfig);

  /** Cancel resting entry BIDS for a token (used on regime change). */
  function cancelBidsForToken(tokenId: string, price: number) {
    for (const o of orderMgr.getAllLiveOrders()) {
      if (o.side !== 'BUY' || o.tokenId !== tokenId) continue;
      if (config.dryRun) {
        orderMgr.recordTerminal(o.orderId, 'CANCELLED');
      } else if (execution) {
        execution.cancelOrder('polymarket', o.orderId)
          .then((ok) => { if (ok) orderMgr.recordTerminal(o.orderId, 'CANCELLED'); })
          .catch(() => {});
      }
    }
    logger.warn(
      { tokenId, price, brokenPrice: config.trendBrokenPrice ?? 0.35 },
      'spread_arb: trend broken — cancelled bids (positions kept, managed by stop/TP/expiry)'
    );
  }

  trendTracker.onBroken((tokenId, price) => cancelBidsForToken(tokenId, price));

  function getSpotBuffer(asset: string): PriceBuffer {
    let buf = spotBuffers.get(asset);
    if (!buf) {
      buf = createPriceBuffer(180);
      spotBuffers.set(asset, buf);
    }
    return buf;
  }

  function getPolyBuffer(asset: string): PriceBuffer {
    let buf = polyBuffers.get(asset);
    if (!buf) {
      buf = createPriceBuffer(180);
      polyBuffers.set(asset, buf);
    }
    return buf;
  }

  function getBook(tokenId: string): OrderbookSnapshot | null {
    const b = books.get(tokenId);
    if (!b) return null;
    if (Date.now() - b.timestamp > config.maxOrderbookStaleMs) return null;
    return b;
  }

  function computeShares(price: number): number {
    if (price <= 0) return config.minShares;
    const raw = config.sizeUsd / price;
    return Math.max(config.minShares, Math.min(config.maxShares, Math.floor(raw * 100) / 100));
  }

  /**
   * maker_then_taker: after the maker order has rested `makerTimeoutMs` without
   * filling, cancel it and cross the spread with a tracked taker order.
   */
  async function escalateMakerToTaker(ctx: {
    makerOrderId: string;
    signal: TradeSignal;
    market: CryptoMarket;
    shares: number;
    internalKey: string;
  }): Promise<void> {
    if (!running || !execution) return;
    const makerOrder = orderMgr.getOrder(ctx.makerOrderId);
    if (!makerOrder || !['SUBMITTED', 'LIVE'].includes(makerOrder.status)) return;

    let cancelled = false;
    try {
      cancelled = await execution.cancelOrder('polymarket', ctx.makerOrderId);
    } catch {
      cancelled = false;
    }
    if (!cancelled) {
      logger.warn({ orderId: ctx.makerOrderId }, 'maker_then_taker: could not cancel maker order — skipping escalation');
      return;
    }
    orderMgr.recordTerminal(ctx.makerOrderId, 'CANCELLED');

    // Re-price off the CURRENT book, not the 5s-old signal: the mid can have
    // moved well past the signal by the time the maker order times out, and
    // blindly paying signal.price + buffer used to chase into bad fills (the
    // dry-run path already had this guard via maxFillVsMidPct).
    const book = getBook(ctx.signal.tokenId);
    const ref = book?.midPrice ?? ctx.signal.price;
    const maxVsMid = (config.maxFillVsMidPct ?? 3) / 100;
    if (book && book.midPrice > 0) {
      const cap = Math.min(0.99, ref * (1 + maxVsMid));
      if (ctx.signal.price > cap) {
        logger.warn(
          { orderId: ctx.makerOrderId, signalPrice: ctx.signal.price, mid: book.midPrice, cap },
          'maker_then_taker: mid moved away — skipping taker escalation rather than chasing'
        );
        return;
      }
    }
    // Never chase if we are already inside the no-entry window near expiry.
    const round = scanner.getRound();
    if (round.timeLeftSec > 0 && round.timeLeftSec <= config.minTimeLeftSec + (config.entryCutoffSec ?? 120)) {
      logger.warn({ timeLeftSec: round.timeLeftSec }, 'maker_then_taker: inside entry cutoff — skipping taker escalation');
      return;
    }

    const takerPrice = Math.min(0.99, ctx.signal.price + config.entryOrder.takerBufferCents);
    const takerResult = await execution.buyLimit({
      platform: 'polymarket',
      marketId: ctx.market.conditionId,
      tokenId: ctx.signal.tokenId,
      price: takerPrice,
      size: ctx.shares,
      negRisk: config.negRisk,
      orderType: 'GTC',
    });

    if (takerResult.success && takerResult.orderId) {
      orderMgr.recordSubmit({
        orderId: takerResult.orderId,
        internalKey: `${ctx.internalKey}:taker`,
        strategy: ctx.signal.strategy,
        asset: ctx.signal.asset,
        direction: ctx.signal.direction,
        tokenId: ctx.signal.tokenId,
        conditionId: ctx.market.conditionId,
        price: takerPrice,
        size: ctx.shares,
        side: 'BUY',
        roundSlot: scanner.getRound().slot,
        reason: `${ctx.signal.reason} (maker_then_taker escalation)`,
        wasMaker: false,
        timeoutMs: ORDER_TIMEOUT_MS,
      });
      logger.info({ orderId: takerResult.orderId }, 'maker_then_taker: taker order placed and tracked');
    } else {
      logger.error({ error: takerResult.error }, 'maker_then_taker: taker order failed');
    }
  }

  async function executeEntry(signal: TradeSignal, market: CryptoMarket) {
    if (haltNewEntries) return;
    if (orderInFlight) return;
    // Prevent duplicate orders for same tokenId+direction while order is resting
    const round = scanner.getRound();
    const internalKey = orderMgr.makeKey(signal.strategy, signal.asset, signal.direction, round.slot);
    if (orderMgr.hasPendingOrder(internalKey)) return;
    orderInFlight = true;

    const shares = computeShares(signal.price);
    // Same pricing as live so paper and live use identical entry economics.
    const postOnly = signal.orderMode === 'maker';
    const orderType = signal.orderMode === 'fok' ? 'FOK' as const : 'GTC' as const;
    const orderPrice = signal.orderMode === 'taker' || signal.orderMode === 'fok'
      ? Math.min(0.99, signal.price + config.entryOrder.takerBufferCents)
      : signal.price;

    logger.info(
      {
        strategy: signal.strategy,
        asset: signal.asset,
        dir: signal.direction,
        price: orderPrice.toFixed(3),
        shares,
        mode: signal.orderMode,
        confidence: signal.confidence.toFixed(2),
        reason: signal.reason,
        dryRun: config.dryRun,
      },
      'Entry signal'
    );

    if (config.dryRun) {
      // Check if price has already moved against us (hard stop)
      const book = getBook(signal.tokenId);
      const currentPrice = book?.bestBid ?? signal.price;
      const priceMovedPct = Math.abs(currentPrice - signal.price) / signal.price * 100;
      if (priceMovedPct > config.stopLossPct) {
        logger.info({ signalPrice: signal.price, currentPrice, moved: priceMovedPct.toFixed(1) }, 'Price moved too much, skipping entry');
        orderInFlight = false;
        return;
      }

      // Simulate the order through the EXACT same lifecycle as live:
      // record in the order manager, then feed confirmed fill(s) through handleFill
      // so the ledger, position sizing and exits are identical code paths.
      const dryOrderId = `dry_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
      orderMgr.recordSubmit({
        orderId: dryOrderId,
        internalKey,
        strategy: signal.strategy,
        asset: signal.asset,
        direction: signal.direction,
        tokenId: signal.tokenId,
        conditionId: market.conditionId,
        price: orderPrice,
        size: shares,
        side: 'BUY',
        roundSlot: round.slot,
        reason: signal.reason,
        wasMaker: postOnly,
        targetExitPrice: signal.features.exitPrice,
        timeoutMs: ORDER_TIMEOUT_MS,
      });

      const isImmediate = signal.orderMode === 'fok' || signal.orderMode === 'taker';
      if (isImmediate) {
        // Taker orders cross the spread and fill immediately at the order price.
        logger.info({ orderId: dryOrderId, orderPrice, shares }, 'dry-run TAKER entry filled immediately');
        handleFill({
          orderId: dryOrderId,
          tradeId: `${dryOrderId}:fill`,
          tokenId: signal.tokenId,
          side: 'BUY',
          price: orderPrice,
          size: shares,
          status: 'CONFIRMED',
        });
      } else {
        // Maker orders are post-only: they rest and only fill if the book crosses
        // the limit. They are cancelled by checkOrderTimeouts if never filled.
        logger.info({ orderId: dryOrderId, limit: orderPrice, shares }, 'dry-run MAKER entry resting — fills only if book crosses limit');
        if (signal.orderMode === 'maker_then_taker') {
          const waitMs = config.entryOrder.makerTimeoutMs || 5000;
          const escalateTimer = setTimeout(() => {
            if (!config.dryRun) return;
            const makerOrder = orderMgr.getOrder(dryOrderId);
            if (!makerOrder || !['SUBMITTED', 'LIVE'].includes(makerOrder.status)) return;
            // Same stale-price guard as live: never chase a mid that ran away.
            const escBook = getBook(signal.tokenId);
            const maxVsMid = (config.maxFillVsMidPct ?? 3) / 100;
            if (escBook && escBook.midPrice > 0 && signal.price > escBook.midPrice * (1 + maxVsMid)) {
              orderMgr.recordTerminal(dryOrderId, 'CANCELLED');
              logger.warn(
                { orderId: dryOrderId, signalPrice: signal.price, mid: escBook.midPrice },
                'dry maker_then_taker: mid moved away — skipping taker escalation'
              );
              return;
            }
            orderMgr.recordTerminal(dryOrderId, 'CANCELLED');
            const takerPrice = Math.min(0.99, signal.price + config.entryOrder.takerBufferCents);
            const takerId = `dry_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
            orderMgr.recordSubmit({
              orderId: takerId,
              internalKey: `${internalKey}:taker`,
              strategy: signal.strategy,
              asset: signal.asset,
              direction: signal.direction,
              tokenId: signal.tokenId,
              conditionId: market.conditionId,
              price: takerPrice,
              size: shares,
              side: 'BUY',
              roundSlot: round.slot,
              reason: `${signal.reason} (dry maker_then_taker escalation)`,
              wasMaker: false,
              targetExitPrice: signal.features.exitPrice,
              timeoutMs: ORDER_TIMEOUT_MS,
            });
            logger.info({ orderId: takerId, takerPrice }, 'dry-run maker_then_taker: escalated to taker');
            handleFill({
              orderId: takerId,
              tradeId: `${takerId}:fill`,
              tokenId: signal.tokenId,
              side: 'BUY',
              price: takerPrice,
              size: shares,
              status: 'CONFIRMED',
            });
          }, waitMs);
          escalateTimer.unref?.();
        }
      }
      orderInFlight = false;
      return;
    }

    if (!execution) {
      orderInFlight = false;
      return;
    }

    try {
      const result = await execution.buyLimit({
        platform: 'polymarket',
        marketId: market.conditionId,
        tokenId: signal.tokenId,
        price: orderPrice,
        size: shares,
        negRisk: config.negRisk,
        orderType,
        postOnly,
      });

      // Log order result
      if (result.success && result.orderId) {
        orderMgr.recordSubmit({
          orderId: result.orderId,
          internalKey,
          strategy: signal.strategy,
          asset: signal.asset,
          direction: signal.direction,
          tokenId: signal.tokenId,
          conditionId: market.conditionId,
          price: orderPrice,
          size: shares,
          side: 'BUY',
          roundSlot: round.slot,
          reason: signal.reason,
          wasMaker: postOnly,
          targetExitPrice: signal.features.exitPrice,
          timeoutMs: ORDER_TIMEOUT_MS,
        });

        // Do NOT call recordFill here — user-ws handleFill is the authoritative source
        // for fill accounting. Calling recordFill here would cause double-counting.

        logger.info({
          orderId: result.orderId,
          status: result.status,
          filledSize: result.filledSize,
          avgFillPrice: result.avgFillPrice,
          asset: signal.asset,
          direction: signal.direction,
        }, 'Order submitted successfully');
      } else {
        logger.error({
          error: result.error,
          asset: signal.asset,
          direction: signal.direction,
          price: orderPrice,
          size: shares,
        }, 'Order submission failed');
        if (result.orderId) {
          orderMgr.recordTerminal(result.orderId, 'REJECTED');
        }
        // Trigger alert sound
        logger.error({ alert: true, message: `下单失败: ${result.error}` }, 'ALERT');
      }

      // ALL orders: track in orderMgr, do NOT create position here.
      // Position creation is driven exclusively by user-ws fill events (handleFill).
      if (result.success && result.orderId) {
        const isFilled = result.filledSize && result.filledSize > 0 && result.avgFillPrice;
        const isResting = result.status === 'open' || (!result.filledSize && result.success);

        if (isFilled) {
          logger.info({
            orderId: result.orderId,
            fillPrice: result.avgFillPrice,
            fillSize: result.filledSize,
            status: result.status,
          }, 'Order filled at submission — waiting for user-ws confirmation');
        } else if (isResting && signal.orderMode === 'maker_then_taker') {
          // Maker order resting — wait makerTimeoutMs, then cancel and take.
          const waitMs = config.entryOrder.makerTimeoutMs || 5000;
          logger.info({ orderId: result.orderId, waitMs }, 'Maker order resting — scheduling taker escalation');
          const timer = setTimeout(() => {
            void escalateMakerToTaker({
              makerOrderId: result.orderId!,
              signal,
              market,
              shares,
              internalKey,
            });
          }, waitMs);
          timer.unref?.();
        } else {
          logger.info({
            orderId: result.orderId,
            status: result.status,
          }, 'Order resting on book — waiting for user-ws fill');
        }
      } else if (!result.success) {
        logger.error({ error: result.error, strategy: signal.strategy, asset: signal.asset }, 'Entry order failed');
        if (result.orderId) orderMgr.recordTerminal(result.orderId, 'REJECTED');
      }
    } catch (err) {
      logger.error({ err, strategy: signal.strategy }, 'Entry execution error');
    } finally {
      orderInFlight = false;
    }
  }

  const URGENT_EXIT_REASONS = new Set<ExitReason>(['stop_loss', 'force_exit', 'time_exit', 'spot_reversal']);
  // Maker exit offers awaiting their maker→taker timeout, keyed by order id.
  const makerExitTimers = new Map<string, ReturnType<typeof setTimeout>>();

  function clearMakerExitTimer(orderId: string) {
    const t = makerExitTimers.get(orderId);
    if (t) { clearTimeout(t); makerExitTimers.delete(orderId); }
  }

  /** Cancel a resting maker exit and cross at the bid (called on maker timeout). */
  async function escalateMakerExit(pos: OpenPosition, makerOrderId: string, reason: ExitReason): Promise<void> {
    if (!running) return;
    const order = orderMgr.getOrder(makerOrderId);
    // Already filled (position closed) or otherwise gone — nothing to escalate.
    if (!positionMgr.getOpen().some(p => p.id === pos.id)) { clearMakerExitTimer(makerOrderId); return; }
    if (!order || !['SUBMITTED', 'LIVE'].includes(order.status)) { clearMakerExitTimer(makerOrderId); return; }

    if (config.dryRun) {
      orderMgr.recordTerminal(makerOrderId, 'CANCELLED');
    } else if (execution) {
      let cancelled = false;
      try { cancelled = await execution.cancelOrder('polymarket', makerOrderId); } catch { cancelled = false; }
      if (!cancelled) {
        logger.warn({ orderId: makerOrderId }, 'maker exit: could not cancel — will retry next timeout sweep');
        return;
      }
      orderMgr.recordTerminal(makerOrderId, 'CANCELLED');
    }
    clearMakerExitTimer(makerOrderId);
    logger.info({ orderId: makerOrderId, asset: pos.asset, reason }, 'maker exit timed out — escalating to taker');
    // Cross immediately at the live bid; bypassCooldown so the urgent follow-up
    // is not suppressed by the sell cooldown the maker attempt set.
    await executeExit(pos, reason, pos.currentPrice, false, true);
  }

  /** Track a resting maker exit and arm its maker→taker timeout. */
  function armMakerExitTimeout(pos: OpenPosition, orderId: string, reason: ExitReason) {
    clearMakerExitTimer(orderId);
    const waitMs = config.exitOrder.makerTimeoutMs || 2000;
    const timer = setTimeout(() => { void escalateMakerExit(pos, orderId, reason); }, waitMs);
    timer.unref?.();
    makerExitTimers.set(orderId, timer);
  }

  async function executeExit(
    pos: OpenPosition,
    reason: ExitReason,
    exitPrice: number,
    useMaker: boolean,
    bypassCooldown = false
  ) {
    // Cancel any resting buy orders for this token+direction before closing
    cancelOrdersForToken(pos.tokenId, pos.direction);

    const urgent = URGENT_EXIT_REASONS.has(reason);
    // One exit at a time per token+direction. A resting maker offer is allowed to
    // keep working while signals are non-urgent (avoid churn); an urgent signal
    // (stop/force/time) always cancels it and crosses immediately.
    const liveExits = orderMgr.findLiveOrders(pos.tokenId, pos.direction).filter(o => o.side === 'SELL');
    const restingMaker = liveExits.find(o => o.wasMaker);
    if (restingMaker) {
      if (useMaker && !urgent) {
        // A zero-fee offer is already resting; don't stack duplicates.
        return;
      }
      // Need to cross now (urgent, or a taker decision): pull the maker offer and
      // its escalation timer, then fall through to place the taker order.
      clearMakerExitTimer(restingMaker.orderId);
      if (config.dryRun || !execution) {
        orderMgr.recordTerminal(restingMaker.orderId, 'CANCELLED');
      } else {
        let cancelFailed = false;
        for (const old of liveExits) {
          try {
            const cancelled = await execution.cancelOrder('polymarket', old.orderId);
            if (cancelled) orderMgr.recordTerminal(old.orderId, 'CANCELLED');
            else cancelFailed = true;
          } catch { cancelFailed = true; }
        }
        if (cancelFailed) {
          logger.warn({ positionId: pos.id }, 'Cancel of resting maker exit failed — aborting this taker attempt');
          return;
        }
        await new Promise(r => setTimeout(r, 300));
      }
    }

    // Slippage guard (toggle): don't chase a one-tick flash gap on non-mandatory
    // exits. Reference the PREVIOUS tick price (pos.prevPrice), not currentPrice,
    // because currentPrice may already have been updated to the gapped price.
    // Mandatory risk exits (stop/force/time/spot-reversal) always execute.
    if (config.slippageGuardEnabled !== false) {
      const refPrice = (pos.prevPrice && pos.prevPrice > 0) ? pos.prevPrice : pos.currentPrice;
      if (refPrice > 0) {
        const mandatory = reason === 'stop_loss' || reason === 'force_exit' || reason === 'time_exit' || reason === 'spot_reversal';
        const maxAdv = config.slippageGuardMaxPct ?? 4;
        const adversePct = ((exitPrice - refPrice) / refPrice) * 100;
        if (!mandatory && adversePct < -maxAdv) {
          logger.warn(
            { positionId: pos.id, asset: pos.asset, reason, prevPrice: refPrice, exitPrice, adversePct: adversePct.toFixed(1) },
            'Slippage guard: skipping exit into flash gap'
          );
          return;
        }
      }
    }

    // Sell cooldown. Urgent risk exits bypass it: a maker TP attempt may have set
    // the cooldown milliseconds before a stop fires, and the stop must cross now.
    if (!bypassCooldown && !urgent && Date.now() - lastSellAt < config.sellCooldownMs) return;
    lastSellAt = Date.now();

    const sellShares = Math.max(0.01, Math.floor((pos.shares - config.exitShareBuffer) * 100) / 100);
    // Maker sells must post ABOVE the bid (at the ask); selling at the bid would be
    // marketable and rejected by post-only. Taker sells cross with a buffer.
    const exitBook = getBook(pos.tokenId);
    const makerPrice = exitBook && exitBook.bestAsk > 0 ? exitBook.bestAsk : exitPrice;
    const takerRef = exitBook && exitBook.bestBid > 0 ? exitBook.bestBid : exitPrice;
    const sellPrice = useMaker
      ? makerPrice
      : Math.max(0.01, takerRef - config.exitOrder.takerBufferCents);

    if (config.dryRun) {
      // Simulate the sell through the same order lifecycle as live.
      const round = scanner.getRound();
      const dryOrderId = `dry_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
      orderMgr.recordSubmit({
        orderId: dryOrderId,
        internalKey: `exit:${pos.tokenId}:${pos.direction}:${round.slot}:${reason}`,
        strategy: pos.strategy,
        asset: pos.asset,
        direction: pos.direction,
        tokenId: pos.tokenId,
        conditionId: pos.conditionId,
        price: sellPrice,
        size: sellShares,
        side: 'SELL',
        roundSlot: round.slot,
        reason,
        wasMaker: useMaker,
        timeoutMs: ORDER_TIMEOUT_MS,
      });
      if (useMaker) {
        // Maker sell rests; checkDryMakerFills closes it when the bid reaches the limit.
        logger.info({ orderId: dryOrderId, limit: sellPrice, shares: sellShares }, 'dry-run MAKER exit resting');
        armMakerExitTimeout(pos, dryOrderId, reason);
      } else {
        handleFill({
          orderId: dryOrderId,
          tradeId: `${dryOrderId}:fill`,
          tokenId: pos.tokenId,
          side: 'SELL',
          price: sellPrice,
          size: sellShares,
          status: 'CONFIRMED',
        });
      }
      return;
    }

    if (!execution) {
      positionMgr.close(pos.id, sellPrice, reason, useMaker);
      return;
    }

    try {
      // Log order submission
      logger.info({
        positionId: pos.id,
        asset: pos.asset,
        direction: pos.direction,
        reason,
        price: sellPrice.toFixed(3),
        size: sellShares,
        useMaker,
      }, 'Submitting exit order');

      const result = await execution.sellLimit({
        platform: 'polymarket',
        marketId: pos.conditionId,
        tokenId: pos.tokenId,
        price: sellPrice,
        size: sellShares,
        negRisk: config.negRisk,
        orderType: useMaker ? 'GTC' as const : 'FOK' as const,
        postOnly: useMaker,
      });

      // Log order result
      if (result.success && result.orderId) {
        const round = scanner.getRound();
        const exitKey = `exit:${pos.tokenId}:${pos.direction}:${round.slot}:${reason}`;
        orderMgr.recordSubmit({
          orderId: result.orderId,
          internalKey: exitKey,
          strategy: pos.strategy,
          asset: pos.asset,
          direction: pos.direction,
          tokenId: pos.tokenId,
          conditionId: pos.conditionId,
          price: sellPrice,
          size: sellShares,
          side: 'SELL',
          roundSlot: round.slot,
          reason,
          wasMaker: useMaker,
          timeoutMs: ORDER_TIMEOUT_MS,
        });

        logger.info({
          orderId: result.orderId,
          status: result.status,
          filledSize: result.filledSize,
          avgFillPrice: result.avgFillPrice,
          asset: pos.asset,
          direction: pos.direction,
          reason,
        }, 'Exit order submitted successfully');

        if (result.filledSize && result.filledSize > 0 && result.avgFillPrice) {
          // Confirmed fill — close position
          positionMgr.close(pos.id, result.avgFillPrice, reason, useMaker);
          orderMgr.recordFill(result.orderId, result.filledSize, result.avgFillPrice);
        } else if (!useMaker) {
          // FOK that didn't fill — DON'T close position, just cancel the order
          // Position stays open; user-ws will confirm if it actually filled
          orderMgr.recordTerminal(result.orderId, 'CANCELLED');
          logger.warn({ orderId: result.orderId, asset: pos.asset }, 'FOK exit did not fill — position kept open');
        } else {
          // Maker exit resting on book: arm maker→taker escalation so a profit
          // target never strands the position waiting for a fill that never comes.
          armMakerExitTimeout(pos, result.orderId, reason);
        }
        // For GTC maker orders that rest on book: position is closed later either
        // by the user-ws fill event or the maker→taker timeout above.
      } else {
        logger.error({
          error: result.error,
          positionId: pos.id,
          asset: pos.asset,
          reason,
          price: sellPrice,
          size: sellShares,
        }, 'Exit order failed, keeping position open');
        // Trigger alert sound
        logger.error({ alert: true, message: `卖出失败: ${result.error}` }, 'ALERT');
      }
    } catch (err) {
      logger.error({ err, positionId: pos.id, reason }, 'Exit execution error, keeping position open');
    }
  }

  let spotTickCount = 0;

  function onSpotTick(update: PriceUpdate) {
    if (!running) return;
    const asset = update.symbol;
    spotTickCount++;
    if (spotTickCount <= 5 || spotTickCount % 100 === 0) {
      logger.info({ asset, price: update.price, total: spotTickCount }, 'Spot tick received');
    }
    if (!config.assets.includes(asset)) return;

    getSpotBuffer(asset).push(update.price);

    // Only evaluate on spot ticks
    evaluateAll(asset);
  }

  /**
   * Falling-knife / crash filter (toggle: crashFilterEnabled).
   * Rejects buying into a market that just moved violently against the bet:
   *  - the ≥0.55 high reading must be at least crashFilterMinHighAgeSec old
   *  - Binance spot over the lookback must not be moving hard against the entry
   */
  /**
   * Momentum alignment filter. The "token held >0.55" confirmation LAGS the
   * underlying: by the time it fires, Binance spot has often already turned, and
   * we end up buying the side that is about to lose. Require spot short-term
   * momentum to not be moving against the token direction.
   */
  function passesMomentumFilter(sig: TradeSignal, spotBuf: PriceBuffer): boolean {
    if (config.momentumFilterEnabled === false) return true;
    const win = config.momentumFilterWindowSec ?? 30;
    const tol = config.momentumFilterMinPct ?? 0;
    const move = spotBuf.movePct(win);
    const against = sig.direction === 'up' ? move < -tol : move > tol;
    if (against) {
      logger.info(
        { asset: sig.asset, direction: sig.direction, spotMovePct: move.toFixed(3), windowSec: win, tolPct: tol },
        'Momentum filter: rejected — spot moving against entry'
      );
      return false;
    }
    return true;
  }

  function evaluateAll(asset: string) {
    // Circuit-breaker halt: no new entries until cooldown elapses.
    if (haltNewEntries) {
      if (Date.now() < haltNewEntriesUntil) return;
      haltNewEntries = false;
      consecutiveLosses = 0;
      riskLastEvent = 'breaker_resumed';
      riskLastEventAt = Date.now();
      logger.info('Circuit breaker cooldown elapsed — resuming new entries');
    }

    // Warmup check
    if (Date.now() - startedAt < config.warmupSec * 1000) return;

    // Round timing check
    const roundCheck = scanner.canTrade();
    if (!roundCheck.ok) {
      if (Math.random() < 0.01) logger.info({ reason: roundCheck.reason }, 'Cannot trade');
      return;
    }

    const market = scanner.getMarket(asset);
    if (!market) {
      if (Math.random() < 0.01) logger.info({ asset }, 'No market found');
      return;
    }

    // Can we open?
    const spotBuf = getSpotBuffer(asset);
    const polyBuf = getPolyBuffer(asset);

    // Gather context
    const round = scanner.getRound();
    const upBook = getBook(market.upTokenId);
    const downBook = getBook(market.downTokenId);
    const spotMove30 = spotBuf.movePct(30);
    const spotMove60 = spotBuf.movePct(60);
    const spotMove5 = spotBuf.movePct(5);
    const polyAge = polyLastTs.has(market.conditionId)
      ? (Date.now() - polyLastTs.get(market.conditionId)!) / 1000
      : 999;

    // Debug: log evaluation context
    logger.info({
      asset,
      spotBufLen: spotBuf.prices.length,
      spotMove30: spotMove30.toFixed(4),
      spotMove60: spotMove60.toFixed(4),
      spotMove5: spotMove5.toFixed(4),
      upBook: !!upBook,
      downBook: !!downBook,
      upMid: upBook ? upBook.midPrice.toFixed(3) : '-',
      downMid: downBook ? downBook.midPrice.toFixed(3) : '-',
      upConf: trendTracker.isConfirmed(market.upTokenId),
      downConf: trendTracker.isConfirmed(market.downTokenId),
      polyAge: polyAge.toFixed(1),
      roundAge: round.ageSec.toFixed(0),
      timeLeft: round.timeLeftSec.toFixed(0),
    }, 'Evaluate context');

    // Need at least 3 spot data points for trend detection
    if (spotBuf.prices.length < 3) return;

    // Evaluate each enabled strategy
    const signals: TradeSignal[] = [];

    if (enabled.momentum) {
      // Momentum picks direction from spot move, use matching book
      const momBook = spotMove30 > 0 ? upBook : downBook;
      const sig = evaluateMomentum(market, spotMove30, 30, momBook, polyAge, momCfg);
      if (sig) signals.push(sig);
    }

    if (enabled.mean_reversion) {
      // Mean reversion: try with up book first, if result trades DOWN re-check with down book
      let sig = evaluateMeanReversion(market, spotMove60, round.ageSec, upBook, revCfg);
      if (sig && sig.direction === 'down' && downBook) {
        // Re-evaluate with the correct book for the DOWN side
        sig = evaluateMeanReversion(market, spotMove60, round.ageSec, downBook, revCfg);
      }
      if (sig) signals.push(sig);
    }

    if (enabled.penny_clipper) {
      // Penny clipper evaluates both sides, use UP book (spread similar on both)
      const sig = evaluatePennyClipper(market, spotBuf, polyBuf, upBook ?? downBook, clipCfg);
      if (sig) signals.push(sig);
    }

    if (enabled.expiry_fade) {
      // Expiry fade buys the cheap side
      const sig = evaluateExpiryFade(market, spotMove5, upBook ?? downBook, fadeCfg);
      if (sig) signals.push(sig);
    }

    if (enabled.poly_momentum) {
      // Poly momentum: pure Polymarket price momentum
      const sig = evaluatePolyMomentum(market, polyBuf, upBook ?? downBook, polyMomCfg);
      if (sig) signals.push(sig);
    }

    if (enabled.spread_arb) {
      // Trend-confirmed dip buyer: rest a bid under the round favourite.
      trendTracker.resetIfNewRound(round.slot);
      const sig = evaluateSpreadArb(market, spotMove60, polyBuf, { up: upBook, down: downBook }, {
        ...DEFAULT_SPREAD_ARB,
        entryFactor: config.spreadArbEntryFactor ?? DEFAULT_SPREAD_ARB.entryFactor,
        trendMinPrice: config.trendMinPrice ?? DEFAULT_SPREAD_ARB.trendMinPrice,
        trendConfirmSec: config.trendConfirmSec ?? DEFAULT_SPREAD_ARB.trendConfirmSec,
        trendBrokenPrice: config.trendBrokenPrice ?? DEFAULT_SPREAD_ARB.trendBrokenPrice,
        trendEntryPrice: config.trendEntryPrice ?? DEFAULT_SPREAD_ARB.trendEntryPrice,
        trendEntryFactor: config.trendEntryFactor ?? DEFAULT_SPREAD_ARB.trendEntryFactor,
        trendMaxEntryPrice: config.trendMaxEntryPrice ?? DEFAULT_SPREAD_ARB.trendMaxEntryPrice,
      }, trendTracker.confirmedTokens());

      // Shadow-log every raw signal (throttled) so edge can be measured later.
      if (sig) {
        const key = `${sig.asset}:${sig.direction}`;
        const now = Date.now();
        if (now - (lastSignalLoggedAt.get(key) ?? 0) >= SIGNAL_LOG_THROTTLE_MS) {
          lastSignalLoggedAt.set(key, now);
          const sigBook = sig.direction === 'up' ? upBook : downBook;
          signalLog.record({
            strategy: sig.strategy,
            asset: sig.asset,
            direction: sig.direction,
            tokenId: sig.tokenId,
            conditionId: market.conditionId,
            signalPrice: sig.price,
            marketPrice: sig.direction === 'up' ? market.upPrice : market.downPrice,
            entryFactor: config.spreadArbEntryFactor ?? DEFAULT_SPREAD_ARB.entryFactor,
            highAgeMs: 0,
            spotMove5,
            spotMove30,
            spotMove60,
            spotPrice: getSpotBuffer(sig.asset).prices[0]?.price ?? 0,
            timeLeftSec: round.timeLeftSec,
            trendAgeSec: trendTracker.get(sig.tokenId)?.aboveSec ?? 0,
            spreadPct: sigBook?.spreadPct,
          });
        }
      }

      if (sig) {
        const sigBook = sig.direction === 'up' ? upBook : downBook;
        const sigContext: ShadowContext = {
          spotMove5,
          spotMove30,
          spotMove60,
          trendAgeSec: trendTracker.get(sig.tokenId)?.aboveSec ?? 0,
          spreadPct: sigBook?.spreadPct,
          timeLeftSec: round.timeLeftSec,
          entryFactor: config.trendEntryFactor ?? DEFAULT_SPREAD_ARB.trendEntryFactor,
        };
        lastSignalContext.set(sig.tokenId, sigContext);
        if (passesMomentumFilter(sig, spotBuf)) {
          signals.push(sig);
          logger.info({ 
            asset: market.asset, 
            upPrice: market.upPrice.toFixed(3), 
            downPrice: market.downPrice.toFixed(3),
            trendConfirmed: [...trendTracker.confirmedTokens()],
            signal: sig.reason 
          }, 'spread_arb signal generated');
        }
      }
    }

    if (enabled.sharp_reversal) {
      // Sharp reversal: buy when high-price token drops sharply
      const sig = evaluateSharpReversal(market, polyBuf, upBook ?? downBook);
      if (sig) signals.push(sig);
    }

    if (signals.length === 0) return;

    // Pick highest confidence signal
    signals.sort((a, b) => b.confidence - a.confidence);
    const best = signals[0];

    // Pre-check: can we open for this asset+direction?
    const canOpen = positionMgr.canOpen(best.asset, best.direction);
    if (!canOpen.ok) return;

    // Reserve slots for RESTING entry orders too: otherwise several bids can be
    // live at once and all fill together, exceeding maxPositions (observed: 3
    // correlated fills at the same second against a limit of 2).
    const openCount = positionMgr.getOpen().length;
    const pendingEntries = orderMgr.getAllLiveOrders().filter(o => o.side === 'BUY').length;
    if (openCount + pendingEntries >= config.maxPositions) {
      if (Math.random() < 0.05) {
        logger.info({ openCount, pendingEntries, max: config.maxPositions }, 'Max positions reached (incl. resting entries)');
      }
      return;
    }

    // Safety margin check: calculate based on Binance spot price
    // Skip for spread_arb strategy (we're buying on dips, not rallies)
    if (best.strategy !== 'spread_arb') {
      const spotBufForMargin = getSpotBuffer(best.asset);
      const currentSpotPrice = spotBufForMargin.prices[0]?.price ?? 0;
      
      // Get or initialize window opening price (price at start of current observation)
      if (!windowOpeningPrices.has(best.asset)) {
        windowOpeningPrices.set(best.asset, currentSpotPrice);
      }
      const windowOpeningPrice = windowOpeningPrices.get(best.asset)!;
      
      // Calculate safety margin
      const safetyMargin = calculateSafetyMargin(currentSpotPrice, windowOpeningPrice);
      stateMachine.updateSafetyMargin(currentSpotPrice, windowOpeningPrice);
      
      // Check if entry is allowed
      const tokenPrice = best.direction === 'up' ? market.upPrice : market.downPrice;
      // For spread_arb: trend is confirmed if token was ≥0.55 recently (already checked in strategy)
      // For other strategies: check if current price ≥0.55
      const trendConfirmed = best.strategy === 'spread_arb' ? true : tokenPrice >= 0.55;
      stateMachine.updateTrend(trendConfirmed);
      
      const entryCheck = canEnter(safetyMargin, tokenPrice, trendConfirmed);
      if (!entryCheck.ok) {
        logger.debug({ 
          reason: entryCheck.reason, 
          safetyMargin: safetyMargin.marginPct.toFixed(2),
          tokenPrice: tokenPrice.toFixed(3),
          trendConfirmed 
        }, 'Entry blocked by safety check');
        return;
      }
    }

    // Update state machine
    stateMachine.updatePosition(positionMgr.getOpen().length > 0);
    if (stateMachine.getState() === 'OBSERVING') {
      stateMachine.transition('ARMED', `Signal: ${best.strategy} ${best.direction}`);
    }

    executeEntry(best, market);
  }

  /** Map tokenId → market/direction for the current round (only manage these). */
  function currentTokenInfo(): Map<string, { market: CryptoMarket; direction: 'up' | 'down' }> {
    const info = new Map<string, { market: CryptoMarket; direction: 'up' | 'down' }>();
    for (const m of scanner.getRound().markets) {
      if (m.upTokenId) info.set(m.upTokenId, { market: m, direction: 'up' });
      if (m.downTokenId) info.set(m.downTokenId, { market: m, direction: 'down' });
    }
    return info;
  }

  /** Record a reconciled fill against an order and adjust the position accordingly. */
  function applyReconciledFill(order: TrackedOrder, qty: number, avg: number): void {
    orderMgr.recordFill(order.orderId, qty, avg);
    const info = currentTokenInfo().get(order.tokenId);
    if (!info) return;
    const existing = positionMgr.getOpen().find(
      p => p.tokenId === order.tokenId && p.direction === info.direction
    );
    if (order.side === 'BUY' && !existing) {
      positionMgr.open({
        strategy: order.strategy,
        asset: order.asset,
        direction: info.direction,
        tokenId: order.tokenId,
        conditionId: order.conditionId || info.market.conditionId,
        entryPrice: avg,
        shares: qty,
        expiresAt: info.market.expiresAt || Date.now() + 900_000,
        wasMaker: order.wasMaker === true,
        targetExitPrice: order.targetExitPrice,
      });
      stateMachine.updatePosition(true);
    } else if (order.side === 'SELL' && existing) {
      const remaining = existing.shares - qty;
      if (remaining <= 0.01) {
        positionMgr.close(existing.id, avg, 'force_exit', false);
      } else {
        existing.shares = remaining;
        existing.costUsd = existing.entryPrice * remaining;
      }
    }
  }

  let timeoutReconcileInFlight = false;
  async function checkOrderTimeouts() {
    if (!running || !execution) return;

    const now = Date.now();
    const timeLeftSec = scanner.getRound().timeLeftSec;
    const clearSec = config.roundEndClearSec ?? 180;
    const staleAgeMs = (config.staleOrderAgeSec ?? 25) * 1000;
    const nearRoundEnd = timeLeftSec > 0 && timeLeftSec <= clearSec;

    // Targets = globally timed-out orders + (in the final N seconds of a round) any
    // resting order outstanding for >= staleAgeMs.
    const targets = new Map<string, TrackedOrder>();
    for (const o of orderMgr.getTimedOutOrders()) targets.set(o.orderId, o);
    if (nearRoundEnd) {
      for (const o of orderMgr.getAllLiveOrders()) {
        if (now - o.submittedAt >= staleAgeMs) targets.set(o.orderId, o);
      }
    }

    // Dry run reconciles locally and never talks to the exchange.
    if (config.dryRun) {
      for (const o of targets.values()) {
        logger.info({ orderId: o.orderId, ageMs: now - o.submittedAt, nearRoundEnd }, 'Dry-run: cancelling resting order');
        orderMgr.recordTerminal(o.orderId, 'CANCELLED');
      }
      return;
    }

    if (timeoutReconcileInFlight) return;
    timeoutReconcileInFlight = true;

    try {
      // Fetch status sources once per sweep.
      let openIds: Set<string> | null = null;
      let openList: Awaited<ReturnType<typeof execution.getOpenOrdersChecked>> = null;
      try {
        openList = await execution.getOpenOrdersChecked('polymarket');
        if (openList) openIds = new Set(openList.map(o => o.orderId));
      } catch { openIds = null; }

      // Mid-run orphan sweep: cancel exchange orders we have no record of, scoped
      // to this round's tokens (catches orders placed but never recorded).
      if (openList) {
        const tokenInfo = currentTokenInfo();
        const known = new Set(orderMgr.getAllOrders().map(o => o.orderId));
        for (const o of openList) {
          if (known.has(o.orderId)) continue;
          if (!tokenInfo.has(o.tokenId || '')) continue;
          logger.error({ orderId: o.orderId, tokenId: o.tokenId, side: o.side, price: o.price }, 'Orphan sweep: cancelling UNKNOWN open order');
          execution.cancelOrder('polymarket', o.orderId).catch(() => {});
        }
      }

      let trades: Array<{ tokenId: string; side: 'BUY' | 'SELL'; price: number; size: number; timestamp: Date }> | null = null;
      try {
        if (typeof execution.getTrades === 'function') trades = await execution.getTrades(500);
      } catch { trades = null; }

      // Orphan sweep already ran above; nothing else to do this pass.
      if (targets.size === 0) return;

      for (const order of targets.values()) {
        // May have been resolved while we fetched status sources.
        if (!['SUBMITTED', 'LIVE', 'PARTIAL'].includes(order.status)) continue;

        // Cannot confirm book state → never risk a mis-cancel; retry next sweep.
        if (!openIds) {
          logger.warn({ orderId: order.orderId }, 'Timeout: order book unavailable — keeping live for retry');
          continue;
        }

        const stillOpen = openIds.has(order.orderId);

        if (!stillOpen) {
          // Not on book → it filled or was cancelled. Only mark terminal if we can tell.
          if (!trades) {
            logger.warn({ orderId: order.orderId }, 'Timeout: order not on book but trade history unavailable — keeping live for retry');
            continue;
          }
          const since = order.submittedAt - 60_000;
          const matches = trades.filter(t =>
            t.tokenId === order.tokenId && t.side === order.side &&
            new Date(t.timestamp).getTime() >= since
          );
          const filledQty = matches.reduce((s, t) => s + (t.size || 0), 0);
          if (filledQty > 0.001) {
            const notional = matches.reduce((s, t) => s + (t.size || 0) * (t.price || 0), 0);
            const avg = notional > 0 ? notional / filledQty : order.price;
            applyReconciledFill(order, Math.min(filledQty, order.size), avg);
            logger.warn({ orderId: order.orderId, filledQty, avg }, 'Timeout reconcile: WS-missed fill — position reconstructed');
          } else {
            orderMgr.recordTerminal(order.orderId, 'CANCELLED');
            logger.info({ orderId: order.orderId }, 'Timeout reconcile: order gone with no trades — marked cancelled');
          }
          continue;
        }

        // Still resting → cancel, only mark terminal on confirmed cancel.
        logger.warn(
          { orderId: order.orderId, asset: order.asset, direction: order.direction, nearRoundEnd, ageMs: now - order.submittedAt },
          nearRoundEnd ? 'Cancelling stale order before round end' : 'Cancelling timed-out order'
        );
        let cancelled = false;
        try {
          cancelled = await execution.cancelOrder('polymarket', order.orderId);
        } catch { cancelled = false; }
        if (cancelled) {
          orderMgr.recordTerminal(order.orderId, 'CANCELLED');
        } else {
          logger.warn({ orderId: order.orderId }, 'Timeout cancel failed — will retry');
        }
      }
    } finally {
      timeoutReconcileInFlight = false;
    }
  }

  function cancelOrdersForToken(tokenId: string, direction: string) {
    const liveOrders = orderMgr.findLiveOrders(tokenId, direction);
    for (const order of liveOrders) {
      // Dry-run orders exist only in the local order manager — cancel locally.
      if (config.dryRun || !execution) {
        orderMgr.recordTerminal(order.orderId, 'CANCELLED');
        continue;
      }
      execution.cancelOrder('polymarket', order.orderId)
        .then((success) => {
          if (success) {
            orderMgr.recordTerminal(order.orderId, 'CANCELLED');
          }
          // If cancel fails, order stays SUBMITTED/LIVE — will be caught by timeout
        })
        .catch(() => {});
    }
  }

  function findTrackedForFill(fill: any): TrackedOrder | null {
    let tracked = orderMgr.getOrder(fill.orderId);
    if (tracked) return tracked;

    // Fallback: user-ws trade events for maker orders may not carry our orderId.
    // Match by tokenId + side (and closest price when ambiguous).
    if (!fill.tokenId) return null;
    const candidates = orderMgr.getAllLiveOrders().filter(
      o => o.tokenId === fill.tokenId && o.side === fill.side
    );
    if (candidates.length === 0) return null;
    if (candidates.length === 1) {
      logger.info({ orderId: fill.orderId, matchedTo: candidates[0].orderId, tokenId: fill.tokenId }, 'Fill matched by tokenId+side fallback');
      return candidates[0];
    }
    const closest = candidates.reduce((best, o) =>
      Math.abs(o.price - fill.price) < Math.abs(best.price - fill.price) ? o : best
    );
    logger.info({ orderId: fill.orderId, matchedTo: closest.orderId, tokenId: fill.tokenId }, 'Fill matched by closest price fallback');
    return closest;
  }

  /** Apply a signed share delta to the tracked order and position. */
  function applyFillDelta(tracked: TrackedOrder, side: 'BUY' | 'SELL', delta: number, price: number, fill: any) {
    if (!Number.isFinite(delta) || delta === 0) return;

    const prevFilled = tracked.filledSize;
    const requested = Math.max(0, prevFilled + delta);
    // Never let an order's cumulative fill exceed its declared size. This caps any
    // over-counting from duplicate fill events and prevents overselling on exit.
    const capped = tracked.size > 0 ? Math.min(requested, tracked.size) : requested;
    const effectiveDelta = capped - prevFilled;
    if (effectiveDelta === 0) {
      if (delta > 0) {
        logger.warn({ orderId: tracked.orderId, size: tracked.size, prevFilled }, 'Fill exceeds order size — clamped (possible duplicate fill event)');
      }
      return;
    }

    const avgPrice = capped > 0
      ? ((tracked.avgFillPrice * prevFilled) + (price * effectiveDelta)) / capped
      : price;
    orderMgr.recordFill(tracked.orderId, capped, Math.max(0, avgPrice));

    const existingPos = positionMgr.getOpen().find(
      p => p.tokenId === tracked.tokenId && p.direction === tracked.direction
    );

    if (side === 'BUY') {
      // positive delta = filled more; negative delta = rollback of a provisional fill
      const newShares = (existingPos?.shares ?? 0) + effectiveDelta;
      if (newShares <= 0.01) {
        if (existingPos) {
          positionMgr.close(existingPos.id, price, (tracked.reason || 'rejected') as any, false);
          logger.warn({ orderId: tracked.orderId, asset: tracked.asset, fillSize: effectiveDelta }, 'BUY rollback — position closed');
        }
      } else if (existingPos) {
        if (effectiveDelta > 0) {
          const newEntryPrice = ((existingPos.entryPrice * existingPos.shares) + (price * effectiveDelta)) / newShares;
          existingPos.entryPrice = newEntryPrice;
        }
        existingPos.shares = newShares;
        existingPos.costUsd = existingPos.entryPrice * newShares;
        logger.info({ orderId: fill.orderId, asset: tracked.asset, fillSize: effectiveDelta, totalShares: newShares }, 'BUY fill — position adjusted');
      } else {
        positionMgr.open({
          strategy: tracked.strategy,
          asset: tracked.asset,
          direction: tracked.direction as 'up' | 'down',
          tokenId: tracked.tokenId,
          conditionId: tracked.conditionId,
          entryPrice: avgPrice,
          shares: newShares,
          expiresAt: scanner.getRound().expiresAt || Date.now() + 900_000,
          wasMaker: tracked.wasMaker === true,
          targetExitPrice: tracked.targetExitPrice,
        });
        stateMachine.updatePosition(true);
        windowOpeningPrices.delete(tracked.asset);
        logger.info({ orderId: fill.orderId, asset: tracked.asset, direction: tracked.direction, fillPrice: avgPrice, fillSize: newShares }, 'BUY fill confirmed — position opened');
      }
    } else if (existingPos) {
      // SELL: positive delta reduces the long; negative delta restores (failed sell rollback)
      const newShares = existingPos.shares - effectiveDelta;
      if (newShares <= 0.01) {
        positionMgr.close(existingPos.id, price, (tracked.reason || 'trailing_stop') as any, tracked.wasMaker === true);
        logger.info({ orderId: tracked.orderId, asset: tracked.asset, fillPrice: price }, 'SELL fill confirmed — position closed');
      } else {
        existingPos.shares = newShares;
        existingPos.costUsd = existingPos.entryPrice * newShares;
        logger.info({ orderId: fill.orderId, asset: tracked.asset, fillSize: effectiveDelta, remaining: newShares }, 'SELL partial fill — position reduced');
      }
    }
  }

  /** Handle fill events from user-ws feed — this is the authoritative fill source */
  function handleFill(fill: any) {
    if (!running) return;
    const tracked = findTrackedForFill(fill);

    if (!tracked) {
      // Buffer briefly instead of dropping: the fill may arrive before recordSubmit,
      // or a maker orderId may be resolvable once the order is registered.
      pendingFills.push({ fill, ts: Date.now() });
      if (pendingFills.length > 500) pendingFills.splice(0, pendingFills.length - 500);
      logger.warn({ orderId: fill.orderId, tokenId: fill.tokenId, side: fill.side }, 'Fill for unknown order — buffered for retry');
      return;
    }

    const hasUniqueFillId = Boolean(fill.tradeId || fill.transactionHash);
    if (!hasUniqueFillId) {
      logger.warn({ orderId: fill.orderId, size: fill.size, price: fill.price }, 'Fill without tradeId/transactionHash — deduping by orderId:size:price');
    }
    const tradeKey = fill.tradeId || fill.transactionHash || `${fill.orderId}:${fill.size}:${fill.price}`;
    const side: 'BUY' | 'SELL' = fill.side === 'SELL' ? 'SELL' : 'BUY';
    const prior = appliedFills.get(tradeKey);

    // FAILED: roll back anything we provisionally applied for this trade.
    if (fill.status === 'FAILED') {
      if (prior && prior.appliedSize > 0) {
        applyFillDelta(tracked, side, -prior.appliedSize, prior.price, fill);
        appliedFills.set(tradeKey, { ...prior, appliedSize: 0 });
        logger.warn({ orderId: tracked.orderId, tradeKey, rolledBack: prior.appliedSize }, 'FAILED fill — rolled back provisional position');
      } else {
        logger.warn({ orderId: tracked.orderId, tradeKey }, 'FAILED fill — nothing to roll back');
      }
      orderMgr.recordTerminal(tracked.orderId, 'REJECTED');
      return;
    }

    const reportedSize = Number.isFinite(fill.size) ? Math.max(0, fill.size) : 0;
    const appliedSize = prior?.appliedSize ?? 0;
    const delta = reportedSize - appliedSize;

    if (delta > 0) {
      applyFillDelta(tracked, side, delta, fill.price, fill);
      appliedFills.set(tradeKey, { appliedSize: reportedSize, price: fill.price, side, orderId: tracked.orderId });
      // Bound the ledger so it cannot grow without limit over a long run.
      if (appliedFills.size > 5000) {
        const oldest = appliedFills.keys().next().value;
        if (oldest !== undefined) appliedFills.delete(oldest);
      }
    } else if (fill.status === 'CONFIRMED' && prior) {
      // Status transition for an already-applied trade — nothing to add.
      appliedFills.set(tradeKey, { ...prior, price: fill.price, appliedSize: Math.max(prior.appliedSize, reportedSize) });
    }
  }

  /** Retry buffered fills whose order has since been registered. */
  function drainPendingFills() {
    if (pendingFills.length === 0) return;
    const now = Date.now();
    const retry: any[] = [];
    for (const entry of pendingFills) {
      if (now - entry.ts > PENDING_FILL_TTL_MS) {
        logger.warn({ orderId: entry.fill.orderId }, 'Dropping expired buffered fill');
        continue;
      }
      retry.push(entry.fill);
    }
    pendingFills.length = 0;
    for (const fill of retry) {
      if (findTrackedForFill(fill)) {
        handleFill(fill);
      } else {
        pendingFills.push({ fill, ts: now });
      }
    }
  }

  /**
   * Dry-run only: resolve resting (maker) orders against the live book.
   * A resting BUY fills when the best ask drops to its limit; a resting SELL
   * fills when the best bid rises to its limit. Unfilled orders are cancelled
   * by checkOrderTimeouts. This makes paper fills realistic instead of instant.
   */
  /**
   * Cancel resting entry bids once we're too close to expiry. A bid that fills
   * at ~180s left gets time-exited almost immediately — the shadow engine showed
   * one such trade later went +113%. Better to not open than to open and be
   * forced out.
   */
  function cancelEntryBidsNearExpiry() {
    const cutoff = config.minTimeLeftSec + (config.entryCutoffSec ?? 60);
    if (scanner.getRound().timeLeftSec > cutoff) return;
    for (const o of orderMgr.getAllLiveOrders()) {
      if (o.side !== 'BUY') continue;
      if (config.dryRun) {
        orderMgr.recordTerminal(o.orderId, 'CANCELLED');
      } else if (execution) {
        execution.cancelOrder('polymarket', o.orderId)
          .then((ok) => { if (ok) orderMgr.recordTerminal(o.orderId, 'CANCELLED'); })
          .catch(() => {});
      }
    }
  }

  /**
   * Cancel resting BUY bids that the market has fallen away from. A passive bid
   * only fills when price crosses it, and during a fast drop that means getting
   * filled ABOVE the (now lower) market — the classic adverse fill. If the mid
   * has dropped more than staleBidPct below the bid, bail out.
   */
  function cancelStaleEntryBids() {
    if (config.cancelStaleBids === false) return;
    const stalePct = config.staleBidPct ?? 5;
    for (const order of orderMgr.getAllLiveOrders()) {
      if (order.side !== 'BUY') continue;
      const book = getBook(order.tokenId);
      if (!book || book.bids.length === 0 || book.asks.length === 0) continue;
      const mid = book.midPrice;
      if (mid > 0 && order.price > mid * (1 + stalePct / 100)) {
        if (config.dryRun) {
          orderMgr.recordTerminal(order.orderId, 'CANCELLED');
          logger.warn({ orderId: order.orderId, bid: order.price, mid: mid.toFixed(3) }, 'Cancelled stale entry bid (mid fell away)');
        } else if (execution) {
          execution.cancelOrder('polymarket', order.orderId)
            .then((ok) => { if (ok) orderMgr.recordTerminal(order.orderId, 'CANCELLED'); })
            .catch(() => {});
          logger.warn({ orderId: order.orderId, bid: order.price, mid: mid.toFixed(3) }, 'Cancelling stale entry bid (mid fell away)');
        }
      }
    }
  }

  function checkDryMakerFills() {
    if (!config.dryRun) return;
    for (const order of orderMgr.getAllLiveOrders()) {
      const book = getBook(order.tokenId);
      if (!book) continue;
      const remaining = Math.round((order.size - order.filledSize) * 100) / 100;
      if (remaining <= 0.001) continue;

      let crosses = false;
      if (order.side === 'BUY') {
        crosses = book.bestAsk > 0 && book.bestAsk <= order.price;
      } else {
        crosses = book.bestBid > 0 && book.bestBid >= order.price;
      }
      if (!crosses) continue;

      // Stale-price guard: never fill a buy far above the current mid. This is
      // what produced entry 0.41 while the mid was 0.23.
      if (order.side === 'BUY') {
        const maxVsMid = (config.maxFillVsMidPct ?? 3) / 100;
        if (book.midPrice > 0 && order.price > book.midPrice * (1 + maxVsMid)) {
          orderMgr.recordTerminal(order.orderId, 'CANCELLED');
          logger.warn(
            { orderId: order.orderId, limit: order.price, mid: book.midPrice },
            'Stale bid above mid — cancelled instead of filling'
          );
          continue;
        }
      }

      logger.info(
        { orderId: order.orderId, side: order.side, limit: order.price, bestBid: book.bestBid, bestAsk: book.bestAsk, size: remaining },
        'dry-run maker order filled on book cross'
      );
      handleFill({
        orderId: order.orderId,
        tradeId: `${order.orderId}:fill:${order.filledSize}`,
        tokenId: order.tokenId,
        side: order.side,
        price: order.price,
        size: remaining,
        status: 'CONFIRMED',
      });
    }
  }

  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  /**
   * Consecutive-loss circuit breaker. Halts NEW entries for a cooldown and
   * flattens open positions, but NEVER stops managing exits — an unmanaged
   * open position was the cause of a winner riding all the way to a loss.
   */
  function updateRiskBreaker() {
    const closedAll = positionMgr.getClosed();
    if (closedAll.length > lastClosedCount) {
      for (let i = lastClosedCount; i < closedAll.length; i++) {
        if (closedAll[i].netPnlUsd < 0) consecutiveLosses++;
        else consecutiveLosses = 0;
      }
      lastClosedCount = closedAll.length;
    }
    if (!haltNewEntries && consecutiveLosses >= MAX_CONSECUTIVE_LOSSES) {
      const cooldownSec = config.breakerCooldownSec ?? 300;
      haltNewEntries = true;
      haltNewEntriesUntil = Date.now() + cooldownSec * 1000;
      riskLastEvent = 'breaker_tripped';
      riskLastEventAt = Date.now();
      logger.error(
        { consecutiveLosses, cooldownSec },
        'CIRCUIT BREAKER: consecutive losses — halting new entries and flattening positions'
      );
      logger.error({ alert: true, message: `熔断: ${consecutiveLosses} 连败，暂停开仓 (${cooldownSec}s)` }, 'ALERT');
      // Halt NEW risk only: cancel resting entry bids. Do NOT flatten open
      // positions — the shadow data showed a forced flatten cut a position that
      // later went +34%. Existing positions keep being managed (TP/stop/expiry).
      cancelAllEntryBids('circuit_breaker');
    }
  }

  /** Cancel every resting BUY (entry) order; leaves positions untouched. */
  function cancelAllEntryBids(reason: string) {
    for (const o of orderMgr.getAllLiveOrders()) {
      if (o.side !== 'BUY') continue;
      if (config.dryRun) {
        orderMgr.recordTerminal(o.orderId, 'CANCELLED');
      } else if (execution) {
        execution.cancelOrder('polymarket', o.orderId)
          .then((ok) => { if (ok) orderMgr.recordTerminal(o.orderId, 'CANCELLED'); })
          .catch(() => {});
      }
    }
    logger.warn({ reason }, 'Cancelled all entry bids (positions kept)');
  }

  /** Cancel all resting orders and market-exit every open position. */
  function flattenAllPositions(reason: string) {
    for (const o of orderMgr.getAllLiveOrders()) {
      execution?.cancelOrder('polymarket', o.orderId)
        .then((ok) => { if (ok) orderMgr.recordTerminal(o.orderId, 'CANCELLED'); })
        .catch(() => {});
    }
    const open = positionMgr.getOpen();
    for (const pos of open) {
      const book = getBook(pos.tokenId);
      const price = book?.bestBid ?? pos.currentPrice;
      executeExit(pos, 'force_exit', price, false, true);
    }
    logger.warn({ reason, open: open.length }, 'Flattening positions');
  }

  function checkExits() {
    if (!running) return;

    // Retry any fills that arrived before their order was registered
    drainPendingFills();

    // Cancel bids the market has fallen away from (avoid adverse fills).
    cancelStaleEntryBids();

    // Never let a bid fill too close to expiry (avoids instant time-exit).
    cancelEntryBidsNearExpiry();

    // Dry-run: fill resting maker orders when the book crosses their limit.
    checkDryMakerFills();

    // Check spot reversal and quick profit for each position.
    // In simple-exit mode these extra exits are disabled (let the thesis run).
    if (config.simpleExitEnabled === false) {
      for (const pos of positionMgr.getOpen()) {
        const market = scanner.getRound().markets.find((m: any) => m.asset === pos.asset);
        if (!market) continue;
        const spotBuf = getSpotBuffer(pos.asset);
        const spotMove5 = spotBuf.movePct(5);
        const holdSec = (Date.now() - pos.enteredAt) / 1000;
        const book = getBook(pos.tokenId);
        const price = book && book.bids.length > 0 && book.asks.length > 0 && book.midPrice > 0 ? book.midPrice : (book?.bestBid ?? pos.currentPrice);
        const pnlPct = pos.entryPrice > 0 ? ((price - pos.entryPrice) / pos.entryPrice) * 100 : 0;

        // If spot moved against position direction, exit early (only after 3s hold)
        const isUpPosition = pos.direction === 'up';
        const spotMovedAgainst = isUpPosition ? spotMove5 < -config.spotReversalThresholdPct : spotMove5 > config.spotReversalThresholdPct;

        if (spotMovedAgainst && holdSec >= 3) {
          executeExit(pos, 'spot_reversal', price, false);
          continue;
        }

        // Quick scalp: if held >30s and profit >5%, take it
        if (holdSec >= 30 && pnlPct >= 5) {
          executeExit(pos, 'quick_profit', price, true);
          continue;
        }
      }
    }

    const exits = positionMgr.checkExits(getBook);

    for (const { position, reason, exitPrice, useMaker } of exits) {
      // For stop loss, always use FOK (speed)
      const actualUseMaker = reason === 'stop_loss' || reason === 'force_exit' ? false : useMaker;
      executeExit(position, reason, exitPrice, actualUseMaker);

      // Update state machine
      stateMachine.updatePosition(positionMgr.getOpen().length > 0);
      if (stateMachine.getState() === 'HOLDING') {
        stateMachine.transition('COOLDOWN', `Closed: ${reason}`);
      }
    }

    // Consecutive-loss circuit breaker (once per cycle, never strands positions)
    updateRiskBreaker();

    // Note: positionMgr.checkExits() above already refreshed each position's
    // exit state/HWM from the current book, so no separate tick pass is needed
    // (double-ticking the same quote inflated the HWM confirmation counter).
  }

  /**
   * Restart reconciliation:
   *  1. For hydrated live orders, decide filled vs cancelled using open orders +
   *     trade history, and reconstruct the position when it actually filled.
   *  2. Adopt any held position in the current round that we don't track, so it
   *     can still be exited (prevents orphan/ghost inventory across restarts).
   */
  async function reconcileOnStart(): Promise<void> {
    if (!execution) return;
    // Dry run is self-contained (simulated fills) — no exchange reconciliation.
    if (config.dryRun) return;

    const tokenInfo = currentTokenInfo();

    // Fetch live exchange orders once.
    let openList: Awaited<ReturnType<typeof execution.getOpenOrdersChecked>> = null;
    try {
      openList = await execution.getOpenOrdersChecked('polymarket');
    } catch { openList = null; }
    const openIds = openList ? new Set(openList.map(o => o.orderId)) : null;

    // Cancel exchange orders we have NO record of (orphans from a crash / lost
    // response). Scoped to tokens of the current crypto round so we never touch
    // unrelated orders that share the wallet. This runs even with no persisted
    // orders — that is exactly when a ghost order is most likely.
    if (openList) {
      const known = new Set(orderMgr.getAllOrders().map(o => o.orderId));
      for (const o of openList) {
        if (known.has(o.orderId)) continue;
        if (!tokenInfo.has(o.tokenId || '')) continue;
        logger.error({ orderId: o.orderId, tokenId: o.tokenId, side: o.side, price: o.price }, 'Restart reconcile: cancelling UNKNOWN open order (orphan)');
        execution.cancelOrder('polymarket', o.orderId).catch(() => {});
      }
    }

    const hydrated = orderMgr.getAllLiveOrders();
    if (hydrated.length > 0) {
      let trades: Array<{ tokenId: string; side: 'BUY' | 'SELL'; price: number; size: number; timestamp: Date }> | null = null;
      try {
        if (typeof execution.getTrades === 'function') trades = await execution.getTrades(500);
      } catch { trades = null; }

      // If we cannot confirm order status, never mark anything cancelled.
      if (!openIds || !trades) {
        logger.warn(
          { openOrdersAvailable: !!openIds, tradesAvailable: !!trades, count: hydrated.length },
          'Restart reconcile: status queries unavailable — keeping recovered orders live for timeout/cancel'
        );
      } else {
        for (const o of hydrated) {
          if (openIds.has(o.orderId)) continue; // still on book — normal timeout/cancel path

          const since = o.submittedAt - 60_000;
          const matches = trades.filter(t =>
            t.tokenId === o.tokenId &&
            t.side === o.side &&
            new Date(t.timestamp).getTime() >= since
          );
          const filledQty = matches.reduce((s, t) => s + (t.size || 0), 0);

          if (filledQty > 0.001) {
            const notional = matches.reduce((s, t) => s + (t.size || 0) * (t.price || 0), 0);
            const avg = notional > 0 ? notional / filledQty : o.price;
            applyReconciledFill(o, Math.min(filledQty, o.size), avg);
            logger.warn({ orderId: o.orderId, qty: Math.min(filledQty, o.size), avg, tokenId: o.tokenId }, 'Restart reconcile: order had filled — position reconstructed');
          } else {
            orderMgr.recordTerminal(o.orderId, 'CANCELLED');
            logger.info({ orderId: o.orderId }, 'Restart reconcile: order not open and no trade found — marked cancelled');
          }
        }
      }
    }

    // Safety net: adopt held positions in the current round that we don't track.
    try {
      if (typeof execution.getPositions !== 'function') return;
      const held = await execution.getPositions();
      if (!held) {
        logger.warn('Restart reconcile: positions query failed — cannot adopt untracked holdings');
        return;
      }
      for (const p of held) {
        if (!p.tokenId || p.size <= 0.01) continue;
        const info = tokenInfo.get(p.tokenId);
        if (!info) continue; // only manage tokens belonging to the current crypto round
        const existing = positionMgr.getOpen().find(x => x.tokenId === p.tokenId && x.direction === info.direction);
        if (existing) continue;
        positionMgr.open({
          strategy: 'reconcile',
          asset: info.market.asset,
          direction: info.direction,
          tokenId: p.tokenId,
          conditionId: p.conditionId || info.market.conditionId,
          entryPrice: p.avgPrice || p.currentPrice || 0.5,
          shares: p.size,
          expiresAt: info.market.expiresAt || Date.now() + 900_000,
          wasMaker: false,
          targetExitPrice: undefined,
        });
        logger.warn({ tokenId: p.tokenId, size: p.size, avgPrice: p.avgPrice }, 'Restart reconcile: adopted untracked held position');
      }
    } catch (err) {
      logger.warn({ err }, 'Restart reconcile: failed to fetch positions');
    }
  }

  return {
    async start() {
      if (running) return;
      running = true;
      startedAt = Date.now();

      // Initialize state machine
      stateMachine.transition('OBSERVING', 'Engine started');

      logger.info(
        {
          assets: config.assets,
          dryRun: config.dryRun,
          size: config.sizeUsd,
          strategies: Object.entries(enabled).filter(([, v]) => v).map(([k]) => k),
        },
        'Crypto HFT engine starting'
      );

      scanner.start();
      await scanner.refresh();

      // Subscribe to spot prices
      for (const asset of config.assets) {
        const unsub = cryptoFeed.subscribeSymbol(asset, onSpotTick);
        unsubscribes.push(unsub);
      }

      // Wire user-ws fill feed for async fill detection
      if (execution) {
        try {
          await execution.connectFillsWebSocket();
          unsubscribes.push(() => execution.disconnectFillsWebSocket());
          // Save onFill disposer to prevent callback accumulation on restart
          const fillUnsub = execution.onFill((fill) => {
            logger.info({ fill }, 'Fill received via user-ws');
            handleFill(fill);
          });
          unsubscribes.push(fillUnsub);
          logger.info('User-WS fill feed connected');
        } catch (err) {
          logger.warn({ err }, 'Failed to connect user-ws fill feed — fills will not be detected');
        }
      }

      // Exit check loop (every 50ms — real HFT needs fast exits)
      exitCheckInterval = setInterval(checkExits, 50);

      // Order timeout check loop (every 10s — cancel unfilled GTC orders)
      orderTimeoutInterval = setInterval(checkOrderTimeouts, 10_000);

      // Shadow signal logger: finalize records every second
      signalTickInterval = setInterval(() => { signalLog.tick(); shadowEngine.tick(); }, 1000);

      // Hydrate order manager from persisted orders
      const savedOrders = loadAllOrders();
      orderMgr.hydrate(savedOrders);
      const liveOrders = savedOrders.filter(o => ['SUBMITTED', 'LIVE', 'PARTIAL'].includes(o.status));
      if (liveOrders.length > 0) {
        logger.warn({ count: liveOrders.length }, 'Recovered live orders from previous run — reconciling');
      }
      // Reconcile against open orders + trade history, and adopt held positions.
      await reconcileOnStart();

      logger.info('Order lifecycle manager active');
    },

    stop() {
      running = false;
      stateMachine.transition('IDLE', 'Engine stopped');
      // Cancel any pending maker→taker exit escalations.
      for (const t of makerExitTimers.values()) clearTimeout(t);
      makerExitTimers.clear();
      // Cancel all live orders before stopping
      const liveOrders = orderMgr.getAllLiveOrders();
      if (liveOrders.length > 0 && execution) {
        logger.warn({ count: liveOrders.length }, 'Cancelling all live orders before stop');
        for (const order of liveOrders) {
          execution.cancelOrder('polymarket', order.orderId)
            .then(() => orderMgr.recordTerminal(order.orderId, 'CANCELLED'))
            .catch(() => {});
        }
      }
      for (const unsub of unsubscribes) unsub();
      unsubscribes.length = 0;
      scanner.stop();
      if (exitCheckInterval) { clearInterval(exitCheckInterval); exitCheckInterval = null; }
      if (orderTimeoutInterval) { clearInterval(orderTimeoutInterval); orderTimeoutInterval = null; }
      if (signalTickInterval) { clearInterval(signalTickInterval); signalTickInterval = null; }
      try { signalLog.flush(); } catch { /* best effort */ }
      try { shadowEngine.flush(); } catch { /* best effort */ }
      logger.info('Crypto HFT engine stopped');
    },

    onOrderbook(tokenId, bids, asks) {
      const snapshot = buildOrderbookSnapshot(tokenId, bids, asks);
      books.set(tokenId, snapshot);
      spreadTracker.record(tokenId, snapshot.spread);
      depthTracker.record(tokenId, snapshot.bidDepth, snapshot.askDepth);
      bidTracker.record(tokenId, snapshot.bestBid);
      signalLog.onPrice(tokenId, snapshot.midPrice);
      shadowEngine.onPrice(tokenId, snapshot.midPrice, Date.now(), { bid: snapshot.bestBid, ask: snapshot.bestAsk });
      trendTracker.onPrice(tokenId, snapshot.midPrice);

      // Update poly price in market scanner + poly buffer
      for (const market of scanner.getRound().markets) {
        if (market.upTokenId === tokenId) {
          scanner.updatePrice(market.conditionId, snapshot.midPrice, market.downPrice);
          getPolyBuffer(market.asset).push(snapshot.midPrice);
          polyLastTs.set(market.conditionId, Date.now());
        } else if (market.downTokenId === tokenId) {
          scanner.updatePrice(market.conditionId, market.upPrice, snapshot.midPrice);
          getPolyBuffer(market.asset).push(snapshot.midPrice);
          polyLastTs.set(market.conditionId, Date.now());
        }
      }
    },

    updatePrices(conditionId, upPrice, downPrice) {
      scanner.updatePrice(conditionId, upPrice, downPrice);
      // Update poly buffer for each asset
      for (const market of scanner.getRound().markets) {
        if (market.conditionId === conditionId) {
          const mid = (upPrice + downPrice) / 2;
          getPolyBuffer(market.asset).push(mid);
          polyLastTs.set(conditionId, Date.now());
          if (market.upTokenId) { signalLog.onPrice(market.upTokenId, upPrice); shadowEngine.onPrice(market.upTokenId, upPrice); trendTracker.onPrice(market.upTokenId, upPrice); }
          if (market.downTokenId) { signalLog.onPrice(market.downTokenId, downPrice); shadowEngine.onPrice(market.downTokenId, downPrice); trendTracker.onPrice(market.downTokenId, downPrice); }

          // Also update open positions for this market
          for (const pos of positionMgr.getOpen()) {
            if (pos.asset === market.asset) {
              const currentPrice = pos.direction === 'up' ? upPrice : downPrice;
              if (isFinite(currentPrice) && currentPrice > 0) {
                positionMgr.tick(pos.id, currentPrice, null);
                logger.info({ asset: pos.asset, dir: pos.direction, entry: pos.entryPrice, current: currentPrice }, 'Position price updated');
              }
            }
          }
        }
      }
    },

    getStats: () => positionMgr.getStats(),
    getPositions: () => positionMgr.getOpen(),
    getClosed: () => positionMgr.getClosed(),
    getMarkets: () => scanner.getRound().markets,
    getPendingEntryCount: () => orderMgr.getAllLiveOrders().filter((o) => o.side === 'BUY').length,

    getRoundInfo() {
      const round = scanner.getRound();
      const ct = scanner.canTrade();
      return { slot: round.slot, ageSec: round.ageSec, timeLeftSec: round.timeLeftSec, canTrade: ct.ok };
    },

    getClockOffset() {
      return scanner.getClockOffset ? scanner.getClockOffset() : 0;
    },

    getPolyLastTs(conditionId: string) {
      return polyLastTs.get(conditionId) ?? 0;
    },

    getStateMachine: () => stateMachine,

    getRiskState: () => ({
      halted: haltNewEntries,
      haltUntil: haltNewEntriesUntil,
      haltedAt: haltNewEntries ? haltNewEntriesUntil - (config.breakerCooldownSec ?? 300) * 1000 : 0,
      consecutiveLosses,
      lastEvent: riskLastEvent,
      lastEventAt: riskLastEventAt,
    }),

    updateConfig(partial) {
      config = { ...config, ...partial };
      logger.info({ updated: Object.keys(partial) }, 'Config updated');
    },

    getConfig: () => ({ ...config }),

    setStrategyEnabled(name, value) {
      if (name in enabled) {
        enabled[name] = value;
        logger.info({ strategy: name, enabled: value }, 'Strategy toggled');
      }
    },

    getEnabledStrategies: () => ({ ...enabled }),
  };
}

// ── Re-exports ──────────────────────────────────────────────────────────────

export type { CryptoHftConfig, CryptoMarket, TradeSignal, HftStats, OpenPosition, ClosedPosition, OrderMode, StrategyPreset, OrderbookSnapshot } from './types.js';
export type { PositionManager } from './positions.js';
export type { MarketScanner } from './market-scanner.js';
export { createMarketScanner } from './market-scanner.js';
export { createPositionManager } from './positions.js';
export { buildOrderbookSnapshot, createSpreadTracker, createDepthTracker, createBidTracker } from './orderbook.js';
export { createPriceBuffer, evaluateMomentum, evaluateMeanReversion, evaluatePennyClipper, evaluateExpiryFade } from './strategies.js';
export { savePreset, loadPreset, deletePreset, listPresets, BUILT_IN_PRESETS } from './presets.js';
export { takerFee, takerFeePct } from './types.js';
