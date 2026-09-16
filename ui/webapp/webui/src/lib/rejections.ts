/**
 * Refusal accounting — two disjoint families that one "拒单" label hides.
 *
 * A signal can be stopped in two unrelated places on the way to the book, and the
 * counters do not nest:
 *
 *  - Before an order exists. The engine's timing and momentum gates reject the
 *    candidate inside `Engine::evaluate` (`core/blitzkrieg_core/src/engine.rs`),
 *    and a configured per-strategy cap rejects it in `Core::engine_evaluate`
 *    (`service.rs`). These increment `blockedTiming` / `blockedMomentum` /
 *    `limitRejected` and `continue` before `Core::place` is ever called.
 *  - Inside `Core::place`. Everything downstream of it — the risk gate, kill
 *    switch, loss breaker, position capacity and "already in asset", the SL/exit/
 *    loss/asset cooldowns, ledger reservation, OME duplicate suppression. These
 *    increment `ordersRejected` and nothing else.
 *
 * So `ordersRejected` is NOT the sum of the named buckets, and a page that shows
 * them side by side under one "被风控拒绝" heading invites exactly the wrong
 * reading: tens of thousands of refusals beside four zeroes looks like the risk
 * layer failed to report, when the risk layer is in fact one of the things inside
 * `place()` that can be doing the refusing.
 *
 * One further trap, worth knowing before reading a bare count: `ordersRejected` is
 * counted per `place()` attempt, and the tick loop re-evaluates ~20x/second. A
 * refused candidate is not recorded as pending, so it is re-proposed next tick and
 * refused again — one signal stuck behind a persistent refusal (an open position in
 * the same asset, a full position book, a tripped breaker) accrues roughly 20
 * counts per second for as long as the condition holds. The count is therefore a
 * refusal *rate over time*, not a number of distinct signals.
 */

export interface RefusalInput {
  ordersPlaced?: number | null
  ordersRejected?: number | null
  limitRejected?: number | null
  blockedTiming?: number | null
  blockedMomentum?: number | null
  rejectionCauses?: Record<string, number> | null
}

export interface RefusalProfile {
  /** Orders `place()` accepted. */
  placed: number
  /** Refusals inside `place()`: risk, breaker, positions, cooldowns, ledger, OME. */
  rejected: number
  /** Per-strategy cap refusals, before `place()`. Disjoint from `rejected`. */
  limitRejected: number
  /** Timing-gate blocks. Disjoint from both of the above. */
  timing: number
  /** Momentum-gate blocks. Disjoint from all of the above. */
  momentum: number
  /** Signals that did become orders: `placed + rejected`. */
  admitted: number
  /** Signals stopped before becoming orders: `limitRejected + timing + momentum`. */
  preGate: number
  /** Share of admitted signals that `place()` refused, as a percent (0..100). */
  refuseRate: number
  /**
   * Orders were refused but the core reported no cause buckets. True for a core
   * older than `rejectionCauses` (#65): the refusals are real, their attribution
   * is simply not on the wire. Shown as such rather than as a silent zero.
   */
  causesMissing: boolean
  /** Non-zero cause buckets, largest first. */
  causes: { name: string; n: number }[]
}

const count = (v: unknown): number => {
  const n = Number(v)
  return Number.isFinite(n) ? n : 0
}

export function refusalProfile(r: RefusalInput | null | undefined): RefusalProfile {
  const placed = count(r?.ordersPlaced)
  const rejected = count(r?.ordersRejected)
  const limitRejected = count(r?.limitRejected)
  const timing = count(r?.blockedTiming)
  const momentum = count(r?.blockedMomentum)
  const admitted = placed + rejected
  const causes = Object.entries(r?.rejectionCauses ?? {})
    .map(([name, n]) => ({ name, n: count(n) }))
    .filter((c) => c.n > 0)
    .sort((a, b) => b.n - a.n)

  return {
    placed,
    rejected,
    limitRejected,
    timing,
    momentum,
    admitted,
    preGate: limitRejected + timing + momentum,
    refuseRate: admitted ? (rejected / admitted) * 100 : 0,
    causesMissing: rejected > 0 && causes.length === 0,
    causes,
  }
}

/**
 * Fleet totals. The counters are additive across strategies; `rejectionCauses` is
 * not, so it is left out — a fleet row has no single cause breakdown, and claiming
 * one would misattribute a per-strategy bucket to the whole fleet.
 */
export function refusalTotals(rows: RefusalInput[]): RefusalProfile {
  const sum = (f: (r: RefusalInput) => unknown) => rows.reduce((a, r) => a + count(f(r)), 0)
  return refusalProfile({
    ordersPlaced: sum((r) => r.ordersPlaced),
    ordersRejected: sum((r) => r.ordersRejected),
    limitRejected: sum((r) => r.limitRejected),
    blockedTiming: sum((r) => r.blockedTiming),
    blockedMomentum: sum((r) => r.blockedMomentum),
    rejectionCauses: null,
  })
}

/**
 * Shared explanation for the refusals KPI and column headers. Kept next to the
 * logic it describes so the wording cannot drift away from what the counters mean.
 */
export const REFUSAL_NOTE =
  '两类拒绝互不包含，不能相加。①「下单被拒」发生在下单函数内部：风控门、熔断器、持仓上限、已持有同标的、各类冷却、资金预留、重复单——它们只计入这一项。'
  + '②「时机 / 动量 / 限额」发生在信号变成订单之前，被它们挡下的信号从未进入下单函数，因此是各自独立的计数。'
  + '所以拒单数既不等于、也不包含那三项之和。'
  + '另外拒单是按每 50ms 的评估周期累计的：被拒的候选不会记为挂单中，下一周期会再次尝试并再次被拒，'
  + '因此一个持续被拒的信号每秒约累加 20 次——这个数字是拒绝率随时间的累积，不是不同信号的笔数。'
