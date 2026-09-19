/**
 * The panel's fallback shape for strategy rows, and the provenance label it
 * must NOT invent.
 *
 * Data has three sources, in descending fidelity (`stores/panel.ts` picks the
 * first that is non-empty):
 *   1. `snapshot.strategyStats` — `engine.stats`, which carries `source`.
 *   2. `snapshot.stats.strategies` — the same rows, older cores.
 *   3. `/api/plugins` — `strategy.list`, which is `{name, enabled}` ONLY.
 *
 * Path 3 has no provenance at all. The panel used to fill the gap with
 * `row.kind ?? 'builtin'`, which is wrong twice over:
 *
 *   - `kind` is the *market class* badge on a plugin row (spot/futures/options —
 *     see `pluginKindLabel`), never a strategy-provenance field; and
 *   - the `'builtin'` default was a fabrication. Since PR-B (#114) the kernel
 *     ships ZERO strategy implementations, so a builtin is a thing that no
 *     longer exists: every strategy arrives as a cdylib and reports
 *     `source = "dylib:<path>"`. Labelling unknown provenance `builtin`
 *     therefore misreports an external strategy as in-tree — the exact opposite
 *     of the truth, for every strategy.
 *
 * So an unknown source is reported as unknown. `—` is this codebase's existing
 * "not available" convention (`App.vue`, `RollingNumber.vue`).
 */
import type { PluginRow, StrategyStatsRow } from '../api/client'

/** This codebase's "no value available" marker. */
export const UNKNOWN_SOURCE = '—'

/**
 * Provenance for a registry row. Returns the row's own `source` when it has one
 * (the `engine.stats` paths), and `UNKNOWN_SOURCE` otherwise — never a guess.
 *
 * Deliberately does NOT read `kind`: on a strategy row that field would be a
 * market class, and reporting a market class as provenance would be a new
 * version of the same defect.
 */
export function strategySource(source: unknown): string {
  return typeof source === 'string' && source.trim() !== '' ? source : UNKNOWN_SOURCE
}

/**
 * Registry-only rows (`strategy.list`) as the strategy table's row shape. The
 * counters are genuinely absent on this path — the registry does not carry them —
 * so they are zero; only `source` would otherwise have been fabricated, and it is
 * now honest.
 */
export function registryStrategyRows(rows: readonly PluginRow[]): StrategyStatsRow[] {
  return rows.map((r) => ({
    name: r.name,
    enabled: r.enabled ?? false,
    source: strategySource(r.source),
    ordersPlaced: 0,
    ordersRejected: 0,
    limitRejected: 0,
    blockedTiming: 0,
    blockedMomentum: 0,
    gateExemptedTiming: 0,
    gateExemptedMomentum: 0,
    gateExemptions: [],
    gateExemptionTimingFloorSec: null,
    closedTrades: 0,
    wins: 0,
    losses: 0,
    netPnlUsd: 0,
    rejectionCauses: null,
  }))
}
