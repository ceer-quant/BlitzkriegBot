<script setup lang="ts">
/**
 * 进化（E13 #95）— 影子引擎把变异策略作为「提案」递上来，操作员在这里看到
 * 变异策略与当前策略的全面对比后拍板：采纳 / 拒绝 / 延后；勾选自动进化后
 * 内核直接落盘采纳（promotions.jsonl 可一键回滚）。所有动作走网关命令，
 * 与 TUI、命令行共用同一套 `decide` / `auto-evolve` / `rollback` 动词。
 *
 * 页面的三个问题必须各有明确答案，否则这个页面就是不可读的：
 *   1. 进化到底跑没跑   → 顶部「进化周期」卡（内核上报的上轮时间 / 下轮时刻）
 *   2. 采纳到底生效没   → 决策后回读快照里的 state 才算数，并在顶部横幅报出
 *   3. 现在跑的是哪一版 → 「当前生效参数」卡（最近一次被采纳的变异 + 谁改的 + 何时）
 */
import { computed, ref } from 'vue'
import { FlaskConical, RotateCcw, Clock } from 'lucide-vue-next'
import {
  api,
  type EvolutionProposalRow,
  type EvolutionMetricsRow,
} from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { num, signedMoney, winRatePct, dateTime } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const store = usePanelStore()

const evo = computed(() => store.snapshot?.evolution ?? null)
const autoEvolve = computed(() => evo.value?.status?.autoEvolve ?? false)

const proposals = computed(() => evo.value?.proposals ?? [])

const pending = computed(() =>
  proposals.value.filter((p) => p.state === 'proposed' || p.state === 'deferred'),
)

/** 现在生效的变异（最近的采纳记录），按策略取最新一条。 */
const liveRows = computed(() => {
  const latest = new Map<string, EvolutionProposalRow>()
  for (const p of proposals.value) {
    if (p.state !== 'accepted') continue
    const cur = latest.get(p.strategy)
    if (!cur || decidedMs(p) >= decidedMs(cur)) latest.set(p.strategy, p)
  }
  return [...latest.entries()]
    .map(([strategy, p]) => ({ strategy, p }))
    .sort((a, b) => a.strategy.localeCompare(b.strategy))
})

/** 台账：所有不再等你拍板的提案（含已采纳，作为历史可追溯）。 */
const ledger = computed(() =>
  proposals.value.filter((p) => p.state !== 'proposed' && p.state !== 'deferred'),
)

const busy = ref<string | null>(null)
const flash = ref<{ tone: 'info' | 'warn' | 'error'; msg: string } | null>(null)
let flashTimer: ReturnType<typeof setTimeout> | null = null

function showFlash(tone: 'info' | 'warn' | 'error', msg: string): void {
  flash.value = { tone, msg }
  if (flashTimer) clearTimeout(flashTimer)
  // 未确认（warn）要 (给人时间读完并去查日志，其余 6 秒也够读两遍。
  flashTimer = setTimeout(() => { flash.value = null }, tone === 'warn' ? 15_000 : 6_000)
}

async function act(label: string, fn: () => Promise<unknown>): Promise<boolean> {
  busy.value = label
  try {
    await fn()
    await store.refresh()
    return true
  } catch (e) {
    showFlash('error', e instanceof Error ? e.message : String(e))
    return false
  } finally {
    busy.value = null
  }
}

async function setAuto(next: boolean): Promise<void> {
  if (!await act('auto-evolve', () => api.setAutoEvolve(next))) return
  // 回读：开关是否真的落了，只看内核回报的状态。
  const now = evo.value?.status?.autoEvolve ?? false
  if (now !== next) {
    showFlash('warn', `指令已发出，但内核回报的开关状态仍是「${now ? '自动' : '人工'}」——改动尚未生效，请稍后刷新或查内核日志。`)
    return
  }
  showFlash('info', next
    ? '自动进化已开启：内核将直接落盘采纳（每次采纳都记进 promotions.jsonl，可用回滚撤销）'
    : '已关闭自动进化：每个提案都等你拍板')
}

