/**
 * Rust core ↔ Node IPC wire contract (Node mirror).
 *
 * SINGLE SOURCE OF TRUTH is the Rust serde schema in
 * blitzkrieg-core/src/{model,ipc/schema}.rs. These zod schemas validate every inbound
 * message; an unknown shape is rejected rather than silently consumed, and no
 * private-key field exists on any message (Node never holds credentials).
 *
 * Framing: one JSON object per '\n' over a Unix domain socket, JSON-RPC 2.0.
 */

import { z } from 'zod';

export const SideSchema = z.enum(['buy', 'sell']);
export const ModeSchema = z.enum(['dry', 'live']);
export const OrderStatusSchema = z.enum([
  'PENDING',
  'LIVE',
  'PARTIALLY_FILLED',
  'FILLED',
  'CANCELLED',
  'REJECTED',
  'FAILED',
]);
export const FillPolicySchema = z.enum(['taker', 'maker', 'maker_then_taker']);
export const CoreErrorCodeSchema = z.enum([
  'INVALID_PARAMS',
  'UNKNOWN_ORDER',
  'WOULD_CROSS',
  'INVALID_TICK_SIZE',
  'INVALID_SIZE',
  'INSUFFICIENT_FUNDS',
  'RISK_REJECTED',
  'KILL_SWITCH_ACTIVE',
  'MARKET_HALTED',
  'NOT_AUTHENTICATED',
  'VENUE_ERROR',
  'TIMEOUT',
  'INTERNAL',
]);

const DecimalSchema = z.number().or(z.string().regex(/^-?\d+(\.\d+)?$/));

export const OrderRequestSchema = z.object({
  tokenId: z.string().min(1),
  conditionId: z.string(),
  side: SideSchema,
  mode: FillPolicySchema,
  price: DecimalSchema,
  size: DecimalSchema,
  internalKey: z.string(),
  strategy: z.string(),
  asset: z.string(),
  direction: z.string(),
  roundSlot: z.number().int(),
});
export type OrderRequestWire = z.infer<typeof OrderRequestSchema>;

export const TrackedOrderSchema = z.object({
  orderId: z.string(),
  internalKey: z.string(),
  strategy: z.string(),
  asset: z.string(),
  direction: z.string(),
  tokenId: z.string(),
  conditionId: z.string(),
  side: SideSchema,
  mode: FillPolicySchema,
  price: z.number(),
  size: z.number(),
  filledSize: z.number(),
  avgFillPrice: z.number().nullable(),
  status: OrderStatusSchema,
  roundSlot: z.number().int(),
  submittedAtMs: z.number(),
  updatedAtMs: z.number(),
  venueOrderId: z.string().nullable().optional(),
  escalateAtMs: z.number().nullable(),
});
export type TrackedOrderWire = z.infer<typeof TrackedOrderSchema>;

export const PositionViewSchema = z.object({
  id: z.string(),
  asset: z.string(),
  direction: z.string(),
  strategy: z.string(),
  tokenId: z.string(),
  entryPrice: z.number(),
  currentPrice: z.number(),
  shares: z.number(),
  unrealizedPct: z.number(),
  highPnlPct: z.number(),
  wasMakerEntry: z.boolean(),
  enteredAtMs: z.number(),
  expiresAtMs: z.number(),
  remainingSec: z.number(),
  // Cash actually moved on this position so far (E17). Lets a monitor check the
  // ledger identity mid-flight, not only when the book happens to be flat.
  costUsd: z.number(),
  entryFeeUsd: z.number(),
  proceedsUsd: z.number(),
  exitFeeUsd: z.number(),
});
export type PositionViewWire = z.infer<typeof PositionViewSchema>;

export const FillDeltaSchema = z.object({
  orderId: z.string(),
  tokenId: z.string(),
  side: SideSchema,
  delta: z.number(),
  price: z.number(),
  cumulative: z.number(),
  strategy: z.string(),
  asset: z.string(),
  direction: z.string(),
  conditionId: z.string(),
});
export type FillDeltaWire = z.infer<typeof FillDeltaSchema>;

export const EventSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('READY'),
    version: z.string(),
    mode: ModeSchema,
  }),
  z.object({ kind: z.literal('ORDER_UPDATE'), order: TrackedOrderSchema }),
  z.object({ kind: z.literal('FILL'), delta: FillDeltaSchema, order: TrackedOrderSchema }),
  z.object({
    kind: z.literal('RISK_ALERT'),
    code: CoreErrorCodeSchema,
    message: z.string(),
  }),
  z.object({
    kind: z.literal('EVOLUTION_SIGNAL'),
    signal: z.object({
      signalId: z.string(),
      timestamp: z.number(),
      confidence: z.number(),
      sampleCount: z.number(),
      expectedImprovement: z.number(),
      variantId: z.string(),
      reason: z.string(),
      fromParams: z.record(z.number()),
      toParams: z.record(z.number()),
    }).passthrough(),
  }),
  z.object({
    kind: z.literal('EVOLUTION_APPLIED'),
    signal: z.object({
      signalId: z.string(),
      timestamp: z.number(),
      confidence: z.number(),
      sampleCount: z.number(),
      variantId: z.string(),
      reason: z.string(),
    }).passthrough(),
  }),
  z.object({
    kind: z.literal('EVOLUTION_REJECTED'),
    signal: z.object({ signalId: z.string(), variantId: z.string() }).passthrough(),
    reason: z.string(),
  }),
  z.object({
    kind: z.literal('RECONCILE_REPORT'),
    filled: z.number().int(),
    markedFilled: z.number().int(),
    markedCancelled: z.number().int(),
    ghostIds: z.array(z.string()),
  }),
  z.object({
    kind: z.literal('POSITION_CLOSED'),
    id: z.string(),
    asset: z.string(),
    direction: z.string(),
    reason: z.string(),
    netPnlUsd: z.number(),
    netPnlPct: z.number(),
    dailyPnlUsd: z.number(),
  }),
  z.object({
    kind: z.literal('ERROR'),
    error: z.object({
      code: CoreErrorCodeSchema,
      message: z.string(),
      raw: z.string().nullable().optional(),
    }),
  }),
]);
export type CoreEvent = z.infer<typeof EventSchema>;

/** `market.list` result — registered market plugins (P0.6). */
export const MarketPluginInfoSchema = z.object({
  name: z.string(),
  type: z.string(),
  hasDataFeed: z.boolean(),
  hasDiscovery: z.boolean(),
  hasExecutor: z.boolean(),
  enabled: z.boolean(),
  active: z.boolean(),
});
export const MarketListSchema = z.object({
  version: z.string(),
  active: z.string().nullable(),
  plugins: z.array(MarketPluginInfoSchema),
});
export type MarketPluginInfo = z.infer<typeof MarketPluginInfoSchema>;

export const RpcErrorSchema = z.object({
  code: z.number(),
  message: z.string(),
  data: z
    .object({
      coreCode: CoreErrorCodeSchema,
      raw: z.string().nullable().optional(),
    })
    .optional(),
});
