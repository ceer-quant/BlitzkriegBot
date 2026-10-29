<script setup lang="ts">
/**
 * 持仓 / 挂单 / 历史三表（issue 717 拆分）— the tab bodies only: the
 * SegmentedControl, account/venue badges, sweep button and the history filter
 * header stay on the page (they read page-level state); everything rendered
 * inside a tab body arrives as props/callbacks here. 历史表 keyed on
 * tradeIdentity（绝不 t.id —— per-boot 计数器跨 boot 撞键，Vue patch 会错位
 * 复用节点）；瀑布 sentinel 与 加载更多 的 fallback 按钮经 slot 传入（状态
 * 在页面）。
 */
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import type { Position, TradeRow, OrderRow } from '@/api/client'
import { money, signedMoney, signedPct, duration, dateTime } from '@/lib/format'
import { tradeIdentity } from '@/lib/trades'

const props = defineProps<{
  /** 'positions' | 'orders' | 'history' — which tab body to render. */
  tab: string
  positions: Position[]
  openOrderRows: OrderRow[]
  /** 历史页签的可见行（成交行 + 终态死单合成行，已分页）。 */
  visibleRows: TradeRow[]
  /** 未分页的过滤全集（「已加载全部 N 笔」用）。 */
  filteredRows: TradeRow[]
  hasMore: boolean
  /** 分页页长（PAGE），fallback 按钮与「已加载全部」判断共用。 */
  page: number
  venueFiltered: boolean
  /** 行情身份 venue（venue 过滤后的空态文案主语）。 */
  activeVenue: string
  /** 引擎 busy（持仓强平按钮禁用）。 */
  busy: boolean
  orderBusy: boolean
  controlCanStop: boolean
  controlBlockedReason: string | null
  /** 身份所轮长（秒）——挂单过期判定；null/undefined 回落 900（页面同链）。 */
  identityDurSec: number | null
}>()

const emit = defineEmits<{
  /** 持仓行的人工强平（页面走 window.confirm 链）。 */
  forceClose: [p: Position]
  /** 挂单行的强平确认词门入口。 */
  askForceClose: [o: OrderRow]
  /** 加载更多（waterfall fallback 按钮）。 */
  loadMore: []
}>()

// 轮次是否已过期：round_slot N 覆盖 [N·dur, (N+1)·dur)，与内核过期扫描同一
// 公式（公式与回落链的完整注释在页面 identityDurSec）；slot 0（无轮次）不
// 参与（内核同样豁免它）。过期与否只决定按钮可用性 —— 真判定在内核，按钮
// 点了也过不了活轮拒绝。逐单调用的当拍取值 —— venue 变化后下一拍即按新族
// 长算，无需重绑。
function orderExpired(o: OrderRow): boolean {
  const dur = props.identityDurSec ?? 900
  return o.roundSlot > 0 && Date.now() / 1000 >= (o.roundSlot + 1) * dur
}

// 内核对 remaining_sec 恒发数值（过期后为负）；负值 = 市场已结束——显示
// 「已结束」而不是把负秒喂给 duration()（它对负值回落 '—'，看起来像数据
// 缺失）。undefined 只来自老内核。
function positionRemainingLabel(p: Position): string {
  if (p.remainingSec === undefined) return '—'
  return p.remainingSec > 0 ? duration(p.remainingSec) : '已结束'
}
/** 过期持仓的强平不再询问场地（下单通道已关）——改走清除确认语义。 */
function positionExpired(p: Position): boolean {
  return p.remainingSec !== undefined && p.remainingSec <= 0
}

function orderStatusLabel(s: string): string {
  const map: Record<string, string> = {
    PENDING: '待确认', LIVE: '挂单中', PARTIALLY_FILLED: '部分成交',
    FILLED: '已成交', CANCELLED: '已撤销', REJECTED: '已拒绝',
    FAILED: '失败', FORCE_CLOSED: '已强平',
  }
  return map[s] ?? s
}
function orderStatusTone(s: string): 'up' | 'down' | 'default' | 'gold' {
  if (s === 'LIVE' || s === 'PENDING' || s === 'PARTIALLY_FILLED') return 'gold'
  if (s === 'FILLED') return 'up'
  if (s === 'FORCE_CLOSED' || s === 'CANCELLED') return 'down'
  return 'default'
}
function exitReasonTone(reason?: string): 'up' | 'down' | 'default' | 'gold' {
  if (!reason) return 'default'
  const r = reason.toLowerCase()
  if (r.includes('take') || r.includes('profit')) return 'up'
  if (r.includes('stop') || r.includes('loss')) return 'down'
  return 'gold'
}
/** 历史页签的终态死单合成行（orderStatus 在场 = 来源「订单」）：状态徽标
 * 语气沿用挂单页签的 orderStatusTone，轮次照挂单页签的 #slot 形。 */