async function decide(p: EvolutionProposalRow, decision: 'accept' | 'reject' | 'defer'): Promise<void> {
  // 自动进化开着时内核自己会采纳，人工采纳只会和它抢同一个提案。
  // 按钮已经隐藏，这里再挡一次，防的是渲染与内核状态切换之间的时间差。
  if (decision === 'accept' && autoEvolve.value) {
    showFlash('warn', '自动进化已开启：内核会自行采纳提案。要人工拍板，请先关掉上方开关。')
    return
  }
  if (decision === 'accept'
    && !window.confirm(`采纳后立即热更新 ${p.strategy} 的实盘参数（原参数已存档，可一键回滚）。确认采纳 ${p.id} 吗？`)) {
    return
  }
  if (!await act(`${p.id}-${decision}`, () => api.decideEvolution(p.id, decision))) return

  // 以刷新后的快照为准：只有内核回报了目标状态才算生效。
  // 否则「已采纳」只是我们自己的一句话——用户看不到任何变化，这正是之前的病。
  const after = proposals.value.find((x) => x.id === p.id)
  if (!after || after.state !== EXPECTED_STATE[decision]) {
    const seen = after ? (STATE_LABELS[after.state] ?? after.state) : '快照里查不到该提案'
    showFlash('warn', `已发出「${DECISION_LABELS[decision]}」指令，但内核回报的状态仍是「${seen}」——提案留在待决区。请稍后刷新，或在内核日志里搜 evolution 看它是否拒绝。`)
    return
  }
  const who = after.decidedBy === 'auto' ? '自动进化' : after.decidedBy === 'user' ? '人工' : '未上报'
  const when = after.decidedAtMs ? `，${dateTime(after.decidedAtMs)}` : ''
  showFlash('info', {
    accept: `已采纳并生效：${p.strategy} 现在跑的是这组变异参数（${who}决定${when}）。参数移动见上方「当前生效参数」。`,
    reject: `已拒绝：${p.strategy} 保持现行参数（${who}决定${when}）。`,
    defer: `已延后：${p.id} 留在待决区，7 天内仍可处理。`,
  }[decision])
}

async function rollback(strategy: string): Promise<void> {
  if (!window.confirm(`把 ${strategy} 回滚到上一次采纳前的参数？（只撤销最近一次，更早的可在审计文件中追溯）`)) return
  await act(`rollback-${strategy}`, () => api.rollbackStrategy(strategy))
  showFlash('info', `已发出回滚：${strategy} 恢复到上一组参数。若「当前生效参数」未随之变化，说明内核未接受该回滚。`)
}

/* ── 展示辅助 ──────────────────────────────────────────────────────────── */

const REASON_LABELS: Record<string, string> = {
  higher_win_rate: '胜率更高',
  better_profit_factor: '盈利因子更优',
  combined_improvement: '综合改善',
}
const reasonLabel = (r: string) => REASON_LABELS[r] ?? r

const DECISION_LABELS: Record<'accept' | 'reject' | 'defer', string> = {
  accept: '采纳',
  reject: '拒绝',
  defer: '延后',
}

/** 决策后内核应当回报的状态：带在快照上校验，用来证明「真的生效了」。 */
const EXPECTED_STATE: Record<'accept' | 'reject' | 'defer', string> = {
  accept: 'accepted',
  reject: 'rejected',
  defer: 'deferred',
}

const STATE_LABELS: Record<string, string> = {
  proposed: '待决',
  deferred: '已延后',
  accepted: '已采纳',
  rejected: '已拒绝',
  expired: '已过期',
  superseded: '已被新提案取代',
}

const decidedMs = (p: EvolutionProposalRow): number => p.decidedAtMs || p.createdAtMs

