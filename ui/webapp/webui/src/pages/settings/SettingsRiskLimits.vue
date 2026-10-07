<script setup lang="ts">
/**
 * 生效风控读数 + 编辑（E26 §4.4 / 用户裁决：九项系统限额必须都能改）。每个限额
 * 都是「值 + 来源」，来源不明的数字不是操作者能据以行动的答案。
 *
 * 可编辑：每行一个输入框（预填当前生效值），保存走 risk.setSystemic —— 只发
 * 改动过的字段，内核 plan-first（一项非法整体拒）+ 逐字段审计。效果分级由
 * 内核在回执里逐字段给出：六项 live（立即约束下一笔策略开仓），回撤预算 +
 * 连亏熔断 + 冷静期三项 next_session（本会话不追溯改判，重启后按新值武装）。
 * 未持久化：重启回到启动 flag/env/TOML 的解析结果 —— 来源徽标会变成
 * 「命令行/热改」，这是内核 LimitSource::Flag 的如实呈现。
 */
import { computed, onMounted, ref } from 'vue'
import { RefreshCw, Save, ShieldCheck } from 'lucide-vue-next'
import {
  api,
  type RiskBound, type RiskLimitsDoc, type SystemicLimitUpdateDoc, type ExitSetUpdateDoc,
} from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const riskDoc = ref<RiskLimitsDoc | null>(null)
const riskErr = ref<string | null>(null)
const riskBusy = ref(false)

async function loadRisk(): Promise<void> {
  riskBusy.value = true
  try {
    riskDoc.value = await api.riskLimits()
    riskErr.value = riskDoc.value.error ?? null
    if (riskDoc.value && !riskDoc.value.error) syncBuffers(riskDoc.value)
  } catch (e) {
    riskErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    riskBusy.value = false
  }
}

/** 来源词表：出厂/配置文件/环境变量/命令行或热改，与内核 LimitSource 一致。 */
function sourceLabel(s: RiskBound['source']): string {
  switch (s) {
    case 'default': return '出厂'
    case 'toml': return '配置文件'
    case 'env': return '环境变量'
    case 'flag': return '命令行/热改'
    default: return String(s)
  }
}
/** 0 = 关闭（§4.1 出厂即静默）：显示「关闭」而不是一个伪装成保护措施的 0。 */
function boundArmed(b: RiskBound): boolean {
  return Number(b.value) > 0
}
function boundText(b: RiskBound): string {
  return boundArmed(b) ? b.value : '关闭'
}

/** 一行的定义：wire 字段名（camelCase）+ 展示元数据 + 整数/小数。 */
interface RiskRowDef {
  field: string; label: string; unit: string; group: string; int: boolean
  bound: () => RiskBound | undefined
}
const ROW_DEFS: RiskRowDef[] = [
  { field: 'maxSingleLossUsd', label: '单笔最大亏损', unit: 'USD', group: '账户', int: false, bound: () => riskDoc.value?.account.limits.maxSingleLossUsd },
  { field: 'maxDailyDrawdownUsd', label: '当日最大回撤', unit: 'USD', group: '账户', int: false, bound: () => riskDoc.value?.account.limits.maxDailyDrawdownUsd },
  { field: 'maxPositionSize', label: '单仓上限', unit: '股', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.maxPositionSize },
  { field: 'maxConsecutiveLosses', label: '连亏熔断', unit: '次', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.maxConsecutiveLosses },
  { field: 'cooldownMinutes', label: '熔断冷静期', unit: '分钟', group: '账户', int: true, bound: () => riskDoc.value?.account.limits.cooldownMinutes },
  { field: 'maxTotalPosition', label: '全局总仓位', unit: '笔', group: '全局', int: true, bound: () => riskDoc.value?.global.limits.maxTotalPosition },
  { field: 'maxTotalExposureUsd', label: '全局总敞口', unit: 'USD', group: '全局', int: false, bound: () => riskDoc.value?.global.limits.maxTotalExposureUsd },
  { field: 'maxCorrelationUsd', label: '同资产敞口上限', unit: 'USD', group: '全局', int: false, bound: () => riskDoc.value?.global.limits.maxCorrelationUsd },
  { field: 'globalKillSwitchLossUsd', label: '全局急停亏损', unit: 'USD', group: '全局', int: false, bound: () => riskDoc.value?.global.limits.globalKillSwitchLossUsd },
]

// ── 编辑缓冲 + 保存（risk.setSystemic，只发改动项）──
const editBufs = ref<Record<string, string>>({})
const riskSaving = ref(false)
const riskMsg = ref<string | null>(null)
const riskReceipt = ref<SystemicLimitUpdateDoc | null>(null)
const exitReceipt = ref<ExitSetUpdateDoc | null>(null)

