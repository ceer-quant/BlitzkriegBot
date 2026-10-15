<script setup lang="ts">
/**
 * 插件 — 扩展/行情两类插件的注册表视图（E9-g 插件治理页；E11/D-32：策略不再
 * 作为插件在此显示——策略页是策略的唯一入口，插件页只回答「核心挂了哪些外部
 * 能力」）。身份徽标、状态、活跃行情源。
 *
 * Issue 481：跨所配对视图（`unified.list` 只读）也在本页 —— 已确认的统一
 * 映射是「外部能力之间」的关系（两个行情插件各自盯住同一事件的一侧），
 * 不是任何单一策略的账本数据，落在插件页符合两页分离的契约。venue 一律
 * 数据渲染（no-venue-branch 门禁）。
 */
import { computed, onMounted, ref } from 'vue'
import { Puzzle, Radio, Plug, AlertTriangle, Link2, Power, RefreshCw } from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import { api, marketTypeLabel, type PluginRow, type UnifiedEventRow, type MarketPluginReadoutDoc } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
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

/** Capability-name → 中文标签；未列出的名字原样展示（数据驱动，无 venue 特判）。
 *  `simulated` 是插件自己的声明位：填充为模拟盘的插件声明它，面板据此标注（Issue 454）。 */
const CAPABILITY_LABELS: Record<string, string> = {
  simulated: '模拟盘（第三方）',
}
function capabilityLabel(c: string): string {
  return CAPABILITY_LABELS[c] ?? c
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

// ── 跨所配对视图（issue 481 G4）：unified.list 的只读快照 ────────────────────
const unifiedEvents = ref<UnifiedEventRow[]>([])
const unifiedError = ref<string | null>(null)
const unifiedLoading = ref(false)

/** 配对状态徽标：paired=可对冲，singleLegAvailable=单腿可用（黄，§425 显式
 *  语义：executor 不再按完整对腿行事，面板必须显示它而不是藏起来）。 */
function pairStatus(e: UnifiedEventRow): { tone: 'up' | 'gold' | 'down'; label: string } {
  if (e.status === 'paired') return { tone: 'up', label: '已配对' }
  if (e.status === 'singleLegAvailable') return { tone: 'gold', label: '单腿可用' }
  return { tone: 'down', label: e.status }
}

/** discrepancy 名称 → 操作员可读的短语；未知名原样展示（数据驱动）。 */
const DISCREPANCY_LABELS: Record<string, string> = {
  settlement_source: '结算源不一致',
  settlement_rules: '结算规则冲突',
  expiry_gap: '到期时间差过大',
}
function discrepancyLabel(d: string): string {
  return DISCREPANCY_LABELS[d] ?? d
}

async function loadUnified(): Promise<void> {
  unifiedLoading.value = true
  unifiedError.value = null
  try {
    const doc = await api.unifiedList()
    unifiedEvents.value = doc.events ?? []
    if (doc.error) unifiedError.value = doc.error
  } catch (e) {
    unifiedError.value = e instanceof Error ? e.message : String(e)
  } finally {
    unifiedLoading.value = false
  }
}
onMounted(loadUnified)

// ── issue 485 插件启停/切换 ────────────────────────────────────────────────────────

/** 启停操作的进行中行（名字键控），防重复点击。 */
const toggling = ref<string | null>(null)
/** 启停的结果提示：ok 显示为绿，错误显示为红。 */
const toggleMsg = ref<{ tone: 'ok' | 'err'; text: string } | null>(null)

/** 扩展行的 启用/停用 按钮：直接驱动内核 extension.enable/disable
 *  （on_load / on_unload 在内核里跑），成功后刷新注册表让 state 徽标跟上。 */
async function toggleExtension(r: PluginRow): Promise<void> {
  if (toggling.value) return
  toggling.value = r.name
  toggleMsg.value = null
  try {
    const enable = r.enabled === false
    const res = await api.extensionSet(r.name, enable)
    if (res.error) {
      toggleMsg.value = { tone: 'err', text: res.error }
    } else {
      toggleMsg.value = {
        tone: 'ok',
        text: `${r.name} 已${enable ? '启用' : '停用'}。`,
      }
    }
  } catch (e) {
    toggleMsg.value = {
      tone: 'err',
      text: e instanceof Error ? e.message : String(e),
    }
  } finally {
    toggling.value = null
    await store.refresh()
  }
}

/** 行情插件的读数（market.plugin.get）：下次会话将选什么、是否旗标钉住。 */
const marketDoc = ref<MarketPluginReadoutDoc | null>(null)
const marketBusy = ref(false)
const marketMsg = ref<{ tone: 'ok' | 'err'; text: string } | null>(null)

async function loadMarketPlugin(): Promise<void> {
  marketBusy.value = true
  try {
    marketDoc.value = await api.marketPlugin()
  } catch (e) {
    marketDoc.value = {
      activePlugin: '',
      registered: [],
      nextSessionPlugin: '',
      persisted: false,
      flagPinned: false,
      error: e instanceof Error ? e.message : String(e),
    }
  } finally {
    marketBusy.value = false
  }
}

/** 设为活跃（下次会话）：写入持久格 data/runtime/config.json，重启生效；
 *  显式 --market-plugin 仍 outrank（内核回执与日志都写明）。 */
async function setActivePlugin(name: string): Promise<void> {
  if (marketBusy.value) return
  marketBusy.value = true
  marketMsg.value = null
  try {
    const res = await api.marketPluginSet({
      name,
      reason: 'webui 面板切换（插件页行情插件卡）',
    })
    if (res.error) {
      marketMsg.value = { tone: 'err', text: res.error }
    } else {
      marketMsg.value = {
        tone: 'ok',
        text: `已保存：下次会话（重启内核后）将以 ${res.nextSessionPlugin} 为活跃行情源；本会话不变。`,
      }
      await loadMarketPlugin()
    }
  } catch (e) {
    marketMsg.value = {
      tone: 'err',
      text: e instanceof Error ? e.message : String(e),
    }
  } finally {
    marketBusy.value = false
  }
}

/** 清除持久格：下次会话回落启动解析（显式旗标 > 首个注册者）。 */
async function resetMarketPlugin(): Promise<void> {
  if (marketBusy.value) return
  marketBusy.value = true
  marketMsg.value = null
  try {
    const res = await api.marketPluginSet({
      reset: true,
      reason: 'webui 面板清除（插件页行情插件卡）',
    })
    if (res.error) {
      marketMsg.value = { tone: 'err', text: res.error }
    } else {
      marketMsg.value = { tone: 'ok', text: '已清除：下次会话回落启动解析。' }
      await loadMarketPlugin()
    }
  } catch (e) {
    marketMsg.value = {
      tone: 'err',
      text: e instanceof Error ? e.message : String(e),
    }
  } finally {
    marketBusy.value = false
  }
}

onMounted(loadMarketPlugin)
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
            活跃源：
            <span v-if="activeMarketName" class="font-semibold text-primary">{{ activeMarketName }}</span>
            <span v-else>未选择</span>
          </template>
          <template v-else>核心注册的扩展能力</template>
        </div>
      </div>
    </div>

    <!-- kind filter -->
    <div v-if="sections.length > 1" class="mt-3.5 flex justify-center">
      <SegmentedControl v-model="filter" :segments="segments" size="sm" />
    </div>

    <!-- issue 485 行情插件切换卡（next_session）：active 是启动选择，写入落
         data/runtime/config.json 的 marketPlugin 格，重启生效；显式
         --market-plugin 仍 outrank（旗标钉住的会话写不动）。 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="活跃行情源（下次会话切换）">
        <template #action>
          <Button variant="ghost" size="sm" :disabled="marketBusy" title="刷新读数" @click="loadMarketPlugin">
            <RefreshCw class="size-3.5" :class="marketBusy && 'animate-spin'" />
          </Button>
        </template>
      </CardHeader>
      <div v-if="marketDoc && !marketDoc.error" class="px-3 pb-3">
        <div class="flex flex-wrap items-center gap-2">
          <span class="text-[12px] text-faint-fg">本会话活跃：</span>
          <Badge variant="gold" dot>{{ marketDoc.activePlugin || '—' }}</Badge>
          <template v-if="marketDoc.nextSessionPlugin !== marketDoc.activePlugin">
            <span class="text-[12px] text-faint-fg">下次会话：</span>
            <Badge variant="info" dot>{{ marketDoc.nextSessionPlugin }}</Badge>
          </template>
          <Badge v-if="marketDoc.flagPinned" variant="down">旗标钉住（--market-plugin 优先）</Badge>
          <span v-if="toggling" class="text-[11.5px] text-faint-fg">处理中…</span>
        </div>
        <div class="mt-2.5 flex flex-wrap gap-1.5">
          <Button
            v-for="p in marketDoc.registered"
            :key="p.name"
            :variant="p.name === marketDoc.activePlugin ? 'outline' : 'ghost'"
            size="sm"
            :disabled="marketBusy || p.active || marketDoc.flagPinned"
            :title="p.active
              ? '本会话的活跃源'
              : marketDoc.flagPinned
                ? '本会话由显式 --market-plugin 钉住，写入不动它（下次会话仍按旗标）'
                : `下次会话以 ${p.name} 为活跃行情源（重启生效）`"
            @click="setActivePlugin(p.name)"
          >
            {{ p.name }}
            <Badge v-if="p.active" variant="gold" dot>活跃</Badge>
            <Badge v-else-if="p.name === marketDoc.nextSessionPlugin" variant="info">下次会话</Badge>
          </Button>
          <Button
            v-if="marketDoc.persisted"
            variant="ghost"
            size="sm"
            :disabled="marketBusy"
            title="清除持久格：下次会话回落启动解析（显式旗标 > 首个注册者）"
            @click="resetMarketPlugin"
          >清除格</Button>
        </div>
        <div v-if="marketMsg" class="mt-2 text-[12px]" :class="marketMsg.tone === 'ok' ? 'text-primary' : 'text-down'">
          {{ marketMsg.text }}
        </div>
      </div>
      <div v-else class="px-3 pb-3 text-[12px] text-faint-fg">
        {{ marketDoc?.error ?? '行情源读数不可用（核心未连接时会自动刷新）。' }}
      </div>
    </Card>

    <!-- 跨所配对视图（issue 481 G4）：已确认的统一映射。venue 是数据行。 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="跨所配对（统一映射）">
        <template #action>
          <Badge variant="gold" dot><RollingNumber :value="unifiedEvents.length" /> 映射</Badge>
        </template>
      </CardHeader>
      <div v-if="unifiedEvents.length" class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[680px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">事件</th>
              <th class="label-micro px-2 pb-2.5">状态</th>
              <th class="label-micro px-2 pb-2.5">场地</th>
              <th class="label-micro px-2 pb-2.5">差异标记</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="e in unifiedEvents"
              :key="e.id"
              class="border-t border-line transition-colors hover:bg-panel-2"
            >
              <td class="px-2 py-2.5 font-semibold">
                <span class="inline-flex min-w-0 items-center gap-2">
                  <Link2 class="size-3.5 shrink-0 text-faint-fg" />
                  <span class="truncate" :title="e.title">{{ e.title || e.id }}</span>
                </span>
              </td>
              <td class="px-2 py-2.5">
                <Badge :variant="pairStatus(e).tone" dot>{{ pairStatus(e).label }}</Badge>
              </td>
              <td class="px-2 py-2.5">
                <span class="inline-flex flex-wrap gap-1.5">
                  <!-- 每侧 venue 一个徽标 —— venue 是数据（`v.venue` 原样渲染，
                       不特判任何名字），配对状态跟着该侧的 listing 走。 -->
                  <Badge
                    v-for="v in e.venues"
                    :key="`${e.id}-${v.venue}`"
                    variant="outline"
                  >{{ v.venue }}</Badge>
                </span>
              </td>
              <td class="px-2 py-2.5">
                <span v-if="e.discrepancies.length" class="inline-flex flex-wrap gap-1.5">
                  <Badge v-for="d in e.discrepancies" :key="`${e.id}-${d}`" variant="down">
                    {{ discrepancyLabel(d) }}
                  </Badge>
                </span>
                <span v-else class="text-[12px] text-faint-fg">无</span>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <div v-else class="px-2 pb-2">
        <EmptyState
          :loading="unifiedLoading"
          :text="unifiedError ? '统一映射读取失败' : '暂无已确认的统一映射'"
          :hint="unifiedError
            ? '通常是核心正在重启或未启用发现链路，稍候刷新。'
            : '两个场地的行情插件发现同一事件并确认后，映射会出现在这里；跨所套利策略（cross_venue_arb）只交易这里的映射。'"
        />
      </div>
    </Card>

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
              <th v-if="s.key === 'extension'" class="label-micro px-2 pb-2.5">操作</th>
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
              <td v-if="s.key === 'extension'" class="px-2 py-2.5">
                <!-- issue 485 启用/停用：内核 extension.enable/disable（on_load /
                     on_unload 在内核里跑）；state 徽标在刷新后跟上。 -->
                <Button
                  :variant="r.enabled === false ? 'outline' : 'ghost'"
                  size="sm"
                  :disabled="toggling !== null"
                  :title="r.enabled === false
                    ? `启用 ${r.name}（内核 on_load）`
                    : `停用 ${r.name}（内核 on_unload）`"
                  @click="toggleExtension(r)"
                >
                  <Power class="size-3.5" />
                  {{ r.enabled === false ? '启用' : '停用' }}
                </Button>
              </td>
              <td v-if="s.key === 'market'" class="px-2 py-2.5">
                <span class="inline-flex flex-wrap gap-1.5">
                  <Badge
                    v-for="c in caps(r)"
                    :key="c.label"
                    :variant="c.on ? 'up' : 'default'"
                    :class="c.on ? '' : 'opacity-45'"
                  >{{ c.label }}</Badge>
                  <!-- E27 (§8.3): the plugin's own mode declaration — concrete
                       structure when unanimous, capability seams by name.
                       Neither present = undeclared (implicit type-only mode). -->
                  <Badge v-if="r.structure" variant="info">{{ r.structure }}</Badge>
                  <Badge v-for="c in r.capabilities ?? []" :key="`m-${c}`" variant="outline">{{ capabilityLabel(c) }}</Badge>
                  <Badge
                    v-if="!r.structure && !r.capabilities?.length"
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

    <div v-if="toggleMsg" class="mt-3.5">
      <AlertBanner :tone="toggleMsg.tone === 'ok' ? 'info' : 'error'" :title="toggleMsg.tone === 'ok' ? '操作成功' : '操作失败'">
        {{ toggleMsg.text }}
      </AlertBanner>
    </div>

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
