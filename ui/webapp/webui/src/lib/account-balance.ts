/**
 * 账户级现金流水账本的影子。
 *
 * 余额 = 账本现金（`row.balance`），净利 = balance − seededTotal（账本口径，
 * 构造上恒等）。这是全面板余额/净利显示的唯一来源，绝不参杂
 * 成交折叠口径（trade fold `netPnlUsd` 只用于权益曲线/胜率/手续费等
 * 「成交口径」展示，不参与余额拼接）。
 *
 * 操作员裁决 2026-11-08：账本流水=唯一真相，面板不再拿成交行的
 * 模型费净利去拼一个可能两头都不是的余额。
 *
 * 返回 `null` 时表示没有账户行可读，调用方自行决定兜底（快照顶层
 * balance 或保持空态），绝不自行编造一个口径。
 */
import type { BalanceView } from './balance'
import { balanceView } from './balance'
import type { PanelAccountRow } from '@/stores/panel'

export function useAccountBalance(
  row: PanelAccountRow | null,
): BalanceView | null {
  if (!row) return null

  const cash = Number(row.balance) || 0
  const seed = row.seededTotal !== undefined ? Number(row.seededTotal) || 0 : null
  const net = seed === null ? cash : cash - seed

  return balanceView(
    {
      balance: cash,
      reserved: Number(row.reserved) || 0,
      available: Number(row.available) || 0,
      seed,
    },
    net,
    0,
  )
}
