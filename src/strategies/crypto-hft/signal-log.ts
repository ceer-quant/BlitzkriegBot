/**
 * Signal Log (shadow / observation only)
 *
 * Records every spread_arb signal and its forward price path so the strategy's
 * edge can be measured empirically — without placing any orders.
 *
 * For each signal we capture the entry context and then sample the token price
 * at +5s/+15s/+30s/+60s plus the max/min over a tracking window. Records are
 * appended to data/signals/signals.jsonl once the window closes.
 */

import { appendFileSync, existsSync, mkdirSync } from 'fs';
import { join } from 'path';
import { logger } from '../../utils/logger.js';

const DATA_DIR = join(process.cwd(), 'data', 'signals');
const SIGNALS_FILE = join(DATA_DIR, 'signals.jsonl');

/** Tracking window for max/min after a signal. */
const TRACK_WINDOW_MS = 180_000;
const SAMPLE_HORIZONS_MS = [5_000, 15_000, 30_000, 60_000] as const;

export interface SignalInput {
  strategy: string;
  asset: string;
  direction: 'up' | 'down';
  tokenId: string;
  conditionId: string;
  /** Limit / fair price the strategy wanted */
  signalPrice: number;
  /** Current token price at signal time */
  marketPrice: number;
  entryFactor: number;
  highAgeMs: number;
  spotMove5: number;
  spotMove30: number;
  spotMove60: number;
  spotPrice: number;
  timeLeftSec: number;
  /** How long the token had held above the trend threshold at signal time */
  trendAgeSec?: number;
  spreadPct?: number;
}

interface PendingSignal extends SignalInput {
  id: string;
  ts: number;
  samples: Record<number, number | undefined>;
  maxPrice: number;
  minPrice: number;
  /** First time the price touched/undershot the resting bid (i.e. would fill) */
  touchedAt?: number;
  fillPrice?: number;
  /** Best / worst price AFTER the (hypothetical) fill */
  postMax?: number;
  postMin?: number;
  postMaxAt?: number;
  postMinAt?: number;
}

export interface SignalLog {
  /** Register a new signal to be tracked. */
  record(input: SignalInput): string;
  /** Feed a token price update (per tokenId). */
  onPrice(tokenId: string, price: number, now?: number): void;
  /** Finalize any signals whose tracking window has elapsed. */
  tick(now?: number): void;
  /** Flush all pending signals (on stop) so nothing is lost. */
  flush(): void;
  /** Number of signals currently being tracked. */
  pendingCount(): number;
}

function ensureDir() {
  if (!existsSync(DATA_DIR)) mkdirSync(DATA_DIR, { recursive: true });
}

let seq = 0;