function syncBuffers(d: RiskLimitsDoc): void {
  const next: Record<string, string> = {}
  for (const r of ROW_DEFS) next[r.field] = r.bound()?.value ?? '0'
  // 退出纪律三行同用一块编辑缓冲（field 不与限额行重叠）。
  next['stopLossPct'] = String(d.exit.stopLossPct)
  next['takeProfitPct'] = String(d.exit.takeProfitPct)
  next['forceExitSec'] = String(d.exit.forceExitSec)
  editBufs.value = next
  riskReceipt.value = null
  exitReceipt.value = null
}

/** 下一笔策略开仓就被这个 bound 约束（live）；其余三项重启后武装。 */
const NEXT_SESSION_FIELDS = new Set(['maxDailyDrawdownUsd', 'maxConsecutiveLosses', 'cooldownMinutes'])
function effectLabel(effect: string): string {
  return effect === 'live' ? '立即生效' : effect === 'next_session' ? '重启后生效' : effect
}
function fieldLabel(field: string): string {
  return ROW_DEFS.find((r) => r.field === field)?.label ?? EXIT_ROW_DEFS.find((r) => r.field === field)?.label ?? field
}
/** 回执里的单位随字段：百分比行带 %，强平行带 s。 */
function fieldUnit(field: string): string {
  if (field === 'stopLossPct' || field === 'takeProfitPct') return '%'
  if (field === 'forceExitSec') return 's'
  return ''
}

/** 只发改动过的字段；空串=保持不变。非法输入就地拒绝保存。 */
function buildPatch(): Record<string, string> | string {
  const patch: Record<string, string> = {}
  for (const r of ROW_DEFS) {
    const raw = (editBufs.value[r.field] ?? '').trim()
    if (raw === '') continue // 空串 = 这一行不动
    const cur = r.bound()?.value ?? '0'
    if (raw === cur) continue
    const n = Number(raw)
    if (!Number.isFinite(n) || n < 0) return `${r.label}：必须是 ≥ 0 的数字（0 = 关闭）`
    if (r.int && !Number.isInteger(n)) return `${r.label}：必须是整数`
    patch[r.field] = raw
  }
  return patch
}

// ── 退出纪律（Gate 4 绑定）：止损/止盈/强平三行，走 risk.setExit ──────────
// 与系统限额同卡但不同写路径：退出纪律不是 Bound（没有来源徽标的枚举语义，
// 内核里它是行为旋钮），percentages 拒绝 ≤ 0，forceExitSec 0 = 关闭强平
// （出厂校准值）。生效 = live：新入场下一笔按新值绑定，已开仓位的退出扫描
// 同步跟随 —— 内核回执逐项 old→new。
interface ExitRowDef { field: string; label: string }
const EXIT_ROW_DEFS: ExitRowDef[] = [
  { field: 'stopLossPct', label: '止损' },
  { field: 'takeProfitPct', label: '止盈' },
  { field: 'forceExitSec', label: '强平' },
]
const EXIT_FIELDS = new Set(EXIT_ROW_DEFS.map((r) => r.field))

/** 退出纪律行的当前值文本（单位随行）：输入框的参照。 */
function exitCurrentText(field: string): string {
  const e = riskDoc.value?.exit
  if (!e) return '—'
  if (field === 'stopLossPct') return `%（当前 ${e.stopLossPct}%）`
  if (field === 'takeProfitPct') return `%（当前 ${e.takeProfitPct}%）`
  return `秒（当前 ${e.forceExitSec}s${Number(e.forceExitSec) === 0 ? '，关闭' : ''}）`
}
function exitPlaceholder(field: string): string {
  const e = riskDoc.value?.exit
  if (!e) return ''
  if (field === 'stopLossPct') return String(e.stopLossPct)
  if (field === 'takeProfitPct') return String(e.takeProfitPct)
  return String(e.forceExitSec)
}
/** 退出纪律没有 LimitSource 枚举：热改后 badge 如实写「热改」。 */
function exitSourceLabel(): string {
  return '行为旋钮'
}

