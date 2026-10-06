import type { SystemVersion } from '@/api/client'

export interface VersionBadge {
  text: string
  variant: 'up' | 'down' | 'default'
}

/**
 * 三态徽章（VERSIONING.md §6.3）—— 全站唯一的实现。
 *
 * **null 不是 false**：把「尚未检查」画成「已是最新」是在撒谎。这条纪律与
 * 内核侧 net_check 的「未知状态原样回显」、以及 TUI render_settings 的
 * 「… not checked」是同一条规则；改任何一侧都必须保持三态可分辨。
 *
 * variant 语义与 Badge 既有用法一致：`up` 好 / `down` 坏 / `default` 中性。
 */
export function versionBadge(v: SystemVersion | null | undefined): VersionBadge | null {
  if (!v) return null
  if (v.updateAvailable === null) return { text: '未检查', variant: 'default' }
  if (v.updateAvailable === false) return { text: '已是最新', variant: 'up' }
  return { text: `可更新 ${v.latestVersion ?? ''}`.trim(), variant: 'down' }
}

/**
 * 修订号的措辞。`nogit`（构建无法指认自己的修订号，例如源码 tarball 构建）
 * 必须明说，不能渲染成一个看起来像 sha 的空串；TUI 的 render_settings 用
 * 同一个词（对等门禁 §6.1：`nogit` 与 dirty 的措辞两侧一致）。
 */
export function revisionText(gitHash: string): string {
  return gitHash === 'nogit' ? '无法指认修订号' : gitHash
}

/**
 * 「暂存下载」的依赖链提示（issue 393 ④）—— 与徽章同源的三态读法。
 *
 * 暂存有两道前置：autoUpdate ON，且检查得出了「可更新」结论。此前这条依赖链
 * 只活在按钮的 hover tooltip 里，灰按钮看起来像坏了而不是 INV-3 的承诺；
 * 提示文案进页面内联，三态分支（未检查 / 已最新 / 可更新）在这里逐个可分辨，
 * 页面不自己解三态 —— 同 versionBadge 的判据同源纪律。
 */
export function stageDependencyHint(v: SystemVersion | null | undefined): string {
  if (!v) return ''
  if (!v.autoUpdate) {
    return '暂存下载需要两步：先开「自动更新」，再「检查更新」得出可更新结论 —— 现在缺第一步。'
  }
  if (v.updateAvailable === null) {
    return '暂存下载需要「检查更新」先得出可更新结论 —— 现在还没有检查结论。'
  }
  if (v.updateAvailable === false) {
    return '暂存下载需要「检查更新」先得出可更新结论 —— 刚才的检查结论是已是最新。'
  }
  return ''
}
