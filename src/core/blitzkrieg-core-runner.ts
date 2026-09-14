/**
 * BlitzkriegCoreRunner — the Node "shell" that runs the Rust core as the trading
 * engine. Node owns process lifecycle and presents status; ALL trading logic
 * (discovery, market data, signals, risk, orders, positions, exits) lives in
 * the Rust process.
 *
 * Opt-in via HFT_CORE=rust (default remains the existing Node engine so the
 * switch is reversible and never silent). Node passes only parameters.
 */

import { BlitzkriegCoreClient, type BlitzkriegCoreOptions } from './blitzkrieg-core-client.js';
import { logger } from '../utils/logger.js';

export interface BlitzkriegRunConfig {
  assets: string[];
  roundSec: number;
  dryRun: boolean;
  sizeUsd: number;
  minShares: number;
  maxShares: number;
  maxPositions: number;
  maxDailyLossUsd: number;
  minRoundAgeSec: number;
  minTimeLeftSec: number;
  /** Per-strategy entry caps, raw `name:maxOpen:maxNotional` (P-1.1). */
  strategyLimits?: string[];
  /** Market-data archive for post-hoc replay (P-1.3). null/absent = off. */
  eventArchive?: EventArchiveConfig | null;
}

/**
 * Always-on market-data capture (P-1.3): the core mirrors every event it consumes
 * into JSONL so a later `--backtest` can replay the exact stream — that is how a
 * past stop-out gets its order book path reconstructed. Rotation is mandatory in
 * practice (`≈11 MB/min`), and the free-space floor is what actually bounds disk
 * use: the archive never deletes, so retention is the operator's decision.
 */
export interface EventArchiveConfig {
  path: string;
  /** Rotate into a new UTC-stamped segment past this size (MiB). 0 = never. */
  rotateMb: number;
  /** Whole-session cap (MiB). 0 = unlimited (rotation + the floor bound usage). */
  maxMb: number;
  /** Stop recording before the volume drops below this many MiB free. */
  minFreeMb: number;
}

export interface BlitzkriegRunStatus {
  running: boolean;
  mode: 'dry' | 'live';
  roundSlot: number;
  ageSec: number;
  timeLeftSec: number;
  canTrade: boolean;
  markets: number;
  marketPrices: Array<{ asset: string; up: number; down: number }>;
  connected: boolean;
  stats: {
    books: number;
    spots: number;
    confirmed: number;
    signals: number;
    blockedTiming: number;
    blockedMomentum: number;
    openPositions: number;
    entries: number;
    fills: number;
    dailyPnlUsd: number;
    wins: number;
    losses: number;
    /** Market-data capture state (P-1.3); null when archiving is off. */
    archive: ArchiveStatusWire | null;
  };
}

/** `engine.stats.archive` as the core reports it (camelCase wire form). */
export interface ArchiveStatusWire {
  path: string;
  events: number;
  bytes: number;
  dropped: number;
  recording: boolean;
  rotateBytes: number;
  segmentBytes: number;
  segments: number;
  freeBytes: number;
  stoppedReason: string | null;
}

/** Whether the Rust core should drive trading instead of the Node engine. */
export function blitzkriegCoreEnabled(): boolean {
  const v = (process.env.HFT_CORE || '').toLowerCase();
  return v === 'rust' || v === 'uds';
}

export class BlitzkriegCoreRunner {
  private client: BlitzkriegCoreClient | null = null;
  private cfg: BlitzkriegRunConfig | null = null;
  /** In-flight start, so concurrent `/crypto-hft start` never spawns two cores. */
  private starting: Promise<void> | null = null;
  private entries = 0;
  private fills = 0;
  private wins = 0;
  private losses = 0;
  private dailyPnl = 0;
  private lastError: string | null = null;

  isRunning(): boolean {
    return this.client !== null;
  }

  /** The underlying IPC client (null when stopped). */
  getClient(): BlitzkriegCoreClient | null {
    return this.client;
  }

