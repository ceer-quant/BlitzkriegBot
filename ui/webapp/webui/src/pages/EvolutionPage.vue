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
 *   3. 现在跑的是哪一版 → 「已采纳的变异」卡（改了什么 / 谁改的 / 何时）。
 *      注意快照只暴露采纳记录、不暴露运行中的参数值，所以这张卡是凭据而非
 *      实时读数 —— 卡内脚注必须写明这一点，别把「记录」说成「现在生效」。
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
/** 引擎开关：false = 评估器根本没跑（无影子策略、无变异体、不会排深度轮）。 */
const engineOn = computed(() => evo.value?.status?.enabled ?? false)
/**
 * 自动模式真正生效的条件是「引擎开着 且 自动开着」：只开自动而引擎未启用时，
 * 内核既不评估也不会采纳 —— 之前的页面只读 autoEvolve，于是把「自动：开」
 * 显示成一个什么都不做的开关。所有「内核会自己采纳」的判断都走这个 computed。
 */
const autoActive = computed(() => engineOn.value && autoEvolve.value)
const cycleSeq = computed(() => evo.value?.status?.cycleSeq ?? 0)
/** 内核上报的深度轮周期（秒）；旧内核不上报时为 0，文案退回「未上报」。 */
const cycleSecs = computed(() => evo.value?.status?.cycleSecs ?? 0)

/** 当前真实行为，一句话（三种组合分开说，不留含糊）。 */
const modeSummary = computed(() => {
  if (!evo.value) return '内核未上报状态'
  if (!engineOn.value) return '不进化（引擎未启用）'
  return autoEvolve.value ? '自动：内核自行采纳，无需人工' : '人工：达标变异等你拍板'
})

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
  if (next && !engineOn.value) {
    showFlash('warn', '自动进化已记录，但进化引擎当前未启用：内核不会评估、不会采纳，也不会有待决提案。请先打开同样的「进化引擎」开关。')
    return
  }
  showFlash('info', next
    ? '自动进化已开启：内核自行采纳达标的变异（每次采纳都记进 promotions.jsonl，可一键回滚）。待决区不会再堆积需要人工拍板的提案。'
    : '已关闭自动进化：每个提案都在待决区等你拍板')
}

async function setEngine(next: boolean): Promise<void> {
  if (next && !window.confirm('启用进化引擎？内核会为每个策略建立影子变异体并持续评估（有 CPU 开销，所有动作仍在原有风控锁内）。')) {
    return
  }
  if (!await act('evolve', () => api.setEvolve(next))) return
  const now = evo.value?.status?.enabled ?? false
  if (now !== next) {
    showFlash('warn', `指令已发出，但内核回报的引擎状态仍是「${now ? '运行中' : '未启用'}」——请稍后刷新或查内核日志。`)
    return
  }
  showFlash('info', next
    ? `进化引擎已启用：开始评估影子变异体${autoEvolve.value ? '，并按自动模式自行采纳' : '，达标的变异挂到待决区等你拍板'}。深度轮首次计时从此刻开始。`
    : '进化引擎已关闭：不再评估、不再采纳，已挂起的提案保留但不会推进。')
}

async function decide(p: EvolutionProposalRow, decision: 'accept' | 'reject' | 'defer'): Promise<void> {
  // 自动模式（引擎开着且自动开着）内核自己会采纳，人工采纳只会和它抢同一个提案。
  // 按钮已经隐藏，这里再挡一次，防的是渲染与内核状态切换之间的时间差。
  if (decision === 'accept' && autoActive.value) {
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
    accept: `已采纳并生效：${p.strategy} 现在跑的是这组变异参数（${who}决定${when}）。参数移动见上方「已采纳的变异」。`,
    reject: `已拒绝：${p.strategy} 保持现行参数（${who}决定${when}）。`,
    defer: `已延后：${p.id} 留在待决区，7 天内仍可处理。`,
  }[decision])
}

