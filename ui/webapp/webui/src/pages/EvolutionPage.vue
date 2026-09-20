<script setup lang="ts">
/**
 * 进化（E13 #95）— 影子引擎把变异策略作为「提案」递上来，操作员在这里看到
 * 变异策略与当前策略的全面对比后拍板：采纳 / 拒绝 / 延后；勾选自动进化后
 * 内核直接落盘采纳（promotions.jsonl 可一键回滚）。所有动作走网关命令，
 * 与 TUI、命令行共用同一套 `decide` / `auto-evolve` / `rollback` 动词。
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
const pending = computed(() =>
  (evo.value?.proposals ?? []).filter((p) => p.state === 'proposed' || p.state === 'deferred'),
)
const decided = computed(() =>
  (evo.value?.proposals ?? []).filter((p) => !(p.state === 'proposed' || p.state === 'deferred')),
)

const busy = ref<string | null>(null)
const flash = ref<{ ok: boolean; msg: string } | null>(null)

function showFlash(ok: boolean, msg: string): void {
  flash.value = { ok, msg }
  setTimeout(() => { flash.value = null }, 5000)
}

async function act(label: string, fn: () => Promise<unknown>): Promise<void> {
  busy.value = label
  try {
    await fn()
    await store.refresh()
  } catch (e) {
    showFlash(false, e instanceof Error ? e.message : String(e))
  } finally {
    busy.value = null
  }
}

async function setAuto(next: boolean): Promise<void> {
  await act('auto-evolve', () => api.setAutoEvolve(next))
  showFlash(true, next ? '已开启自动进化：内核将直接落盘采纳（可用回滚撤销）' : '已关闭自动进化：每个提案都等你拍板')
}

async function decide(p: EvolutionProposalRow, decision: 'accept' | 'reject' | 'defer'): Promise<void> {
  if (decision === 'accept'
    && !window.confirm(`采纳后立即热更新 ${p.strategy} 的实盘参数（原参数已存档，可一键回滚）。确认采纳 ${p.id} 吗？`)) {
    return
  }
  await act(`${p.id}-${decision}`, () => api.decideEvolution(p.id, decision))
  showFlash(true, {
    accept: `已采纳：${p.strategy} 已切换到变异参数`,
    reject: `已拒绝：${p.strategy} 保持现行参数`,
    defer: `已延后：${p.id} 保留在待决区（7 天内仍可处理）`,
  }[decision])
}

async function rollback(strategy: string): Promise<void> {
  if (!window.confirm(`把 ${strategy} 回滚到上一次采纳前的参数？（只撤销最近一次，更早的可在审计文件中追溯）`)) return
  await act(`rollback-${strategy}`, () => api.rollbackStrategy(strategy))
  showFlash(true, `已回滚：${strategy} 已恢复到上一组参数`)
}

/* ── 展示辅助 ──────────────────────────────────────────────────────────── */

const REASON_LABELS: Record<string, string> = {
  higher_win_rate: '胜率更高',
  better_profit_factor: '盈利因子更优',
  combined_improvement: '综合改善',
}
const reasonLabel = (r: string) => REASON_LABELS[r] ?? r

const STATE_LABELS: Record<string, string> = {
  proposed: '待决',
  deferred: '已延后',
  accepted: '已采纳',
  rejected: '已拒绝',
  expired: '已过期',
  superseded: '已被新提案取代',
}

function ttlLeft(expiresAtMs: number): string {
  const left = expiresAtMs - Date.now()
  if (left <= 0) return '已过期'
  const h = Math.floor(left / 3_600_000)
  if (h >= 48) return `${Math.floor(h / 24)}天${h % 24}小时`
  if (h >= 1) return `${h}小时${Math.floor((left % 3_600_000) / 60_000)}分`
  return `${Math.max(1, Math.round(left / 60_000))}分`
}

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
            <Clock class="mr-1 inline size-3.5 text-primary" />72 小时一轮深度进化
          </div>
          <p class="mt-0.5 text-[11.5px] text-faint-fg">
            上轮 {{ evo?.status?.lastCycleMs ? cycleAgo(evo.status.lastCycleMs) : '尚未运行' }}
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
      <AlertBanner :tone="flash.ok ? 'info' : 'error'" title="进化操作" dismissible @dismiss="flash = null">
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <!-- 待决提案 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="待决提案（变异 vs 现行）">
        <template #action>
          <Badge variant="gold" dot>{{ pending.length }} 个</Badge>
        </template>
      </CardHeader>

      <EmptyState v-if="!pending.length" :loading="store.loading" text="暂无待决提案 —— 评估器发现更优变异时会把它挂在这里" />

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
            <Button size="sm" :disabled="busy === `${p.id}-accept`" @click="decide(p, 'accept')">
              采纳（热更新）
            </Button>
            <Button size="sm" variant="outline" :disabled="busy === `${p.id}-reject`" @click="decide(p, 'reject')">
              拒绝
            </Button>
            <Button size="sm" variant="ghost" :disabled="busy === `${p.id}-defer`" @click="decide(p, 'defer')">
              延后
            </Button>
            <Button
              size="sm"
              variant="ghost"
              :title="`撤销 ${p.strategy} 最近一次已采纳的变异`"
              @click="rollback(p.strategy)"
            >
              <RotateCcw class="size-3.5" />回滚该策略
            </Button>
          </div>
        </div>
      </div>
    </Card>

    <!-- 已决记录 -->
    <Card class="mt-3.5" dense>
      <CardHeader label="已决与过期记录" />
      <EmptyState v-if="!decided.length" text="暂无已决提案" />
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
          <tr v-for="p in decided" :key="p.id" class="border-t border-line">
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
      </p>
    </Card>
  </div>
</template>