/** 退出纪律 patch：只发改动项；百分比必须 > 0，强平 ≥ 0（0 = 关闭）。 */
function buildExitPatch(): Record<string, string | number> | string {
  const e = riskDoc.value?.exit
  if (!e) return '读不到退出纪律当前值'
  const patch: Record<string, string | number> = {}
  const sl = (editBufs.value['stopLossPct'] ?? '').trim()
  if (sl !== '' && String(e.stopLossPct) !== sl) {
    const n = Number(sl)
    if (!Number.isFinite(n) || n <= 0) return '止损：必须是 > 0 的百分比'
    patch.stopLossPct = sl
  }
  const tp = (editBufs.value['takeProfitPct'] ?? '').trim()
  if (tp !== '' && String(e.takeProfitPct) !== tp) {
    const n = Number(tp)
    if (!Number.isFinite(n) || n <= 0) return '止盈：必须是 > 0 的百分比'
    patch.takeProfitPct = tp
  }
  const fe = (editBufs.value['forceExitSec'] ?? '').trim()
  if (fe !== '' && String(e.forceExitSec) !== fe) {
    const n = Number(fe)
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n)) return '强平：必须是 ≥ 0 的整数秒（0 = 关闭强平）'
    patch.forceExitSec = n
  }
  return patch
}

async function saveRisk(): Promise<void> {
  riskMsg.value = null
  riskReceipt.value = null
  exitReceipt.value = null
  const built = buildPatch()
  if (typeof built === 'string') {
    riskMsg.value = built
    return
  }
  const builtExit = buildExitPatch()
  if (typeof builtExit === 'string') {
    riskMsg.value = builtExit
    return
  }
  const keys = Object.keys(built)
  const exitKeys = Object.keys(builtExit)
  if (keys.length === 0 && exitKeys.length === 0) {
    riskMsg.value = '没有改动 —— 九项限额与退出纪律都与内核当前生效值一致。'
    return
  }
  riskSaving.value = true
  try {
    if (keys.length > 0) {
      const res = await api.riskSetSystemic({ ...built, reason: 'webui 面板编辑（生效风控卡）' })
      if (res.error) throw new Error(res.error)
      riskReceipt.value = res
    }
    if (exitKeys.length > 0) {
      const res = await api.riskSetExit({ ...builtExit, reason: 'webui 面板编辑（退出纪律）' })
      if (res.error) throw new Error(res.error)
      exitReceipt.value = res
    }
    await loadRisk()
  } catch (e) {
    riskErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    riskSaving.value = false
  }
}

/** 行渲染：读数（bound）驱动徽标，编辑缓冲驱动输入框。 */
const riskRows = computed(() =>
  ROW_DEFS.map((r) => ({ ...r, bound: r.bound() })))
const armedRows = computed(() => riskRows.value.filter((r) => r.bound && boundArmed(r.bound)))

const riskBadge = computed(() => {
  if (!riskDoc.value) return { text: '不可用', variant: 'default' as const }
  return armedRows.value.length > 0
    ? { text: `已武装 ${armedRows.value.length} 项`, variant: 'gold' as const }
    : { text: '出厂（全部关闭）', variant: 'up' as const }
})
const riskHint = computed(() => {
  if (!riskDoc.value) return ''
  return armedRows.value.length > 0
    ? '六项改动立即约束下一笔策略开仓；回撤 / 连亏 / 冷静期三项重启后生效（本会话不追溯改判）。连亏熔断触发时暂停该账户的新开仓，平仓永不受限。'
    : '九项系统限额全部为 0 = 关闭（出厂承诺）：内核行为与未加风控时逐位一致。保存后六项立即生效。'
})
onMounted(() => void loadRisk())
</script>

