/**
 * Orders domain — the tracked-order wire (mirror core model.rs TrackedOrder)
 * and the order/position force-close writers.
 */
import { request } from './core.ts'

// ── 挂单卡（issue 456；mirror core model.rs TrackedOrder wire shape）────────

/** `orders.list` 的一行：内核记账的订单，status 是大写枚举（LIVE/FORCE_CLOSED/…）。 */
export interface OrderRow {
  orderId: string
  strategy: string
  asset: string
  direction: string
  tokenId: string
  side: string
  mode: string
  price: number | string
  size: number | string
  filledSize: number | string
  status: string
  roundSlot: number
  /**
   * 订单提交时刻（毫秒）。wire 一直带着（内核 TrackedOrder.submittedAtMs，
   * serde camelCase 直通），此前前端没消费故未声明 —— 终态死单分流历史
   * 页签时要拿它当合成行的时间戳，按需声明即可，老读数不用动。
   */
  submittedAtMs?: number
  /**
   * E28 (§9.2): the account the order belongs to. Empty/absent = pre-E28
   * core — unattributed, shown regardless of the active account.
   */
  accountId?: string
  /**
   * #427-style venue tag. The orders wire does not carry it yet (positions
   * and trades do), so it is always absent today: the venue filter keeps
   * such rows visible — an order must not be silently dropped just because
   * the kernel cannot say which venue it trades. When the kernel starts
   * tagging orders this field lights up with no UI change.
   */
  venue?: string
}

/** `orders.force_close` 请求体：只点名订单，判定（幽灵/活轮）在内核。 */
export interface ForceCloseOrderBody {
  orderId: string
}

/** #684 `positions.force_close` 请求体：只点名持仓，判定（过期/审计门）在内核。 */
export interface ForceClosePositionBody {
  positionId: string
}

/** #684 `positions.force_close` 回执：平账落地的持仓与价格（最后 mid，缺则入场价）。 */
export interface ForceClosePositionResultDoc {
  positionId: string
  price: number | string
}

export const ordersApi = {
  /**
   * 挂单卡的读路径（issue 456）：内核记账的全部订单（orders.list 的
   * TrackedOrder 行）。挂单卡据此渲染，每行按 status 决定是否给强平按钮。
   */
  orders: () => request<{ orders: OrderRow[] }>('/orders'),
  /**
   * 挂单卡的强平写路径（issue 456）：把订单标记为 ForceClosed 并撤场地侧
   * 残留。轮次仍活着的订单内核拒绝（错误信息原样抛回）——强平只兜幽灵，
   * 活单走撤单。每次调用内核写一条 force_close 审计。
   */
  forceCloseOrder: (body: ForceCloseOrderBody) =>
    request<{ success: boolean }>('/orders/force-close', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * #684 当前持仓卡的清除写路径（`positions.force_close`）：只对已过期市场
   * 的持仓可用 —— 场地 updown 列表只载未来轮次，过期 conditionId 的裁决
   * 永远等不到。内核三道门（持仓存在 / 已过期 / manual-ops 审计路径），
   * 活轮持仓拒绝并提示走 positions.exit。按兜底价平账入历史（最后 mid，
   * 缺则入场价 = 成本回收），不建 redemption claim —— 现金真值由既有
   * reconcile/realign sweep 跟平。
   */
  forceClosePosition: (body: ForceClosePositionBody) =>
    request<ForceClosePositionResultDoc>('/positions/force-close', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
