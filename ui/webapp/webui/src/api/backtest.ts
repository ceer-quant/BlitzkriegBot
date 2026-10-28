/**
 * Backtest domain — the #353 in-browser backtest pipeline's IPC proxies
 * (mirror core/blitzkrieg_core/src/backtest_jobs.rs wire shape).
 */
import { request } from './core.ts'
import type { BacktestReport } from '../backtest'

// ── #353 回测类型（mirror core/blitzkrieg_core/src/backtest_jobs.rs 的 wire 形）──

/** `backtest.onchain.pull` 参数。钱包/时间窗必填；资产过滤可空 = 全量。 */
export interface OnchainPullParams {
  wallet: string
  /** `YYYY-MM-DD`（UTC，含尾日）或 epoch 秒。 */
  start: string
  end: string
  /** 空 = 整个钱包的交易。 */
  assets: string[]
  market?: string | null
}

/** `backtest.run` 参数。strategy 只传名字 — 任意脚本/代码在内核被拒。 */
export interface BacktestRunParams {
  /** 数据集路径（来自列表/拉取结果，限数据集根内）。 */
  archive: string
  /** `mine`（默认）| `verify` | `sweep` — #351 延迟阶梯。 */
  mode?: 'mine' | 'verify' | 'sweep' | string
  verifyLatencyMs?: number
  /** 诚实摩擦下限：≥1 tick，0/负数被拒。 */
  slippageTicks?: number
  tickMs?: number
  tailMs?: number
  strategies?: string[]
}

/** 一条数据集（`backtest.onchain.list` 的元素）。 */
export interface BacktestDataset {
  dataset: string
  eventsPath: string
  manifest: {
    wallet?: string
    window?: { startMs?: number; endMs?: number; startSec?: number; endSec?: number }
    counts?: { trades?: number; events?: number; conditions?: number; skippedRows?: number }
    files?: { trades?: string; events?: string }
    sha256?: { trades?: string; events?: string }
    generatedAtMs?: number
    [extra: string]: unknown
  }
}

/** `backtest.onchain.list` 应答。 */
export interface BacktestDatasetListDoc {
  outDir: string
  datasets: BacktestDataset[]
  error?: string
}

/** `backtest.onchain.pull` 应答：每资产一个任务。 */
export interface BacktestJobIdsDoc {
  jobIds: string[]
}

/** 任务状态（`backtest.status`）。done 时附带全量 `result`。 */
export interface BacktestJobStatusDoc {
  jobId: string
  /** `pull` | `backtest` */
  kind: string
  /** `running` | `done` | `failed` */
  state: string
  /** pull 的链上阶段（trades/markets/prices/convert/done）或 replay。 */
  phase: string
  detail: string
  progress: number
  startedAtMs: number
  finishedAtMs: number | null
  error: string | null
  result?: BacktestResultDoc
}

/** 阶梯一行（`backtest.result.ladder` 的元素）。 */
export interface BacktestLadderRow {
  latencyMs: number
  netPnlUsd: number
  closed: number
  winRatePct: number
  profitFactor: number | null
}

/** `backtest.result`：报告 + 阶梯 + 判读。`report` 即 #351 的 BacktestReport。 */
export interface BacktestResultDoc {
  mode: string
  archive: string
  verdict: string
  ladder: BacktestLadderRow[]
  report: BacktestReport
  reportPath: string
}

/** `backtest.export`：浏览器保存所需的文件名 + 内容。 */
export interface BacktestExportDoc {
  fileName: string
  content: string
}

export const backtestApi = {
  // ── #353 回测（拉数据 → 配置 → 回测 → 看结果，全程浏览器内）────────────
  // 六条都是内核 `backtest.*` IPC 的纯代理：网关不加工任何参数（body 即
  // params，逐字转发），内核的 JSON-RPC 错误在网关侧包成 200 + `{error}`
  // 文档 — 所以每个调用方都要检查 `error` 字段，原样抛给页面展示。

  /** 数据集列表（`backtest.onchain.list`）：第一步「拉数据」的选择器。 */
  backtestOnchainList: () => request<BacktestDatasetListDoc>('/backtest/onchain-list'),
  /**
   * 拉数据（`backtest.onchain.pull`）：钱包 + 时间窗 + 可选资产过滤。
   * 空 `assets` = 整个钱包；命名资产各起一个可断点续拉的任务。
   */
  backtestOnchainPull: (params: OnchainPullParams) =>
    request<BacktestJobIdsDoc>('/backtest/onchain-pull', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 发起回测（`backtest.run`）：数据集 + #351 模式档位。策略只传名字。 */
  backtestRun: (params: BacktestRunParams) =>
    request<{ jobId: string }>('/backtest/run', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 轮询任务状态（`backtest.status`）：~1s 一次直到 done/failed。 */
  backtestStatus: (id: string) =>
    request<BacktestJobStatusDoc>(`/backtest/status?id=${encodeURIComponent(id)}`),
  /** 已完成任务的全量结果（`backtest.result`）。 */
  backtestResult: (id: string) =>
    request<BacktestResultDoc>(`/backtest/result?id=${encodeURIComponent(id)}`),
  /** 导出信封（`backtest.export`）：文件由浏览器自己保存（`fileName`+`content`）。 */
  backtestExport: (id: string) =>
    request<BacktestExportDoc>(`/backtest/export?id=${encodeURIComponent(id)}`),
}
