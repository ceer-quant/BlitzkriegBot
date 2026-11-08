/**
 * The balance the panel presents.
 *
 * ## The accounting basis
 *
 * **真实余额 = 账本现金余额。** 内核账本（seed 入金后累计每一笔成交）
 * 是权威来源，面板不再从成交折叠语料派生净利，避免账本与交易日志
 * 口径分叉导致同一账户在不同卡片显示不同数字的问题。
 *
 * Fees are an expense, so they are shown as a cost charged against gross profit
 * — never folded into the balance, which would make the fee look like it moved
 * the principal.
 */
import type { Snapshot } from '@/api/client'

export interface BalanceView {
  /** Starting principal, or null when the core did not report one (LIVE, old core). */
  seed: number | null
  /** Realized net profit = cash − principal. When there is no principal (LIVE,
   * old core), net equals cash (all cash is profit since there is no principal
   * to subtract). */
  net: number
  /** Total fees paid, an expense. 0 while gate-keeping on the frozen balance sheet. */
  fees: number
  /** Realized gross profit: equals `cash` (net + fees with fees = 0). */
  gross: number
  /** 真实余额 = 账本现金余额。Always equals `cash`. */
  equity: number
  /** The core's cash ledger, gross of local reservations. The LIVE headline. */
  cash: number
  /** Cash tied up in resting buy orders. */
  reserved: number
  /** Free cash — what the engine can actually commit. */
  available: number
}

export function balanceView(
  balance: Snapshot['balance'] | null | undefined,
): BalanceView {
  const cash = Number(balance?.balance ?? 0)
  const reserved = Number(balance?.reserved ?? 0)
  const available = Number(balance?.available ?? 0)
  const seed = balance?.seed === null || balance?.seed === undefined ? null : Number(balance.seed)
  const net = seed === null ? cash : cash - seed

  return {
    seed,
    net,
    fees: 0,
    gross: cash,
    equity: cash,
    cash,
    reserved,
    available,
  }
}