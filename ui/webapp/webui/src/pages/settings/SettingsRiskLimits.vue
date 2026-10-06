<script setup lang="ts">
/**
 * 生效风控读数（E26 §4.4）：内核的 risk.limits（boot 快照，只读）。每个限额
 * 都是「值 + 来源」，来源不明的数字不是操作者能据以行动的答案。九个新限额
 * 改配置需重启内核生效，读数不会中途变陈旧；进页读一次 + 手动刷新。
 *
 * issue 393 (①): 卡片尾部开一个 #params 插槽 —— 账户生效执行参数（budget 三元组
 * / 最低权益 / 单标的持仓笔数）就地渲染为可编辑字段。它们有运行时写路径
 * （execution_policy.set），与上面九项 boot 限额（无写路径，重启生效）在
 * 同一张卡上各归各位；插槽内容（输入与缓冲）由编排壳 SettingsRisk 提供，
 * 本卡保持无 IPC。
 */
import { computed, onMounted, ref } from 'vue'
import { RefreshCw, ShieldCheck } from 'lucide-vue-next'
import { api, type RiskBound, type RiskLimitsDoc } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const riskDoc = ref<RiskLimitsDoc | null>(null)
const riskErr = ref<string | null>(null)
const riskBusy = ref(false)

async function loadRisk(): Promise<void> {
  riskBusy.value = true
  try {
    riskDoc.value = await api.riskLimits()
    riskErr.value = riskDoc.value.error ?? null
  } catch (e) {
    riskErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    riskBusy.value = false
  }
}
onMounted(() => void loadRisk())

/** 来源词表：出厂/配置文件/环境变量/命令行，与内核 boot log 的拼法一致。 */
function sourceLabel(s: RiskBound['source']): string {
  switch (s) {
    case 'default': return '出厂'
    case 'toml': return '配置文件'
    case 'env': return '环境变量'
    case 'flag': return '命令行'
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

/** 账户级五行 + 全局级四行，一行一条：标签 / 值 / 来源。 */
const riskRows = computed(() => {
  const d = riskDoc.value
  if (!d) return []
  const a = d.account.limits
  const g = d.global.limits
  return [
    { label: '单笔最大亏损', unit: 'USD', bound: a.maxSingleLossUsd, group: '账户' },
    { label: '当日最大回撤', unit: 'USD', bound: a.maxDailyDrawdownUsd, group: '账户' },
    { label: '单仓上限', unit: '股', bound: a.maxPositionSize, group: '账户' },
    { label: '连亏熔断', unit: '次', bound: a.maxConsecutiveLosses, group: '账户' },
    { label: '熔断冷静期', unit: '分钟', bound: a.cooldownMinutes, group: '账户' },
    { label: '全局总仓位', unit: '笔', bound: g.maxTotalPosition, group: '全局' },
    { label: '全局总敞口', unit: 'USD', bound: g.maxTotalExposureUsd, group: '全局' },
    { label: '同资产敞口上限', unit: 'USD', bound: g.maxCorrelationUsd, group: '全局' },
    { label: '全局急停亏损', unit: 'USD', bound: g.globalKillSwitchLossUsd, group: '全局' },
  ]
})

/** 武装摘要：任一限额非零即「已武装」，否则出厂静默。 */
const riskArmedCount = computed(() =>
  riskRows.value.filter((r) => boundArmed(r.bound)).length)
const riskBadge = computed(() => {
  if (!riskDoc.value) return { text: '不可用', variant: 'default' as const }
  return riskArmedCount.value > 0
    ? { text: `已武装 ${riskArmedCount.value} 项`, variant: 'gold' as const }
    : { text: '出厂（全部关闭）', variant: 'up' as const }
})
const riskHint = computed(() => {
  if (!riskDoc.value) return ''
  return riskArmedCount.value > 0
    ? '新限额改配置后需重启内核生效；连亏熔断触发时暂停该账户的新开仓，平仓永不受限。'
    : '九项系统限额全部为 0 = 关闭（出厂承诺）：内核行为与未加风控时逐位一致，改配置需重启内核。'
})
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
      环境变量 / 命令行）。没来源的数字不是能据以行动的答案；0 = 关闭，不是伪装成保护措施的零。
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
              :key="row.label"
              class="flex items-center justify-between gap-3 py-1.5 text-[12px]"
              :class="(gi === 0 && i === 0) || (gi === 1 && i === 0) ? '' : 'border-t border-line'"
            >
              <span class="text-faint-fg">{{ row.label }}</span>
              <span class="flex items-center gap-2">
                <span
                  class="num font-semibold"
                  :class="boundArmed(row.bound) ? 'text-fg' : 'text-faint-fg'"
                >{{ boundText(row.bound) }}<span v-if="boundArmed(row.bound)" class="ml-0.5 text-[10.5px] font-normal text-faint-fg">{{ row.unit }}</span></span>
                <Badge :variant="boundArmed(row.bound) ? 'gold' : 'default'">{{ sourceLabel(row.bound.source) }}</Badge>
              </span>
            </div>
          </div>
        </template>

        <div class="label-micro mt-3">退出纪律（Gate 4 绑定）</div>
        <div class="mt-1 flex items-center justify-between gap-3 border-t border-line py-1.5 text-[12px]">
          <span class="text-faint-fg">止损 / 止盈 / 强平</span>
          <span class="num font-semibold">
            {{ riskDoc.exit.stopLossPct }}% / {{ riskDoc.exit.takeProfitPct }}% / {{ riskDoc.exit.forceExitSec }}s
          </span>
        </div>
      </div>

      <!-- issue 393 (①): 账户生效执行参数（可编辑）插进来 —— 有运行时写路径的
           风控参数与 boot 限额同卡呈现，各归各位（插槽内容来自编排壳）。 -->
      <slot name="params" />

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button variant="outline" size="sm" :disabled="riskBusy" title="重新读取内核的风控读数" @click="loadRisk">
          <RefreshCw class="size-3.5" />刷新
        </Button>
        <span class="text-[11px] text-faint-fg">{{ riskHint }}</span>
      </div>
      <p v-if="riskErr" class="mt-2 text-[11px] leading-snug text-down">{{ riskErr }}</p>
    </template>
  </Card>
</template>
