/**
 * 面板的信息架构 —— 导航是纯数据（零 Vue/组件依赖），App.vue 渲染它，
 * tui-parity / page-separation 门禁直接 import 它：两侧的面清单从同一份
 * 事实解析，不再各抄一份后悄悄漂移。
 *
 * 两级结构：直连页（总览 / 行情面板 / 插件 / 设置）+ 两个组
 * （回测研究、策略与演化）。组只是导航上的归拢，不是新功能面 —— 组内每个
 * 叶子仍是原页面，TUI 的每个面仍能在叶子上找到家（parity 由 check:parity
 * 断言）；插件与设置保持直连，因为它们是入口而不是产物。
 */

export type TabId =
  | 'overview'
  | 'hft'
  | 'backtest'
  | 'blueprint'
  | 'strategies'
  | 'decisions'
  | 'evolution'
  | 'plugins'
  | 'settings'

export type GroupId = 'research' | 'fleet'

export interface NavLeaf {
  kind: 'page'
  id: TabId
  label: string
}

export interface NavGroup {
  kind: 'group'
  id: GroupId
  label: string
  children: NavLeaf[]
}

export type NavEntry = NavLeaf | NavGroup

export const NAV: NavEntry[] = [
  { kind: 'page', id: 'overview', label: '总览' },
  { kind: 'page', id: 'hft', label: '行情面板' },
  {
    kind: 'group',
    id: 'research',
    label: '回测研究',
    children: [
      { kind: 'page', id: 'backtest', label: '回测' },
      { kind: 'page', id: 'blueprint', label: '蓝图' },
    ],
  },
  {
    kind: 'group',
    id: 'fleet',
    label: '策略与演化',
    children: [
      { kind: 'page', id: 'strategies', label: '策略' },
      { kind: 'page', id: 'decisions', label: '裁决流' },
      { kind: 'page', id: 'evolution', label: '进化' },
    ],
  },
  { kind: 'page', id: 'plugins', label: '插件' },
  { kind: 'page', id: 'settings', label: '设置' },
]

/** 全部叶子页（含组内），平铺 —— 门禁与页表共用。 */
export function navLeaves(): NavLeaf[] {
  return NAV.flatMap((e) => (e.kind === 'group' ? e.children : [e]))
}

/** 叶子所属的组；直连页返回 null。 */
export function groupOf(id: TabId): NavGroup | null {
  const hit = NAV.find((e) => e.kind === 'group' && e.children.some((c) => c.id === id))
  return hit && hit.kind === 'group' ? hit : null
}
