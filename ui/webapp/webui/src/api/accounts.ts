/**
 * Accounts domain — the account-scoped surface: venue binding, display
 * account, trading/strategy fan-out, runtime create/remove, the per-account
 * rounds whitelist, the wallet journal, and the per-account aggregates.
 */
import { request } from './core.ts'
import type { CumulativeTradeSummary, EngineStats } from './snapshot.ts'

// ── 钱包现金流水（account.journal，#509 内核 / #511 动词）────────────────────

/** 一行流水：JournalEntry 的 camelCase wire 形状，amount/balanceAfter 是
 *  crate::decimal 字符串（内核约定：小数一律字符串过线）。 */
export interface AccountJournalRowDoc {
  atMs: number
  /** snake_case：seed/buy_fill/sell_fill/fee/merge/redemption/cashflow/realign/reset。 */
  kind: string
  amount: string
  balanceAfter: string
  /** 产生该行的来源标记（boot / venue / 订单 id / 审计行 …）。 */
  ref: string
}

/** `account.journal` 回执：最近 limit 行（升序）；truncated = 内存窗口比返回
 *  的多 —— 读者看到的是窗口（JOURNAL_CAP = 10000），不是全史。 */
export interface AccountJournalDoc {
  accountId: string
  rows: AccountJournalRowDoc[]
  truncated: boolean
  error?: string
}

// ── 账户↔交易所绑定（account.venue.get / account.venue.set）─────────────────

/** 一行绑定：venue 空串 = 未绑定（可交易任意交易所）。 */
export interface AccountVenueRowDoc {
  accountId: string
  venue: string
  /** 谁写的（boot / panel / venue-derive-migration …）。 */
  actor: string
  updatedAtMs: number
}

/** `account.venue.get` 回执。 */
export interface AccountVenueDoc {
  version: string
  venues: AccountVenueRowDoc[]
  /** 本连接的默认账户（会话切换后 = 会话值）。 */
  active: string
  error?: string
}

/** `account.venue.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface AccountVenueSetDoc {
  accountId: string
  venue: string
  persisted: boolean
  atMs: number
  actor: string
  note: string
  error?: string
}

// ── 跨所套利创建（arb.config.set / arb.config.get，合同 v1）─────────────────
// 配置意图模型：A 腿（交易所+账户）+ B 腿（交易所+账户）+ 币对多选，创建即
// 持久化（内核写 accounts.toml，面板零持久化职责）。专用代理路由（body 即
// params 逐字转发，同 account-venue/set 的纪律）。B≠A（venue 与账户两个维度）
// 的校验在内核是 APPLICATION 错误 —— 消息点名哪个字段非法，弹窗把原文转述。

/** `arb.config.set` 请求体（camelCase wire 与合同 v1 对齐；version 恒 1）。 */
export interface ArbConfigSetBody {
  version: 1
  legAVenue: string
  legAAccount: string
  legBVenue: string
  legBAccount: string
  pairs: string[]
}

/** `arb.config.get` / `arb.config.set` 的回执（configured 分支预填与回显用）。 */
export interface ArbConfigDoc {
  version: number
  configured: boolean
  legAVenue: string | null
  legAAccount: string | null
  legBVenue: string | null
  legBAccount: string | null
  pairs: string[]
  error?: string
}

// ── 显示账户（issue 559）───────────────────────────────────────────────────
// 身份唯一源改造：行情页倒计时卡的「这是谁的市场」不再从账户绑定派生，而
// 是内核的 display_account 独立事实（accounts.toml 顶层持久化 + 运行时生
// 效）。写动词 `display.account.set`（payload {accountId}）——内核回执里
// 的 displayVenue 是该显示账户绑定 venue 的读数（空串 = 未绑定，可交易任
// 意交易所），与本面板会话默认账户彻底解耦：切换器选新账户 ≠ 行情身份
// 跟着跳。老内核没有该事实 → 快照缺字段，页面回落账户绑定链（现行为）。

