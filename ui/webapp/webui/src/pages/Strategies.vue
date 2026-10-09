<script setup lang="ts">
/**
 * 策略 — 分策略表现总表 + 风控拦截归因（E9-g）。每行可直接启用/停用
 * （网关 `strategy <name> on|off`）。
 *
 * 重设计（2026-10 操作者方案）：
 *  - 状态分组：运行中 / 已停止 / 异常（不相容）——组头一行，一眼分层。
 *  - 重叠根因修复：策略列只放「状态点 + 名称」，模式声明标签（E27 §8.3）
 *    整体移入详情展开行 —— 标签与来源路径同挤一行是截图里文字重叠的根源。
 *  - 车队 KPI 收敛为 3 张：规模 / 下单健康 / 净 PnL。被砍掉的两卡内容
 *    不丢：信号前拦截三计数就是表格里「限额拒 / 时机挡 / 动量挡」三列，
 *    胜负细目并进净 PnL 卡的 sub。
 *  - 行内编辑（Lua 行）+ 原因列展开详情：完整来源路径（不截断、可选中）、
 *    模式声明、闸门豁免、拒单原因分布与计数口径。
 *  - 搜索按名称 / 来源过滤（KPI 恒显全量，不随过滤漂移）。
 *
 * issue 393 (⑤): 蓝图入口 —「创建策略」跳蓝图（新建模式）；Lua 策略行带
 * 「编辑策略」，跳蓝图并预载该包的蓝图文档（blueprint.load，只读 IPC）。
 * 跳转走 store 的 navRequest —— 页签是 App 壳的资产，页面只提请求。
 */
import { computed, ref } from 'vue'
import { AlertTriangle, ChevronDown, Pencil, Plus, Search, ShieldCheck, ShieldOff } from 'lucide-vue-next'
import { api, type StrategyStatsRow } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { num, signedMoney, pct } from '@/lib/format'
import { refusalProfile, refusalTotals, REFUSAL_NOTE } from '@/lib/rejections'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()
const rows = computed<StrategyStatsRow[]>(() => store.strategyRows)

const busy = ref<string | null>(null)
const flash = ref<{ name: string; ok: boolean; msg: string } | null>(null)

async function toggle(r: StrategyStatsRow, next: boolean): Promise<void> {
  busy.value = r.name
  flash.value = null
  try {
    const res = await api.setStrategy(r.name, next)
    flash.value = {
      name: r.name,
      ok: res.ok !== false && !res.reason,
      // §8.2: an enable refused by the mode handshake speaks BOTH sides —
      // surface the core's reason verbatim instead of the generic "已启用".
      msg: res.reason ?? res.message ?? `${r.name} 已${next ? '启用' : '停用'}`,
    }
    await store.refresh()
  } catch (e) {
    flash.value = { name: r.name, ok: false, msg: e instanceof Error ? e.message : String(e) }
  } finally {
    busy.value = null
    setTimeout(() => { flash.value = null }, 5000)
  }
}

const totals = computed(() => {
  const rs = rows.value
  const sum = (f: (r: StrategyStatsRow) => number) => rs.reduce((a, r) => a + (Number(f(r)) || 0), 0)
  const wins = sum((r) => r.wins)
  const losses = sum((r) => r.losses)
  return {
    count: rs.length,
    enabled: rs.filter((r) => r.enabled).length,
    /** Refusal attribution — two disjoint families, see `lib/rejections.ts`. */
    refusal: refusalTotals(rs),
    closed: sum((r) => r.closedTrades),
    wins,
    losses,
    net: sum((r) => Number(r.netPnlUsd) || 0),
    winRate: wins + losses ? (wins / (wins + losses)) * 100 : 0,
  }
})

const profile = (r: StrategyStatsRow) => refusalProfile(r)

/**
 * E27 (§8.3): the strategy's declared modes as compact `type[/structure]`
 * tags, straight off the §2.3 wire objects (`strategy.list` rows). Null =
 * undeclared (§7.4): the strategy sits out of the handshake entirely, so it
 * renders no mode badges at all — not even a "未声明" marker, matching the
 * core's semantics that undeclared means "no claim, no conflict".
 * （重设计：标签只在详情行出现 —— 行内与来源路径同挤一行是重叠的根源。）
 */
