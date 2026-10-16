/**
 * Per-account row filtering (E28 §9.5).
 *
 * The kernel attributes every position/order/trade row with the account it
 * belongs to (`accountId`, forwarded verbatim by the gateway). The panel's
 * connection has ONE session default account (the top-bar switcher), so the
 * pages filter rows locally: only rows of the active account stay.
 *
 * The empty string is a STATE, not a missing value: a pre-E28 core (or a
 * pre-E28 record in a long-lived trade log) writes no `accountId` at all.
 * Those rows are unattributed — nobody can say which account they belong to,
 * so hiding them would silently falsify the book. They always show, whatever
 * the switcher says, and `attributed` reports whether any filtering is
 * actually happening so pages can label the view honestly.
 */

/** One attributed row: the only field the filter reads. */
interface AccountedRow {
  accountId?: string
}

export interface AccountFilterResult<T> {
  /** Rows to render, in input order. */
  rows: T[]
  /**
   * True only when filtering is IN EFFECT: an active account is known AND at
   * least one input row carried a non-empty accountId (i.e. the core speaks
   * E28). A page may label its table "已按账户过滤" only when this is true.
   */
  attributed: boolean
}

/**
 * Filter rows to the active account. `active === null` (switcher not mounted,
 * book not fetched yet) or an empty accountId on the row → keep the row.
 */
export function filterByAccount<T extends AccountedRow>(
  rows: readonly T[],
  active: string | null,
): AccountFilterResult<T> {
  if (!active) return { rows: [...rows], attributed: false }
  const anyAttributed = rows.some((r) => !!r.accountId)
  if (!anyAttributed) return { rows: [...rows], attributed: false }
  return {
    // Unattributed rows (empty accountId) stay visible: pre-E28 cores and
    // pre-E28 log records cannot be filtered and must not be silently dropped.
    rows: rows.filter((r) => !r.accountId || r.accountId === active),
    attributed: true,
  }
}