function ttlLeft(expiresAtMs: number): string {
  const left = expiresAtMs - Date.now()
  if (left <= 0) return '已过期'
  const h = Math.floor(left / 3_600_000)
  if (h >= 48) return `${Math.floor(h / 24)}天${h % 24}小时`
  if (h >= 1) return `${h}小时${Math.floor((left % 3_600_000) / 60_000)}分`
  return `${Math.max(1, Math.round(left / 60_000))}分`
}

/** 距下轮深度进化的倒计时（内核上报 nextCycleAtMs；缺省说明内核没给）。 */
function untilNext(ms: number | null | undefined): string {
  if (!ms) return '内核未安排'
  if (ms - Date.now() <= 0) return '即将触发'
  return ttlLeft(ms)
}

const emptyPendingText = computed(() => {
  const last = evo.value?.status?.lastCycleMs
  const cycle = last ? `上轮深度进化 ${cycleAgo(last)}` : '内核尚未上报深度进化记录'
  return `暂无待决提案 —— 评估器发现更优变异时会挂到这里。${cycle}，周期 72 小时。`
})

interface MetricRow {
  name: string
  base: string
  variant: string
  delta: string
  better: boolean | null
}

function deltaRow(name: string, base: string, variant: string, delta: string, better: boolean | null): MetricRow {
  return { name, base, variant, delta, better }
}

/** 对比表行：笔数 / 胜率 / 盈亏比 / 盈利因子 / 净 PnL，差值按是否利好变异着色。 */
function metricsRows(b: EvolutionMetricsRow, v: EvolutionMetricsRow): MetricRow[] {
  const wrB = winRatePct(b.winRate)
  const wrV = winRatePct(v.winRate)
  const netB = Number(b.netPnlUsd) || 0
  const netV = Number(v.netPnlUsd) || 0
  const pfB = Number(b.profitFactor) || 0
  const pfV = Number(v.profitFactor) || 0
  const poB = Number(b.payoff) || 0
  const poV = Number(v.payoff) || 0
  return [
    deltaRow('平仓笔数', num(b.closed ?? 0), num(v.closed ?? 0),
      `${(v.closed ?? 0) - (b.closed ?? 0)}`, null),
    deltaRow('胜率', `${wrB.toFixed(1)}%`, `${wrV.toFixed(1)}%`,
      `${wrV - wrB >= 0 ? '+' : ''}${(wrV - wrB).toFixed(1)}pp`, wrV > wrB),
    deltaRow('盈亏比', poB.toFixed(2), poV.toFixed(2),
      `${poV - poB >= 0 ? '+' : ''}${(poV - poB).toFixed(2)}`, poV > poB),
    deltaRow('盈利因子', pfB.toFixed(2), pfV.toFixed(2),
      `${pfV - pfB >= 0 ? '+' : ''}${(pfV - pfB).toFixed(2)}`, pfV > pfB),
    deltaRow('净 PnL', signedMoney(netB), signedMoney(netV),
      signedMoney(netV - netB), netV > netB),
  ]
}

const cycleAgo = (ms: number): string => {
  const s = Math.max(0, Date.now() - ms) / 1000
  if (s < 90) return `${Math.round(s)} 秒前`
  const m = Math.round(s / 60)
  if (m < 90) return `${m} 分钟前`
  const h = Math.round(m / 60)
  if (h < 48) return `${h} 小时前`
  return `${Math.round(h / 24)} 天前`
}
</script>

