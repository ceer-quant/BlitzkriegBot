<script setup lang="ts">
/**
 * 设置 · 风控与策略 —— 生效风控域的编排壳（参数化补全后的结构）。
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
import { RefreshCw, LockOpen, Minus, Plug } from 'lucide-vue-next'
import {
  api,
  type ExecutionPolicyListDoc, type ExecutionPolicySectionDoc,
  type ExecutionPolicyPreviewDoc, type ExecutionPolicyHistoryDoc,
  type ExecutionPolicySetParams, type PolicyRuleView,
} from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { isConfigGate } from '@/lib/safety'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import SettingsRiskLimits from './SettingsRiskLimits.vue'
import SettingsExecutionPolicy from './SettingsExecutionPolicy.vue'
import PolicyRulesEditor from './PolicyRulesEditor.vue'

const store = usePanelStore()
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

// ── 账户冻结解冻（issue 943，红色横幅的另一半；issue 1044 呈现真相）─────────
// 风控冻结（accountGates，内核 entry gate 簿）：账户因 venue-rejection 连拒
// 或日内亏损熔断被冻结，拒开仓、平仓照放；解冻走 account.unfreeze（issue
// 1069），内核先 persist 配置态再清闸门，回执带 entriesAllowed，面板必读
// 该字段，不得把「发出请求」当作「已解冻」。冻结持久，仅自动冻结可被
// 健康探针自愈；不存在按会期自动解除，亦不随节点重启自动清。
// 配置态停用（account.status === 'frozen'）：来自账户配置（内核持久存储），与
// 风控冻结是两回事，只读呈现「配置停用」，不提供内核解冻按钮（需改
// 账户配置并重启）。
const freezeOperateId = ref<string | null>(null)

// 风控冻结账户（可解冻）：唯一来源是内核 entry gate 簿 stats.accountGates，
// 取 blocked === true 的条目。配置态闸门（read_only / suspended / trading
// disabled）不列在此处——它们不提供内核解冻，只在配置卡下方以配置语义
// 提示呈现。
const frozenAccounts = computed<{ id: string; reason: string }[]>(() => {
  const gates = store.snapshot?.stats?.accountGates
  return gates
    ? gates
        .filter(
          (g) =>
            g.blocked === true &&
            g.account &&
            g.account !== 'global' &&
            // 配置态闸门不出现在可解冻列表：改账户配置并重启，面板无解冻门
            !isConfigGate(g.reasonKind)
        )
        .sort((a, b) => a.account.localeCompare(b.account))
        .map((g) => ({ id: g.account, reason: g.reason || '原因未上报' }))
    : []
})

// 配置态闸门（read_only / suspended / trading disabled）：来自 accountGates 的
// reasonKind，不可从面板解冻，只读呈现配置语义提示。
const configGateAccounts = computed<{ id: string; reason: string; kind: string }[]>(() => {
  const gates = store.snapshot?.stats?.accountGates
  return gates
    ? gates
        .filter(
          (g) =>
            g.blocked === true &&
            g.account &&
            g.account !== 'global' &&
            isConfigGate(g.reasonKind)
        )
        .sort((a, b) => a.account.localeCompare(b.account))
        .map((g) => ({
          id: g.account,
          reason: g.reason || '原因未上报',
          kind: g.reasonKind ?? '',
        }))
    : []
})

// 配置态停用账户（不可从面板解冻）：来源 account.status === 'frozen'。
const configuredFrozenAccounts = computed<{ id: string; reason: string }[]>(() => {
  return store.accounts
    .filter((a) => a.status === 'frozen')
    .map((a) => ({ id: a.id, reason: a.statusReason || '配置态停用（需改账户配置并重启）' }))
})

async function unfreeze(accountId: string): Promise<void> {
  if (freezeOperateId.value) return
  freezeOperateId.value = accountId
  policyErr.value = null
  policyMsg.value = null
  try {
    const r = await api.accountUnfreeze({ accountId })
    if (r.error) throw new Error(r.error)
    // entriesAllowed 是内核的即时「可开新单」判断：全局 kill 期间它会为 false，
    // 而 freeze book / dailyLoss 都盖不住全局 kill（issue 1062 回滚 F3 后 accountGates
    // 已过滤 GLOBAL_KILL_ACCOUNT）。不得把请求发出当作「已解冻」。
    if (r.entriesAllowed === false) {
      policyErr.value = `解冻 ${accountId} 未生效：内核回报 entriesAllowed=false，仍有全局或其他闸门在，未恢复新单入场。`
      return
    }
    // 复读两个闸门：freeze book + daily loss latch，给出更细化的说明。
    await store.refresh()
    const dailyLoss = store.snapshot?.stats?.dailyLoss
    const acctDaily = dailyLoss?.accounts?.find((a) => a.account === accountId)
    const dailyStillTripped = acctDaily?.tripped ?? false
    const resetAtMs = acctDaily?.resetAtMs ?? null
    // carried_trip 跨日残留：trip 的 day_index 可能早于当前 UTC 日，
    // 此时 resetAtMs 已早于当前时间，闸门实际已到期，不应阻断解冻。
    const now = Date.now()
    const resetPassed = resetAtMs !== null && resetAtMs < now
    const stillInFreezeBook = frozenAccounts.value.some((f) => f.id === accountId)
    if (stillInFreezeBook) {
      policyErr.value = `解冻 ${accountId} 未生效：账户仍在风控冻结书中，请检查内核风控记录。`
      return
    }
    if (dailyStillTripped && resetAtMs && !resetPassed) {
      const resetUtc = new Date(resetAtMs).toISOString().slice(11, 16) // UTC HH:MM
      policyErr.value = `解冻 ${accountId}：风控闸门已解除，但日内亏损熔断仍在，将于 UTC ${resetUtc} 自动恢复入场权限。`
      return
    }
    if (dailyStillTripped && !resetAtMs) {
      policyErr.value = `解冻 ${accountId}：风控闸门已解除，但日内亏损熔断仍在，入场暂未恢复。`
      return
    }
    policyMsg.value = `已解冻 ${accountId}，该账户恢复接新单。`
  } catch (e) {
    policyErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    freezeOperateId.value = null
  }
}

/** 扩展插件启停写（`extension.enable` / `extension.disable`，POST /api/extension/set，
 * 网关路由已存在，之前面板无入口）。body 即 `{name, enabled}` 逐字转发，内核运行
 * on_load/on_unload 并回答 `{name, enabled}`，或命名失败原因。 */