/** `display.account.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface DisplayAccountSetDoc {
  /** 内核收下的显示账户 id。 */
  accountId: string
  /** 该显示账户绑定的 venue 读数（空串 = 未绑定）。 */
  displayVenue: string
  persisted: boolean
  atMs: number
  note: string
  error?: string
}

// ── 多账户扇出（account.trading.set / account.strategies.set）───────────────
// 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。

/** `account.trading.set` 回执：关 = 拒开仓、平仓照放；persist 失败内核整体拒绝。 */
export interface AccountTradingSetDoc {
  accountId: string
  enabled: boolean
  persisted: boolean
  atMs: number
  reason?: string
  note: string
  error?: string
}

/**
 * `account.strategies.set` 回执。keptExitsOnly = 该账户仍持仓但被移出分配
 * 的策略 —— 内核以 exits-only 保活到平完，安全网永不孤儿化持仓。
 */
export interface AccountStrategiesSetDoc {
  accountId: string
  strategies: string[]
  keptExitsOnly: string[]
  persisted: boolean
  atMs: number
  reason?: string
  note: string
  error?: string
}

// ── 账户簿运行时增删（account.create / account.remove，#516 内核面）─────────

/** `account.create` 回执。 */
export interface AccountCreateDoc {
  id: string
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

/** `account.remove` 回执。 */
export interface AccountRemoveDoc {
  id: string
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

// ── 轮次周期集（account.rounds.set，轮次多族 B2）─────────────────────────────
// 账户的轮长族声明写动词：params `{accountId, rounds, reason?}`（camelCase
// wire，rounds = 白名单秒数数组，空数组 = 不限制）。移除有敞口的族会被内核
// 整体拒绝（错误消息点名族列表）—— 面板不预判敞口，拒绝原文透出。老内核
// 不认识该动词 → JSON-RPC unknown-method 错误原样返回，卡面据此降级只读。

/** `account.rounds.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface AccountRoundsSetResult {
  accountId: string
  /** 内核收下的族集（秒）；空数组 = 未声明限制（许可轴空集放行）。 */
  rounds: number[]
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  runtimeEnabled?: boolean
  error?: string
}

export const accountsApi = {
  /**
   * 钱包现金流水卡的读路径（`account.journal`，#511，只读）：一个账户的
   * append-only 现金流水最近 limit 行（升序）。accountId 缺省 = 会话默认
   * 账户（§9.5）；limit 缺省 100、内核 clamp 1..=500。truncated = true 时
   * 内存窗口比返回的多，面板必须把「这是窗口不是全史」说出来。
   */
  accountJournal: (params?: { accountId?: string; limit?: number }) =>
    request<AccountJournalDoc>('/account-journal', {}, {
      accountId: params?.accountId,
      limit: params?.limit,
    }),
  // ── 账户级聚合读数（PR H 账户筛选修复）─────────────────────────────────────
  // 专用代理路由（同 account-journal 的纪律：JSON-RPC 信封不走 /command
  // 文本动词通道）。accountId 缺省 = 全局口径（与旧面板逐字节一致）。

  /**
   * 闭环统计（`trades.summary` 薄代理）。带 accountId 时内核按行过滤
   * 重算该账户的 summary；缺席 = 全局持久 headline。
   */
  tradesSummary: (params?: { accountId?: string }) =>
    request<{ summary?: CumulativeTradeSummary | null; error?: string }>(
      '/trades-summary', {}, { accountId: params?.accountId },
    ),
  /**
   * 引擎统计（`engine.stats` 薄代理）。带 accountId 时内核把
   * `strategies` 行切到该账户（strategy_stats_for_account，同形状）；
   * 缺席 = 全局 fold（每行带 `accounts` 数组）。
   */
  engineStatsFor: (params?: { accountId?: string }) =>
    request<EngineStats>('/strategy-stats', {}, { accountId: params?.accountId }),
  // ── 账户↔交易所绑定（account.venue.get / account.venue.set）────────────────
  // 专用代理路由（#508 的教训：JSON-RPC 信封走 /command 文本动词通道会被
  // 网关当 unknown command 拒掉 —— 绑定面必须走自己的 proxy 路由，同
  // account-journal 的纪律）。

  /** 绑定读数（只读）：accountId 缺省 = 全簿。venue 空串 = 未绑定。 */
  accountVenue: (params?: { accountId?: string }) =>
    request<AccountVenueDoc>('/account-venue', {}, {
      accountId: params?.accountId,
    }),
  /** 绑定写：venue 空串 = 解绑。内核校验插件名并持久化进 accounts.toml。 */
  accountVenueSet: (body: { accountId: string; venue: string; reason?: string }) =>
    request<AccountVenueSetDoc>('/account-venue/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 跨所套利创建（arb.config.set / arb.config.get，合同 v1）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 内核侧校验失败 = JSON-RPC APPLICATION 错误，网关包成 200 + `{error}` 文档
  // —— 调用方必须检查 `error` 并把消息原文转述给用户（消息点名非法字段）。
  // get 返回 configured=false = 老内核（不认识该动词）与「未配置」两种情况的
  // 区分在 feature-detect 层做（get 的 error 带 rpc -32601 字样 = 动词不存在）。

  /** 已配置双腿的读数（弹窗预填：configured=true 时按腿回显）。 */
  arbConfigGet: () => request<ArbConfigDoc>('/arb-config'),
  /** 双腿配置写：先于 setStrategy 调用（合同提交序：config 成功才启用）。 */
  arbConfigSet: (body: ArbConfigSetBody) =>
    request<ArbConfigDoc>('/arb-config/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 显示账户（issue 559）─────────────────────────────────────────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 写的是「行情页显示谁的市场」——与 `account <id>`（本面板会话默认账户）
  // 是两个独立事实：切换器成功路径两者都写，本动词 fire-and-forget，失败
  // 静默（快照下一拍带回新 displayVenue，页面回落链兜底）。
  /** 显示账户写：内核校验账户存在并持久化进 accounts.toml 顶层 display_account。 */
  displayAccountSet: (body: { accountId: string; reason?: string }) =>
    request<DisplayAccountSetDoc>('/display-account/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 多账户扇出（account.trading.set / account.strategies.set）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。

  /** 交易开关写：关 = 该账户拒开仓、平仓照放（内核 Gate 2 裁决并持久化）。 */
  accountTradingSet: (body: { accountId: string; enabled: boolean; reason?: string }) =>
    request<AccountTradingSetDoc>('/account-trading/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 策略分配写：空列表 = 继承全部全局启用；非空 = 精确钉住。返回的
   * keptExitsOnly 是被移出但仍持仓的策略 —— 内核保活到平完。
   */
  accountStrategiesSet: (body: { accountId: string; strategies: string[]; reason?: string }) =>
    request<AccountStrategiesSetDoc>('/account-strategies/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 账户簿运行时增删（#517：添加账户弹窗 / 账户删除，只删不改）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 凭据不走这两个动词 —— 另行 market.credentials.set（值只下行、从不回显）。

  /** 建户：id 非空且不与现有重复、default 保留名被内核拒绝；venue 缺省 = 未绑定。 */
  accountCreate: (body: { id: string; name?: string; venue?: string; reason?: string }) =>
    request<AccountCreateDoc>('/account-create', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /** 删户：default 与有持仓的账户内核拒绝；删的是当前默认时内核回切。 */
  accountRemove: (body: { id: string; reason?: string }) =>
    request<AccountRemoveDoc>('/account-remove', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 轮次周期集写（`account.rounds.set`，轮次多族 B2）：body 即 params 对象
   * （`{accountId, rounds, reason?}`），逐字转发（同 account-trading/set 的
   * 纪律）。rounds = 白名单秒数数组（空数组 = 不限制）；移除有敞口的族被
   * 内核整体拒绝 —— APPLICATION 错误原文随 `{error}` 回来，调用方必须把消
   * 息转述给用户。老内核不认识该动词 → error 带 JSON-RPC unknown-method 字
   * 样，卡面 feature-detect 降级为只读提示。
   */
  accountRoundsSet: (body: { accountId: string; rounds: number[]; reason?: string }) =>
    request<AccountRoundsSetResult>('/account-rounds/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
