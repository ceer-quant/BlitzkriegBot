/**
 * 提示音 — 快照差分（issue 781：从 HftPage 的脚本搬出的 domain 逻辑）。
 *
 * 声音合成仍在 ./alertSounds（Web Audio，单上下文 + gain + compressor）；
 * 这里只决定「哪一次快照变化放哪一声」：
 *
 *   盈利单笔 (profit) / 亏损单笔 (dang) —— 成交行计数增长；
 *   新开仓 (order)  —— 持仓行数增长；
 *   熔断 (wuwu) / 风控恢复 (ding) —— 内核的交易冻结状态翻转。
 *
 * 触发面是差分而不是日志重放：首帧只建立基线（重挂载/刷新不会补放历史成交
 * 的声音风暴），此后每类每帧最多三声（批量成交刷屏的上限）。
 */
import { watch, type ComputedRef } from 'vue'
import type { Snapshot, TradeRow } from '@/api/client'
import { playDang, playDing, playOrder, playProfit, playWuwu } from './alertSounds'

/**
 * @param snap      the live snapshot (the page's store mirror)
 * @param tradeRows the deduped closed-trade rows, read at callback time
 */
export function useAlertSounds(
  snap: ComputedRef<Snapshot | null>,
  tradeRows: () => TradeRow[],
): void {
  let primedWins: number | null = null
  let primedLosses: number | null = null
  let primedPositions: number | null = null
  let wasHalted = false

  watch(snap, (s) => {
    if (!s) return
    // Deduped: the raw rows can carry the same `hft-N` id from an earlier run, and
    // counting those twice would fire an alert sound for a trade that never closed.
    const rows = tradeRows()
    const wins = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
    const losses = rows.filter((t) => (Number(t.netPnlUsd) || 0) <= 0).length
    const pos = s.positions?.length ?? 0
    // first frame only establishes the baseline (no replay storm on reload)
    if (primedWins === null || primedLosses === null || primedPositions === null) {
      primedWins = wins; primedLosses = losses; primedPositions = pos
      return
    }
    if (wins > primedWins) for (let i = 0; i < Math.min(wins - primedWins, 3); i++) playProfit()
    if (losses > primedLosses) for (let i = 0; i < Math.min(losses - primedLosses, 3); i++) playDang()
    if (pos > primedPositions) playOrder()
    primedWins = wins; primedLosses = losses; primedPositions = pos

    // Trading halted: the sound's source is the kernel's freeze state
    // (`stats.tradingFrozen`), not `snapshot.lastError` — that one is the
    // GATEWAY's "core not reachable" string, which never contains "breaker", so
    // this pair of sounds could never fire (issue 236 shook this loose). The
    // daily-loss breaker is a DIFFERENT signal (`dailyLoss.tripped`, still not
    // carried through the seam) and would need its own line here if the panel
    // should hear it too.
    const halted = s.stats?.tradingFrozen?.active === true
    if (halted && !wasHalted) playWuwu()
    if (!halted && wasHalted) playDing()
    wasHalted = halted
  })
}
