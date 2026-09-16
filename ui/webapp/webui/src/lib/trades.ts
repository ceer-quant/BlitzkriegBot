/** Shared shaping for the closed-trade rows the gateway hands the panel. */

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
