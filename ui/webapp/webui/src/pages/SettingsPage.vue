<script setup lang="ts">
/**
 * 设置 — E8-d 指令面 & 收尾。
 *
 * 五块：
 *   1. 会话与访问 token：状态探测（/api/ping 三态）、token 掩码展示、复制、
 *      带 ?token= 的面板链接复制（E6-a 会话交接通道）、过期语义提示。
 *   2. Gateway 指令台：网关身份（socket/--manage/managed/pid/重启次数）、
 *      status/start/stop 指令（沿用 lifecycle 的能力闸，不重造规则）。
 *   3. 网络诊断：内核拨测它自己出网要走的每条路径（net.check），回答「是网络还是
 *      交易所」。读法全在 lib/net-check.ts —— 状态词 → 中文标签、未知状态原样回显、
 *      空报告与「探测中」都不算通过。
 *   4. 版本与更新：内核自述的版本/构建来源（system.version）+ 更新三态徽章。
 *      三态读法全在 lib/version.ts —— null ≠ false，旧内核明说「不认识」。
 *   5. 外观与节奏：主题三态、提示音、行情快速轮询（设置持久化 store）。
 *
 * 本页只做展示与指令；凭证与下单原语永不出内核。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import {
  Copy, Check, KeyRound, RefreshCw, ShieldCheck, ShieldAlert, ShieldQuestion,
  TerminalSquare, Play, Square, LogOut, Info, Server, Network, Tag,
} from 'lucide-vue-next'
import {
  api, getToken, loginAt, logout, ping, probeSession,
  type CommandDoc, type NetCheckDoc, type RiskBound, type RiskLimitsDoc,
  type ExecutionPolicyListDoc, type ExecutionPolicySectionDoc,
  type ExecutionPolicyPreviewDoc, type ExecutionPolicyHistoryDoc,
  type ExecutionPolicySetParams, type PolicyRuleView,
  type PolicyWhenView, type PolicyThenView,
} from '@/api/client'
import { adjudicateSession } from '@/lib/session'
import { readNetCheck } from '@/lib/net-check'
import { revisionText, versionBadge } from '@/lib/version'
import { usePanelStore } from '@/stores/panel'
import { useSettingsStore } from '@/stores/settings'
import { useTheme, type ThemeMode } from '@/lib/theme'
import { controlState, exitNotice } from '@/lib/lifecycle'
import { dateTime } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()
const settings = useSettingsStore()
const { theme, setTheme, sound, setSoundEnabled } = useTheme()

// ── session state: reachability from ping, the token's fate from a real call ─
type SessionState = 'checking' | 'valid' | 'expired' | 'unreachable'
const sessionState = ref<SessionState>('checking')

async function checkSession(): Promise<void> {
  sessionState.value = 'checking'
  // `ping` answers reachability only — it is deliberately sessionless, so its
  // `authRequired` flag describes the GATEWAY, never our token. Mapping that
  // flag to "expired" made this badge claim 会话已过期 forever on any gateway
  // that requires login (re-logging in could not change it: the probe never
  // looked at the token). The token's verdict comes from an authenticated call.
  sessionState.value = await adjudicateSession(await ping(), probeSession)
}
onMounted(checkSession)

const sessionBadge = computed(() => {
  switch (sessionState.value) {
    case 'valid': return { text: '会话有效', variant: 'up' as const, icon: ShieldCheck }
    case 'expired': return { text: '会话已过期', variant: 'down' as const, icon: ShieldAlert }
    case 'unreachable': return { text: '网关不可达', variant: 'down' as const, icon: ShieldQuestion }
    default: return { text: '检查中…', variant: 'default' as const, icon: RefreshCw }
  }
})
const sessionHint = computed(() => {
  switch (sessionState.value) {
    case 'valid': return '会话由网关签发并保存在网关内存里；网关重启后所有会话失效，需要重新登录。'
    case 'expired': return '当前 token 已被网关拒绝。网关重启、登出或长时间未活动都会导致过期 — 重新登录即可恢复。'
    case 'unreachable': return '连不上网关（/api/ping 无应答）。检查网关进程与端口后重试。'
    default: return ''
  }
})

// ── token display + copy ────────────────────────────────────────────────────
const tokenMasked = computed(() => {
  const t = getToken()
  if (!t) return '（无会话 token）'
  if (t.length <= 10) return `${t.slice(0, 2)}…`
  return `${t.slice(0, 6)}…${t.slice(-4)}`
})
const loginTime = computed(() => {
  const at = loginAt()
  return at ? dateTime(at) : '—（本会话由 ?token= 链接交接或来自旧版本）'
})

const tokenCopied = ref(false)
const linkCopied = ref(false)
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    // Clipboard needs a focused document + permission; a rejected copy is a
    // failed copy, and the button state must not lie about it.
    return false
  }
}
async function copyToken(): Promise<void> {
  if (await copyText(getToken())) {
    tokenCopied.value = true
    setTimeout(() => { tokenCopied.value = false }, 1800)
  }
}
/** ?token= hand-off link — the E6-a channel, now one click away. */
const panelLink = computed(() =>
  `${window.location.origin}${window.location.pathname}?token=${getToken()}`,
)
async function copyLink(): Promise<void> {
  if (await copyText(panelLink.value)) {
    linkCopied.value = true
    setTimeout(() => { linkCopied.value = false }, 1800)
  }
}

