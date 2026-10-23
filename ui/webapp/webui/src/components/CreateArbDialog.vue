<script setup lang="ts">
/**
 * 创建套利弹窗 —— 策略页「创建套利」入口的配置面（配置意图模型，合同 v1）。
 *
 * 模型裁决（2026-10）：跨所套利从「发现机会」改为「配置意图」—— 操作员
 * 选 A 腿（交易所+账户）+ B 腿（交易所+账户）+ 币对多选，创建即持久化
 * （内核写 accounts.toml，面板零持久化职责）。旧的「选目标映射」段整体
 * 作废：它等一个 live 链路产不出的候选（unified.candidates），弹窗永远
 * 无法完成创建 —— 概念连同「已配对无差异」这个映射筛选条件一起删。
 *
 * 四段 + 确认页：
 *
 *   段0 A 腿 —— 交易所下拉（venue 列表数据渲染）+ 账户下拉。账户过滤在
 *        源头：venue 匹配（`account.venue == 所选 || venue == ''`），
 *        read_only/frozen 状态不进列表；唯一匹配自动预选；换交易所清空
 *        账户选择重过滤。
 *   段1 B 腿 —— 同段0，但交易所排除段0 已选（B≠A 硬门之一）、账户 ≠ A
 *        账户（硬门之二）。两道门前端先拦（禁用下一步 + 说明文案），
 *        内核校验是第二道（APPLICATION 错误原文转述）。
 *   段2 币对多选 —— 复选框组，候选集按探测结论：内核没有 assets 配置
 *        读取动词，面板用静态候选集并在确认页如实披露「候选集为面板
 *        静态清单」。至少选 1。
 *   段3 确认页 —— 只读汇总（两腿账户@venue + 凭证状态 + 币对清单），
 *        per-leg 凭证就绪复用 market.credentials.status 读数（issue 542
 *        的面）：ready=false 禁用创建；行查不到 = 显示「凭证按交易所
 *        全局环境变量」+ 缺失变量名（若有）。
 *   参数预设 —— 保留四个 tunables 的编辑，只提交与默认不同的项。
 *
 * 提交序（合同）：arb.config.set 成功 → setStrategy(selectedArbStrategy,
 * true, legAAccount) 启用；任一步失败停在弹窗内转述错误原文。
 *
 * 去标题化 + 策略选择段（2026-10）：cross_venue_arb 只是套利策略，不是套利
 * 本身 —— 标题回归中性词「创建套利」，弹窗首段是「策略」选择。候选面探测
 * 结论：strategy.list 行 wire 上只有 name/enabled/modes/compatible/
 * incompatibleReason/accounts，modes wire 对象只带 market_type/structure
 * —— 没有任何运行时字段能判定「谁是套利策略」（atomic_batch 只活在
 * manifest 里，不上 wire），所以候选集是静态单选项，平台化时换成能力
 * 声明驱动（内核 strategy.list 行带出套利能力后，候选集从该读数派生）。
 * 内核 arb.config.set 的 IPC 动词只认 accounts.toml 的 [cross_venue_arb]
 * 节 —— 选其它名字 = 该策略暂未支持，置灰不假提交（不提交一个内核必拒
 * 的配置）。
 *
 * 老内核降级：arb.config.get feature-detect 失败（error 带 rpc -32601
 * 字样 = 动词不存在）→ 弹窗降级为只读提示「当前内核版本为单账户模型，
 * 双腿双账户需升级内核」+ 禁用创建。不降级为旧的单账户单选。
 *
 * venue 纪律：venue 一律数据渲染（no-venue-branch 门禁）；组件不分支于
 * 任何具体交易所名。文案纪律：账户是腿的属性 —— 全面板不出现那两个把
 * 账户说成独立执行体的旧词（见 arb-dialog-v2 gate 的禁词表）。
 */
import { computed, ref, watch } from 'vue'
import { Link2, SlidersHorizontal, Wallet, Zap } from 'lucide-vue-next'
import {
  api,
  type PluginCredentialRowDoc,
  type ArbConfigDoc,
} from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import Button from '@/components/ui/button/Button.vue'
import Input from '@/components/ui/input/Input.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'

const props = defineProps<{
  open: boolean
  /** 当前显示账户 id（issue 561：面板唯一可信账户源；store.activeAccount
   *  是它的兼容读数；null = 老面板/单账户）。段0 选择器的初始值——没有
   *  更高的事实源，操作员可改选。 */
  activeAccountId: string | null
  /**
   * 显示账户的绑定 venue（''/undefined = 未绑定）。store.activeAccountVenue
   * 已 display 优先派生；弹窗对**选中的腿账户**另走 account.venue.get
   * 复核绑定读数 —— 两处不一致时以内核读数为准。
   */
  activeVenue: string
}>()

