/**
 * Version domain — the VERSIONING.md update surface (check/configure/stage)
 * behind the 设置 page's update card.
 */
import { request } from './core.ts'

export const versionApi = {
  /**
   * 设置页「检查更新」按钮（VERSIONING.md §6.3）。网关只负责把请求转给内核并
   * 立即返回 —— 内核拨 GitHub 以秒计，网关的单线程接受循环绝不等它。轮询
   * snapshot 的 `systemVersion` 直到 `lastCheckMs` 变化；`error` 非空时是内核
   * 明确拒绝（例如 checkEnabled=false），必须原样展示，不许静默。
   */
  updateCheck: () => request<{ started: boolean; running?: boolean; error?: string }>('/version/check', { method: 'POST' }),
  /**
   * 设置页「自动更新」/「更新检查」开关（VERSIONING.md §7.4，#393）。内核是
   * 开关的唯一事实来源：它落盘 + 落审计后回显两个值；写盘失败必须报错
   * （静默回退的开关比没有更糟）。body 里缺席的键保持内核现值不动。
   */
  updateConfigure: (body: { autoUpdate?: boolean; checkEnabled?: boolean }) =>
    request<{ ok: boolean; autoUpdate?: boolean; checkEnabled?: boolean; error?: string }>(
      '/version/configure',
      {
        method: 'POST',
        body: JSON.stringify(body),
      },
    ),
  /**
   * 设置页「暂存下载」按钮（VERSIONING.md §7.5，#379）。让内核把较新的
   * 发布资产下载到 data/update/staging/ 并校验 SHA256 —— 校验过也只停在
   * 「重启可应用」，替换二进制永远是启动器的职责，内核不碰自己正在执行的
   * 文件。网关同样只转发、立即返回；轮询 snapshot 的 `stageState` 直到
   * phase 变化；`error` 非空是内核拒绝（autoUpdate=false），原样展示。
   */
  updateStage: () => request<{ started: boolean; running?: boolean; error?: string }>('/version/stage', { method: 'POST' }),
}
