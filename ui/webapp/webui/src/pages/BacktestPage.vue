<script setup lang="ts">
/**
 * 回测（issue 353）— 傻白甜全流程：拉数据 → 选数据集 → 跑回测 → 看结果，全在浏览器完成。
 * 页面自身不碰本地文件系统：数据集列表 / 拉取 / 回测 / 导出全部经由网关代理的
 * 内核 IPC（backtest.onchain.pull|list、backtest.run|status|result|export），
 * 网关是纯代理、内核是唯一执行者。三张图：资金曲线 / 平仓原因分布 / 入场价格带分布。
 * 报告持久化在 localStorage，刷新不丢；可导出 JSON 或清除。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { Download, HardDriveDownload, Play, RefreshCw, Trash2 } from 'lucide-vue-next'
import { api, ApiError } from '@/api/client'
import type { BacktestDataset, BacktestJobStatusDoc, BacktestResultDoc } from '@/api/client'
import { backtestLabel, loadBacktest, saveBacktest, clearBacktest } from '@/composables/backtestStore'
import type { BacktestReport } from '@/backtest'
import { useChart, areaFade, palette, tooltipStyle, axisX, axisY } from '@/lib/chart'
import { num, money, signedMoney, pct, duration, dateTime, shortAddr } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import Input from '@/components/ui/input/Input.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'

// ── 通用 ────────────────────────────────────────────────────────────────────

const err = ref<string | null>(null)
function setErr(m: string | null): void {
  err.value = m
}
function failMsg(e: unknown): string {
  if (e instanceof ApiError) return e.message
  return e instanceof Error ? e.message : String(e)
}

/** UTC 日历日（内核接受 `YYYY-MM-DD`，含尾日）。 */
function isoDay(d: Date): string {
  return d.toISOString().slice(0, 10)
}

// ── 第一步：数据集 ──────────────────────────────────────────────────────────

const datasets = ref<BacktestDataset[]>([])
const listErr = ref<string | null>(null)
const selected = ref<string | null>(null) // eventsPath — 内核按数据集根校验
const loadingList = ref(false)

const selectedDataset = computed(() => datasets.value.find((d) => d.eventsPath === selected.value) ?? null)

async function refreshDatasets(): Promise<void> {
  loadingList.value = true
  try {
    const doc = await api.backtestOnchainList()
    datasets.value = doc.datasets ?? []
    listErr.value = doc.error ?? null
    if (!selected.value) selected.value = datasets.value[0]?.eventsPath ?? null
  } catch (e) {
    listErr.value = failMsg(e)
  } finally {
    loadingList.value = false
  }
}

// 拉取表单 — 预填默认值：最近 7 天（UTC）。
const wallet = ref('')
const start = ref(isoDay(new Date(Date.now() - 7 * 86400_000)))
const end = ref(isoDay(new Date()))
const assetsRaw = ref('')
const pulling = ref(false)
const pullDocs = ref<BacktestJobStatusDoc[]>([])
const pullsSettled = ref(false) // 全部落定后只刷新一次列表

const pullBusy = computed(() => pullDocs.value.some((d) => d.state === 'running'))

async function startPull(): Promise<void> {
  setErr(null)
  const w = wallet.value.trim()
  if (!(w.startsWith('0x') && w.length >= 4)) {
    setErr('钱包地址要以 0x 开头（完整地址共 42 个字符）。')
    return
  }
  if (!start.value || !end.value) {
    setErr('请填起止日期。')
    return
  }
  if (start.value > end.value) {
    setErr('开始日期晚于结束日期 — 把两个日期换过来再试。')
    return
  }
  const assets = assetsRaw.value.trim() ? assetsRaw.value.split(/[\s,，、]+/).filter(Boolean) : []
  pulling.value = true
  pullsSettled.value = false
  try {
    const { jobIds } = await api.backtestOnchainPull({ wallet: w, start: start.value, end: end.value, assets })
    pullDocs.value = jobIds.map((jobId) => ({
      jobId,
      kind: 'pull',
      state: 'running',
      phase: 'queued',
      detail: '已提交，等待内核受理…',
      progress: 0,
      startedAtMs: Date.now(),
      finishedAtMs: null,
      error: null,
    }))
    startPoll()
  } catch (e) {
    setErr(`拉取没发出去：${failMsg(e)}`)
  } finally {
    pulling.value = false
  }
}

// ── 第二步：回测配置 ────────────────────────────────────────────────────────