// ── gateway console ─────────────────────────────────────────────────────────
const gateway = computed(() => store.snapshot?.gateway ?? null)
const engineUp = computed(() => store.snapshot?.connected ?? false)
/** Reuse the lifecycle rules verbatim — the console must not invent its own. */
const control = computed(() => controlState(gateway.value, engineUp.value))
const exit = computed(() => exitNotice(gateway.value, engineUp.value))

const busy = ref(false)
const cmdMsg = ref<string | null>(null)
async function send(cmd: 'start' | 'stop' | 'status'): Promise<void> {
  busy.value = true
  cmdMsg.value = null
  try {
    const res: CommandDoc = await api.command(cmd)
    cmdMsg.value = res.message ?? `${cmd} 已下发`
    await store.refresh()
  } catch (e) {
    cmdMsg.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    setTimeout(() => { cmdMsg.value = null }, 6000)
  }
}

// ── 网络诊断 ────────────────────────────────────────────────────────────────
const netDoc = ref<NetCheckDoc | null>(null)
/** 读法（标签、计数、空/探测中/失败三态）全部来自 lib/net-check.ts。 */
const netView = computed(() => readNetCheck(netDoc.value))
const netErr = ref<string | null>(null)
const netBusy = ref(false)
/** 探测在网关侧后台跑；这里只轮询缓存，直到 `probing` 落下。 */
let netTimer: ReturnType<typeof setTimeout> | null = null

function scheduleNetPoll(): void {
  if (netTimer !== null) {
    clearTimeout(netTimer)
    netTimer = null
  }
  if (netDoc.value?.probing) {
    netTimer = setTimeout(() => void loadNet(), 1200)
  }
}

async function loadNet(): Promise<void> {
  try {
    netDoc.value = await api.netCheck()
    // 「面板调不到网关」与「内核没应答探测」是两件事，分开报：前者是这一行，
    // 后者是 netView.hint（网关把内核的错误原样带在 error 里）。
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  }
  scheduleNetPoll()
}

async function probeNet(): Promise<void> {
  netBusy.value = true
  try {
    netDoc.value = await api.probeNetCheck()
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    netBusy.value = false
    scheduleNetPoll()
  }
}

onMounted(() => void loadNet())
onUnmounted(() => {
  if (netTimer !== null) clearTimeout(netTimer)
})

const netBadge = computed(() => {
  switch (netView.value.tone) {
    case 'up': return { text: '全部通过', variant: 'up' as const }
    case 'down': return { text: '有失败', variant: 'down' as const }
    default: return { text: '探测中', variant: 'default' as const }
  }
})

// ── 版本与更新 ──────────────────────────────────────────────────────────────
// 数据来自 snapshot 轮询里的 systemVersion（内核自述，非本地猜测）。
// 三态读法只有一份（lib/version.ts）：null = 未检查，绝不画成「已是最新」。
const version = computed(() => store.snapshot?.systemVersion ?? null)
const versionBadgeView = computed(() => versionBadge(version.value))
const versionErr = ref<string | null>(null)
const checkBusy = ref(false)

/**
 * 「检查更新」：网关只是转交（内核拨 GitHub 以秒计，网关绝不等它），所以这里
 * 轮询 snapshot 直到内核的 lastCheckMs 动了；内核明确拒绝（checkEnabled=false）
 * 时 error 原样展示 —— 关闭就是关闭，不静默、不偷跑。
 */
let checkTimer: ReturnType<typeof setTimeout> | null = null
const checkDeadlineAt = ref(0)

async function checkNow(): Promise<void> {
  if (!version.value || checkBusy.value) return
  const before = version.value.lastCheckMs
  try {
    const res = await api.updateCheck()
    if (res.error) {
      versionErr.value = res.error
      return
    }
  } catch (e) {
    versionErr.value = e instanceof Error ? e.message : String(e)
    return
  }
  checkBusy.value = true
  checkDeadlineAt.value = Date.now() + 45_000
  const poll = async (): Promise<void> => {
    await store.refresh()
    const now = store.snapshot?.systemVersion
    const moved = now && now.lastCheckMs !== before && now.lastCheckMs !== null
    if (moved || Date.now() > checkDeadlineAt.value) {
      checkBusy.value = false
      return
    }
    checkTimer = setTimeout(() => void poll(), 1500)
  }
  void poll()
}

/** 「自动更新」开关：状态来源是内核；写盘失败必须报错（不静默回退）。 */
async function toggleAutoUpdate(on: boolean): Promise<void> {
  try {
    const res = await api.updateConfigure(on)
    if (res.ok === false && res.error) {
      versionErr.value = res.error
    }
  } catch (e) {
    versionErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    await store.refresh()
  }
}

onUnmounted(() => {
  if (checkTimer !== null) clearTimeout(checkTimer)
})

// ── 生效风控（E26 §4.4）─────────────────────────────────────────────────────
// 数据来自内核的 risk.limits（boot 快照，只读）：每个限额都是「值 + 来源」，
// 来源不明的数字不是操作者能据以行动的答案。九个新限额改配置需重启内核，
// 所以读数不会中途变陈旧；这里只在进页时读一次，另给一个手动刷新。
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

