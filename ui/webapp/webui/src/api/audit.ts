/**
 * E25 arbitration audit domain — the intent-audit tail proxy and its wire
 * types (mirror arbitration/audit.rs's IntentAuditRecord).
 */
import { request } from './core.ts'

export interface CommandDoc {
  ok: boolean
  action?: string
  message?: string
  /** E27 (§8.2): the mode-handshake refusal when a `strategy … on` is refused. */
  reason?: string
  [extra: string]: unknown
}

// ── E25 arbitration audit (mirror arbitration/audit.rs's IntentAuditRecord) ──

/** One gate's verdict inside an audited suggestion (§3.2). */
export interface GateTraceView {
  gate: 'LEGALITY' | 'RISK' | 'RESERVATION' | 'PHYSICS' | string
  outcome: 'PASS' | 'MODIFY' | 'REJECT' | string
  /** The kernel's own justification — the UI prints it verbatim (§13.4). */
  detail: string
}

/** The decision, tagged by `status` — same vocabulary as the tail filter. */
export interface DecisionView {
  status: 'APPROVED' | 'MODIFIED' | 'REJECTED' | string
  /** Present on REJECTED. */
  reason?: string
  gate?: string
  detail?: string
  request_id?: string
  shares?: string | number
  price?: string | number
  physics?: {
    stopPrice: string | number
    forceExitSec: number
    ladder: { atPct: string | number; closeRatio: string | number; moveStopTo: string | number | null }[]
  }
  modification?: { kind: string; suggested?: string | number; approved?: string | number; limit?: string; tick?: string | number }
}

/** One audited suggestion (§3.4). Decimal values cross as STRINGS. */
export interface IntentAuditRecordView {
  tsMs: number
  accountId: string
  strategy: string
  intentId: string
  intent: unknown
  decision: DecisionView
  gates: GateTraceView[]
  latencyUs: number
}

export interface IntentAuditTailDoc {
  records: IntentAuditRecordView[]
  total?: number
  error?: string
}

export const auditApi = {
  /**
   * E25 (#331): the arbitration audit tail — a thin proxy of the core's
   * `intent.audit.tail` (§12.3). Filters pass through untouched.
   */
  intentAuditTail: (params?: { limit?: number; strategy?: string; decision?: string }) =>
    request<IntentAuditTailDoc>('/intent-audit', {}, {
      limit: params?.limit,
      strategy: params?.strategy,
      decision: params?.decision,
    }),
}
