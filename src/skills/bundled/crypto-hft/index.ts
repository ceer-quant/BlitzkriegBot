/**
 * Crypto HFT Skill — Chat commands for 15-minute crypto market trading
 *
 * Commands:
 *   /crypto-hft start [assets] [--size N] [--dry-run] [--preset NAME]
 *   /crypto-hft stop
 *   /crypto-hft status
 *   /crypto-hft positions
 *   /crypto-hft markets
 *   /crypto-hft config --tp 15 --sl 12 --size 20
 *   /crypto-hft enable <strategy>
 *   /crypto-hft disable <strategy>
 *   /crypto-hft preset list
 *   /crypto-hft preset save <name>
 *   /crypto-hft preset load <name>
 *   /crypto-hft preset delete <name>
 *   /crypto-hft round
 */

import {
  createCryptoHftEngine,
  DEFAULT_CONFIG,
  type CryptoHftEngine,
} from '../../../strategies/crypto-hft/index.js';
import { createMarketScanner } from '../../../strategies/crypto-hft/market-scanner.js';
import { savePreset, loadPreset, deletePreset, listPresets } from '../../../strategies/crypto-hft/presets.js';
import { getRecentTrades, getAllTrades } from '../../../strategies/crypto-hft/trade-db.js';
import { createLocalOrderBookManager, type LocalOrderBookManager } from '../../../strategies/crypto-hft/local-orderbook.js';
import { initPrecomputedData, cacheMarketMetadata, type FastOrderParams } from '../../../strategies/crypto-hft/presign-pool.js';
import type { CryptoFeed } from '../../../feeds/crypto/index.js';
import type { ExecutionService } from '../../../execution/index.js';
import { formatHelp } from '../../help.js';
import { wrapSkillError } from '../../errors.js';
import { logger } from '../../../utils/logger.js';
import { blitzkriegCoreEnabled, getBlitzkriegCoreRunner } from '../../../core/blitzkrieg-core-runner.js';

// ── Lazy service instances ──────────────────────────────────────────────────

let feedInstance: CryptoFeed | null = null;
let polyFeedInstance: any | null = null;
let polySubUnsubs: Array<() => void> = [];
let execInstance: ExecutionService | null = null;
let engine: CryptoHftEngine | null = null;
let localBook: LocalOrderBookManager | null = null;

async function getFeed(): Promise<CryptoFeed | null> {
  if (feedInstance) return feedInstance;
  try {
    const { createCryptoFeed } = await import('../../../feeds/crypto/index.js');
    feedInstance = createCryptoFeed();
    feedInstance.start();
    return feedInstance;
  } catch {
    return null;
  }
}

async function getExecution(): Promise<ExecutionService | null> {
  if (execInstance) return execInstance;
  try {
    const privateKey = process.env.POLYMARKET_PRIVATE_KEY || process.env.PRIVATE_KEY;
    if (!privateKey) return null;

    const apiKey = process.env.POLYMARKET_API_KEY || '';
    const apiSecret = process.env.POLYMARKET_API_SECRET || '';
    const apiPassphrase = process.env.POLYMARKET_API_PASSPHRASE || '';
    // L2 POLY_ADDRESS must be the signer EOA (owns the API key),
    // funder is the proxy wallet holding funds (Magic Link accounts)
    const { deriveAddress } = await import('../../../utils/polymarket-setup.js');
    const address = deriveAddress(privateKey);

    const { createExecutionService } = await import('../../../execution/index.js');
    execInstance = createExecutionService({
      polymarket: {
        privateKey,
        address,
        funderAddress: process.env.POLYMARKET_FUNDER_ADDRESS,
        signatureType: 3,
        apiKey,
        apiSecret,
        apiPassphrase,
      },
      dryRun: process.env.DRY_RUN === 'true',
    });
    return execInstance;
  } catch {
    return null;
  }
}

async function getPolyFeed(): Promise<any | null> {
  if (polyFeedInstance) return polyFeedInstance;
  try {
    const { createPolymarketFeed } = await import('../../../feeds/polymarket/index.js');
    polyFeedInstance = await createPolymarketFeed();
    await polyFeedInstance.start();
    return polyFeedInstance;
  } catch {
    return null;
  }
}

let polyWsConnected = false;

function wirePolyWsToEngine(polyFeed: any, eng: CryptoHftEngine, localBook?: LocalOrderBookManager) {
  const latestPrices = new Map<string, number>();

  // Listen for price updates from WS
  polyFeed.on('price', (update: any) => {
    if (update.outcomeId && Number.isFinite(update.price) && update.price > 0) {
      latestPrices.set(update.outcomeId, update.price);
      // Update the market prices
      for (const market of eng.getMarkets()) {
        if (market.upTokenId === update.outcomeId || market.downTokenId === update.outcomeId) {
          const isUp = market.upTokenId === update.outcomeId;
          if (isUp) {
            eng.updatePrices(
              market.conditionId,
              update.price,
              latestPrices.get(market.downTokenId) ?? market.downPrice,
            );
          } else {
            eng.updatePrices(
              market.conditionId,
              latestPrices.get(market.upTokenId) ?? market.upPrice,
              update.price,
            );
          }
        }
      }
    }
  });

  // Listen for orderbook updates from WS
  polyFeed.on('orderbook', (update: any) => {
    if (update.outcomeId) {
      const bids: Array<[number, number]> = (update.bids || []).map((b: any) => [b[0] || b.price, b[1] || b.size]);
      const asks: Array<[number, number]> = (update.asks || []).map((a: any) => [a[0] || a.price, a[1] || a.size]);
      // Feed engine's book tracker for strategy evaluation
      eng.onOrderbook(update.outcomeId, bids, asks);
      // Feed local order book for fast reads
      if (localBook) {
        localBook.applySnapshot(update.outcomeId, bids, asks);
      }
    }
  });

  // Re-subscribe on new rounds. We re-assert the subscription every tick (not
  // only on slot change) so that a WS reconnect — which loses the feed's
  // resubscribe table — always has token IDs to restore. A stable noop callback
  // lets the feed's callback Set dedupe, so this does not leak.
  let lastSubscribedSlot = -1;
  const noopSubscribe = () => {};
  const resubscribe = () => {
    const markets = eng.getMarkets();
    if (markets.length === 0) return;

    for (const market of markets) {
      for (const tokenId of [market.upTokenId, market.downTokenId]) {
        if (!tokenId) continue;
        polyFeed.subscribePrice?.('polymarket', tokenId, noopSubscribe);
      }
    }

    // Check if we already subscribed to this round's markets
    const currentSlot = markets[0]?.roundSlot ?? -1;
    if (currentSlot !== lastSubscribedSlot) {
      lastSubscribedSlot = currentSlot;
      logger.info({ slot: currentSlot, markets: markets.length }, 'Poly WS subscribed to new round');
    }
  };
  resubscribe();
  // Re-subscribe every 10s to catch new rounds fast
  const resubInterval = setInterval(resubscribe, 10_000);
  polyWsUnsubscribes.push(() => clearInterval(resubInterval));
}

