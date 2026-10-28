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
  winRate: number
  avgHold: number | undefined
  best: number | undefined
  worst: number | undefined
  fromSummary: boolean
}
