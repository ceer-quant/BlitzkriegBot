/**
 * Market Scanner — Round-based market discovery and rotation
 *
 * Tracks round slots (unix_ts / roundDurationSec) and automatically fetches new markets
 * when rounds transition. Enforces timing gates: min round age, min time left.
 *
 * Discovery method: Direct slug-based queries (e.g., btc-updown-5m-1770935700)
 * This ensures reliable discovery of time-duration-specific markets.
 */

import { logger } from '../../utils/logger.js';
import type { CryptoMarket, CryptoHftConfig, RoundState } from './types.js';

const GAMMA_URL = 'https://gamma-api.polymarket.com';

/** Raw Gamma API market shape */
interface GammaMarket {
  condition_id: string;
  conditionId: string;
  question_id: string;
  question: string;
  outcomes: string; // JSON string: '["Up", "Down"]'
  outcomePrices: string; // JSON string: '["0.5", "0.5"]'
  clobTokenIds: string; // JSON string: '["token1", "token2"]'
  end_date_iso: string;
  endDate: string;
  active: boolean;
  closed: boolean;
  neg_risk: boolean;
  volume: string;
  liquidity: string;
  slug: string;
}

export interface MarketScanner {
  /** Fetch fresh markets from Gamma. Call on round transitions. */
  refresh(): Promise<CryptoMarket[]>;
  /** Get current round state (slot, timing, markets) */
  getRound(): RoundState;
  /** Get market for a specific asset in current round */
  getMarket(asset: string): CryptoMarket | null;
  getClockOffset(): number;
  /** Check if we're in a tradeable window (not too early, not too late) */
  canTrade(): { ok: boolean; reason?: string };
  /** Start auto-refresh loop (checks for new rounds every 10s) */
  start(): void;
  stop(): void;
  /** Update live prices from WS/feed data */
  updatePrice(conditionId: string, upPrice: number, downPrice: number): void;
}

