/**
 * BlitzkriegCoreClient — Node's only handle into the trading core.
 *
 * Responsibilities (deliberately narrow, per the architecture rules):
 *  - spawn `blitzkrieg-core` and own its lifecycle / crash restart
 *  - open the Unix-domain-socket JSON-RPC 2.0 channel and do a ready handshake
 *  - provide typed request methods and an event subscription
 *  - validate every inbound message against the zod wire contract
 *
 * It NEVER reads, holds or sends a private key / API secret. Credentials are
 * loaded by the Rust process from its own environment. No HTTP, no polling.
 */

import { EventEmitter } from 'events';
import { spawn, type ChildProcess } from 'child_process';
import { createConnection, type Socket } from 'net';
import { tmpdir } from 'os';
import { join } from 'path';
import { existsSync } from 'fs';
import { logger } from '../utils/logger.js';
import {
  defaultSocketPath,
} from './core-socket.js';
import {
  EventSchema,
  PositionViewSchema,
  RpcErrorSchema,
  type CoreEvent,
  type OrderRequestWire,
  type PositionViewWire,
  type TrackedOrderWire,
} from './schema.js';

export interface BlitzkriegCoreOptions {
  /** Path to the compiled binary. Defaults to core/blitzkrieg_core/target/release/blitzkrieg-core. */
  binaryPath?: string;
  /** Override the UDS path. Default: $TMPDIR/blitzkrieg-core-$USER.sock. */
  socketPath?: string;
  mode?: 'dry' | 'live';
  /** Simulated starting balance for dry mode. */
  seedBalance?: number;
  /** Per-order notional hard cap enforced inside the core. */
  maxOrderNotional?: number;
  /** Maintenance tick in ms (maker→taker escalation / pending-fill retry). */
  tickMs?: number;
  /** Auto-restart the process if it exits unexpectedly. Default true. */
  autoRestart?: boolean;
  /** Extra CLI args appended to the core process (e.g. --no-auto-exits). */
  extraArgs?: string[];
  /**
   * Working directory for the core process. The core persists its trade log and
   * near-miss records at RELATIVE paths (`data/trades/trades.jsonl`,
   * `data/shadow/near-miss.jsonl`), so test harnesses MUST set this to a scratch
   * dir or their synthetic trades leak into the production data directory.
   * Production leaves it unset (inherits the repo root).
   */
  cwd?: string;
  /**
   * Explicit trade-log path for the core (`--trade-log`). Preferred over `cwd`
   * isolation for tests: an absolute path under a temp dir is guaranteed not to
   * touch the production ledger regardless of the core's working directory.
   */
  tradeLogPath?: string;
  /** Disable trade-log persistence entirely (`--no-trade-log`). */
  noTradeLog?: boolean;
  /**
   * Disable order-log persistence (`--no-order-log`). Test harnesses that assert
   * only via events/positions set this so crash-recovery files never leak between
   * co-located cores.
   */
  noOrderLog?: boolean;
  /**
   * Disable open-position persistence (`--no-position-log`). Same rationale as
   * `noOrderLog`: keeps one harness's restored positions from perturbing the next.
   */
  noPositionLog?: boolean;
  /**
   * Select the operative market plugin by name (`--market-plugin <name>`).
   * Unset = the core uses its first registered plugin (Polymarket).
   */
  marketPlugin?: string;
}

export interface PlaceOrderParams extends OrderRequestWire {
  makerTimeoutMs?: number;
}

export interface BalanceWire {
  balance: number;
  reserved: number;
  available: number;
}

interface Pending {
  resolve: (v: any) => void;
  reject: (e: any) => void;
  timer: ReturnType<typeof setTimeout>;
}

const REQUEST_TIMEOUT_MS = 5000;
const STARTUP_TIMEOUT_MS = 15000;

