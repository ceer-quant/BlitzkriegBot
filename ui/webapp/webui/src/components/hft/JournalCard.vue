<script setup lang="ts">
/**
 * 现金流水卡（account.journal，issue 509 内核 / issue 511 动词 / issue 513
 * 面板消费面）— 钱包重构的最后一公里：操作者能看见每笔钱的来去。折叠卡
 * 默认收起，展开时拉一次 account.journal（limit 100）——流水是审计 trail，
 * 不是行情，不随 2s 快照轮询打。老内核没有该动词时整卡退化为一句升级提示。
 * 九个 JournalKind 的中文标签映射在本卡（单一消费面）。
 */
import { computed, ref, watch } from 'vue'
import { ChevronDown } from 'lucide-vue-next'
import { api, type AccountJournalDoc, type AccountJournalRowDoc } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import { money, signedMoney, clockTime } from '@/lib/format'

const journalOpen = defineModel<boolean>('open', { required: true })

const journalLoading = ref(false)
const journalErr = ref<string | null>(null)
const journalDoc = ref<AccountJournalDoc | null>(null)
const journalRows = computed<AccountJournalRowDoc[]>(() => journalDoc.value?.rows ?? [])

/** 九个内核 JournalKind 的中文标签——固定映射表，一处消费（本卡）。 */
const JOURNAL_KIND_LABELS: Record<string, string> = {
  seed: '注资',
  buy_fill: '买入成交',
  sell_fill: '卖出成交',
  fee: '手续费',
  merge: '合并赎回',
  redemption: '结算赔付',
  cashflow: '返现',
  realign: '对账回正',
  reset: '重置',
}

/**
 * 老内核没有 account.journal 动词 → 网关把 JSON-RPC 错误包成 200 + {error}
 * 文档。照 AccountSwitcher 的容错正则判「未知动词」，整卡退化为一句升级
 * 提示而不是崩——这份 UI 先合，内核合上后自动活。
 */
const journalUnsupported = computed(() => {
  const msg = journalDoc.value?.error ?? journalErr.value ?? ''
  return /unknown|not found|method|未知的|不支持/i.test(msg)
})
const journalFatal = computed(() => {
  if (journalUnsupported.value) return '当前内核不支持流水查看，需要升级内核'
  return journalErr.value ?? journalDoc.value?.error ?? ''
})
const journalKindLabel = (kind: string): string => JOURNAL_KIND_LABELS[kind] ?? kind
/** ref 等宽小字超长截断：悬停看全文。 */
const journalRef = (r: string): string => (r.length > 24 ? `${r.slice(0, 24)}…` : r || '—')
/** 余额列渲染 balanceAfter（decimal 字符串）。 */
const journalBalance = (v: string): string => money(v)

/** 展开时拉一次（缺省账户由内核按 §9.5 解析）；重展开刷新，不加轮询。 */
async function loadJournal(): Promise<void> {
  journalLoading.value = true
  journalErr.value = null
  try {
    journalDoc.value = await api.accountJournal({ limit: 100 })
  } catch (e) {
    journalDoc.value = null
    journalErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    journalLoading.value = false
  }
}
watch(journalOpen, (v) => { if (v) void loadJournal() })
</script>