// ── 生效风控 · 执行策略（Issue 364）───────────────────────────────────────────
// 可编辑的账户执行策略。纪律与后端同一条：浏览器不读 TOML —— 生效视图是
// 内核 `execution_policy.get` 自己折叠出来的，写路径是 `set`/`reset`（内核
// 落盘→重读→落审计后才应答），预览是内核自己的 evaluate 重放最近已平仓交易。
// 保存成功后重新 get：页面永远渲染「内核确认过的」状态，不渲染本地猜测。
const policyList = ref<ExecutionPolicyListDoc | null>(null)
const policySection = ref<ExecutionPolicySectionDoc | null>(null)
const policyPreview = ref<ExecutionPolicyPreviewDoc | null>(null)
const policyHistory = ref<ExecutionPolicyHistoryDoc>([])
const policyAccountId = ref<string>('defaults')
const policyErr = ref<string | null>(null)
const policyMsg = ref<string | null>(null)
const policyBusy = ref(false)
const policySaving = ref(false)
/** 新建/编辑中的规则（null = 构建器收起）。 */
const ruleDraft = ref<PolicyRuleView | null>(null)
const ruleDraftIdx = ref<number | null>(null)
/** 构建器的 when.value 以文本编辑：数字、符号，或 `in` 的逗号分隔列表。 */
const ruleDraftValueText = ref('')
const ruleDraftThenArg = ref('')
/** 拖拽中的规则下标（HTML5 原生拖放排序优先级）。 */
const dragIdx = ref<number | null>(null)
const dragOverIdx = ref<number | null>(null)

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
  ruleDraft.value = null
  await loadPolicy()
}

/** 「恢复 defaults 后自定义」：把当前编辑缓冲清回全局段的值（仍需保存落盘）。 */
function resetToDefaultsLocal(): void {
  loadEditorFromSection(policyList.value?.defaults ?? null)
  editDirty.value = true
  policyMsg.value = '已按 defaults 重置编辑区 —— 保存后才写入内核。'
}

function markDirty(): void {
  editDirty.value = true
}

// ── 规则构建器 ──
const CONDITION_FIELDS = [
  { value: 'available_balance', label: '可用余额', unit: 'USD', numeric: true },
  { value: 'total_equity', label: '总权益', unit: 'USD', numeric: true },
  { value: 'open_positions', label: '持仓数', unit: '笔', numeric: true },
  { value: 'current_price', label: '当前价', unit: '', numeric: true },
  { value: 'time_left_sec', label: '剩余秒数', unit: 's', numeric: true },
  { value: 'symbol', label: '交易标的', unit: '', numeric: false },
  { value: 'recent_pnl_1h', label: '近1小时盈亏', unit: 'USD', numeric: true },
  { value: 'consecutive_losses', label: '连亏次数', unit: '次', numeric: true },
] as const
const CONDITION_OPS = ['<', '<=', '>', '>=', '==', '!=', 'in'] as const
const THEN_ACTIONS = [
  { value: 'skip', label: '跳过本单（不下）' },
  { value: 'budget_ratio', label: '覆盖下注比例' },
  { value: 'min_budget_usd', label: '覆盖最小下注' },
  { value: 'max_budget_usd', label: '覆盖最大下注' },
  { value: 'cooldown_sec', label: '冷静期（秒）' },
] as const

function draftValueFor(v: PolicyWhenView['value']): string {
  if (Array.isArray(v)) return v.join(',')
  return v == null ? '' : String(v)
}

function startAddRule(): void {
  ruleDraftIdx.value = null
  ruleDraft.value = {
    name: '',
    priority: (editRules.value.reduce((m, r) => Math.max(m, r.priority ?? 0), 0) || 90) + 10,
    enabled: true,
    when: { field: 'open_positions', op: '>=', value: 2 },
    then: { action: 'skip' },
    reason: '',
  }
  ruleDraftValueText.value = '2'
  ruleDraftThenArg.value = ''
}

function startEditRule(i: number): void {
  const r = structuredClone(editRules.value[i])
  ruleDraftIdx.value = i
  ruleDraft.value = r
  ruleDraftValueText.value = draftValueFor(r.when.value)
  const t = r.then
  if (t.action === 'skip') ruleDraftThenArg.value = ''
  else if (t.cooldown_sec != null) ruleDraftThenArg.value = String(t.cooldown_sec)
  else ruleDraftThenArg.value = t.budget_ratio ?? t.min_budget_usd ?? t.max_budget_usd ?? ''
}

function draftFieldMeta() {
  return CONDITION_FIELDS.find((f) => f.value === ruleDraft.value?.when.field)
    ?? CONDITION_FIELDS[2]
}

