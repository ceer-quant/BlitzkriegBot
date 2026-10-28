/**
 * Unified-mapping domain — the cross-venue pairing view and the candidate
 * confirm/revoke flow (mirror core unified.list JSON shape).
 */
import { request } from './core.ts'

// ── 跨所配对视图（issue 481；mirror core unified.list JSON shape）────────────

/** 一个映射里的一侧 venue（数据行 —— venue 是数据，不是分支）。 */
export interface UnifiedVenueRow {
  venue: string
  conditionId: string
  asset?: string | null
  expiresAtMs: number
  status: string
}

/** `unified.list` 的一个事件行：配对状态 + 两侧 venue + discrepancy 标记。 */
export interface UnifiedEventRow {
  id: string
  title: string
  status: 'paired' | 'singleLegAvailable' | string
  settlementSource?: string
  venues: UnifiedVenueRow[]
  discrepancies: string[]
}

/** `unified.list` 回执。 */
export interface UnifiedListDoc {
  version?: string
  events: UnifiedEventRow[]
  error?: string
}

// ── ④ 统一映射候选（unified.candidates / confirm / revoke）────────────────

/** 一个候选提议里的一侧 listing —— 就是 `unified.confirm` 回传的形状（原样
 *  往返，ledger 侧会重新验证一切）。 */
export interface UnifiedCandidateListing {
  venue: string
  conditionId: string
  upTokenId: string
  downTokenId: string
  asset?: string | null
  title: string
  settlementRules: string
  settlementSource: string
  expiresAtMs: number
  status: string
}

/** `unified.candidates` 的一行：两侧 listing + 匹配分。 */
export interface UnifiedCandidateRow {
  a: UnifiedCandidateListing
  b: UnifiedCandidateListing
  score: number
  expiryDeltaMs: number
}

/** `unified.candidates` 回执。 */
export interface UnifiedCandidatesDoc {
  version?: string
  candidates: UnifiedCandidateRow[]
  error?: string
}

/** `unified.confirm` 回执（事件已落账本 + 引擎可见）。 */
export interface UnifiedConfirmDoc {
  version?: string
  event?: { id: string; title: string; status: string; venues: string[] }
  error?: string
}

/** `unified.revoke` 回执。 */
export interface UnifiedRevokeDoc {
  version?: string
  revoked?: string
  error?: string
}

export const unifiedApi = {
  /**
   * 跨所配对视图的读路径（`unified.list`，issue 481，只读）：已确认的
   * 统一映射快照 —— 每个事件的配对状态、两侧 venue、discrepancy 标记。
   * venue 一律数据渲染。
   */
  unifiedList: () => request<UnifiedListDoc>('/unified'),
  /**
   * ④ 统一映射候选的读路径（`unified.candidates`，只读）：当前轮的跨所
   * 配对提议 —— 自动匹配只提议，confirm 才生效。空数组 = 本轮没有
   * ≥2 个已声明 venue 的配对（或单插件世界）。
   */
  unifiedCandidates: () => request<UnifiedCandidatesDoc>('/unified-candidates'),
  /**
   * ④ 候选确认写路径（`unified.confirm`）：`{title, listings, actor?}` 原样
   * 转发。ledger 侧重新验证（标题相似度、资产一致性、一 venue 一事件），
   * 被拒的 confirm 原地不动。
   */
  unifiedConfirm: (body: { title: string; listings: UnifiedCandidateListing[]; actor?: string }) =>
    request<UnifiedConfirmDoc>('/unified/confirm', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * ④ 映射撤销写路径（`unified.revoke`）：`{eventId, actor?}` 原样转发，
   * 账本落审计、引擎视图同步拔除（策略立刻看不见）。
   */
  unifiedRevoke: (body: { eventId: string; actor?: string }) =>
    request<UnifiedRevokeDoc>('/unified/revoke', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
