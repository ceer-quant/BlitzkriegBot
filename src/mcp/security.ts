/**
 * MCP Security Layers
 *
 * Auth, tool allowlisting, rate limiting, audit logging, and input sanitization.
 * All opt-in via env vars — zero breaking changes when unconfigured.
 */

import { RateLimiter, detectInjection } from '../security/index.js';
import type { McpTool } from './index.js';

// =============================================================================
// CONFIG
// =============================================================================

export interface McpSecurityConfig {
  /** Comma-separated allowlist of tool names (empty = all allowed) */
  allowedTools: Set<string>;
  /** Comma-separated blocklist of tool names */
  blockedTools: Set<string>;
  /** Max calls per minute per client */
  rateLimit: number;
  /** Whether audit logging is enabled */
  auditEnabled: boolean;
  /** Predefined tool profile: read-only, trading, full */
  toolProfile: string;
}

/** Tool profiles — predefined sets of allowed tool prefixes (canonical `blitzkrieg_` names) */
const TOOL_PROFILES: Record<string, string[]> = {
  'read-only': [
    'blitzkrieg_feeds', 'blitzkrieg_markets', 'blitzkrieg_analytics',
    'blitzkrieg_portfolio', 'blitzkrieg_watchlist', 'blitzkrieg_search',
  ],
  'trading': [
    'blitzkrieg_feeds', 'blitzkrieg_markets', 'blitzkrieg_analytics',
    'blitzkrieg_portfolio', 'blitzkrieg_watchlist', 'blitzkrieg_search',
    'blitzkrieg_trading', 'blitzkrieg_execution', 'blitzkrieg_order',
  ],
  'full': [],
};

export function loadSecurityConfig(): McpSecurityConfig {
  const allowed = process.env.BLITZKRIEG_MCP_ALLOWED_TOOLS?.trim();
  const blocked = process.env.BLITZKRIEG_MCP_BLOCKED_TOOLS?.trim();
  const rateLimit = parseInt(process.env.BLITZKRIEG_MCP_RATE_LIMIT || '60', 10);
  const auditEnabled = process.env.BLITZKRIEG_MCP_AUDIT !== 'false';
  const toolProfile = process.env.BLITZKRIEG_MCP_TOOL_PROFILE || 'full';

  const parseEntries = (raw: string): Set<string> =>
    new Set(
      raw
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean),
    );

  return {
    allowedTools: allowed ? parseEntries(allowed) : new Set(),
    blockedTools: blocked ? parseEntries(blocked) : new Set(),
    rateLimit: isNaN(rateLimit) || rateLimit <= 0 ? 60 : rateLimit,
    auditEnabled,
    toolProfile,
  };
}

// =============================================================================
// TOOL ALLOWLISTING
// =============================================================================

/** Set member lookup. */
function setHas(set: Set<string>, name: string): boolean {
  return set.has(name);
}

/** Check whether a single tool name is allowed by the config. */
export function isToolAllowed(toolName: string, config: McpSecurityConfig): boolean {
  const name = toolName;

  // Blocklist always wins
  if (setHas(config.blockedTools, name)) return false;

  // Explicit allowlist
  if (config.allowedTools.size > 0) {
    return setHas(config.allowedTools, name);
  }

  // Profile-based filtering
  const prefixes = TOOL_PROFILES[config.toolProfile];
  if (prefixes && prefixes.length > 0) {
    return prefixes.some((prefix) => name.startsWith(prefix));
  }

  // 'full' profile or unknown — allow everything
  return true;
}

/** Filter a tools/list response to only include allowed tools */
export function filterTools(tools: McpTool[], config: McpSecurityConfig): McpTool[] {
  const hasFilters =
    config.blockedTools.size > 0 ||
    config.allowedTools.size > 0 ||
    (config.toolProfile !== 'full' && TOOL_PROFILES[config.toolProfile]?.length);

  if (!hasFilters) return tools;

  return tools.filter((t) => isToolAllowed(t.name, config));
}

// =============================================================================
// RATE LIMITING
// =============================================================================

let rateLimiter: RateLimiter | null = null;

function getRateLimiter(config: McpSecurityConfig): RateLimiter {
  if (!rateLimiter) {
    rateLimiter = new RateLimiter({
      maxRequests: config.rateLimit,
      windowMs: 60_000,
    });
  }
  return rateLimiter;
}

/**
 * Check rate limit for a client. Returns null if allowed,
 * or an error message string if rate limited.
 */
export function checkRateLimit(
  clientId: string,
  config: McpSecurityConfig,
): string | null {
  const limiter = getRateLimiter(config);
  const result = limiter.check(clientId);
  if (!result.allowed) {
    return `Rate limited: ${config.rateLimit} calls/min exceeded. Retry in ${Math.ceil(result.resetIn / 1000)}s`;
  }
  return null;
}

// =============================================================================
// INPUT SANITIZATION
// =============================================================================

/**
 * Scan all string values in tool arguments for injection patterns.
 * Returns null if clean, or a description of threats found.
 */
export function sanitizeToolArgs(args: Record<string, unknown>): string | null {
  const threats: string[] = [];

  function scanValue(value: unknown, path: string): void {
    if (typeof value === 'string') {
      const result = detectInjection(value);
      if (!result.safe) {
        threats.push(`${path}: ${result.threats.join(', ')}`);
      }
    } else if (Array.isArray(value)) {
      value.forEach((item, i) => scanValue(item, `${path}[${i}]`));
    } else if (value && typeof value === 'object') {
      for (const [k, v] of Object.entries(value)) {
        scanValue(v, `${path}.${k}`);
      }
    }
  }

  for (const [key, value] of Object.entries(args)) {
    scanValue(value, key);
  }

  return threats.length > 0 ? `Injection detected: ${threats.join('; ')}` : null;
}

// =============================================================================
// AUDIT LOGGING
// =============================================================================

export interface AuditEntry {
  tool: string;
  clientId: string;
  timestamp: number;
  durationMs: number;
  success: boolean;
  error?: string;
}

/** Log a structured audit entry to stderr (not pino — stdout would corrupt JSON-RPC) */
export function logAudit(entry: AuditEntry, config: McpSecurityConfig): void {
  if (!config.auditEnabled) return;

  const record = {
    level: 'info',
    time: entry.timestamp,
    audit: true,
    tool: entry.tool,
    client: entry.clientId,
    durationMs: entry.durationMs,
    success: entry.success,
    ...(entry.error ? { error: entry.error } : {}),
  };
  process.stderr.write(JSON.stringify(record) + '\n');
}
