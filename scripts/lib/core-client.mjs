/**
 * Bare-Node client for the Rust trading core (no npm dependencies).
 *
 * The repository's Node verification layer (src/ + tsconfig + package.json) was
 * removed: production runs core + panel entirely in Rust
 * (ui_kit_web/gateway/supervisor.rs), and the acceptance gates only need to
 * drive a temporary core over the UDS JSON-RPC channel. This module replaces
 * the compiled TS client for those gates with ~150 lines of stdlib code.
 *
 * Contract (single source of truth: blitzkrieg-core/src/ipc/schema.rs):
 *   framing  — one JSON object per '\n' over a Unix domain socket
 *   requests — {jsonrpc:"2.0",id,method,params}
 *   events   — {jsonrpc:"2.0",method:"core.event",params:{kind,...}}
 *   errors   — msg.error = {code,message,data:{coreCode,raw?}}
 *
 * It NEVER reads, holds or sends a private key / API secret. Credentials are
 * loaded by the Rust process from its own environment.
 */

import { spawn } from 'child_process';
import { connect } from 'net';
import { tmpdir } from 'os';
import { join } from 'path';
import { existsSync } from 'fs';

const REQUEST_TIMEOUT_MS = 5000;
const CONNECT_TIMEOUT_MS = 15000;

/** Workspace-root release binary path (authoritative), then fallbacks. */
export function defaultBinaryPath(cwd = process.cwd()) {
  const candidates = [
    join(cwd, 'target', 'release', 'blitzkrieg-core'),
    join(cwd, 'core', 'blitzkrieg_core', 'target', 'release', 'blitzkrieg-core'),
    join(cwd, 'target', 'debug', 'blitzkrieg-core'),
    join(cwd, 'core', 'blitzkrieg_core', 'target', 'debug', 'blitzkrieg-core'),
  ];
  return candidates.find((p) => existsSync(p)) ?? candidates[0];
}

/**
 * The path this client should use. Test harnesses always pass an explicit
 * `socketPath` (a private scratch socket), so production adoption logic is not
 * reproduced here: an explicit path wins, otherwise the canonical name.
 */
export function defaultSocketPath(env = process.env) {
  const user = env.USER || 'user';
  const dir = env.TMPDIR && env.TMPDIR.length > 0 ? env.TMPDIR : tmpdir();
  return join(dir.replace(/\/$/, ''), `blitzkrieg-core-${user}.sock`);
}

export class CoreClientError extends Error {
  constructor(message, coreCode, rpcCode) {
    super(message);
    this.name = 'CoreClientError';
    this.coreCode = coreCode;
    this.rpcCode = rpcCode;
  }
}

/**
 * Spawn (or connect to) a blitzkrieg-core and speak the wire protocol.
 *
 * Options (subset the gates rely on):
 *   binaryPath, socketPath, mode, seedBalance, maxOrderNotional, tickMs,
 *   autoRestart, extraArgs, cwd, tradeLogPath, noTradeLog, noOrderLog,
 *   noPositionLog, marketPlugin
 *
 * Events emitted: 'event' (every core.event), 'error', 'fatal'.
 */
export class CoreClient {
  constructor(opts = {}) {
    this.binaryPath = opts.binaryPath ?? defaultBinaryPath();
    this.socketPath = opts.socketPath ?? defaultSocketPath();
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
    this.onEvent = null;
    this.onError = null;
    this.onFatal = null;

    this.proc = null;
    this.ownsProc = false;
    this.sock = null;
    this.connected = false;
    this.stopped = false;
    this.starting = null;
    this.nextId = 1;
    this.pending = new Map();
    this.rx = '';
    this.lastStderr = '';
  }

  /** Spawn + connect + ready handshake. Idempotent. */
  start() {
    if (this.starting) return this.starting;
    if (this.connected) return Promise.resolve();
    this.stopped = false;
    this.starting = this.#boot();
    return this.starting.finally(() => { this.starting = null; });
  }

  /**
   * Attach to a core that is ALREADY listening — no spawn.
   *
   * The path for every client that did not start the process it is talking to:
   * the panel supervisor did, another harness did, or (for the operator probes)
   * the deployment did. `ownsProc` stays false on an adopted client, so `stop()`
   * can only close this socket — it cannot signal a core it does not own, which
   * is what makes it safe to point at a live one.
   *
   * One attempt, no retry: every caller either spins on the socket path itself
   * (the gate scripts all wait for it to appear) or is talking to a core it
   * knows is live. Retrying here would turn "the core is gone" — an assertion
   * several gates make — into a multi-second wait per probe, and a UDS connect
   * fails at once (ENOENT / ECONNREFUSED) rather than hanging.
   */
  static async connect({ socketPath } = {}) {
    const client = new CoreClient({ socketPath, autoRestart: false });
    await client.#connectWithRetry(0); // deadline 0 → the first failure gives up
    return client;
  }

