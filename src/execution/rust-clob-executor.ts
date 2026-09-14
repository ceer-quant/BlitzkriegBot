import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';

import { logger } from '../utils/logger.js';

interface RustResponse {
  id: number;
  ok: boolean;
  result?: {
    orderId?: string;
    status?: string;
    success?: boolean;
    tradeIds?: string[];
    balance?: string;
    allowances?: unknown;
    signer?: string;
    funder?: string;
  };
  error?: string;
}

export interface RustOrderRequest {
  tokenId: string;
  side: 'BUY' | 'SELL';
  price: number;
  size: number;
  postOnly: boolean;
}

export interface RustOrderResult {
  success: boolean;
  orderId?: string;
  status?: string;
  error?: string;
}

let processRef: ChildProcessWithoutNullStreams | null = null;
let nextId = 1;
let queue = Promise.resolve();
const pending = new Map<number, { resolve: (value: RustResponse) => void; reject: (error: Error) => void }>();

function binaryPath(): string {
  return join(process.cwd(), 'rust-executor', 'target', 'release', 'clodds-rust-executor');
}

function ensureProcess(): ChildProcessWithoutNullStreams {
  if (processRef && !processRef.killed) return processRef;
  const path = binaryPath();
  if (!existsSync(path)) throw new Error(`Rust executor not built: ${path}`);

  const child = spawn(path, [], { env: process.env, stdio: ['pipe', 'pipe', 'pipe'] });
  processRef = child;
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    try {
      const response = JSON.parse(line) as RustResponse;
      const request = pending.get(response.id);
      if (!request) return;
      pending.delete(response.id);
      request.resolve(response);
    } catch (error) {
      logger.error({ error, line: line.slice(0, 300) }, 'Invalid Rust executor response');
    }
  });
  child.stderr.on('data', data => logger.info({ message: data.toString().trim() }, 'Rust CLOB executor'));
  child.on('exit', (code, signal) => {
    logger.error({ code, signal }, 'Rust CLOB executor exited');
    processRef = null;
    for (const request of pending.values()) request.reject(new Error(`Rust executor exited (${code ?? signal})`));
    pending.clear();
  });
  return child;
}

function request(method: string, params: unknown): Promise<RustResponse> {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    try {
      const child = ensureProcess();
      pending.set(id, { resolve, reject });
      child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    } catch (error) {
      reject(error instanceof Error ? error : new Error(String(error)));
    }
  });
}

export function rustAuthCheck(): Promise<RustResponse> {
  return request('auth_check', {});
}

/**
 * Query the collateral (USDC) balance via the Rust SDK's Poly1271-authenticated
 * client. Returns the balance in USDC (already scaled from 6-decimals).
 */
export function rustBalance(): Promise<{ success: boolean; balance?: number; raw?: string; error?: string }> {
  const run = queue.then(async () => {
    const response = await request('balance', {});
    if (!response.ok) return { success: false, error: response.error || 'Rust executor balance failed' };
    const raw = response.result?.balance;
    if (raw === undefined) return { success: false, error: 'No balance returned' };
    const n = Number(raw);
    if (!Number.isFinite(n)) return { success: false, raw, error: 'Non-numeric balance' };
    // USDC has 6 decimals; the SDK returns raw base units.
    const balance = n > 1_000_000 ? n / 1e6 : n;
    return { success: true, balance, raw };
  });
  queue = run.then(() => undefined, () => undefined);
  return run;
}

export function rustPlaceLimitOrder(order: RustOrderRequest): Promise<RustOrderResult> {
  // SAFETY GATE: this executor signs and posts orders with NO local order
  // tracking — an order placed here is invisible to the OME, so it can become an
  // orphan the bot cannot manage or cancel. That is exactly the failure that lost
  // real funds when it was first wired to the Node HFT path. It is therefore
  // disabled unless explicitly opted in, and callers should prefer the core's
  // tracked live path (HFT_CORE=rust) instead.
  if (process.env.ALLOW_STATELESS_RUST_EXECUTOR !== 'true') {
    return Promise.resolve({
      success: false,
      error:
        'stateless rust executor disabled: it places orders with no local tracking ' +
        '(orphan risk). Set ALLOW_STATELESS_RUST_EXECUTOR=true only for a deliberate, ' +
        'manually-managed test — otherwise use the core live path.',
    });
  }
  const run = queue.then(async () => {
    const response = await request('place_limit', order);
    if (!response.ok) return { success: false, error: response.error || 'Rust executor rejected order' };
    return {
      success: Boolean(response.result?.success),
      orderId: response.result?.orderId,
      status: response.result?.status,
      error: response.error,
    };
  });
  queue = run.then(() => undefined, () => undefined);
  return run;
}

export function stopRustExecutor(): void {
  processRef?.kill('SIGTERM');
  processRef = null;
}
