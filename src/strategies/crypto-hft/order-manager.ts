/**
 * Order Manager — Full lifecycle tracking for all orders.
 *
 * Features:
 * - Unique order IDs (one per tokenId+direction per round)
 * - Persistent JSONL database of all orders
 * - Timeout cancellation for unfilled GTC orders
 * - Forced cancel when strategy closes position
 * - Forced cancel when order is already filled
 */

import { appendFileSync, readFileSync, existsSync, mkdirSync } from 'fs';
import { join } from 'path';
import { logger } from '../../utils/logger.js';

const DATA_DIR = join(process.cwd(), 'data', 'orders');
const ORDERS_FILE = join(DATA_DIR, 'orders.jsonl');

// ── Types ───────────────────────────────────────────────────────────────────

export type OrderStatus =
  | 'SUBMITTED'   // sent to exchange
  | 'LIVE'        // resting on book
  | 'FILLED'      // fully filled
  | 'PARTIAL'     // partially filled
  | 'CANCELLED'   // cancelled by us
  | 'EXPIRED'     // expired by exchange
  | 'REJECTED';   // rejected by exchange

export interface TrackedOrder {
  /** Unique order ID from exchange (e.g. 0x...) */
  orderId: string;
  /** Internal unique key: `${strategy}:${asset}:${direction}:${roundSlot}` */
  internalKey: string;
  strategy: string;
  asset: string;
  direction: string;
  tokenId: string;
  conditionId: string;
  price: number;
  size: number;
  side: 'BUY' | 'SELL';
  status: OrderStatus;
  submittedAt: number;
  lastUpdatedAt: number;
  filledSize: number;
  avgFillPrice: number;
  roundSlot: number;
  reason: string;
  /** Whether this order was submitted as a maker (post-only) order */
  wasMaker?: boolean;
  /** Strategy target exit price carried onto the resulting position */
  targetExitPrice?: number;
  /** Whether this order is being actively tracked for timeout */
  timeoutAt: number;
}

// ── Database ────────────────────────────────────────────────────────────────

function ensureDir() {
  if (!existsSync(DATA_DIR)) {
    mkdirSync(DATA_DIR, { recursive: true });
  }
}

function appendOrder(record: TrackedOrder) {
  ensureDir();
  appendFileSync(ORDERS_FILE, JSON.stringify(record) + '\n');
}

export function loadAllOrders(): TrackedOrder[] {
  if (!existsSync(ORDERS_FILE)) return [];
  try {
    const lines = readFileSync(ORDERS_FILE, 'utf-8').split('\n').filter(Boolean);
    return lines.map(line => JSON.parse(line));
  } catch {
    return [];
  }
}

// hydrate is now a method on OrderManager

// ── Manager ─────────────────────────────────────────────────────────────────

export interface OrderManager {
  /** Generate a unique internal key for an order */
  makeKey(strategy: string, asset: string, direction: string, roundSlot: number): string;

  /** Check if an order with this internal key already exists (pending or live) */
  hasPendingOrder(internalKey: string): boolean;

  /** Record a new order submission */
  recordSubmit(params: {
    orderId: string;
    internalKey: string;
    strategy: string;
    asset: string;
    direction: string;
    tokenId: string;
    conditionId: string;
    price: number;
    size: number;
    side: 'BUY' | 'SELL';
    roundSlot: number;
    reason: string;
    wasMaker?: boolean;
    targetExitPrice?: number;
    timeoutMs?: number;
  }): TrackedOrder;

  /** Mark an order as filled */
  recordFill(orderId: string, filledSize: number, avgFillPrice: number): void;

  /** Mark an order as cancelled/expired/rejected */
  recordTerminal(orderId: string, status: OrderStatus): void;

  /** Find all live orders for a given token+direction */
  findLiveOrders(tokenId: string, direction: string): TrackedOrder[];

  /** Find all live orders */
  getAllLiveOrders(): TrackedOrder[];

  /** Get orders that have exceeded their timeout */
  getTimedOutOrders(): TrackedOrder[];

  /** Get a tracked order by exchange orderId */
  getOrder(orderId: string): TrackedOrder | null;

