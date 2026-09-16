/**
 * The 模拟余额 / 交易所余额 reconciliation.
 *
 * The balance the core reports is *cash*: principal moved by every settled fill
 * and fee, minus whatever is currently reserved by resting buy orders. The
 * realized-net figure is derived from the closed-trade records. Those two only
 * agree when the running core charges fees to the cash ledger on the same basis
 * the trade records use — commit #75 established that, so a core booted before
 * it (or one whose maker/taker classification differs) will show a residual.
 *
 * This module states the arithmetic and surfaces the residual rather than
 * asserting an identity that may not hold. A non-zero residual is a fact about
 * the core, not a bug in the panel, so it is shown instead of hidden.
 */
import type { Snapshot } from '@/api/client'

export interface BalanceReconciliation {
  /** Cash the core reports. */
  balance: number
  /** Cash tied up in resting buy orders. */
  reserved: number
  /** Free cash. */
  available: number
  /** Starting principal, or null when the core did not report one (LIVE). */
  seed: number | null
  /** Realized net PnL over the deduped closed-trade rows. */
  net: number
  /**
   * `balance − reserved − seed − net`.
   *
   * Zero when the ledger and the trade records agree. Non-zero means the core's
   * cash basis differs from its trade-record basis by this amount; the sign says
   * which way (positive = cash ahead of the records).
   */
  residual: number | null
  /** True when the residual is non-zero and worth calling out. */
  drifted: boolean
}

/** Residuals under half a cent are rounding, not drift. */
const TOLERANCE = 0.005

export function reconcileBalance(
  balance: Snapshot['balance'] | null | undefined,
  net: number,
): BalanceReconciliation {
  const cash = Number(balance?.balance ?? 0)
  const reserved = Number(balance?.reserved ?? 0)
  const available = Number(balance?.available ?? 0)
  const seed = balance?.seed === null || balance?.seed === undefined ? null : Number(balance.seed)
  const realized = Number(net) || 0

  const residual = seed === null ? null : cash - reserved - seed - realized
  return {
    balance: cash,
    reserved,
    available,
    seed,
    net: realized,
    residual,
    drifted: residual !== null && Math.abs(residual) > TOLERANCE,
  }
}