function modeTags(r: StrategyStatsRow): string[] {
  if (!Array.isArray(r.modes)) return []
  return r.modes
    .filter((m): m is Record<string, unknown> => m !== null && typeof m === 'object')
    .map((m) => {
      const mt = typeof m.market_type === 'string' ? m.market_type : '?'
      const st = typeof m.structure === 'string' ? `/${m.structure}` : ''
      const caps = Array.isArray(m.capabilities) ? m.capabilities.length : 0
      return caps ? `${mt}${st}+${caps}` : `${mt}${st}`
    })
}

/**
 * The tooltip must say how FAR an opt-out reaches (D-31). Two strategies both
 * showing 「时机」 can behave completely differently: one stops at the kernel's
 * window, the other enters right up to the last second. Without the floor the
 * badge would report them identically.
 */
function exemptionTooltip(r: StrategyStatsRow): string {
  const gates = r.gateExemptions.join(' / ')
  const named =
    r.gateExemptions.includes('timing') && r.gateExemptionTimingFloorSec != null
      ? `，其中时机闸门仅在剩余 ≥${r.gateExemptionTimingFloorSec}s 时豁免`
      : ''
  return `声明豁免：${gates}${named}`
}

/** 详情行里豁免的一行话（无豁免也要有说法，不能留空）。 */
function exemptionSummary(r: StrategyStatsRow): string {
  if (!r.gateExemptions?.length) return '未声明豁免 —— 全部信号闸门照常生效。'
  return exemptionTooltip(r)
}

/**
 * 状态分组（操作者重设计）：异常 = 已声明模式但无相容插件（§8.3），这样的
 * 策略无论开关都交易不了 —— 单独成组才「一眼识别异常」，不与能交易的混排。
 * 组序 = 运行中 → 已停止 → 异常。
 */
function isAbnormal(r: StrategyStatsRow): boolean {
  return r.compatible === false
}

const query = ref('')

const filteredRows = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return rows.value
  return rows.value.filter(
    (r) => r.name.toLowerCase().includes(q) || r.source.toLowerCase().includes(q),
  )
})

const groups = computed(() =>
  [
    { id: 'running', label: '运行中', rows: filteredRows.value.filter((r) => r.enabled && !isAbnormal(r)) },
    { id: 'stopped', label: '已停止', rows: filteredRows.value.filter((r) => !r.enabled && !isAbnormal(r)) },
    { id: 'abnormal', label: '异常', rows: filteredRows.value.filter(isAbnormal) },
  ].filter((g) => g.rows.length > 0),
)

function causePills(r: StrategyStatsRow): { name: string; n: number }[] {
  return refusalProfile(r).causes
}

const expanded = ref<string | null>(null)
function toggleExpand(name: string): void {
  expanded.value = expanded.value === name ? null : name
}

// ── issue 393 (⑤): blueprint entries ─────────────────────────────────────────────
// 「创建策略」跳蓝图的新建模式（画布首访本就铺示例模板）；「编辑策略」只在
 // Lua 策略行出现（source = `lua:<dir>`），跳蓝图并按名预载该包的蓝图文档。
function createStrategy(): void {
  store.requestNav('blueprint')
}
function editStrategy(name: string): void {
  store.requestBlueprintPreload(name)
  store.requestNav('blueprint')
}
/** Lua 策略的来源拼法（engine.stats 的 `source`）；蓝图编辑只对 Lua 包有意义。 */
function isLua(r: StrategyStatsRow): boolean {
  return typeof r.source === 'string' && r.source.startsWith('lua:')
}
</script>