  /** Get all orders (for debugging) */
  getAllOrders(): TrackedOrder[];

  /** Hydrate from persisted orders on restart */
  hydrate(orders: TrackedOrder[]): void;
}

export function createOrderManager(): OrderManager {
  // In-memory index for fast lookups
  const ordersById = new Map<string, TrackedOrder>();
  const ordersByKey = new Map<string, TrackedOrder>();

  return {
    makeKey(strategy, asset, direction, roundSlot) {
      return `${strategy}:${asset}:${direction}:${roundSlot}`;
    },

    hasPendingOrder(internalKey) {
      const order = ordersByKey.get(internalKey);
      if (!order) return false;
      return ['SUBMITTED', 'LIVE', 'PARTIAL'].includes(order.status);
    },

    recordSubmit({ orderId, internalKey, strategy, asset, direction, tokenId, conditionId, price, size, side, roundSlot, reason, wasMaker, targetExitPrice, timeoutMs }) {
      const now = Date.now();
      const record: TrackedOrder = {
        orderId,
        internalKey,
        strategy,
        asset,
        direction,
        tokenId,
        conditionId,
        price,
        size,
        side,
        status: 'SUBMITTED',
        submittedAt: now,
        lastUpdatedAt: now,
        filledSize: 0,
        avgFillPrice: 0,
        roundSlot,
        reason,
        wasMaker,
        targetExitPrice,
        timeoutAt: timeoutMs ? now + timeoutMs : 0,
      };
      ordersById.set(orderId, record);
      ordersByKey.set(internalKey, record);
      appendOrder(record);
      logger.info({ orderId, asset, direction, price, size, side, timeoutMs }, 'Order tracked');
      return record;
    },

    recordFill(orderId, filledSize, avgFillPrice) {
      const order = ordersById.get(orderId);
      if (!order) return;
      order.filledSize = filledSize;
      order.avgFillPrice = avgFillPrice;
      order.status = filledSize >= order.size ? 'FILLED' : 'PARTIAL';
      order.lastUpdatedAt = Date.now();
      order.timeoutAt = 0; // no longer pending timeout
      appendOrder(order);
      logger.info({ orderId, filledSize, avgFillPrice, status: order.status }, 'Order filled');
    },

    recordTerminal(orderId, status) {
      const order = ordersById.get(orderId);
      if (!order) return;
      order.status = status;
      order.lastUpdatedAt = Date.now();
      order.timeoutAt = 0;
      appendOrder(order);
      logger.info({ orderId, status }, 'Order terminal');
    },

    findLiveOrders(tokenId, direction) {
      return Array.from(ordersById.values()).filter(
        o => o.tokenId === tokenId && o.direction === direction && ['SUBMITTED', 'LIVE', 'PARTIAL'].includes(o.status)
      );
    },

    getAllLiveOrders() {
      return Array.from(ordersById.values()).filter(o => ['SUBMITTED', 'LIVE', 'PARTIAL'].includes(o.status));
    },

    getTimedOutOrders() {
      const now = Date.now();
      return Array.from(ordersById.values()).filter(
        o => o.timeoutAt > 0 && now > o.timeoutAt && ['SUBMITTED', 'LIVE', 'PARTIAL'].includes(o.status)
      );
    },

    getOrder(orderId) {
      return ordersById.get(orderId) ?? null;
    },

    getAllOrders() {
      return Array.from(ordersById.values());
    },

    hydrate(orders) {
      // Build map of last status per orderId (JSONL is append-only, last entry wins)
      const lastByOrderId = new Map<string, TrackedOrder>();
      for (const order of orders) {
        lastByOrderId.set(order.orderId, order);
      }
      let hydrated = 0;
      for (const order of lastByOrderId.values()) {
        if (['SUBMITTED', 'LIVE', 'PARTIAL'].includes(order.status)) {
          ordersById.set(order.orderId, order);
          ordersByKey.set(order.internalKey, order);
          hydrated++;
        }
      }
      logger.info({ hydrated, total: lastByOrderId.size }, 'Hydrated orders into OrderManager');
    },
  };
}