const orderRowIsSynthetic = (t: TradeRow): boolean => t.orderStatus !== undefined
</script>

<template>
  <!-- positions -->
  <div v-if="tab === 'positions'">
    <div v-if="positions.length" class="overflow-x-auto">
      <table class="w-full min-w-[640px] text-[13px]">
        <thead>
          <tr class="text-left">
            <th class="label-micro px-2 pb-2">资产</th>
            <th class="label-micro px-2 pb-2">方向</th>
            <th class="label-micro px-2 pb-2">场地</th>
            <th class="label-micro px-2 pb-2">策略</th>
            <th class="label-micro px-2 pb-2 text-right">入场</th>
            <th class="label-micro px-2 pb-2 text-right">现价</th>
            <th class="label-micro px-2 pb-2 text-right">份额</th>
            <th class="label-micro px-2 pb-2 text-right">浮动</th>
            <th class="label-micro px-2 pb-2 text-right">剩余</th>
            <th class="label-micro px-2 pb-2 text-right">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="p in positions"
            :key="p.id"
            class="border-t border-line transition-colors hover:bg-panel-2"
          >
            <td class="px-2 py-2.5 font-semibold">{{ p.asset }}</td>
            <td class="px-2 py-2.5">
              <Badge :variant="p.direction === 'up' ? 'up' : 'down'">{{ p.direction.toUpperCase() }}</Badge>
            </td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ p.venue || '—' }}</td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ p.strategy ?? '—' }}</td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(p.entryPrice).toFixed(3)" /></td>
            <td class="px-2 py-2.5 text-right num"><RollingNumber :value="Number(p.currentPrice).toFixed(3)" /></td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="p.shares ?? '—'" /></td>
            <td class="px-2 py-2.5 text-right num font-semibold" :class="p.unrealizedPct >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber :value="`${Number(p.unrealizedPct) >= 0 ? '+' : ''}${Number(p.unrealizedPct).toFixed(2)}%`" />
            </td>
            <td class="px-2 py-2.5 text-right num text-faint-fg">
              <RollingNumber :value="positionRemainingLabel(p)" />
            </td>
            <td class="px-2 py-2.5 text-right">
              <Button
                variant="danger"
                size="sm"
                :disabled="busy || !controlCanStop"
                :title="busy ? '正在下发命令…' : controlCanStop ? (positionExpired(p) ? '市场已结束：按兜底价平账清除（人工审计）' : '人工强行平仓') : (controlBlockedReason ?? '当前网关不允许人工操作')"
                @click="emit('forceClose', p)"
              >
                {{ positionExpired(p) ? '清除' : '强平' }}
              </Button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <EmptyState v-else :text="venueFiltered ? `暂无 ${activeVenue} 的持仓` : '无持仓'" :hint="venueFiltered
      ? `当前账户绑定了 ${activeVenue}，只显示该交易所的持仓。若该所行情未喂入，请到插件页确认插件已启用（插件启用将随账户派生：绑定即启用）。`
      : '启用策略后，成交平仓即出现在这里。'" compact />
  </div>

  <!-- issue 456: 挂单簿 —— 内核记账的订单，逐行强平（确认词门）。
       只显活单：终态死单已被分流去历史页签（裁决「过期轮次订单自动
       进入历史订单」），不再在这里堆尸体充「当前挂单」。 -->
  <div v-else-if="tab === 'orders'">
    <div v-if="openOrderRows.length" class="overflow-x-auto">
      <table class="w-full min-w-[900px] text-[13px]">
        <thead>
          <tr class="text-left">
            <th class="label-micro px-2 pb-2">资产</th>
            <th class="label-micro px-2 pb-2">方向</th>
            <th class="label-micro px-2 pb-2">侧</th>
            <th class="label-micro px-2 pb-2">方式</th>
            <th class="label-micro px-2 pb-2">策略</th>
            <th class="label-micro px-2 pb-2 text-right">价格</th>
            <th class="label-micro px-2 pb-2 text-right">数量</th>
            <th class="label-micro px-2 pb-2 text-right">已成交</th>
            <th class="label-micro px-2 pb-2">轮次</th>
            <th class="label-micro px-2 pb-2">状态</th>
            <th class="label-micro px-2 pb-2 text-right">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="o in openOrderRows"
            :key="o.orderId"
            class="border-t border-line transition-colors hover:bg-panel-2"
          >
            <td class="px-2 py-2.5 font-semibold">{{ o.asset }}</td>
            <td class="px-2 py-2.5">
              <Badge :variant="o.direction === 'up' ? 'up' : 'down'">{{ o.direction.toUpperCase() }}</Badge>
            </td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.side }}</td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.mode }}</td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.strategy || '—' }}</td>
            <td class="px-2 py-2.5 text-right num"><RollingNumber :value="Number(o.price).toFixed(3)" /></td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(o.size).toFixed(1)" /></td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(o.filledSize).toFixed(1)" /></td>
            <td class="px-2 py-2.5 text-[12px] num text-faint-fg">
              <RollingNumber :value="`#${o.roundSlot}`" />
            </td>
            <td class="px-2 py-2.5">
              <Badge :variant="orderStatusTone(o.status)">{{ orderStatusLabel(o.status) }}</Badge>
            </td>
            <td class="px-2 py-2.5 text-right">
              <Button
                variant="danger"
                size="sm"
                :disabled="orderBusy || !orderExpired(o)"
                :title="orderExpired(o)
                  ? `强平 ${o.orderId}（轮次已过期，写 force_close 审计）`
                  : '轮次仍活着，强平被内核拒绝 —— 活单等成交或自然过期'"
                @click="emit('askForceClose', o)"
              >
                强平
              </Button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <EmptyState v-else text="无挂单" hint="策略报出的活单（挂单中/部分成交）会出现在这里；过期活单由内核自动强平归历史，被拒/撤销的死单在历史订单页签。" compact />
  </div>

  <!-- history waterfall -->
  <div v-else>
    <div v-if="visibleRows.length" class="overflow-x-auto">
      <!-- 13 列在手机上不可压成一列一个字符：min-width 让表格保持
           可读列宽、在容器内横向滚动（与 Plugins 页表格同一模式），
           否则 min-content 按"每列一个折行点"计算，数字会竖排。 -->
      <table class="w-full min-w-[1160px] text-[13px]">
        <thead>
          <tr class="text-left">
            <th class="label-micro px-2 pb-2">资产</th>
            <th class="label-micro px-2 pb-2">方向</th>
            <th class="label-micro px-2 pb-2">场地</th>
            <th class="label-micro px-2 pb-2">策略</th>
            <th class="label-micro px-2 pb-2 text-right">入场</th>
            <th class="label-micro px-2 pb-2 text-right">出场</th>
            <th class="label-micro px-2 pb-2 text-right">份额</th>
            <th class="label-micro px-2 pb-2 text-right">费用</th>
            <th class="label-micro px-2 pb-2 text-right">净利</th>
            <th class="label-micro px-2 pb-2 text-right">收益率</th>
            <th class="label-micro px-2 pb-2 text-right">持仓时长</th>
            <th class="label-micro px-2 pb-2">买入时间</th>
            <th class="label-micro px-2 pb-2">卖出时间</th>
            <th class="label-micro px-2 pb-2">原因</th>
          </tr>
        </thead>
        <tbody>
          <!--
            Keyed on the trade's identity, never on `t.id`: the id is a
            per-boot counter (`hft-1` …) and the log is append-only, so the
            first page of 30 rows carried only 14 distinct ids. Duplicate
            keys break Vue's patch algorithm — it reuses the wrong nodes, so
            the table showed rows from the pre-filter list and left stale
            rows behind on 重置.
          -->
          <tr
            v-for="t in visibleRows"
            :key="tradeIdentity(t)"
            class="border-t border-line transition-colors hover:bg-panel-2"
          >
            <td class="px-2 py-2.5 font-semibold">{{ t.asset }}</td>
            <td class="px-2 py-2.5">
              <Badge :variant="t.direction === 'up' ? 'up' : 'down'">{{ t.direction.toUpperCase() }}</Badge>
            </td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ t.venue || '—' }}</td>
            <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ t.strategy ?? '—' }}</td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(t.entryPrice).toFixed(3)" /></td>
            <td class="px-2 py-2.5 text-right num text-muted-fg">
              <!-- 死单合成行从未成交，出场没有值 —— 占位而不是 0.000
                   （0 会冒充「以零价出场」）。 -->
              <RollingNumber v-if="!orderRowIsSynthetic(t)" :value="Number(t.exitPrice).toFixed(3)" />
              <span v-else class="text-faint-fg">—</span>
            </td>
            <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="t.shares" /></td>
            <td class="px-2 py-2.5 text-right num text-down">
              <RollingNumber v-if="!orderRowIsSynthetic(t)" :value="t.feesUsd ? money(t.feesUsd, 3) : '—'" />
              <span v-else class="text-faint-fg">—</span>
            </td>
            <td class="px-2 py-2.5 text-right num font-semibold" :class="Number(t.netPnlUsd) >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber v-if="!orderRowIsSynthetic(t)" :value="signedMoney(t.netPnlUsd, 2)" />
              <span v-else class="text-faint-fg">—</span>
            </td>
            <td class="px-2 py-2.5 text-right num" :class="Number(t.netPnlPct ?? 0) >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber v-if="!orderRowIsSynthetic(t)" :value="signedPct(t.netPnlPct, 2)" />
              <span v-else class="text-faint-fg">—</span>
            </td>
            <td class="px-2 py-2.5 text-right num text-faint-fg">
              <RollingNumber v-if="!orderRowIsSynthetic(t)" :value="t.holdTimeSec !== undefined ? duration(t.holdTimeSec) : '—'" />
              <span v-else class="text-faint-fg">—</span>
            </td>
            <td class="px-2 py-2.5 text-[11.5px] text-faint-fg num">{{ dateTime(t.entryTime) }}</td>
            <td class="px-2 py-2.5 text-[11.5px] text-faint-fg num">{{ dateTime(t.exitTime) }}</td>
            <td class="px-2 py-2.5">
              <!-- 死单合成行：原因列 = 状态徽标 + 轮次 #slot + 来源「订单」；
                   成交行照旧 exitReason。 -->
              <template v-if="orderRowIsSynthetic(t)">
                <Badge :variant="orderStatusTone(t.orderStatus ?? '')">{{ orderStatusLabel(t.orderStatus ?? '') }}</Badge>
                <RollingNumber class="ml-1.5 text-[11px] text-faint-fg" :value="`#${t.orderRoundSlot ?? 0}`" />
                <span class="ml-1.5 text-[10.5px] text-faint-fg">订单</span>
              </template>
              <Badge v-else-if="t.exitReason" :variant="exitReasonTone(t.exitReason)">{{ t.exitReason }}</Badge>
              <span v-else class="text-faint-fg">—</span>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <EmptyState v-else text="无匹配订单" hint="清空搜索或筛选后仍为空，说明该窗口尚无成交、也没有终态订单（被拒/撤销/强平的死单会落在这里）。" compact />

    <!-- waterfall sentinel + fallback button (state lives on the page) -->
    <slot name="waterfall" />
    <div v-if="hasMore" class="mt-3 flex flex-col items-center gap-2">
      <Button variant="outline" size="sm" @click="emit('loadMore')">加载更多</Button>
      <span class="text-[10.5px] text-faint-fg num">
        已显示 <RollingNumber :value="visibleRows.length" /> / <RollingNumber :value="filteredRows.length" />
      </span>
    </div>
    <p v-else-if="filteredRows.length > page" class="mt-3 text-center text-[10.5px] text-faint-fg">
      已加载全部 <RollingNumber :value="filteredRows.length" /> 笔
    </p>
  </div>
</template>
