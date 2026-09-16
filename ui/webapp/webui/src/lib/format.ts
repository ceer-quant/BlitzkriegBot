/** Display formatters — one place so every page renders numbers identically. */

export function num(v: unknown, fallback = '—'): string {
  if (v === undefined || v === null || v === '') return fallback
  const n = Number(v)
  return Number.isFinite(n) ? n.toLocaleString() : String(v)
}

/** Compact large counts: 148,913,828 → 148.9M */
export function compact(v: unknown, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  const abs = Math.abs(n)
  if (abs >= 1e9) return `${(n / 1e9).toFixed(2)}B`
  if (abs >= 1e6) return `${(n / 1e6).toFixed(2)}M`
  if (abs >= 1e4) return `${(n / 1e3).toFixed(1)}K`
  return n.toLocaleString()
}

export function money(v: unknown, digits = 2, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  return `$${n.toLocaleString(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits })}`
}

/** Signed money for PnL: +$12.30 / −$4.10 */
export function signedMoney(v: unknown, digits = 2, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  const sign = n > 0 ? '+' : n < 0 ? '−' : ''
  return `${sign}$${Math.abs(n).toLocaleString(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits })}`
}

/** Core sends winRate as either a 0..1 fraction or an already-scaled percent. */
export function winRatePct(v: unknown): number {
  const n = Number(v)
  if (!Number.isFinite(n)) return 0
  return n > 1.5 ? n : n * 100
}

export function pct(v: unknown, digits = 1, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  return `${n.toFixed(digits)}%`
}

export function signedPct(v: unknown, digits = 1, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  const sign = n > 0 ? '+' : n < 0 ? '−' : ''
  return `${sign}${Math.abs(n).toFixed(digits)}%`
}

/** Price on a 0..1 prediction market → cents with 1 decimal. */
export function cents(v: unknown, fallback = '—'): string {
  const n = Number(v)
  if (!Number.isFinite(n)) return fallback
  return `${(n * 100).toFixed(1)}¢`
}

export function duration(sec: unknown, fallback = '—'): string {
  const s = Number(sec)
  if (!Number.isFinite(s) || s < 0) return fallback
  if (s < 60) return `${s.toFixed(0)}s`
  const m = Math.floor(s / 60)
  const rem = Math.floor(s % 60)
  if (m < 60) return `${m}m${String(rem).padStart(2, '0')}s`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`
}

export function mmss(sec: unknown, fallback = '--:--'): string {
  const s = Number(sec)
  if (!Number.isFinite(s) || s < 0) return fallback
  return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(Math.floor(s % 60)).padStart(2, '0')}`
}

export function clockTime(ms?: number | null, fallback = '—'): string {
  if (!ms) return fallback
  return new Date(ms).toLocaleTimeString(undefined, { hour12: false })
}

export function dateTime(ms?: number | null, fallback = '—'): string {
  if (!ms) return fallback
  const d = new Date(ms)
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}`
}

export function shortAddr(addr?: string | null): string {
  if (!addr) return '—'
  return addr.length > 12 ? `${addr.slice(0, 6)}…${addr.slice(-4)}` : addr
}