function defaultBinaryPath(): string {
  // The repo is a cargo WORKSPACE, so the binary lands in the workspace-level
  // target/ at the repo root (NOT in core/blitzkrieg_core/target/). Check root
  // first (authoritative), then the crate-local path for older builds.
  const root = process.cwd();
  const candidates = [
    join(root, 'target', 'release', 'blitzkrieg-core'),
    join(root, 'core', 'blitzkrieg_core', 'target', 'release', 'blitzkrieg-core'),
    join(root, 'target', 'debug', 'blitzkrieg-core'),
    join(root, 'core', 'blitzkrieg_core', 'target', 'debug', 'blitzkrieg-core'),
  ];
  return candidates.find((p) => existsSync(p)) ?? candidates[0];
}

export class BlitzkriegCoreError extends Error {
  constructor(
    message: string,
    public readonly rpcCode?: number,
    public readonly coreCode?: string,
    public readonly raw?: string
  ) {
    super(message);
    this.name = 'BlitzkriegCoreError';
  }
}

export class BlitzkriegCoreClient extends EventEmitter {
  private readonly binaryPath: string;
  /** Mutable: `start()` may redirect to the legacy name to adopt an old core. */
  private socketPath: string;
  private readonly mode: 'dry' | 'live';
  private readonly seedBalance: number;
  private readonly maxOrderNotional: number;
  private readonly tickMs: number;
  private readonly autoRestart: boolean;
  private readonly extraArgs: string[];
  private readonly cwd: string | undefined;
  private readonly tradeLogPath: string | undefined;
  private readonly noTradeLog: boolean;
  private readonly noOrderLog: boolean;
  private readonly noPositionLog: boolean;
  private readonly marketPlugin: string | undefined;
  /** False when the caller pinned `socketPath`; the legacy probe is then skipped. */
  private readonly ownDefaultSocket: boolean;

  private proc: ChildProcess | null = null;
  private socket: Socket | null = null;
  private nextId = 1;
  private pending = new Map<string, Pending>();
  private rx = '';
  private starting: Promise<void> | null = null;
  private stopped = false;
  private connected = false;
  /** Latest socket we connected on, so a stale 'close' can't clobber a newer one. */
  private activeSocket: Socket | null = null;
  /** Consecutive failed boots (reset on a successful start or a manual start). */
  private restartAttempts = 0;
  /** Guards the restart scheduler so socket-close + process-exit schedule ONE. */
  private restartTimer: NodeJS.Timeout | null = null;
  /** Tail of the child's stderr, used to detect a duplicate-core rejection. */
  private lastStderr = '';
  /** True when `this.proc` is a process we spawned (vs. adopted/absent). */
  private ownsProc = false;

  /**
   * Consecutive boot failures after which auto-restart gives up (so a persistent
   * failure — e.g. another core owns the socket — becomes a visible error instead
   * of a tight spawn/crash loop). A manual `start()` always resets the budget.
   */
  private static readonly MAX_AUTO_RESTARTS = 5;

  constructor(opts: BlitzkriegCoreOptions = {}) {
    super();
    this.binaryPath = opts.binaryPath ?? defaultBinaryPath();
    this.socketPath = opts.socketPath ?? defaultSocketPath();
    this.ownDefaultSocket = opts.socketPath === undefined;
    this.mode = opts.mode ?? 'dry';
    this.seedBalance = opts.seedBalance ?? 10000;
    this.maxOrderNotional = opts.maxOrderNotional ?? 100;
    this.tickMs = opts.tickMs ?? 50;
    this.autoRestart = opts.autoRestart ?? true;
    this.extraArgs = opts.extraArgs ?? [];
    this.cwd = opts.cwd;
    this.tradeLogPath = opts.tradeLogPath;
    this.noTradeLog = opts.noTradeLog ?? false;
    this.noOrderLog = opts.noOrderLog ?? false;
    this.noPositionLog = opts.noPositionLog ?? false;
    this.marketPlugin = opts.marketPlugin;
  }

  /** Spawn + connect + ready handshake. Idempotent. */
  async start(): Promise<void> {
    if (this.starting) return this.starting;
    if (this.connected) return;
    this.stopped = false;
    this.restartAttempts = 0; // a manual start always gets a fresh budget
    this.starting = this.boot();
    try {
      await this.starting;
    } catch (e) {
      // A failed boot must not leave the child running in the background (it would
      // otherwise auto-restart on its own, unreachable by `stop()`). Tear it down.
      this.teardownProc();
      throw e;
    } finally {
      this.starting = null;
    }
  }


