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
 * Collapse volatile numbers inside an error message so the identity survives
 * them: the accounting-audit note embeds live figures (balances, drift, an
 * anchor ms-timestamp) that the kernel recomputes on every reconcile pass —
 * `error:CODE:msg 13.59… drift -7.89…` became `… 7.89…` one audit later and the
 * dismissed-identity match broke, resurrecting the banner the operator closed.
 * Digits are the only churn class observed in practice; wording changes SHOULD
 * re-raise. (2026-10-28 operator report: 警告还是关不掉.)
 */
const stableIdentity = (message: string): string => message.replace(/\d[\d.,]*/g, 'N')

/**
 * Error codes that are NORMAL market outcomes, not failures the operator must
 * act on. A fast-moving book routinely rejects a taker leg with
 * `WOULD_CROSS` (no counter-party at/inside the limit, or not enough depth
 * for the size) — surfacing every one as a cockpit warning is exactly the
 * "机器人太敏感" complaint: these are not errors, they are the market saying
 * "not right now". The refusal still lands in the order/trade log with its
 * full detail; only the persistent warning banner is suppressed.
 *
 * Add codes here whose meaning is "the market did not fill this leg" rather
 * than "something broke / you misconfigured something".
 */
const SUPPRESSED_BANNER_CODES = new Set([
  'WOULD_CROSS',
])

/**
 * The kernel's last recorded error — the ONE slot every internal refusal path
 * writes (`Core::note_error`).
 *
 * Prefers the structured `lastError` (`{tsMs, code, message}`) and falls back to
 * the legacy `lastVenueError` (`{tsMs, message}`, already rendered as
 * `<CODE>: <message>`) so a panel running against a core that predates the
 * structured key still shows something. Suppressed while frozen: the freeze
 * banner already carries the reason, and the freeze is the thing to act on.
 *
 * Market-normal refusals (`WOULD_CROSS` and friends) are NOT surfaced here —
 * they belong in the order log, not as a persistent cockpit warning.
 */
