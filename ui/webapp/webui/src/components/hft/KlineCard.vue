<script setup lang="ts">
/**
 * K线卡 (E29, kline.history) — the aggregator keys bars by TOKEN id, so the
 * card needs the books view's E29 token-id fields; the page computes
 * `klineReady`/`klineSymbol` and hides the card entirely when the core omits
 * them (or the market type has no OHLC concept). Shares the depth card's
 * asset/token pickers via defineModel — one selection drives both charts.
 */
import type { AssetBook } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import KlineChart from '@/components/charts/KlineChart.vue'

defineProps<{
  /** Gate-cleared, venue-filtered book rows (the asset options). */
  visibleBooks: AssetBook[]
  /** The token id the aggregator keys bars by ('' = hide the card upstream). */
  klineSymbol: string
}>()

/** Selected asset — shared with the depth card's picker. */
const depthAsset = defineModel<string>('asset', { required: true })
/** UP or DOWN token of the selected asset. */
const depthToken = defineModel<'up' | 'down'>('token', { required: true })
</script>

<template>
  <Card class="mt-3.5">
    <CardHeader label="K线">
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
    </CardHeader>
    <div class="mt-2">
      <KlineChart :symbol="klineSymbol" :height="260" />
    </div>
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