/** 构建器 → 规则对象：值类型跟字段走（数字字段转 number，symbol/in 为文本）。 */
function commitDraft(): void {
  const d = ruleDraft.value
  if (!d) return
  const field = draftFieldMeta()
  let value: PolicyWhenView['value']
  if (d.when.op === 'in') {
    value = ruleDraftValueText.value.split(',').map((s) => s.trim()).filter(Boolean)
    if (!value.length) { policyMsg.value = 'in 条件至少要一个标的'; return }
  } else if (field.numeric) {
    const n = Number(ruleDraftValueText.value)
    if (!Number.isFinite(n)) { policyMsg.value = '条件值必须是数字'; return }
    value = n
  } else {
    value = ruleDraftValueText.value.trim()
    if (!value) { policyMsg.value = '条件值不能为空'; return }
  }
  d.when.value = value
  const action = d.then.action ?? 'skip'
  const then: PolicyThenView = { action }
  if (action !== 'skip') {
    const arg = ruleDraftThenArg.value.trim()
    if (action === 'cooldown_sec') {
      const secs = Number(arg)
      if (!Number.isInteger(secs) || secs <= 0) { policyMsg.value = '冷静期必须是正整数秒'; return }
      then.cooldown_sec = secs
    } else {
      if (!arg) { policyMsg.value = '该动作需要一个数值'; return }
      then[action] = arg
    }
  }
  d.then = then
  d.name = d.name.trim()
  if (!d.name) { policyMsg.value = '规则需要一个名字'; return }
  if (ruleDraftIdx.value == null) editRules.value.push(d)
  else editRules.value[ruleDraftIdx.value] = d
  ruleDraft.value = null
  ruleDraftIdx.value = null
  editDirty.value = true
  policyMsg.value = null
}

function removeRule(i: number): void {
  editRules.value.splice(i, 1)
  editDirty.value = true
}

/** 优先级重排（HTML5 拖放）：拖到目标位置后，按新顺序把 priority 重写成
 *  10, 20, 30…（升序、互不相等 — 内核按 priority 升序取第一个命中的规则）。 */
