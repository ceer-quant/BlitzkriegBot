/** Shared shaping for the closed-trade rows the gateway hands the panel. */

/**
 * 持仓过期判定（issue 733 单源）：按钮态（清除 vs 强平）与清除动词分流
 * 共用这一处，两处判据分叉会把同一行送错动词。轮次号匹配（slot N 覆盖
 * [N·dur, (N+1)·dur)），族长优先用行自带的 roundDurationSec（内核按本仓
 * 三态回落解析，与 force_close 门同源）——页面级族长属于身份 venue 的族，
 * 多族部署下对别的族是错钟。行缺轮次号/族长（老内核）回落倒计时符号
 * （remaining_sec 整数除法把到期前不足 1 秒截成 0，边界差由内核门兜住：
 * 两动词都过内核判定，分错只会被拒不会误清）。
 */
export function positionExpired(p: {
  roundSlot?: number
  roundDurationSec?: number
  remainingSec?: number
}, fallbackDurSec: number): boolean {
  if (p.roundSlot !== undefined && p.roundSlot > 0) {
    const dur = p.roundDurationSec && p.roundDurationSec > 0
      ? p.roundDurationSec
      : fallbackDurSec
    return Date.now() / 1000 >= (p.roundSlot + 1) * dur
  }
  return p.remainingSec !== undefined && p.remainingSec <= 0
}

/**
 * The identity of a *trade*, as opposed to the id the row carries.
 *
 * `hft-N` is an in-process counter that restarts at 1 on every boot, and the
 * trade log is append-only across restarts — so the same id names a different
 * trade in every run. Measured on the live log (2026-09-16): 267 rows, 240
 * distinct ids, and `hft-1` alone stood for six different trades (different
 * tokens, entry prices and timestamps). The id is therefore not an identity and
 * must never be used as one.
 *
 * What actually identifies a trade is its own timestamps and asset. Keying on
 * those collapses a genuine re-append of one trade while leaving the distinct
 * trades that merely share a counter value alone.
 */
export function tradeIdentity(r: {
  id: string
  asset?: string
  entryTime?: number
  exitTime?: number
}): string {
  if (r.entryTime !== undefined && r.exitTime !== undefined) {
    return `t:${r.asset ?? ''}:${r.entryTime}:${r.exitTime}`
  }
  // A core older than the timestamp fields cannot be identified by time, so the
  // counter is the only key left. Better a possible over-collapse than treating
  // every row as a fresh trade when the log's rows are not distinguishable.
  return `i:${r.id}`
}

/**
 * Collapse closed-trade rows that describe the SAME trade, keeping the last
 * write. Insertion order is preserved, so the equity curve stays chronological.
 *
 * Keyed on `tradeIdentity`, not on the row's `id`: the id is a per-boot counter
 * (see above), so keying on it deletes real trades. On the live log that was
 * 267 rows collapsing to 240 — 27 trades and $9.24 of net PnL silently dropped
 * from the equity curve, which then ended at $38.72 while the summary printed
 * directly above it said $47.96.
 */
export function dedupeTrades<T extends { id: string }>(rows: readonly T[]): T[] {
  const byTrade = new Map<string, T>()
  for (const r of rows) byTrade.set(tradeIdentity(r), r)
  return [...byTrade.values()]
}

/** When a trade closed, falling back to when it opened (a row always has one). */
function closeStamp(r: { exitTime?: number; entryTime?: number }): number {
  return Number(r.exitTime ?? r.entryTime ?? 0) || 0
}

/**
 * Newest close first — the display ORDER for the history table.
 *
 * The rows arrive in trade-log order, and that log is append-only, so it reads
 * oldest first. On the live log that put 9/14 trades in the first page of 275
 * and left the newest trade nine pages down, which made the table look stale
 * beside the summary printed directly above it. Newest-first is also the
 * direction the waterfall should grow: scrolling down loads older trades.
 *
 * Display-only. The equity curve reads the unsorted list, which has to stay
 * chronological, so never sort `tradeRows` itself.
 */
export function newestFirst<T extends { exitTime?: number; entryTime?: number }>(
  rows: readonly T[],
): T[] {
  return [...rows].sort((a, b) => closeStamp(b) - closeStamp(a))
}
