/**
 * SSE change stream (#838) — the push half of the panel's data feed.
 *
 * The gateway exposes `GET /api/events` as a `text/event-stream` change
 * ticker: same auth gate as every `/api` route, frames are `event: snapshot`
 * carrying exactly the `/api/snapshot` document. This module consumes it with
 * a streaming `fetch` — not `EventSource`, which cannot send the
 * `X-Auth-Token` header, and the `?token=` query channel was removed in #460
 * (it leaks into logs/Referer). Same session, same credential channel, no
 * exceptions carved out of the gate.
 *
 * Failure posture: the stream is an optimization on freshness, never a
 * dependency. Any death (401, network drop, parse derail) resolves to
 * `false` and the caller just keeps its existing polling loop — the panel is
 * never blinded by a dead push stream, and never widens its own attack
 * surface with a token-in-URL fallback.
 *
 * Written for Node's strip-only TypeScript mode too (the panel's `check:*`
 * gates import `src/` directly): no parameter properties, no enums.
 */

/** One delivered snapshot frame, already parsed. */
export type SseFrame =
  | { kind: 'snapshot'; doc: Record<string, unknown> }
  | { kind: 'ping' }

/**
 * Read `event:` frames off the wire until the stream dies. Resolves `true`
 * if at least one snapshot was delivered (caller may reconnect), `false` if
 * the stream never became useful (auth refused, gateway down — caller does
 * NOT hot-reconnect; polling already covers that path).
 */
export async function readEventStream(
  token: string,
  onFrame: (frame: SseFrame) => void,
): Promise<boolean> {
  let res: Response
  try {
    res = await fetch('/api/events', {
      headers: { 'X-Auth-Token': token, Accept: 'text/event-stream' },
    })
  } catch {
    return false // gateway unreachable; polling handles the alert
  }
  if (!res.ok || !res.body) return false
  let delivered = false
  let buf = ''
  const decoder = new TextDecoder()
  // `getReader()` rather than `for await` over the body: the DOM
  // ReadableStream type has no asyncIterator under this tsconfig, and the
  // reader loop is the same shape anyway.
  const reader = res.body.getReader()
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      // SSE frames are \r\n\r\n-terminated; an event block may also end at
      // \n\n. Normalize while scanning, then split on blank lines.
      buf += decoder.decode(value, { stream: true })
      let idx: number
      while ((idx = buf.search(/\r?\n\r?\n/)) >= 0) {
        const block = buf.slice(0, idx)
        buf = buf.slice(idx + (buf[idx] === '\r' ? 4 : 2))
        const frame = parseBlock(block)
        if (frame?.kind === 'snapshot') delivered = true
        if (frame) onFrame(frame)
      }
    }
  } catch {
    // stream torn mid-frame; whatever was delivered stands
  }
  return delivered
}

/** Parse one SSE block into a frame; anything unparseable is skipped. */
function parseBlock(block: string): SseFrame | null {
  let event = 'message'
  const dataLines: string[] = []
  for (const line of block.split(/\r?\n/)) {
    if (line.startsWith(':')) continue // keepalive comment
    if (line.startsWith('event:')) event = line.slice(6).trim()
    else if (line.startsWith('data:')) dataLines.push(line.slice(5).trimStart())
  }
  if (event === 'snapshot') {
    try {
      const doc = JSON.parse(dataLines.join('\n')) as Record<string, unknown>
      if (doc && typeof doc === 'object') return { kind: 'snapshot', doc }
    } catch {
      return null // torn/partial frame — next frame supersedes it anyway
    }
  }
  return null
}
