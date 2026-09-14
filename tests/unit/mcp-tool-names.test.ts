import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  MCP_TOOL_PREFIX,
  LEGACY_MCP_TOOL_PREFIX,
  mcpToolName,
  skillFromToolName,
  isLegacyToolName,
  canonicalizeToolName,
} from '../../src/mcp/tool-names';
import {
  isToolAllowed,
  type McpSecurityConfig,
} from '../../src/mcp/security';

function config(overrides: Partial<McpSecurityConfig>): McpSecurityConfig {
  return {
    allowedTools: new Set(),
    blockedTools: new Set(),
    rateLimit: 60,
    auditEnabled: false,
    toolProfile: 'full',
    ...overrides,
  };
}

describe('MCP tool names', () => {
  it('advertises canonical blitzkrieg_ names', () => {
    assert.equal(mcpToolName('trading-polymarket'), 'blitzkrieg_trading_polymarket');
    assert.ok(mcpToolName('feeds').startsWith(MCP_TOOL_PREFIX));
  });

  it('resolves skill names from both prefixes', () => {
    assert.equal(skillFromToolName('blitzkrieg_trading_polymarket'), 'trading-polymarket');
    assert.equal(skillFromToolName('clodds_trading_polymarket'), 'trading-polymarket');
    assert.equal(skillFromToolName('blitzkrieg_feeds'), 'feeds');
    assert.equal(skillFromToolName('clodds_feeds'), 'feeds');
  });

  it('rejects unknown namespaces', () => {
    assert.equal(skillFromToolName('evil_trading_polymarket'), null);
    assert.equal(skillFromToolName('trading-polymarket'), null);
  });

  it('flags only the legacy prefix as legacy', () => {
    assert.equal(isLegacyToolName('clodds_feeds'), true);
    assert.equal(isLegacyToolName('blitzkrieg_feeds'), false);
    assert.equal(isLegacyToolName('blitzkrieg_clodds_x'), false);
  });

  it('canonicalises legacy names and leaves canonical ones untouched', () => {
    assert.equal(canonicalizeToolName('clodds_feeds'), 'blitzkrieg_feeds');
    assert.equal(canonicalizeToolName('blitzkrieg_feeds'), 'blitzkrieg_feeds');
    assert.equal(canonicalizeToolName('other'), 'other');
  });

  it('accepts legacy incoming tool calls under a canonical allowlist', () => {
    const cfg = config({ allowedTools: new Set(['blitzkrieg_feeds']) });
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_trading', cfg), false);
  });

  it('honours a legacy-prefixed blocklist entry against the canonical tool', () => {
    const cfg = config({ blockedTools: new Set(['clodds_trading']) });
    assert.equal(isToolAllowed('blitzkrieg_trading', cfg), false);
    assert.equal(isToolAllowed('clodds_trading', cfg), false);
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
  });

  it('canonical read-only profile admits both spellings and rejects trading', () => {
    const cfg = config({ toolProfile: 'read-only' });
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_order', cfg), false);
    assert.equal(isToolAllowed('blitzkrieg_order', cfg), false);
  });

  it('keeps both prefix constants exported', () => {
    assert.equal(LEGACY_MCP_TOOL_PREFIX, 'clodds_');
    assert.equal(MCP_TOOL_PREFIX, 'blitzkrieg_');
  });
});