let polyWsUnsubscribes: Array<() => void> = [];

function stopPolyWs() {
  for (const unsub of polyWsUnsubscribes) { try { unsub(); } catch {} }
  polyWsUnsubscribes = [];
}

// Price polling via CLOB REST /book — runs every 2s using token IDs from engine markets
let restFallbackInterval: ReturnType<typeof setInterval> | null = null;

async function restFallback(eng: CryptoHftEngine) {
  const CLOB = 'https://clob.polymarket.com';
  const now = Date.now();
  const markets = eng.getMarkets();

  // Skip if no markets or markets are expired
  if (markets.length === 0) return;

  for (const market of markets) {
    // Skip expired markets
    if (market.expiresAt <= now) continue;

    for (const [dir, tokenId] of [['up', market.upTokenId], ['down', market.downTokenId]] as const) {
      if (!tokenId) continue;
      try {
        const res = await fetch(`${CLOB}/book?token_id=${tokenId}`, { signal: AbortSignal.timeout(2000) });
        if (!res.ok) continue;
        const data = await res.json() as any;
        const bids = data.bids || [];
        const asks = data.asks || [];
        let bestBid = 0, bestAsk = 0;
        if (bids.length > 0) bestBid = parseFloat(bids[0].price) || 0;
        if (asks.length > 0) bestAsk = parseFloat(asks[0].price) || 0;
        if (bestBid <= 0 && bestAsk <= 0) continue;
        let mid = 0;
        if (bestBid > 0 && bestAsk > 0) mid = (bestBid + bestAsk) / 2;
        else if (bestBid > 0) mid = bestBid;
        else mid = bestAsk;
        if (mid <= 0 || !isFinite(mid)) continue;
        if (dir === 'up') eng.updatePrices(market.conditionId, mid, 1 - mid);
        else eng.updatePrices(market.conditionId, 1 - mid, mid);
      } catch {}
    }
  }
}

function startPricePolling(eng: CryptoHftEngine) {
  stopPricePolling();
  // REST polling disabled - use WebSocket for price updates
  // This avoids consuming CLOB API rate limit for order submissions
  logger.info('Price polling disabled - using WebSocket for price updates');
}

function stopPricePolling() {
  if (restFallbackInterval) { clearInterval(restFallbackInterval); restFallbackInterval = null; }
}

// ── Formatters ──────────────────────────────────────────────────────────────

function fmtUsd(n: number): string { return (n >= 0 ? '+' : '-') + '$' + Math.abs(n).toFixed(2); }
function fmtPct(n: number): string { return (n >= 0 ? '+' : '-') + Math.abs(n).toFixed(1) + '%'; }

/**
 * Aggregate closed-trade records into the cumulative stats the panel's cards
 * read. Shared by the Node engine (persisted JSONL) and the Rust core (its
 * `trades.history`, same record shape) so both report identical numbers.
 */
function aggregateTrades(trades: any[]) {
  if (trades.length === 0) return null;
  const wins = trades.filter((t) => t.netPnlUsd > 0).length;
  const losses = trades.length - wins;
  const grossPnlUsd = trades.reduce((a, t) => a + (t.grossPnlUsd || 0), 0);
  const feesUsd = trades.reduce((a, t) => a + (t.feesUsd || 0), 0);
  const netPnlUsd = trades.reduce((a, t) => a + (t.netPnlUsd || 0), 0);
  const dayStart = new Date();
  dayStart.setHours(0, 0, 0, 0);
  const dailyPnlUsd = trades
    .filter((t) => (t.exitTime || 0) >= dayStart.getTime())
    .reduce((a, t) => a + (t.netPnlUsd || 0), 0);
  const exitReasons: Record<string, number> = {};
  for (const t of trades) exitReasons[t.exitReason] = (exitReasons[t.exitReason] || 0) + 1;
  const volumeUsd = trades.reduce((a, t) => a + (t.costUsd || 0), 0);
  const totalShares = trades.reduce((a, t) => a + (t.shares || 0), 0);
  return {
    totalTrades: trades.length,
    wins,
    losses,
    winRate: (wins / trades.length) * 100,
    grossPnlUsd,
    feesUsd,
    netPnlUsd,
    dailyPnlUsd,
    bestTradePct: Math.max(...trades.map((t) => t.netPnlPct)),
    worstTradePct: Math.min(...trades.map((t) => t.netPnlPct)),
    makerEntryRate: (trades.filter((t) => t.wasMakerEntry).length / trades.length) * 100,
    makerExitRate: (trades.filter((t) => t.wasMakerExit).length / trades.length) * 100,
    volumeUsd,
    avgPrice: totalShares > 0 ? volumeUsd / totalShares : 0,
    exitReasons,
  };
}

/**
 * Cumulative stats computed from the persisted trades file, so PnL survives a
 * restart (the in-memory position manager starts empty every run).
 */
function computePersistedStats() {
  return aggregateTrades(getRecentTrades(100000));
}

// ── Command Handler ─────────────────────────────────────────────────────────

/**
 * Rust-core command path (HFT_CORE=rust). Node only starts/stops the Rust
 * process and formats its status; the core discovers rounds, pulls market data,
 * decides and trades on its own.
 */
