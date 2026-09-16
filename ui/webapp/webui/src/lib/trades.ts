/** Shared shaping for the closed-trade rows the gateway hands the panel. */

/**
 * Collapse closed-trade rows by id, keeping the last write per id.
 *
 * `hft-N` is an in-process counter and the trade log is append-only across
 * restarts, so the rows read after a restart can hold both the previous and
 * the current record under the same id. Summing the list raw double-counts
 * those, which is why any aggregate built from rows must run through here.
 * Insertion order is preserved, so the equity curve stays chronological.
 */
export function dedupeTrades<T extends { id: string }>(rows: readonly T[]): T[] {
  const byId = new Map<string, T>()
  for (const r of rows) byId.set(r.id, r)
  return [...byId.values()]
}