  /** Start the Rust core with the given parameters. */
  async start(cfg: BlitzkriegRunConfig): Promise<void> {
    if (this.client) throw new Error('Rust core already running');
    // Serialise concurrent starts: the readiness handshake awaits the socket, and
    // without this guard two `/crypto-hft start` commands can both pass the
    // `client` check and spawn two cores (one then dies with EADDRINUSE, or worse
    // steals the live core's socket).
    if (this.starting) return this.starting;
    this.starting = this.doStart(cfg);
    try {
      await this.starting;
    } finally {
      this.starting = null;
    }
  }

  private async doStart(cfg: BlitzkriegRunConfig): Promise<void> {
    this.cfg = cfg;
    this.entries = 0;
    this.fills = 0;
    this.wins = 0;
    this.losses = 0;
    this.dailyPnl = 0;

    // Per-order share bounds. Node owns the parameter, the core enforces it. Keep
    // min<=max so a bad config cannot invert the clamp (the core guards too).
    const minShares = Math.max(0, cfg.minShares || 0);
    const maxShares = Math.max(minShares, cfg.maxShares || minShares);

    const opts: BlitzkriegCoreOptions = {
      mode: cfg.dryRun ? 'dry' : 'live',
      seedBalance: Math.max(1000, cfg.maxDailyLossUsd * 5),
      // The per-order notional cap is a SAFETY bound, not the strategy's nominal
      // size: orders are priced at (shares * price) with shares clamped to
      // maxShares and price capped by the entry ceiling (~0.45). Using sizeUsd
      // here rejected every 10-share order (10*0.43 > 2.5). Size it from the real
      // worst case so it never blocks a legitimate order but still catches runaway
      // sizing bugs: maxShares * 0.6 (covers the 0.45 entry cap + taker buffer).
      maxOrderNotional: Math.max(cfg.sizeUsd, maxShares * 0.6),
      tickMs: 50,
      autoRestart: true,
      extraArgs: [
        '--engine',
        '--feed-ws',
        '--assets', cfg.assets.join(','),
        '--round-sec', String(cfg.roundSec),
        '--min-round-age', String(cfg.minRoundAgeSec),
        '--min-time-left', String(cfg.minTimeLeftSec),
        '--max-positions', String(cfg.maxPositions),
        '--min-shares', String(minShares),
        '--max-shares', String(maxShares),
        // Optional per-strategy entry caps (P-1.1): raw `name:maxOpen:maxNotional`
        // strings passed straight through; the core validates and warns on
        // malformed entries. Absent → no flag → behaviour unchanged.
        ...(cfg.strategyLimits ?? []).flatMap((s) => ['--strategy-limit', s]),
        '--max-order-notional', String(Math.max(cfg.sizeUsd, maxShares * 0.6)),
        // Always-on market-data capture (P-1.3). Rotation keeps the file replayable
        // in chunks; the free-space floor stops recording before the volume fills.
        // Passing no archive flag (eventArchive: null) disables it entirely.
        ...(cfg.eventArchive
          ? [
              '--event-archive', cfg.eventArchive.path,
              '--event-archive-rotate-mb', String(cfg.eventArchive.rotateMb),
              '--event-archive-max-mb', String(cfg.eventArchive.maxMb),
              '--event-archive-min-free-mb', String(cfg.eventArchive.minFreeMb),
            ]
          : []),
      ],
    };
    const client = new BlitzkriegCoreClient(opts);
    client.on('event', (e: any) => this.onEvent(e));
    client.on('error', (err: any) => { this.lastError = String(err?.message || err); });
    client.on('fatal', (err: any) => { this.lastError = String(err?.message || err); });
    try {
      await client.start();
    } catch (e) {
      // Do not strand a half-started client: the client's own start() already
      // tore down the child, but make sure `this.client` stays null so a later
      // `/crypto-hft start` can retry cleanly.
      this.client = null;
      throw e;
    }
    this.client = client;
    logger.info({ mode: opts.mode, assets: cfg.assets }, 'Rust core engine started');
    // The core discovers rounds for these assets itself; nothing else to push.
    await client.ready().catch(() => null);
  }

