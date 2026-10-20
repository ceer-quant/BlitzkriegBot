<script setup lang="ts">
/**
 * 创建套利弹窗 —— 策略页「创建套利」入口的确认面（插件页跨所配对卡迁入的
 * 新落点，2026-10）。
 *
 * 内核执行面实情决定面板形状：cross_venue_arb 的双腿由**同一策略实例、同一
 * 账户**发出（引擎 scope 模型），「选两个账户分别跑两腿」在执行面上不存在。
 * 所以段0 是唯一一个「执行账户」选择器：选中的账户在映射两侧的交易所各发
 * 一腿，两个交易所由映射决定、不由账户决定。四段 + 确认页：
 *
 *   段0 执行账户 —— store.accounts 数据渲染（venue 纪律同样适用）；
 *        account.venue.get 复核选中账户的绑定读数；提交走
 *        setStrategy('cross_venue_arb', true, accountId)（default=全局开关
 *        + assignment 镜像，其余账户=assignment 即启用，#532/#537 语义）。
 *   段1 跨所映射 —— 待确认候选（unified.candidates，确认=unified.confirm /
 *        撤销=unified.revoke）+ 目标映射单选（unified.list 里 status=paired
 *        且无 discrepancy 的事件）。
 *   段2 参数预设 —— 只发改动项提交 strategy.params.set（内核 plan-first：
 *        一个非法值整体拒，错误原样转述、值不动）。
 *   段3 启用 —— 如实展示选中账户的 venue 绑定读数（绑定单一 venue 时第二腿
 *        会被 venue 守卫拒绝 —— 面板必须说出这一条，而不是藏起来）。
 *
 * 确认页（提交前最后一步）汇总三段；预估利润按 store 快照的两腿实时盘口
 * 随 tick 更新（账户↔venue 解耦批次）：|价差| − 双腿费用（内核同一费率
 * 曲线）− 滑点估计（1 tick × 2 腿，保守）。盘口缺失（某腿无报价/未带
 * token）→ 显示「盘口数据不足」，不显示陈旧数字 —— 编一个就是撒谎。
 * 静态公式说明 + 原子批配额警告原样披露。
 *
 * venue 纪律：venue 一律数据渲染（no-venue-branch 门禁）；组件不分支于任何
 * 具体交易所名。
 */
import { computed, ref, watch } from 'vue'
import { Link2, SlidersHorizontal, Wallet, Zap } from 'lucide-vue-next'
import {
  api,
  type UnifiedEventRow,
  type UnifiedCandidateRow,
  type UnifiedVenueRow,
  type PluginCredentialRowDoc,
  type AssetBook,
} from '@/api/client'
import { cents } from '@/lib/format'
import { usePanelStore } from '@/stores/panel'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const props = defineProps<{
  open: boolean
  /** 当前面板的 default 账户 id（store.activeAccount；null = 老面板/单账户）。
   *  段0 选择器的初始值——没有更高的事实源，操作员可改选。 */
  activeAccountId: string | null
  /**
   * default 账户的绑定 venue（''/undefined = 未绑定）。会话事实已由
   * store.activeAccountVenue 派生；弹窗对**选中**的执行账户另走
   * account.venue.get 复核绑定读数 —— 两处不一致时以内核读数为准。
   */
  activeVenue: string
}>()

const emit = defineEmits<{
  'update:open': [v: boolean]
  /** 提交完成（无论成败）—— 父页面据此刷新 store 读数。 */
  committed: []
}>()

/** cross_venue_arb 的预设旋钮：键 = 声明的 tunable 名，value = 默认草稿。 */
const PARAM_DEFAULTS: Record<string, string> = {
  spread_open_pct: '2.00',
  spread_close_pct: '0.50',
  min_pair_shares: '1',
  min_time_left_sec: '30',
}
/** 每个旋钮一行的帮助文案（帮助是面板的义务：不给解释的数字只是装饰）。 */
const PARAM_HINTS: Record<string, string> = {
  spread_open_pct: '两腿合计全成本低于 $1 的百分比缺口开仓——2.00 表示全成本 ≤ 0.98。',
  spread_close_pct: '缺口收窄到该百分比以下即平仓离场。',
  min_pair_shares: '一组双腿的最小份数。',
  min_time_left_sec: '事件剩余时间低于该秒数不再开新仓。',
}

// ── 段0 执行账户 ─────────────────────────────────────────────────────────────
const store = usePanelStore()

/** 选中的执行账户 id；null = 面板没有账户读数（单账户/老面板），内核按其
 *  配置的初始账户执行——此时段0 退化为一句说明，不发明选择器。 */
const execAccountId = ref<string | null>(null)

/** 选择器选项 = store.accounts 原样（id/name/venue 都是会话事实）。 */
const execAccountOptions = computed(() =>
  store.accounts.map((a) => ({ id: a.id, name: a.name, venue: a.venue ?? '' })),
)

/** 选中账户的行（段3 文案与确认页汇总用）。 */
const execAccount = computed(
  () => store.accounts.find((a) => a.id === execAccountId.value) ?? null,
)

