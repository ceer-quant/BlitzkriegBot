<script setup lang="ts">
/**
 * 插件 — 扩展/行情两类插件的注册表状态视图（E9-g 插件治理页；E11/D-32：策略
 * 不再作为插件在此显示——策略页是策略的唯一入口，插件页只回答「核心挂了哪些
 * 外部能力」）。身份徽标、状态（行情按账户绑定自动派生，无全局活跃源）。
 *
 * 绑定即启用（2026-10-16 裁决）：插件启用由账户绑定派生，本页是纯读的状态
 * 视图 —— 启用/停用与行情源选择等写动词已移交（绑定在账户切换器里编辑）。
 * 这里只渲染会话事实：注册状态、模式声明、凭据解析（都不改「哪些插件启用」）。
 *
 * 统一映射（issue 481）不在本页：跨所配对的确认/撤销与目标映射选择，在
 * 策略页「创建套利」弹窗里 —— 插件页只回答「核心挂了哪些外部能力」。venue
 * 一律数据渲染（no-venue-branch 门禁）。
 */
import { computed, ref } from 'vue'
import { Puzzle, Radio, Plug, AlertTriangle } from 'lucide-vue-next'
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
  extension: { label: '扩展插件', icon: Puzzle },
  market: { label: '行情插件', icon: Radio },
} as const
type Kind = keyof typeof KIND_META

const sections = computed(() => {
  const p = store.plugins
  if (!p) return []
  return [
    { key: 'extension' as Kind, rows: p.extensions },
    { key: 'market' as Kind, rows: p.marketPlugins },
  ].filter((s) => s.rows.length > 0)
})


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

/** 注册状态徽标（纯读）：内核注册表的 lifecycle state / enabled 位如实渲染，
 *  不再提供改它的按钮 —— 启用集合由账户绑定派生（绑定即启用）。 */
function status(r: PluginRow): { tone: Tone; label: string } {
  if (r.status === 'error') return { tone: 'down', label: '错误' }
  if (r.enabled === false) return { tone: 'default', label: '未启用' }
  if (r.state === 'disabled') return { tone: 'default', label: '未启用' }
  return { tone: 'up', label: '已注册' }
}

/** Market-plugin capabilities — which seams the source actually implements. */
function caps(r: PluginRow): { label: string; on: boolean }[] {
  return [
    { label: '行情', on: r.hasDataFeed === true },
    { label: '发现', on: r.hasDiscovery === true },
    { label: '下单', on: r.hasExecutor === true },
  ]
}

/** Capability-name → 中文标签；未列出的名字原样展示（数据驱动，无 venue 特判）。
 *  `simulated` 是插件自己的声明位：填充为模拟盘的插件声明它，面板据此标注（Issue 454）。 */
const CAPABILITY_LABELS: Record<string, string> = {
  simulated: '模拟盘（第三方）',
}
function capabilityLabel(c: string): string {
  return CAPABILITY_LABELS[c] ?? c
}

/** 模式声明（E27 §8.3）：插件的 `modes` wire 对象原样转可读串列表；数据驱动，
 *  不特判。渲染成 name×side 摘要，缺字段时回落到 JSON 原样。 */
function modesLabel(m: unknown): string {
  if (m == null) return ''
  if (typeof m === 'string') return m
  if (Array.isArray(m)) return m.map((x) => modesLabel(x)).filter(Boolean).join('、')
  if (typeof m === 'object') {
    const o = m as Record<string, unknown>
    const name = typeof o.name === 'string' ? o.name : ''
    const side = typeof o.side === 'string' ? o.side : ''
    if (name || side) return [name, side].filter(Boolean).join('·')
  }
  return JSON.stringify(m)
}

/** modes → 徽标串列表：数组拆项，单值包一元；null/undefined → 空数组。 */
function modesList(m: unknown): string[] {
  if (m == null) return []
  const items = Array.isArray(m) ? m : [m]
  return items.map((x) => modesLabel(x)).filter(Boolean)
}

