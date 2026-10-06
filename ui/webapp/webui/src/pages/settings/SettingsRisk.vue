<script setup lang="ts">
/**
 * 设置 · 风控与策略 —— 生效风控域的编排壳（issue 393 ①参数化补全后的结构）。
 *
 * 三个部分，一个数据源：
 *   1. 生效风控读数（SettingsRiskLimits）—— 九项系统限额 + 退出纪律，boot
 *      快照，改配置需重启，无运行时写路径（E26 设计如此），保持只读；
 *   2. 账户生效参数 —— budgetRatio/min/max/minEquity/maxPositionsPerAsset，
 *      经由 SettingsRiskLimits 卡的 #params 插槽就地可编辑；
 *   3. 执行策略规则（SettingsExecutionPolicy 的 #rules 插槽）—— 规则编辑器、
 *      预览与审计流水。
 *
 * 为什么壳持有全部状态：`execution_policy.set` 写的是【整个账户段】——
 * 参数与规则同段落盘（内核把段整体替换，漏写 rules 就等于清空规则）。
 * 参数在风控卡编辑、规则在规则卡编辑，但缓冲区只有一份、保存只有一个，
 * 两半永远不可能互相覆盖。写路径仍是 `execution_policy.set`（内核验证→
 * 落盘→重读→落审计后才应答），不新增任何旁路。
 */
import { computed, onMounted, ref } from 'vue'
import { RefreshCw } from 'lucide-vue-next'
import {
  api,
  type ExecutionPolicyListDoc, type ExecutionPolicySectionDoc,
  type ExecutionPolicyPreviewDoc, type ExecutionPolicyHistoryDoc,
  type ExecutionPolicySetParams, type PolicyRuleView,
} from '@/api/client'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import SettingsRiskLimits from './SettingsRiskLimits.vue'
import SettingsExecutionPolicy from './SettingsExecutionPolicy.vue'
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
  <div>
    <AlertBanner v-if="policyErr" class="mb-3.5" tone="warn" dismissible @dismiss="policyErr = null">
      {{ policyErr }}
    </AlertBanner>

    <!-- 账户 chips：defaults + 各账户 —— 一行选择，两张卡（参数/规则）同源。 -->
    <div class="mb-3.5 flex flex-wrap items-center gap-1.5">
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

    <!-- 生效风控读数（只读，boot 快照）+ 就地可编辑的账户生效参数（#params）。 -->
    <SettingsRiskLimits>
      <template #params>
        <div v-if="policySection" class="mt-3 border-t border-line pt-3">
          <div class="label-micro">账户生效参数（execution_policy · 可编辑）</div>
          <p class="mt-1 text-[11px] leading-snug text-faint-fg">
            下单前内核按这组基础参数给每一单定预算、设门槛 —— 与下方规则同属一个账户段，
            一并保存（内核验证→落盘→重读→落审计后才应答）。
          </p>
          <div class="mt-2 grid gap-2 sm:grid-cols-2">
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
            <div class="flex items-end gap-3">
              <Button variant="outline" size="sm" :disabled="policyBusy" @click="resetToDefaultsLocal">
                恢复 defaults 后自定义
              </Button>
              <span v-if="editDirty" class="text-[11px] text-primary">有未保存的修改 —— 到下方保存条落盘</span>
            </div>
          </div>
        </div>
        <AlertBanner v-else-if="!policyBusy && !policyErr" class="mt-3" tone="info">
          读不到策略视图 —— 这个内核可能不认识 execution_policy。
        </AlertBanner>
      </template>
    </SettingsRiskLimits>

    <!-- 执行策略规则：编辑器（列表 + 构建器 + 预览 + 保存条）——同一个账户段
         的另一半，保存按钮在这里，把参数与规则一并写回。 -->
    <SettingsExecutionPolicy
      class="mt-3.5"
      :version="policyVersion"
      :msg="policyMsg"
      @dismiss="policyMsg = null"
    >
      <template #rules>
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
          <span v-if="editDirty" class="sr-only">有未保存的修改</span>
        </div>
      </template>
    </SettingsExecutionPolicy>
  </div>
</template>