export function createMarketScanner(config: CryptoHftConfig | (() => CryptoHftConfig)): MarketScanner {
  const getConfig = typeof config === 'function' ? config : () => config;
  let markets: CryptoMarket[] = [];
  let currentSlot = 0;
  let refreshTimer: NodeJS.Timeout | null = null;
  let lastRefreshAt = 0;

  function getCurrentSlot(): number {
    return Math.floor(Date.now() / 1000 / getConfig().roundDurationSec);
  }

  // Track actual Polymarket end time
  let actualEndTime = 0; // ms timestamp from Polymarket
  let actualStartTime = 0; // ms timestamp from Polymarket
  let clockOffset = 0; // Polymarket time - local time (ms)

  function getSlotExpiry(slot: number): number {
    return (slot + 1) * getConfig().roundDurationSec * 1000;
  }

  function getRoundState(): RoundState {
    const cfg = getConfig();
    // Apply clock offset to get accurate Polymarket time
    const localNow = Date.now();
    const polymarketNow = localNow + clockOffset;
    const slot = Math.floor(polymarketNow / 1000 / cfg.roundDurationSec);
    let expiresAt = getSlotExpiry(slot);
    // If we have actual Polymarket end time, use it (most accurate)
    if (actualEndTime > 0) {
      const now = localNow;
      const actualTimeLeft = Math.max(0, (actualEndTime - now) / 1000);
      return {
        slot,
        expiresAt: actualEndTime,
        markets,
        ageSec: cfg.roundDurationSec - actualTimeLeft,
        timeLeftSec: actualTimeLeft,
      };
    }
    const now = Date.now();
    const timeLeftSec = Math.max(0, (expiresAt - now) / 1000);
    const ageSec = cfg.roundDurationSec - timeLeftSec;

    return {
      slot,
      expiresAt,
      markets,
      ageSec,
      timeLeftSec,
    };
  }

  async function fetchMarkets(): Promise<CryptoMarket[]> {
    const cfg = getConfig();
    const found: CryptoMarket[] = [];

    // Determine duration label — must match Polymarket slug patterns exactly
    const durationMap: Record<number, string> = {
      300: '5m',
      900: '15m',
      3600: '1h',
      14400: '4h',
      86400: 'daily',
    };
    const durationLabel = durationMap[cfg.roundDurationSec];
    if (!durationLabel) {
      logger.warn({ roundDurationSec: cfg.roundDurationSec }, 'Unsupported market duration');
      return found;
    }

    for (const asset of cfg.assets) {
      try {
        // Calculate current market slot and build slug
        // E.g., for 5-min: btc-updown-5m-1770935700
        // The timestamp is floored to the slot boundary: floor(now / 300) * 300
        const nowSec = Math.floor(Date.now() / 1000);
        const slotStart = Math.floor(nowSec / cfg.roundDurationSec) * cfg.roundDurationSec;
        const slug = `${asset.toLowerCase()}-updown-${durationLabel}-${slotStart}`;

        // Try direct slug query first (most reliable)
        const slugRes = await fetch(
          `${GAMMA_URL}/markets?slug=${encodeURIComponent(slug)}&active=true&closed=false`
        );

        if (slugRes.status === 429) {
          // Rate limited, skip this refresh
          logger.warn('Gamma API rate limited');
          continue;
        }
        if (slugRes.ok) {
          const slugData = (await slugRes.json()) as GammaMarket[];
          if (slugData.length > 0) {
            const m = slugData[0]; // slug should return exactly 1 result
            if (!m.closed && m.active) {
              // Capture actual Polymarket end time for accurate countdown
              const apiEndTime = new Date(m.endDate || m.end_date_iso).getTime();
              if (apiEndTime > 0) {
                actualEndTime = apiEndTime;
                // Calculate clock offset: how much our local clock is off from Polymarket
                // Local expected end = (slot+1) * duration
                // Polymarket actual end = apiEndTime
                // offset = actualEndTime - localExpectedEnd
                const localSlot = Math.floor(Date.now() / 1000 / cfg.roundDurationSec);
                const localExpectedEnd = (localSlot + 1) * cfg.roundDurationSec * 1000;
                clockOffset = apiEndTime - localExpectedEnd;
              }
              // Parse outcomes and clobTokenIds from Gamma API response
              let outcomes: string[] = [];
              let tokenIds: string[] = [];
              try {
                outcomes = JSON.parse(m.outcomes || '[]');
                tokenIds = JSON.parse(m.clobTokenIds || '[]');
              } catch { /* ignore parse errors */ }

              if (outcomes.length >= 2 && tokenIds.length >= 2) {
                const upIdx = outcomes.findIndex(o => o.toLowerCase() === 'up' || o.toLowerCase() === 'yes');
                const downIdx = outcomes.findIndex(o => o.toLowerCase() === 'down' || o.toLowerCase() === 'no');

                if (upIdx !== -1 && downIdx !== -1) {
                  const upTokenId = tokenIds[upIdx];
                  const downTokenId = tokenIds[downIdx];
                  const prices = JSON.parse(m.outcomePrices || '[]');
                  const upPrice = parseFloat(prices[upIdx]) || 0.5;
                  const downPrice = parseFloat(prices[downIdx]) || 0.5;
                  const expiresAt = new Date(m.endDate || m.end_date_iso).getTime();
                  const roundSlot = Math.floor(expiresAt / 1000 / cfg.roundDurationSec);
                  // Update actualEndTime from search results too
                  if (expiresAt > 0) actualEndTime = expiresAt;

                  found.push({
                    asset: asset.toUpperCase(),
                    conditionId: m.conditionId || m.condition_id,
                    questionId: m.question_id,
                    upTokenId,
                    downTokenId,
                    upPrice,
                    downPrice,
                    expiresAt,
                    roundSlot,
                    negRisk: m.neg_risk ?? true,
                    question: m.question,
                  });

                  logger.debug({ asset, slug, slot: roundSlot }, 'Found market by slug');
                  continue; // Successfully found, move to next asset
                }
              }
            }
          }
        }

        // Fallback: try generic search if slug query fails (between rounds, market not yet live)
        const searchQueries = [
          `${asset}-updown`,
        ];

        for (const query of searchQueries) {
          const searchRes = await fetch(
            `${GAMMA_URL}/markets?_limit=10&active=true&closed=false&_q=${encodeURIComponent(query)}`
          );
          if (!searchRes.ok) continue;

          const searchData = (await searchRes.json()) as GammaMarket[];

          for (const m of searchData) {
            if (m.closed || !m.active) continue;

            // Parse outcomes and token IDs from Gamma API
            let outcomes: string[] = [];
            let tokenIds: string[] = [];
            let prices: string[] = [];
            try {
              outcomes = JSON.parse(m.outcomes || '[]');
              tokenIds = JSON.parse(m.clobTokenIds || '[]');
              prices = JSON.parse(m.outcomePrices || '[]');
            } catch { continue; }

            if (outcomes.length < 2 || tokenIds.length < 2) continue;

            // Verify this is the right duration market
            const q = m.question.toLowerCase();
            if (!q.includes(asset.toLowerCase())) continue;

            // Filter by duration: must expire within roundDuration + buffer
            const expiresAt = new Date(m.end_date_iso).getTime();
            const now = Date.now();
            const secsLeft = (expiresAt - now) / 1000;

            if (secsLeft <= 0 || secsLeft > cfg.roundDurationSec + 60) continue;

            // Verify slug matches expected pattern
            if (!m.slug.includes(`-${durationLabel}-`)) continue;

            // Find UP/YES and DOWN/NO tokens
            const upIdx = outcomes.findIndex(o => o.toLowerCase() === 'yes' || o.toLowerCase() === 'up');
            const downIdx = outcomes.findIndex(o => o.toLowerCase() === 'no' || o.toLowerCase() === 'down');
            if (upIdx === -1 || downIdx === -1) continue;

            const upTokenId = tokenIds[upIdx];
            const downTokenId = tokenIds[downIdx];
            const upPrice = parseFloat(prices[upIdx]) || 0.5;
            const downPrice = parseFloat(prices[downIdx]) || 0.5;

            // Skip if already found a closer-expiry market for this asset
            const existing = found.find((f) => f.asset === asset.toUpperCase());
            if (existing && existing.expiresAt <= expiresAt) continue;
            if (existing) {
              const idx = found.indexOf(existing);
              found.splice(idx, 1);
            }

            const roundSlot = Math.floor(expiresAt / 1000 / cfg.roundDurationSec);

            found.push({
              asset: asset.toUpperCase(),
              conditionId: m.condition_id,
              questionId: m.question_id,
              upTokenId,
              downTokenId,
              upPrice,
              downPrice,
              expiresAt,
              roundSlot,
              negRisk: m.neg_risk ?? true,
              question: m.question,
            });

            logger.debug({ asset, slug: m.slug, slot: roundSlot }, 'Found market by search');
          }

          // Found one for this asset, stop trying queries
          if (found.some((f) => f.asset === asset.toUpperCase())) break;
        }
      } catch (err) {
        logger.warn(
          { err, asset, durationLabel, roundDurationSec: cfg.roundDurationSec },
          'Market scan failed for asset'
        );
        // Wait before next asset to avoid rate limiting
        await new Promise(resolve => setTimeout(resolve, 1000));
      }
    }

    return found;
  }

  async function maybeRefresh() {
    const slot = getCurrentSlot();

    // Only refresh on new round or if we have no markets
    if (slot !== currentSlot || markets.length === 0) {
      const prevSlot = currentSlot;
      currentSlot = slot;

      // Always refresh on new round, but rate limit if same slot
      if (slot === prevSlot) {
        if (Date.now() - lastRefreshAt < 60_000) return; // 30s between same-slot refreshes
      }
      lastRefreshAt = Date.now();

      markets = await fetchMarkets();

      if (markets.length > 0 && slot !== prevSlot) {
        logger.info(
          {
            slot,
            markets: markets.map((m) => `${m.asset}(${((m.expiresAt - Date.now()) / 1000).toFixed(0)}s)`),
          },
          'New round — markets loaded'
        );
      }
    }
  }

  return {
    async refresh() {
      lastRefreshAt = Date.now();
      currentSlot = getCurrentSlot();
      markets = await fetchMarkets();
      return markets;
    },

    getRound() {
      return getRoundState();
    },

    getMarket(asset) {
      return markets.find((m) => m.asset === asset.toUpperCase()) ?? null;
    },

    getClockOffset() {
      return clockOffset;
    },

    canTrade() {
      const cfg = getConfig();
      const round = getRoundState();

      if (round.markets.length === 0) {
        return { ok: false, reason: 'No active markets' };
      }
      if (round.ageSec < cfg.minRoundAgeSec) {
        return { ok: false, reason: `Round too young (${round.ageSec.toFixed(0)}s < ${cfg.minRoundAgeSec}s)` };
      }
      if (round.timeLeftSec < cfg.minTimeLeftSec) {
        return { ok: false, reason: `Too close to expiry (${round.timeLeftSec.toFixed(0)}s < ${cfg.minTimeLeftSec}s)` };
      }
      return { ok: true };
    },

    start() {
      // Check for new rounds every 5 seconds
      refreshTimer = setInterval(() => maybeRefresh(), 5_000);
      // Immediate first refresh
      maybeRefresh();
    },

    stop() {
      if (refreshTimer) {
        clearInterval(refreshTimer);
        refreshTimer = null;
      }
    },

    updatePrice(conditionId, upPrice, downPrice) {
      const m = markets.find((mk) => mk.conditionId === conditionId);
      if (m) {
        m.upPrice = upPrice;
        m.downPrice = downPrice;
      }
    },
  };
}
