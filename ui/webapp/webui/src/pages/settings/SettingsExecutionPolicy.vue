<script setup lang="ts">
/**
 * 生效风控 · 执行策略编辑器（Issue 364）。
 *
 * 纪律与后端同一条：浏览器不读 TOML —— 生效视图是内核 `execution_policy.get`
 * 自己折叠出来的，写路径是 `set`/`reset`（内核落盘→重读→落审计后才应答），
 * 预览是内核自己的 evaluate 重放最近已平仓交易。保存成功后重新 get：页面永远
 * 渲染「内核确认过的」状态，不渲染本地猜测。规则列表/构建器/预览/保存条在
 * PolicyRulesEditor，字段词表在 lib/policy-fields.ts。
 */
import { computed, onMounted, ref } from 'vue'
import { RefreshCw, ShieldCheck } from 'lucide-vue-next'
import {
  api,
  type ExecutionPolicyListDoc, type ExecutionPolicySectionDoc,
  type ExecutionPolicyPreviewDoc, type ExecutionPolicyHistoryDoc,
  type ExecutionPolicySetParams, type PolicyRuleView,
} from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import PolicyRulesEditor from './PolicyRulesEditor.vue'

const policyList = ref<ExecutionPolicyListDoc | null>(null)
const policySection = ref<ExecutionPolicySectionDoc | null>(null)
const policyPreview = ref<ExecutionPolicyPreviewDoc | null>(null)
const policyHistory = ref<ExecutionPolicyHistoryDoc>([])
const policyAccountId = ref<string>('defaults')
const policyErr = ref<string | null>(null)
const policyMsg = ref<string | null>(null)
const policyBusy = ref(false)
const policySaving = ref(false)

/** chips：固定的 defaults + 内核报来的账户列表。 */
const policyChips = computed(() => [
  { id: 'defaults', label: 'defaults' },
  ...(policyList.value?.accounts ?? []).map((a) => ({
    id: a.accountId,
    label: a.accountId,
  })),
])

/** 本地编辑缓冲：进入页面/切换账户时从生效视图拷贝，保存时整体写回。 */
const editBudgetRatio = ref('')
const editMinBudgetUsd = ref('')
const editMaxBudgetUsd = ref('')
const editMinEquityUsd = ref('')
/** Input 组件以 string 过线（decimals 本就是字符串约定），保存时再解析。 */
const editMaxPositionsPerAsset = ref('1')
const editRules = ref<PolicyRuleView[]>([])
const editDirty = ref(false)

function loadEditorFromSection(doc: ExecutionPolicySectionDoc | null): void {
  editBudgetRatio.value = doc?.budgetRatio ?? ''
  editMinBudgetUsd.value = doc?.minBudgetUsd ?? ''
  editMaxBudgetUsd.value = doc?.maxBudgetUsd ?? ''
  editMinEquityUsd.value = doc?.minEquityUsd ?? ''
  editMaxPositionsPerAsset.value = String(doc?.maxPositionsPerAsset ?? 1)
  editRules.value = (doc?.rules ?? []).map((r) => structuredClone(r))
  editDirty.value = false
}

async function loadPolicy(): Promise<void> {
  policyBusy.value = true
  try {
    const list = await api.executionPolicyList()
    if (list.error) throw new Error(list.error)
    policyList.value = list
    const section = await api.executionPolicyGet(policyAccountId.value)
    policySection.value = 'error' in (section as object) ? null : section
    loadEditorFromSection(section)
    try {
      policyPreview.value = await api.executionPolicyPreview(policyAccountId.value)
    } catch { /* preview 缺席不阻塞页面（无已平仓交易/无策略都可能出现） */ }
    policyHistory.value = await api.executionPolicyHistory(policyAccountId.value)
    policyErr.value = null
  } catch (e) {
    policyErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    policyBusy.value = false
  }
}
onMounted(() => void loadPolicy())

async function switchAccount(id: string): Promise<void> {
  if (policyBusy.value || id === policyAccountId.value) return
  policyAccountId.value = id
  await loadPolicy()
}

/** 「恢复 defaults 后自定义」：把当前编辑缓冲清回全局段的值（仍需保存落盘）。 */
function resetToDefaultsLocal(): void {
  loadEditorFromSection(policyList.value?.defaults ?? null)
  editDirty.value = true
  policyMsg.value = '已按 defaults 重置编辑区 —— 保存后才写入内核。'
}

// ── 保存 / 回滚 ──
function buildSetParams(): ExecutionPolicySetParams {
  const params: ExecutionPolicySetParams = { accountId: policyAccountId.value }
  if (editBudgetRatio.value) params.budgetRatio = editBudgetRatio.value
  if (editMinBudgetUsd.value) params.minBudgetUsd = editMinBudgetUsd.value
  if (editMaxBudgetUsd.value) params.maxBudgetUsd = editMaxBudgetUsd.value
  if (editMinEquityUsd.value) params.minEquityUsd = editMinEquityUsd.value
  const maxPos = Number(editMaxPositionsPerAsset.value)
  if (Number.isInteger(maxPos) && maxPos > 0) params.maxPositionsPerAsset = maxPos
  params.rules = editRules.value
  return params
}

/** 保存 = set（内核验证→落盘→重读→审计），成功后重新 get 刷新整个视图。 */
async function savePolicy(): Promise<void> {
  policySaving.value = true
  policyMsg.value = null
  try {
    const res = await api.executionPolicySet(buildSetParams())
    if (res.error) throw new Error(res.error)
    editDirty.value = false
    policyMsg.value = `已保存（${policyAccountId.value}）—— 内核已重读生效。`
    await loadPolicy()
  } catch (e) {
    policyErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    policySaving.value = false
  }
}

