/**
 * MCP tool namespace (E1-d).
 *
 * `blitzkrieg_<skill>` is canonical; the legacy `clodds_<skill>` prefix is
 * accepted on incoming tools/call requests for one release so existing MCP
 * clients keep working. Only canonical names are advertised in tools/list.
 */

export const MCP_TOOL_PREFIX = 'blitzkrieg_';
export const LEGACY_MCP_TOOL_PREFIX = 'clodds_';

/** Canonical advertised tool name for a skill: `trading-polymarket` -> `blitzkrieg_trading_polymarket`. */
export function mcpToolName(skillName: string): string {
  return `${MCP_TOOL_PREFIX}${skillName.replace(/-/g, '_')}`;
}

/** Map an accepted (canonical or legacy) tool name back to its skill name; null if neither prefix matches. */
export function skillFromToolName(toolName: string): string | null {
  let rest: string;
  if (toolName.startsWith(MCP_TOOL_PREFIX)) {
    rest = toolName.slice(MCP_TOOL_PREFIX.length);
  } else if (toolName.startsWith(LEGACY_MCP_TOOL_PREFIX)) {
    rest = toolName.slice(LEGACY_MCP_TOOL_PREFIX.length);
  } else {
    return null;
  }
  return rest.replace(/_/g, '-');
}

export function isLegacyToolName(toolName: string): boolean {
  return toolName.startsWith(LEGACY_MCP_TOOL_PREFIX) && !toolName.startsWith(MCP_TOOL_PREFIX);
}

/** Normalise an incoming tool name (or an allowlist entry) to the canonical prefix. */
export function canonicalizeToolName(toolName: string): string {
  return isLegacyToolName(toolName)
    ? MCP_TOOL_PREFIX + toolName.slice(LEGACY_MCP_TOOL_PREFIX.length)
    : toolName;
}
