/**
 * Strategy domain — the tunables editor card (issue 481; mirror core schema
 * StrategyParamsReadout), the strategy-stats row shape, and the
 * strategy/command verbs (enable toggle, flatten, evolution switches).
 */
import { request } from './core.ts'
import type { CommandDoc } from './audit.ts'
import type { EngineStats } from './snapshot.ts'

// ── 策略 tunables 编辑卡（issue 481；mirror core schema StrategyParamsReadout）──

/** 一个声明过的 tunable：名称 + 声明域 [min, max] + 当前生效值（十进制字符串）。 */
export interface TunableViewDoc {
  name: string
  /** 当前生效值（注册表 cell 或 manifest 默认），十进制字符串。 */
  value: string
  min: string
  max: string
}

/** `strategy.params.get` 回执：一个策略的声明旋钮与生效值。 */
export interface StrategyParamsReadoutDoc {
  name: string
  found: boolean
  /** 空数组 = 该策略没有可调参数（未声明 tunables 域）。 */
  tunables: TunableViewDoc[]
  evolvable: boolean
  /** 注册表覆盖层是否挂载（写入后为 true）。 */
  paramsAttached: boolean
  note?: string
}

/**
 * `strategy.params.set` 请求体：一个策略 + 要改的旋钮（十进制字符串）+
 * 审计备注。多旋钮补丁是原子的：一个非法值整体拒。
 */
export interface StrategyParamsSetBody {
  name: string
  params: Record<string, string>
  reason?: string
}

/** 一个旋钮的审计变更（内核 StrategyParamChange 的 wire 形状）。 */
export interface StrategyParamChangeDoc {
  knob: string
  from: string
  to: string
  /** live：策略下一次 evaluate 即读到新值（cell 合并到 manifest 默认包上）。 */
  effect: 'live' | string
}

/** `strategy.params.set` 回执：改了什么、谁改的、何时、未持久化。 */
export interface StrategyParamsSetUpdateDoc {
  name: string
  applied: StrategyParamChangeDoc[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
  note: string
  error?: string
}

export const strategyApi = {
  /**
   * 策略 tunables 编辑卡的读路径（`strategy.params.get`，issue 481，只读）：
   * 一个策略声明的全部可调参数（名称 + 声明域 [min, max]）与当前生效值。
   * schema 源是策略自己的 manifest tunables —— 内核不发明域。
   */
  strategyParams: (name: string) =>
    request<StrategyParamsReadoutDoc>(
      `/strategy-params?name=${encodeURIComponent(name)}`,
    ),
  /**
   * 策略 tunables 编辑卡的写路径（`strategy.params.set`，issue 481）：
   * `risk.setExit` 同款姿态 —— 内核对每个旋钮按声明域校验（plan-first，
   * 一个非法整体拒），热写注册表 cell，回执逐项 old→new 带 effect=live
   * （策略下一次 evaluate 即读到新值）。未持久化：重启回落 manifest 默认
   * （+ 已采纳的进化晋升）。
   */
  strategyParamsSet: (body: StrategyParamsSetBody) =>
    request<StrategyParamsSetUpdateDoc>('/strategy-params/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  /**
   * Strategy enable toggle (`strategy.enable` on the core). `accountId` 缺省
   * = default（全局开关分支，与旧面板逐字节一致）；非 default 账户走内核的
   * assignment=enablement 分支（账户级启停 —— 启停绑定在「账户+交易所」级）。
   * 回执带 `accountId` 回显（合同字段）。
   */
  setStrategy: (name: string, enabled: boolean, accountId?: string) => {
    // 文本动词通道不认 JSON 信封的 params —— accountId 走专用 JSON-RPC 代理
    // 路由（body 即 params 逐字转发，同 account-venue/set 的纪律）；缺省时
    // 走原 /api/command 文本动词（老面板路径不变）。
    if (accountId === undefined) {
      return request<CommandDoc>('/command', {
        method: 'POST',
        body: `strategy ${name} ${enabled ? 'on' : 'off'}`,
      })
    }
    return request<CommandDoc>('/strategy-enable', {
      method: 'POST',
      body: JSON.stringify({ name, enabled, accountId }),
    })
  },
  /** Manual operator flatten for one stable position id. */
  flatten: (positionId: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: `flatten ${positionId}` }),
  /** E13: decide one evolution proposal (accept hot-swaps, reject/defer don't). */
  decideEvolution: (id: string, decision: 'accept' | 'reject' | 'defer') =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `decide ${id} ${decision}`,
    }),
  /** E13: the unattended-evolution switch (`auto-evolve on|off`). */
  setAutoEvolve: (on: boolean) =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `auto-evolve ${on ? 'on' : 'off'}`,
    }),
  /** #249: the evolution ENGINE switch (`evolve on|off`) — whether anything evolves. */
  setEvolve: (on: boolean) =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `evolve ${on ? 'on' : 'off'}`,
    }),
  /** E13: undo the last accepted promotion of one strategy (一键回滚). */
  rollbackStrategy: (strategy: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: `rollback ${strategy}` }),
}
