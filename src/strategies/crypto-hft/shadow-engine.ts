/**
 * Shadow Engine — Tesla-style counterfactual recorder (v2).
 *
 * For every opened position it records BOTH price paths for a FIXED window
 * (default 240s) from entry, independent of when the position closes:
 *   - the position's own token (what we actually did), and
 *   - the OPPOSITE token (to test the flipped / momentum hypothesis).
 *
 * Because sampling does not stop at exit, offline we can replay longer holds
 * and both directions over the exact path, and measure the regret vs reality.
 * Observation-only: never trades.
 *
 * Records are appended to data/shadow/positions.jsonl when the window closes.
 */

import { appendFileSync, existsSync, mkdirSync } from 'fs';
import { join } from 'path';
import { logger } from '../../utils/logger.js';
import type { OpenPosition, ClosedPosition } from './types.js';

const DATA_DIR = join(process.cwd(), 'data', 'shadow');
const FILE = join(DATA_DIR, 'positions.jsonl');
// Sample almost to the round's force-exit horizon instead of a flat 240s, so the
// replay can evaluate expiry/time-exit behaviour over the whole tradeable life.
const FALLBACK_WINDOW_MS = 780_000; // used only if expiresAt is missing
const FORCE_EXIT_BUFFER_MS = 120_000; // stop sampling at the force-exit deadline
const SAMPLE_MIN_INTERVAL_MS = 500;
const MAX_SAMPLES = 2000;

export interface ShadowContext {
  spotMove5?: number;
  spotMove30?: number;
  spotMove60?: number;
  trendAgeSec?: number;
  spreadPct?: number;
  timeLeftSec?: number;
  entryFactor?: number;
}

interface Sample { t: number; p: number; b?: number; a?: number }

interface Path {
  samples: Sample[];
  lastAt: number;
  first: number;
  max: number;
  min: number;
  maxAt: number;
  minAt: number;
}

interface Tracked {
  pos: OpenPosition;
  oppositeTokenId?: string;
  context?: ShadowContext;
  own: Path;
  opp: Path | null;
  actual?: ClosedPosition;
  windowEndAt: number;
}

export interface ShadowEngine {
  onOpen(pos: OpenPosition, context?: ShadowContext, oppositeTokenId?: string): void;
  onPrice(tokenId: string, price: number, now?: number, quote?: { bid?: number; ask?: number }): void;
  onClose(pos: ClosedPosition): void;
  /** Finalize records whose fixed window has elapsed. */
  tick(now?: number): void;
  /** Write snapshots for anything still open (on stop). */
  flush(): void;
  pendingCount(): number;
}

function ensureDir() {
  if (!existsSync(DATA_DIR)) mkdirSync(DATA_DIR, { recursive: true });
}

function newPath(first: number, t: number): Path {
  return { samples: [], lastAt: 0, first, max: first, min: first, maxAt: t, minAt: t };
}

function pushSample(path: Path, price: number, t: number, now: number, quote?: { bid?: number; ask?: number }) {
  if (path.first === 0) path.first = price;
  if (path.samples.length > 0 && now - path.lastAt < SAMPLE_MIN_INTERVAL_MS) return;
  path.lastAt = now;
  path.samples.push({ t, p: price, b: quote?.bid, a: quote?.ask });
  if (path.samples.length > MAX_SAMPLES) path.samples.shift();
  if (price > path.max) { path.max = price; path.maxAt = now; }
  if (price < path.min) { path.min = price; path.minAt = now; }
}

function pathStats(path: Path | null, base: number, enteredAt: number) {
  if (!path || path.samples.length === 0) return null;
  const rel = (p: number) => (base > 0 ? ((p - base) / base) * 100 : 0);
  return {
    entryRef: path.first,
    samples: path.samples.map((s) => ({ t: Math.round(s.t / 100) / 10, p: s.p, b: s.b, a: s.a })),
    maxPrice: path.max,
    minPrice: path.min,
    maxPct: rel(path.max),
    minPct: rel(path.min),
    timeToMaxSec: (path.maxAt - enteredAt) / 1000,
    timeToMinSec: (path.minAt - enteredAt) / 1000,
  };
}

export function createShadowEngine(): ShadowEngine {
  const tracked = new Map<string, Tracked>();

  function write(rec: Record<string, unknown>) {
    ensureDir();
    try {
      appendFileSync(FILE, JSON.stringify(rec) + '\n');
    } catch (err) {
      logger.warn({ err }, 'Shadow engine write failed');
    }
  }

  function finalize(t: Tracked) {
    const entry = t.pos.entryPrice;
    write({
      id: t.pos.id,
      strategy: t.pos.strategy,
      asset: t.pos.asset,
      direction: t.pos.direction,
      tokenId: t.pos.tokenId,
      conditionId: t.pos.conditionId,
      entryPrice: entry,
      shares: t.pos.shares,
      wasMakerEntry: t.pos.wasMakerEntry,
      enteredAt: t.pos.enteredAt,
      expiredAt: t.pos.expiresAt,
      windowSec: Math.round((t.windowEndAt - t.pos.enteredAt) / 100) / 10,
      context: t.context,
      own: pathStats(t.own, entry, t.pos.enteredAt),
      oppositeTokenId: t.oppositeTokenId,
      opposite: t.opp ? pathStats(t.opp, t.opp.first, t.pos.enteredAt) : null,
      closed: Boolean(t.actual),
      actual: t.actual
        ? {
            exitPrice: t.actual.exitPrice,
            exitReason: t.actual.exitReason,
            holdSec: t.actual.holdTimeSec,
            netPnlUsd: t.actual.netPnlUsd,
            netPnlPct: t.actual.netPnlPct,
            wasMakerExit: t.actual.wasMakerExit,
          }
        : null,
    });
  }

  return {
    onOpen(pos, context, oppositeTokenId) {
      // Sample through to the force-exit deadline of THIS round (fallback if the
      // engine did not supply an expiry), so expiry exits are replayable.
      const windowEndAt = pos.expiresAt > 0
        ? pos.expiresAt - FORCE_EXIT_BUFFER_MS
        : pos.enteredAt + FALLBACK_WINDOW_MS;
      tracked.set(pos.id, {
        pos,
        oppositeTokenId,
        context,
        own: newPath(pos.entryPrice, pos.enteredAt),
        opp: oppositeTokenId ? newPath(0, pos.enteredAt) : null,
        windowEndAt,
      });
    },

    onPrice(tokenId, price, now = Date.now(), quote) {
      if (!Number.isFinite(price) || price <= 0) return;
      for (const t of tracked.values()) {
        if (now < t.pos.enteredAt || now > t.windowEndAt) continue;
        const elapsed = now - t.pos.enteredAt;
        if (tokenId === t.pos.tokenId) pushSample(t.own, price, elapsed, now, quote);
        else if (t.oppositeTokenId && tokenId === t.oppositeTokenId && t.opp) pushSample(t.opp, price, elapsed, now, quote);
      }
    },

    onClose(pos) {
      const t = tracked.get(pos.id);
      if (t) t.actual = pos; // keep sampling until the window closes
    },

    tick(now = Date.now()) {
      if (tracked.size === 0) return;
      for (const [id, t] of tracked) {
        if (now >= t.windowEndAt) {
          finalize(t);
          tracked.delete(id);
        }
      }
    },

    flush() {
      for (const [id, t] of tracked) {
        finalize(t);
        tracked.delete(id);
      }
    },

    pendingCount() {
      return tracked.size;
    },
  };
}
