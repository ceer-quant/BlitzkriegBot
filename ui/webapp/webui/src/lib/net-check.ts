/**
 * 网络自检的读法：把内核的拨测报告翻成设置页能直接渲染的结论。
 *
 * 三件事必须由这块（而不是模板）决定，因为它们都是「不许撒谎」的规则：
 *
 *  1. **未知状态原样显示。** 内核将来多一个 `status` 时，页面必须把它照抄出来，
 *     不能落到空字符串或某个看起来更顺眼的词上 — 操作员看不到的那个状态，就是
 *     他无法处理的那个状态。Rust 侧 `ui_kit::core::net_check::status_label`
 *     是同一条规则的另一份实现（CLI/TUI 用它），两边都只做「已知值打标签、
 *     未知值回显」。
 *
 *  2. **`unsupported` / `rejected` 不是网络故障。** 前者是「本构建没有这条探针」，
 *     后者是「按策略在拨号前就拒绝了」。把它们读成「交易所挂了」，会把人派去找
 *     错的问题。
 *
 *  3. **空报告与「探测中」都不是通过。** `0/0` 只是没有证据；`probing` 为真时
 *     显示的是「上一次结果 + 正在重测」，绝不拿旧结果冒充新结果。
 *
 * 纯函数、无 Vue：`scripts/net-check.check.mjs` 直接驱动它们（同 `lib/safety.ts`
 * 与 `npm run check:safety` 的模式）。
 */
import type { NetCheckDoc, NetCheckItem, NetCheckReport } from '@/api/client'

/** 一条探测的状态词 → 中文标签。未知值原样返回。 */
export function netStatusLabel(status: string): string {
  switch (status) {
    case 'ok': return '通过'
    case 'dns_failed': return 'DNS 解析失败'
    case 'tcp_refused': return 'TCP 被拒'
    case 'tcp_timeout': return 'TCP 超时'
    case 'tls_cert': return 'TLS 证书被拒'
    case 'tls_error': return 'TLS 握手失败'
    case 'timeout': return '超时'
    case 'http_error': return 'HTTP 错误'
    case 'transport_error': return '传输错误'
    case 'rejected': return '被策略拒绝（未拨号）'
    case 'unsupported': return '无探针（本构建未实现）'
    default: return status
  }
}

/** 整份报告的判读码 → 短标题。未知码回落到内核自己的一句话。 */
export function netHintTitle(code: string | undefined): string {
  switch (code) {
    case 'ok': return '每条路径都通'
    case 'tls_blocked': return 'TLS 在请求前被拦'
    case 'dns_failed': return '域名解析失败'
    case 'proxy_env': return '本进程配了代理'
    case 'fake_ip': return '解析器返回了 fake-IP'
    case 'partial': return '部分路径失败'
    case 'unsupported': return '本构建没有网络探针'
    default: return '网络自检'
  }
}

/** 一行渲染数据（字段已定型，模板不再做判断）。 */
export interface NetCheckRow {
  name: string
  target: string
  ok: boolean
  /** 中文标签或内核原样回显的未知状态。 */
  label: string
  ms: number | null
  fakeIp: boolean
  detail: string
}

export interface NetCheckView {
  /**
   * `up` 全通过 / `down` 有失败（含 `unsupported`、空报告）/ `default` 尚无结论。
   */
  tone: 'up' | 'down' | 'default'
  title: string
  /** 计数行，例如「2/3 条路径通过」。 */
  summary: string
  /** 内核的判读；缺省时给一句可操作的话。 */
  hint: string
  rows: NetCheckRow[]
  /** 代理变量名（永不含值）；没有则为 null。 */
  proxyNote: string | null
  /** 结果年龄的说明（「刚刚」「42 秒前」），无结果为 null。 */
  ageNote: string | null
  /** 无结果可渲染时给读者的下一步。 */
  emptyHint: string | null
}

/** 毫秒 → 相对时间。刻意不做「X 分钟前」以外的精度：这是一份缓存结果。 */
export function ageLabel(ms: number | null | undefined): string | null {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return null
  const s = Math.round(ms / 1000)
  if (s < 5) return '刚刚'
  if (s < 60) return `${s} 秒前`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} 分钟前`
  return `${Math.floor(m / 60)} 小时前`
}

function toRow(item: NetCheckItem): NetCheckRow {
  return {
    name: item.name,
    target: item.target ?? '',
    ok: item.ok === true,
    label: netStatusLabel(item.status ?? ''),
    ms: typeof item.ms === 'number' ? item.ms : null,
    fakeIp: item.fakeIp === true,
    detail: item.detail ?? '',
  }
}

/**
 * 报告 → 视图。`doc` 可能只有 `probing`（第一次探测还没回来），也可能是
 * `report: null` + `error`（内核没应答）。
 */
export function readNetCheck(doc: NetCheckDoc | null | undefined): NetCheckView {
  const report: NetCheckReport | null = doc?.report ?? null
  const rows = (report?.items ?? []).map(toRow)

  if (!report) {
    // 没有报告就没有结论：探测中要说「正在测」，失败要说失败原因，其余情况
    // 说明还没有人测过 —— 三种都不许渲染成空表（空表看起来像「没问题」）。
    if (doc?.probing) {
      return {
        tone: 'default',
        title: '正在探测…',
        summary: '尚无结果',
        hint: '正在拨测内核出网用到的每条路径，通常几秒内返回。',
        rows: [],
        proxyNote: null,
        ageNote: null,
        emptyHint: '页面会在探测完成后自动刷新。',
      }
    }
    return {
      tone: 'down',
      title: '没有拿到报告',
      summary: '无结果',
      hint: doc?.error
        ? `内核未返回网络自检结果：${doc.error}`
        : '还没有对内核发起过网络自检。',
      rows: [],
      proxyNote: null,
      ageNote: null,
      emptyHint: doc?.error
        ? '确认内核在运行（本页上方指令台可查询），然后点「重新探测」。'
        : '点「开始探测」发起一次；内核离线时先把它启动起来。',
    }
  }

  const passed = rows.filter((r) => r.ok).length
  const proxyNote = report.proxyEnv?.length
    ? `本进程设置了代理变量：${report.proxyEnv.join('、')}（只报名字，值可能含凭证）`
    : null

  // `ok` 由内核判定（空报告与 unsupported 都是 false）；这里只用它决定语气，
  // 不用它决定是否显示失败行 — 那些行照样逐条列出来。
  const empty = rows.length === 0
  return {
    tone: report.ok ? 'up' : 'down',
    title: netHintTitle(report.hintCode),
    summary: empty ? '0/0 条路径（没有探测结果，不算通过）' : `${passed}/${rows.length} 条路径通过`,
    hint: report.hint?.trim()
      ? report.hint
      : (empty
        ? '内核没有上报任何探测项 — 这不代表网络正常。'
        : '内核未给出判读，按失败行的状态词处理。'),
    rows,
    proxyNote,
    ageNote: ageLabel(doc?.ageMs),
    emptyHint: empty ? '确认内核的市场插件实现了网络探针（`netcheck` 也可在 TUI 里查看）。' : null,
  }
}