// ── 段1 读数：候选 + 已确认映射 ─────────────────────────────────────────────
const candidates = ref<UnifiedCandidateRow[]>([])
const events = ref<UnifiedEventRow[]>([])
const loadError = ref<string | null>(null)
const loading = ref(false)

/** 目标映射单选：status=paired 且 discrepancies 空 —— 有差异标记的映射不许
 *  作为套利目标（差异就是结算风险，弹窗不替操作员咽下去）。 */
const pairable = computed(() =>
  events.value.filter((e) => e.status === 'paired' && !e.discrepancies.length),
)

/** 进行中的确认/撤销（candidate 索引 / event id 键控），防重复点击。 */
const mappingBusy = ref<string | null>(null)
const mappingMsg = ref<{ ok: boolean; text: string } | null>(null)

async function loadMappings(): Promise<void> {
  loading.value = true
  loadError.value = null
  try {
    const [candDoc, evDoc] = await Promise.all([api.unifiedCandidates(), api.unifiedList()])
    candidates.value = candDoc.candidates ?? []
    events.value = evDoc.events ?? []
    if (evDoc.error) loadError.value = evDoc.error
  } catch (e) {
    loadError.value = e instanceof Error ? e.message : String(e)
    candidates.value = []
    events.value = []
  } finally {
    loading.value = false
  }
}

/** 确认一个候选：标题用两侧相同的 question（ledger 仍会重新验证相似度），
 *  listings 原样回传。成功后刷新候选架与映射清单，并自动选中新映射。 */
async function confirmCandidate(c: UnifiedCandidateRow, idx: number): Promise<void> {
  if (mappingBusy.value) return
  mappingBusy.value = `cand-${idx}`
  mappingMsg.value = null
  try {
    const title = c.a.title || c.b.title
    const doc = await api.unifiedConfirm({ title, listings: [c.a, c.b] })
    if (doc.error) {
      mappingMsg.value = { ok: false, text: doc.error }
    } else {
      mappingMsg.value = { ok: true, text: `已确认映射「${doc.event?.title ?? title}」。` }
    }
  } catch (e) {
    mappingMsg.value = { ok: false, text: e instanceof Error ? e.message : String(e) }
  } finally {
    mappingBusy.value = null
    await loadMappings()
  }
}

/** 撤销一个已确认映射：账本落审计、引擎视图同步拔除。若撤销的是当前选中的
 *  目标映射，选择一并清掉。 */
async function revokeEvent(e: UnifiedEventRow): Promise<void> {
  if (mappingBusy.value) return
  mappingBusy.value = `evt-${e.id}`
  mappingMsg.value = null
  try {
    const doc = await api.unifiedRevoke({ eventId: e.id })
    if (doc.error) {
      mappingMsg.value = { ok: false, text: doc.error }
    } else {
      mappingMsg.value = { ok: true, text: `已撤销映射「${e.title || e.id}」。` }
      if (selectedEventId.value === e.id) selectedEventId.value = ''
    }
  } catch (err) {
    mappingMsg.value = { ok: false, text: err instanceof Error ? err.message : String(err) }
  } finally {
    mappingBusy.value = null
    await loadMappings()
  }
}

// ── 段2 参数草稿：只发改动项 ─────────────────────────────────────────────────
const paramsDraft = ref<Record<string, string>>({})
const paramsError = ref<string | null>(null)
/** 打开弹窗时是否已有声明参数读数（读失败时确认页要如实说明预设未验证）。 */
const paramsReadFailed = ref(false)

async function loadCurrentParams(): Promise<void> {
  paramsReadFailed.value = false
  const draft: Record<string, string> = { ...PARAM_DEFAULTS }
  try {
    const doc = await api.strategyParams('cross_venue_arb')
    if (!doc.found) {
      paramsReadFailed.value = true
      return
    }
    for (const t of doc.tunables) {
      // 声明域里的旋钮以当前生效值为草稿起点；域外名不显示（内核是唯一事实源）。
      if (t.name in draft) draft[t.name] = t.value
    }
  } catch {
    paramsReadFailed.value = true
  } finally {
    paramsDraft.value = draft
  }
}

/** patch = 只发改动项（对齐策略页 saveParams 的纪律：与生效值相同的旋钮不进补丁）。 */
const paramsPatch = computed<Record<string, string>>(() => {
  const patch: Record<string, string> = {}
  for (const [k, v] of Object.entries(paramsDraft.value)) {
    const val = (v ?? '').trim()
    if (val && val !== PARAM_DEFAULTS[k] && val !== '') patch[k] = val
  }
  return patch
})

// ── 段3 启用 + 选中执行账户的绑定读数 ────────────────────────────────────────
const enable = ref(true)
/** 选中执行账户的 venue 绑定读数（account.venue.get 的行；读失败 = null）。 */
const venueRow = ref<{ venue: string; actor: string } | null>(null)
const venueReadFailed = ref(false)