const modeSegments = [
  { id: 'mine', label: '我的摩擦', badge: '默认' },
  { id: 'verify', label: '验证延迟' },
  { id: 'sweep', label: '延迟扫描' },
]
const MODE_HINTS: Record<string, string> = {
  mine: '完全按你配置的滑点重放一遍 — 结果最接近「如果实盘」。',
  verify: '在指定延迟（默认 286ms，香港 VPS 实测往返）下再跑一遍，看利润是否扛得住。',
  sweep: '从 0ms 逐档加延迟跑出一张阶梯表 — 判断策略是延迟敏感还是稳。',
}
const mode = ref('mine')
const modeHint = computed(() => MODE_HINTS[mode.value] ?? '')
const slippageTicks = ref('1')
const verifyLatencyMs = ref('286')
const strategiesRaw = ref('')

const btDoc = ref<BacktestJobStatusDoc | null>(null)
const lastResult = ref<BacktestResultDoc | null>(null)
const lastBtId = ref<string | null>(null)
const canRun = computed(() => selected.value !== null && btDoc.value?.state !== 'running')

async function runBacktest(): Promise<void> {
  setErr(null)
  const archive = selected.value
  if (!archive) {
    setErr('先在第一步选择一个数据集（或先拉一次数据）。')
    return
  }
  const slip = Math.floor(Number(slippageTicks.value))
  if (!Number.isFinite(slip) || slip < 1) {
    setErr('滑点至少 1 tick — 0 滑点会把回测吹成神话，内核同样会拒绝。')
    return
  }
  const verify = mode.value === 'verify' ? Math.floor(Number(verifyLatencyMs.value)) || 286 : undefined
  const strategies = strategiesRaw.value.trim()
    ? strategiesRaw.value.split(/[\s,，、]+/).filter(Boolean)
    : undefined
  try {
    const { jobId } = await api.backtestRun({
      archive,
      mode: mode.value,
      slippageTicks: slip,
      verifyLatencyMs: verify,
      strategies,
    })
    lastBtId.value = jobId
    btDoc.value = {
      jobId,
      kind: 'backtest',
      state: 'running',
      phase: 'replay',
      detail: '已提交，引擎正在虚拟时钟里重放数据集…',
      progress: 0,
      startedAtMs: Date.now(),
      finishedAtMs: null,
      error: null,
    }
    startPoll()
  } catch (e) {
    setErr(`回测没发出去：${failMsg(e)}`)
  }
}

const MODE_NAMES: Record<string, string> = { mine: '我的摩擦', verify: '验证延迟', sweep: '延迟扫描' }
function modeName(m: string): string {
  return MODE_NAMES[m] ?? m
}

// ── 轮询：~1s 一次 backtest.status，直到任务落定 ────────────────────────────

let timer: number | null = null
function startPoll(): void {
  if (timer === null) timer = window.setInterval(pollOnce, 1000)
}
function stopPoll(): void {
  if (timer !== null) {
    window.clearInterval(timer)
    timer = null
  }
}
onUnmounted(stopPoll)

async function pollOnce(): Promise<void> {
  for (const d of pullDocs.value) {
    if (d.state !== 'running') continue
    try {
      Object.assign(d, await api.backtestStatus(d.jobId))
    } catch {
      /* 瞬时网络抖动 — 下一轮再试 */
    }
  }
  if (pullDocs.value.length > 0 && !pullBusy.value && !pullsSettled.value) {
    pullsSettled.value = true
    await refreshDatasets()
  }

  const b = btDoc.value
  if (b?.state === 'running') {
    try {
      const nd = await api.backtestStatus(b.jobId)
      btDoc.value = nd
      if (nd.state === 'done') await collectResult(nd.jobId)
    } catch {
      /* 继续轮询 */
    }
  }
  if (!pullBusy.value && btDoc.value?.state !== 'running') stopPoll()
}

async function collectResult(id: string): Promise<void> {
  try {
    const res = await api.backtestResult(id)
    lastResult.value = res
    report.value = res.report
    saveBacktest(res.report)
  } catch (e) {
    setErr(`结果取回失败：${failMsg(e)} — 任务已完成，稍后重试导出即可。`)
  }
}

// ── 导出 / 清除 ─────────────────────────────────────────────────────────────

