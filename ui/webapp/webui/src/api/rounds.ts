/**
 * Round-duration domain — the round clock coordinate system's read/write
 * surface (mirror core schema RoundDurationResult / runtime cell).
 */
import { request } from './core.ts'

// ── 轮次时间周期设置卡（mirror core schema RoundDurationResult / runtime cell）──

/** 白名单一行（内核 RoundDurationOption 的 wire 形状；label 缺省 = 非标档位）。 */
export interface RoundDurationOption {
  sec: number
  label?: string
}

/** `round_duration.get` 回执：本会话生效值 + 来源 + 白名单 + 下次会话将解析到什么。 */
export interface RoundDurationDoc {
  effectiveSec: number
  /** cli_or_env = 显式旗标/环境变量在管（本卡写入不会赢）；default/runtime/toml/env。 */
  source: string
  allowed: RoundDurationOption[]
  nextSessionSec: number
  persisted: boolean
  error?: string
}

/** `round_duration.set` 请求体：`durationSec`（白名单档位）或 `reset`（清格）。 */
export interface RoundDurationSetBody {
  durationSec?: number
  reset?: boolean
  reason?: string
}

/** `round_duration.set` 回执：下次会话将跑的值 + 是否为清格操作。 */
export interface RoundDurationUpdateDoc {
  nextSessionSec: number
  reset: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

export const roundsApi = {
  /**
   * 轮次时间周期设置卡的读路径（`round_duration.get`，只读）：本会话生效
   * 秒数、来源、可切换白名单、下次会话将解析到什么（持久格 outranks TOML/
   * 默认，但显式 `--round-sec` outranks 持久格 —— source 字段如实报告）。
   */
  roundDuration: () => request<RoundDurationDoc>('/round-duration'),
  /**
   * 轮次时间周期的写路径（`round_duration.set`）：next_session 语义 ——
   * 轮次周期是网格坐标系（round_slot 是 UTC 网格索引），本会话网格不动，
   * 写持久格重启生效。`reset: true` 清格回落启动解析。
   */
  roundDurationSet: (body: RoundDurationSetBody) =>
    request<RoundDurationUpdateDoc>('/round-duration/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