async function loadVenueBinding(): Promise<void> {
  venueReadFailed.value = false
  venueRow.value = null
  const id = execAccountId.value
  if (!id) return
  try {
    const doc = await api.accountVenue({ accountId: id })
    const row = (doc.venues ?? []).find((r) => r.accountId === id)
    if (row) venueRow.value = { venue: row.venue, actor: row.actor }
  } catch {
    venueReadFailed.value = true
  }
}

/** 选中账户换人：绑定读数必须跟着重读 —— 上一账户的读数对新账户是陈旧
 *  事实，段3 的警告文案按它渲染。 */
watch(execAccountId, () => {
  void loadVenueBinding()
})

/** 绑定了单一 venue = 双腿会被 venue 守卫拒绝第二腿 —— 这条警告必须出现。
 *  判据取 account.venue.get 的行读数；store 侧的账户行 venue 只在行读数缺失
 *  （读失败/老读数）时兜底 —— 两处都如实渲染，不发明第二张表。 */
const venueGuardWarning = computed(() => {
  const v = venueRow.value?.venue ?? execAccount.value?.venue ?? ''
  return v.trim() !== ''
})

/** 选中账户的 venue 绑定显示值：account.venue.get 行读数优先，缺失时
 *  回落 store 账户行的 venue（都是会话事实，不是第二张表）。 */
const venueDisplay = computed(() => venueRow.value?.venue ?? execAccount.value?.venue ?? '')

// ── 确认页 + 提交 ────────────────────────────────────────────────────────────
const step = ref<'edit' | 'confirm'>('edit')
const busy = ref(false)
const error = ref<string | null>(null)

const selectedEventId = ref('')

/** 目标映射单选值；撤销当前选中项时由 revokeEvent 一并清掉。 */
const selectedEvent = computed(() =>
  pairable.value.find((e) => e.id === selectedEventId.value) ?? null,
)

// ── 映射两侧 venue 的凭证就绪（issue 542，P1-a 缓解面）───────────────────────
// 内核 `market.credentials.status` 已产出 per-plugin ready 读数（executor 缺
// 凭证时 start 仍 Ok 但 inert —— 挂出后无人拉取，面板必须提前说出这条）。
// 数据纪律：ready 是行数据，venue 是行键 —— 不分支于任何具体交易所名。

/** 凭证读数行（挂载即读一次）；读失败 = null，如实降级为「无法验证」。 */
const credRows = ref<PluginCredentialRowDoc[] | null>(null)

async function loadCredentialRows(): Promise<void> {
  credRows.value = null
  try {
    const doc = await api.marketCredentials()
    credRows.value = doc.rows ?? []
  } catch {
    credRows.value = null
  }
}

/** 每条映射 venue 一行：ready / 未解析的变量名 / 读数里查不到该行（未知）。 */
interface LegCredStatus {
  venue: string
  /** rows 里有该 plugin 行且 ready=true。 */
  ready: boolean
  /** requiredVars − resolvedVars（ready=false 时的缺失变量名，如实列出）。 */
  missingVars: string[]
  /** rows 里没有该 plugin 的行 —— 读数覆盖不到，不阻断但必须披露。 */
  unknown: boolean
}

function legCredStatus(v: UnifiedVenueRow): LegCredStatus {
  const row = credRows.value?.find((r) => r.plugin === v.venue)
  if (!row) return { venue: v.venue, ready: false, missingVars: [], unknown: true }
  const resolved = new Set(row.resolvedVars)
  return {
    venue: v.venue,
    ready: row.ready,
    missingVars: row.requiredVars.filter((name) => !resolved.has(name)),
    unknown: false,
  }
}

/** 选中映射两侧的凭证状态行（确认页 per-leg 披露块的数据源）。 */
const selectedLegCreds = computed<LegCredStatus[] | null>(() => {
  const ev = selectedEvent.value
  if (!ev) return null
  return ev.venues.map(legCredStatus)
})

/** 读数可用且有任一腿明确 ready=false ⇒ 阻断进确认页/提交（保守方向的
 *  一侧：明确未就绪不许过）。读数缺失或行覆盖不到 = 未知 —— 不阻断，
 *  由确认页的「无法验证」披露兜底。 */
const credBlocked = computed(() =>
  (selectedLegCreds.value ?? []).some((leg) => leg.unknown === false && !leg.ready),
)

/** 可以进确认页 = 选了目标映射，且映射两侧凭证校验未明确失败。 */
const canReview = computed(() => selectedEvent.value !== null && !credBlocked.value)

/** 凭证读数读取失败（请求层）——确认页如实披露「无法验证两腿凭证就绪」。 */
const credReadFailed = computed(() => credRows.value === null)

/** 明确 ready=false 的腿（编辑页聚合警示 + 阻断判据共用）。 */
const credUnreadyRows = computed(() =>
  (selectedLegCreds.value ?? []).filter((leg) => leg.unknown === false && !leg.ready),
)

/** 存在读数里查不到的腿（未知）——不阻断，确认页披露「无法验证」。 */
const credUnverifiable = computed(() =>
  (selectedLegCreds.value ?? []).some((leg) => leg.unknown),
)