<template>
  <div class="rise-in">
    <!-- 开关 + 周期钟 -->
    <div class="grid gap-3.5 sm:grid-cols-3">
      <Card dense>
        <CardHeader label="托管模式" />
        <div class="flex items-center justify-between px-1">
          <div>
            <div class="text-[13px] font-semibold" :class="evo?.status?.autoEvolve ? 'text-up' : 'text-primary'">
              {{ evo?.status?.autoEvolve ? '自动进化：开' : '人工拍板' }}
            </div>
            <p class="mt-0.5 text-[11.5px] text-faint-fg">
              {{ evo?.status?.autoEvolve ? '内核直接采纳提案（promotions 可回滚）' : '每个提案都在待决区等你确认' }}
            </p>
          </div>
          <Switch
            :model-value="evo?.status?.autoEvolve ?? false"
            :disabled="busy === 'auto-evolve'"
            :label="evo?.status?.autoEvolve ? '自动' : '人工'"
            @update:model-value="setAuto"
          />
        </div>
      </Card>
      <Card dense>
        <CardHeader label="进化周期" />
        <div class="px-1 text-[13px]">
          <div class="font-semibold">
            <Clock class="mr-1 inline size-3.5 text-primary" />距下轮深度进化
            <span class="num">{{ untilNext(evo?.status?.nextCycleAtMs) }}</span>
          </div>
          <p class="mt-0.5 text-[11.5px] text-faint-fg">
            上轮 {{ evo?.status?.lastCycleMs ? cycleAgo(evo.status.lastCycleMs) : '内核未上报' }}
            · 周期 72 小时
          </p>
        </div>
      </Card>
      <Card dense>
        <CardHeader label="待决提案" />
        <div class="px-1 text-[13px]">
          <div class="text-[22px] font-bold num">{{ pending.length }}</div>
          <p class="mt-0.5 text-[11.5px] text-faint-fg">
            {{ evo ? `当前 ${pending.length} 个提案待拍板` : '内核未上报进化状态（旧版内核？）' }}
          </p>
        </div>
      </Card>
    </div>

    <div v-if="flash" class="mt-3.5">
      <AlertBanner :tone="flash.tone" title="进化操作" dismissible @dismiss="flash = null">
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <!-- 现在生效的是什么 —— 回答「当前策略是哪一版、谁改的、何时改的」 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="当前生效参数（最近一次被采纳的变异）">
        <template #action>
          <Badge :variant="liveRows.length ? 'gold' : 'outline'">{{ liveRows.length }} 个策略已变异</Badge>
        </template>
      </CardHeader>

      <EmptyState
        v-if="!liveRows.length"
        :loading="store.loading"
        text="暂无采纳记录：所有策略仍在基线参数上运行"
      />

      <div v-else class="space-y-3">
        <div
          v-for="row in liveRows"
          :key="row.strategy"
          class="rounded-lg border border-line bg-panel-2/60 p-3.5"
        >
          <div class="flex flex-wrap items-center gap-2.5">
            <span class="text-[14px] font-bold">{{ row.strategy }}</span>
            <Badge :variant="row.p.decidedBy === 'auto' ? 'gold' : 'default'">
              {{ row.p.decidedBy === 'auto' ? '自动进化采纳' : row.p.decidedBy === 'user' ? '人工采纳' : '采纳（决定方未上报）' }}
            </Badge>
            <span class="text-[11.5px] text-faint-fg num">
              {{ row.p.decidedAtMs ? `${dateTime(row.p.decidedAtMs)}（${cycleAgo(row.p.decidedAtMs)}）` : '采纳时间未上报' }}
            </span>
            <Badge v-if="row.p.cycleSeq" variant="outline">第 {{ row.p.cycleSeq }} 轮深度进化</Badge>
            <span class="text-[11.5px] text-faint-fg num">{{ row.p.id }}</span>
          </div>

          <div v-if="row.p.knobMoves.length" class="mt-2.5">
            <table class="w-full max-w-[560px] text-[12.5px]">
              <thead>
                <tr class="text-left text-faint-fg">
                  <th class="label-micro pb-1">参数</th>
                  <th class="label-micro pb-1">变异前</th>
                  <th class="label-micro pb-1">现在生效</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="[name, from, to] in row.p.knobMoves" :key="name" class="border-t border-line">
                  <td class="py-1.5 font-medium">{{ name }}</td>
                  <td class="py-1.5 num text-faint-fg">{{ from }}</td>
                  <td class="py-1.5 num font-bold text-primary">{{ to }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <p v-else class="mt-2 text-[12px] text-faint-fg">该提案未记录参数移动。</p>

          <p class="mt-2 text-[12px] text-muted-fg">
            采纳时窗口：胜率 <span class="num">{{ winRatePct(row.p.baseline.winRate).toFixed(1) }}%</span>
            → <span class="num font-semibold">{{ winRatePct(row.p.variant.winRate).toFixed(1) }}%</span>
            · 净 PnL <span class="num">{{ signedMoney(Number(row.p.baseline.netPnlUsd) || 0) }}</span>
            → <span class="num font-semibold">{{ signedMoney(Number(row.p.variant.netPnlUsd) || 0) }}</span>
            · 样本 <span class="num">{{ num(row.p.sampleCount) }}</span> 笔
          </p>

          <div class="mt-3">
            <Button
              size="sm"
              variant="outline"
              :disabled="busy === `rollback-${row.strategy}`"
              @click="rollback(row.strategy)"
            >
              <RotateCcw class="size-3.5" />回滚该策略
            </Button>
          </div>
        </div>
      </div>

      <p class="mt-2.5 text-[11px] leading-snug text-faint-fg">
        数据来源：采纳记录（与 <span class="num">data/evolution/promotions.jsonl</span> 同源）。
        内核暂未通过快照直读运行中的参数值，所以这里给出的是「改了哪几项、从什么改到什么、谁改的、何时改的」，
        而不是内存里参数的实时读数。
      </p>
    </Card>

    <!-- 待决提案 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="待决提案（变异 vs 现行）">
        <template #action>
          <Badge variant="gold" dot>{{ pending.length }} 个</Badge>
        </template>
      </CardHeader>

      <AlertBanner v-if="autoEvolve" tone="info" class="mb-3">
        自动进化已开启：内核会自行采纳提案，因此这里不提供「采纳」按钮，只保留拒绝与延后。
        要人工拍板，请先关掉上方开关。
      </AlertBanner>

      <EmptyState v-if="!pending.length" :loading="store.loading" :text="emptyPendingText" />

      <div v-else class="space-y-4">
        <div
          v-for="p in pending"
          :key="p.id"
          class="rounded-lg border border-line bg-panel-2/60 p-3.5"
        >
          <div class="flex flex-wrap items-center gap-2.5">
            <FlaskConical class="size-4 text-primary" />
            <span class="text-[14px] font-bold">{{ p.strategy }}</span>
            <Badge :variant="p.state === 'deferred' ? 'outline' : 'default'">
              {{ p.state === 'deferred' ? '已延后' : '待决' }}
            </Badge>
            <span class="text-[11.5px] text-faint-fg num">{{ p.id }}</span>
            <Badge variant="outline">
              <Clock class="mr-1 size-3" />剩余 {{ ttlLeft(p.expiresAtMs) }}
            </Badge>
            <Badge v-if="p.cycleSeq" variant="outline">第 {{ p.cycleSeq }} 轮深度进化</Badge>
          </div>

          <p class="mt-2 text-[12.5px] text-muted-fg">
            评估器理由：<span class="font-semibold text-primary">{{ reasonLabel(p.reason) }}</span>
            · 置信度 <span class="num font-semibold">{{ p.confidence.toFixed(2) }}</span>
            · 样本 <span class="num font-semibold">{{ num(p.sampleCount) }}</span> 笔
          </p>

          <!-- 参数移动 -->
          <div v-if="p.knobMoves.length" class="mt-3">
            <div class="label-micro mb-1.5">参数变异</div>
            <table class="w-full max-w-[560px] text-[12.5px]">
              <thead>
                <tr class="text-left text-faint-fg">
                  <th class="label-micro pb-1">参数</th>
                  <th class="label-micro pb-1">现行</th>
                  <th class="label-micro pb-1">变异</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="[name, from, to] in p.knobMoves" :key="name" class="border-t border-line">
                  <td class="py-1.5 font-medium">{{ name }}</td>
                  <td class="py-1.5 num text-faint-fg">{{ from }}</td>
                  <td class="py-1.5 num font-bold text-primary">{{ to }}</td>
                </tr>
              </tbody>
            </table>
          </div>

          <!-- 全面对比 -->
          <div class="mt-3">
            <div class="label-micro mb-1.5">窗口对比（同一采样窗：现行 → 变异）</div>
            <table class="w-full max-w-[560px] text-[12.5px]">
              <thead>
                <tr class="text-left">
                  <th class="label-micro pb-1">指标</th>
                  <th class="label-micro pb-1 text-right">现行</th>
                  <th class="label-micro pb-1 text-right">变异</th>
                  <th class="label-micro pb-1 text-right">差值</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in metricsRows(p.baseline, p.variant)" :key="row.name" class="border-t border-line">
                  <td class="py-1.5 font-medium">{{ row.name }}</td>
                  <td class="py-1.5 text-right num text-faint-fg">{{ row.base }}</td>
                  <td class="py-1.5 text-right num font-bold">{{ row.variant }}</td>
                  <td
                    class="py-1.5 text-right num font-semibold"
                    :class="row.better === null ? 'text-faint-fg' : row.better ? 'text-up' : 'text-down'"
                  >{{ row.delta || '—' }}</td>
                </tr>
              </tbody>
            </table>
          </div>

          <div class="mt-3.5 flex flex-wrap items-center gap-2">
            <Button
              v-if="!autoEvolve"
              size="sm"
              :disabled="busy === `${p.id}-accept`"
              @click="decide(p, 'accept')"
            >
              采纳（热更新）
            </Button>
            <Button size="sm" variant="outline" :disabled="busy === `${p.id}-reject`" @click="decide(p, 'reject')">
              拒绝
            </Button>
            <Button size="sm" variant="ghost" :disabled="busy === `${p.id}-defer`" @click="decide(p, 'defer')">
              延后
            </Button>
          </div>
        </div>
      </div>
    </Card>

    <!-- 处理台账 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="处理台账（只读：已采纳 / 已拒绝 / 已过期 / 被取代）" />
      <EmptyState v-if="!ledger.length" text="暂无处理记录" />
      <table v-else class="w-full min-w-[640px] text-[12.5px]">
        <thead>
          <tr class="text-left">
            <th class="label-micro pb-1.5">提案</th>
            <th class="label-micro pb-1.5">策略</th>
            <th class="label-micro pb-1.5">结论</th>
            <th class="label-micro pb-1.5">决定方</th>
            <th class="label-micro pb-1.5">时间</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="p in ledger" :key="p.id" class="border-t border-line">
            <td class="py-2 num text-faint-fg">{{ p.id }}</td>
            <td class="py-2 font-medium">{{ p.strategy }}</td>
            <td class="py-2">
              <Badge :variant="p.state === 'accepted' ? 'gold' : p.state === 'rejected' ? 'down' : 'default'">
                {{ STATE_LABELS[p.state] ?? p.state }}
              </Badge>
            </td>
            <td class="py-2 text-muted-fg">{{ p.decidedBy === 'auto' ? '自动进化' : p.decidedBy === 'user' ? '人工' : '—' }}</td>
            <td class="py-2 num text-faint-fg">{{ p.decidedAtMs ? dateTime(p.decidedAtMs) : '—' }}</td>
          </tr>
        </tbody>
      </table>
      <p class="mt-2.5 text-[11px] leading-snug text-faint-fg">
        采纳记录持久化在 <span class="num">data/evolution/promotions.jsonl</span>，回滚只撤销最近一次采纳；
        提案全档在 <span class="num">data/evolution/proposals.jsonl</span>，超过 7 天未决自动过期。
        若「距下轮深度进化」长期显示同一时间而这里没有新增记录，说明深度轮没有真的触发（常见原因是样本不足）——
        可在内核日志里搜 <span class="num">evolution</span> 确认。
      </p>
    </Card>
  </div>
</template>