/** 回滚 = 取该账户最近一次 set 的 before 状态写回（版本号 = 审计记录数）。 */
const policyVersion = computed(() => policyHistory.value.length)
async function rollbackPolicy(): Promise<void> {
  const lastSet = [...policyHistory.value]
    .reverse()
    .find((l) => l.action === 'set' && l.before)
  if (!lastSet?.before) {
    policyMsg.value = '没有可回滚的历史版本（该账户还没有 set 记录）。'
    return
  }
  const b = lastSet.before
  const params: ExecutionPolicySetParams = { accountId: policyAccountId.value }
  if (b.budgetRatio) params.budgetRatio = b.budgetRatio
  if (b.minBudgetUsd) params.minBudgetUsd = b.minBudgetUsd
  if (b.maxBudgetUsd) params.maxBudgetUsd = b.maxBudgetUsd
  if (b.minEquityUsd) params.minEquityUsd = b.minEquityUsd
  if (b.maxPositionsPerAsset) params.maxPositionsPerAsset = b.maxPositionsPerAsset
  params.rules = b.rules ?? []
  policySaving.value = true
  policyMsg.value = null
  try {
    const res = await api.executionPolicySet(params)
    if (res.error) throw new Error(res.error)
    editDirty.value = false
    policyMsg.value = `已回滚（${policyAccountId.value}）到 v${policyVersion.value - 1}。`
    await loadPolicy()
  } catch (e) {
    policyErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    policySaving.value = false
  }
}
</script>

<template>
  <Card>
    <CardHeader label="生效风控 · 执行策略">
      <template #title>
        <ShieldCheck class="size-4 text-faint-fg" />
      </template>
      <template #action>
        <Badge :variant="policyVersion > 0 ? 'gold' : 'default'" dot>
          v{{ policyVersion }}
        </Badge>
      </template>
    </CardHeader>

    <p class="text-[11.5px] leading-snug text-muted-fg">
      下单前内核按「基础参数 → 规则（priority 升序，取第一个命中）」裁决每一单：
      放行 / 跳过 / 冷静期。生效视图由内核折叠后下发，浏览器不读配置文件；
      保存后内核验证→落盘→重读→落审计才应答，页面渲染的永远是内核确认过的状态。
    </p>

    <AlertBanner v-if="policyErr" class="mt-3" tone="warn" dismissible @dismiss="policyErr = null">
      {{ policyErr }}
    </AlertBanner>

    <!-- 账户 chips：defaults + 各账户 -->
    <div class="mt-3 flex flex-wrap items-center gap-1.5">
      <button
        v-for="chip in policyChips"
        :key="chip.id"
        class="rounded-full border px-2.5 py-[3px] text-[11px] font-semibold transition-colors"
        :class="chip.id === policyAccountId
          ? 'border-primary/40 bg-primary/14'
          : 'border-line bg-panel-2 text-muted-fg hover:text-fg'"
        :disabled="policyBusy"
        @click="switchAccount(chip.id)"
      >{{ chip.label }}</button>
      <Button variant="outline" size="sm" class="ml-1" :disabled="policyBusy" @click="loadPolicy">
        <RefreshCw class="size-3.5" />刷新
      </Button>
      <span v-if="policyBusy" class="text-[11px] text-faint-fg">读取中…</span>
    </div>

    <!-- 基础参数 -->
    <div v-if="policySection" class="mt-3 grid gap-2 sm:grid-cols-2">
      <label class="block">
        <span class="label-micro">下注比例（balance × ratio）</span>
        <Input v-model="editBudgetRatio" class="mt-1" type="text" placeholder="如 0.02" @input="editDirty = true" />
      </label>
      <label class="block">
        <span class="label-micro">最小下注（USD）</span>
        <Input v-model="editMinBudgetUsd" class="mt-1" type="text" placeholder="如 10" @input="editDirty = true" />
      </label>
      <label class="block">
        <span class="label-micro">最大下注（USD）</span>
        <Input v-model="editMaxBudgetUsd" class="mt-1" type="text" placeholder="如 100" @input="editDirty = true" />
      </label>
      <label class="block">
        <span class="label-micro">最低权益要求（USD）</span>
        <Input v-model="editMinEquityUsd" class="mt-1" type="text" placeholder="如 50" @input="editDirty = true" />
      </label>
      <label class="block">
        <span class="label-micro">单标的最多持仓笔数</span>
        <Input
          v-model="editMaxPositionsPerAsset"
          class="mt-1"
          type="number"
          min="1"
          step="1"
          @input="editDirty = true"
        />
      </label>
      <div class="flex items-end">
        <Button variant="outline" size="sm" :disabled="policyBusy" @click="resetToDefaultsLocal">
          恢复 defaults 后自定义
        </Button>
      </div>
    </div>
    <AlertBanner v-else-if="!policyBusy && !policyErr" class="mt-3" tone="info">
      读不到策略视图 —— 这个内核可能不认识 execution_policy。
    </AlertBanner>

    <!-- 规则编辑器（列表 + 构建器 + 预览 + 保存条）-->
    <div v-if="policySection" class="mt-4">
      <PolicyRulesEditor
        v-model="editRules"
        :preview="policyPreview"
        :version="policyVersion"
        :busy="policyBusy"
        :saving="policySaving"
        @touch="editDirty = true"
        @save="savePolicy"
        @rollback="rollbackPolicy"
        @invalid="policyMsg = $event"
      />
      <p v-if="policyMsg" class="mt-2 text-[11.5px] leading-snug text-muted-fg">{{ policyMsg }}</p>
      <span v-if="editDirty" class="sr-only">有未保存的修改</span>
    </div>
  </Card>
</template>