<template>
  <Card>
    <CardHeader label="生效风控">
      <template #title>
        <ShieldCheck class="size-4 text-faint-fg" />
      </template>
      <template #action>
        <Badge :variant="riskBadge.variant" dot>{{ riskBadge.text }}</Badge>
      </template>
    </CardHeader>

    <p class="text-[11.5px] leading-snug text-muted-fg">
      内核正在执行的系统性限额（risk.limits）——每个数字都带着它从哪来（出厂 / 配置文件 /
      环境变量 / 热改）。没来源的数字不是能据以行动的答案；0 = 关闭，不是伪装成保护措施的零。
      改动保存进内核内存并逐字段落审计，重启后回到启动配置的解析结果。
    </p>

    <AlertBanner v-if="!riskDoc && !riskErr" class="mt-3" tone="info">正在读取风控读数…</AlertBanner>
    <AlertBanner v-else-if="!riskDoc" class="mt-3" tone="warn">
      读不到风控读数（{{ riskErr ?? '内核离线或版本过旧' }}）。这个内核可能不认识 risk.limits。
    </AlertBanner>

    <template v-else>
      <div class="mt-3">
        <template v-for="(group, gi) in ['账户', '全局']" :key="group">
          <div class="label-micro mt-2 first:mt-0">{{ group }}级限额</div>
          <div class="mt-1">
            <div
              v-for="(row, i) in riskRows.filter((r) => r.group === group)"
              :key="row.field"
              class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
              :class="(gi === 0 && i === 0) || (gi === 1 && i === 0) ? '' : 'border-t border-line'"
            >
              <span class="text-faint-fg">{{ row.label }}</span>
              <span class="flex items-center gap-2">
                <Input
                  v-model="editBufs[row.field]"
                  class="num h-7 w-24 text-right text-[12px]"
                  :type="row.int ? 'number' : 'text'"
                  :min="0"
                  :step="row.int ? 1 : 'any'"
                  :placeholder="row.bound ? boundText(row.bound) : ''"
                  :aria-label="row.label"
                />
                <span v-if="row.bound && boundArmed(row.bound)" class="text-[10.5px] font-normal text-faint-fg">{{ row.unit }}</span>
                <span v-else class="text-[10.5px] font-normal text-faint-fg">关闭</span>
                <Badge :variant="row.bound && boundArmed(row.bound) ? 'gold' : 'default'">{{ row.bound ? sourceLabel(row.bound.source) : '—' }}</Badge>
              </span>
            </div>
          </div>
        </template>

        <div class="label-micro mt-3">退出纪律（Gate 4 绑定）</div>
        <div class="mt-1">
          <div
            v-for="(row, i) in EXIT_ROW_DEFS"
            :key="row.field"
            class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
            :class="i === 0 ? '' : 'border-t border-line'"
          >
            <span class="text-faint-fg">{{ row.label }}</span>
            <span class="flex items-center gap-2">
              <Input
                v-model="editBufs[row.field]"
                class="num h-7 w-24 text-right text-[12px]"
                type="text"
                :min="0"
                step="any"
                :placeholder="exitPlaceholder(row.field)"
                :aria-label="row.label"
              />
              <span class="text-[10.5px] font-normal text-faint-fg">{{ exitCurrentText(row.field) }}</span>
              <Badge variant="gold">{{ exitSourceLabel() }}</Badge>
            </span>
          </div>
        </div>
      </div>

      <!-- issue 393 (①): 账户生效执行参数（可编辑）插进来 —— 有运行时写路径的
           风控参数与系统限额同卡呈现，各归各位（插槽内容来自编排壳）。 -->
      <slot name="params" />

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          variant="outline" size="sm" :disabled="riskBusy" title="重新读取内核的风控读数（放弃未保存的编辑）"
          @click="loadRisk"
        >
          <RefreshCw class="size-3.5" />刷新
        </Button>
        <Button size="sm" :disabled="riskSaving || riskBusy" title="把改动过的限额与退出纪律写入内核（risk.setSystemic / risk.setExit）" @click="saveRisk">
          <Save class="size-3.5" />{{ riskSaving ? '保存中…' : '保存改动' }}
        </Button>
        <span class="text-[11px] text-faint-fg">{{ riskHint }}</span>
      </div>
      <p v-if="riskMsg" class="mt-2 text-[11px] leading-snug text-gold-400">{{ riskMsg }}</p>
      <div v-if="riskReceipt" class="mt-2 rounded border border-line bg-panel-2 px-2.5 py-2 text-[11px] leading-relaxed">
        <div class="font-semibold text-fg">
          已写入 {{ riskReceipt.applied.length }} 项（{{ riskReceipt.persisted ? '已持久化' : '未持久化，重启回到启动配置' }}）
        </div>
        <div v-for="c in riskReceipt.applied" :key="c.field" class="mt-0.5 text-muted-fg">
          {{ fieldLabel(c.field) }}：{{ c.from === '0' ? '关闭' : c.from }} → {{ c.to === '0' ? '关闭' : c.to }}
          <Badge :variant="c.effect === 'live' ? 'gold' : 'default'" class="ml-1">{{ effectLabel(c.effect) }}</Badge>
        </div>
      </div>
      <div v-if="exitReceipt" class="mt-2 rounded border border-line bg-panel-2 px-2.5 py-2 text-[11px] leading-relaxed">
        <div class="font-semibold text-fg">
          退出纪律已写入 {{ exitReceipt.applied.length }} 项（未持久化，重启回到启动配置）
        </div>
        <div v-for="c in exitReceipt.applied" :key="c.field" class="mt-0.5 text-muted-fg">
          {{ fieldLabel(c.field) }}：{{ c.from === '0' ? '关闭' : c.from }}{{ fieldUnit(c.field) }} → {{ c.to === '0' ? '关闭' : c.to }}{{ fieldUnit(c.field) }}
          <Badge variant="gold" class="ml-1">{{ effectLabel(c.effect) }}</Badge>
        </div>
        <div class="mt-1 text-faint-fg">新入场下一笔按新值绑定；已开仓位的退出扫描同步跟随新值。</div>
      </div>
      <p v-if="riskErr" class="mt-2 text-[11px] leading-snug text-down">{{ riskErr }}</p>
    </template>
  </Card>
</template>