watch(
  () => props.open,
  (v) => {
    if (!v) return
    step.value = 'edit'
    error.value = null
    mappingMsg.value = null
    paramsError.value = null
    selectedEventId.value = ''
    enable.value = true
    execAccountId.value = props.activeAccountId
    void loadMappings()
    void loadCurrentParams()
    void loadVenueBinding()
    void loadCredentialRows()
  },
)

function close(): void {
  if (!busy.value) emit('update:open', false)
}

function back(): void {
  if (!busy.value) {
    error.value = null
    step.value = 'edit'
  }
}

/** 提交序：strategy.params.set（失败即停，值不动）→ setStrategy on。执行
 *  账户的启用语义按账户分叉（default=全局开关+assignment 镜像；其余账户=
 *  assignment 即启用）——选择器给哪个 id 就走哪条，面板不预判结果。 */
async function commit(): Promise<void> {
  if (!canReview.value || busy.value) return
  busy.value = true
  error.value = null
  try {
    if (Object.keys(paramsPatch.value).length) {
      await api.strategyParamsSet({
        name: 'cross_venue_arb',
        params: paramsPatch.value,
        reason: 'panel create-arb',
      })
    }
    const res = await api.setStrategy(
      'cross_venue_arb',
      true,
      execAccountId.value ?? undefined,
    )
    if (res.ok === false || res.reason) {
      // 内核拒绝（模式握手/已启用等）：如实转述 reason，值不动。
      throw new Error(res.reason ?? res.message ?? '启用被内核拒绝')
    }
    emit('committed')
    emit('update:open', false)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
  }
}

/** 确认页的 patch 展示行（k = v）。 */
const patchLines = computed(() =>
  Object.entries(paramsPatch.value).map(([k, v]) => `${k}: ${v}`),
)

// ── 实时预估利润（账户↔venue 解耦批次补课）─────────────────────────────────
// 数据源 = 既有 store 快照（snapshot.books 的 L2 盘口），随轮询 tick 自动
// 更新 —— 不另开数据通道，不发明第二张行情表。配对规则照抄策略
// cross_venue_arb 的 pick_pair 语义：UP 腿取 UP 卖价更便宜的那一侧 listing，
// DOWN 腿取另一侧；每组锁定利润 = $1 − (ask_UP + ask_DOWN) − 双腿费用 − 滑点。
//
// 费用 = 内核唯一的费率曲线（exit_policy::FeeSchedule::fee_per_share）：
// rate·(p·(1−p))^exponent，上线部署线 = legacy_quadratic（0.125, 2）。价格在
// (0,1) 之外内核不收费 —— 面板按 0 计（内核同式同值）。
// 滑点 = 保守 1 tick（0.01）× 2 腿；内核 taker_slippage_ticks 默认 0，这里
// 往坏处估而不是往好处估。
//
// 盘口缺失（任一腿没有 ask、或快照未带 books）→ 显示「盘口数据不足」，
// 绝不显示上一个数字或编造的 0 —— 陈旧数字比没有数字更危险。

/** 两腿要价（每组一份）：无法配出完整双腿 → null。 */
interface ArbLegQuote {
  /** UP 腿所在 venue（数据渲染用，不分支于具体交易所名）。 */
  upVenue: string
  downVenue: string
  askUp: number
  askDown: number
  /** 较小一侧的可见卖深（份数）—— 容量提示，非保证成交。 */
  depth: number
}

/** 一条 UnifiedVenueRow 对应的盘口行：优先 venue 标签配对（新内核行带
 *  venue 字段），缺标签时按 asset 名兜底（feature-detect，永不假设字段在）。 */
function bookForVenueRow(rows: AssetBook[], r: UnifiedVenueRow): AssetBook | null {
  const tagged = rows.filter((b) => typeof b.venue === 'string' && b.venue !== '')
  if (tagged.length) return tagged.find((b) => b.venue === r.venue) ?? null
  const asset = (r.asset ?? '').trim()
  if (!asset) return null
  return rows.find((b) => b.asset === asset) ?? null
}

const arbLegs = computed<ArbLegQuote | null>(() => {
  const ev = selectedEvent.value
  if (!ev || ev.venues.length !== 2) return null
  const rows = store.snapshot?.books ?? []
  if (!rows.length) return null
  // 每个 venue 侧的报价：UP 卖价 = up.bestAsk，DOWN 卖价 = down.bestAsk；
  // 任一侧没有完整双腿 ask → 该 listing 报不出价（策略同款：报不出的
  // listing 不是折扣，是事故）。
  const quotes = ev.venues
    .map((r) => {
      const b = bookForVenueRow(rows, r)
      if (!b) return null
      const askUp = b.up.bestAsk
      const askDown = b.down.bestAsk
      if (askUp == null || askDown == null) return null
      const dUp = b.up.asks.reduce((a, l) => a + (Number(l.size) || 0), 0)
      const dDown = b.down.asks.reduce((a, l) => a + (Number(l.size) || 0), 0)
      return { venue: r.venue, askUp, askDown, depth: Math.min(dUp, dDown) }
    })
    .filter((q): q is NonNullable<typeof q> => q !== null)
  if (quotes.length !== 2) return null
  // pick_pair：UP 腿取 UP 卖价便宜的一侧，DOWN 腿取另一侧；同侧并列时
  // 稳定取前者（与策略的排序语义一致，不额外发明）。
  const [x, y] = quotes
  const upFirst = x.askUp <= y.askUp
  const up = upFirst ? x : y
  const down = upFirst ? y : x
  return {
    upVenue: up.venue,
    downVenue: down.venue,
    askUp: up.askUp,
    askDown: down.askDown,
    depth: Math.min(up.depth, down.depth),
  }
})