<template>
  <Card class="mt-3.5" dense>
    <button
      class="flex w-full items-center gap-2 text-left"
      @click="journalOpen = !journalOpen"
    >
      <ChevronDown
        class="size-3.5 text-faint-fg transition-transform"
        :class="journalOpen && 'rotate-180'"
      />
      <span class="label-micro">现金流水</span>
      <span
        v-if="journalDoc?.accountId"
        class="font-mono text-[11px] text-faint-fg"
        :title="'流水所属账户（内核缺省解析：跟随显示账户）'"
      >{{ journalDoc.accountId }}</span>
      <span
        v-if="journalRows.length"
        class="text-[11px] text-faint-fg num"
      ><RollingNumber :value="journalRows.length" /> 笔</span>
      <!-- 口径说明（现金事件流水）：一笔成交拆成买入+卖出两行现金事件，
           与「交易记录」（一笔一行）口径不同 —— 数字对得上，行数对不上
           是本来的形状。悬浮展开。 -->
      <Tooltip content="这里是现金事件流水：一笔成交拆成「买入成交」+「卖出成交」两行（手续费、注资、重置各占一行），与交易记录的一笔一行口径不同——金额逐笔相等，行数天然更多。">
        <span class="cursor-help text-[11px] text-faint-fg underline decoration-dotted decoration-line underline-offset-2">现金事件口径</span>
      </Tooltip>
      <span class="ml-auto text-[11px] text-faint-fg">{{ journalOpen ? '收起' : '展开' }}</span>
    </button>

    <div v-if="journalOpen" class="mt-3">
      <!-- 老内核 / 网络失败：一句话如实说，不崩卡。 -->
      <p v-if="journalFatal" class="text-[12px]" :class="journalUnsupported ? 'text-muted-fg' : 'text-down'">
        {{ journalFatal }}
      </p>
      <p v-else-if="journalLoading && !journalRows.length" class="text-[12px] text-faint-fg">
        加载中…
      </p>
      <template v-else-if="journalRows.length">
        <div class="overflow-x-auto">
          <table class="w-full min-w-[640px] text-[12.5px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">时间</th>
                <th class="label-micro px-2 pb-2">类型</th>
                <th class="label-micro px-2 pb-2 text-right">金额</th>
                <th class="label-micro px-2 pb-2 text-right">余额</th>
                <th class="label-micro px-2 pb-2">ref</th>
              </tr>
            </thead>
            <tbody>
              <template v-for="(r, i) in journalRows" :key="`${r.atMs}-${i}`">
                <!-- 重置分隔线：reset 是「此前一切清账」的硬事实，账目在其
                     前后互不可加和（流水总额与当前余额对不上 = 中间有
                     reset），显性切一刀免得读者硬算一个不存在的总额。 -->
                <tr v-if="r.kind === 'reset' && i > 0" class="border-t border-line">
                  <td colspan="5" class="px-2 py-1 text-center text-[10.5px] text-faint-fg">
                    ── 以下为此前记录 · {{ clockTime(r.atMs) }} 交易环境已重置，余额回到 {{ journalBalance(r.balanceAfter) }} ──
                  </td>
                </tr>
                <tr
                  class="border-t border-line transition-colors hover:bg-panel-2"
                >
                  <td class="whitespace-nowrap px-2 py-2 font-mono text-[11.5px] text-faint-fg">{{ clockTime(r.atMs) }}</td>
                  <td class="px-2 py-2">
                    <Badge variant="outline">{{ journalKindLabel(r.kind) }}</Badge>
                  </td>
                  <td
                    class="px-2 py-2 text-right num font-semibold"
                    :class="Number(r.amount) >= 0 ? 'text-up' : 'text-down'"
                  ><RollingNumber :value="signedMoney(r.amount)" /></td>
                  <td class="px-2 py-2 text-right num text-muted-fg"><RollingNumber :value="journalBalance(r.balanceAfter)" /></td>
                  <td
                    class="max-w-[220px] truncate px-2 py-2 font-mono text-[11px] text-faint-fg"
                    :title="r.ref"
                  >{{ journalRef(r.ref) }}</td>
                </tr>
              </template>
            </tbody>
          </table>
        </div>
        <!-- truncated 披露：读者看到的是内存窗口（JOURNAL_CAP = 10000），不是全史。 -->
        <p v-if="journalDoc?.truncated" class="mt-2 text-[10.5px] text-faint-fg">
          仅显示最近 100 笔（内核窗口 10000）
        </p>
      </template>
      <EmptyState
        v-else
        text="流水为空"
        hint="该账户的账本还没有任何现金变动；启动引擎、注资或成交后这里会出现每一笔。"
        compact
      />
    </div>
  </Card>
</template>