const emit = defineEmits<{
  'update:open': [v: boolean]
  /** 提交完成（无论成败）—— 父页面据此刷新 store 读数。 */
  committed: []
}>()

/**
 * 套利策略候选集（静态单选项）。平台化时换成能力声明驱动 —— 内核
 * strategy.list 行带出套利/双腿能力声明后，候选集从该读数派生（wire 现状
 * 无可判定字段，见文件头探测结论）。单实例合同：候选集里的名字必须 =
 * ARB_STRATEGY（内核 arb.config.set 只认 [cross_venue_arb] 节），新增名字
 * 时内核未接即「暂未支持」，置灰不假提交。
 */
const ARB_STRATEGY = 'cross_venue_arb' // 平台化时换成配置驱动
// 候选数据源：静态清单（wire 无套利能力声明字段）；平台化时换成能力声明驱动。
const ARB_STRATEGY_OPTIONS: string[] = [ARB_STRATEGY]

/** 选中策略是否已支持：唯一裁决 = ARB_STRATEGY 合同（内核只接这个名字）。
 *  未来加策略时把支持集从合同里拆出（内核动词认几个就放几个）。 */
function strategySupported(name: string): boolean {
  return name === ARB_STRATEGY
}

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

/**
 * 币对候选集（探测结论：内核暂无 assets 配置读取动词，面板用静态清单，
 * 确认页如实披露）。venue 是数据、币对也是数据 —— 新 venue/新币对接入
 * 面板零分支。
 */
const PAIR_CANDIDATES: string[] = ['BTC', 'ETH', 'SOL', 'XRP']

// ── 账户/交易所候选（源头过滤）──────────────────────────────────────────────
const store = usePanelStore()

/** 可交易状态白名单：read_only/frozen（及其余非 active 态）不进腿选择。 */
function tradable(status: string): boolean {
  return status === 'active' || status === ''
}

/** 一条腿的交易所选项 = 已接入 venue 的数据行。来源 = store 快照的市场
 *  插件注册表（marketPlugins 行带 name/type —— venue 是数据不是分支），
 *  注册表缺席（老内核）回落 plugins 读数；两者都空 = 空下拉 + 说明。 */
const venueOptions = computed<string[]>(() => {
  const fromSnapshot = (store.snapshot?.marketPlugins ?? [])
    .map((p) => p.name)
    .filter((n): n is string => !!n)
  if (fromSnapshot.length) return [...new Set(fromSnapshot)].sort()
  const fromPlugins = store.plugins?.marketPlugins ?? []
  const names = fromPlugins.map((p) => p.name).filter((n): n is string => !!n)
  return [...new Set(names)].sort()
})

/** 账户在所选 venue 下可选 = venue 匹配（绑定留空 = 可交易任意交易所）
 *  且状态可交易（read_only/frozen 不进列表）。 */
function accountFitsVenue(a: { venue?: string; status: string }, venue: string): boolean {
  if (!tradable(a.status)) return false
  const bound = (a.venue ?? '').trim()
  return bound === '' || bound === venue
}

interface LegAccountOption {
  id: string
  name: string
  venue: string
}

/** 一条腿的账户选项：按该腿所选 venue 源头过滤后的 store.accounts 行。 */
function legAccountOptions(venue: string): LegAccountOption[] {
  return store.accounts
    .filter((a) => accountFitsVenue(a, venue))
    .map((a) => ({ id: a.id, name: a.name, venue: a.venue ?? '' }))
}

// ── 段0 策略选择（首段）：现阶段唯一 cross_venue_arb ────────────────────────
const selectedArbStrategy = ref(ARB_STRATEGY)

/** 未支持的策略名不假提交：禁用下一步并在段内如实提示（内核 arb.config.set
 *  只认 [cross_venue_arb] 节，提交一个必拒的配置是撒谎）。 */
const strategyUnsupported = computed(() => !strategySupported(selectedArbStrategy.value))

// ── 段1 A 腿 / 段2 B 腿 的选择状态 ──────────────────────────────────────────
const legAVenue = ref('')
const legAAccount = ref('')
const legBVenue = ref('')
const legBAccount = ref('')

