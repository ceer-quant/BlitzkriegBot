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
  /** Name of the active market feed, or null when none is active. */
  activeMarket: string | null
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
  const marketDone = input.activeMarket !== null
  const strategyDone = input.anyStrategyEnabled
  return [
    {
      id: 'market', index: 0, done: marketDone, target: 'plugins', cta: '去插件页',
      title: '① 选定行情源',
      body: '行情插件提供盘口数据。到插件页确认一个行情源已设为活跃。',
    },
    {
      id: 'strategy', index: 1, done: strategyDone, target: 'strategies', cta: '去策略页',
      title: '② 启用一个策略',
      body: '策略默认停用。到策略页打开开关，机器人即按该策略开始出单。',
    },
    {
      id: 'wrap', index: 2, done: true, target: null, cta: null,
      title: '③ 完成收尾',
      body: '总览页盯真实余额与净利，行情面板看盘口与成交。',
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
