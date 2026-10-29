<script setup lang="ts">
/**
 * 盘口深度卡 (E8-c) — the selected asset/token's L2 depth: touch metrics
 * first (best bid/ask, spread, OBI, cumulative depth), the BookDepth chart
 * below. Shares the asset/token selection with the K-line card via
 * defineModel — one selection drives both charts (E29 pairing). Null metrics
 * mean the feed has not delivered this token's book — never a zero quote.
 * 行只吃页面的 visibleBooks（Loading 门清空 + venue 过滤后的行），组件不
 * 接触原始快照 —— 行情面全部消费可见行（hft-venue-switch 门禁语义）。
 */
import { computed } from 'vue'
import type { AssetBook, BookSide } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import BookDepth from '@/components/charts/BookDepth.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import { cents, signedPct } from '@/lib/format'

const props = defineProps<{
  /** Gate-cleared, venue-filtered book rows. */
  visibleBooks: AssetBook[]
  /** Whether this core serves depth at all (engine.books present). */
  hasDepthData: boolean
  /** 页面侧的选中账本（K线判定也要用 —— 单一派生留在页面）。 */
  activeBook: AssetBook | null
}>()

/** Selected asset — shared with the K-line card's picker. */
const depthAsset = defineModel<string>('asset', { required: true })
/** UP or DOWN token of the selected asset. */
const depthToken = defineModel<'up' | 'down'>('token', { required: true })

const activeSide = computed<BookSide | null>(() => {
  const b = props.activeBook
  if (!b) return null
  return depthToken.value === 'up' ? b.up : b.down
})

const depthMetrics = computed(() => {
  const s = activeSide.value
  if (!s) return null
  const sum = (xs: { size: number }[]) => xs.reduce((a, l) => a + (Number(l.size) || 0), 0)
  return {
    bestBid: s.bestBid,
    bestAsk: s.bestAsk,
    spread: s.spread,
    obi: s.obi,
    bidDepth: sum(s.bids),
    askDepth: sum(s.asks),
  }
})
</script>

<template>
  <Card v-if="visibleBooks.length" class="mt-3.5">
    <CardHeader label="盘口深度">
      <template #action>
        <div class="flex items-center gap-2">
          <select v-model="depthAsset" class="filter-select">
            <option v-for="b in visibleBooks" :key="`${b.venue ?? ''}:${b.asset}`" :value="b.asset">{{ b.asset }}</option>
          </select>
          <SegmentedControl
            v-model="depthToken"
            :segments="[
              { id: 'up', label: 'UP' },
              { id: 'down', label: 'DOWN' },
            ]"
            size="sm"
          />
        </div>
      </template>
      <template #title>
        <span class="flex items-center gap-1.5">
          <span class="truncate text-[13px] font-bold tracking-wide">{{ depthAsset }}</span>
        </span>
      </template>
    </CardHeader>
    <!--
      The touch metrics first: a depth ladder is read from the spread and the
      imbalance before any level is looked at. Null means the feed has not
      delivered this token's book yet — never a zero quote.
    -->
    <div
      v-if="depthMetrics && activeSide"
      class="flex flex-wrap items-center gap-x-5 gap-y-1.5 text-[11.5px]"
    >
      <span>买一 <span class="num text-up"><RollingNumber :value="depthMetrics.bestBid != null ? cents(depthMetrics.bestBid) : '—'" /></span></span>
      <span>卖一 <span class="num text-down"><RollingNumber :value="depthMetrics.bestAsk != null ? cents(depthMetrics.bestAsk) : '—'" /></span></span>
      <span>点差 <span class="num"><RollingNumber :value="depthMetrics.spread != null ? cents(depthMetrics.spread) : '—'" /></span></span>
      <span>盘口失衡 OBI <span class="num" :class="(depthMetrics.obi ?? 0) >= 0 ? 'text-up' : 'text-down'"><RollingNumber :value="depthMetrics.obi != null ? signedPct(depthMetrics.obi * 100, 1) : '—'" /></span></span>
      <span class="text-faint-fg">累计买深 <span class="num text-fg"><RollingNumber :value="depthMetrics.bidDepth.toFixed(1)" /></span> · 卖深 <span class="num text-fg"><RollingNumber :value="depthMetrics.askDepth.toFixed(1)" /></span> 份</span>
    </div>
    <div class="mt-2">
      <BookDepth :side="activeSide" :height="168" />
    </div>
  </Card>
  <Card v-else-if="!hasDepthData" class="mt-3.5">
    <EmptyState text="盘口深度暂不可用" hint="当前内核未提供 engine.books——重启到新内核后自动出现。" compact />
  </Card>
</template>

<style scoped>
.filter-select {
  height: 28px;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: var(--panel-2);
  color: var(--fg);
  font-size: 11.5px;
  font-family: inherit;
  padding: 0 8px;
  outline: none;
  transition: border-color 0.15s;
}
.filter-select:hover { border-color: var(--line-strong); }
.filter-select:focus { border-color: oklch(0.78 0.16 68 / 0.55); }
.filter-select option { background: var(--panel-solid); color: var(--fg); }
</style>
