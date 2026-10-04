/**
 * Minimal Lua syntax highlighting for the preview pane.
 *
 * WHY NOT SHIKI: shiki's oniguruma/WASM engine is ~1MB of extra bundle for one
 * read-only pane that shows ~30 generated lines. This module does the whole
 * job a generated strategy file needs — comments, strings, numbers, keywords,
 * the kernel API names — with one tokenizing regex and zero dependencies. The
 * trade-off is disclosed in the issue-362 report.
 *
 * Output is a span-class list per line (never HTML — the panel's own red line
 * the panel's script-execution gate refuses markup injection everywhere, and classes let
 * the theme tokens colour both modes).
 */

export type LuaTokenKind =
  | 'comment'
  | 'string'
  | 'number'
  | 'keyword'
  | 'api'
  | 'plain'

export interface LuaToken {
  kind: LuaTokenKind
  text: string
}

const KEYWORDS = new Set([
  'and', 'break', 'do', 'else', 'elseif', 'end', 'false', 'for', 'function',
  'if', 'in', 'local', 'nil', 'not', 'or', 'repeat', 'return', 'then', 'true',
  'until', 'while',
])

/** The host surface a generated strategy may call / read. */
const API_NAMES = new Set([
  'place_order', 'on_tick', 'declare_modes', 'tick', 'account_id',
  'market_type', 'structure', 'symbol', 'side', 'order_type', 'price',
  'budget_usd', 'available_balance', 'mid_price', 'time_left_sec',
  'trend_confirmed', 'obi', 'prediction', 'binary_outcome_wheel',
  'websocket_feed', 'level2_snapshot',
])

/** One pass, ordered alternation: comment | string | number | word | anything. */
const TOKEN_RE =
  /(--[^\n]*)|("(?:[^"\\\n]|\\.)*")|(\b\d+(?:\.\d+)?\b)|([A-Za-z_][A-Za-z0-9_]*)|(\s+)|(.)/g

/** Tokenize one line of (generated) Lua. Stateless per line — good enough for
 * the fixed shapes the blueprint codegen emits, disclosed as the minimal option. */
export function tokenizeLuaLine(line: string): LuaToken[] {
  const tokens: LuaToken[] = []
  TOKEN_RE.lastIndex = 0
  let m: RegExpExecArray | null
  while ((m = TOKEN_RE.exec(line)) !== null) {
    const [text, comment, str, num, word, ws] = m
    if (comment !== undefined) {
      tokens.push({ kind: 'comment', text })
    } else if (str !== undefined) {
      tokens.push({ kind: 'string', text })
    } else if (num !== undefined) {
      tokens.push({ kind: 'number', text })
    } else if (word !== undefined) {
      if (KEYWORDS.has(word)) tokens.push({ kind: 'keyword', text: word })
      else if (API_NAMES.has(word)) tokens.push({ kind: 'api', text: word })
      else tokens.push({ kind: 'plain', text: word })
    } else if (ws !== undefined) {
      tokens.push({ kind: 'plain', text: ws })
    } else {
      tokens.push({ kind: 'plain', text })
    }
    if (m.index === TOKEN_RE.lastIndex) TOKEN_RE.lastIndex++
  }
  return tokens
}

/** Tokenize a whole source string into lines of tokens. */
export function highlightLua(source: string): LuaToken[][] {
  return source.split('\n').map(tokenizeLuaLine)
}
