/**
 * 行情面板子组件共享的读数形状（issue 717 拆分）：页面持有全部状态与
 * 派生计算，组件按 prop 消费 —— 这里只声明跨组件复用的形状。
 */

/** cumStats（累计口径）— HftPage 同名 computed 的返回形状。 */
export interface CumStats {
  total: number
  gross: number
  fees: number
  net: number
  wins: number
  losses: number
  /**
   * 胜率（0–100，永远是数值）。**操作者裁决 2026-11-14：永不出现「未知」**——
   * 与总览页「已平仓交易」卡同口径：`wins / (wins + losses)`，0 笔 = 0.0%。
   * 未知盈亏的行不进这个分母，由 `unknownPnlCount` 单独披露。
   */
  winRate: number
  /**
   * netPnlUsd 读数缺失/非法的行数（未知盈亏）。这些行不进 wins/losses/
   * net/胜率分母 —— 单独计数、单独标注，绝不折叠成 0 混入。issue 1199 P0-4。
   */
  unknownPnlCount: number
  avgHold: number | undefined
  best: number | undefined
  worst: number | undefined
  fromSummary: boolean
}
