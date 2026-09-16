<script setup lang="ts">
/**
 * 插件 — 策略/扩展/行情三类插件的注册表视图（E9-g 插件治理页）：
 * 身份徽标、状态、活跃行情源，以及每个策略的启用开关与实时计数。
 */
import { computed, ref } from 'vue'
import { Boxes, Puzzle, Radio, Plug, AlertTriangle } from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import { marketTypeLabel, type PluginRow } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()

const KIND_META = {
  strategy: { label: '策略插件', icon: Boxes },
  extension: { label: '扩展插件', icon: Puzzle },
  market: { label: '行情插件', icon: Radio },
} as const
type Kind = keyof typeof KIND_META

const sections = computed(() => {
  const p = store.plugins
  if (!p) return []
  return [
    { key: 'strategy' as Kind, rows: p.strategies, active: null as string | null },
    { key: 'extension' as Kind, rows: p.extensions, active: null },
    { key: 'market' as Kind, rows: p.marketPlugins, active: activeMarketName.value },
  ].filter((s) => s.rows.length > 0)
})

/** The gateway's `marketActive` is a boolean flag; the name rides on the row. */
const activeMarketName = computed(
  () => store.plugins?.marketPlugins.find((r) => r.active)?.name ?? null,
)

/** Identity badge: 二元预测市场 / 现货市场 / 合约市场 / 期货实现；未知类型原样展示。 */
function identity(r: PluginRow): string {
  const kval = r.type ?? r.kind
  return marketTypeLabel(kval) || kval || '—'
}

/** Extension lifecycle state from the core registry (`installed`, …). */
const STATE_LABELS: Record<string, string> = {
  installed: '已安装',
  enabled: '已启用',
  disabled: '已停用',
  error: '错误',
}
function stateLabel(s: string): string {
  return STATE_LABELS[s] ?? s
}

type Tone = 'up' | 'down' | 'gold' | 'default'
function status(r: PluginRow): { tone: Tone; label: string } {
  if (r.status === 'error') return { tone: 'down', label: '错误' }
  if (r.enabled === false) return { tone: 'default', label: '停用' }
  return { tone: 'up', label: '正常' }
}

/** Market-plugin capabilities — which seams the source actually implements. */
function caps(r: PluginRow): { label: string; on: boolean }[] {
  return [
    { label: '行情', on: r.hasDataFeed === true },
    { label: '发现', on: r.hasDiscovery === true },
    { label: '下单', on: r.hasExecutor === true },
  ]
}

const totals = computed(() => {
  const p = store.plugins
  return {
    strategies: p?.strategies.length ?? 0,
    extensions: p?.extensions.length ?? 0,
    markets: p?.marketPlugins.length ?? 0,
    enabledStrategies: p?.strategies.filter((r) => r.enabled !== false).length ?? 0,
  }
})

// kind filter: 'all' | Kind
const filter = ref<'all' | Kind>('all')
const visible = computed(() => (filter.value === 'all' ? sections.value : sections.value.filter((s) => s.key === filter.value)))
const segments = computed(() => [
  { id: 'all', label: '全部', badge: sections.value.reduce((a, s) => a + s.rows.length, 0) },
  ...sections.value.map((s) => ({ id: s.key, label: KIND_META[s.key].label, badge: s.rows.length })),
])
</script>

<template>
  <div v-if="store.plugins" class="rise-in">
    <!-- identity strip -->
    <div class="grid gap-3.5 sm:grid-cols-3">
      <div v-for="k in (['strategy', 'extension', 'market'] as Kind[])" :key="k" class="glass card-pad">
        <div class="flex items-center justify-between">
          <span class="label-micro">{{ KIND_META[k].label }}</span>
          <component :is="KIND_META[k].icon" class="size-4 text-primary/70" />
        </div>
        <div class="stat-num mt-2 text-[30px] leading-none">
          <RollingNumber :value="totals[k === 'strategy' ? 'strategies' : k === 'extension' ? 'extensions' : 'markets']" />
        </div>
        <div class="mt-2 text-[11.5px] text-faint-fg">
          <template v-if="k === 'market'">
            活跃源：
            <span v-if="activeMarketName" class="font-semibold text-primary">{{ activeMarketName }}</span>
            <span v-else>未选择</span>
          </template>
          <template v-else-if="k === 'strategy'">
            其中 <span class="font-semibold text-up num"><RollingNumber :value="totals.enabledStrategies" /></span> 个已启用
          </template>
          <template v-else>核心注册的扩展能力</template>
        </div>
      </div>
    </div>

    <!-- kind filter -->
    <div v-if="sections.length > 1" class="mt-3.5 flex justify-center">
      <SegmentedControl v-model="filter" :segments="segments" size="sm" />
    </div>

    <Card v-for="s in visible" :key="s.key" class="mt-3.5" dense>
      <CardHeader :label="KIND_META[s.key].label">
        <template #action>
          <!-- `s.active` is the active plugin's NAME, not a count: plain text. -->
          <Badge v-if="s.active" variant="gold" dot>活跃 {{ s.active }}</Badge>
          <Badge variant="default"><RollingNumber :value="s.rows.length" /></Badge>
        </template>
      </CardHeader>
      <div class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[680px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">名称</th>
              <th class="label-micro px-2 pb-2.5">类型</th>
              <th class="label-micro px-2 pb-2.5">状态</th>
              <th v-if="s.key === 'market'" class="label-micro px-2 pb-2.5">能力</th>
              <th class="label-micro px-2 pb-2.5">说明</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="r in s.rows"
              :key="`${s.key}-${r.name}`"
              class="border-t border-line transition-colors hover:bg-panel-2"
              :class="s.active === r.name && 'bg-primary/[0.06]'"
            >
              <td class="px-2 py-2.5 font-semibold">
                <span class="inline-flex items-center gap-2">
                  <Plug class="size-3.5 text-faint-fg" />
                  {{ r.name }}
                  <Badge v-if="s.active === r.name" variant="gold" dot>活跃</Badge>
                </span>
              </td>
              <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ identity(r) }}</td>
              <td class="px-2 py-2.5">
                <Badge :variant="status(r).tone" dot>{{ status(r).label }}</Badge>
              </td>
              <td v-if="s.key === 'market'" class="px-2 py-2.5">
                <span class="inline-flex gap-1.5">
                  <Badge
                    v-for="c in caps(r)"
                    :key="c.label"
                    :variant="c.on ? 'up' : 'default'"
                    :class="c.on ? '' : 'opacity-45'"
                  >{{ c.label }}</Badge>
                </span>
              </td>
              <td class="px-2 py-2.5 text-[12px] text-muted-fg">
                {{ r.description ?? (r.state ? stateLabel(r.state) : '—') }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>

    <div v-if="store.plugins.lastError" class="mt-3.5">
      <AlertBanner tone="error" title="插件注册表读取失败">{{ store.plugins.lastError }}</AlertBanner>
    </div>

    <Card v-if="!sections.length" class="mt-3.5">
      <EmptyState text="没有已注册的插件" />
    </Card>

    <div v-if="!store.connected" class="mt-3.5">
      <AlertBanner tone="warn" title="内核未连接">
        <span class="inline-flex items-center gap-1.5">
          <AlertTriangle class="size-3.5" />
          列表来自上一次成功读取；启动 blitzkrieg-core 后此处会自动刷新。
        </span>
      </AlertBanner>
    </div>
  </div>

  <Card v-else class="rise-in">
    <EmptyState :loading="store.loading" text="暂无插件数据" />
  </Card>
</template>
