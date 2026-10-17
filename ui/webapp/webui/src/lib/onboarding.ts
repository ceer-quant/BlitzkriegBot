/**
 * 首次运行三步引导 (E11) — a pure state machine over panel data, plus the
 * localStorage flag that retires the tour.
 *
 * The three steps are exactly what a brand-new operator must do before the
 * robot can trade: confirm an active market feed, enable a strategy, and know
 * where to watch. Steps complete themselves from live panel state — there is no
 * "next" button to click past — so the tour never asks a question the page can
 * already answer, and an operator who already did both real steps never sees it.
 *
 * Kept free of store/component imports so the gate (`check:first-run`) can run
 * this module directly under node, like `theme.ts` and `session.ts`.
 */

export interface OnboardInput {
  /** Any market plugin is registered (绑定即启用：行情按账户绑定自动派生). */
  anyMarketPlugin: boolean
  /** Whether any registered strategy is enabled. */
  anyStrategyEnabled: boolean
}

export interface OnboardStep {
  id: 'market' | 'strategy' | 'wrap'
  index: number
  title: string
  body: string
  /** Nav tab the CTA jumps to; null on the wrap step. */
  target: 'plugins' | 'strategies' | null
  cta: string | null
  done: boolean
}

/** The three fixed steps with live completion flags folded in. */
export function onboardSteps(input: OnboardInput): OnboardStep[] {
  const marketDone = input.anyMarketPlugin
  const strategyDone = input.anyStrategyEnabled
  return [
    {
      id: 'market', index: 0, done: marketDone, target: 'plugins', cta: '去插件页',
      title: '① 绑定账户交易所',
      body: '行情插件提供盘口数据（Polymarket / Kalshi / predict.fun 等；PolySimulator 是第三方模拟盘，只用于演练）。绑定即启用：在右上角账户切换器里给账户绑定交易所，对应行情插件即自动接入——没有全局「活跃源」开关。',
    },
    {
      id: 'strategy', index: 1, done: strategyDone, target: 'strategies', cta: '去策略页',
      title: '② 启用一个策略',
      body: '策略默认停用。到策略页打开开关（跨平台套利 cross_venue_arb 需至少两个场地的行情，先在插件页的「跨所配对」表确认映射已生成），机器人即按该策略开始出单；策略行展开后可直接调它的可调参数。',
    },
    {
      id: 'wrap', index: 2, done: true, target: null, cta: null,
      title: '③ 完成收尾',
      body: '总览页盯真实余额与净利，行情面板看盘口与成交，持仓/成交表的场地列标注每条腿来自哪个平台。',
    },
  ]
}

/** The step the tour shows: the first unfinished one, or null when all set. */
export function currentStep(steps: OnboardStep[]): OnboardStep | null {
  return steps.find((s) => !s.done) ?? null
}

/** localStorage key that retires the tour. */
export const DONE_KEY = 'blitzkrieg-onboard-done'

/** Whether the tour has been retired (completed or skipped). */
export function onboardDone(): boolean {
  try {
    return localStorage.getItem(DONE_KEY) === '1'
  } catch {
    return true // no storage (private mode): stay out of the way
  }
}

export function markOnboardDone(): void {
  try {
    localStorage.setItem(DONE_KEY, '1')
  } catch {
    // nothing to persist on top of; the tour just shows again next visit
  }
}