/** 每腿的账户选项（源头过滤结果；交易所未选 = 空 + 文案说明）。 */
const legAAccountOptions = computed(() => legAccountOptions(legAVenue.value))
const legBAccountOptions = computed(() => legAccountOptions(legBVenue.value))

/** 换交易所 ⇒ 清空该腿账户选择重过滤（用户动作才清 —— 内核配置预填的
 *  账户合法地随预填 venue 到位，程序化赋值不走这条）。清空后若该 venue
 *  下只剩一个可选账户，自动预选（唯一匹配 = 无歧义）。 */
function onLegAVenueChange(): void {
  const opts = legAccountOptions(legAVenue.value)
  legAAccount.value = opts.length === 1 ? opts[0].id : ''
}
function onLegBVenueChange(): void {
  const opts = legAccountOptions(legBVenue.value)
  legBAccount.value = opts.length === 1 ? opts[0].id : ''
}

/** 账户选择为空且过滤后唯一匹配 ⇒ 自动预选。覆盖：打开弹窗（store 账户簿
 *  可能晚到）、清空重过滤后、账户簿异步刷新 —— 三个时刻的同一规则。 */
watch([legAVenue, () => store.accounts], () => {
  if (legAAccount.value) return
  const opts = legAccountOptions(legAVenue.value)
  if (opts.length === 1) legAAccount.value = opts[0].id
})
watch([legBVenue, () => store.accounts], () => {
  if (legBAccount.value) return
  const opts = legAccountOptions(legBVenue.value)
  if (opts.length === 1) legBAccount.value = opts[0].id
})

/** 交易所下拉排除对腿已选交易所：B≠A 硬门之一（B 排除 A；A 排除 B）。 */
const legAVenueOptions = computed(() =>
  venueOptions.value.filter((v) => v !== legBVenue.value),
)
const legBVenueOptions = computed(() =>
  venueOptions.value.filter((v) => v !== legAVenue.value),
)

/** 账户选项补行的说明：为什么这条腿的账户下拉是空的。 */
const legAAccountEmptyHint = computed(() => {
  if (!venueOptions.value.length) return '暂无已接入交易所读数 —— 内核注册表为空或快照未到达。'
  if (!legAVenue.value) return '先选 A 腿交易所。'
  return '该交易所下没有可选账户（无 venue 匹配，或账户处于只读/冻结状态）。'
})
const legBAccountEmptyHint = computed(() => {
  if (!venueOptions.value.length) return '暂无已接入交易所读数 —— 内核注册表为空或快照未到达。'
  if (!legBVenue.value) return '先选 B 腿交易所（不能与 A 腿相同）。'
  return '该交易所下没有可选账户（无 venue 匹配，或账户处于只读/冻结状态）。'
})

// ── 段2 币对多选 ────────────────────────────────────────────────────────────
const selectedPairs = ref<string[]>([])

function togglePair(p: string): void {
  const i = selectedPairs.value.indexOf(p)
  if (i >= 0) selectedPairs.value = selectedPairs.value.filter((x) => x !== p)
  else selectedPairs.value = [...selectedPairs.value, p]
}

// ── 参数预设草稿：只发改动项 ────────────────────────────────────────────────
const paramsDraft = ref<Record<string, string>>({})
const paramsError = ref<string | null>(null)
/** 打开弹窗时是否已有声明参数读数（读失败时确认页要如实说明预设未验证）。 */
const paramsReadFailed = ref(false)

