<script setup lang="ts">
/**
 * 插件 — 扩展/行情两类插件的注册表状态视图（E9-g 插件治理页；E11/D-32：策略
 * 不再作为插件在此显示——策略页是策略的唯一入口，插件页只回答「核心挂了哪些
 * 外部能力」）。身份徽标、状态、活跃行情源。
 *
 * 绑定即启用（2026-10-16 裁决）：插件启用由账户绑定派生，本页是纯读的状态
 * 视图 —— 启用/停用与行情源选择等写动词已移交（绑定在账户切换器里编辑）。
 * 这里只渲染会话事实：注册状态、模式声明、凭据解析；写动词只剩凭据录入与
 * 统一映射确认（都不改「哪些插件启用」）。
 *
 * Issue 481：跨所配对视图（`unified.list` 只读）也在本页 —— 已确认的统一
 * 映射是「外部能力之间」的关系（两个行情插件各自盯住同一事件的一侧），
 * 不是任何单一策略的账本数据，落在插件页符合两页分离的契约。venue 一律
 * 数据渲染（no-venue-branch 门禁）。
 */
import { computed, onMounted, ref } from 'vue'
import { Puzzle, Radio, Plug, AlertTriangle, Link2 } from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import { api, marketTypeLabel, type PluginRow, type UnifiedEventRow, type UnifiedCandidateRow } from '@/api/client'
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

// ── ④ 统一映射候选：自动匹配只提议，操作员 confirm 才生效 ─────────────────
const unifiedCandidates = ref<UnifiedCandidateRow[]>([])
const unifiedCandidatesLoading = ref(false)
/** 进行中的确认/撤销行（candidate 索引 / event id 键控），防重复点击。 */
const unifiedBusy = ref<string | null>(null)
const unifiedMsg = ref<{ tone: 'error' | 'info'; text: string } | null>(null)

async function loadUnifiedCandidates(): Promise<void> {
  unifiedCandidatesLoading.value = true
  try {
    const doc = await api.unifiedCandidates()
    unifiedCandidates.value = doc.candidates ?? []
  } catch {
    // 候选架是附加视图：读失败静默（主表格的错误已经有 unifiedError 承载）。
    unifiedCandidates.value = []
  } finally {
    unifiedCandidatesLoading.value = false
  }
}

/** 确认一个候选：标题用两侧相同的 question（ledger 仍会重新验证相似度），
 *  listings 原样回传。成功后同时刷新候选架与已确认映射。 */
async function confirmCandidate(c: UnifiedCandidateRow, idx: number): Promise<void> {
  if (unifiedBusy.value) return
  unifiedBusy.value = `cand-${idx}`
  unifiedMsg.value = null
  try {
    const title = c.a.title || c.b.title
    const doc = await api.unifiedConfirm({ title, listings: [c.a, c.b] })
    if (doc.error) {
      unifiedMsg.value = { tone: 'error', text: doc.error }
    } else {
      unifiedMsg.value = { tone: 'info', text: `已确认映射「${doc.event?.title ?? title}」。` }
    }
  } catch (e) {
    unifiedMsg.value = { tone: 'error', text: e instanceof Error ? e.message : String(e) }
  } finally {
    unifiedBusy.value = null
    await Promise.all([loadUnified(), loadUnifiedCandidates()])
  }
}

/** 撤销一个已确认映射：账本落审计、引擎视图同步拔除。 */
async function revokeEvent(e: UnifiedEventRow): Promise<void> {
  if (unifiedBusy.value) return
  unifiedBusy.value = `evt-${e.id}`
  unifiedMsg.value = null
  try {
    const doc = await api.unifiedRevoke({ eventId: e.id })
    if (doc.error) {
      unifiedMsg.value = { tone: 'error', text: doc.error }
    } else {
      unifiedMsg.value = { tone: 'info', text: `已撤销映射「${e.title || e.id}」。` }
    }
  } catch (err) {
    unifiedMsg.value = { tone: 'error', text: err instanceof Error ? err.message : String(err) }
  } finally {
    unifiedBusy.value = null
    await Promise.all([loadUnified(), loadUnifiedCandidates()])
  }
}

onMounted(loadUnifiedCandidates)


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

    <!-- 跨所配对视图（issue 481 G4）：已确认的统一映射。venue 是数据行。 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="跨所配对（统一映射）">
        <template #action>
          <Badge variant="gold" dot><RollingNumber :value="unifiedEvents.length" /> 映射</Badge>
        </template>
      </CardHeader>
      <!-- ④ 候选提议架：自动匹配只提议、操作员 confirm 才生效。列表为空 =
           本轮没有 ≥2 个已声明 venue 的候选（单插件世界常态），不显示区块。 -->
      <div v-if="unifiedCandidates.length" class="mx-2 mb-2 rounded-lg border border-line bg-panel-2/40 p-2.5">
        <div class="mb-1.5 flex items-center gap-2">
          <span class="label-micro">待确认候选</span>
          <Badge variant="info" dot>{{ unifiedCandidates.length }}</Badge>
          <span class="text-[11px] text-faint-fg">两个场地发现了疑似同一事件 —— 确认后映射才生效</span>
        </div>
        <div
          v-for="(c, i) in unifiedCandidates"
          :key="`cand-${i}`"
          class="flex flex-wrap items-center gap-2 border-t border-line/60 py-2 first:border-t-0"
        >
          <span class="min-w-0 flex-1 truncate text-[12px]" :title="c.a.title || c.b.title">
            {{ c.a.title || c.b.title }}
          </span>
          <Badge variant="outline">{{ c.a.venue }}</Badge>
          <Link2 class="size-3 text-faint-fg" />
          <Badge variant="outline">{{ c.b.venue }}</Badge>
          <span class="text-[11px] text-faint-fg">分 {{ c.score.toFixed(2) }}</span>
          <Button
            size="sm"
            variant="outline"
            :disabled="unifiedBusy !== null"
            @click="confirmCandidate(c, i)"
          >确认配对</Button>
        </div>
      </div>
      <div v-if="unifiedMsg" class="px-2 pb-2">
        <AlertBanner :tone="unifiedMsg.tone" :title="unifiedMsg.text" />
      </div>
      <div v-if="unifiedEvents.length" class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[680px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">事件</th>
              <th class="label-micro px-2 pb-2.5">状态</th>
              <th class="label-micro px-2 pb-2.5">场地</th>
              <th class="label-micro px-2 pb-2.5">差异标记</th>
              <th class="label-micro px-2 pb-2.5">操作</th>
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
              <td class="px-2 py-2.5">
                <!-- ④ 撤销：账本落审计、引擎视图同步拔除（策略立刻看不见）。 -->
                <Button
                  size="sm"
                  variant="ghost"
                  :disabled="unifiedBusy !== null"
                  :title="`撤销映射 ${e.id}（可重新确认）`"
                  @click="revokeEvent(e)"
                >撤销</Button>
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
