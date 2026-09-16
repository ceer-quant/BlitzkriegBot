/**
 * The balance the panel presents.
 *
 * ## The accounting basis
 *
 * **真实余额 = 本金 ＋ 净利润.** The principal is a fixed fact about the run; the
 * net profit is the closed-trade total *after* fees. Fees therefore enter this
 * figure the way a cost should — as an expense deducted from gross profit — and
 * nothing about a fee moves the principal itself. Making that the headline
 * matters: presenting the core's cash ledger as "the balance" reads as if a fee
 * had changed the principal, which is precisely what an expense must not do.
 *
 * ## Why the core's cash ledger is not shown beside it in DRY
 *
 * The core's `Ledger` is a real control — `reserve()` refuses a BUY whose
 * notional exceeds available collateral — and LIVE seeds it from the venue
 * (`extensions/polymarket/src/live.rs`). In DRY it is seeded from
 * `--seed-balance` at every boot (`service.rs`, `ipc/server.rs`) and then moves
 * only on the fills of *that* run, so its value is always `本金 ＋ 本次会话净利`:
 * a strict subset of what the trade log already records, and one that forgets
 * every earlier run.
 *
 * Measured on this book (2026-09-16): cash 1002.06 while the log's all-time net
 * was 48.21, so the −46.15 difference was nothing more than the profit earned
 * before the boot. Comparing the two thus produced a discrepancy the panel had
 * to explain on every page, about a quantity carrying no information the log
 * does not already have — and whose sign flips with the pre-boot history.
 *
 * So the panel drops the comparison in DRY and keeps the cash ledger where it is
 * the answer rather than a footnote: LIVE, where the venue balance is the only
 * figure that exists because the core reports no principal there.
 *
 * A missing principal is not drift: without 本金 there is no equity, so the panel
 * falls back to the cash ledger and labels it as such instead of treating the
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
  /**
   * 本金 ＋ 净利润 — the DRY headline. Null when the core reported no principal
   * (LIVE, or a core older than the field), which is when `cash` takes over.
   */
  equity: number | null
  /** The core's cash ledger, gross of local reservations. The LIVE headline. */
  cash: number
  /** Cash tied up in resting buy orders. */
  reserved: number
  /** Free cash — what the engine can actually commit. */
  available: number
}

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

  return {
    seed,
    net: realized,
    fees: paidFees,
    gross: realized + paidFees,
    equity: seed === null ? null : seed + realized,
    cash,
    reserved,
    available,
  }
}
