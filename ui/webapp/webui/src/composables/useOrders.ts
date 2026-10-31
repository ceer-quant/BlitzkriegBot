/**
 * 挂单簿与强平动作（issue 781：从 HftPage 的脚本搬出的 domain 逻辑）。
 *
 * 行源与筛选：snapshot.orders → 账户过滤 → venue 过滤（与持仓/历史同一
 * 纪律：无归属的旧行仍显示）；终态死单合成为历史页签的行。动作面：单笔
 * 强平（确认词门）与批量清理过期挂单，逐单调内核、逐单写审计。
 *
 * 页面仍持有轮长回落链与过期谓词（identityDurSec / orderExpired）—— 它们
 * 的形状被 account-rounds 门禁逐字钉住；过期行由页面算好后以 getter 传进
 * 来，批量清理只消费它（点击时才读，无创建序耦合）。
 */
import { computed, ref, type ComputedRef } from 'vue'
import { api, type OrderRow, type Snapshot, type TradeRow } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { filterByAccount, filterByVenue } from '@/lib/account-filter'

export function useOrders(
  snap: ComputedRef<Snapshot | null>,
  activeVenue: ComputedRef<string>,
  expiredOrderRows: () => OrderRow[],
) {
  const store = usePanelStore()

  const orderRows = computed<OrderRow[]>(() => {
    // snapshot 里已带 orders（网关从 orders.list 拉的同一份）；不在时回落
    // 到专用读路径，两边都是内核的 TrackedOrder wire 形状。
    // E28 (§9.5) + issue 561: rows are scoped to the display account — same
    // discipline as positions/history (pre-E28 unattributed rows stay visible).
    const fromSnap = (snap.value as unknown as { orders?: OrderRow[] } | null)?.orders
    return filterByVenue(
      filterByAccount(fromSnap ?? [], store.activeAccount).rows,
      activeVenue.value,
    ).rows
  })
  // 内核 is_live 的三态（service.rs 活单判定同口径）：挂单页签只显活单 ——
  // 终态死单（REJECTED/FAILED/FORCE_CLOSED/CANCELLED/FILLED）不再堆在挂单
  // 表里当「当前挂单 N」充数。FORCE_CLOSED 的活单走内核 run_expiry_sweep
  // 自动收（reason="round expired"），强平按钮保留作手动兜底；死单全部
  // 分流到历史页签（terminalOrderRows），挂单页签的徽标 = 真挂单数。
  const isLiveOrder = (o: OrderRow): boolean =>
    o.status === 'LIVE' || o.status === 'PARTIALLY_FILLED' || o.status === 'PENDING'
  const openOrderRows = computed(() => orderRows.value.filter(isLiveOrder))

  // ── 终态死单自动落历史页签（裁决「过期轮次订单自动进入历史订单」）───────
  // 历史页签的行源是 trades（成交流水），从未成交的死单在里面没有行 —— 35 行
  // REJECTED/FORCE_CLOSED 曾经只能在挂单页签烂着。这里把 orderRows 里的终态
  // 行合成 TradeRow 形状补进历史页签：
  //   * 只并四种「无成交终态」（REJECTED/FAILED/FORCE_CLOSED/CANCELLED）。
  //     FILLED 天然有成交，trades 里已有（或将有）对应行，并进去必然重复，
  //     故不合成 —— 这就是去重：按状态语义划界，不做 (asset, side, price,
  //     size, submittedAtMs) 的近似匹配（那是在给两个独立台账硬造一致性）。
  //   * 合成行是「未成交」不是「亏了零块钱」：净利/收益率/费用/出场全占位
  //     （—），字段给 0 只为 TradeRow 形状完整，盈亏统计（cumStats /
  //     filteredStats / winBands / 权益曲线）全部只吃 historyRows（成交行），
  //     口径不动 —— 历史徽标（cumStats.total）也不改，两口径差异是既有语义。
  //   * entryTime = submittedAtMs（wire 一直带，见 OrderRow 注释），exitTime
  //     不设：时间筛选与 newestFirst 都回落 entryTime（lib/trades 同一处理），
  //     买入时间 = 提交时刻、卖出时间 = —（从未出场，诚实）；更重要的是
  //     tradeIdentity 的键——双时间戳齐会按 (asset, entry, exit) 合成键，同毫秒
  //     批量提交的两单会撞键（Vue patch 错乱），缺 exitTime 落 i:order:<orderId>
  //     的 id 键，orderId 内核唯一，天然不撞。
  //   * strategy/venue 照抄订单行：历史页签的资产/策略筛选器对合成行照常
  //     生效；「结果」筛选（仅盈利/仅亏损）按净利 0 把合成行划进「亏损」
  //     桶的边界之外（pnl > 0 才算盈），「全部」下总能看到。
  const terminalOrderRows = computed<TradeRow[]>(() =>
    orderRows.value
      .filter((o) => o.status === 'REJECTED' || o.status === 'FAILED' || o.status === 'FORCE_CLOSED' || o.status === 'CANCELLED')
      .map((o) => ({
        id: `order:${o.orderId}`,
        strategy: o.strategy || undefined,
        asset: o.asset,
        direction: o.direction,
        entryPrice: Number(o.price) || 0,
        exitPrice: 0,
        shares: Number(o.size) || 0,
        netPnlUsd: 0,
        feesUsd: 0,
        entryTime: o.submittedAtMs,
        venue: o.venue,
        orderStatus: o.status,
        orderRoundSlot: o.roundSlot,
      })),
  )

  /**
   * E28 (§9.5): the account these 挂单/历史 rows belong to. issue 561: that
   * account is the DISPLAY account (面板唯一可信源) — the switcher's
   * display.account.set moves that context, the next poll re-pulls everything
   * through it, and the rows are filtered locally (lib/account-filter) to the
   * display account. `filtered` is true only when the kernel actually
   * attributes rows (E28): on a pre-E28 core every row is unattributed and
   * nothing is filtered — the label below says 范围, not 已过滤, accordingly.
   * Null id = book never fetched.
   */
  const accountBadge = computed(() => {
    const id = store.activeAccount
    if (!id) return ''
    const row = store.accounts.find((a) => a.id === id)
    return row?.name ? `${id} · ${row.name}` : id
  })
  const accountFiltered = computed(() =>
    accountBadge.value !== '' &&
    (filterByAccount(snap.value?.positions ?? [], store.activeAccount).attributed ||
      filterByAccount(store.tradeRows, store.activeAccount).attributed),
  )
  // venue 层标注：display 账户绑定了 venue 且行数据带 venue 标签时，过滤真的
  // 在生效 —— 标注要说清「过滤器在哪、为什么在」（显示账户绑定，不是独立控件）。
  const venueFiltered = computed(() =>
    activeVenue.value !== '' &&
    (filterByVenue(snap.value?.positions ?? [], activeVenue.value).attributed ||
      filterByVenue(store.tradeRows, activeVenue.value).attributed),
  )

  const orderBusy = ref(false)
  const orderErr = ref<string | null>(null)
  const orderMsg = ref<string | null>(null)
  const forceOpen = ref(false)
  const forceTarget = ref<OrderRow | null>(null)
  const sweepOpen = ref(false)

  function askForceClose(o: OrderRow): void {
    orderErr.value = null
    forceTarget.value = o
    forceOpen.value = true
  }

  async function doForceClose(): Promise<void> {
    const target = forceTarget.value
    if (!target || orderBusy.value) return
    orderBusy.value = true
    orderErr.value = null
    try {
      await api.forceCloseOrder({ orderId: target.orderId })
      orderMsg.value = `已强平 ${target.orderId}（审计已写）`
      forceOpen.value = false
      await store.refresh()
    } catch (e) {
      orderErr.value = e instanceof Error ? e.message : String(e)
    } finally {
      orderBusy.value = false
      setTimeout(() => { orderMsg.value = null }, 5000)
    }
  }

  /** 批量清理过期挂单：逐单调内核，一单失败不挡下一单（各自审计）。 */
  async function doSweepExpired(): Promise<void> {
    if (orderBusy.value) return
    orderBusy.value = true
    orderErr.value = null
    let closed = 0
    const failed: string[] = []
    try {
      for (const o of expiredOrderRows()) {
        try {
          await api.forceCloseOrder({ orderId: o.orderId })
          closed += 1
        } catch {
          failed.push(o.orderId)
        }
      }
      orderMsg.value = failed.length
        ? `清理完成：${closed} 笔强平，${failed.length} 笔被内核拒绝（${failed.slice(0, 3).join('、')}${failed.length > 3 ? '…' : ''}）`
        : `清理完成：${closed} 笔挂单已强平（各写一条审计）`
      sweepOpen.value = false
      await store.refresh()
    } finally {
      orderBusy.value = false
      setTimeout(() => { orderMsg.value = null }, 6000)
    }
  }

  return {
    orderRows,
    openOrderRows,
    terminalOrderRows,
    accountBadge,
    accountFiltered,
    venueFiltered,
    orderBusy,
    orderErr,
    orderMsg,
    forceOpen,
    forceTarget,
    sweepOpen,
    askForceClose,
    doForceClose,
    doSweepExpired,
  }
}