async function rollback(strategy: string): Promise<void> {
  if (!window.confirm(`把 ${strategy} 回滚到上一次采纳前的参数？（只撤销最近一次，更早的可在审计文件中追溯）`)) return
  await act(`rollback-${strategy}`, () => api.rollbackStrategy(strategy))
  showFlash('info', `已发出回滚：${strategy} 恢复到上一组参数。若「已采纳的变异」未随之变化，说明内核未接受该回滚。`)
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

/**
 * 距下轮深度进化的倒计时。内核上报 nextCycleAtMs 为空有两种截然不同的原因，
 * 必须分开说：引擎没开（什么都不会发生）和引擎开着但时钟还没起来。把它们
 * 混成一句「内核未安排」，正是「进化跑没跑」看不清的来源。
 */
function untilNext(ms: number | null | undefined): string {
  if (!engineOn.value) return '不会触发（引擎未启用）'
  if (!ms) return '等待首次计时'
  if (ms - Date.now() <= 0) return '即将触发'
  return ttlLeft(ms)
}

/** 深度轮周期：内核上报多少就写多少，旧内核不上报时如实说不知道。 */
const cyclePeriod = computed(() => {
  const s = cycleSecs.value
  if (s <= 0) return '周期未上报'
  const h = Math.round(s / 3600)
  return `周期 ${h} 小时`
})

/** 周期卡副标题：深度轮跑过几轮 + 上轮何时。 */
const cycleDetail = computed(() => {
  const last = evo.value?.status?.lastCycleMs ?? 0
  if (!cycleSeq.value) {
    const started = last ? `，计时自 ${cycleAgo(last)} 起算` : ''
    return engineOn.value
      ? `深度轮尚未跑过第 1 轮${started}`
      : '引擎未启用：不评估、不排深度轮'
  }
  return `已完成 ${cycleSeq.value} 轮，上轮 ${last ? cycleAgo(last) : '时间未上报'}`
})

const emptyPendingText = computed(() => {
  if (!engineOn.value) {
    return '引擎未启用：内核不会评估，也不会产生提案。打开上方「进化引擎」开关后，达标的变异才会出现在这里。'
  }
  if (autoActive.value) {
    return '自动进化开着：内核自己采纳达标的变异，这里正常情况下会一直是空的 —— 采纳结果见上方「已采纳的变异」与下方台账。'
  }
  return `暂无待决提案 —— 评估器发现更优变异时会挂到这里。${cycleDetail.value}，${cyclePeriod.value}。`
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
        <CardHeader label="进化引擎与托管模式" />
        <div class="space-y-2.5 px-1">
          <div class="flex items-center justify-between">
            <div>
              <div class="text-[13px] font-semibold" :class="engineOn ? 'text-up' : 'text-down'">
                {{ engineOn ? '引擎：运行中' : '引擎：未启用' }}
              </div>
              <p class="mt-0.5 text-[11.5px] text-faint-fg">
                {{ engineOn ? '影子变异体在评估（关闭后不再评估、不再采纳）' : '不评估、不采纳、不排深度轮' }}
              </p>
            </div>
            <Switch
              :model-value="engineOn"
              :disabled="busy === 'evolve'"
              :label="engineOn ? '开' : '关'"
              @update:model-value="setEngine"
            />
          </div>
          <div class="flex items-center justify-between border-t border-line pt-2.5">
            <div>
              <div class="text-[13px] font-semibold" :class="autoActive ? 'text-up' : 'text-primary'">
                {{ autoActive ? '自动进化：开' : autoEvolve ? '自动：已记录（引擎关着，不生效）' : '人工拍板' }}
              </div>
              <p class="mt-0.5 text-[11.5px] text-faint-fg">
                {{ autoActive
                  ? '内核自行采纳达标的变异，不等人工'
                  : autoEvolve
                    ? '引擎未启用，自动采纳不会发生'
                    : '达标的变异挂进待决区，等你确认' }}
              </p>
            </div>
            <Switch
              :model-value="autoEvolve"
              :disabled="busy === 'auto-evolve'"
              :label="autoEvolve ? '自动' : '人工'"
              @update:model-value="setAuto"
            />
          </div>
          <p class="border-t border-line pt-2 text-[11px] leading-snug text-faint-fg">
            当前模式：<span class="font-semibold text-primary">{{ modeSummary }}</span>
          </p>
        </div>
      </Card>
      <Card dense>
        <CardHeader label="进化周期" />
        <div class="px-1 text-[13px]">
          <div class="font-semibold" :class="engineOn ? '' : 'text-faint-fg'">
            <Clock class="mr-1 inline size-3.5 text-primary" />距下轮深度进化
            <span class="num">{{ untilNext(evo?.status?.nextCycleAtMs) }}</span>
          </div>
          <p class="mt-0.5 text-[11.5px] text-faint-fg">
            {{ cycleDetail }} · {{ cyclePeriod }}
          </p>
        </div>
      </Card>
      <Card dense>
        <CardHeader label="待决提案" />
        <div class="px-1 text-[13px]">
          <div class="text-[22px] font-bold num">{{ pending.length }}</div>
          <p class="mt-0.5 text-[11.5px] text-faint-fg">
            {{ evo
              ? (autoActive
                ? '自动模式下内核自行采纳，这里应为 0 或短暂停留'
                : `当前 ${pending.length} 个提案待拍板`)
              : '内核未上报进化状态（旧版内核？）' }}
          </p>
        </div>
      </Card>
    </div>

    <!-- 引擎未启用：一切「没动静」的统一解释，放在最显眼处 -->
    <div v-if="evo && !engineOn" class="mt-3.5">
      <AlertBanner tone="warn" title="进化引擎未启用">
        内核当前不评估、不采纳、也不排深度轮 —— 下面的「已采纳的变异」只是历史记录，
        待决提案不会被自动处理（自动开关记录的是模式，不是运行状态）。
        打开上方「进化引擎」开关即可让它跑起来，开关状态会持久化，重启后仍然有效。
        注意：内核上次关机时记下的开关优先于
        <span class="num">user_layer/configs/shadow_evolution.toml</span> 里的
        <span class="num">enabled</span>（那一行只决定首次启动），所以改文件关不掉/开不动它 ——
        想用文件说话，得先删掉 <span class="num">data/evolution/state.json</span>。
      </AlertBanner>
    </div>

    <div v-if="flash" class="mt-3.5">
      <AlertBanner :tone="flash.tone" title="进化操作" dismissible @dismiss="flash = null">
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <!-- 现在生效的是什么 —— 回答「当前策略是哪一版、谁改的、何时改的」 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="已采纳的变异（最近一次，改了什么 / 谁改的 / 何时）">
        <template #action>
          <Badge :variant="liveRows.length ? 'gold' : 'outline'">{{ liveRows.length }} 个策略有采纳记录</Badge>
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
                  <th class="label-micro pb-1">采纳前</th>
                  <th class="label-micro pb-1">采纳后</th>
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
        它是对「采纳发生过、改了什么、谁改的、何时」的凭据 ——
        内核在启动时会按这些记录把参数恢复回来（<span class="num">#245</span> 之后不再回落到声明默认值），
        但快照仍不暴露运行中参数的实时读数，核对实时值请查内核日志或 IPC。
      </p>
    </Card>

    <!-- 待决提案 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="待决提案（变异 vs 现行）">
        <template #action>
          <Badge variant="gold" dot>{{ pending.length }} 个</Badge>
        </template>
      </CardHeader>

      <AlertBanner v-if="autoActive" tone="info" class="mb-3">
        自动进化已开启：内核会自行采纳达标的变异，因此这里不提供「采纳」按钮，只保留拒绝与延后。
        正常情况下待决区会保持为空 —— 刚出现的提案会在下一轮评估里被内核采纳。
        要人工拍板，请先关掉上方「自动」开关。
      </AlertBanner>
      <AlertBanner v-else-if="autoEvolve && !engineOn" tone="warn" class="mb-3">
        自动模式已记录，但引擎未启用：这些提案不会被自动采纳，会一直留到过期（7 天）。
        现在可以人工拍板，或先打开上方「进化引擎」开关让内核自行处理。
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
              <Clock class="mr-1 size-3" />{{ ttlLeft(p.expiresAtMs) }}后过期
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
              v-if="!autoActive"
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