async function loadCurrentParams(): Promise<void> {
  paramsReadFailed.value = false
  const draft: Record<string, string> = { ...PARAM_DEFAULTS }
  try {
    const doc = await api.strategyParams(selectedArbStrategy.value)
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

// ── 配置预填（arb.config.get）+ 老内核 feature-detect ──────────────────────
const configLoading = ref(false)
/** get 回执原文（configured=true 时按腿回显；null = 未读/读失败）。 */
const configDoc = ref<ArbConfigDoc | null>(null)
/**
 * 动词不存在的降级面：老内核答 JSON-RPC unknown-method（错误文本带
 * `rpc -32601`）—— 弹窗降级为只读提示 + 禁用创建，绝不降级为旧的单
 * 账户单选（那是一个产不出双腿的模型，恢复它就是复活死锁）。
 */
const legacyCore = ref(false)
const configReadError = ref<string | null>(null)

function detectLegacy(e: unknown): boolean {
  const msg = e instanceof Error ? e.message : String(e)
  return /rpc\s*-32601|method not found/i.test(msg)
}

async function loadConfig(): Promise<void> {
  configLoading.value = true
  legacyCore.value = false
  configDoc.value = null
  configReadError.value = null
  try {
    const doc = await api.arbConfigGet()
    if (doc.error) {
      if (detectLegacy(new Error(doc.error))) {
        legacyCore.value = true
      } else {
        configReadError.value = doc.error
      }
      return
    }
    configDoc.value = doc
    if (doc.configured) {
      legAVenue.value = doc.legAVenue ?? ''
      legAAccount.value = doc.legAAccount ?? ''
      legBVenue.value = doc.legBVenue ?? ''
      legBAccount.value = doc.legBAccount ?? ''
      selectedPairs.value = [...(doc.pairs ?? [])]
    } else if (props.activeAccountId) {
      // 未配置过：账户下拉没有内核读数可预填 —— 用面板 default 账户作
      // 初始提示（没有更高的事实源，操作员可改选）。
      const row = store.accounts.find((a) => a.id === props.activeAccountId)
      if (row) {
        const v = (row.venue ?? '').trim()
        if (v && venueOptions.value.includes(v)) legAVenue.value = v
      }
    }
  } catch (e) {
    if (detectLegacy(e)) {
      legacyCore.value = true
    } else {
      configReadError.value = e instanceof Error ? e.message : String(e)
    }
  } finally {
    configLoading.value = false
  }
}

// ── per-leg 凭证就绪（issue 542 的既有面：market.credentials.status）───────
// ready 是行数据，venue 是行键 —— 不分支于任何具体交易所名。行查不到 =
// 「凭证按交易所全局环境变量」+ 缺失变量名（若有），不阻断但如实披露。

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

interface LegCredStatus {
  venue: string
  /** rows 里有该 plugin 行且 ready=true。 */
  ready: boolean
  /** requiredVars − resolvedVars（ready=false 时的缺失变量名，如实列出）。 */
  missingVars: string[]
  /** rows 里没有该 plugin 的行 —— 读数覆盖不到，不阻断但必须披露。 */
  unknown: boolean
}

function legCredStatus(venue: string): LegCredStatus {
  const row = credRows.value?.find((r) => r.plugin === venue)
  if (!row) return { venue, ready: false, missingVars: [], unknown: true }
  const resolved = new Set(row.resolvedVars)
  return {
    venue,
    ready: row.ready,
    missingVars: row.requiredVars.filter((name) => !resolved.has(name)),
    unknown: false,
  }
}

const legACred = computed<LegCredStatus | null>(() =>
  legAVenue.value ? legCredStatus(legAVenue.value) : null,
)
const legBCred = computed<LegCredStatus | null>(() =>
  legBVenue.value ? legCredStatus(legBVenue.value) : null,
)

/** 运行模式（快照顶层 mode；缺省读 dry —— 与各页面 isDry 同一谓词）。
 *  凭证门是 LIVE 语义：内核 arb_precheck 的凭证门包在 `mode.may_trade()`
 *  里（dry 模式不拦、策略照跑模拟成交），面板无条件拦是超owner ——
 *  dry 下创建不得被「腿凭证未就绪」挡（dry run 不需要账户凭证），
 *  live 下维持原门。 */
const isDry = computed(() => (store.snapshot?.mode ?? 'dry') === 'dry')

/** dry 模式的凭证门豁免（有名字的独立谓词）：两腿凭证本就绪时豁免无感；
 *  未就绪时确认页照列缺失变量（切 live 前操作员仍需读到），只是不再阻断。 */
const dryCredExempt = computed(() => isDry.value)

/** 明确 ready=false 的腿（创建按钮禁用判据 —— 保守方向：明确未就绪不许过）。
 *  行查不到（unknown）= 未知 —— 不阻断，确认页的「凭证按交易所全局环境
 *  变量」披露兜底。dry 模式豁免（见 isDry 注释）——内核同门只对 live 生效。 */
const credBlocked = computed(() =>
  !dryCredExempt.value &&
  [legACred.value, legBCred.value].some((leg) => leg && !leg.unknown && !leg.ready),
)

/** 凭证读数读取失败（请求层）——确认页如实披露「无法验证两腿凭证就绪」。 */
const credReadFailed = computed(() => credRows.value === null)

// ── 硬门 + 步进 ─────────────────────────────────────────────────────────────
const step = ref<'edit' | 'confirm'>('edit')
const busy = ref(false)
const error = ref<string | null>(null)

/** B≠A 硬门之二的文案与判据：B 账户不得与 A 账户相同。 */
const sameAccountClash = computed(
  () =>
    !!legAAccount.value &&
    legAAccount.value === legBAccount.value,
)

/** 四段的完成判据：两腿各选了交易所+账户，币对至少选 1。 */
const legsComplete = computed(
  () =>
    !!legAVenue.value &&
    !!legAAccount.value &&
    !!legBVenue.value &&
    !!legBAccount.value &&
    !sameAccountClash.value,
)
const pairsComplete = computed(() => selectedPairs.value.length >= 1)

/** 可以进确认页 = 两腿齐 + 币对齐 + 凭证校验未明确失败 + 策略已支持。 */
const canReview = computed(() => legsComplete.value && pairsComplete.value && !credBlocked.value && !strategyUnsupported.value)

/** 下一步按钮的禁用原因（编辑页底部实时文案 —— 门在前端先拦）。 */
const reviewBlockReason = computed<string | null>(() => {
  if (legacyCore.value) return '老内核不支持双腿配置'
  if (strategyUnsupported.value) return '所选策略暂未支持 —— 当前内核只接受 cross_venue_arb'
  if (credBlocked.value) return '腿凭证未就绪 —— 提交前需补齐'
  if (sameAccountClash.value) return 'B 腿账户不能与 A 腿相同 —— 双腿双账户是配置意图模型的硬约束'
  if (!legAVenue.value || !legAAccount.value) return '先完成 A 腿（交易所 + 账户）'
  if (!legBVenue.value || !legBAccount.value) return '先完成 B 腿（交易所 + 账户）—— B 腿交易所不能与 A 腿相同'
  if (!pairsComplete.value) return '至少选择一个币对'
  return null
})

watch(
  () => props.open,
  (v) => {
    if (!v) return
    step.value = 'edit'
    error.value = null
    paramsError.value = null
    selectedArbStrategy.value = ARB_STRATEGY
    selectedPairs.value = []
    legAVenue.value = ''
    legAAccount.value = ''
    legBVenue.value = ''
    legBAccount.value = ''
    void loadConfig()
    void loadCurrentParams()
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

/** 提交序（合同）：arb.config.set 成功 → setStrategy on（A 腿账户作用域
 *  启用）；任一步失败停在弹窗内转述错误原文（消息点名非法字段 —— 原样
 *  给操作员，面板不代内核措辞）。 */
async function commit(): Promise<void> {
  if (!canReview.value || busy.value) return
  busy.value = true
  error.value = null
  try {
    const cfg = await api.arbConfigSet({
      version: 1,
      legAVenue: legAVenue.value,
      legAAccount: legAAccount.value,
      legBVenue: legBVenue.value,
      legBAccount: legBAccount.value,
      pairs: [...selectedPairs.value],
    })
    if (cfg.error) throw new Error(cfg.error)
    const res = await api.setStrategy(selectedArbStrategy.value, true, legAAccount.value)
    if (res.ok === false || res.reason) {
      // 内核拒绝（模式握手/已启用等）：如实转述 reason，配置已落盘 —— 文案
      // 说清哪一步失败，让操作员知道配置不再需要重写。
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

/** 腿的账户显示名（确认页只读汇总）。 */
function accountName(id: string): string {
  return store.accounts.find((a) => a.id === id)?.name ?? id
}

/** 凭证状态的中文短语（确认页 per-leg 披露块）。 */
function credLabel(leg: LegCredStatus | null): string {
  if (!leg) return '未选交易所'
  if (leg.unknown) return '凭证按交易所全局环境变量'
  return leg.ready ? '凭证就绪' : '凭证未就绪'
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
          <h3 class="text-[13.5px] font-semibold">创建套利</h3>
          <Badge variant="outline" class="ml-auto">{{ step === 'edit' ? '配置双腿意图' : '提交前确认' }}</Badge>
        </div>

        <div class="min-h-0 flex-1 space-y-3.5 overflow-y-auto px-4 py-3">
          <!-- ═══ 老内核降级（只读提示，不降级为旧的单账户单选）═══ -->
          <template v-if="legacyCore">
            <EmptyState
              text="当前内核版本为单账户模型，双腿双账户需升级内核"
              hint="本弹窗的配置意图模型（A 腿 + B 腿 + 币对）依赖 arb.config.set / arb.config.get 动词；旧内核不认识该动词，创建面随之停用 —— 恢复旧的单账户单选不是降级选项（那条链路产不出双腿）。"
            />
          </template>

          <!-- ═══ 编辑页：四段 ═══ -->
          <template v-else-if="step === 'edit'">
            <div v-if="configLoading" class="text-[12px] text-faint-fg">双腿配置读取中…</div>
            <AlertBanner
              v-else-if="configReadError"
              tone="warn"
              title="已配置双腿读取失败 —— 以下为空白草稿，不影响创建。"
              :hint="configReadError"
            />

            <!-- 段0 策略选择：候选面静态（wire 无套利能力字段，见文件头探测
                 结论）；现阶段唯一 cross_venue_arb。选其它名字 = 暂未支持，
                 置灰不假提交。 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Zap class="size-3 text-faint-fg" />
                <span class="label-micro">策略</span>
                <span class="text-[11px] text-faint-fg">—— 要启用的套利策略；当前可用清单如下。</span>
              </div>
              <div class="mt-1.5 grid gap-2">
                <label
                  v-for="s in ARB_STRATEGY_OPTIONS"
                  :key="s"
                  class="flex cursor-pointer items-center gap-2 rounded-md border px-2.5 py-2 text-[12px]"
                  :class="selectedArbStrategy === s ? 'border-primary/55 bg-primary/8' : 'border-line bg-panel-2/40'"
                >
                  <input
                    type="radio"
                    name="arb-strategy"
                    class="accent-[var(--primary)]"
                    :checked="selectedArbStrategy === s"
                    @change="selectedArbStrategy = s"
                  />
                  <span class="num font-semibold">{{ s }}</span>
                </label>
                <div
                  v-if="strategyUnsupported"
                  class="rounded-md border border-down/30 bg-down/10 px-2.5 py-2 text-[11.5px] text-down"
                >
                  该策略暂未支持 —— 当前内核的套利配置动词只接受 cross_venue_arb，创建已禁用（不提交必被拒绝的配置）。
                </div>
              </div>
            </div>

            <!-- 段1 A 腿：交易所 + 账户。账户过滤在源头（venue 匹配 + 可交易
                 状态），换交易所清空账户选择重过滤。 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Wallet class="size-3 text-faint-fg" />
                <span class="label-micro">A 腿</span>
                <span class="text-[11px] text-faint-fg">—— 交易所 + 账户；账户是腿的属性，跟随所选交易所过滤。</span>
              </div>
              <div class="mt-1.5 grid grid-cols-2 gap-2">
                <label class="grid gap-1 text-[12px]">
                  <span class="text-muted-fg">交易所</span>
                  <select v-model="legAVenue" class="policy-select" :disabled="configLoading" @change="onLegAVenueChange">
                    <option value="" disabled>选择交易所</option>
                    <option v-for="v in legAVenueOptions" :key="v" :value="v">{{ v }}</option>
                  </select>
                </label>
                <label class="grid gap-1 text-[12px]">
                  <span class="text-muted-fg">账户</span>
                  <select v-model="legAAccount" class="policy-select" :disabled="!legAVenue || configLoading">
                    <option value="" disabled>选择账户</option>
                    <option v-for="a in legAAccountOptions" :key="a.id" :value="a.id">{{ a.name }}</option>
                  </select>
                </label>
              </div>
              <div
                v-if="legAVenue && !legAAccountOptions.length"
                class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg"
              >
                {{ legAAccountEmptyHint }}
              </div>
              <div
                v-else-if="!venueOptions.length"
                class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg"
              >
                {{ legAAccountEmptyHint }}
              </div>
            </div>

            <!-- 段1 B 腿：同段0，交易所下拉排除 A 腿已选（B≠A 硬门之一）；
                 账户 ≠ A 账户（硬门之二）在前端先拦。 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Wallet class="size-3 text-faint-fg" />
                <span class="label-micro">B 腿</span>
                <span class="text-[11px] text-faint-fg">—— 交易所不能与 A 腿相同，账户不能与 A 腿账户相同。</span>
              </div>
              <div class="mt-1.5 grid grid-cols-2 gap-2">
                <label class="grid gap-1 text-[12px]">
                  <span class="text-muted-fg">交易所</span>
                  <select v-model="legBVenue" class="policy-select" :disabled="configLoading" @change="onLegBVenueChange">
                    <option value="" disabled>选择交易所</option>
                    <option v-for="v in legBVenueOptions" :key="v" :value="v">{{ v }}</option>
                  </select>
                </label>
                <label class="grid gap-1 text-[12px]">
                  <span class="text-muted-fg">账户</span>
                  <select v-model="legBAccount" class="policy-select" :disabled="!legBVenue || configLoading">
                    <option value="" disabled>选择账户</option>
                    <option v-for="a in legBAccountOptions" :key="a.id" :value="a.id">{{ a.name }}</option>
                  </select>
                </label>
              </div>
              <div
                v-if="sameAccountClash"
                class="mt-1.5 rounded-md border border-down/30 bg-down/10 px-2.5 py-2 text-[11.5px] text-down"
              >
                B 腿账户不能与 A 腿相同 —— 换一个账户，否则双腿落在同一账户上（配置意图模型的硬约束）。
              </div>
              <div
                v-else-if="legBVenue && !legBAccountOptions.length"
                class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg"
              >
                {{ legBAccountEmptyHint }}
              </div>
            </div>

            <!-- 段2 币对多选：静态候选集（探测结论：内核无 assets 配置读取
                 动词）—— 至少选 1，确认页披露候选集来源。 -->
            <div>
              <div class="flex items-center gap-1.5">
                <Link2 class="size-3 text-faint-fg" />
                <span class="label-micro">币对</span>
                <span class="text-[11px] text-faint-fg">—— 至少选 1；候选集为面板静态清单。</span>
              </div>
              <div class="mt-1.5 flex flex-wrap gap-1.5">
                <label
                  v-for="p in PAIR_CANDIDATES"
                  :key="p"
                  class="flex cursor-pointer items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12px]"
                  :class="selectedPairs.includes(p) ? 'border-primary/55 bg-primary/8' : 'border-line bg-panel-2/40'"
                >
                  <input
                    type="checkbox"
                    class="accent-[var(--primary)]"
                    :checked="selectedPairs.includes(p)"
                    @change="togglePair(p)"
                  />
                  <span class="num">{{ p }}</span>
                </label>
              </div>
              <div v-if="selectedPairs.length" class="mt-1 text-[11px] text-faint-fg">
                已选 {{ selectedPairs.length }} 个币对。
              </div>
            </div>

            <!-- 参数预设段：保留现状（四个 tunables，只提交与默认不同的项）-->
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

            <!-- 段3 提示段：启用动作说明（配置写入 + A 腿账户作用域启用）-->
            <div>
              <div class="flex items-center gap-1.5">
                <Zap class="size-3 text-faint-fg" />
                <span class="label-micro">创建动作</span>
              </div>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[11.5px] leading-relaxed text-muted-fg">
                提交后：先写入双腿配置（内核持久化进 accounts.toml，重启保留），再以 A 腿账户作用域启用所选套利策略（{{ selectedArbStrategy }}）。
                两腿各按其交易所的全局凭证（env）签名；凭证就绪见确认页披露。
              </div>
            </div>

            <div v-if="error">
              <AlertBanner tone="error" title="提交被拒" :hint="error">
                {{ error }}
              </AlertBanner>
            </div>
          </template>

          <!-- ═══ 确认页：只读汇总 + 提交 ═══ -->
          <template v-else>
            <div>
              <span class="label-micro">A 腿</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div class="flex flex-wrap items-center gap-1.5">
                  <span class="font-semibold">{{ accountName(legAAccount) }}</span>
                  <span class="text-[11px] text-faint-fg">{{ legAAccount }}</span>
                  <Badge variant="outline">@ {{ legAVenue }}</Badge>
                </div>
                <div class="mt-1 flex flex-wrap items-center gap-1.5 text-[11px]">
                  <span class="text-muted-fg">凭证状态</span>
                  <template v-if="credReadFailed">
                    <Badge variant="gold">读数读取失败，无法验证</Badge>
                  </template>
                  <template v-else-if="legACred">
                    <Badge v-if="legACred.unknown" variant="gold">{{ credLabel(legACred) }}</Badge>
                    <Badge v-else-if="legACred.ready" variant="up">{{ credLabel(legACred) }}</Badge>
                    <Badge v-else variant="down">{{ credLabel(legACred) }}</Badge>
                    <span v-if="!legACred.unknown && !legACred.ready" class="num text-down">
                      缺变量：{{ legACred.missingVars.join('、') || '读数未列出' }}
                    </span>
                    <span v-else-if="legACred.unknown && legACred.missingVars.length" class="num text-muted-fg">
                      检测到缺失变量：{{ legACred.missingVars.join('、') }}
                    </span>
                  </template>
                </div>
              </div>
            </div>

            <div>
              <span class="label-micro">B 腿</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div class="flex flex-wrap items-center gap-1.5">
                  <span class="font-semibold">{{ accountName(legBAccount) }}</span>
                  <span class="text-[11px] text-faint-fg">{{ legBAccount }}</span>
                  <Badge variant="outline">@ {{ legBVenue }}</Badge>
                </div>
                <div class="mt-1 flex flex-wrap items-center gap-1.5 text-[11px]">
                  <span class="text-muted-fg">凭证状态</span>
                  <template v-if="credReadFailed">
                    <Badge variant="gold">读数读取失败，无法验证</Badge>
                  </template>
                  <template v-else-if="legBCred">
                    <Badge v-if="legBCred.unknown" variant="gold">{{ credLabel(legBCred) }}</Badge>
                    <Badge v-else-if="legBCred.ready" variant="up">{{ credLabel(legBCred) }}</Badge>
                    <Badge v-else variant="down">{{ credLabel(legBCred) }}</Badge>
                    <span v-if="!legBCred.unknown && !legBCred.ready" class="num text-down">
                      缺变量：{{ legBCred.missingVars.join('、') || '读数未列出' }}
                    </span>
                    <span v-else-if="legBCred.unknown && legBCred.missingVars.length" class="num text-muted-fg">
                      检测到缺失变量：{{ legBCred.missingVars.join('、') }}
                    </span>
                  </template>
                </div>
              </div>
            </div>

            <div>
              <span class="label-micro">币对清单</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <div class="flex flex-wrap gap-1.5">
                  <Badge v-for="p in selectedPairs" :key="p" variant="outline">{{ p }}</Badge>
                </div>
                <div class="mt-1 text-[11px] text-faint-fg">
                  候选集为面板静态清单（{{ PAIR_CANDIDATES.join(' / ') }}）—— 内核暂无已接入币对的配置读取动词，面板不谎称这是内核读数。
                </div>
              </div>
            </div>

            <div>
              <span class="label-micro">凭证就绪（提交前校验）</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px] leading-relaxed">
                <div v-if="isDry && !credReadFailed && credRows?.some((r) => !r.ready)" class="text-muted-fg">
                  Dry 模式：凭证门不生效（内核同门只对 live 拦）—— 两腿凭证未就绪照实列出（{{ credRows.filter((r) => !r.ready).map((r) => r.plugin).join('、') }}），切 Live 前需补齐。
                </div>
                <div v-else-if="credReadFailed" class="text-muted-fg">
                  凭证读数读取失败，无法验证两腿凭证就绪 —— 提交不因此阻断，但执行器可能因缺凭证 inert（挂单无人拉取）。
                </div>
                <div v-else-if="credBlocked" class="text-down">
                  存在明确未就绪的腿 —— 创建已禁用，先补齐该交易所的凭证变量。
                </div>
                <div v-else class="text-muted-fg">
                  两腿凭证读数未明确失败；读数覆盖不到的交易所按「凭证按交易所全局环境变量」处理，缺失变量名已逐腿列出。
                </div>
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
              <span class="label-micro">策略</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                <span class="num font-semibold">{{ selectedArbStrategy }}</span>
                <div v-if="strategyUnsupported" class="mt-1 text-[11px] text-down">
                  该策略暂未支持 —— 当前内核只接受 cross_venue_arb，返回上一页改选。
                </div>
              </div>
            </div>

            <div>
              <span class="label-micro">启用动作</span>
              <div class="mt-1.5 rounded-md border border-line bg-panel-2/40 px-2.5 py-2 text-[12px]">
                提交后启用 {{ selectedArbStrategy }}（arb.config.set 成功 → strategy {{ selectedArbStrategy }} on，A 腿账户作用域）。
                <div class="mt-1 text-[11px] text-faint-fg">
                  双腿配置由内核持久化（accounts.toml，重启保留）；启用失败时配置已落盘，修复原因后无需重写配置。
                </div>
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
              <span v-if="!canReview && reviewBlockReason" class="text-[11px] text-down">{{ reviewBlockReason }}</span>
              <Button variant="gold" size="sm" :disabled="!canReview" @click="step = 'confirm'">
                下一步：确认
              </Button>
            </template>
            <template v-else>
              <span
                v-if="credBlocked"
                class="text-[11px] text-down"
                title="market.credentials.status 读数里该交易所的凭证 ready=false"
              >腿凭证未就绪 —— 创建禁用</span>
              <Button variant="gold" size="sm" :disabled="busy || credBlocked" @click="commit">
                {{ busy ? '提交中…' : '创建' }}
              </Button>
            </template>
          </div>
        </div>
      </div>
    </div>
  </Teleport>
</template>