/** 内核费率曲线（legacy_quadratic 上线部署线）；(0,1) 之外 = 0，与内核
 *  `p outside (0,1) → fee 0` 同式。 */
const FEE_RATE = 0.125
const FEE_EXPONENT = 2
function feePerShare(p: number): number {
  if (!(p > 0 && p < 1)) return 0
  return FEE_RATE * (p * (1 - p)) ** FEE_EXPONENT
}

/** 保守滑点：1 tick = 0.01 × 2 腿（内核 tick = Decimal::new(1, 2)）。 */
const SLIPPAGE_PER_LEG = 0.01

/** 每组锁定利润（$/份·组）；盘口不足 → null（模板显示「盘口数据不足」）。 */
const estProfitPerShare = computed<number | null>(() => {
  const q = arbLegs.value
  if (!q) return null
  return 1 - q.askUp - q.askDown - feePerShare(q.askUp) - feePerShare(q.askDown)
    - SLIPPAGE_PER_LEG * 2
})

// ── discrepancy 名称 → 操作员可读的短语；未知名原样展示（数据驱动）────────
const DISCREPANCY_LABELS: Record<string, string> = {
  settlement_source: '结算源不一致',
  settlement_rules: '结算规则冲突',
  expiry_gap: '到期时间差过大',
}
function discrepancyLabel(d: string): string {
  return DISCREPANCY_LABELS[d] ?? d
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      @click.self="close"
    >
      <div class="flex max-h-[85vh] w-full max-w-lg flex-col rounded-lg border border-line bg-panel-solid shadow-lg">
        <div class="flex items-center gap-2 border-b border-line px-4 py-3">
          <Link2 class="size-4 text-primary" />
          <h3 class="text-[13.5px] font-semibold">创建套利（cross_venue_arb）</h3>
          <Badge variant="outline" class="ml-auto">{{ step === 'edit' ? '确认映射与参数' : '提交前确认' }}</Badge>
        </div>

        <div class="min-h-0 flex-1 space-y-3.5 overflow-y-auto px-4 py-3">
          <!-- ═══ 编辑页：四段 ═══ -->
          <template v-if="step === 'edit'">
            <!-- 段0 执行账户：双腿由这一账户在映射两侧的交易所各发一腿。
                 选项 = store.accounts 原样；没有读数时只说明、不发明控件。 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Wallet class="size-3 text-faint-fg" />
                <span class="label-micro">执行账户</span>
                <span class="text-[11px] text-faint-fg">—— 双腿由同一账户在两个交易所执行；交易所由下方映射决定。</span>
              </div>
              <div v-if="!execAccountOptions.length" class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg">
                当前面板没有账户读数（单账户/老面板）—— 内核按其配置的初始账户执行双腿。
              </div>
              <div v-else class="mt-1.5 grid gap-1.5 sm:grid-cols-2">
                <label
                  v-for="a in execAccountOptions"
                  :key="a.id"
                  class="flex cursor-pointer items-center gap-2 rounded-md border px-2.5 py-2 text-[12px]"
                  :class="execAccountId === a.id ? 'border-primary/55 bg-primary/8' : 'border-line bg-panel-2/40'"
                >
                  <input
                    v-model="execAccountId"
                    type="radio"
                    name="arb-exec-account"
                    :value="a.id"
                    class="accent-[var(--primary)]"
                  />
                  <span class="min-w-0 flex-1 truncate" :title="a.id">{{ a.name }}</span>
                  <Badge v-if="a.venue" variant="outline">{{ a.venue }}</Badge>
                  <Badge v-else variant="info">任意所</Badge>
                </label>
              </div>
            </div>

            <!-- 段1 跨所映射 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Link2 class="size-3 text-faint-fg" />
                <span class="label-micro">跨所映射</span>
              </div>
              <div v-if="mappingMsg" class="mt-1.5">
                <AlertBanner :tone="mappingMsg.ok ? 'info' : 'error'" :title="mappingMsg.text" />
              </div>

              <!-- 待确认候选：确认/撤销的新落点 -->
              <div v-if="candidates.length" class="mt-1.5 rounded-lg border border-line bg-panel-2/40 p-2.5">
                <div class="mb-1 flex items-center gap-2">
                  <span class="text-[11px] font-semibold text-muted-fg">待确认候选</span>
                  <Badge variant="info" dot>{{ candidates.length }}</Badge>
                  <span class="text-[11px] text-faint-fg">两个场地发现了疑似同一事件 —— 确认后映射才生效</span>
                </div>
                <div
                  v-for="(c, i) in candidates"
                  :key="`cand-${i}`"
                  class="flex flex-wrap items-center gap-2 border-t border-line/60 py-2 first:border-t-0"
                >
                  <span class="min-w-0 flex-1 truncate text-[12px]" :title="c.a.title || c.b.title">
                    {{ c.a.title || c.b.title }}
                  </span>
                  <Badge variant="outline">{{ c.a.venue }}</Badge>
                  <Link2 class="size-3 text-faint-fg" />
                  <Badge variant="outline">{{ c.b.venue }}</Badge>
                  <span class="text-[11px] text-faint-fg">分 {{ c.score.toFixed(2) }}</span>
                  <Button
                    size="sm"
                    variant="outline"
                    :disabled="mappingBusy !== null"
                    @click="confirmCandidate(c, i)"
                  >确认配对</Button>
                </div>
              </div>

              <!-- 目标映射：单选。作为套利前提，必须是 paired 且无差异标记。 -->
              <div class="mt-2">
                <span class="text-[11px] font-semibold text-muted-fg">选择目标映射（已配对、无差异标记）</span>
                <div v-if="loading" class="mt-1.5 text-[12px] text-faint-fg">映射读取中…</div>
                <div v-else-if="loadError" class="mt-1.5">
                  <AlertBanner tone="error" title="统一映射读取失败" :hint="loadError" />
                </div>
                <EmptyState
                  v-else-if="!pairable.length"
                  compact
                  text="暂无可用作目标映射的事件"
                  hint="先确认候选（上方），或到插件页确认两个行情插件已在线并发现同一事件；只有已配对且无差异标记的映射才能交易。"
                />
                <div v-else class="mt-1.5 space-y-1.5">
                  <!-- 凭证就绪（issue 542）：映射两侧 venue 的 credentials 读数，
                       ready=false 明确阻断；行查不到/读数缺失 = 未知，不阻断但披露。 -->
                  <div v-if="credReadFailed" class="rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] text-muted-fg">
                    凭证读数读取失败，无法验证两腿凭证就绪 —— 确认页会如实披露。
                  </div>
                  <div v-else-if="credUnreadyRows.length" class="rounded-md border border-down/30 bg-down/10 px-2.5 py-2 text-[11.5px] text-down">
                    <div class="font-semibold">当前选中映射的以下 venue 凭证未就绪 —— 无法进入确认页：</div>
                    <div v-for="row in credUnreadyRows" :key="row.venue" class="mt-0.5">
                      venue {{ row.venue }} 凭证未就绪（缺变量：{{ row.missingVars.join('、') || '读数未列出' }}）
                    </div>
                  </div>
                  <label
                    v-for="e in pairable"
                    :key="e.id"
                    class="flex cursor-pointer items-center gap-2 rounded-md border px-2.5 py-2 text-[12px]"
                    :class="selectedEventId === e.id ? 'border-primary/55 bg-primary/8' : 'border-line bg-panel-2/40'"
                  >
                    <input
                      v-model="selectedEventId"
                      type="radio"
                      name="arb-target-event"
                      :value="e.id"
                      class="accent-[var(--primary)]"
                    />
                    <span class="min-w-0 flex-1 truncate" :title="e.title">{{ e.title || e.id }}</span>
                    <Badge v-for="v in e.venues" :key="`${e.id}-${v.venue}`" variant="outline">{{ v.venue }}</Badge>
                    <Button
                      size="sm"
                      variant="ghost"
                      :disabled="mappingBusy !== null"
                      :title="`撤销映射 ${e.id}（可重新确认）`"
                      @click.prevent="revokeEvent(e)"
                    >撤销</Button>
                  </label>
                </div>
              </div>
            </div>

            <!-- 段2 参数预设 -->
            <div>
              <div class="flex items-center gap-1.5">
                <SlidersHorizontal class="size-3 text-faint-fg" />
                <span class="label-micro">参数预设</span>
                <span class="text-[11px] text-faint-fg">—— 只提交与默认不同的项；域校验在内核（一个非法值整体拒）。</span>
              </div>
              <div v-if="paramsReadFailed" class="mt-1.5">
                <AlertBanner
                  tone="warn"
                  title="当前参数读取失败（策略未注册或内核离线）—— 以下为面板默认值，提交时以内核校验为准。"
                />
              </div>
              <div class="mt-1.5 grid gap-2">
                <label
                  v-for="(hint, name) in PARAM_HINTS"
                  :key="name"
                  class="grid grid-cols-[minmax(0,10.5rem)_7rem] items-start gap-2 text-[12px]"
                >
                  <span class="break-all pt-1.5 text-muted-fg">
                    <span class="num">{{ name }}</span>
                  </span>
                  <Input
                    v-model="paramsDraft[name]"
                    size="sm"
                    class="text-right"
                    autocomplete="off"
                    spellcheck="false"
                  />
                  <span class="col-span-2 text-[11px] leading-snug text-faint-fg">{{ hint }}</span>
                </label>
              </div>
            </div>

            <!-- 段3 启用 + default 账户绑定读数 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Zap class="size-3 text-faint-fg" />
                <span class="label-micro">启用</span>
              </div>
              <div class="mt-1.5 flex items-center gap-2">
                <Switch v-model="enable" label="启用 cross_venue_arb" />
                <span class="text-[12px] text-muted-fg">双腿由内核在选中账户上原子执行（见「执行账户」）。</span>
              </div>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg">
                <template v-if="!execAccountId">
                  当前面板没有账户读数 —— 段3 的 venue 绑定警告不可得，内核按其配置的初始账户执行双腿。
                </template>
                <template v-else-if="venueReadFailed">
                  执行账户「{{ execAccountId }}」的 venue 绑定读数读取失败 —— 无法如实展示，请稍后重试。
                </template>
                <template v-else-if="venueGuardWarning">
                  执行账户「{{ execAccount?.name ?? execAccountId }}」绑定在
                  <Badge variant="gold">{{ venueDisplay }}</Badge>
                  —— 双腿会被 venue 守卫拒绝第二腿：绑定需留空（任意交易所）或覆盖两侧交易所，否则原子批开不出完整双腿。
                </template>
                <template v-else>
                  执行账户「{{ execAccount?.name ?? execAccountId }}」未绑定单一 venue（venue 绑定：留空 = 可交易任意交易所）—— 双腿可正常发出。
                </template>
              </div>
            </div>
          </template>

          <!-- ═══ 确认页：汇总 + 公式 + 提交 ═══ -->
          <template v-else>
            <div>
              <span class="label-micro">目标映射</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div class="truncate font-semibold" :title="selectedEvent?.title">{{ selectedEvent?.title || selectedEvent?.id }}</div>
                <div class="mt-1 flex flex-wrap items-center gap-1.5">
                  <Badge v-for="v in selectedEvent?.venues ?? []" :key="v.venue" variant="outline">{{ v.venue }}</Badge>
                  <span class="text-[11px] text-faint-fg">映射 id：{{ selectedEvent?.id }}</span>
                </div>
              </div>
            </div>

            <!-- issue 542: per-leg credential disclosure from the kernel's
                 market.credentials.status readout. Every leg is a data row
                 (venue + ready state + missing variable names) — venue is the
                 row key, never a branch. ready=false blocks submission; a leg
                 absent from the readout is UNKNOWN: not a block, but it must
                 be stated as unverifiable rather than silently passed. -->
            <div>
              <span class="label-micro">两腿凭证就绪（提交前校验）</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div v-if="credReadFailed" class="text-muted-fg">
                  凭证读数读取失败，无法验证两腿凭证就绪 —— 提交不因此阻断，但执行器可能因缺凭证 inert（挂单无人拉取）。
                </div>
                <template v-else>
                  <div v-for="leg in selectedLegCreds ?? []" :key="leg.venue" class="flex flex-wrap items-center gap-1.5 border-t border-line/60 py-1.5 first:border-t-0 first:pt-0">
                    <span class="text-muted-fg">腿 venue</span>
                    <Badge variant="outline">{{ leg.venue }}</Badge>
                    <Badge v-if="leg.unknown" variant="gold">读数无该 venue 行 —— 无法验证</Badge>
                    <Badge v-else-if="leg.ready" variant="up">凭证就绪</Badge>
                    <Badge v-else variant="down">凭证未就绪</Badge>
                    <span v-if="!leg.unknown && !leg.ready" class="num text-[11px] text-down">
                      缺变量：{{ leg.missingVars.join('、') || '读数未列出' }}
                    </span>
                  </div>
                  <div v-if="credUnverifiable" class="mt-1.5 text-[11px] text-muted-fg">
                    存在无法验证的腿（读数无该 venue 行）—— 不阻断提交，但该腿执行器可能因缺凭证 inert。
                  </div>
                </template>
              </div>
            </div>

            <div>
              <span class="label-micro">执行账户</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <template v-if="execAccountId">
                  <span class="font-semibold">{{ execAccount?.name ?? execAccountId }}</span>
                  <span class="ml-1 text-[11px] text-faint-fg">{{ execAccountId }}</span>
                  <Badge v-if="venueDisplay" variant="gold" class="ml-1.5">绑定 {{ venueDisplay }}</Badge>
                  <Badge v-else variant="info" class="ml-1.5">任意所</Badge>
                  <div class="mt-1 text-[11px] text-faint-fg">双腿由该账户在映射两侧的交易所各发一腿。</div>
                </template>
                <template v-else>
                  <span class="text-faint-fg">内核配置的初始账户（面板无账户读数）。</span>
                </template>
              </div>
            </div>

            <div>
              <span class="label-micro">参数改动（只发改动项）</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div v-if="patchLines.length" class="space-y-0.5">
                  <div v-for="line in patchLines" :key="line" class="num">{{ line }}</div>
                </div>
                <div v-else class="text-faint-fg">无改动 —— 沿用当前生效参数。</div>
                <div v-if="paramsReadFailed" class="mt-1 text-[11px] text-faint-fg">当前参数读取失败（预设未经生效值校验）。</div>
              </div>
            </div>

            <div>
              <span class="label-micro">启用动作</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                {{ enable ? '提交后启用 cross_venue_arb（strategy cross_venue_arb on）。' : '提交后保持停用 —— 只确认映射与参数（可稍后在策略表手动启用）。' }}
                <div class="mt-1 text-[11px] text-faint-fg">
                  执行账户 = <template v-if="execAccountId">「{{ execAccount?.name ?? execAccountId }}」{{ execAccountId === 'default' ? '（default：全局开关 + 账户镜像）' : '（assignment 即启用）' }}</template><template v-else>内核配置的初始账户</template>；双腿在同一账户原子执行。
                </div>
              </div>
            </div>

            <div>
              <span class="label-micro">预估锁定利润（实时盘口）</span>
              <!--
                Live per-share estimate from the panel store's snapshot books:
                $1 − (ask_UP + ask_DOWN) − both legs' fees (the kernel's one
                curve, legacy_quadratic 0.125·(p(1−p))²) − 2 legs × 1 tick of
                conservative slippage. It re-reads on every store tick — there
                is no cached number to go stale. When either leg has no ask
                (or the books field is absent) the estimate is `null` and the
                block states the gap instead of quoting a stale figure.
              -->
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px] leading-relaxed">
                <template v-if="estProfitPerShare !== null && arbLegs">
                  <div class="flex items-baseline gap-2">
                    <span class="stat-num text-[20px] leading-none" :class="estProfitPerShare >= 0 ? 'text-up' : 'text-down'">
                      <RollingNumber :value="estProfitPerShare >= 0 ? `+${(estProfitPerShare * 100).toFixed(1)}¢` : `−${(Math.abs(estProfitPerShare) * 100).toFixed(1)}¢`" />
                    </span>
                    <span class="text-[11px] text-faint-fg">每组（1 份 UP + 1 份 DOWN）</span>
                  </div>
                  <div class="mt-1 space-y-0.5 text-[11.5px] text-muted-fg">
                    <div>
                      UP 腿 <Badge variant="outline">{{ arbLegs.upVenue }}</Badge>
                      卖价 <span class="num"><RollingNumber :value="cents(arbLegs.askUp)" /></span>
                      · DOWN 腿 <Badge variant="outline">{{ arbLegs.downVenue }}</Badge>
                      卖价 <span class="num"><RollingNumber :value="cents(arbLegs.askDown)" /></span>
                    </div>
                    <div class="text-faint-fg">
                      已含双腿费用（费率曲线 0.125·(p(1−p))²，随内核）与滑点估计（每腿 1 tick）；
                      可见卖深较小侧约 <span class="num"><RollingNumber :value="arbLegs.depth.toFixed(1)" /></span> 份（容量提示，非保证成交）。
                    </div>
                  </div>
                </template>
                <template v-else>
                  <div class="font-semibold text-muted-fg">盘口数据不足</div>
                  <div class="mt-0.5 text-[11.5px] text-faint-fg">
                    两个场地的卖一价尚未同时到达（行情未到 / 某侧无卖价 / 快照未带盘口）——
                    有了实时盘口这里会立即给出数字；不显示陈旧估计。
                  </div>
                </template>
                <div class="mt-1 text-[11px] text-faint-fg">
                  公式：$1 − (腿UP卖价 + 腿DOWN卖价) − 双腿费用 − 滑点；实际入场由内核逐 tick 按 L2 深度判定。
                </div>
                <div class="mt-1 text-[11px] text-down">原子批受 virtual-limit 配额约束，整组放不下时零腿开出。</div>
              </div>
            </div>

            <div v-if="error">
              <AlertBanner tone="error" title="提交被拒" :hint="error">
                {{ error }}
              </AlertBanner>
            </div>
          </template>
        </div>

        <div class="flex items-center gap-2 border-t border-line px-4 py-3">
          <Button variant="ghost" size="sm" :disabled="busy" @click="step === 'edit' ? close() : back()">
            {{ step === 'edit' ? '取消' : '返回' }}
          </Button>
          <div class="ml-auto flex items-center gap-2">
            <template v-if="step === 'edit'">
              <span v-if="!canReview && credBlocked" class="text-[11px] text-down">映射 venue 凭证未就绪 —— 提交前需补齐</span>
              <span v-else-if="!canReview" class="text-[11px] text-faint-fg">先选择一个目标映射</span>
              <Button variant="gold" size="sm" :disabled="!canReview" @click="step = 'confirm'">
                下一步：确认
              </Button>
            </template>
            <template v-else>
              <Button variant="gold" size="sm" :disabled="busy || !canReview" @click="commit">
                {{ busy ? '提交中…' : '确认创建' }}
              </Button>
            </template>
          </div>
        </div>
      </div>
    </div>
  </Teleport>
</template>
