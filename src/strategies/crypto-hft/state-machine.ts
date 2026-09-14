/**
 * State Machine — Manages bot lifecycle states and transitions.
 *
 * States:
 *   IDLE      — Bot stopped, no activity
 *   OBSERVING — Bot running, watching market, no orders
 *   ARMED     — Conditions met, order ready to submit
 *   HOLDING   — Position open, monitoring exit
 *   COOLDOWN  — Just closed position, waiting before next trade
 *
 * Transitions:
 *   IDLE → OBSERVING      (start command)
 *   OBSERVING → ARMED     (trend confirmed + safety margin OK)
 *   ARMED → HOLDING       (order filled)
 *   ARMED → OBSERVING     (order cancelled/expired)
 *   HOLDING → COOLDOWN    (position closed)
 *   COOLDOWN → OBSERVING  (cooldown expired)
 *   Any → IDLE            (stop command)
 */

import { logger } from '../../utils/logger.js';

// ============================================================================
// TYPES
// ============================================================================

export type BotState = 'IDLE' | 'OBSERVING' | 'ARMED' | 'HOLDING' | 'COOLDOWN';

export interface StateTransition {
  from: BotState;
  to: BotState;
  reason: string;
  timestamp: number;
}

export interface SafetyMargin {
  /** Current Binance spot price */
  currentPrice: number;
  /** Opening price (start of window) */
  openingPrice: number;
  /** Safety margin percentage */
  marginPct: number;
  /** Is margin acceptable? */
  ok: boolean;
}

export interface StateContext {
  /** Current bot state */
  state: BotState;
  /** When current state was entered */
  stateEnteredAt: number;
  /** Last 10 state transitions */
  transitions: StateTransition[];
  /** Current safety margin */
  safetyMargin: SafetyMargin | null;
  /** Whether trend is confirmed (token >= 0.55) */
  trendConfirmed: boolean;
  /** Whether we have an open position */
  hasPosition: boolean;
  /** Cooldown expiry timestamp */
  cooldownExpiresAt: number;
}

// ============================================================================
// STATE MACHINE
// ============================================================================

export interface StateMachine {
  /** Get current state */
  getState(): BotState;
  /** Get full context */
  getContext(): StateContext;
  /** Transition to new state */
  transition(to: BotState, reason: string): void;
  /** Check if transition is valid */
  canTransition(to: BotState): boolean;
  /** Update safety margin */
  updateSafetyMargin(currentPrice: number, openingPrice: number): void;
  /** Update trend status */
  updateTrend(confirmed: boolean): void;
  /** Update position status */
  updatePosition(hasPosition: boolean): void;
  /** Check if cooldown has expired */
  isCooldownExpired(): boolean;
}

const VALID_TRANSITIONS: Record<BotState, BotState[]> = {
  IDLE: ['OBSERVING'],
  OBSERVING: ['ARMED', 'IDLE'],
  ARMED: ['HOLDING', 'OBSERVING', 'IDLE'],
  HOLDING: ['COOLDOWN', 'IDLE'],
  COOLDOWN: ['OBSERVING', 'IDLE'],
};

const COOLDOWN_DURATION_MS = 5_000; // 5 seconds

export function createStateMachine(): StateMachine {
  let state: BotState = 'IDLE';
  let stateEnteredAt = Date.now();
  const transitions: StateTransition[] = [];
  let safetyMargin: SafetyMargin | null = null;
  let trendConfirmed = false;
  let hasPosition = false;
  let cooldownExpiresAt = 0;

  function transition(to: BotState, reason: string): void {
    if (!VALID_TRANSITIONS[state]?.includes(to)) {
      logger.warn({ from: state, to, reason }, 'Invalid state transition');
      return;
    }

    const now = Date.now();
    const transitionRecord: StateTransition = {
      from: state,
      to,
      reason,
      timestamp: now,
    };

    transitions.push(transitionRecord);
    if (transitions.length > 10) transitions.shift();

    logger.info({ from: state, to, reason }, 'State transition');

    state = to;
    stateEnteredAt = now;

    // Set cooldown expiry when entering COOLDOWN
    if (to === 'COOLDOWN') {
      cooldownExpiresAt = now + COOLDOWN_DURATION_MS;
    }
  }

  return {
    getState: () => state,
    getContext: () => ({
      state,
      stateEnteredAt,
      transitions: [...transitions],
      safetyMargin,
      trendConfirmed,
      hasPosition,
      cooldownExpiresAt,
    }),
    transition,
    canTransition: (to) => VALID_TRANSITIONS[state]?.includes(to) ?? false,
    updateSafetyMargin: (currentPrice, openingPrice) => {
      const marginPct = openingPrice > 0
        ? ((currentPrice - openingPrice) / openingPrice) * 100
        : 0;
      safetyMargin = {
        currentPrice,
        openingPrice,
        marginPct,
        ok: marginPct >= 0.1, // 0.1% minimum safety margin
      };
    },
    updateTrend: (confirmed) => { trendConfirmed = confirmed; },
    updatePosition: (pos) => { hasPosition = pos; },
    isCooldownExpired: () => Date.now() >= cooldownExpiresAt,
  };
}

// ============================================================================
// SAFETY MARGIN CALCULATOR
// ============================================================================

/**
 * Calculate safety margin based on Binance spot price.
 *
 * Logic:
 * - Track price at start of observation window
 * - safety_margin = (current_price - window_opening_price) / window_opening_price
 * - Only allow entry when:
 *   1. safety_margin > 0.1% (price hasn't dropped)
 *   2. Token price >= 0.55 (trend confirmed)
 * - Cancel order if safety_margin < 0 (Binance crashed)
 */
export function calculateSafetyMargin(
  currentBinancePrice: number,
  windowOpeningPrice: number
): SafetyMargin {
  const marginPct = windowOpeningPrice > 0
    ? ((currentBinancePrice - windowOpeningPrice) / windowOpeningPrice) * 100
    : 0;

  return {
    currentPrice: currentBinancePrice,
    openingPrice: windowOpeningPrice,
    marginPct,
    ok: marginPct >= 0.1,
  };
}

/**
 * Check if entry is allowed based on safety margin and trend.
 */
export function canEnter(
  safetyMargin: SafetyMargin,
  tokenPrice: number,
  trendConfirmed: boolean
): { ok: boolean; reason: string } {
  if (!trendConfirmed) {
    return { ok: false, reason: 'Trend not confirmed (token < 0.55)' };
  }
  if (!safetyMargin.ok) {
    return { ok: false, reason: `Safety margin too low (${safetyMargin.marginPct.toFixed(2)}%)` };
  }
  return { ok: true, reason: 'All conditions met' };
}
