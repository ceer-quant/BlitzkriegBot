/**
 * The balance the panel presents, and what the core's own cash ledger means next
 * to it.
 *
 * ## The accounting basis
 *
 * Two figures describe the same money and they are NOT the same number:
 *
 *   * **真实余额 = 本金 ＋ 净利润.** The principal is a fixed fact about the run;
 *     the net profit is the closed-trade total *after* fees. Fees therefore
 *     enter this figure the way a cost should — as an expense deducted from
 *     gross profit — and nothing about a fee moves the principal itself.
 *   * **内核现金账 (cash).** The core's `Ledger` moves its cash balance on every
 *     settled fill and charges taker fees to it (`charge_fee`, and the fee
 *     argument of `settle_sell_fill`). It is the collateral the engine may
 *     commit to resting orders, so the panel still needs it — but it is a
 *     bookkeeping quantity, not the answer to "how much money is there".
 *
 * This module makes `本金 ＋ 净利润` the headline and treats the cash ledger as
 * the operative footnote, rather than presenting cash as the balance and
 * flagging every difference from `本金 ＋ 净利润` as a discrepancy. Charging fees
 * into the balance reads as if the fee had changed the principal, which is
 * precisely what an expense must not do.
 *
 * ## Why a gap can exist at all
 *
 * A core that charges fees into cash does land cash on `本金 ＋ 净利润`. When it
 * does not (a core booted before fees were wired into `Ledger`, or one whose
 * maker/taker classification disagrees with the trade record), cash sits above
 * the equity — it has credited gross proceeds and never deducted the fee. That
 * is the core's cash basis being incomplete, so the panel reports the gap as a
 * property of the kernel and keeps 本金 ＋ 净利润 as the trustworthy figure.
 *
 * A missing principal is not drift: without 本金 there is no equity to compare
 * against, so no gap is computed and the panel says so instead of treating the
 * absent principal as zero and reporting the whole balance as an error.
 */
import type { Snapshot } from '@/api/client'

export interface BalanceView {
  /** Starting principal, or null when the core did not report one (LIVE, old core). */
  seed: number | null
  /** Realized net profit over the deduped closed-trade rows — already after fees. */
  net: number
  /** Total fees paid, an expense. Already deducted from `net`, never added to it. */
  fees: number
  /** Realized gross profit: `net + fees`. The figure fees are a cost *against*. */
  gross: number
  /** 本金 ＋ 净利润, the balance the panel shows. Null when the principal is unknown. */
  equity: number | null
  /** The core's cash ledger, gross of local reservations. */
  cash: number
  /** Cash tied up in resting buy orders. */
  reserved: number
  /** Free cash — what the engine can actually commit. */
  available: number
  /**
   * `cash − reserved − equity`. Positive means the core's cash sits above
   * `本金 ＋ 净利润` — typically because it never charged the fees to cash. Null
   * when the principal is unknown, i.e. the comparison is not computable.
   */
  cashGap: number | null
  /** True when the gap is large enough to be a basis difference, not rounding. */
  gapMaterial: boolean
}

/** Gaps under half a cent are rounding, not a basis difference. */
const TOLERANCE = 0.005

export function balanceView(
  balance: Snapshot['balance'] | null | undefined,
  net: number,
  fees: number,
): BalanceView {
  const cash = Number(balance?.balance ?? 0)
  const reserved = Number(balance?.reserved ?? 0)
  const available = Number(balance?.available ?? 0)
  const seed = balance?.seed === null || balance?.seed === undefined ? null : Number(balance.seed)
  const realized = Number(net) || 0
  const paidFees = Number(fees) || 0

  const equity = seed === null ? null : seed + realized
  const cashGap = equity === null ? null : cash - reserved - equity
  return {
    seed,
    net: realized,
    fees: paidFees,
    gross: realized + paidFees,
    equity,
    cash,
    reserved,
    available,
    cashGap,
    gapMaterial: cashGap !== null && Math.abs(cashGap) > TOLERANCE,
  }
}