export function createSignalLog(): SignalLog {
  const pending = new Map<string, PendingSignal>();

  function finalize(rec: PendingSignal, now: number, closed: boolean) {
    const pct = (p: number | undefined) =>
      p !== undefined && rec.marketPrice > 0
        ? ((p - rec.marketPrice) / rec.marketPrice) * 100
        : undefined;
    // Percentage relative to our resting bid (the price we'd actually be filled at)
    const relPct = (p: number | undefined) =>
      p !== undefined && rec.signalPrice > 0
        ? ((p - rec.signalPrice) / rec.signalPrice) * 100
        : undefined;
    const samplesOut: Record<string, number | undefined> = {};
    const pctOut: Record<string, number | undefined> = {};
    for (const h of SAMPLE_HORIZONS_MS) {
      samplesOut[`t${h / 1000}s`] = rec.samples[h];
      pctOut[`pct${h / 1000}s`] = pct(rec.samples[h]);
    }
    const record = {
      id: rec.id,
      ts: rec.ts,
      time: new Date(rec.ts).toISOString(),
      strategy: rec.strategy,
      asset: rec.asset,
      direction: rec.direction,
      tokenId: rec.tokenId,
      conditionId: rec.conditionId,
      signalPrice: rec.signalPrice,
      marketPrice: rec.marketPrice,
      entryFactor: rec.entryFactor,
      trendAgeSec: rec.trendAgeSec,
      spotMove5: rec.spotMove5,
      spotMove30: rec.spotMove30,
      spotMove60: rec.spotMove60,
      spotPrice: rec.spotPrice,
      timeLeftSec: rec.timeLeftSec,
      spreadPct: rec.spreadPct,
      samples: samplesOut,
      pct: pctOut,
      maxPrice: rec.maxPrice,
      minPrice: rec.minPrice,
      maxPct: pct(rec.maxPrice),
      minPct: pct(rec.minPrice),
      // Fill-aware outcome: did price reach our resting bid, and what happened next?
      touched: rec.touchedAt !== undefined,
      touchDelaySec: rec.touchedAt ? (rec.touchedAt - rec.ts) / 1000 : undefined,
      fillPrice: rec.fillPrice,
      postMaxPrice: rec.postMax,
      postMinPrice: rec.postMin,
      postMaxPct: relPct(rec.postMax),
      postMinPct: relPct(rec.postMin),
      timeToPostMaxSec: rec.postMaxAt && rec.touchedAt ? (rec.postMaxAt - rec.touchedAt) / 1000 : undefined,
      timeToPostMinSec: rec.postMinAt && rec.touchedAt ? (rec.postMinAt - rec.touchedAt) / 1000 : undefined,
      windowClosed: closed,
      finalizedAt: now,
    };
    ensureDir();
    try {
      appendFileSync(SIGNALS_FILE, JSON.stringify(record) + '\n');
    } catch (err) {
      logger.warn({ err }, 'Failed to write signal record');
    }
  }

  return {
    record(input) {
      const id = `sig-${Date.now().toString(36)}-${seq++}`;
      const rec: PendingSignal = {
        ...input,
        id,
        ts: Date.now(),
        samples: {},
        maxPrice: input.marketPrice,
        minPrice: input.marketPrice,
      };
      pending.set(id, rec);
      logger.info(
        {
          id,
          asset: input.asset,
          direction: input.direction,
          signalPrice: input.signalPrice,
          marketPrice: input.marketPrice,
          highAgeSec: (input.highAgeMs / 1000).toFixed(0),
          spotMove30: input.spotMove30.toFixed(3),
          timeLeftSec: input.timeLeftSec.toFixed(0),
        },
        'Signal logged (shadow)'
      );
      return id;
    },

    onPrice(tokenId, price, now = Date.now()) {
      if (!Number.isFinite(price) || price <= 0) return;
      for (const rec of pending.values()) {
        if (rec.tokenId !== tokenId) continue;
        const age = now - rec.ts;
        if (age < 0 || age > TRACK_WINDOW_MS) continue;
        if (price > rec.maxPrice) rec.maxPrice = price;
        if (price < rec.minPrice) rec.minPrice = price;
        // Fill detection: price reaching our resting bid fills the order.
        if (rec.touchedAt === undefined && price <= rec.signalPrice) {
          rec.touchedAt = now;
          rec.fillPrice = rec.signalPrice;
          rec.postMax = price;
          rec.postMin = price;
          rec.postMaxAt = now;
          rec.postMinAt = now;
        } else if (rec.touchedAt !== undefined) {
          if (rec.postMax === undefined || price > rec.postMax) { rec.postMax = price; rec.postMaxAt = now; }
          if (rec.postMin === undefined || price < rec.postMin) { rec.postMin = price; rec.postMinAt = now; }
        }
        for (const h of SAMPLE_HORIZONS_MS) {
          if (rec.samples[h] === undefined && age >= h) rec.samples[h] = price;
        }
      }
    },

    tick(now = Date.now()) {
      if (pending.size === 0) return;
      for (const [id, rec] of pending) {
        if (now - rec.ts >= TRACK_WINDOW_MS) {
          finalize(rec, now, true);
          pending.delete(id);
        }
      }
    },

    flush() {
      const now = Date.now();
      for (const [id, rec] of pending) {
        finalize(rec, now, false);
        pending.delete(id);
      }
    },

    pendingCount() {
      return pending.size;
    },
  };
}