async function executeRust(cmd: string, args: string, parts: string[]): Promise<string> {
  const runner = getBlitzkriegCoreRunner();
  try {
    switch (cmd) {
      case 'start': {
        if (runner.isRunning()) return 'Rust core already running. `/crypto-hft stop` first.';
        const assetArg = parts[1] && !parts[1].startsWith('-') ? parts[1] : null;
        const assets = assetArg
          ? assetArg.toUpperCase().split(',')
          : (process.env.HFT_ASSETS ? process.env.HFT_ASSETS.split(',') : DEFAULT_CONFIG.assets);
        const dryRun = args.includes('--dry-run') || args.includes('--dry') || process.env.DRY_RUN !== 'false';
        const sizeMatch = args.match(/--size\s+(\d+(?:\.\d+)?)/);
        const sizeUsd = sizeMatch ? parseFloat(sizeMatch[1]) : DEFAULT_CONFIG.sizeUsd;
        const roundSec = parseInt(process.env.HFT_ROUND_SEC || String(DEFAULT_CONFIG.roundDurationSec), 10);
        // Per-order lot bounds. Env override lets ops right-size the lot for a
        // small live balance (e.g. 4.8u → HFT_MAX_SHARES=4) without a rebuild.
        const minShares = parseInt(process.env.HFT_MIN_SHARES || String(DEFAULT_CONFIG.minShares), 10);
        const maxShares = parseInt(process.env.HFT_MAX_SHARES || String(DEFAULT_CONFIG.maxShares), 10);
        // Optional per-strategy entry caps (P-1.1), comma-separated
        // `name:maxOpen:maxNotional` (`-`/empty segment = uncapped). Ops-only knob:
        // unset → no flag → per-strategy behaviour unchanged.
        const strategyLimits = (process.env.HFT_STRATEGY_LIMITS || '')
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean);
        await runner.start({
          assets,
          roundSec,
          dryRun,
          sizeUsd,
          minShares,
          maxShares,
          maxPositions: DEFAULT_CONFIG.maxPositions,
          maxDailyLossUsd: DEFAULT_CONFIG.maxDailyLossUsd,
          minRoundAgeSec: DEFAULT_CONFIG.minRoundAgeSec,
          minTimeLeftSec: DEFAULT_CONFIG.minTimeLeftSec,
          strategyLimits,
        });
        return [
          `**Crypto HFT Started (Rust core) [${dryRun ? 'DRY RUN' : 'LIVE'}]**`,
          `Assets: ${assets.join(', ')}`,
          `Round: ${roundSec}s | Size: $${sizeUsd}/trade | Lot: ${minShares}–${maxShares} sh`,
          ...(strategyLimits.length ? [`Strategy caps: ${strategyLimits.join(' | ')}`] : []),
          `Engine: blitzkrieg-core (Rust owns discovery, market data, signals, risk, orders, positions)`,
          `Node role: UI / parameters / logs only`,
        ].join('\n');
      }
      case 'stop': {
        if (!runner.isRunning()) return 'Rust core not running.';
        const st = await runner.status().catch(() => null);
        await runner.stop();
        const pnl = st ? `$${st.stats.dailyPnlUsd.toFixed(2)}` : '$0.00';
        return `Rust core stopped. entries=${st?.stats.entries ?? 0}, fills=${st?.stats.fills ?? 0}, net=${pnl}, W/L=${st?.stats.wins ?? 0}/${st?.stats.losses ?? 0}`;
      }
      case 'status': {
        if (!runner.isRunning()) return 'Not running. `/crypto-hft start` (Rust core mode).';
        const st = await runner.status();
        if (!st) return 'Rust core status unavailable.';
        const err = runner.lastErrorMessage();
        // Cumulative stats come from the CORE's persisted trade ledger, not the
        // runner's in-memory counters (which reset on every Node/core restart and
        // only see closures that happened during this process). Limit 0 = all.
        // Without this the panel's 盈亏/胜率/交易笔数/今日/交易量 cards read 0.
        let agg: ReturnType<typeof aggregateTrades> = null;
        try {
          const client = runner.getClient();
          if (client) agg = aggregateTrades((await client.tradesHistory(0)).trades ?? []);
        } catch { /* fall back to in-memory below */ }
        const total = agg ? agg.totalTrades : st.stats.wins + st.stats.losses;
        const wins = agg ? agg.wins : st.stats.wins;
        const losses = agg ? agg.losses : st.stats.losses;
        const wr = total > 0 ? Math.round((wins / total) * 100) : 0;
        const gross = agg ? agg.grossPnlUsd : st.stats.dailyPnlUsd;
        const fees = agg ? agg.feesUsd : 0;
        const net = agg ? agg.netPnlUsd : st.stats.dailyPnlUsd;
        const today = agg ? agg.dailyPnlUsd : st.stats.dailyPnlUsd;
        const open = st.stats.openPositions;
        let out = `**Crypto HFT Status (Rust core)**\n`;
        out += `Round: #${st.roundSlot} | ${st.ageSec.toFixed(0)}s old | ${st.timeLeftSec.toFixed(0)}s left | ${st.canTrade ? 'TRADING' : 'WAITING'}\n`;
        out += `Breaker: OK | consecutiveLosses: 0\n`;
        out += `Markets: ${st.markets} | Open: ${open} | PendingEntries: ${st.stats.entries - st.stats.fills > 0 ? st.stats.entries - st.stats.fills : 0}\n`;
        out += `Trades: ${total} (${wins}W/${losses}L) ${wr}% WR\n`;
        out += `Gross: ${fmtUsd(gross)} | Fees: $${fees.toFixed(2)} | Net: ${fmtUsd(net)}\n`;
        out += `Today: ${fmtUsd(today)} | Best: ${agg ? fmtPct(agg.bestTradePct) : '0%'} | Worst: ${agg ? fmtPct(agg.worstTradePct) : '0%'}\n`;
        out += `Clock offset: 0.00s\n`;
        out += `Volume: $${(agg?.volumeUsd ?? 0).toFixed(2)} | Avg: $${(agg?.avgPrice ?? 0).toFixed(4)}\n`;
        out += `Feed: books=${st.stats.books} spots=${st.stats.spots} confirmed=${st.stats.confirmed} signals=${st.stats.signals}\n`;
        out += `Blocked (near-miss): timing=${st.stats.blockedTiming} momentum=${st.stats.blockedMomentum} | connected=${st.connected ? 'yes' : 'NO'}\n`;
        if (err) out += `Last error: ${err}\n`;
        if (st.marketPrices.length > 0) {
          out += `\n**Prices:**\n`;
          for (const m of st.marketPrices) {
            out += `  ${m.asset}: UP=${m.up.toFixed(2)} DOWN=${m.down.toFixed(2)} (spread=${((m.up + m.down - 1) * 100).toFixed(1)}c)\n`;
          }
        }
        const pos = await runner.positions();
        if (pos.length > 0) {
          out += `\n**Open Positions:**\n`;
          for (const p of pos) {
            out += `  ${p.asset} ${p.direction.toUpperCase()} @ ${p.entryPrice.toFixed(2)} -> ${p.currentPrice.toFixed(2)} (${p.unrealizedPct >= 0 ? '+' : ''}${p.unrealizedPct.toFixed(1)}%) [${p.strategy}] ${p.timeLeftSec.toFixed(0)}s left\n`;
          }
        }
        return out;
      }
      case 'positions': {
        if (!runner.isRunning()) return 'Not running.';
        const client = runner.getClient();
        if (!client) return 'Rust core client unavailable.';
        // The panel's 历史订单 tab parses this exact format (see ui/hft.html).
        // Emit the SAME line shape the Node engine produced so the table fills.
        // Default (no arg) = ALL trades, matching the Node engine (limit 0 = all).
        const limitArg = parts[1] ? parseInt(parts[1], 10) : NaN;
        const limit = Number.isFinite(limitArg) && limitArg > 0 ? limitArg : 0;
        const { trades } = await client.tradesHistory(limit);
        if (!trades || trades.length === 0) return 'No closed trades yet.';
        const list = limit > 0 ? trades.slice(-limit) : trades;
        const fmtPct = (n: number) => (n >= 0 ? '+' : '-') + Math.abs(n).toFixed(1) + '%';
        const fmtUsd = (n: number) => (n >= 0 ? '+' : '-') + '$' + Math.abs(n).toFixed(2);
        const fmtTime = (v: any): string => {
          const n = typeof v === 'string' ? Date.parse(v) : Number(v);
          if (!n) return '--:--:--';
          return new Date(n).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
        };
        let out = `**Last ${list.length} Trades:**\n`;
        // Newest first, matching the Node engine's output.
        for (const t of [...list].reverse()) {
          const priceChange = Number(t.exitPrice) - Number(t.entryPrice);
          out += `  ${t.asset} ${String(t.direction).toUpperCase()} ${fmtPct(Number(t.netPnlPct))} (${fmtUsd(Number(t.netPnlUsd))}) [${t.strategy}] ` +
            `${fmtTime(t.entryTime)}->${fmtTime(t.exitTime)} $${Number(t.entryPrice).toFixed(2)}->$${Number(t.exitPrice).toFixed(2)} ` +
            `(${priceChange >= 0 ? '+' : ''}${priceChange.toFixed(3)}) ${Number(t.holdTimeSec).toFixed(0)}s\n`;
        }
        return out;
      }
      case 'shadow-evolution':
      case 'shadow_evolution': {
        if (!runner.isRunning()) return 'Not running.';
        const sub = (parts[1] || 'status').toLowerCase();
        const client = runner.getClient();
        if (!client) return 'Rust core client unavailable.';
        if (sub === 'enable') { await client.shadowEvolutionEnable(); return 'Shadow Evolution **enabled** (opt-in).'; }
        if (sub === 'disable') { await client.shadowEvolutionDisable(); return 'Shadow Evolution **disabled**.'; }
        if (sub === 'rollback') {
          try { await client.shadowEvolutionRollback(); return 'Rolled back to previous parameters.'; }
          catch (e: any) { return `Rollback failed: ${e?.message || e}`; }
        }
        if (sub === 'history') {
          const h = await client.shadowEvolutionHistory(20);
          if (!h.history.length) return 'Shadow Evolution history: (empty)';
          return ['**Shadow Evolution history:**', ...h.history.map((r: any) =>
            `- ${new Date(r.timestamp).toISOString().slice(0,19)} ${r.applied ? 'APPLIED' : 'REJECTED'} ${r.reason} conf=${r.confidence} n=${r.sampleCount}${r.rejection ? ' ('+r.rejection+')' : ''}`)].join('\n');
        }
        const st = await client.shadowEvolutionStatus();
        const p = st.currentParams || {};
        return [
          `**Shadow Evolution status: ${st.status}**`,
          `Variants: ${st.variantCount} | Applied: ${st.evolutionsApplied} | Rejected: ${st.evolutionsRejected} | Since last: ${st.secondsSinceLastEvolution}s`,
          `Live params: minPrice=${p.trendMinPrice} entryFactor=${p.trendEntryFactor} maxEntry=${p.trendMaxEntryPrice} broken=${p.trendBrokenPrice}`,
          ...(st.variants || []).map((v: any) => `- ${v.label} n=${v.sampleCount} WR=${(Number(v.winRate)*100).toFixed(0)}% PF=${Number(v.profitFactor).toFixed(2)} pnl=$${Number(v.totalPnlUsd).toFixed(2)}`),
        ].join('\n');
      }
      case 'help':
      default:
        return [
          '**Crypto HFT — Rust core mode (HFT_CORE=rust)**',
          'The trading engine runs in the Rust core; Node is UI/params/logs only.',
          'Commands:',
          '  /crypto-hft start [ASSETS] [--size N] [--dry-run]',
          '  /crypto-hft stop',
          '  /crypto-hft status',
          '  /crypto-hft positions',
          'Strategy/order/risk parameters are owned by the Rust core.',
        ].join('\n');
    }
  } catch (e: any) {
    return wrapSkillError('crypto-hft (rust)', cmd, e);
  }
}