const totals = computed(() => {
  const p = store.plugins
  return {
    extension: p?.extensions.length ?? 0,
    market: p?.marketPlugins.length ?? 0,
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
    <div class="grid gap-3.5 sm:grid-cols-2">
      <div v-for="k in (['extension', 'market'] as Kind[])" :key="k" class="glass card-pad">
        <div class="flex items-center justify-between">
          <span class="label-micro">{{ KIND_META[k].label }}</span>
          <component :is="KIND_META[k].icon" class="size-4 text-primary/70" />
        </div>
        <div class="stat-num mt-2 text-[30px] leading-none">
          <RollingNumber :value="totals[k]" />
        </div>
        <div class="mt-2 text-[11.5px] text-faint-fg">
          <template v-if="k === 'market'">
            <!-- 多插件数据面（issue 502）后没有全局「活跃源」：行情按账户绑定
                 自动派生接入，这里只报已注册的行情插件个数。 -->
            已注册 {{ totals.market }} 个行情插件（按账户绑定自动接入）。
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
          <!-- 绑定即启用后没有全局「活跃源」概念（行情按账户绑定派生）；
               徽标只报个数。 -->
          <Badge variant="default"><RollingNumber :value="s.rows.length" /></Badge>
        </template>
      </CardHeader>
      <!-- 绑定即启用（2026-10-16 裁决）：注册表是状态视图，启停写动词已移交。
           每张插件卡都带这行说明，操作员不会在这里找不存在的开关。 -->
      <p class="px-3 pt-2.5 text-[12px] text-muted-fg">
        插件启用由账户绑定派生：在账户切换器里给账户绑定交易所即启用对应插件（绑定即启用）。
      </p>
      <div class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[680px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">名称</th>
              <th class="label-micro px-2 pb-2.5">类型</th>
              <th class="label-micro px-2 pb-2.5">注册状态</th>
              <th v-if="s.key === 'market'" class="label-micro px-2 pb-2.5">模式与能力</th>
              <th class="label-micro px-2 pb-2.5">说明</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="r in s.rows"
              :key="`${s.key}-${r.name}`"
              class="border-t border-line transition-colors hover:bg-panel-2"
            >
              <td class="px-2 py-2.5 font-semibold">
                <span class="inline-flex items-center gap-2">
                  <Plug class="size-3.5 text-faint-fg" />
                  {{ r.name }}
                </span>
              </td>
              <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ identity(r) }}</td>
              <td class="px-2 py-2.5">
                <Badge :variant="status(r).tone" dot>{{ status(r).label }}</Badge>
              </td>
              <td v-if="s.key === 'market'" class="px-2 py-2.5">
                <span class="inline-flex flex-wrap gap-1.5">
                  <Badge
                    v-for="c in caps(r)"
                    :key="c.label"
                    :variant="c.on ? 'up' : 'default'"
                    :class="c.on ? '' : 'opacity-45'"
                  >{{ c.label }}</Badge>
                  <!-- E27 (§8.3): the plugin's own mode declaration — the wire
                       `modes` object verbatim (数据驱动，不特判), plus the
                       concrete structure when unanimous. Neither present =
                       undeclared (implicit type-only mode). -->
                  <Badge v-for="m in modesList(r.modes)" :key="`mode-${m}`" variant="gold">{{ m }}</Badge>
                  <Badge v-if="r.structure" variant="info">{{ r.structure }}</Badge>
                  <Badge v-for="c in r.capabilities ?? []" :key="`m-${c}`" variant="outline">{{ capabilityLabel(c) }}</Badge>
                  <Badge
                    v-if="!r.structure && !r.capabilities?.length && !modesList(r.modes).length"
                    variant="default"
                    class="opacity-60"
                  >未声明</Badge>
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
      <AlertBanner
        tone="error"
        title="插件注册表读取失败"
        hint="通常是核心正在重启，稍候自动刷新；持续出现时检查 blitzkrieg-core 进程。"
      >{{ store.plugins.lastError }}</AlertBanner>
    </div>

    <Card v-if="!sections.length" class="mt-3.5">
      <EmptyState
        text="没有已注册的插件"
        hint="启动核心并加载插件目录后，扩展与行情插件会自动注册到这里。"
      />
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
    <EmptyState
      :loading="store.loading"
      text="暂无插件数据"
      hint="网关在线时会自动拉取；持续为空请确认 blitzkrieg-core 已启动。"
    />
  </Card>
</template>