const exporting = ref(false)
async function exportReport(): Promise<void> {
  if (!lastBtId.value) return
  exporting.value = true
  try {
    const doc = await api.backtestExport(lastBtId.value)
    const blob = new Blob([doc.content], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = doc.fileName
    a.click()
    URL.revokeObjectURL(url)
  } catch (e) {
    setErr(`导出失败：${failMsg(e)}`)
  } finally {
    exporting.value = false
  }
}

function onClear(): void {
  report.value = null
  lastResult.value = null
  clearBacktest()
}

// ── 报告（localStorage 恢复）与派生序列 ─────────────────────────────────────

const report = ref<BacktestReport | null>(loadBacktest())

/** 内核口径的已实现净盈亏（已扣手续费）— 决定曲线颜色。 */
const netPnl = computed(() => Number(report.value?.trades?.netPnlUsd ?? 0))

/** 超出 tradeLines 容量的笔数 — 曲线来自截断后的列表。 */
const truncated = computed(() => Number(report.value?.tradeLinesTruncated ?? 0))

/** 资金曲线：逐笔累计净盈亏，带 0 起点。 */
const equity = computed<number[]>(() => {
  const r = report.value
  if (!r) return []
  let cum = 0
  return [0, ...r.tradeLines.reduce<number[]>((acc, t) => {
    cum += Number(t.netPnlUsd) || 0
    acc.push(cum)
    return acc
  }, [])]
})

interface DistRow {
  label: string
  count: number
  avg: number
}

/** 平仓原因分布：按 reason 聚合，桶色按该原因的平均盈亏符号。 */
const reasonDist = computed<DistRow[]>(() => {
  const m = new Map<string, { count: number; pnl: number }>()
  for (const t of report.value?.tradeLines ?? []) {
    const k = String(t.reason || '未注明')
    const e = m.get(k) ?? { count: 0, pnl: 0 }
    e.count += 1
    e.pnl += Number(t.netPnlUsd) || 0
    m.set(k, e)
  }
  return [...m.entries()]
    .map(([label, v]) => ({ label, count: v.count, avg: v.count ? v.pnl / v.count : 0 }))
    .sort((a, b) => b.count - a.count)
    .slice(0, 10)
})

function fmtPrice(p: number): string {
  return p.toFixed(p >= 1 ? 2 : p >= 0.01 ? 4 : 6)
}

/** 入场价格带分布：entryPrice 分 12 桶，桶色按该桶平均盈亏符号。 */
const bandDist = computed<DistRow[]>(() => {
  const lines = (report.value?.tradeLines ?? [])
    .map((t) => ({ price: Number(t.entryPrice), pnl: Number(t.netPnlUsd) || 0 }))
    .filter((t) => Number.isFinite(t.price) && t.price > 0)
  if (lines.length === 0) return []
  let lo = Infinity
  let hi = -Infinity
  for (const t of lines) {
    if (t.price < lo) lo = t.price
    if (t.price > hi) hi = t.price
  }
  const NB = 12
  if (!(hi > lo)) {
    return [{ label: fmtPrice(lo), count: lines.length, avg: lines.reduce((s, t) => s + t.pnl, 0) / lines.length }]
  }
  const w = (hi - lo) / NB
  const buckets = Array.from({ length: NB }, () => ({ count: 0, pnl: 0 }))
  for (const t of lines) {
    let i = Math.floor((t.price - lo) / w)
    if (i >= NB) i = NB - 1
    if (i < 0) i = 0
    buckets[i].count += 1
    buckets[i].pnl += t.pnl
  }
  return buckets.map((b, i) => ({
    label: fmtPrice(lo + w * (i + 0.5)),
    count: b.count,
    avg: b.count ? b.pnl / b.count : 0,
  }))
})

// ── 三张图（容器 v-show 常驻：ECharts 实例不能挂在被卸载的节点上）──────────

const eqEl = ref<HTMLDivElement | null>(null)
const reasonEl = ref<HTMLDivElement | null>(null)
const bandEl = ref<HTMLDivElement | null>(null)

function eqOption(): EChartsOption {
  const p = palette()
  const c = netPnl.value >= 0 ? p.up : p.down
  return {
    backgroundColor: 'transparent',
    tooltip: { trigger: 'axis', ...tooltipStyle(), valueFormatter: (v) => signedMoney(v) },
    grid: { left: 8, right: 14, top: 18, bottom: 6, containLabel: true },
    xAxis: { ...axisX(equity.value.map((_, i) => String(i))), boundaryGap: false },
    yAxis: axisY(),
    series: [{
      type: 'line',
      data: equity.value,
      smooth: true,
      showSymbol: false,
      lineStyle: { color: c, width: 2 },
      itemStyle: { color: c },
      areaStyle: areaFade(c, '40'),
    }],
  }
}

function distOption(rows: DistRow[]): EChartsOption {
  const p = palette()
  return {
    backgroundColor: 'transparent',
    tooltip: { trigger: 'axis', ...tooltipStyle() },
    grid: { left: 8, right: 14, top: 22, bottom: 6, containLabel: true },
    xAxis: axisX(rows.map((r) => r.label)),
    yAxis: axisY(),
    series: [{
      type: 'bar',
      barMaxWidth: 26,
      data: rows.map((r) => ({
        value: r.count,
        itemStyle: { color: r.avg >= 0 ? p.up : p.down, borderRadius: [3, 3, 0, 0] },
      })),
      label: { show: true, position: 'top', color: p.textDim, fontSize: 10 },
    }],
  }
}
const reasonOption = () => distOption(reasonDist.value)
const bandOption = () => distOption(bandDist.value)

useChart(eqEl, eqOption)
useChart(reasonEl, reasonOption)
useChart(bandEl, bandOption)

onMounted(() => {
  void refreshDatasets()
})
</script>

<template>
  <div class="rise-in space-y-4">
    <AlertBanner
      v-if="err"
      tone="error"
      title="操作没成功"
      :hint="err"
      dismissible
      @dismiss="setErr(null)"
    />

    <!-- 第一步 · 数据源 -->
    <Card>
      <CardHeader label="第一步 · 拉数据">
        <template #action>
          <Button variant="ghost" size="sm" :disabled="loadingList" @click="refreshDatasets">
            <RefreshCw class="size-3.5" />刷新
          </Button>
        </template>
      </CardHeader>

      <div class="space-y-3">
        <AlertBanner
          v-if="listErr"
          tone="warn"
          title="数据集列表读不到"
          :hint="`${listErr} — 确认内核在跑，然后点右上角「刷新」。`"
        />
        <EmptyState
          v-else-if="datasets.length === 0"
          text="还没有链上数据集"
          hint="填下面的钱包和时间范围，点「拉取数据」— 拉完它会自动出现在这里。"
          :loading="loadingList"
        />
        <div v-else class="grid gap-2 md:grid-cols-2">
          <button
            v-for="d in datasets"
            :key="d.dataset"
            type="button"
            class="rounded-md border px-3 py-2 text-left transition-colors"
            :class="selected === d.eventsPath
              ? 'border-primary/55 bg-primary/8'
              : 'border-line bg-panel-2 hover:border-line-strong'"
            @click="selected = d.eventsPath"
          >
            <div class="flex items-center justify-between gap-2">
              <span class="truncate text-[13px] font-medium text-fg">{{ d.dataset }}</span>
              <Badge variant="default">{{ num(d.manifest?.counts?.events) }} 事件</Badge>
            </div>
            <div class="mt-0.5 text-[11.5px] text-faint-fg">
              {{ shortAddr(d.manifest?.wallet) }} · {{ num(d.manifest?.counts?.trades) }} 笔成交 · {{ dateTime(d.manifest?.generatedAtMs) }}
            </div>
          </button>
        </div>

        <!-- 拉取任务进度 -->
        <div v-if="pullDocs.length" class="space-y-1.5 rounded-md border border-line bg-panel-2 p-2.5">
          <div v-for="d in pullDocs" :key="d.jobId" class="flex items-center gap-2 text-[12px]">
            <span
              v-if="d.state === 'running'"
              class="size-3 shrink-0 animate-spin rounded-full border-2 border-line-strong border-t-primary"
            />
            <span v-else-if="d.state === 'done'" class="shrink-0 text-up">✓</span>
            <span v-else class="shrink-0 text-down">✕</span>
            <span class="min-w-0 truncate text-fg">{{ d.detail || d.phase }}</span>
            <span v-if="d.state === 'running'" class="shrink-0 text-faint-fg">{{ num(d.progress) }} 笔</span>
            <span v-if="d.error" class="min-w-0 truncate text-down">{{ d.error }}</span>
          </div>
        </div>

        <div class="grid gap-2 md:grid-cols-4">
          <label class="md:col-span-2">
            <span class="label-micro">钱包地址</span>
            <Input v-model="wallet" placeholder="0x…（要回测的钱包）" size="sm" />
          </label>
          <label>
            <span class="label-micro">开始日期 (UTC)</span>
            <Input v-model="start" type="date" size="sm" />
          </label>
          <label>
            <span class="label-micro">结束日期 (UTC)</span>
            <Input v-model="end" type="date" size="sm" />
          </label>
        </div>
        <div class="flex items-end gap-2">
          <label class="flex-1">
            <span class="label-micro">只拉这些资产（可选，逗号分隔，留空 = 全部）</span>
            <Input v-model="assetsRaw" placeholder="例如 BTC, ETH" size="sm" />
          </label>
          <Button :disabled="pulling || pullBusy" @click="startPull">
            <HardDriveDownload class="size-4" />
            {{ pullBusy ? '拉取中…' : '拉取数据' }}
          </Button>
        </div>
      </div>
    </Card>

    <!-- 第二步 · 跑回测 -->
    <Card>
      <CardHeader label="第二步 · 跑回测" />
      <div class="space-y-3">
        <div>
          <span class="label-micro">回测模式</span>
          <SegmentedControl v-model="mode" :segments="modeSegments" />
          <p class="mt-1.5 text-[12px] text-faint-fg">{{ modeHint }}</p>
        </div>
        <div class="grid gap-2 md:grid-cols-3">
          <label>
            <span class="label-micro">滑点 (tick)</span>
            <Input v-model="slippageTicks" type="number" size="sm" />
          </label>
          <label v-if="mode === 'verify'">
            <span class="label-micro">延迟 (ms)</span>
            <Input v-model="verifyLatencyMs" type="number" size="sm" />
          </label>
          <label :class="mode === 'verify' ? '' : 'md:col-span-2'">
            <span class="label-micro">只跑这些策略（可选，留空 = 当前启用的）</span>
            <Input v-model="strategiesRaw" placeholder="策略名，逗号分隔" size="sm" />
          </label>
        </div>
        <p class="text-[12px] text-faint-fg">
          滑点是诚实摩擦的下限（至少 1 tick）— 策略栏只收名字，任何代码都会被内核拒绝。
        </p>
        <div class="flex items-center gap-2">
          <Button :disabled="!canRun" @click="runBacktest">
            <Play class="size-4" />开始回测
          </Button>
          <span v-if="!selectedDataset" class="text-[12px] text-faint-fg">先在第一步选择一个数据集</span>
          <span v-else class="min-w-0 truncate text-[12px] text-faint-fg">数据集：{{ selectedDataset.dataset }}</span>
        </div>
      </div>
    </Card>

    <!-- 回测进行中 -->
    <Card v-if="btDoc && btDoc.state === 'running'">
      <div class="flex items-center gap-3">
        <span class="size-4 shrink-0 animate-spin rounded-full border-2 border-line-strong border-t-primary" />
        <div class="min-w-0">
          <div class="text-[13px] text-fg">
            回测进行中 — 已处理 <RollingNumber :value="btDoc.progress" /> 个事件
          </div>
          <div class="truncate text-[12px] text-faint-fg">{{ btDoc.detail || btDoc.phase }}</div>
        </div>
      </div>
    </Card>
    <AlertBanner
      v-else-if="btDoc && btDoc.state === 'failed'"
      tone="error"
      title="回测失败了"
      :hint="btDoc.error ?? '未知原因 — 修正参数后重试。'"
    />

    <!-- 结果总览 -->
    <template v-if="report">
      <Card>
        <CardHeader label="回测结果">
          <template #action>
            <Button variant="ghost" size="sm" :disabled="exporting || lastBtId === null" @click="exportReport">
              <Download class="size-3.5" />导出 JSON
            </Button>
            <Button variant="ghost" size="sm" @click="onClear">
              <Trash2 class="size-3.5" />清除
            </Button>
          </template>
        </CardHeader>

        <div class="flex flex-wrap items-center gap-2 text-[12px] text-muted-fg">
          <Badge variant="gold" dot>{{ backtestLabel }}</Badge>
          <Badge v-if="lastResult" variant="outline">{{ modeName(lastResult.mode) }}</Badge>
          <span>
            事件 {{ num(report.sourceStats?.events) }} · 匹配盘口 {{ num(report.feed?.books) }}
            · tick {{ report.tickMs }}ms · 虚拟时长 {{ duration(Math.floor(Number(report.virtualMs ?? 0) / 1000)) }}
          </span>
        </div>

        <AlertBanner
          v-if="lastResult && lastResult.ladder.length > 1"
          tone="info"
          :title="lastResult.verdict"
          hint="阶梯每一档都是完整重放：延迟越高利润越薄是正常物理，关键是哪一档转负。"
        />

        <div v-if="lastResult && lastResult.ladder.length" class="mt-3 overflow-x-auto">
          <table class="w-full text-[12.5px]">
            <thead>
              <tr class="label-micro text-left">
                <th class="py-1.5 pr-3 font-medium">延迟</th>
                <th class="py-1.5 pr-3 font-medium">净盈亏</th>
                <th class="py-1.5 pr-3 font-medium">平仓笔数</th>
                <th class="py-1.5 pr-3 font-medium">胜率</th>
                <th class="py-1.5 font-medium">盈亏比</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="r in lastResult.ladder" :key="r.latencyMs" class="border-t border-line">
                <td class="stat-num py-1.5 pr-3"><RollingNumber :value="r.latencyMs" />ms</td>
                <td class="stat-num py-1.5 pr-3" :class="Number(r.netPnlUsd) >= 0 ? 'text-up' : 'text-down'">
                  <RollingNumber :value="signedMoney(r.netPnlUsd)" />
                </td>
                <td class="stat-num py-1.5 pr-3"><RollingNumber :value="num(r.closed)" /></td>
                <td class="stat-num py-1.5 pr-3"><RollingNumber :value="pct(r.winRatePct)" /></td>
                <td class="stat-num py-1.5">
                  <RollingNumber :value="r.profitFactor != null ? num(r.profitFactor) : '—'" />
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </Card>

      <div class="grid gap-3 md:grid-cols-4">
        <StatTile label="净盈亏" :tone="netPnl >= 0 ? 'up' : 'down'" :value="signedMoney(netPnl)">
          <template #sub>
            毛利 +{{ money(report.trades?.grossProfitUsd) }} · 毛亏 {{ money(report.trades?.grossLossUsd) }}
          </template>
        </StatTile>
        <StatTile label="胜率" :value="pct(report.trades?.winRatePct)">
          <template #sub>
            <span class="text-up">{{ num(report.trades?.wins) }}</span> 胜 ·
            <span class="text-down">{{ num(report.trades?.losses) }}</span> 亏 ·
            共 {{ num(report.trades?.closed) }} 笔
          </template>
        </StatTile>
        <StatTile
          label="盈亏比"
          :value="report.trades?.profitFactor != null ? num(report.trades.profitFactor) : '—'"
        >
          <template #sub>均笔 {{ signedMoney(report.trades?.avgPnlUsd) }}</template>
        </StatTile>
        <StatTile label="最大回撤" :value="money(report.trades?.maxDrawdownUsd)">
          <template #sub>
            占权益峰 {{ pct(report.trades?.maxDrawdownPct) }} · 手续费 {{ money(report.trades?.feesUsd) }}
          </template>
        </StatTile>
      </div>
    </template>
    <EmptyState
      v-else
      text="还没有回测结果"
      hint="在第二步点「开始回测」— 跑完这里会出现净盈亏、胜率和三张图。"
    />

    <!-- 三张图。v-show 而非 v-if：容器必须常驻，否则 ECharts 实例会挂在被卸载的节点上。 -->
    <div v-show="report" class="space-y-3">
      <Card>
        <CardHeader label="资金曲线">
          <template #action>
            <span class="text-[11.5px] text-faint-fg">逐笔累计净盈亏（已扣手续费）</span>
          </template>
        </CardHeader>
        <div ref="eqEl" class="h-56 w-full" />
        <p v-if="truncated > 0" class="mt-1 text-[11.5px] text-faint-fg">
          曲线只画了报告保留的成交明细（另有 {{ num(truncated) }} 笔超出容量未画）；上面的汇总数字仍是全量的。
        </p>
      </Card>
      <div class="grid gap-3 md:grid-cols-2">
        <Card>
          <CardHeader label="平仓原因分布">
            <template #action>
              <span class="text-[11.5px] text-faint-fg">绿 = 平均赚钱 · 红 = 平均亏钱</span>
            </template>
          </CardHeader>
          <div ref="reasonEl" class="h-56 w-full" />
        </Card>
        <Card>
          <CardHeader label="入场价格带分布">
            <template #action>
              <span class="text-[11.5px] text-faint-fg">按入场价分桶 · 桶色 = 该桶平均盈亏</span>
            </template>
          </CardHeader>
          <div ref="bandEl" class="h-56 w-full" />
        </Card>
      </div>
    </div>
  </div>
</template>