  /** Stop the Rust core. */
  async stop(): Promise<void> {
    // If a start is in flight, let it finish so we stop the client it created
    // (otherwise the core would come up unmanaged right after `/crypto-hft stop`).
    if (this.starting) await this.starting.catch(() => {});
    if (!this.client) return;
    try { await this.client.stop(); } catch { /* best effort */ }
    this.client = null;
    logger.info('Rust core engine stopped');
  }

  private onEvent(e: any): void {
    switch (e?.kind) {
      case 'ORDER_UPDATE':
        if (e.order?.strategy === 'spread_arb' && e.order?.side === 'buy' && e.order?.status === 'FILLED') this.entries++;
        break;
      case 'FILL':
        this.fills++;
        break;
      case 'POSITION_CLOSED':
        this.dailyPnl = Number(e.dailyPnlUsd ?? this.dailyPnl);
        if (Number(e.netPnlUsd) >= 0) this.wins++; else this.losses++;
        break;
      case 'ERROR':
        this.lastError = e.error?.message ?? this.lastError;
        break;
      default:
        break;
    }
  }

  async status(): Promise<BlitzkriegRunStatus | null> {
    if (!this.client) return null;
    const [round, stats, pos] = await Promise.all([
      this.client.round().catch(() => null),
      this.client.stats().catch(() => null),
      this.client.positions().catch(() => ({ positions: [] })),
    ]);
    const r: any = round ?? {};
    return {
      running: true,
      mode: this.cfg?.dryRun === false ? 'live' : 'dry',
      roundSlot: r.slot ?? 0,
      ageSec: r.ageSec ?? 0,
      timeLeftSec: r.timeLeftSec ?? 0,
      canTrade: Boolean(r.canTrade),
      markets: r.markets ?? 0,
      marketPrices: Array.isArray(r.marketPrices)
        ? r.marketPrices.map((m: any) => ({ asset: m.asset, up: Number(m.up), down: Number(m.down) }))
        : [],
      connected: this.client.isConnected(),
      stats: {
        books: stats?.books ?? 0,
        spots: stats?.spots ?? 0,
        confirmed: stats?.confirmed?.length ?? 0,
        signals: stats?.signals ?? 0,
        blockedTiming: stats?.blocked?.timing ?? 0,
        blockedMomentum: stats?.blocked?.momentum ?? 0,
        openPositions: pos.positions.length,
        entries: this.entries,
        fills: this.fills,
        dailyPnlUsd: this.dailyPnl,
        wins: this.wins,
        losses: this.losses,
        archive: (stats?.archive ?? null) as ArchiveStatusWire | null,
      },
    };
  }

  async positions(): Promise<Array<{ asset: string; direction: string; entryPrice: number; currentPrice: number; unrealizedPct: number; strategy: string; timeLeftSec: number }>> {
    if (!this.client) return [];
    const { positions } = await this.client.positions();
    return positions.map((p: any) => ({
      asset: p.asset,
      direction: p.direction,
      entryPrice: Number(p.entryPrice),
      currentPrice: Number(p.currentPrice),
      unrealizedPct: Number(p.unrealizedPct),
      strategy: p.strategy ?? 'spread_arb',
      timeLeftSec: Number(p.remainingSec ?? 0),
    }));
  }

  lastErrorMessage(): string | null {
    return this.lastError;
  }
}

/** Process-wide singleton (the skill owns its lifecycle). */
let runnerInstance: BlitzkriegCoreRunner | null = null;

export function getBlitzkriegCoreRunner(): BlitzkriegCoreRunner {
  if (!runnerInstance) runnerInstance = new BlitzkriegCoreRunner();
  return runnerInstance;
}
