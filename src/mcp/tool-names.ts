/**
 * MCP tool namespace.
 *
 * `blitzkrieg_<skill>` is the only prefix. Only canonical names are advertised
 * in tools/list; anything else is an unknown tool.
 */

export const MCP_TOOL_PREFIX = 'blitzkrieg_';

/** Canonical advertised tool name for a skill: `trading-polymarket` -> `blitzkrieg_trading_polymarket`. */
export function mcpToolName(skillName: string): string {
  return `${MCP_TOOL_PREFIX}${skillName.replace(/-/g, '_')}`;
}

/** Map a tool name back to its skill name; null unless the canonical prefix matches. */
export function skillFromToolName(toolName: string): string | null {
  if (!toolName.startsWith(MCP_TOOL_PREFIX)) return null;
  return toolName.slice(MCP_TOOL_PREFIX.length).replace(/_/g, '-');
}
