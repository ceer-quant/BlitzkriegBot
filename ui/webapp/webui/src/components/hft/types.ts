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
   * 胜率（0–100）。**null = 未知**（服务端 summary 缺 winRate 字段，或
   * 前端行折叠口径）—— 显示「未知」，绝不回落 0（0% 是「全败」的读数，
   * 不是「不知道」）。issue 1199 P0-4。
   */
  winRate: number | null
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
