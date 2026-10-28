/**
 * Execution-policy domain — the effective-risk overlay (#364; mirror
 * core/blitzkrieg_core/src/ipc/schema.rs wire shape).
 */
import { request } from './core.ts'

// ── #364 生效风控（mirror core/blitzkrieg_core/src/ipc/schema.rs 的 wire 形）──

/** 八个条件字段（issue 清单逐字；serde snake_case 的 wire 拼写）。 */
export type PolicyConditionField =
  | 'available_balance' | 'total_equity' | 'open_positions' | 'current_price'
  | 'time_left_sec' | 'symbol' | 'recent_pnl_1h' | 'consecutive_losses'

/** 七个比较算子。GET 应答里是 snake_case（`ge`），set 也接受符号拼写（`>=`）。 */
export type PolicyOpWire = 'lt' | 'le' | 'gt' | 'ge' | 'eq' | 'ne' | 'in' | string

/** 五个 then 动作（封闭集；止损/熔断不在词汇表里 — 结构性锁）。 */
export type PolicyThenAction = 'skip' | 'budget_ratio' | 'min_budget_usd' | 'max_budget_usd' | 'cooldown_sec'

/** `when` 的表格拼写（字符串拼写在内核侧也接受，UI 统一发表格形）。 */
export interface PolicyWhenView {
  field: PolicyConditionField | string
  op: PolicyOpWire
  /** 数字字段为 number，symbol 为字符串或字符串数组（plain-value 拼写）。 */
  value: number | string | string[]
}

/** `then`：恰一个动作（内核 `deny_unknown_fields` 保证；UI 同样只填一个）。 */
export interface PolicyThenView {
  action?: PolicyThenAction
  budget_ratio?: string
  min_budget_usd?: string
  max_budget_usd?: string
  cooldown_sec?: number
}

/** 一条规则。 */
export interface PolicyRuleView {
  name: string
  priority: number
  enabled: boolean
  when: PolicyWhenView
  then: PolicyThenView
  reason?: string
}

/**
 * 一个账户段的生效视图（`execution_policy.get` 应答本体；list 里 `section`
 * 的元素形）。字段可能缺席（内核 skip_serializing_if）— 缺席 = 继承全局。
 */
export interface ExecutionPolicySectionDoc {
  budgetRatio?: string
  minBudgetUsd?: string
  maxBudgetUsd?: string
  minEquityUsd?: string
  maxPositionsPerAsset: number
  rules?: PolicyRuleView[]
}

/** `execution_policy.list` 应答：账户 chips 的来源。 */
export interface ExecutionPolicyListDoc {
  loaded: boolean
  /** 全局段摘要（chips 里固定的 `defaults`）；未加载策略文件时为 null。 */
  defaults: ExecutionPolicySectionDoc | null
  /** 文件里已命名的账户段，每项带自己的生效视图。 */
  accounts: { accountId: string; section: ExecutionPolicySectionDoc }[]
  error?: string
}

/** 预览一行：一笔已平仓交易按当前规则的判定。 */
export interface ExecutionPolicyPreviewRow {
  tsMs: number
  symbol: string
  /** 预览时刻的活账本余额（账本是账，不是历史磁带 — 内核明示）。 */
  balance: string | number
  price: string | number
  /** `place` | `skip` | `cooldown` | `refused`。 */
  verdict: string
  /** place 的预算金额，或 skip/cooldown 的原因。 */
  detail?: string
}

/** `execution_policy.preview` 应答：最近 N 笔的重放（N≤100）。 */
export interface ExecutionPolicyPreviewDoc {
  considered: number
  skipped: number
  avgBudgetUsd?: string
  rows: ExecutionPolicyPreviewRow[]
  error?: string
}

/** 一条审计记录（`execution_policy.history` 的元素）。before/after 是段摘要。 */
export interface ExecutionPolicyAuditLine {
  tsMs: number
  actor: string
  /** `set` | `reset` | `load-refused`。 */
  action: string
  accountId: string
  before?: ExecutionPolicySectionDoc | null
  after?: ExecutionPolicySectionDoc | null
  error?: string
}

/** `execution_policy.history` 应答：裸数组，按写入顺序（升序）的审计流水。 */
export type ExecutionPolicyHistoryDoc = ExecutionPolicyAuditLine[]

/** `execution_policy.set` 参数：账户段的可写字段（小数全部字符串过线）。 */
export interface ExecutionPolicySetParams {
  accountId: string
  budgetRatio?: string
  minBudgetUsd?: string
  maxBudgetUsd?: string
  minEquityUsd?: string
  maxPositionsPerAsset?: number
  rules?: PolicyRuleView[]
}

export const executionPolicyApi = {
  // ── #364 生效风控（`execution_policy.*` IPC 的纯代理；TOML 永不出内核）─────
  // 与回测六条同一纪律：网关不加工参数（body 即 params，逐字转发），内核的
  // JSON-RPC 错误在网关侧包成 200 + `{error}` 文档 — 每个调用方都要检查
  // `error` 字段，原样抛给页面展示。小数一律字符串过线（最短十进制规则）。

  /** 哪些账户可配 + 策略文件是否加载（`execution_policy.list`）：chips 的来源。 */
  executionPolicyList: () => request<ExecutionPolicyListDoc>('/execution-policy/list'),
  /**
   * 生效视图（`execution_policy.get`）：内核自己把 `[accounts.<id>]` 折叠到
   * `[defaults]` 上 — 浏览器拿到的永远是内核视角的「实际生效」，不是文件原文。
   */
  executionPolicyGet: (accountId: string) =>
    request<ExecutionPolicySectionDoc>(
      `/execution-policy?accountId=${encodeURIComponent(accountId)}`,
    ),
  /**
   * 预览（`execution_policy.preview`）：最近 ~100 笔已平仓交易按当前规则重放
   * （内核自己的 `evaluate`，UI 端没有第二套规则引擎）。
   */
  executionPolicyPreview: (accountId: string) =>
    request<ExecutionPolicyPreviewDoc>(
      `/execution-policy/preview?accountId=${encodeURIComponent(accountId)}`,
    ),
  /**
   * 写路径（`execution_policy.set`）：内核落盘→重读→落审计后才应答，应答
   * `{accountId}` — UI 保存后自己重新 `get` 刷新，不靠本地猜测。
   */
  executionPolicySet: (params: ExecutionPolicySetParams) =>
    request<{ accountId: string; error?: string }>('/execution-policy/set', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 删除账户段（`execution_policy.reset`）：该账户折回 `[defaults]`。 */
  executionPolicyReset: (accountId: string) =>
    request<{ accountId: string; error?: string }>('/execution-policy/reset', {
      method: 'POST',
      body: JSON.stringify({ accountId }),
    }),
  /** 审计流水（`execution_policy.history`）：版本列表与回滚快照的来源。 */
  executionPolicyHistory: (accountId: string) =>
    request<ExecutionPolicyHistoryDoc>(
      `/execution-policy/history?accountId=${encodeURIComponent(accountId)}`,
    ),
}