const extensionOperateId = ref<string | null>(null)
const extensionErr = ref<string | null>(null)
const extensionMsg = ref<string | null>(null)

async function setExtension(name: string, enabled: boolean): Promise<void> {
  if (extensionOperateId.value) return
  extensionOperateId.value = name
  extensionErr.value = null
  extensionMsg.value = null
  try {
    const res = await api.extensionSet({ name, enabled, reason: 'panel extension toggle' })
    if (res.error) throw new Error(res.error)
    extensionMsg.value = `扩展 ${name} 已${enabled ? '启用' : '停用'}。`
  } catch (e) {
    extensionErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    extensionOperateId.value = null
    setTimeout(() => { extensionMsg.value = null }, 6000)
  }
}

/** 行情插件选切写（`market.plugin.set`，POST /api/market-plugin/set，网关路由已存在，
 * 之前面板无入口）：body 即 `{name?, reset?, reason?}` 逐字转发。只在下次会话生效，
 * 运行中的会话 feed/discovery/executor 不受影响。回执 `nextSessionPlugin` 为下次会话
 * 将选的插件。 */
const pluginOperateId = ref<string | null>(null)
const pluginErr = ref<string | null>(null)
const pluginMsg = ref<string | null>(null)

async function setMarketPlugin(name: string | undefined, reset: boolean): Promise<void> {
  if (pluginOperateId.value) return
  pluginOperateId.value = name ?? 'reset'
  pluginErr.value = null
  pluginMsg.value = null
  try {
    const res = await api.marketPluginSet({ name, reset, reason: 'panel plugin toggle' })
    if (res.error) throw new Error(res.error)
    pluginMsg.value = reset
      ? '行情插件持久格已清，下次会话回落至启动分辨。'
      : `行情插件下次会话将切至 ${res.nextSessionPlugin || name}。`
  } catch (e) {
    pluginErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    pluginOperateId.value = null
    setTimeout(() => { pluginMsg.value = null }, 6000)
  }
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

    <!-- issue 943 / issue 1044: 风控冻结账户（可从面板解冻；唯一来源是内核 freeze book stats.accountFreezes） -->
    <Card v-if="frozenAccounts.length" class="mt-3.5 mb-4">
      <CardHeader label="风控冻结中的账户">
        <template #title>
          <LockOpen class="size-4 shrink-0 text-down" />
        </template>
      </CardHeader>
      <p class="mt-3 text-[11px] leading-snug text-muted-fg">
        风控冻结 = 该账户因场地连拒或日内亏损熔断被拒新单，平仓照放；只影响这一个账户。
        冻结持久，需到本卡「解冻」运营解锁，或由健康探针自愈（仅自动冻结）；不存在按会期自动解除，亦不随节点重启自动清。
      </p>
      <div
        v-for="fa in frozenAccounts"
        :key="fa.id"
        class="mt-3 flex flex-wrap items-center gap-3 rounded-lg border border-line bg-panel-2/40 px-3 py-2.5"
      >
        <LockOpen class="size-3.5 shrink-0 text-down" />
        <span class="text-[12.5px] font-semibold text-fg truncate min-w-[80px]">{{ fa.id }}</span>
        <span class="min-w-0 flex-1 truncate text-[11px] text-muted-fg" :title="fa.reason">
          {{ fa.reason || '原因未上报' }}
        </span>
        <Button
          variant="danger"
          size="sm"
          :disabled="freezeOperateId !== null"
          @click="unfreeze(fa.id)"
        >
          {{ freezeOperateId === fa.id ? '解冻中…' : '解冻' }}
        </Button>
      </div>
    </Card>

    <!-- 插件与扩展启停（网关写路由补前端入口）：extension.enable/disable 与 market.plugin.set 此前
         只有后端路由，面板无入口。此处补显式开关，每次操作读回执，成功或失败可见；不与注册态的只读
         显示混为一谈。 -->
    <Card class="mt-3.5 mb-4">
      <CardHeader label="插件与扩展启停">
        <template #title>
          <Plug class="size-4 shrink-0 text-primary" />
        </template>
      </CardHeader>
      <p class="mt-3 text-[11px] leading-snug text-muted-fg">
        扩展插件启用/停用 与 行情插件下次会话选切，走网关已存在的写路由（extension.set / market-plugin.set），
        每次操作读回执，成功或失败可见；不与注册态的只读显示混为一谈。
      </p>

      <!-- 扩展插件启停：逐个按钮，点击即调用 extension.enable/disable 并读回执。 -->
      <div v-if="store.plugins" class="mt-3 flex flex-wrap gap-2">
        <template v-for="ext in store.plugins.extensions" :key="ext.name">
          <span class="flex items-center gap-2">
            <span class="text-[11px] text-muted-fg shrink-0">{{ ext.name }}</span>
            <Button
              size="sm"
              :disabled="extensionOperateId !== null || pluginOperateId !== null"
              :class="ext.enabled ? 'bg-up/10 text-up' : 'bg-down/10 text-down'"
              @click="setExtension(ext.name, !ext.enabled)"
            >{{ ext.enabled ? '停用' : '启用' }}</Button>
          </span>
        </template>
      </div>

      <!-- 行情插件选切：仅在下次会话生效，运行中的会话不受影响。 -->
      <div v-if="store.plugins" class="mt-3 flex flex-wrap gap-2 items-start">
        <span class="text-[11px] text-muted-fg shrink-0 mt-0.5">行情插件（下次会话生效）：</span>
        <template v-for="p in store.plugins.marketPlugins" :key="p.name">
          <Button
            size="sm"
            :disabled="pluginOperateId !== null || extensionOperateId !== null"
            :class="p.active ? 'bg-up/10 text-up' : 'bg-default/10 text-default'"
            @click="setMarketPlugin(p.name, false)"
          >{{ p.name }}</Button>
        </template>
        <Button
          size="sm"
          :disabled="pluginOperateId !== null || extensionOperateId !== null"
          @click="setMarketPlugin(undefined, true)"
        >重置持久格</Button>
      </div>

      <span v-if="extensionErr" class="mt-1 block text-[10.5px] text-down">{{ extensionErr }}</span>
      <span v-else-if="extensionMsg" class="mt-1 block text-[10.5px] text-up">{{ extensionMsg }}</span>
      <span v-if="pluginErr" class="mt-1 block text-[10.5px] text-down">{{ pluginErr }}</span>
      <span v-else-if="pluginMsg" class="mt-1 block text-[10.5px] text-up">{{ pluginMsg }}</span>
    </Card>

    <!-- 配置态停用账户（不可从面板解冻；来源 account.status === 'frozen'，与风控冻结是两回事） -->
    <Card v-if="configuredFrozenAccounts.length" class="mt-3.5 mb-4">
      <CardHeader label="配置停用的账户">
        <template #title>
          <Minus class="size-4 shrink-0 text-muted-fg" />
        </template>
      </CardHeader>
      <p class="mt-3 text-[11px] leading-snug text-muted-fg">
        配置停用 = 该账户的账户状态被配置为停用（status=Frozen 或 trading_enabled=false），内核拒绝新单；
        此状态由账户配置决定、重启后生效，面板不提供内核解冻（改账户配置并重启即可）。
      </p>
      <div
        v-for="fa in configuredFrozenAccounts"
        :key="fa.id"
        class="mt-3 flex flex-wrap items-center gap-3 rounded-lg border border-line bg-panel-2/40 px-3 py-2.5"
      >
        <Minus class="size-3.5 shrink-0 text-muted-fg" />
        <span class="text-[12.5px] font-semibold text-fg truncate min-w-[80px]">{{ fa.id }}</span>
        <span class="min-w-0 flex-1 truncate text-[11px] text-muted-fg" :title="fa.reason">
          {{ fa.reason || '原因未上报' }}
        </span>
        <span class="text-[10px] text-muted-fg">配置停用 · 无面板解冻入口</span>
      </div>
    </Card>

    <!-- 配置态闸门（read_only / suspended / trading disabled）：来自内核 accountGates，
         不可从面板解冻，只读呈现配置语义提示；改账户配置并重启即可恢复。 -->
    <Card v-if="configGateAccounts.length" class="mt-3.5 mb-4">
      <CardHeader label="配置态闸门（不可面板解冻）">
        <template #title>
          <Minus class="size-4 shrink-0 text-muted-fg" />
        </template>
      </CardHeader>
      <p class="mt-3 text-[11px] leading-snug text-muted-fg">
        配置态闸门由账户状态 / 交易开关等账户配置派生，由配置决定、
        重启后生效。面板不提供内核解冻：改账户配置并重启内核即可恢复。平仓照放，
        仅新单被拒。
      </p>
      <div
        v-for="cg in configGateAccounts"
        :key="`${cg.id}-${cg.kind}`"
        class="mt-3 flex flex-wrap items-center gap-3 rounded-lg border border-line bg-panel-2/40 px-3 py-2.5"
      >
        <Minus class="size-3.5 shrink-0 text-muted-fg" />
        <span class="text-[12.5px] font-semibold text-fg truncate min-w-[80px]">{{ cg.id }}</span>
        <span class="min-w-0 flex-1 truncate text-[11px] text-muted-fg" :title="cg.reason">
          {{ cg.reason || '原因未上报' }}
        </span>
        <span class="text-[10px] text-muted-fg">{{ cg.kind === 'ConfigReadOnly' ? 'read_only' : cg.kind === 'ConfigSuspended' ? 'suspended' : 'trading disabled' }} · 无面板解冻入口</span>
      </div>
    </Card>

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
