/**
 * Position Manager — Full exit logic ported from firstorder.rs
 *
 * Exits (priority order):
 *  1. Force exit (< forceExitSec before expiry)
 *  2. Take profit
 *  3. Stop loss
 *  4. Ratchet floor (progressive giveback from confirmed high)
 *  5. Trailing stop (time-aware: tightens near expiry)
 *  6. Depth collapse (depth -60%, price dropping)
 *  7. Stale profit (up +9%, bid unchanged 7s)
 *  8. Stagnant profit (at +3% for 13s, no progress)
 *  9. Time exit (< minTimeLeftSec)
 */

import { logger } from '../../utils/logger.js';
import { takerFeePct } from './types.js';
import { saveTrade } from './trade-db.js';
import {
  createExitState,
  updateExitState,
  decideExit,
  executableBid,
  referenceMid,
  type ExitState,
} from './exit-policy.js';
import type {
  CryptoHftConfig,
  OpenPosition,
  ClosedPosition,
  ExitReason,
  SignalDirection,
  HftStats,
  OrderbookSnapshot,
} from './types.js';

// Re-exported for any module/tests that imported the tables from here. The
// single source of truth now lives in exit-policy.ts (shared with the replay).
export {
  getRatchetFloor,
  getProfitTrailPct,
  getTimeTrailPct,
  effectiveStopPct,
} from './exit-policy.js';

// ── Position Manager ────────────────────────────────────────────────────────

export interface PositionManager {
  open(params: {
    strategy: string;
    asset: string;
    direction: SignalDirection;
    tokenId: string;
    conditionId: string;
    entryPrice: number;
    shares: number;
    expiresAt: number;
    wasMaker: boolean;
    targetExitPrice?: number;
  }): OpenPosition;

  /** Check all positions for exit conditions. Returns exits to execute. */
  checkExits(
    getBook: (tokenId: string) => OrderbookSnapshot | null,
    now?: number
  ): Array<{ position: OpenPosition; reason: ExitReason; exitPrice: number; useMaker: boolean }>;

  /** Record a price tick for a position. Updates HWM, staleness, etc. */
  tick(positionId: string, price: number, book: OrderbookSnapshot | null): void;

  /** Mark a position closed after execution. */
  close(positionId: string, exitPrice: number, reason: ExitReason, wasMaker: boolean): ClosedPosition | null;

  /** Can we open a new position? */
  canOpen(asset?: string, direction?: SignalDirection): { ok: boolean; reason?: string };

  getOpen(): OpenPosition[];
  getClosed(): ClosedPosition[];
  getStats(): HftStats;
  resetDaily(): void;
}

export interface PositionHooks {
  onOpen?: (pos: OpenPosition) => void;
  onClose?: (pos: ClosedPosition) => void;
}