  /** Kill the child we own (if any) and clear the socket, without touching state. */
  private teardownProc() {
    const p = this.proc;
    this.proc = null;
    this.ownsProc = false;
    if (p) {
      try { p.removeAllListeners('exit'); } catch { /* noop */ }
      try { p.kill('SIGTERM'); } catch { /* noop */ }
    }
    if (this.socket) { try { this.socket.destroy(); } catch { /* noop */ } this.socket = null; }
    this.activeSocket = null;
  }

  private boot(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      if (!existsSync(this.binaryPath)) {
        return reject(new BlitzkriegCoreError(`blitzkrieg-core binary not found: ${this.binaryPath}`));
      }
      const args = [
        '--socket', this.socketPath,
        '--mode', this.mode,
        '--tick-ms', String(this.tickMs),
        '--seed-balance', String(this.seedBalance),
        '--max-order-notional', String(this.maxOrderNotional),
        // Explicit trade-log control: tests point at a throwaway path (or disable
        // persistence) so synthetic orders never reach the production ledger.
        ...(this.noTradeLog
          ? ['--no-trade-log']
          : this.tradeLogPath
            ? ['--trade-log', this.tradeLogPath]
            : []),
        // Recovery logs are DISABLED by default when the caller asked for an
        // isolated test harness; otherwise a previous harness's resting order /
        // open position would be restored into this core.
        ...(this.noOrderLog ? ['--no-order-log'] : []),
        ...(this.noPositionLog ? ['--no-position-log'] : []),
        ...(this.marketPlugin ? ['--market-plugin', this.marketPlugin] : []),
        ...this.extraArgs,
      ];
      // Inherit only non-secret config via args; the child reads POLYMARKET_*
      // credentials itself from its environment. Node never passes keys explicitly.
      const child = spawn(this.binaryPath, args, {
        stdio: ['ignore', 'pipe', 'pipe'],
        env: process.env,
        ...(this.cwd ? { cwd: this.cwd } : {}),
      });
      this.proc = child;
      this.ownsProc = true;
      this.lastStderr = '';

      child.stderr.on('data', (d) => {
        const s = String(d).trimEnd();
        // Keep only the tail — enough to classify a duplicate-core rejection.
        this.lastStderr = (this.lastStderr + ' ' + s).slice(-2000);
        logger.info({ component: 'Blitzkrieg_core' }, s);
      });
      child.on('exit', (code, signal) => {
        this.handleExit(code, signal);
      });
      child.on('error', (err) => {
        logger.error({ err, component: 'Blitzkrieg_core' }, 'failed to spawn blitzkrieg-core');
      });

      this.connectWithRetry(resolve, reject, Date.now() + STARTUP_TIMEOUT_MS);
    });
  }

  private connectWithRetry(resolve: () => void, reject: (e: Error) => void, deadline: number) {
    const sock = createConnection(this.socketPath);
    this.socket = sock;
    let settled = false;

    const giveUp = (err: Error) => {
      if (settled) return;
      settled = true;
      try { this.proc?.kill('SIGTERM'); } catch { /* noop */ }
      reject(err);
    };

    sock.on('connect', () => {
      this.connected = true;
      this.restartAttempts = 0; // a successful connection clears the backoff budget
      this.attachSocket(sock);
      settled = true;
      logger.info({ socket: this.socketPath, mode: this.mode }, 'blitzkrieg-core connected');
      resolve();
    });
    sock.on('error', (err: any) => {
      if (settled || this.stopped) return;
      if (Date.now() > deadline) return giveUp(err);
      // socket not bound yet; retry shortly.
      sock.destroy();
      setTimeout(() => {
        if (!this.stopped && !settled) this.connectWithRetry(resolve, reject, deadline);
      }, 100);
    });
  }

  private attachSocket(sock: Socket) {
    this.activeSocket = sock;
    sock.on('data', (chunk: Buffer) => this.onData(chunk.toString()));
    sock.on('close', () => {
      // Ignore a stale close from a socket we already replaced.
      if (this.activeSocket !== sock) return;
      this.activeSocket = null;
      this.connected = false;
      this.failAllPending(new BlitzkriegCoreError('blitzkrieg-core socket closed'));
      this.emit('disconnect');
      // A socket close without a process exit means the core is still alive; a
      // reconnect (not a spawn) is the right response. Only auto-restart when we
      // own the process; an adopted core is reconnected by its own owner.
      if (this.autoRestart && !this.stopped) {
        this.scheduleRestart('socket closed');
      }
    });
    sock.on('error', (err) => logger.error({ err, component: 'Blitzkrieg_core' }, 'blitzkrieg-core socket error'));
  }

  private handleExit(code: number | null, signal: NodeJS.Signals | null) {
    logger.warn({ code, signal, component: 'Blitzkrieg_core' }, 'blitzkrieg-core process exited');
    this.connected = false;
    this.failAllPending(new BlitzkriegCoreError(`blitzkrieg-core exited (code=${code} signal=${signal})`));
    if (this.socket) { this.socket.destroy(); this.socket = null; }
    this.activeSocket = null;
    this.proc = null;
    this.ownsProc = false;

    if (!this.autoRestart || this.stopped) return;

    // A duplicate-core rejection means another core already owns the socket — so
    // spawning again can never succeed. Try to ADOPT the existing core instead of
    // looping spawn→"already listening"→crash forever.
    if (/already (listening|in use)/i.test(this.lastStderr)) {
      logger.warn('blitzkrieg-core socket already served by another process — adopting it');
      void this.tryAdopt();
      return;
    }

    this.scheduleRestart(`exit code=${code}`);
  }

  /**
   * Connect to a core we did not spawn (one that already owns the socket). We do
   * not own its process, so `stop()` only disconnects from it.
   */
  private async tryAdopt(): Promise<void> {
    try {
      await this.connectExisting(Date.now() + STARTUP_TIMEOUT_MS);
      this.ownsProc = false;
      this.restartAttempts = 0;
      logger.info({ socket: this.socketPath }, 'adopted the existing blitzkrieg-core');
    } catch (e) {
      this.emit('error', e);
      logger.error(
        { err: e, component: 'Blitzkrieg_core' },
        'cannot adopt existing core — giving up auto-restart'
      );
      this.emit('fatal', e);
    }
  }

  /** Connect-only retry loop (no spawn), used to adopt an existing core. */
  private connectExisting(deadline: number): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      this.connectWithRetry(resolve, reject, deadline);
    });
  }

  /**
   * Schedule a single auto-restart with exponential backoff and a hard cap, so a
   * persistent failure surfaces as an error instead of a tight crash loop.
   */
  private scheduleRestart(reason: string) {
    if (this.stopped || this.restartTimer) return;
    if (this.restartAttempts >= BlitzkriegCoreClient.MAX_AUTO_RESTARTS) {
      const err = new BlitzkriegCoreError(
        `blitzkrieg-core failed to start ${this.restartAttempts} times (last: ${reason}); auto-restart disabled`
      );
      logger.error({ component: 'Blitzkrieg_core' }, err.message);
      this.emit('fatal', err);
      return;
    }
    this.restartAttempts += 1;
    const delay = Math.min(300 * 2 ** (this.restartAttempts - 1), 30_000);
    logger.warn(
      { attempt: this.restartAttempts, delayMs: delay },
      `blitzkrieg-core restart scheduled (${reason})`
    );
    this.restartTimer = setTimeout(() => {
      this.restartTimer = null;
      if (!this.stopped) this.start().catch((e) => logger.error({ err: e }, 'blitzkrieg-core restart failed'));
    }, delay);
    this.restartTimer.unref?.();
  }

  private failAllPending(err: Error) {
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(err);
    }
    this.pending.clear();
  }

  private onData(text: string) {
    this.rx += text;
    let nl: number;
    while ((nl = this.rx.indexOf('\n')) >= 0) {
      const line = this.rx.slice(0, nl).trim();
      this.rx = this.rx.slice(nl + 1);
      if (!line) continue;
      this.onLine(line);
    }
  }

  private onLine(line: string) {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      logger.warn({ line: line.slice(0, 200) }, 'blitzkrieg-core sent non-JSON line');
      return;
    }

    // Server-pushed event.
    if (msg.method === 'core.event') {
      const parsed = EventSchema.safeParse(msg.params);
      if (!parsed.success) {
        logger.warn({ errors: parsed.error.issues, raw: msg.params }, 'blitzkrieg-core event failed schema validation');
        this.emit('protocolError', msg.params);
        return;
      }
      const ev: CoreEvent = parsed.data;
      this.emit('event', ev);
      this.emit(ev.kind.toLowerCase(), ev);
      return;
    }

    // Response to one of our requests.
    if (msg.id === undefined || msg.id === null) return;
    const p = this.pending.get(String(msg.id));
    if (!p) return;
    this.pending.delete(String(msg.id));
    clearTimeout(p.timer);
    if (msg.error) {
      const errMeta = RpcErrorSchema.safeParse(msg.error);
      const data = errMeta.success ? errMeta.data.data : undefined;
      p.reject(new BlitzkriegCoreError(msg.error.message, msg.error.code, data?.coreCode, data?.raw ?? undefined));
    } else {
      p.resolve(msg.result);
    }
  }

  private request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    if (!this.connected || !this.socket || this.socket.destroyed) {
      return Promise.reject(new BlitzkriegCoreError('blitzkrieg-core not connected'));
    }
    const id = 'n' + this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) reject(new BlitzkriegCoreError(`request timeout: ${method}`));
      }, REQUEST_TIMEOUT_MS);
      timer.unref?.();
      this.pending.set(id, { resolve, reject, timer });
      this.socket!.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }

  // ── Typed API ─────────────────────────────────────────────────────────────

  async ping(): Promise<{ pong: boolean; ts: number }> {
    return this.request('core.ping');
  }

  async ready(): Promise<{ version: string; mode: string; authenticated: boolean }> {
    return this.request('core.ready');
  }

  async kill(reason = 'manual'): Promise<void> {
    await this.request('risk.kill', { reason });
  }
  async resume(): Promise<void> {
    await this.request('risk.resume');
  }

  async placeOrder(p: PlaceOrderParams): Promise<{ orderId: string; status: TrackedOrderWire['status'] }> {
    const { makerTimeoutMs, ...order } = p;
    return this.request('orders.place', { ...order, makerTimeoutMs: makerTimeoutMs ?? 0 });
  }

  async cancelOrder(orderId: string): Promise<{ success: boolean }> {
    return this.request('orders.cancel', { orderId });
  }

  async cancelAllOrders(tokenId?: string): Promise<{ cancelled: number }> {
    return this.request('orders.cancel_all', { tokenId: tokenId ?? null });
  }

  async listOrders(): Promise<{ orders: TrackedOrderWire[] }> {
    return this.request('orders.list');
  }

  async balance(): Promise<BalanceWire> {
    return this.request('ledger.balance');
  }

  /** Trigger a reconciliation sweep against a caller-supplied venue snapshot.
   *  In live mode the core also reconciles periodically on its own. */
  async reconcile(snapshot: {
    openOrderIds?: string[];
    trades?: Array<{
      venueOrderId: string;
      tradeId: string;
      tokenId: string;
      side: 'buy' | 'sell';
      size: number;
      price: number;
      tsMs?: number;
      /** The venue's own maker/taker report for this trade. When supplied it
       *  overrides the order's fill policy, which is what makes a live ledger
       *  follow the venue rather than our intent (E17). */
      maker?: boolean | null;
    }>;
  }): Promise<{ filled: number; markedFilled: number; markedCancelled: number; ghostIds: string[] }> {
    return this.request('orders.reconcile', {
      openOrderIds: snapshot.openOrderIds ?? [],
      trades: snapshot.trades ?? [],
    });
  }

  /** P0 bridge: push an L2 snapshot so the dry core can simulate maker fills.
   *  Removed in P3 once market-data ingestion lives inside the core. */
  async bookSnapshot(tokenId: string, bids: Array<[number, number]>, asks: Array<[number, number]>): Promise<void> {
    await this.request('books.snapshot', {
      tokenId,
      bids: bids.map(([price, size]) => ({ price, size })),
      asks: asks.map(([price, size]) => ({ price, size })),
    });
  }

  /** Open positions enriched with current price / unrealised PnL. */
  async positions(): Promise<{ positions: PositionViewWire[] }> {
    const raw = await this.request<{ positions: unknown[] }>('positions.list');
    return { positions: raw.positions.map((p) => PositionViewSchema.parse(p)) };
  }

  /** Force-close one position (by id) or all open positions. */
  async exitPositions(positionId?: string): Promise<{ closed: number }> {
    return this.request('positions.exit', { positionId: positionId ?? null });
  }

  // ── P3 self-driving engine feed ──────────────────────────────────────────

  /** Supply the current round's UP/DOWN markets so the engine can evaluate. */
  async setMarkets(markets: Array<Record<string, unknown>>): Promise<void> {
    await this.request('engine.markets', { markets });
  }

  /** Full L2 book straight to the engine: the Rust-native feed's path
   *  (`--feed-ws`) and the one `--backtest` replays, with no dry maker-fill
   *  simulation (that lives in `books.snapshot`). Use it to capture a session
   *  that is decision-for-decision replayable offline. */
  async engineBook(tokenId: string, bids: Array<[number, number]>, asks: Array<[number, number]>): Promise<void> {
    await this.request('engine.book', {
      tokenId,
      bids: bids.map(([price, size]) => ({ price, size })),
      asks: asks.map(([price, size]) => ({ price, size })),
    });
  }

  /** Binance spot price tick (feeds the momentum filter). */
  async spotPrice(asset: string, price: number): Promise<void> {
    await this.request('spot.price', { asset, price });
  }

  /** Top-of-book update (price_change / best_bid_ask). */
  async topOfBook(tokenId: string, bestBid?: number, bestAsk?: number): Promise<void> {
    await this.request('books.top', { tokenId, bestBid, bestAsk });
  }

  /** Engine diagnostics: feed counters + confirmed tokens + blocked near-misses. */
  async stats(): Promise<{
    books: number; tops: number; spots: number; rounds: number;
    evaluations: number; signals: number; placeRejected: number;
    strategyLimitRejected: number;
    /** Entry-gate near-misses. The two counts are global totals; `byStrategy`
     *  attributes them to the strategy whose candidate was blocked (E2-b), and
     *  `declaredExemptions` lists the opt-outs in force so a waived gate is
     *  visible rather than a silent hole. */
    blocked: {
      timing: number; momentum: number;
      byStrategy: Record<string, { timing: number; momentum: number }>;
      declaredExemptions: Array<{ strategy: string; gates: string[] }>;
    } | null;
    confirmed: string[];
    /** Per-strategy session ledger + live exposure (P-1.1), the quota /
     *  effective sizing in force (E2-a) and the declared entry-gate exemptions
     *  with how often each was honoured (E2-b). Caps and the sizing band are
     *  null when the strategy inherits the globals; `sizingSource` says which. */
    strategies: Array<{
      name: string; enabled: boolean; source: string;
      openPositions: number; openNotionalUsd: number;
      /** Configured caps; null = uncapped / inherits the global value. */
      maxOpenPositions: number | null;
      maxOpenNotionalUsd: number | null;
      sizingSource: 'global' | 'strategy';
      effectiveSizeUsd: number; effectiveMinShares: number; effectiveMaxShares: number;
      ordersPlaced: number; ordersRejected: number; limitRejected: number;
      /** Entry gates this strategy declared unnecessary; [] = fully gated. */
      gateExemptions: string[];
      blockedTiming: number; blockedMomentum: number;
      gateExemptedTiming: number; gateExemptedMomentum: number;
      closedTrades: number; wins: number; losses: number;
      feesUsd: number; netPnlUsd: number;
    }>;
    /** Market-data capture state (P-1.3); null when archiving is off. */
    archive: {
      path: string; events: number; bytes: number; dropped: number;
      recording: boolean; rotateBytes: number; segmentBytes: number;
      segments: number; freeBytes: number; stoppedReason: string | null;
    } | null;
  } | null> {
    return this.request('engine.stats');
  }

  /** Current round state from the engine's scanner. */
  async round(): Promise<{ slot: number; ageSec: number; timeLeftSec: number; markets: number } | null> {
    return this.request('engine.round');
  }

  /** Closed-trade history (Node-compatible records). */
  async tradesHistory(limit = 50): Promise<{ trades: any[] }> {
    return this.request('trades.history', { limit });
  }

  // ── Market plugins (P0.6) ────────────────────────────────────────────────
  /** List registered market plugins (name/type/capabilities/enabled/active). */
  async listMarketPlugins(): Promise<{
    version: string;
    active: string | null;
    plugins: Array<{
      name: string;
      type: string;
      hasDataFeed: boolean;
      hasDiscovery: boolean;
      hasExecutor: boolean;
      enabled: boolean;
      active: boolean;
    }>;
  }> {
    return this.request('market.list');
  }

  // ── Shadow Evolution (opt-in, per-strategy since E2-c) ───────────────────
  //
  // Every mutation names exactly ONE strategy: parameters, audit files, evaluate
  // decisions and rollback anchors are all per strategy, so two strategies can
  // evolve in parallel without touching each other. `status` reports each
  // strategy's own block (`strategies[]`) alongside the aggregate counters kept
  // for older consumers.
  async shadowEvolutionEnable(): Promise<{ enabled: boolean }> {
    return this.request('shadow_evolution.enable');
  }
  async shadowEvolutionDisable(): Promise<{ enabled: boolean }> {
    return this.request('shadow_evolution.disable');
  }
  /**
   * Per-strategy status. `strategies[]` is the authoritative view: one entry per
   * strategy that declared evolvable knobs, carrying its own params, declared
   * knob domains, counters and cooldown. A strategy absent from it declared
   * nothing and is therefore **not evolvable** — an explicit answer, not a
   * "disabled".
   */
  async shadowEvolutionStatus(): Promise<{
    status: string;
    currentParams: Record<string, Record<string, string>>;
    variantCount: number;
    variants: any[];
    evolutionsApplied: number;
    evolutionsRejected: number;
    secondsSinceLastEvolution: number;
    strategies: Array<{
      strategy: string;
      status: string | null;
      params: Record<string, string> | null;
      knobs: Array<{ name: string; value: string; min: string; max: string }>;
      evolutionsApplied: number;
      evolutionsRejected: number;
      secondsSinceLastEvolution: number;
    }>;
  }> {
    return this.request('shadow_evolution.status');
  }
  /** Audit history, optionally narrowed to ONE strategy's own file. */
  async shadowEvolutionHistory(limit = 50, strategy?: string): Promise<{ strategy: string | null; history: any[] }> {
    return this.request('shadow_evolution.history', strategy ? { limit, strategy } : { limit });
  }
  /**
   * Roll back ONE strategy to the parameters in force before its last change.
   * Errors when that strategy has nothing to roll back to, so one strategy's
   * rollback can never be mistaken for another's silent no-op.
   */
  async shadowEvolutionRollback(strategy: string): Promise<{ rolledBack: boolean; strategy: string }> {
    return this.request('shadow_evolution.rollback', { strategy });
  }
  /**
   * Operator override for ONE strategy. Values are validated against that
   * strategy's own declaration, domain and the ±gradient lock, and the change is
   * audited as a manual (operator) action. Send decimals as strings to keep the
   * no-float wire rule.
   */
  async shadowEvolutionApply(
    strategy: string,
    params: Record<string, string | number>,
  ): Promise<{ applied: boolean; strategy: string }> {
    return this.request('shadow_evolution.apply', { strategy, params });
  }

  /**
   * Stop the core and WAIT until it is actually gone.
   *
   * The previous implementation fired SIGTERM and returned, which made shutdown
   * unobservable: the caller could not tell whether the process was still
   * holding the socket, still inside a tick, or still managing resting orders.
   * Anything sequenced after `stop()` — starting a replacement core, deleting
   * scratch dirs, releasing a singleton lock — raced it.
   *
   * The order of operations is deliberate:
   *   1. mark stopped, so no restart is scheduled and no caller retries;
   *   2. settle resting orders while the channel is still up — the "no ghost
   *      orders" step, which must happen BEFORE the socket closes;
   *   3. close the socket;
   *   4. SIGTERM, await exit, escalate to SIGKILL if the core does not go.
   *
   * An adopted core is never signalled: it belongs to another owner, so we only
   * close our channel to it.
   */
  async stop(options: { cancelRestingOrders?: boolean } = {}): Promise<void> {
    const { cancelRestingOrders = true } = options;
    this.stopped = true;
    if (this.restartTimer) { clearTimeout(this.restartTimer); this.restartTimer = null; }

    // (2) Settle resting orders while we still have a live channel. Best effort
    // by design: a core that is already wedged must not block shutdown, and the
    // durable order log means the next boot restores-and-sweeps anything we
    // failed to cancel here.
    if (cancelRestingOrders && this.connected && this.ownsProc) {
      try {
        const res = await this.cancelAllOrders();
        const n = res?.cancelled ?? 0;
        if (n > 0) logger.info({ cancelled: n }, 'cancelled resting orders before shutdown');
      } catch (err) {
        logger.warn({ err }, 'could not cancel resting orders before shutdown');
      }
    }

    this.failAllPending(new BlitzkriegCoreError('blitzkrieg-core stopped'));
    if (this.activeSocket) { try { this.activeSocket.end(); } catch { /* noop */ } this.activeSocket = null; }
    if (this.socket) { try { this.socket.end(); } catch { /* noop */ } this.socket = null; }

    // (4) Only kill a process we actually spawned; an adopted core belongs to
    // another owner and must be left running.
    const proc = this.proc;
    const owned = this.ownsProc;
    this.proc = null;
    this.ownsProc = false;
    if (!proc || !owned) return;

    await BlitzkriegCoreClient.terminate(proc);
  }

  /**
   * Terminate `proc` and resolve only once it has really exited.
   *
   * SIGTERM first so the core can flush near-misses and unlink its socket. A
   * core stuck inside a tick can exceed the grace period, so escalate to
   * SIGKILL: a half-shut-down core is worse than a hard-killed one, because it
   * keeps a claim on the socket and may still act on the book.
   */
  private static async terminate(
    proc: ChildProcess,
    graceMs = 5000,
    killMs = 2000,
  ): Promise<void> {
    if (proc.exitCode !== null || proc.signalCode !== null) return;

    const awaitExit = (ms: number) =>
      new Promise<boolean>((resolve) => {
        const onDone = () => { clearTimeout(timer); resolve(true); };
        const timer = setTimeout(() => {
          proc.removeListener('exit', onDone);
          proc.removeListener('close', onDone);
          resolve(false);
        }, ms);
        // 'exit' can fire before stdio drains; 'close' is the safer edge. Take
        // whichever comes first and let the timeout bound the wait.
        proc.once('exit', onDone);
        proc.once('close', onDone);
      });

    try { proc.kill('SIGTERM'); } catch { /* already gone */ }
    if (await awaitExit(graceMs)) return;

    logger.warn({ pid: proc.pid, graceMs }, 'blitzkrieg-core ignored SIGTERM; escalating to SIGKILL');
    try { proc.kill('SIGKILL'); } catch { /* already gone */ }
    if (!(await awaitExit(killMs))) {
      // Unreapable child. Surfacing this is the point: the caller must not
      // believe shutdown succeeded while a process still holds resources.
      throw new BlitzkriegCoreError(
        `blitzkrieg-core (pid ${proc.pid}) did not exit after SIGKILL`,
      );
    }
  }

  isConnected(): boolean {
    return this.connected;
  }

  /**
   * The socket actually in use. Differs from the canonical name only during the
   * migration window, when a pre-rename core is adopted instead of duplicated.
   */
  resolvedSocketPath(): string {
    return this.socketPath;
  }
}
