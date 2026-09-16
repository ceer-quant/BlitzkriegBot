/**
 * Feed liveness — telling "no new prices" apart from "prices are not moving".
 *
 * A dead market-data feed is invisible by default. The core keeps serving the
 * last orderbook it holds, so every price on screen stays plausible while being
 * hours old, and the counters that would expose it climb anyway: `evaluations`
 * runs off the tick loop, and a round's `ageSec` is derived from the local clock.
 * The page therefore looks healthy and busy while quoting two-hour-old markets.
 *
 * The only counters that move *because* market data moved are the orderbook ones
 * (`books` / `tops` — cumulative book-update counts). Liveness is judged on those
 * alone.
 */

/**
 * How long the book counters may stand still before the feed is called dead.
 *
 * Anchored to the engine's own tolerance rather than picked for feel: the core
 * declares an orderbook too old to trade at `max_orderbook_stale_ms = 8000`
 * (`core/blitzkrieg_core/src/engine.rs`). This sits just above that, so the panel
 * raises the alarm at roughly the point the core stops acting on the data — it
 * would be wrong for the panel to present prices as live that the engine has
 * already stopped trusting.
 *
 * The margin is also enormous against the observed live rate. Over the run that
 * fed this panel, `tops` accumulated ~1.3k updates/second while the feed was up;
 * ten seconds of silence is therefore on the order of 10,000 updates that did not
 * arrive, not a quiet market. Book counters advance on any of the four assets'
 * updates, so no plausible lull reaches this threshold.
 */
export const FEED_STALE_MS = 10_000

/**
 * Identity of the market data observed so far.
 *
 * Returns a value that changes whenever a book update has been counted. Both
 * inputs are cumulative and monotonic, so their sum is too.
 *
 * A round's `slot` is deliberately NOT part of this: it is derived from wall-clock
 * time and so advances every round (15m) with or without a feed, which would reset
 * the staleness clock twice an hour and hide a feed that is down all day.
 */
export function feedProgress(books?: number | null, tops?: number | null): number {
  const n = (v: unknown) => {
    const x = Number(v)
    return Number.isFinite(x) ? x : 0
  }
  return n(books) + n(tops)
}

export interface FeedStaleness {
  /** True once the book counters have stood still past `staleMs`. */
  stale: boolean
  /** Milliseconds since the counters last moved; null when not yet observed. */
  ageMs: number | null
  /**
   * Ready-to-render text for the *outage* state, e.g. `≥ 2h44m 无行情更新`.
   * Only meaningful when `stale`; see `ageLabel` for the always-on indicator.
   */
  label: string
  /**
   * Ready-to-render text for a permanent freshness indicator, valid in both
   * states — `行情 3s 前更新` while alive, `行情已停更 ≥ 11s` once stale.
   * Rendering the age even when healthy is what makes the next outage
   * self-evident: the number stops climbing on its own before anyone thinks to
   * ask. Worded to sit under the outage banner without echoing it.
   */
  ageLabel: string
}

/**
 * Judge liveness from the time the book counters last changed.
 *
 * `lastChangeAtMs` is when *this panel* last saw movement, which is all the wire
 * format supports — the snapshot carries no last-event timestamp. That makes the
 * reported age a lower bound on a real outage (opening the panel onto an
 * already-dead feed reports the time since opening, not the full duration), so the
 * label says "≥" rather than a precise figure.
 */
export function feedStaleness(
  lastChangeAtMs: number | null,
  nowMs: number,
  staleMs: number = FEED_STALE_MS,
): FeedStaleness {
  if (lastChangeAtMs === null) {
    // Nothing observed yet: not enough evidence either way. Reporting "stale"
    // here would cry wolf on every page load, before the first poll lands.
    return { stale: false, ageMs: null, label: '行情状态未知', ageLabel: '行情状态未知' }
  }
  const ageMs = Math.max(0, nowMs - lastChangeAtMs)
  const stale = ageMs > staleMs
  return {
    stale,
    ageMs,
    // The banner's headline; it carries the explanation itself.
    label: stale ? `≥ ${durationShort(ageMs / 1000)} 无行情更新` : '行情更新中',
    // The permanent indicator's text, worded to sit under the banner without
    // repeating it verbatim.
    ageLabel: stale
      ? `行情已停更 ≥ ${durationShort(ageMs / 1000)}`
      : `行情 ${durationShort(ageMs / 1000)} 前更新`,
  }
}

/** `2h44m` / `3m20s` — coarse on purpose, this is an outage notice. */
function durationShort(sec: number): string {
  const s = Math.max(0, Math.floor(sec))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`
}