  async #boot() {
    if (!existsSync(this.binaryPath)) {
      throw new CoreClientError(`blitzkrieg-core binary not found: ${this.binaryPath}`);
    }
    const args = [
      '--socket', this.socketPath,
      '--mode', this.mode,
      '--tick-ms', String(this.tickMs),
      '--seed-balance', String(this.seedBalance),
      '--max-order-notional', String(this.maxOrderNotional),
      ...(this.noTradeLog ? ['--no-trade-log']
        : this.tradeLogPath ? ['--trade-log', this.tradeLogPath] : []),
      ...(this.noOrderLog ? ['--no-order-log'] : []),
      ...(this.noPositionLog ? ['--no-position-log'] : []),
      ...(this.marketPlugin ? ['--market-plugin', this.marketPlugin] : []),
      ...this.extraArgs,
    ];
    // The child reads POLYMARKET_* credentials from its own environment; the
    // client never passes keys explicitly.
    const child = spawn(this.binaryPath, args, {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: process.env,
      ...(this.cwd ? { cwd: this.cwd } : {}),
    });
    this.proc = child;
    this.ownsProc = true;
    this.lastStderr = '';
    child.stderr?.on('data', (d) => {
      // Raw, with no separator invented between chunks. `eprintln!` writes an
      // unbuffered stderr one format fragment at a time, so a single banner line
      // genuinely arrives as several chunks; joining them with a space puts a
      // character into the buffer that the stream never contained, and every
      // assertion whose pattern spans a boundary then fails against a correct
      // core (`...socketMode=` + ` ` + `0600...` matches neither
      // `socketMode=0600` nor `socketMode=\S+`). The window is 8 KiB rather than
      // 2 KiB because the shipped log level is INFO (#184): a boot can now emit
      // far more than 2 KiB before the first assertion reads this buffer.
      this.lastStderr = (this.lastStderr + String(d)).slice(-8192);
    });
    child.on('exit', (code, signal) => this.#handleExit(code, signal));

    // A duplicate-core rejection aborts this boot; the caller decides whether to
    // adopt (core-adopt-check drives that path itself).
    await this.#connectWithRetry(Date.now() + 15000);
  }

  #connectWithRetry(deadline) {
    return new Promise((resolve, reject) => {
      const attempt = (remaining) => {
        if (this.stopped) return reject(new CoreClientError('client stopped'));
        const sock = connect(this.socketPath);
        this.sock = sock;
        let settled = false;
        const giveUp = (err) => {
          if (settled) return;
          settled = true;
          try { this.proc?.kill('SIGTERM'); } catch { /* noop */ }
          reject(err);
        };
        sock.on('connect', () => {
          settled = true;
          this.connected = true;
          this.#attach(sock);
          resolve();
        });
        sock.on('error', (err) => {
          if (settled || this.stopped) return;
          if (Date.now() > deadline) return giveUp(err);
          sock.destroy();
          setTimeout(attempt, 100);
        });
      };
      attempt(deadline);
    });
  }

  #attach(sock) {
    sock.on('data', (chunk) => this.#onData(String(chunk)));
    sock.on('close', () => {
      if (this.sock !== sock) return;
      this.sock = null;
      this.connected = false;
      this.#failAllPending(new CoreClientError('blitzkrieg-core socket closed'));
    });
  }

  #handleExit(code, signal) {
    this.connected = false;
    this.#failAllPending(new CoreClientError(`blitzkrieg-core exited (code=${code} signal=${signal})`));
    try { this.sock?.destroy(); } catch { /* noop */ }
    this.sock = null;
    this.proc = null;
    this.ownsProc = false;
    if (this.autoRestart && !this.stopped && !/already (listening|in use)/i.test(this.lastStderr)) {
      // Gates drive one-shot sessions; a live parent restarts by calling
      // start() again. No implicit respawn timer — the old shell's auto-restart
      // ownership lives in the Rust gateway now.
      this.onError?.(new CoreClientError(`core exited (code=${code} signal=${signal})`));
    }
  }

  #onData(text) {
    this.rx += text;
    let nl;
    while ((nl = this.rx.indexOf('\n')) >= 0) {
      const line = this.rx.slice(0, nl).trim();
      this.rx = this.rx.slice(nl + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      if (msg.method === 'core.event') {
        this.onEvent?.(msg.params);
        continue;
      }
      if (msg.id === undefined || msg.id === null) continue;
      const p = this.pending.get(msg.id);
      if (!p) continue;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) {
        p.reject(new CoreClientError(
          msg.error.message ?? 'rpc error',
          msg.error.data?.coreCode ?? msg.error.data?.core_code,
          msg.error.code,
        ));
      } else {
        p.resolve(msg.result);
      }
    }
  }

  #failAllPending(err) {
    for (const p of this.pending.values()) { clearTimeout(p.timer); p.reject(err); }
    this.pending.clear();
  }

  request(method, params = {}, timeoutMs = REQUEST_TIMEOUT_MS) {
    if (!this.connected || !this.sock) {
      return Promise.reject(new CoreClientError('blitzkrieg-core not connected'));
    }
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) reject(new CoreClientError(`request timeout: ${method}`));
      }, timeoutMs);
      timer.unref?.();
      this.pending.set(id, { resolve, reject, timer });
      this.sock.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }

  isConnected() { return this.connected; }

  /** Stop the core we own: cancel resting orders, SIGTERM, escalate to SIGKILL. */
  async stop({ cancelRestingOrders = true } = {}) {
    this.stopped = true;
    if (cancelRestingOrders && this.connected && this.ownsProc) {
      await this.request('orders.cancel_all', { tokenId: null }).catch(() => {});
    }
    this.#failAllPending(new CoreClientError('blitzkrieg-core stopped'));
    try { this.sock?.end(); } catch { /* noop */ }
    const proc = this.proc;
    const owned = this.ownsProc;
    this.proc = null;
    this.ownsProc = false;
    if (!proc || !owned) return;
    await this.#terminate(proc);
  }

  /** Synchronous SIGKILL for signal handlers (process exit cannot await). */
  killNow() {
    this.stopped = true;
    const proc = this.proc;
    if (!proc || !this.ownsProc) return;
    this.proc = null;
    this.ownsProc = false;
    try { proc.kill('SIGKILL'); } catch { /* already gone */ }
  }

  async #terminate(proc, graceMs = 5000, killMs = 2000) {
    if (proc.exitCode !== null || proc.signalCode !== null) return;
    const awaitExit = (ms) => new Promise((resolve) => {
      const onDone = () => { clearTimeout(t); resolve(true); };
      const t = setTimeout(() => {
        proc.removeListener('exit', onDone);
        proc.removeListener('close', onDone);
        resolve(false);
      }, ms);
      proc.once('exit', onDone);
      proc.once('close', onDone);
    });
    try { proc.kill('SIGTERM'); } catch { /* already gone */ }
    if (await awaitExit(graceMs)) return;
    try { proc.kill('SIGKILL'); } catch { /* already gone */ }
    if (!(await awaitExit(killMs))) {
      throw new CoreClientError(`blitzkrieg-core (pid ${proc.pid}) did not exit after SIGKILL`);
    }
  }
}

