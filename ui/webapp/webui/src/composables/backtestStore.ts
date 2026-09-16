/**
 * 本地保存的回放报告 — localStorage 持久化，刷新不丢。
 * 报告 JSON 可能到几百 KB；超过 ~2MB 时放弃持久化（只在当前会话展示）。
 */
import type { BacktestReport } from '../backtest'

const KEY = 'blitzkrieg-panel-backtest'

export function loadBacktest(): BacktestReport | null {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return null
    return JSON.parse(raw) as BacktestReport
  } catch {
    localStorage.removeItem(KEY)
    return null
  }
}

export function saveBacktest(report: BacktestReport): void {
  try {
    const raw = JSON.stringify(report)
    if (raw.length <= 2 * 1024 * 1024) localStorage.setItem(KEY, raw)
  } catch {
    // quota exceeded — keep the in-memory view, skip persistence
  }
}

export function clearBacktest(): void {
  localStorage.removeItem(KEY)
}

export const backtestLabel = 'ECharts 回放复盘'