async function execute(args: string): Promise<string> {
  const parts = args.trim().split(/\s+/);
  const cmd = parts[0]?.toLowerCase() || 'help';

  // ── Rust core mode ─────────────────────────────────────────────────────────
  // HFT_CORE=rust routes the trading engine to the Rust core: Node starts/stops
  // the process and renders status; ALL trading logic runs in Rust.
  if (blitzkriegCoreEnabled()) {
    return executeRust(cmd, args, parts);
  }

  try {
  switch (cmd) {
    case 'start': {
      if (engine) return 'Already running. `/crypto-hft stop` first.';

      const feed = await getFeed();
      if (!feed) return 'Crypto feed not available. Check that Binance WS is reachable.';
      const exec = await getExecution();

      // Check for preset
      const presetIdx = args.indexOf('--preset');
      let presetConfig: Record<string, any> = {};
      let presetStrategies: Record<string, boolean> | null = null;

      if (presetIdx !== -1) {
        const presetName = args.slice(presetIdx + 9).trim().split(/\s+/)[0];
        const preset = loadPreset(presetName);
        if (!preset) return `Preset "${presetName}" not found. Use \`/crypto-hft preset list\`.`;
        presetConfig = preset.config;
        presetStrategies = preset.strategies;
      }

      // Parse inline flags (override preset)
      const assetArg = parts[1] && !parts[1].startsWith('-') ? parts[1] : null;
      const assets = assetArg ? assetArg.toUpperCase().split(',') : (presetConfig.assets ?? DEFAULT_CONFIG.assets);
      const dryRun = args.includes('--dry-run') || args.includes('--dry') || (presetConfig.dryRun ?? (process.env.DRY_RUN === 'true'));
      const sizeMatch = args.match(/--size\s+(\d+)/);
      const rawSize = sizeMatch ? parseInt(sizeMatch[1], 10) : NaN;
      const sizeUsd = !isNaN(rawSize) && rawSize > 0 ? rawSize : (presetConfig.sizeUsd ?? DEFAULT_CONFIG.sizeUsd);

      engine = createCryptoHftEngine(feed, exec, {
        ...presetConfig,
        assets,
        sizeUsd,
        dryRun,
      });

      if (presetStrategies) {
        for (const [name, val] of Object.entries(presetStrategies)) {
          engine.setStrategyEnabled(name, val);
        }
      }

      await engine.start();

      // Initialize local order book
      localBook = createLocalOrderBookManager();

      // Initialize pre-sign pool
      try {
        const privateKey = process.env.POLYMARKET_PRIVATE_KEY || process.env.PRIVATE_KEY;
        const funderAddress = process.env.POLYMARKET_FUNDER_ADDRESS;
        if (privateKey) {
          initPrecomputedData(privateKey, funderAddress);
          logger.info('Pre-sign pool initialized');
        }
      } catch (e: any) {
        logger.warn({ err: e?.message }, 'Failed to initialize pre-sign pool');
      }

      // Use Polymarket WebSocket for real-time prices (0.1s updates)
      try {
        const { createPolymarketFeed } = await import('../../../feeds/polymarket/index.js');
        const polyFeed = await createPolymarketFeed();
        await polyFeed.start();
        wirePolyWsToEngine(polyFeed, engine, localBook);
        polyWsConnected = true;
        polyFeed.on('close', () => { polyWsConnected = false; });
        polyFeed.on('open', () => { polyWsConnected = true; });
        logger.info('Polymarket WS wired to HFT engine');
      } catch (e: any) {
        logger.warn({ err: e?.message }, 'Failed to wire Polymarket WS');
      }

      // Start CLOB REST price polling (500ms interval)
      startPricePolling(engine);

      const mode = dryRun ? 'DRY RUN' : 'LIVE';
      const strats = Object.entries(engine.getEnabledStrategies()).filter(([, v]) => v).map(([k]) => k);
      return [
        `**Crypto HFT Started [${mode}]**`,
        `Assets: ${assets.join(', ')}`,
        `Size: $${sizeUsd}/trade`,
        `Strategies: ${strats.join(', ')}`,
        `TP: ${engine.getConfig().takeProfitPct}% | SL: ${engine.getConfig().stopLossPct}%`,
        `Ratchet: ${engine.getConfig().ratchetEnabled ? 'ON' : 'OFF'} | Trailing: ${engine.getConfig().trailingEnabled ? 'ON' : 'OFF'}`,
        `Entry: ${engine.getConfig().entryOrder.mode} | Exit: ${engine.getConfig().exitOrder.mode}`,
      ].join('\n');
    }

    case 'stop': {
      if (!engine) return 'Not running.';
      const stats = engine.getStats();
      engine.stop();
      engine = null;
      stopPricePolling();
      // Clean up local order book
      if (localBook) {
        localBook.clear();
        localBook = null;
      }
      // Clean up feed and execution service to prevent resource leaks
      if (feedInstance) {
        try { feedInstance.stop(); } catch { /* best effort */ }
        feedInstance = null;
      }
      if (execInstance) {
        try { (execInstance as any).cleanup?.(); } catch { /* best effort */ }
        execInstance = null;
      }
      return `Stopped. ${stats.totalTrades} trades, ${fmtUsd(stats.netPnlUsd)} net, ${stats.winRate.toFixed(0)}% WR, fees: $${stats.feesUsd.toFixed(2)}`;
    }

    case 'status': {
      if (!engine) return 'Not running. `/crypto-hft start`';
      // Cumulative stats come from persisted trades (survive restarts); the
      // in-memory engine stats reset to zero on every start.
      const memoryStats = engine.getStats();
      const persisted = computePersistedStats();
      const s = persisted ? { ...memoryStats, ...persisted, openPositions: memoryStats.openPositions } : memoryStats;
      const r = engine.getRoundInfo();
      const p = engine.getPositions();
      const markets = engine.getMarkets();
      const clockOffset = engine.getClockOffset ? engine.getClockOffset() : 0;

      let out = `**Crypto HFT Status**\n`;
      out += `Round: #${r.slot} | ${r.ageSec.toFixed(0)}s old | ${r.timeLeftSec.toFixed(0)}s left | ${r.canTrade ? 'TRADING' : 'WAITING'}\n`;
      try {
        const risk = engine.getRiskState ? engine.getRiskState() : null;
        if (risk) {
          if (risk.halted) {
            const mins = Math.max(0, Math.ceil((risk.haltUntil - Date.now()) / 60000));
            out += `Breaker: HALTED | consecutiveLosses: ${risk.consecutiveLosses} | resume in ~${mins}min\n`;
          } else {
            out += `Breaker: OK | consecutiveLosses: ${risk.consecutiveLosses}\n`;
          }
        }
      } catch { /* ignore */ }
      out += `Markets: ${markets.length} | Open: ${s.openPositions} | PendingEntries: ${engine.getPendingEntryCount()}\n`;
      out += `Trades: ${s.totalTrades} (${s.wins}W/${s.losses}L) ${s.winRate.toFixed(0)}% WR\n`;
      out += `Gross: ${fmtUsd(s.grossPnlUsd)} | Fees: $${s.feesUsd.toFixed(2)} | Net: ${fmtUsd(s.netPnlUsd)}\n`;
      out += `Today: ${fmtUsd(s.dailyPnlUsd)} | Best: ${fmtPct(s.bestTradePct)} | Worst: ${fmtPct(s.worstTradePct)}\n`;
      out += `Clock offset: ${clockOffset.toFixed(2)}s\n`;
      out += `Maker: entry ${s.makerEntryRate.toFixed(0)}% / exit ${s.makerExitRate.toFixed(0)}%\n`;
      const volumeUsd = (s as any).volumeUsd ?? 0;
      const avgPrice = (s as any).avgPrice ?? 0;
      out += `Volume: $${volumeUsd.toFixed(2)} | Avg: $${avgPrice.toFixed(4)}\n`;

      if (Object.keys(s.exitReasons).length > 0) {
        out += `Exits: ${Object.entries(s.exitReasons).map(([k, v]) => `${k}(${v})`).join(', ')}\n`;
      }

      // Show current prices
      if (markets.length > 0) {
        out += `\n**Prices:**\n`;
        for (const m of markets) {
          out += `  ${m.asset}: UP=${m.upPrice.toFixed(2)} DOWN=${m.downPrice.toFixed(2)} (spread=${((m.upPrice + m.downPrice - 1) * 100).toFixed(1)}c)\n`;
        }
      }

      if (p.length > 0) {
        out += `\n**Open Positions:**\n`;
        for (const pos of p) {
          const pnl = pos.entryPrice !== 0 ? ((pos.currentPrice - pos.entryPrice) / pos.entryPrice) * 100 : 0;
          const secsLeft = Math.max(0, (pos.expiresAt - Date.now()) / 1000);
          out += `  ${pos.asset} ${pos.direction.toUpperCase()} @ ${pos.entryPrice.toFixed(2)} -> ${pos.currentPrice.toFixed(2)} (${fmtPct(pnl)}) [${pos.strategy}] ${secsLeft.toFixed(0)}s left\n`;
        }
      }
      return out;
    }

    case 'positions': {
      // Optional limit: `/crypto-hft positions 50`; default (no arg) = ALL trades.
      const limitArg = parts[1]?.toLowerCase();
      const parsedLimit = limitArg ? parseInt(limitArg, 10) : NaN;
      const limit = Number.isFinite(parsedLimit) && parsedLimit > 0 ? parsedLimit : 0;

      // Prefer the persisted file: it is cumulative and includes the current run.
      let rows: Array<{
        asset: string; direction: string; netPnlPct: number; netPnlUsd: number;
        strategy: string; entryTime: number | string; exitTime: number | string;
        entryPrice: number; exitPrice: number; holdTimeSec: number;
      }>;
      const persisted = getAllTrades();
      if (persisted.length > 0) {
        const list = limit > 0 ? persisted.slice(-limit) : persisted;
        rows = list.map((t) => ({
          asset: t.asset, direction: t.direction, netPnlPct: t.netPnlPct, netPnlUsd: t.netPnlUsd,
          strategy: t.strategy, entryTime: t.entryTime, exitTime: t.exitTime,
          entryPrice: t.entryPrice, exitPrice: t.exitPrice, holdTimeSec: t.holdTimeSec,
        }));
      } else {
        const closed = engine ? engine.getClosed() : [];
        const list = limit > 0 ? closed.slice(-limit) : closed;
        rows = list.map((c) => ({
          asset: c.asset, direction: c.direction, netPnlPct: c.netPnlPct, netPnlUsd: c.netPnlUsd,
          strategy: c.strategy, entryTime: c.enteredAt, exitTime: c.exitedAt ?? 0,
          entryPrice: c.entryPrice, exitPrice: c.exitPrice, holdTimeSec: c.holdTimeSec,
        }));
      }

      if (rows.length === 0) return 'No closed trades yet.';
      const fmtTime = (v: number | string): string => {
        const n = typeof v === 'string' ? Date.parse(v) : v;
        if (!n) return '--';
        return new Date(n).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', second: '2-digit' });
      };
      let out = `**Last ${rows.length} Trades:**\n`;
      for (const t of rows.reverse()) {
        const priceChange = t.exitPrice - t.entryPrice;
        out += `  ${t.asset} ${t.direction.toUpperCase()} ${fmtPct(t.netPnlPct)} (${fmtUsd(t.netPnlUsd)}) [${t.strategy}] ${fmtTime(t.entryTime)}->${fmtTime(t.exitTime)} $${t.entryPrice.toFixed(2)}->$${t.exitPrice.toFixed(2)} (${priceChange>=0?'+':''}${priceChange.toFixed(3)}) ${t.holdTimeSec.toFixed(0)}s\n`;
      }
      return out;
    }

    case 'markets': {
      const assets = parts[1] ? parts[1].toUpperCase().split(',') : DEFAULT_CONFIG.assets;
      const scanner = createMarketScanner({ ...DEFAULT_CONFIG, assets });
      const markets = await scanner.refresh();
      if (markets.length === 0) return 'No active 15-min crypto markets.';

      let out = `**Active Markets (${markets.length}):**\n`;
      for (const m of markets) {
        const secsLeft = ((m.expiresAt - Date.now()) / 1000).toFixed(0);
        out += `  ${m.asset}: UP ${m.upPrice.toFixed(2)} / DOWN ${m.downPrice.toFixed(2)} -- ${secsLeft}s left -- round #${m.roundSlot}\n`;
      }
      return out;
    }

    case 'round': {
      if (!engine) return 'Not running.';
      const r = engine.getRoundInfo();
      return `Round #${r.slot} | Age: ${r.ageSec.toFixed(0)}s | Left: ${r.timeLeftSec.toFixed(0)}s | ${r.canTrade ? 'CAN TRADE' : 'WAITING'}`;
    }

    case 'config': {
      if (!engine) return 'Not running.';

      const updates: Record<string, any> = {};
      const pairs: Array<[RegExp, string, (v: string) => any]> = [
        [/--tp\s+(\d+)/, 'takeProfitPct', Number],
        [/--sl\s+(\d+)/, 'stopLossPct', Number],
        [/--size\s+(\d+)/, 'sizeUsd', Number],
        [/--max-pos\s+(\d+)/, 'maxPositions', Number],
        [/--max-loss\s+(\d+)/, 'maxDailyLossUsd', Number],
        [/--clear-sec\s+(\d+)/, 'roundEndClearSec', Number],
        [/--stale-sec\s+(\d+)/, 'staleOrderAgeSec', Number],
        [/--entry-factor\s+([\d.]+)/, 'spreadArbEntryFactor', Number],
        [/--trend-price\s+([\d.]+)/, 'trendMinPrice', Number],
        [/--trend-confirm\s+(\d+)/, 'trendConfirmSec', Number],
        [/--trend-ratio\s+([\d.]+)/, 'trendRatio', Number],
        [/--trend-broken\s+([\d.]+)/, 'trendBrokenPrice', Number],
        [/--momentum-filter\s+(on|off)/, 'momentumFilterEnabled', (v: string) => v === 'on'],
        [/--momentum-window\s+(\d+)/, 'momentumFilterWindowSec', Number],
        [/--momentum-tol\s+([\d.]+)/, 'momentumFilterMinPct', Number],
        [/--max-fill-vs-mid\s+([\d.]+)/, 'maxFillVsMidPct', Number],
        [/--trend-entry\s+([\d.]+)/, 'trendEntryPrice', Number],
        [/--trend-entry-factor\s+([\d.]+)/, 'trendEntryFactor', Number],
        [/--trend-max-entry\s+([\d.]+)/, 'trendMaxEntryPrice', Number],
        [/--cancel-stale\s+(on|off)/, 'cancelStaleBids', (v: string) => v === 'on'],
        [/--stale-bid-pct\s+([\d.]+)/, 'staleBidPct', Number],
        [/--min-age\s+(\d+)/, 'minRoundAgeSec', Number],
        [/--min-time-left\s+(\d+)/, 'minTimeLeftSec', Number],
        [/--breaker-cooldown\s+(\d+)/, 'breakerCooldownSec', Number],
        // Risk-control toggles (A/B testing)
        [/--tight-stop\s+(on|off)/, 'tightStopEnabled', (v: string) => v === 'on'],
        [/--tight-stop-pct\s+(\d+)/, 'tightStopPct', Number],
        [/--crash-filter\s+(on|off)/, 'crashFilterEnabled', (v: string) => v === 'on'],
        [/--crash-min-age\s+(\d+)/, 'crashFilterMinHighAgeSec', Number],
        [/--crash-max-move\s+([\d.]+)/, 'crashFilterMaxSpotMovePct', Number],
        [/--slippage-guard\s+(on|off)/, 'slippageGuardEnabled', (v: string) => v === 'on'],
        [/--slippage-max\s+([\d.]+)/, 'slippageGuardMaxPct', Number],
        [/--prop-trail\s+(on|off)/, 'proportionalTrailEnabled', (v: string) => v === 'on'],
        [/--prop-trail-pct\s+([\d.]+)/, 'proportionalTrailPct', Number],
        [/--prop-trail-min\s+([\d.]+)/, 'proportionalTrailMinPct', Number],
        [/--prop-trail-min-give\s+([\d.]+)/, 'proportionalTrailMinGivebackPct', Number],
        [/--trail-min-high\s+([\d.]+)/, 'trailingMinHighPct', Number],
        [/--min-trail\s+([\d.]+)/, 'minTrailPct', Number],
        [/--exit-grace\s+(\d+)/, 'exitGraceSec', Number],
        [/--simple-exit\s+(on|off)/, 'simpleExitEnabled', (v: string) => v === 'on'],
        [/--asset-cooldown\s+(\d+)/, 'assetCooldownSec', Number],
        [/--loss-cooldown\s+(\d+)/, 'lossCooldownSec', Number],
        [/--entry-cutoff\s+(\d+)/, 'entryCutoffSec', Number],
        [/--dyn-stop\s+(on|off)/, 'dynamicStopEnabled', (v: string) => v === 'on'],
        [/--stop-tighten-start\s+(\d+)/, 'stopTightenStartSec', Number],
        [/--stop-min\s+(\d+)/, 'stopMinPct', Number],
        [/--ratchet\s+(on|off)/, 'ratchetEnabled', (v: string) => v === 'on'],
        [/--trailing\s+(on|off)/, 'trailingEnabled', (v: string) => v === 'on'],
      ];

      for (const [re, key, transform] of pairs) {
        const m = args.match(re);
        if (m) updates[key] = transform(m[1]);
      }

      if (Object.keys(updates).length === 0) {
        const c = engine.getConfig();
        return [
          '**Current Config:**',
          `Size: $${c.sizeUsd} | Max Pos: ${c.maxPositions} | Max Loss: $${c.maxDailyLossUsd}`,
          `TP: ${c.takeProfitPct}% | SL: ${c.stopLossPct}% | SimpleExit: ${c.simpleExitEnabled !== false ? 'ON' : 'OFF'}`,
          `DynStop: ${c.dynamicStopEnabled !== false ? 'ON' : 'OFF'} (base ${c.stopLossPct}% → ${c.stopMinPct ?? 10}% by ${c.stopTightenStartSec ?? 300}s left)`,
          `Ratchet: ${c.ratchetEnabled ? 'ON' : 'OFF'} | Trailing: ${c.trailingEnabled ? 'ON' : 'OFF'}`,
          `Entry: ${c.entryOrder.mode} | Exit: ${c.exitOrder.mode}`,
          `Min time left: ${c.minTimeLeftSec}s | Min round age: ${c.minRoundAgeSec}s | Force exit: ${c.forceExitSec}s`,
          `Round-end clear: ${c.roundEndClearSec ?? 180}s | Stale order age: ${c.staleOrderAgeSec ?? 25}s`,
          `spread_arb entry factor: ${c.spreadArbEntryFactor ?? 0.95} (limit = price × factor)`,
          `spread_arb trend: hold >${c.trendMinPrice ?? 0.55} for ${c.trendConfirmSec ?? 30}s → bid ${(c.trendEntryPrice ?? 0) > 0 ? (c.trendEntryPrice) : `${((c.trendEntryFactor ?? 0.9) * 100).toFixed(0)}% of price`} (max ${c.trendMaxEntryPrice ?? 0.7})`,
          `Toggles: tightStop=${c.tightStopEnabled !== false ? 'on' : 'off'}(${c.tightStopPct ?? 12}%) crashFilter=${c.crashFilterEnabled !== false ? 'on' : 'off'}(${c.crashFilterMinHighAgeSec ?? 20}s/${c.crashFilterMaxSpotMovePct ?? 3}%) slippageGuard=${c.slippageGuardEnabled !== false ? 'on' : 'off'}(${c.slippageGuardMaxPct ?? 4}%) propTrail=${c.proportionalTrailEnabled !== false ? 'on' : 'off'}(${c.proportionalTrailPct ?? 10}%≥${c.proportionalTrailMinPct ?? 15}%) trailMinHigh=${c.trailingMinHighPct ?? 5}% minTrail=${c.minTrailPct ?? 5}`,
          '',
          'Set: `/crypto-hft config --tp 15 --sl 12 --ratchet on`',
        ].join('\n');
      }

      engine.updateConfig(updates);
      return `Updated: ${Object.entries(updates).map(([k, v]) => `${k}=${v}`).join(', ')}`;
    }

    case 'enable': {
      if (!engine) return 'Not running.';
      const s = parts[1];
      if (!s) return 'Usage: `/crypto-hft enable momentum`';
      engine.setStrategyEnabled(s, true);
      return `Enabled: ${s}`;
    }

    case 'disable': {
      if (!engine) return 'Not running.';
      const s = parts[1];
      if (!s) return 'Usage: `/crypto-hft disable expiry_fade`';
      engine.setStrategyEnabled(s, false);
      return `Disabled: ${s}`;
    }

    case 'preset': {
      const sub = parts[1]?.toLowerCase() || 'list';

      if (sub === 'list') {
        const presets = listPresets();
        if (presets.length === 0) return 'No presets.';
        let out = '**Presets:**\n';
        for (const p of presets) {
          const strats = Object.entries(p.strategies).filter(([, v]) => v).map(([k]) => k);
          out += `  **${p.name}** -- ${p.description || 'No description'}\n    Strategies: ${strats.join(', ')}\n`;
        }
        return out;
      }

      if (sub === 'save') {
        if (!engine) return 'Not running.';
        const name = parts[2];
        if (!name) return 'Usage: `/crypto-hft preset save my_preset`';
        const cfg = engine.getConfig();
        const strats = engine.getEnabledStrategies();
        savePreset(name, cfg, strats);
        return `Saved preset: ${name}`;
      }

      if (sub === 'load') {
        const name = parts[2];
        if (!name) return 'Usage: `/crypto-hft preset load scalper`';
        const preset = loadPreset(name);
        if (!preset) return `Preset "${name}" not found.`;

        if (engine) {
          engine.updateConfig(preset.config);
          for (const [k, v] of Object.entries(preset.strategies)) {
            engine.setStrategyEnabled(k, v);
          }
          return `Loaded preset "${name}" into running engine.`;
        }
        return `Preset "${name}" found. Use \`/crypto-hft start --preset ${name}\` to start with it.`;
      }

      if (sub === 'delete') {
        const name = parts[2];
        if (!name) return 'Usage: `/crypto-hft preset delete my_preset`';
        if (deletePreset(name)) return `Deleted: ${name}`;
        return `Preset "${name}" not found (built-in presets can't be deleted).`;
      }

      return 'Usage: `/crypto-hft preset [list|save|load|delete] [name]`';
    }

    default:
      return formatHelp({
        name: 'Crypto HFT',
        emoji: '\u26A1',
        description: 'Trade 15-minute crypto binary markets on Polymarket with 4 automated strategies.',
        sections: [
          {
            title: 'Start/Stop',
            commands: [
              { cmd: '/crypto-hft start [BTC,ETH] [--size 20] [--dry-run] [--preset scalper]', description: 'Start the HFT engine' },
              { cmd: '/crypto-hft stop', description: 'Stop engine and show summary' },
            ],
          },
          {
            title: 'Monitor',
            commands: [
              { cmd: '/crypto-hft status', description: 'Stats, open positions, round info' },
              { cmd: '/crypto-hft positions', description: 'Recent closed trades' },
              { cmd: '/crypto-hft markets', description: 'Active 15-min markets' },
              { cmd: '/crypto-hft round', description: 'Current round timing' },
            ],
          },
          {
            title: 'Configure',
            commands: [
              { cmd: '/crypto-hft config [--tp N] [--sl N] [--ratchet on/off]', description: 'View or update config' },
              { cmd: '/crypto-hft enable <strategy>', description: 'Enable a strategy' },
              { cmd: '/crypto-hft disable <strategy>', description: 'Disable a strategy' },
            ],
          },
          {
            title: 'Presets',
            commands: [
              { cmd: '/crypto-hft preset list', description: 'Show all presets' },
              { cmd: '/crypto-hft preset save <name>', description: 'Save current config as preset' },
              { cmd: '/crypto-hft preset load <name>', description: 'Load a preset' },
              { cmd: '/crypto-hft preset delete <name>', description: 'Delete a preset' },
            ],
          },
        ],
        envVars: [
          { name: 'POLY_PRIVATE_KEY', description: 'Polymarket wallet private key (or PRIVATE_KEY)', required: true },
          { name: 'POLY_API_KEY', description: 'Polymarket CLOB API key', required: true },
          { name: 'POLY_API_SECRET', description: 'Polymarket CLOB API secret', required: true },
          { name: 'POLY_API_PASSPHRASE', description: 'Polymarket CLOB API passphrase', required: true },
          { name: 'POLY_FUNDER_ADDRESS', description: 'Polymarket funder wallet address', required: true },
          { name: 'DRY_RUN', description: 'Set to "true" for paper trading' },
        ],
        seeAlso: [
          { cmd: '/hl', description: 'Hyperliquid perps trading' },
          { cmd: '/copy', description: 'Copy trading' },
          { cmd: '/execution', description: 'Execution service controls' },
          { cmd: '/strategy', description: 'Strategy management' },
        ],
        notes: [
          'Shortcut: `/hft` is an alias for `/crypto-hft`.',
          'Strategies: momentum, mean_reversion, penny_clipper, expiry_fade.',
          'Built-in presets: conservative, aggressive, scalper, momentum_only.',
        ],
      });
  }
  } catch (error) {
    return wrapSkillError('Crypto HFT', cmd || 'command', error);
  }
}

// ── Skill Registration ──────────────────────────────────────────────────────

export default {
  name: 'crypto-hft',
  description: 'Trade 15-minute crypto binary markets on Polymarket with 4 automated strategies',
  commands: ['/crypto-hft', '/hft'],
  handle: execute,
};