/**
 * One request over an already-listening socket, then hang up.
 *
 * This replaces the per-script `rpc()` helpers that each spoke the wire
 * protocol themselves, and it replaces them for a reason beyond line count: the
 * core broadcasts `core.event` to EVERY session (`ipc/server.rs`), so a client
 * that reads only the first line can read a notification instead of its reply
 * and quietly answer `undefined` — a gate that then fails on a correct core, or
 * worse, passes an assertion about a value it never received. This client
 * matches replies by id and skips notifications.
 *
 * Rejects with a `CoreClientError` on an RPC error, on a timeout, and on an
 * unreachable socket. `timeoutMs` bounds the request; the connect is a single
 * attempt (see `connect`).
 */
export async function requestOnce(socketPath, method, params = {}, { timeoutMs = REQUEST_TIMEOUT_MS } = {}) {
  const client = await CoreClient.connect({ socketPath });
  try {
    return await client.request(method, params, timeoutMs);
  } finally {
    // Closes the socket only: an adopted client owns no process to signal.
    await client.stop({ cancelRestingOrders: false });
  }
}

/**
 * Typed wrappers used by the gates. Kept as plain functions over `client` so
 * each gate reads exactly like the wire protocol it asserts.
 */
export const rpc = {
  ping: (c) => c.request('core.ping'),
  ready: (c) => c.request('core.ready'),
  placeOrder: (c, p) => c.request('orders.place', {
    ...p,
    makerTimeoutMs: p.makerTimeoutMs ?? 0,
  }),
  cancelOrder: (c, orderId) => c.request('orders.cancel', { orderId }),
  cancelAll: (c, tokenId = null) => c.request('orders.cancel_all', { tokenId }),
  listOrders: (c) => c.request('orders.list'),
  balance: (c) => c.request('ledger.balance'),
  reconcile: (c, snapshot) => c.request('orders.reconcile', {
    openOrderIds: snapshot.openOrderIds ?? [],
    trades: snapshot.trades ?? [],
  }),
  bookSnapshot: (c, tokenId, bids, asks) => c.request('books.snapshot', {
    tokenId,
    bids: bids.map(([price, size]) => ({ price, size })),
    asks: asks.map(([price, size]) => ({ price, size })),
  }),
  positions: (c) => c.request('positions.list'),
  exitPositions: (c, positionId = null) => c.request('positions.exit', { positionId }),
  setMarkets: (c, markets) => c.request('engine.markets', { markets }),
  spotPrice: (c, asset, price) => c.request('spot.price', { asset, price }),
  stats: (c) => c.request('engine.stats'),
  round: (c) => c.request('engine.round'),
  tradesHistory: (c, limit = 50) => c.request('trades.history', { limit }),
  kill: (c, reason = 'manual') => c.request('risk.kill', { reason }),
  resume: (c) => c.request('risk.resume'),
};