export function createPositionManager(
  getConfig: () => CryptoHftConfig,
  hooks?: PositionHooks
): PositionManager {
  const positions = new Map<string, OpenPosition>();
  // Replayable exit state per open position (HWM, staleness, depth). The pure
  // exit policy in exit-policy.ts reads this; it is the same state the offline
  // replay reconstructs, keeping backtest and live identical.
  const exitStates = new Map<string, ExitState>();
  const closed: ClosedPosition[] = [];
  let dailyPnl = 0;
  let lastStopLossAt = 0;
  let nextId = 1;

  // Per coin+direction exit cooldowns
  const exitCooldowns = new Map<string, number>();
  // Per-asset cooldowns to avoid whipsawing a chopping market (e.g. SOL up then
  // SOL down within a minute, both stopped out).
  const assetLastExitAt = new Map<string, number>();
  const assetLastLossAt = new Map<string, number>();

  function cooldownKey(asset: string, direction: SignalDirection): string {
    return `${asset}_${direction}`;
  }

  return {
    open(params) {
      const id = `hft-${nextId++}`;
      const entryFeePct = params.wasMaker ? 0 : takerFeePct(params.entryPrice);
      const now = Date.now();

      const pos: OpenPosition = {
        id,
        strategy: params.strategy,
        asset: params.asset,
        direction: params.direction,
        tokenId: params.tokenId,
        conditionId: params.conditionId,
        entryPrice: params.entryPrice,
        currentPrice: params.entryPrice,
        prevPrice: params.entryPrice,
        shares: params.shares,
        costUsd: params.entryPrice * params.shares,
        wasMakerEntry: params.wasMaker,
        entryFeePct,
        targetExitPrice: params.targetExitPrice,
        highWaterMark: params.entryPrice,
        hwmConfirmCount: 0,
        confirmedHigh: params.entryPrice,
        enteredAt: now,
        expiresAt: params.expiresAt,
        lastBidPrice: params.entryPrice,
        bidUnchangedSince: now,
        lastProgressAt: now,
        lastProgressPct: 0,
        initialDepth: 0,
        highPnlPct: 0,
        lowPnlPct: 0,
        wasEverPositive: false,
      };

      positions.set(id, pos);
      logger.info(
        {
          id,
          strategy: pos.strategy,
          asset: pos.asset,
          dir: pos.direction,
          price: pos.entryPrice.toFixed(2),
          shares: pos.shares,
          maker: pos.wasMakerEntry,
          fee: entryFeePct.toFixed(2) + '%',
        },
        'Position opened'
      );
      hooks?.onOpen?.(pos);
      return pos;
    },

    tick(positionId, price, book) {
      const config = getConfig();
      const pos = positions.get(positionId);
      if (!pos) return;
      let state = exitStates.get(positionId);
      if (!state) {
        state = createExitState(pos.entryPrice, pos.enteredAt);
        exitStates.set(positionId, state);
      }
      const now = Date.now();

      // HWM / staleness / depth are tracked off the EXECUTABLE bid (same price a
      // sell actually realizes), so recorded peaks match tradeable value.
      updateExitState(state, pos.entryPrice, book, now, config);

      // Mirror state back onto the position for logs / persistence / dashboard.
      const val = book ? (executableBid(book) || referenceMid(book, price)) : price;
      pos.prevPrice = pos.currentPrice;
      pos.currentPrice = val;
      pos.highPnlPct = state.highPnlPct;
      pos.lowPnlPct = state.lowPnlPct;
      pos.wasEverPositive = state.wasEverPositive;
      pos.highWaterMark = state.highWaterMark;
      pos.hwmConfirmCount = state.hwmConfirmCount;
      pos.confirmedHigh = state.confirmedHigh;
      pos.lastBidPrice = state.lastBidPrice;
      pos.bidUnchangedSince = state.bidUnchangedSince;
      pos.lastProgressAt = state.lastProgressAt;
      pos.lastProgressPct = state.lastProgressPct;
      pos.initialDepth = state.initialDepth;
    },

    checkExits(getBook, now = Date.now()) {
      const config = getConfig();
      const exits: Array<{ position: OpenPosition; reason: ExitReason; exitPrice: number; useMaker: boolean }> = [];

      for (const pos of positions.values()) {
        const book = getBook(pos.tokenId);
        let state = exitStates.get(pos.id);
        if (!state) {
          state = createExitState(pos.entryPrice, pos.enteredAt);
          exitStates.set(pos.id, state);
        }
        // Update from this tick's book BEFORE deciding, so the trigger and the
        // recorded HWM use the same quote (previously the decision ran on a tick
        // newer than the HWM, understating peaks).
        updateExitState(state, pos.entryPrice, book, now, config);

        const exitPrice = executableBid(book) || referenceMid(book, pos.currentPrice);
        const timeLeftSec = (pos.expiresAt - now) / 1000;
        const holdSec = (now - pos.enteredAt) / 1000;

        // Fixed-target strategies (sharp_reversal): sell into the bid once it
        // reaches the target. A maker offer at the target is appropriate.
        if (pos.targetExitPrice && exitPrice >= pos.targetExitPrice && holdSec >= (config.exitGraceSec ?? 3)) {
          exits.push({ position: pos, reason: 'take_profit', exitPrice: pos.targetExitPrice, useMaker: true });
          continue;
        }

        const decision = decideExit({
          entryPrice: pos.entryPrice,
          book,
          fallbackPrice: pos.currentPrice,
          timeLeftSec,
          holdSec,
          state,
          now,
          cfg: config,
        });
        if (decision) {
          exits.push({ position: pos, reason: decision.reason, exitPrice, useMaker: decision.useMaker });
        }
      }

      return exits;
    },

    close(positionId, exitPrice, reason, wasMaker) {
      const config = getConfig();
      const pos = positions.get(positionId);
      if (!pos) return null;

      positions.delete(positionId);
      exitStates.delete(positionId);

      const exitFeePct = wasMaker ? 0 : takerFeePct(exitPrice);
      const pnlPct = pos.entryPrice > 0 ? ((exitPrice - pos.entryPrice) / pos.entryPrice) * 100 : 0;
      const grossPnlUsd = (exitPrice - pos.entryPrice) * pos.shares;
      const entryFeeUsd = (pos.entryFeePct / 100) * pos.entryPrice * pos.shares;
      const exitFeeUsd = (exitFeePct / 100) * exitPrice * pos.shares;
      const netPnlUsd = grossPnlUsd - entryFeeUsd - exitFeeUsd;
      const netPnlPct = pos.costUsd > 0 ? (netPnlUsd / pos.costUsd) * 100 : 0;
      const holdTimeSec = (Date.now() - pos.enteredAt) / 1000;

      dailyPnl += netPnlUsd;
      if (reason === 'stop_loss') lastStopLossAt = Date.now();

      // Set exit cooldown for this coin+direction
      exitCooldowns.set(cooldownKey(pos.asset, pos.direction), Date.now());
      assetLastExitAt.set(pos.asset, Date.now());
      if (netPnlUsd < 0) assetLastLossAt.set(pos.asset, Date.now());

      const result: ClosedPosition = {
        ...pos,
        exitPrice,
        exitReason: reason,
        exitedAt: Date.now(),
        wasMakerExit: wasMaker,
        exitFeePct,
        pnlUsd: grossPnlUsd,
        pnlPct,
        netPnlUsd,
        netPnlPct,
        holdTimeSec,
      };
      closed.push(result);
      if (closed.length > 5000) {
        closed.splice(0, closed.length - 5000);
      }

      logger.info(
        {
          id: positionId,
          strat: pos.strategy,
          asset: pos.asset,
          dir: pos.direction,
          reason,
          gross: grossPnlUsd.toFixed(3),
          net: netPnlUsd.toFixed(3),
          pct: netPnlPct.toFixed(1) + '%',
          hold: holdTimeSec.toFixed(0) + 's',
          makerEntry: pos.wasMakerEntry,
          makerExit: wasMaker,
        },
        'Position closed'
      );

      // Save to trade database
      const settings = {
        stopPct: config.tightStopEnabled !== false ? (config.tightStopPct ?? 12) : config.stopLossPct,
        tightStop: config.tightStopEnabled !== false,
        crashFilter: config.crashFilterEnabled !== false,
        crashMinHighAgeSec: config.crashFilterMinHighAgeSec ?? 20,
        crashMaxSpotMovePct: config.crashFilterMaxSpotMovePct ?? 3,
        slippageGuard: config.slippageGuardEnabled !== false,
        slippageGuardMaxPct: config.slippageGuardMaxPct ?? 4,
        propTrail: config.proportionalTrailEnabled !== false,
        propTrailPct: config.proportionalTrailPct ?? 10,
        trailingMinHighPct: config.trailingMinHighPct ?? 5,
        minTrailPct: config.minTrailPct ?? 5,
        exitGraceSec: config.exitGraceSec ?? 3,
        entryFactor: config.trendEntryFactor ?? 0.9,
      };
      saveTrade(result, pos.entryPrice, exitPrice, settings);
      hooks?.onClose?.(result);

      return result;
    },

    canOpen(asset, direction) {
      const config = getConfig();
      if (positions.size >= config.maxPositions) {
        return { ok: false, reason: `Max positions (${config.maxPositions})` };
      }
      if (dailyPnl <= -config.maxDailyLossUsd) {
        return { ok: false, reason: `Daily loss limit ($${config.maxDailyLossUsd})` };
      }
      // Stop loss cooldown
      if (config.stopLossCooldownSec > 0 && Date.now() - lastStopLossAt < config.stopLossCooldownSec * 1000) {
        const left = Math.ceil((config.stopLossCooldownSec * 1000 - (Date.now() - lastStopLossAt)) / 1000);
        return { ok: false, reason: `SL cooldown: ${left}s` };
      }
      // Already have position on this asset?
      if (asset) {
        for (const pos of positions.values()) {
          if (pos.asset === asset) {
            return { ok: false, reason: `Already in ${asset}` };
          }
        }
      }
      // Exit cooldown per coin+direction
      if (asset && direction) {
        const key = cooldownKey(asset, direction);
        const lastExit = exitCooldowns.get(key);
        if (lastExit && Date.now() - lastExit < config.exitCooldownSec * 1000) {
          const left = Math.ceil((config.exitCooldownSec * 1000 - (Date.now() - lastExit)) / 1000);
          return { ok: false, reason: `Exit cooldown ${asset} ${direction}: ${left}s` };
        }
      }
      // Per-asset cooldown after a loss (avoid whipsawing a chopping asset)
      if (asset) {
        const lossSec = config.lossCooldownSec ?? 0;
        const lastLoss = assetLastLossAt.get(asset);
        if (lossSec > 0 && lastLoss && Date.now() - lastLoss < lossSec * 1000) {
          const left = Math.ceil((lossSec * 1000 - (Date.now() - lastLoss)) / 1000);
          return { ok: false, reason: `Loss cooldown ${asset}: ${left}s` };
        }
        const assetSec = config.assetCooldownSec ?? 0;
        const lastExit = assetLastExitAt.get(asset);
        if (assetSec > 0 && lastExit && Date.now() - lastExit < assetSec * 1000) {
          const left = Math.ceil((assetSec * 1000 - (Date.now() - lastExit)) / 1000);
          return { ok: false, reason: `Asset cooldown ${asset}: ${left}s` };
        }
      }
      return { ok: true };
    },

    getOpen() {
      return [...positions.values()];
    },

    getClosed() {
      return [...closed];
    },

    getStats() {
      const wins = closed.filter((c) => c.netPnlUsd > 0);
      const losses = closed.filter((c) => c.netPnlUsd <= 0);
      const grossPnl = closed.reduce((s, c) => s + c.pnlUsd, 0);
      const netPnl = closed.reduce((s, c) => s + c.netPnlUsd, 0);
      const fees = grossPnl - netPnl;
      const holdTimes = closed.map((c) => c.holdTimeSec);
      const makerEntries = closed.filter((c) => c.wasMakerEntry).length;
      const makerExits = closed.filter((c) => c.wasMakerExit).length;

      const exitReasons: Record<string, number> = {};
      for (const c of closed) {
        exitReasons[c.exitReason] = (exitReasons[c.exitReason] ?? 0) + 1;
      }

      return {
        totalTrades: closed.length,
        wins: wins.length,
        losses: losses.length,
        winRate: closed.length > 0 ? (wins.length / closed.length) * 100 : 0,
        grossPnlUsd: grossPnl,
        feesUsd: fees,
        netPnlUsd: netPnl,
        dailyPnlUsd: dailyPnl,
        openPositions: positions.size,
        bestTradePct: closed.length > 0 ? Math.max(...closed.map((c) => c.netPnlPct)) : 0,
        worstTradePct: closed.length > 0 ? Math.min(...closed.map((c) => c.netPnlPct)) : 0,
        avgHoldTimeSec: holdTimes.length > 0 ? holdTimes.reduce((a, b) => a + b, 0) / holdTimes.length : 0,
        makerEntryRate: closed.length > 0 ? (makerEntries / closed.length) * 100 : 0,
        makerExitRate: closed.length > 0 ? (makerExits / closed.length) * 100 : 0,
        exitReasons,
      };
    },

    resetDaily() {
      dailyPnl = 0;
      lastStopLossAt = 0;
      exitCooldowns.clear();
    },
  };
}
