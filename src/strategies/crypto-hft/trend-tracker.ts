/**
 * TrendTracker — robust, decoupled trend confirmation.
 *
 * Replaces the old brittle "continuous price >= threshold" streak (a single
 * wick below the threshold reset the whole timer, and it only updated on Binance
 * spot ticks). Instead we keep a rolling time window per token and confirm when
 * the fraction of samples above the threshold over that window clears a ratio.
 * This tolerates brief dips while still requiring a durable trend.
 *
 * Phases per token (reset every round):
 *   IDLE      — not enough above-threshold history
 *   BUILDING  — some history, ratio not yet high enough
 *   CONFIRMED — the trend is established (bids may be placed)
 *   BROKEN    — a confirmed trend fell below the break price (regime change)
 */

import type { CryptoHftConfig } from './types.js';

export type TrendPhase = 'IDLE' | 'BUILDING' | 'CONFIRMED' | 'BROKEN';

export interface TrendState {
  phase: TrendPhase;
  price: number;
  /** Fraction (0..1) of samples in the window that were >= threshold */
  aboveRatio: number;
  /** Number of samples currently in the window */
  samples: number;
  /** How long the token has held above the threshold within this window (sec) */
  aboveSec: number;
  confirmedAt?: number;
  brokenAt?: number;
}

interface Sample {
  t: number;
  p: number;
}

export interface TrendTracker {
  /** Feed the latest price for a token (called on every book/price update). */
  onPrice(tokenId: string, price: number, now?: number): void;
  /** Reset all state when the round changes. */
  resetIfNewRound(slot: number): void;
  get(tokenId: string): TrendState | null;
  isConfirmed(tokenId: string): boolean;
  /** Set of currently-confirmed tokenIds (safe to mutate by callers? returns a copy). */
  confirmedTokens(): Set<string>;
  /** Fired when a confirmed trend breaks below the break price. */
  onBroken(cb: (tokenId: string, price: number) => void): void;
  reset(): void;
}

export function createTrendTracker(getConfig: () => CryptoHftConfig): TrendTracker {
  const states = new Map<string, { phase: TrendPhase; samples: Sample[]; confirmedAt?: number; brokenAt?: number }>();
  let roundSlot = -1;
  const brokenCbs: Array<(tokenId: string, price: number) => void> = [];

  function fire(tokenId: string, price: number) {
    for (const cb of brokenCbs) {
      try { cb(tokenId, price); } catch { /* isolation */ }
    }
  }

  return {
    onPrice(tokenId, price, now = Date.now()) {
      if (!tokenId || !Number.isFinite(price) || price <= 0) return;
      const cfg = getConfig();
      const windowMs = Math.max(10_000, (cfg.trendConfirmSec ?? 60) * 1000);
      const threshold = cfg.trendMinPrice ?? 0.55;
      const brokenPrice = cfg.trendBrokenPrice ?? 0.35;
      const ratioNeeded = cfg.trendRatio ?? 0.8;

      let s = states.get(tokenId);
      if (!s) {
        s = { phase: 'IDLE', samples: [] };
        states.set(tokenId, s);
      }
      s.samples.push({ t: now, p: price });
      while (s.samples.length > 0 && now - s.samples[0].t > windowMs) s.samples.shift();

      const total = s.samples.length;
      const above = s.samples.filter((x) => x.p >= threshold).length;
      const aboveRatio = total > 0 ? above / total : 0;
      const oldest = total > 0 ? s.samples[0].t : now;
      const spannedMs = now - oldest;

      // Regime change: a confirmed trend broke its floor.
      if (s.phase === 'CONFIRMED' && price < brokenPrice) {
        s.phase = 'BROKEN';
        s.brokenAt = now;
        fire(tokenId, price);
        return;
      }

      // Confirmation requires a full-ish window and a high above-threshold ratio.
      if (s.phase !== 'CONFIRMED' && spannedMs >= windowMs * 0.9 && aboveRatio >= ratioNeeded) {
        s.phase = 'CONFIRMED';
        s.confirmedAt = now;
        return;
      }

      if (s.phase !== 'CONFIRMED' && s.phase !== 'BROKEN') {
        s.phase = aboveRatio > 0.5 ? 'BUILDING' : 'IDLE';
      }
    },

    resetIfNewRound(slot) {
      if (slot !== roundSlot) {
        roundSlot = slot;
        states.clear();
      }
    },

    get(tokenId) {
      const s = states.get(tokenId);
      if (!s) return null;
      const cfg = getConfig();
      const threshold = cfg.trendMinPrice ?? 0.55;
      const total = s.samples.length;
      const aboveSamples = s.samples.filter((x) => x.p >= threshold);
      const aboveRatio = total > 0 ? aboveSamples.length / total : 0;
      const aboveSec = aboveSamples.length > 0
        ? (s.samples[s.samples.length - 1].t - aboveSamples[0].t) / 1000
        : 0;
      const price = total > 0 ? s.samples[s.samples.length - 1].p : 0;
      return {
        phase: s.phase,
        price,
        aboveRatio,
        samples: total,
        aboveSec,
        confirmedAt: s.confirmedAt,
        brokenAt: s.brokenAt,
      };
    },

    isConfirmed(tokenId) {
      return states.get(tokenId)?.phase === 'CONFIRMED';
    },

    confirmedTokens() {
      const out = new Set<string>();
      for (const [id, s] of states) if (s.phase === 'CONFIRMED') out.add(id);
      return out;
    },

    onBroken(cb) {
      brokenCbs.push(cb);
    },

    reset() {
      states.clear();
    },
  };
}