function onDrop(target: number): void {
  const from = dragIdx.value
  dragIdx.value = null
  dragOverIdx.value = null
  if (from == null || from === target) return
  const [moved] = editRules.value.splice(from, 1)
  editRules.value.splice(target, 0, moved)
  editRules.value.forEach((r, i) => { r.priority = (i + 1) * 10 })
  editDirty.value = true
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

function thenText(t: PolicyThenView): string {
  if (t.action === 'skip') return '跳过本单'
  if (t.action === 'cooldown_sec') return `冷静 ${t.cooldown_sec}s`
  if (t.action === 'budget_ratio') return `下注比例 ${t.budget_ratio}`
  if (t.action === 'min_budget_usd') return `最小下注 $${t.min_budget_usd}`
  if (t.action === 'max_budget_usd') return `最大下注 $${t.max_budget_usd}`
  return JSON.stringify(t)
}
function verdictBadge(v: string): { text: string; variant: 'up' | 'down' | 'gold' | 'default' } {
  switch (v) {
    case 'place': return { text: '放行', variant: 'up' }
    case 'skip': return { text: '跳过', variant: 'down' }
    case 'cooldown': return { text: '冷静中', variant: 'gold' }
    default: return { text: v, variant: 'default' }
  }
}

// ── theme segmented ─────────────────────────────────────────────────────────
const themeSegments = [
  { id: 'system', label: '跟随系统' },
  { id: 'light', label: '浅色' },
  { id: 'dark', label: '深色' },
]
const themeValue = computed<ThemeMode>({
  get: () => theme.value,
  set: (v) => setTheme(v as ThemeMode),
})
</script>

<template>
  <div class="rise-in">
    <!-- ── 会话与访问 token ─────────────────────────────────────────────── -->
    <Card>
      <CardHeader label="会话与访问 token">
        <template #title>
          <KeyRound class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="sessionBadge.variant" dot>
            <component :is="sessionBadge.icon" class="size-3" /> {{ sessionBadge.text }}
          </Badge>
        </template>
      </CardHeader>

      <div class="grid gap-3 sm:grid-cols-2">
        <div class="rounded-lg border border-line bg-panel-2 p-3">
          <div class="label-micro">当前 token</div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="truncate font-mono text-[13px]">{{ tokenMasked }}</span>
            <Tooltip content="复制完整 token。它等于会话本身 — 交给谁，谁就能打开这个面板。">
              <Button variant="ghost" size="sm" :title="tokenCopied ? '已复制' : '复制 token'" @click="copyToken">
                <Check v-if="tokenCopied" class="size-3.5 text-up" />
                <Copy v-else class="size-3.5" />
              </Button>
            </Tooltip>
          </div>
          <p class="mt-1.5 text-[11px] leading-snug text-faint-fg">
            登录时间：{{ loginTime }}
          </p>
        </div>

        <div class="rounded-lg border border-line bg-panel-2 p-3">
          <div class="label-micro">会话交接链接（?token=）</div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="truncate text-[12px] text-muted-fg num">{{ panelLink }}</span>
            <Tooltip content="复制带 token 的面板地址：新浏览器打开即登录，无需输入密码。链接只在当前会话有效。">
              <Button variant="ghost" size="sm" :title="linkCopied ? '已复制' : '复制链接'" @click="copyLink">
                <Check v-if="linkCopied" class="size-3.5 text-up" />
                <Copy v-else class="size-3.5" />
              </Button>
            </Tooltip>
          </div>
          <p class="mt-1.5 text-[11px] leading-snug text-faint-fg">
            链接会随会话失效；请勿发到不受信任的聊天/邮件渠道。
          </p>
        </div>
      </div>

      <div class="mt-3 flex items-start justify-between gap-3 rounded-md border border-line bg-panel-2 px-3 py-2">
        <div class="flex items-start gap-2 text-[11.5px] leading-snug text-muted-fg">
          <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
          <span>{{ sessionHint || '正在探测网关会话状态…' }}</span>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <Button variant="outline" size="sm" :disabled="sessionState === 'checking'" @click="checkSession">
            <RefreshCw class="size-3.5" />重新探测
          </Button>
          <Button variant="ghost" size="sm" title="清除本地会话并回到登录页" @click="logout">
            <LogOut class="size-3.5" />退出登录
          </Button>
        </div>
      </div>
    </Card>

    <!-- ── Gateway 指令台 ──────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="Gateway 指令台">
        <template #title>
          <TerminalSquare class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="engineUp ? 'up' : 'down'" dot>{{ engineUp ? '内核在线' : '内核离线' }}</Badge>
        </template>
      </CardHeader>

      <div v-if="gateway" class="grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-2">
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">socket</span>
          <span class="truncate text-right num" :title="gateway.socket">{{ gateway.socket }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">进程控制（--manage）</span>
          <Badge :variant="gateway.lifecycleEnabled ? 'up' : 'default'">
            {{ gateway.lifecycleEnabled ? '已开启' : '未开启（只读）' }}
          </Badge>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核归属</span>
          <span>{{ gateway.managed ? '本网关启动（可停止）' : '外部进程（只接管读取）' }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核 PID</span>
          <span class="num">
            <template v-if="gateway.corePid != null"><RollingNumber :value="gateway.corePid" /></template>
            <template v-else>—</template>
          </span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">崩溃自动重启次数</span>
          <span class="num"><RollingNumber :value="gateway.restarts ?? 0" /></span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">已放弃重启</span>
          <Badge :variant="gateway.restartGivenUp ? 'down' : 'default'">{{ gateway.restartGivenUp ? '是' : '否' }}</Badge>
        </div>
      </div>
      <p v-else class="flex items-center gap-2 text-[12px] text-faint-fg">
        <Server class="size-3.5" />当前网关未上报进程控制信息（旧版网关或只读适配器），指令台不可用。
      </p>

      <!--
        A crash is an alert, so it wears the kit's alert tint rather than a
        page-local copy of it (issue 260): the hand-rolled `border-down/35 bg-down/8`
        was the same semantics at a second concentration, which is how the panel
        ended up with two reds.
      -->
      <AlertBanner
        v-if="exit"
        class="mt-3"
        :tone="exit.kind === 'crash' ? 'error' : 'info'"
      >{{ exit.kind === 'crash' ? `内核曾崩溃：${exit.description}` : `内核已停止：${exit.description}` }}</AlertBanner>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button variant="default" size="sm" :disabled="busy" title="查询网关与内核状态" @click="send('status')">
          <RefreshCw class="size-3.5" />状态查询
        </Button>
        <Button
          variant="up"
          size="sm"
          :disabled="busy || !control.canStart"
          :title="control.canStart ? '启动内核' : (control.blockedReason ?? '内核已在运行')"
          @click="send('start')"
        >
          <Play class="size-3.5" />启动内核
        </Button>
        <Button
          variant="danger"
          size="sm"
          :disabled="busy || !control.canStop"
          :title="control.canStop ? '停止本网关启动的内核' : (control.blockedReason ?? '内核未运行')"
          @click="send('stop')"
        >
          <Square class="size-3.5" />停止内核
        </Button>
        <span class="ml-auto text-[11px] text-faint-fg">指令经 /api/command 下发，token 鉴权；本面板不下发任何交易参数</span>
      </div>

      <div v-if="!control.usable && control.blockedReason" class="mt-3 flex items-start gap-2 rounded-md border border-line bg-panel-2 px-3 py-2 text-[11.5px] leading-snug text-muted-fg">
        <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
        <span>{{ control.blockedReason }}</span>
      </div>
      <div v-if="cmdMsg" class="mt-2.5 rounded-md border border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
        {{ cmdMsg }}
      </div>
    </Card>

    <!-- ── 网络诊断 ────────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="网络诊断">
        <template #title>
          <Network class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="netBadge.variant" dot>{{ netBadge.text }}</Badge>
        </template>
      </CardHeader>

      <p class="text-[11.5px] leading-snug text-muted-fg">
        内核拨测它自己出网用到的每条路径（解析 → TCP → TLS → 一次廉价请求）。
        读数是内核进程本身看到的网络，不是浏览器这一侧 —— 这正是「是网络还是交易所」的分界。
      </p>

      <div v-if="netView.rows.length" class="mt-3">
        <div
          v-for="(row, i) in netView.rows"
          :key="row.name"
          class="flex flex-wrap items-center gap-x-2 gap-y-1 py-2"
          :class="i > 0 ? 'border-t border-line' : ''"
        >
          <Badge :variant="row.ok ? 'up' : 'down'" dot>{{ row.label }}</Badge>
          <span class="text-[12.5px] font-semibold">{{ row.name }}</span>
          <span class="min-w-0 truncate text-[11.5px] text-faint-fg" :title="row.target">{{ row.target }}</span>
          <span class="ml-auto flex items-center gap-2 text-[11.5px] text-muted-fg">
            <!--
              The TUI paints this fact in its WARN colour, so it may not sit in
              the panel's faintest tier here: "the resolver answered with a
              fake IP" changes how a failure below should be read, and the two
              faces must not disagree about how loud it is (issue 260). `text-primary`
              is the panel's existing warn token — the same one AlertBanner's
              `warn` tone uses.
            -->
            <Tooltip v-if="row.fakeIp" content="解析到的地址全在代理的 fake-IP 段内：这是解析器的事实，不是这张路径的故障。">
              <span class="text-primary">fake-IP</span>
            </Tooltip>
            <span v-if="row.ms != null" class="num">{{ row.ms }} ms</span>
          </span>
          <p v-if="row.detail" class="w-full text-[11px] leading-snug text-faint-fg">{{ row.detail }}</p>
        </div>
      </div>

      <EmptyState
        v-else
        compact
        :loading="netView.tone === 'default'"
        :text="netView.summary"
        :hint="netView.emptyHint ?? undefined"
      />

      <!--
        The verdict strip is an alert, so it is the kit's `AlertBanner` and not a
        page-local tint (issue 260). `info` covers both "all paths OK, here is the
        reading" and "no verdict yet" — the same tone the Plugins page uses for a
        normal state; only a real failure gets the error tint.
      -->
      <AlertBanner
        class="mt-3"
        :tone="netView.tone === 'down' ? 'error' : 'info'"
        :title="netView.title"
        :hint="netView.hint"
      >{{ netView.summary }}</AlertBanner>

      <!-- Same fact, same volume as the TUI's WARN-coloured proxy line (issue 260):
           a proxy in front of the probe is the explanation for half the
           readings below, so it may not be the faintest text on the card. -->
      <p v-if="netView.proxyNote" class="mt-2 text-[11px] leading-snug text-primary">{{ netView.proxyNote }}</p>
      <p v-if="netErr" class="mt-2 text-[11px] leading-snug text-down">面板调用失败：{{ netErr }}</p>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          :disabled="netBusy || netView.tone === 'default'"
          title="让内核重新拨测一遍（结果在网关侧缓存 20 秒）"
          @click="probeNet"
        >
          <RefreshCw class="size-3.5" />重新探测
        </Button>
        <span class="text-[11px] text-faint-fg">
          <template v-if="netView.ageNote">结果时间：{{ netView.ageNote }}；探测在网关后台运行，页面自动刷新</template>
          <template v-else>还没有探测结果</template>
        </span>
      </div>
    </Card>

    <!-- ── 版本与更新 ──────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="版本与更新">
        <template #title>
          <Tag class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge v-if="versionBadgeView" :variant="versionBadgeView.variant" dot>{{ versionBadgeView.text }}</Badge>
          <Badge v-else variant="default" dot>不可用</Badge>
        </template>
      </CardHeader>

      <p class="text-[11.5px] leading-snug text-muted-fg">
        版本是<strong>正在运行</strong>的内核自述的（system.version），与磁盘上安装的是哪一版不是一回事 ——
        分别可问：<code>blitzkrieg version</code>（磁盘）/ <code>blitzkrieg version --core</code>（运行中）。
      </p>

      <!--
        旧内核不认识 system.version：说「不认识」，不画一个空版本号（§5.6）。
        这条与 net.check 的「空报告不算通过」是同一条诚实规则。
      -->
      <AlertBanner v-if="!version && !versionErr" class="mt-3" tone="info">正在读取内核版本…</AlertBanner>
      <AlertBanner v-else-if="!version" class="mt-3" tone="warn">
        这个内核不认识 system.version（{{ versionErr ?? '内核离线或版本过旧' }}）。版本信息不可用。
      </AlertBanner>

      <template v-else>
        <div class="mt-3 grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-2">
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">版本</span>
            <span class="font-mono">{{ version.version }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">修订号</span>
            <span class="font-mono">
              {{ revisionText(version.gitHash) }}
              <span v-if="version.gitDirty" class="text-primary">dirty</span>
              <span v-else class="text-faint-fg">clean</span>
            </span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">构建时间</span>
            <span class="font-mono">{{ version.buildDate }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">目标平台</span>
            <span class="font-mono">{{ version.target }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">上次检查</span>
            <span class="num">{{ version.lastCheckMs ? dateTime(version.lastCheckMs) : '—' }}</span>
          </div>
          <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
            <span class="text-faint-fg">出网检查</span>
            <Badge :variant="version.checkEnabled ? 'up' : 'default'">
              {{ version.checkEnabled ? '已允许' : '已关闭' }}
            </Badge>
          </div>
        </div>

        <div class="mt-3 flex flex-wrap items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            :disabled="checkBusy || !version.checkEnabled"
            :title="version.checkEnabled
              ? '让内核询问一次发布源（后台执行，结果自动刷新）'
              : '出网检查已在配置中关闭；开启后此按钮才可用（user_layer/configs/update.toml 或下方开关说明）'"
            @click="checkNow"
          >
            <RefreshCw class="size-3.5" />{{ checkBusy ? '检查中…' : '检查更新' }}
          </Button>
          <Tooltip v-if="!version.checkEnabled" content="INV-3：检查关闭时内核一个包都不发 —— 这不是故障，是默认承诺。">
            <span class="text-[11px] text-faint-fg">检查已关闭</span>
          </Tooltip>

          <div class="ml-auto flex items-center gap-2">
            <span class="text-[11px] text-faint-fg">自动更新（默认关闭；开启后安装由启动器校验执行，需重启内核生效）</span>
            <Switch :model-value="version.autoUpdate" @update:model-value="toggleAutoUpdate" />
          </div>
        </div>
        <p v-if="versionErr" class="mt-2 text-[11px] leading-snug text-down">{{ versionErr }}</p>
      </template>
    </Card>

    <!-- ── 生效风控（E26 §4.4）─────────────────────────────────────────── -->
    <Card class="mt-3.5">
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

        <div class="mt-3 flex flex-wrap items-center gap-2">
          <Button variant="outline" size="sm" :disabled="riskBusy" title="重新读取内核的风控读数" @click="loadRisk">
            <RefreshCw class="size-3.5" />刷新
          </Button>
          <span class="text-[11px] text-faint-fg">{{ riskHint }}</span>
        </div>
        <p v-if="riskErr" class="mt-2 text-[11px] leading-snug text-down">{{ riskErr }}</p>
      </template>
    </Card>

    <!-- ── 生效风控 · 执行策略（Issue 364）───────────────────────────────── -->
    <Card class="mt-3.5">
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
          <Input v-model="editBudgetRatio" class="mt-1" type="text" placeholder="如 0.02" @input="markDirty" />
        </label>
        <label class="block">
          <span class="label-micro">最小下注（USD）</span>
          <Input v-model="editMinBudgetUsd" class="mt-1" type="text" placeholder="如 10" @input="markDirty" />
        </label>
        <label class="block">
          <span class="label-micro">最大下注（USD）</span>
          <Input v-model="editMaxBudgetUsd" class="mt-1" type="text" placeholder="如 100" @input="markDirty" />
        </label>
        <label class="block">
          <span class="label-micro">最低权益要求（USD）</span>
          <Input v-model="editMinEquityUsd" class="mt-1" type="text" placeholder="如 50" @input="markDirty" />
        </label>
        <label class="block">
          <span class="label-micro">单标的最多持仓笔数</span>
          <Input
            v-model="editMaxPositionsPerAsset"
            class="mt-1"
            type="number"
            min="1"
            step="1"
            @input="markDirty"
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

      <!-- 规则列表（拖拽排序 = priority 重排）-->
      <div v-if="policySection" class="mt-4">
        <div class="flex items-center justify-between">
          <span class="label-micro">规则（拖拽卡片调整优先级）</span>
          <Button variant="outline" size="sm" :disabled="ruleDraft != null" @click="startAddRule">
            + 新规则
          </Button>
        </div>
        <div class="mt-2 space-y-2">
          <div
            v-for="(rule, i) in editRules"
            :key="`${rule.name}-${i}`"
            draggable="true"
            class="rounded-lg border bg-panel-2 px-3 py-2 text-[12px] transition-opacity"
            :class="[
              rule.enabled ? 'border-line' : 'border-line opacity-55',
              dragOverIdx === i && dragIdx !== i ? 'border-primary/50' : '',
            ]"
            @dragstart="dragIdx = i"
            @dragenter.prevent="dragOverIdx = i"
            @dragover.prevent
            @drop.prevent="onDrop(i)"
            @dragend="dragIdx = null; dragOverIdx = null"
          >
            <div class="flex items-center justify-between gap-2">
              <span class="font-semibold">
                <span class="text-faint-fg">#{{ rule.priority }}</span>
                {{ rule.name }}
                <Badge :variant="rule.enabled ? 'up' : 'default'" class="ml-1">
                  {{ rule.enabled ? '启用' : '停用' }}
                </Badge>
              </span>
              <span class="flex items-center gap-1">
                <Button variant="ghost" size="sm" @click="startEditRule(i)">编辑</Button>
                <Button variant="ghost" size="sm" @click="removeRule(i)">删除</Button>
              </span>
            </div>
            <div class="mt-1 text-muted-fg">
              WHEN {{ rule.when.field }} {{ rule.when.op }}
              {{ Array.isArray(rule.when.value) ? rule.when.value.join(', ') : rule.when.value }}
              → THEN {{ thenText(rule.then) }}
            </div>
          </div>
          <div v-if="!editRules.length" class="rounded-lg border border-dashed border-line px-3 py-3 text-[11.5px] text-faint-fg">
            无规则 —— 只走基础参数（clamp(max(balance × ratio, min), ≤ max)）。
          </div>
        </div>

      <!-- 条件构建器 -->
        <div v-if="ruleDraft" class="mt-3 rounded-lg border border-primary/30 bg-primary/5 px-3 py-3">
          <div class="text-[12px] font-semibold">
            {{ ruleDraftIdx == null ? '新规则' : `编辑规则：${ruleDraft.name}` }}
          </div>
          <div class="mt-2 grid gap-2 sm:grid-cols-2">
            <label class="block">
              <span class="label-micro">规则名</span>
              <Input v-model="ruleDraft.name" class="mt-1" type="text" placeholder="如 大额冷静" />
            </label>
            <label class="block">
              <span class="label-micro">WHEN 字段</span>
              <select v-model="ruleDraft.when.field" class="policy-select mt-1">
                <option v-for="f in CONDITION_FIELDS" :key="f.value" :value="f.value">{{ f.label }}</option>
              </select>
            </label>
            <label class="block">
              <span class="label-micro">比较符</span>
              <select v-model="ruleDraft.when.op" class="policy-select mt-1">
                <option v-for="op in CONDITION_OPS" :key="op" :value="op">{{ op }}</option>
              </select>
            </label>
            <label class="block">
              <span class="label-micro">
                条件值{{ draftFieldMeta().unit ? `（${draftFieldMeta().unit}）` : '' }}
                <span v-if="ruleDraft.when.op === 'in'" class="text-faint-fg">（逗号分隔多个标的）</span>
              </span>
              <Input v-model="ruleDraftValueText" class="mt-1" type="text" placeholder="如 2 或 BTC,ETH" />
            </label>
            <label class="block">
              <span class="label-micro">THEN 动作</span>
              <select v-model="ruleDraft.then.action" class="policy-select mt-1">
                <option v-for="a in THEN_ACTIONS" :key="a.value" :value="a.value">{{ a.label }}</option>
              </select>
            </label>
            <label v-if="ruleDraft.then.action && ruleDraft.then.action !== 'skip'" class="block">
              <span class="label-micro">动作参数（{{ ruleDraft.then.action === 'cooldown_sec' ? '秒' : 'USD / 比例' }}）</span>
              <Input v-model="ruleDraftThenArg" class="mt-1" type="text" />
            </label>
            <label class="block sm:col-span-2">
              <span class="label-micro">备注（可选，写进审计）</span>
              <Input :model-value="ruleDraft.reason ?? ''" class="mt-1" type="text" placeholder="为什么要有这条规则" @update:model-value="ruleDraft.reason = $event" />
            </label>
          </div>
          <div class="mt-3 flex items-center gap-2">
            <Button size="sm" @click="commitDraft">确定</Button>
            <Button variant="outline" size="sm" @click="ruleDraft = null; ruleDraftIdx = null">取消</Button>
          </div>
        </div>

        <!-- 预览：内核用自己的 evaluate 重放最近已平仓交易 -->
        <div v-if="policyPreview" class="mt-4 rounded-lg border border-line bg-panel-2 px-3 py-2 text-[12px]">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
            <span class="label-micro">预览（最近已平仓交易重放）</span>
            <span class="num">最近 {{ policyPreview.considered }} 单</span>
            <span class="text-muted-fg">跳过 {{ policyPreview.skipped }}</span>
            <span class="text-muted-fg">平均下注 ${{ policyPreview.avgBudgetUsd ?? '—' }}</span>
          </div>
          <div v-if="policyPreview.rows.length" class="mt-1.5 space-y-0.5">
            <div
              v-for="(row, i) in policyPreview.rows"
              :key="`${row.tsMs}-${i}`"
              class="flex items-center justify-between gap-2 text-[11px] text-muted-fg"
            >
              <span class="num">{{ dateTime(row.tsMs) }} · {{ row.symbol }} · 余额 ${{ row.balance }}</span>
              <Badge :variant="verdictBadge(row.verdict).variant">
                {{ verdictBadge(row.verdict).text }}<template v-if="row.detail"> · {{ row.detail }}</template>
              </Badge>
            </div>
          </div>
        </div>

        <!-- 版本与保存 -->
        <div class="mt-4 flex flex-wrap items-center gap-2">
          <Button size="sm" :disabled="policySaving || policyBusy" @click="savePolicy">
            {{ policySaving ? '保存中…' : '保存（写入内核）' }}
          </Button>
          <Button
            variant="outline"
            size="sm"
            :disabled="policySaving || policyBusy || policyVersion === 0"
            :title="policyVersion === 0 ? '还没有可回滚的历史' : `回滚到 v${policyVersion - 1}（取最近一次 set 的 before 状态写回）`"
            @click="rollbackPolicy"
          >回滚到 v{{ Math.max(policyVersion - 1, 0) }}</Button>
          <span class="text-[11px] text-faint-fg">
            版本号 = 审计记录数（当前 v{{ policyVersion }}）；
            {{ editDirty ? '有未保存的修改' : '与内核一致' }}
          </span>
        </div>
      </div>
    </Card>

    <!-- ── 外观与节奏 ──────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="外观与刷新" />
      <div class="grid gap-4 sm:grid-cols-2">
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">主题</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">跟随系统 / 浅色 / 深色，本地记忆</p>
          </div>
          <SegmentedControl v-model="themeValue" :segments="themeSegments" size="sm" />
        </div>
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">提示音</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">开/平仓、熔断提示音，本地记忆</p>
          </div>
          <Switch :model-value="sound" @update:model-value="setSoundEnabled" />
        </div>
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">行情快速刷新</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">行情面板 2s 轮询；关闭后跟全局 15s，适合只盯盘不交易</p>
          </div>
          <Switch :model-value="settings.fastPoll" @update:model-value="settings.setFastPoll" />
        </div>
      </div>
    </Card>
  </div>
</template>