<template>
  <div v-if="rows.length" class="rise-in">
    <!-- fleet KPIs — 3 tiles（重设计：拦截三计数本就是表格三列，胜负并进净 PnL sub） -->
    <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-3">
      <StatTile label="策略规模" :value="String(totals.count)" tone="gold">
        <template #sub>运行中 <RollingNumber :value="totals.enabled" /> · 共 <RollingNumber :value="totals.count" /> 个已注册</template>
      </StatTile>
      <!--
        「拒单」是两族互不包含的计数,不可相加,也不可互相解释。旧文案把
        ordersRejected 写成「笔被风控拒绝」并把 限额/时机/动量 并列在后,读起来
        像后者应当是前者的明细 —— 于是一个 3 万多的数旁边摆着四个 0,看起来像
        风控没上报,实际是风控本身也在那 3 万多里。这里按发生位置分开表述。
      -->
      <StatTile label="下单 / 拒单率" :value="num(totals.refusal.placed)">
        <template #sub>
          <Tooltip :content="REFUSAL_NOTE">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              被拒 <span class="text-down font-semibold"><RollingNumber :value="num(totals.refusal.rejected)" /></span> · 拒单率
              <RollingNumber :value="pct(totals.refusal.refuseRate)" />
            </span>
          </Tooltip>
        </template>
      </StatTile>
      <StatTile
        label="净 PnL（扣费）"
        :value="signedMoney(totals.net)"
        :tone="totals.net >= 0 ? 'up' : 'down'"
      >
        <template #sub>
          <Tooltip content="分策略账本合计；胜率按已平仓计，盈 / 亏细目见下表「胜 / 负」列。">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              平仓 <RollingNumber :value="totals.closed" /> · 胜率 <RollingNumber :value="pct(totals.winRate)" />（<RollingNumber :value="totals.wins" /> 盈 / <RollingNumber :value="totals.losses" /> 亏）
            </span>
          </Tooltip>
        </template>
      </StatTile>
    </div>

    <div v-if="flash" class="mt-3.5">
      <AlertBanner
        :tone="flash.ok ? 'info' : 'error'"
        :title="flash.name"
        :hint="flash.ok ? undefined : '若持续失败，确认网关在线后重试。'"
        dismissible
        @dismiss="flash = null"
      >
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <Card class="mt-3.5" dense>
      <CardHeader label="策略表现">
        <template #action>
          <div class="relative">
            <Search class="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-faint-fg" />
            <Input
              :model-value="query"
              placeholder="搜索策略…"
              size="sm"
              class="w-44 pl-7"
              @update:model-value="query = $event"
            />
          </div>
          <Badge variant="gold" dot><RollingNumber :value="rows.length" /> 行</Badge>
          <!-- issue 393 (⑤): the page's create entry — jumps to the blueprint canvas. -->
          <Button variant="gold" size="sm" title="在蓝图中新建一个策略" @click="createStrategy">
            <Plus class="size-3.5" />创建策略
          </Button>
        </template>
      </CardHeader>

      <div class="-mx-2 overflow-x-auto">
        <!--
          `table-fixed` + explicit colgroup, not automatic layout. Under auto
          layout the browser derives column widths from cell content, so the
          expanded detail row (`<td colspan="13">`) made the numeric columns
          want more width: expanding a row slid 「来源」 left and 「下单被拒」
          right, so the header row visibly jolted sideways. Pinning the columns
          removes that coupling — the detail row can no longer influence the
          layout. The redesign keeps the 13-column skeleton; what moved is
          content: mode tags left the name cell (they were what overlapped the
          source path), and the source cell holds only a truncating path.
        -->
        <table class="w-full min-w-[1080px] table-fixed text-[13px]">
          <colgroup>
            <col class="w-[13%]" />
            <col class="w-[5.5%]" />
            <col class="w-[22%]" />
            <col class="w-[5.5%]" />
            <col class="w-[6.5%]" />
            <col class="w-[5.5%]" />
            <col class="w-[5.5%]" />
            <col class="w-[5.5%]" />
            <col class="w-[4.5%]" />
            <col class="w-[5.5%]" />
            <col class="w-[6.5%]" />
            <col class="w-[7.5%]" />
            <col class="w-[7%]" />
          </colgroup>
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">策略</th>
              <th class="label-micro px-2 pb-2.5">启用</th>
              <th class="label-micro px-2 pb-2.5">来源</th>
              <th class="label-micro px-2 pb-2.5 text-right">下单</th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">下单被拒</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">限额拒</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">时机挡</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">动量挡</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">豁免</th>
              <th class="label-micro px-2 pb-2.5 text-right">平仓</th>
              <th class="label-micro px-2 pb-2.5 text-right">胜 / 负</th>
              <th class="label-micro px-2 pb-2.5 text-right">净 PnL</th>
              <th class="label-micro px-2 pb-2.5 text-right">原因</th>
            </tr>
          </thead>
          <tbody>
            <template v-for="g in groups" :key="g.id">
              <!-- 状态分组头：一行一组，色点 + 组名 + 计数（异常组用 ⚠ 图标） -->
              <tr>
                <td colspan="13" class="px-2 pt-3.5 pb-1">
                  <span class="inline-flex items-center gap-2">
                    <AlertTriangle v-if="g.id === 'abnormal'" class="size-3 text-down" />
                    <span
                      v-else
                      class="size-1.5 rounded-full"
                      :style="{ background: g.id === 'running' ? 'var(--up)' : 'var(--faint-fg)' }"
                    />
                    <span class="label-micro">{{ g.label }}</span>
                    <Badge variant="outline"><RollingNumber :value="g.rows.length" /></Badge>
                  </span>
                </td>
              </tr>
              <template v-for="r in g.rows" :key="r.name">
                <tr class="border-t border-line transition-colors hover:bg-panel-2">
                  <td class="px-2 py-2.5 font-semibold">
                    <span class="inline-flex min-w-0 items-center gap-2">
                      <span
                        class="size-1.5 shrink-0 rounded-full"
                        :style="{ background: isAbnormal(r) ? 'var(--down)' : r.enabled ? 'var(--up)' : 'var(--faint-fg)' }"
                      />
                      <span class="truncate" :title="r.name">{{ r.name }}</span>
                      <!-- issue 393 (⑤): per-row 编辑策略 — jumps to the blueprint
                           canvas and preloads this package's blueprint document
                           (kernel `blueprint.load`). Lua rows only: the canvas
                           edits the graph a blueprint compiles from. -->
                      <Tooltip v-if="isLua(r)" content="在蓝图中编辑该策略（预载它的蓝图文档）">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          title="在蓝图中编辑该策略"
                          @click="editStrategy(r.name)"
                        >
                          <Pencil class="size-3.5" />
                        </Button>
                      </Tooltip>
                      <!-- E27 (§8.3): incompatible = no active plugin satisfies any
                           declared mode; the hover speaks both sides verbatim. -->
                      <Tooltip v-if="r.compatible === false" :content="r.incompatibleReason ?? '无相容插件模式'">
                        <Badge variant="down" dot>不相容</Badge>
                      </Tooltip>
                    </span>
                  </td>
                  <td class="px-2 py-2.5">
                    <Switch
                      :model-value="r.enabled"
                      :disabled="busy === r.name"
                      @update:model-value="toggle(r, $event)"
                    />
                  </td>
                  <td class="px-2 py-2.5">
                    <span class="block truncate text-[12px] text-muted-fg" :title="r.source">{{ r.source }}</span>
                  </td>
                  <td class="px-2 py-2.5 text-right num"><RollingNumber :value="num(r.ordersPlaced)" /></td>
                  <td
                    class="px-2 py-2.5 text-right num"
                    :class="r.ordersRejected ? 'font-semibold text-down' : 'text-faint-fg'"
                  ><RollingNumber :value="num(r.ordersRejected)" /></td>
                  <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="num(r.limitRejected)" /></td>
                  <td
                    class="px-2 py-2.5 text-right num"
                    :class="r.blockedTiming ? 'font-semibold text-primary' : 'text-faint-fg'"
                  ><RollingNumber :value="num(r.blockedTiming)" /></td>
                  <td
                    class="px-2 py-2.5 text-right num"
                    :class="r.blockedMomentum ? 'font-semibold text-primary' : 'text-faint-fg'"
                  ><RollingNumber :value="num(r.blockedMomentum)" /></td>
                  <td class="px-2 py-2.5 text-right">
                    <Tooltip
                      v-if="r.gateExemptions?.length"
                      :content="exemptionTooltip(r)"
                    >
                      <span class="num inline-flex items-center justify-end gap-1 font-semibold text-info">
                        <ShieldOff class="size-3" /><RollingNumber :value="r.gateExemptions.length" />
                      </span>
                    </Tooltip>
                    <span v-else class="num text-faint-fg">0</span>
                  </td>
                  <td class="px-2 py-2.5 text-right num"><RollingNumber :value="num(r.closedTrades)" /></td>
                  <td class="px-2 py-2.5 text-right num whitespace-nowrap">
                    <span class="text-up"><RollingNumber :value="num(r.wins)" /></span>
                    <span class="text-faint-fg"> / </span>
                    <span class="text-down"><RollingNumber :value="num(r.losses)" /></span>
                  </td>
                  <td
                    class="px-2 py-2.5 text-right num font-semibold"
                    :class="Number(r.netPnlUsd) >= 0 ? 'text-up' : 'text-down'"
                  ><RollingNumber :value="signedMoney(r.netPnlUsd)" /></td>
                  <td class="px-2 py-2.5 text-right">
                    <Button
                      variant="ghost"
                      size="sm"
                      :title="expanded === r.name ? '收起详情' : '展开详情'"
                      @click="toggleExpand(r.name)"
                    >
                      <AlertTriangle
                        v-if="profile(r).causes.length || profile(r).causesMissing"
                        class="size-3.5 text-primary"
                      />
                      <ShieldCheck v-else class="size-4 text-up/45" />
                      <ChevronDown
                        class="size-3 transition-transform"
                        :class="expanded === r.name && 'rotate-180'"
                      />
                    </Button>
                  </td>
                </tr>
                <!-- 详情展开行：来源完整路径（不截断）、模式声明、豁免、拒单归因。
                     table-fixed 已把列钉死 —— 本行再宽也影响不了列布局。 -->
                <tr v-if="expanded === r.name" :key="`${r.name}-detail`" class="border-t border-line bg-panel-2">
                  <td colspan="13" class="px-3 py-3">
                    <div class="grid gap-x-6 gap-y-3 lg:grid-cols-2">
                      <div class="min-w-0">
                        <div class="label-micro mb-1">来源（完整路径）</div>
                        <div class="num select-all break-all text-[12px] text-muted-fg">{{ r.source }}</div>
                        <div class="label-micro mb-1 mt-3">模式声明</div>
                        <div class="flex flex-wrap gap-2">
                          <Badge v-for="t in modeTags(r)" :key="`m-${t}`" variant="outline">{{ t }}</Badge>
                          <!-- Undeclared (§7.4): no claim, no conflict — say so in
                               prose rather than fabricating a badge the core
                               deliberately does not send. -->
                          <span v-if="!modeTags(r).length" class="text-[12px] text-faint-fg">未声明 —— 不参与模式握手，无冲突可言。</span>
                        </div>
                        <div v-if="r.compatible === false" class="mt-2 text-[12px] text-down">
                          {{ r.incompatibleReason ?? '无相容插件模式：当前没有任何启用的插件能满足声明的模式。' }}
                        </div>
                      </div>
                      <div class="min-w-0">
                        <div class="label-micro mb-1">闸门豁免</div>
                        <div class="text-[12px] text-muted-fg">{{ exemptionSummary(r) }}</div>
                        <div class="label-micro mb-1 mt-3">拒单原因分布（下单函数内部拒绝）</div>
                        <div v-if="profile(r).causes.length" class="flex flex-wrap gap-2">
                          <!-- A pill with a tone triple is a Badge — the kit's `gold`
                               variant is this exact shape, so the page composes it
                               instead of copying its tint (issue 260). -->
                          <Badge v-for="p in causePills(r)" :key="p.name" variant="gold">
                            {{ p.name }}
                            <span class="num font-bold">×<RollingNumber :value="p.n" /></span>
                          </Badge>
                        </div>
                        <div v-else-if="profile(r).causesMissing" class="text-[12px] text-muted-fg">
                          该策略有
                          <span class="num font-semibold"><RollingNumber :value="num(profile(r).rejected)" /></span>
                          次下单被拒，但内核未上报拒单原因 ——
                          通常是内核版本早于拒单归因（rejectionCauses）功能，重启内核后即可看到分类。
                        </div>
                        <div v-else class="text-[12px] text-faint-fg">无内部拒绝记录。</div>
                        <!--
                          The bare counts look implausible next to a single open position,
                          so state the measurement basis here: a refusal is counted per
                          evaluation cycle, and a refused candidate is retried each cycle.
                        -->
                        <p v-if="profile(r).causes.length" class="mt-2.5 text-[11px] leading-snug text-faint-fg">
                          计数口径：被拒的候选不会记为「挂单中」，引擎每个评估周期（约 50ms，即每秒约 20 次）会重新提出同一信号并再次被拒，
                          因此上述数字是<span class="text-muted-fg">拒绝次数随时间的累积</span>，不是不同信号的笔数。
                          一个持续被挡的信号（例如同标的有持仓、持仓已满、熔断未解除）每小时可累积约 7 万次。
                        </p>
                      </div>
                    </div>
                  </td>
                </tr>
              </template>
            </template>
            <tr v-if="!groups.length">
              <td colspan="13" class="px-3 py-10">
                <EmptyState
                  text="没有匹配的策略"
                  :hint="`搜索「${query.trim()}」无结果 —— 按名称或来源路径过滤，清空搜索框恢复全表。`"
                />
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>
  </div>

  <Card v-else class="rise-in">
    <EmptyState
      :loading="store.loading"
      text="暂无策略数据"
      hint="注册策略后自动出现；新策略可用 blitzkrieg-new-strategy 脚手架生成，或在蓝图中创建。"
    >
      <template #default>
        <Button variant="gold" size="sm" class="mt-3" @click="createStrategy">
          <Plus class="size-3.5" />创建策略
        </Button>
      </template>
    </EmptyState>
  </Card>
</template>
