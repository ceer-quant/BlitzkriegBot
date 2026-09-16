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
 * The two figures do not share a starting point, so they part company on a
 * restart:
 *
 *   * the core's cash ledger is seeded from `--seed-balance` at every boot
 *     (`service.rs`: `ledger.set_balance(config.dry_seed_balance)`, and
 *     `ipc/server.rs` does the same for an embedded core). It then moves only on
 *     the fills of *that* run — it has no memory of earlier runs;
 *   * `本金 ＋ 净利润` pairs the principal with the persisted all-time net, and
 *     that log survives every restart (`trade_db` reloads `summary.json`).
 *
 * So a core that traded before this boot reports cash covering only this boot's
 * fills, and the gap lands on the net profit accrued before the boot. Measured
 * on this book: cash 1002.06, principal 1000, all-time net 48.21, pre-boot net
 * 46.12, post-boot net 2.09 — the −46.15 gap is the pre-boot profit, not the
 * 38.69 of fees. A core that also never charges fees to cash adds that residue
 * on top of the same gap. Either way the difference is a property of the
 * kernel's bookkeeping basis, not of the balance, so the panel states the gap
 * and keeps 本金 ＋ 净利润 as the trustworthy figure.
 *
 * The gap is SIGNED: a core whose history before this boot lost money reports
 * cash *below* the equity. The direction must therefore be read off the sign
 * (`cashGapDirection`) and never assumed — see `cashGapShort` / `cashGapDetail`.
 *
 * A missing principal is not drift: without 本金 there is no equity to compare
 * against, so no gap is computed and the panel says so instead of treating the
 * absent principal as zero and reporting the whole balance as an error.
 */
import type { Snapshot } from '@/api/client'
// Explicit `.ts`: this module is loaded both by Vite and, verbatim, by the
// `check:balance` script under bare Node, which resolves ESM specifiers
// literally and would fail on an extensionless `./format`.
import { money, signedMoney } from './format.ts'

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
   * `cash − reserved − equity`. Signed: positive means the core's cash sits
   * above `本金 ＋ 净利润`, negative means below. Null when the principal is
   * unknown, i.e. the comparison is not computable.
   */
  cashGap: number | null
  /** True when the gap is large enough to be a basis difference, not rounding. */
  gapMaterial: boolean
  /** Which side of `本金 ＋ 净利润` the cash ledger sits on. Null when not computable. */
  gapDir: CashGapDirection | null
}

/** Which way the core's cash ledger sits relative to `本金 ＋ 净利润`. */
export type CashGapDirection = 'above' | 'below' | 'level'

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
    gapDir: cashGapDirection(cashGap),
  }
}

/**
 * The gap's direction, read off its sign.
 *
 * Callers must never hardcode 「高于」: the gap is a signed difference, and a core
 * whose pre-boot history lost money reports cash *below* `本金 ＋ 净利润`. The
 * live book on 2026-09-16 is exactly that case — cash 1002.06 against an equity
 * of 1048.21, i.e. a −46.15 gap, which a fixed 「高于」 renders as a
 * self-contradiction.
 *
 * Sub-tolerance differences are `level`: rounding, not a basis difference.
 */
export function cashGapDirection(gap: number | null): CashGapDirection | null {
  if (gap === null || !Number.isFinite(gap)) return null
  if (gap > TOLERANCE) return 'above'
  if (gap < -TOLERANCE) return 'below'
  return 'level'
}

/**
 * The inline clause that follows the cash figure — 「高于真实余额 −$46.15」,
 * 「低于真实余额 −$46.15」, or 「与真实余额一致」. Empty when the comparison is not
 * computable, so a caller can render it unconditionally.
 */
export function cashGapShort(v: BalanceView): string {
  if (v.gapDir === null || v.cashGap === null) return ''
  if (v.gapDir === 'level') return '与真实余额一致'
  return `${v.gapDir === 'above' ? '高于' : '低于'}真实余额 ${signedMoney(v.cashGap)}`
}

/**
 * The full explanation behind `cashGapShort`, for a tooltip.
 *
 * States the mechanism the panel can actually verify — the two figures have
 * different starting points, so a restart splits them — and names the fee case
 * as a secondary contributor rather than the cause. The earlier copy asserted
 * the fee explanation outright and read 「差额≈未入账的手续费 $38.69」 next to a
 * −$46.15 gap, which was wrong in both mechanism and sign.
 */
export function cashGapDetail(v: BalanceView): string {
  if (v.gapDir === null || v.cashGap === null || v.equity === null) return ''
  if (v.gapDir === 'level') {
    return (
      `内核现金账 ${money(v.cash)} 与「本金 ＋ 净利」${money(v.equity)} 一致：` +
      `内核本次启动累计的现金，与全部历史净利吻合。`
    )
  }
  const side = v.gapDir === 'above' ? '高于' : '低于'
  return (
    `内核现金账 ${money(v.cash)} ${side}「本金 ＋ 净利」${money(v.equity)}，差额 ${signedMoney(v.cashGap)}。` +
    `两者的统计起点不同：内核现金账每次启动都从本金重新累计，只含本次启动之后的成交；` +
    `真实余额的净利来自全部历史成交。差额因而≈本次启动之前的历史净利，` +
    `若内核未把手续费计入现金也会落在同一差额里。` +
    `引擎下单能力看的是现金账，真实余额看的是本金＋净利。`
  )
}
