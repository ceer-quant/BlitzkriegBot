/**
 * The panel's trading-safety banners — what they read, and what their titles may
 * claim.
 *
 * These three banners are the operator's only cockpit-level signal that trading
 * is down: the kill switch is frozen, the last thing the kernel did was fail, or
 * the trading-capability self-check came back red. #236 is the issue that kept
 * them dark: `EngineStats` declared the fields, the kernel sent them, and the UI
 * Kit's view type dropped them on the way through — so every one of these
 * predicates read `undefined` forever and the cockpit stayed calm through a
 * freeze. The data now reaches `snapshot.stats`; this module owns the reading.
 *
 * Two titles were also lying about their source (#232 and the same defect next
 * door), and that is why the decisions live here rather than inline in the
 * template:
 *
 *  - the trading-error banner said "（venue）", but the slot it reads carries a
 *    venue refusal, a safety-net failure AND the kernel's own leg refusals. It
 *    now names no source, and puts the kernel's `code` on screen as DATA — the
 *    reader can tell which of the three it was without the title guessing.
 *  - the connection banner said "引擎最近错误", but `snapshot.lastError` is the
 *    GATEWAY's own failure to reach the core ("core not reachable on <socket>",
 *    `ui/ui_kit/src/gateway/command.rs`). An operator sent to the engine log by
 *    that title is sent to the wrong first place; it now says what it means.
 *
 * Pure functions, no Vue: `scripts/safety-banners.check.mjs` drives them
 * directly (same pattern as `lib/rejections.ts` + `check:rejections`).
 */
import type { EngineStats, Snapshot } from '@/api/client'

export interface SafetyBanner {
  tone: 'error' | 'warn'
  /**
   * Severity, not color alone: `urgent` banners are the conditions under which
   * trading is DOWN right now (freeze, failed self-check) — red, persistent,
   * no auto-dismiss, no ✕ (the operator must act or fix the condition; hiding
   * the only red on the cockpit hides the outage). Everything else is
   * `attention`: amber, dismissible, and it fades on its own — a transient
   * refusal record must not squat on the cockpit until the next error
   * overwrites it (2026-10-28 operator call: 红色只留给紧急，其余黄色 +
   * 自动消失 + X).
   */
  urgent: boolean
  /**
   * Stable key for dismiss-memory on non-urgent banners (`error:INVALID_PARAMS`
   …
   * the same failure re-raises; a DIFFERENT error gets a fresh banner). Urgent
   * banners ignore it — they cannot be dismissed away.
   */
  identity?: string
  title: string
  body: string
  hint?: string
  /**
   * The kernel's `CoreErrorCode` for the refusal, when the core is new enough to
   * send the structured record. Rendered as a tag beside the message.
   */
  code?: string
  /** For the self-check banner: the probes that failed, `name：detail`. */
  items?: { name: string; detail: string }[]
}

/**
 * The panel cannot talk to the kernel at all.
 *
 * `snapshot.lastError` is the UI Kit's / gateway's own string — "core not
 * reachable on <socket>" — never a kernel-side error. `stats.lastError` is the
 * kernel's. Confusing the two is the defect this title had.
 */
export function connectionBanner(snap: Snapshot | null | undefined): SafetyBanner | null {
  const err = snap?.lastError
  if (!err) return null
  return {
    tone: 'warn',
    urgent: false,
    identity: 'connection',
    title: '面板无法连接内核',
    body: err,
    hint: '先确认 blitzkrieg-core 进程在运行（进程页可启停），再刷新；这一条来自网关，不是内核的交易错误。',
  }
}

/**
 * Trading is frozen (kill switch): the one condition under which the panel must
 * never look normal. Reads `stats.tradingFrozen`, which only exists because the
 * kernel's freeze state survives the view type now.
 */
export function freezeBanner(stats: EngineStats | null | undefined): SafetyBanner | null {
  const frozen = stats?.tradingFrozen
  if (!frozen?.active) return null
  return {
    tone: 'error',
    urgent: true,
    title: '交易已冻结（kill switch）',
    body: frozen.reason || '交易已被冻结，原因未上报。',
    hint: '冻结期间新开仓被拒绝，平仓不受影响。核对原因后在进程页重启内核，或按风控流程解除。',
  }
}

/**
 * The kernel's last recorded error — the ONE slot every internal refusal path
 * writes (`Core::note_error`).
 *
 * Prefers the structured `lastError` (`{tsMs, code, message}`) and falls back to
 * the legacy `lastVenueError` (`{tsMs, message}`, already rendered as
 * `<CODE>: <message>`) so a panel running against a core that predates the
 * structured key still shows something. Suppressed while frozen: the freeze
 * banner already carries the reason, and the freeze is the thing to act on.
 */
export function lastErrorBanner(
  stats: EngineStats | null | undefined,
  frozen = false,
): SafetyBanner | null {
  if (frozen) return null
  const structured = stats?.lastError
  if (structured) {
    return {
      tone: 'warn',
      urgent: false,
      identity: `error:${structured.code}:${structured.message}`,
      title: '最近交易错误',
      body: structured.message,
      code: structured.code,
      hint: '来源不限于交易所：也可能是安全网（对账/自检）或内核自身拒绝了一条腿；code 与拒单返回的 data.coreCode 同表。',
    }
  }
  const legacy = stats?.lastVenueError
  if (!legacy) return null
  // No `code` field: an older core pre-rendered the code into this string.
  return {
    tone: 'warn',
    urgent: false,
    identity: `error:legacy:${legacy.message}`,
    title: '最近交易错误',
    body: legacy.message,
    hint: '旧内核的兼容字段（lastVenueError），内容形如「代码: 原因」。',
  }
}

/**
 * The newest trading-capability self-check report, when it failed. A failed
 * check freezes trading, so this is the second red banner an operator has to
 * see — and the reason it carries the failing probes verbatim.
 */
export function selfCheckBanner(stats: EngineStats | null | undefined): SafetyBanner | null {
  const report = stats?.selfCheck
  if (!report || report.ok) return null
  const failed = (report.items ?? []).filter((i) => !i.ok)
  return {
    tone: 'error',
    urgent: true,
    title: '交易能力自检未通过',
    body: failed.length
      ? `${failed.length} 项探测失败`
      : '自检未通过，但内核未上报失败的探测项。',
    items: failed.map((i) => ({ name: i.name, detail: i.detail })),
  }
}

/**
 * The market-data archive stopped recording (issue 669 ②). The kernel's own
 * stop is a single journal warn line — an external operator found FIVE DAYS of
 * silently missing backtest corpus with a manual `df`. The kernel also emits
 * one risk alert on the stop edge; this banner is the CONTINUOUS face: it reads
 * `stats.archive` (recording/stoppedReason) every snapshot, so a panel opened
 * days after the stop still shows it. Older cores send `archive: null` —
 * absence renders nothing, never an invented outage.
 */
export function archiveStoppedBanner(stats: EngineStats | null | undefined): SafetyBanner | null {
  const archive = stats?.archive
  if (!archive || archive.recording !== false) return null
  const reason = archive.stoppedReason || 'unknown'
  return {
    tone: 'warn',
    urgent: false,
    identity: 'archive-stopped',
    title: '事件归档已停录',
    body: `事件归档已停录（${reason}）——回测语料不再记录`,
    hint: '行情事件不再落盘，回测/复盘语料从停录时刻起缺失。清理磁盘或调大归档上限后重启会话恢复录制。',
  }
}