export function lastErrorBanner(
  stats: EngineStats | null | undefined,
  frozen = false,
): SafetyBanner | null {
  if (frozen) return null
  const structured = stats?.lastError
  if (structured) {
    if (SUPPRESSED_BANNER_CODES.has(structured.code)) return null
    // 任何拒绝记录（含 RISK_REJECTED：日损失限额、持仓上限等）都不再升
    // 为「账户冻结」：冻结横幅 + 解冻入口只来自
    // stats.accountGates[acct].blocked === true（accountFreezeBanners / accountFrozen）。
    // 瞬时拒单只走下面的普通警告横幅，不出冻结文案，不指路解冻。
    return {
      tone: 'warn',
      urgent: false,
      identity: `error:${structured.code}:${stableIdentity(structured.message)}`,
      title: '最近交易错误',
      body: structured.message,
      code: structured.code,
      hint: '来源不限于交易所：也可能是安全网（对账/自检）或内核自身拒绝了一条腿；code 与拒单返回的 data.coreCode 同表。',
    }
  }
  const legacy = stats?.lastVenueError
  if (!legacy) return null
  // No `code` field: an older core pre-rendered the code into this string as
  // `<CODE>: <message>`. Apply the same suppression so a legacy core does not
  // re-introduce the noisy banner.
  const legacyCode = legacy.message.match(/^([A-Z_]+):/)?.[1]
  if (legacyCode && SUPPRESSED_BANNER_CODES.has(legacyCode)) return null
  return {
    tone: 'warn',
    urgent: false,
    identity: `error:legacy:${stableIdentity(legacy.message)}`,
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
 * Per-account entry gates (#1059, red banner #2): the kernel freezes ONE
 * account on its venue-rejection streak while every other account keeps
 * trading — the global `tradingFrozen` banner above stays dark, and without
 * this the panel looks perfectly normal to the operator of the frozen
 * account. Reads `stats.accountGates` (array of `{account, blocked, reason,
 * reasonKind, sinceMs, expiresAtMs, actor}`): one urgent banner per account
 * whose gate is `blocked`, so a two-account freeze means two red lines.
 * Absent/empty on the wire (older core, or nothing frozen) renders nothing —
 * never an invented freeze. The lift lives on 设置·风控 (account.unfreeze) —
 * the banner names that door instead of pretending the operator must restart
 * the kernel.
 */
export function accountFreezeBanners(stats: EngineStats | null | undefined): SafetyBanner[] {
  const gates = stats?.accountGates
  if (!gates) return []
  return gates
    .filter(
      (g) =>
        g.blocked &&
        g.account &&
        // Configuration-posture gates (read_only / suspended / trading
        // disabled) are NOT risk freezes: no red banner, no unfreeze door.
        // They render via configGateBanners instead.
        !isConfigGate(g.reasonKind)
    )
    .sort((a, b) => a.account.localeCompare(b.account))
    .map((g) => ({
      // Urgent per the 2026-10-28 operator call: while THIS account is frozen
      // its trading is down right now — red, pinned, no ✕, no auto-dismiss.
      // Urgent banners carry no identity: they cannot be dismissed away.
      tone: 'error' as const,
      urgent: true,
      title: `账户已冻结（${g.account}）`,
      body: g.reason || '该账户已被冻结，原因未上报。',
      hint: '该账户拒一切新单（手动 + 策略开仓），平仓照放。核对原因后到 设置 · 风控卡片解冻（持久冻结，需运营人员解冻，仅自动冻结可被健康探针自愈）。冻结只影响这一个账户，其他账户照常交易。',
    }))
}

/**
 * Configuration-posture gate banners (read_only / suspended / trading
 * disabled): amber, dismissible, auto-fade. No unfreeze door — the operator
 * must edit .env and restart. One banner per account whose gate is a config
 * gate.
 */
export function configGateBanners(stats: EngineStats | null | undefined): SafetyBanner[] {
  const gates = stats?.accountGates
  if (!gates) return []
  return gates
    .filter(
      (g) =>
        g.blocked &&
        g.account &&
        isConfigGate(g.reasonKind)
    )
    .sort((a, b) => a.account.localeCompare(b.account))
    .map((g) => {
      const kind = g.reasonKind ?? ''
      const title =
        kind === 'ConfigReadOnly'
          ? `账户配置为只读（${g.account}）`
          : kind === 'ConfigSuspended'
            ? `账户已配置停用（${g.account}）`
            : `账户交易已关闭（${g.account}）`
      const hint =
        kind === 'ConfigReadOnly'
          ? 'read_only 派生自 .env AccountStatus，由配置决定。面板不提供内核解冻：改 .env 并重启内核即可恢复。平仓照放，仅新单被拒。'
          : kind === 'ConfigSuspended'
            ? 'suspended 派生自 .env AccountStatus，由配置决定。面板不提供内核解冻：改 .env 并重启内核即可恢复。'
            : 'trading_enabled=false 派生自 .env，由配置决定。面板不提供内核解冻：改 .env 并重启内核即可恢复。'
      return {
        tone: 'warn' as const,
        urgent: false,
        identity: `config-gate:${g.account}:${kind}`,
        title,
        body: g.reason || '该账户由配置态停用新单入场。',
        hint,
      }
    })
}

/**
 * Is this account currently frozen (issue 943 ③, the countdown pause)? A thin
 * predicate over the same `accountGates` read: the HFT page stops
 * interpolating its countdown while the DISPLAYED account is frozen — the
 * clock the kernel keeps running says nothing about trading being down, and
 * a countdown that keeps ticking reads as "everything is fine".
 */
export function accountFrozen(
  stats: EngineStats | null | undefined,
  accountId: string | null | undefined,
): boolean {
  if (!accountId) return false
  return Boolean(
    stats?.accountGates?.some(
      (g) =>
        g.account === accountId &&
        g.blocked === true &&
        // Configuration-posture gates are not "frozen" for HFT countdown
        // purposes — they block entries via config, not via the risk freeze
        // path, and the HFT page should not treat them as a risk freeze.
        !isConfigGate(g.reasonKind),
    ),
  )
}

/**
 * Configuration-derived gate kinds: the account is blocked because its .env /
 * config posture says so (read_only / suspended / trading disabled), NOT because
 * the risk system froze it. These gates are NOT liftable via account.unfreeze —
 * the operator must edit the config and restart. They must never render as a
 * red "account frozen" banner with an unfreeze door.
 */
const CONFIG_GATE_KINDS = new Set([
  'ConfigReadOnly',
  'ConfigSuspended',
  'TradingDisabled',
])

/**
 * Is this gate kind a configuration posture (read_only / suspended / trading
 * disabled) rather than a risk event (venue rejection / daily loss / manual /
 * kill switch)? Configuration gates are informational only: no unfreeze door,
 * no red freeze banner — just a "read the config" hint.
 */
export function isConfigGate(kind: string | null | undefined): boolean {
  return kind != null && CONFIG_GATE_KINDS.has(kind)
}
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

/**
 * A fallback receivable has aged past the operator's budget (issue #991, amber
 * banner #3). Under `BK_UNVERIFIED_EXPIRY_POLICY=receivable` an unverifiable-
 * expiry close books an estimated receivable instead of cash; the claim
 * redeems at venue truth on resolution/reconcile. When the venue stays silent
 * past `BK_RECEIVABLE_MAX_AGE_MS` (48h default) the estimated money is stuck:
 * it is excluded from day-budget and loss-breaker headroom, so the operator
 * must know it exists. Reads `stats.settlement.oldestFallbackClaimAgeMs`
 * (`null` when no claim is open) — the continuous face of the kernel's
 * once-per-episode warn. Amber + dismissible + auto-fade: an aging claim is
 * ATTENTION, not trading-down (red stays reserved for freeze/self-check).
 * Cash-policy kernels and older cores (no additive keys) render nothing.
 */
export function overdueReceivableBanner(
  stats: EngineStats | null | undefined,
  maxAgeMs: number | null | undefined,
): SafetyBanner | null {
  const settlement = stats?.settlement
  const age = settlement?.oldestFallbackClaimAgeMs
  if (age == null) return null
  if (maxAgeMs == null || maxAgeMs <= 0) return null
  if (age < maxAgeMs) return null
  const usd = settlement?.fallbackReceivableUsd
  const usdText =
    usd == null ? '' : `，金额 ${typeof usd === 'number' ? usd.toFixed(2) : usd} USDC`
  return {
    tone: 'warn',
    urgent: false,
    identity: 'receivable-overdue',
    title: '回退应收账款超龄',
    body: `有一笔过期回退结算的应收账款已等待 ${Math.round(age / 3_600_000)} 小时未获场馆确认${usdText}。`,
    hint: '这笔估算金额不计入日内预算与连损熔断的可用资金（不可交易），场馆给出真实结算价后会自动冲正入账。持续超龄请检查该市场的结算状态。',
  }
}
