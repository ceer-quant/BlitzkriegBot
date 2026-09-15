import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  MCP_TOOL_PREFIX,
  mcpToolName,
  skillFromToolName,
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

  it('resolves skill names from the canonical prefix', () => {
    assert.equal(skillFromToolName('blitzkrieg_trading_polymarket'), 'trading-polymarket');
    assert.equal(skillFromToolName('blitzkrieg_feeds'), 'feeds');
  });

  it('rejects unknown namespaces', () => {
    assert.equal(skillFromToolName('evil_trading_polymarket'), null);
    assert.equal(skillFromToolName('trading-polymarket'), null);
    assert.equal(skillFromToolName('clodds_trading_polymarket'), null);
  });

  it('accepts canonical incoming tool calls under a canonical allowlist', () => {
    const cfg = config({ allowedTools: new Set(['blitzkrieg_feeds']) });
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_feeds', cfg), false);
  });

  it('honours a canonical blocklist entry', () => {
    const cfg = config({ blockedTools: new Set(['blitzkrieg_trading']) });
    assert.equal(isToolAllowed('blitzkrieg_trading', cfg), false);
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
  });

  it('canonical read-only profile admits feeds and rejects trading', () => {
    const cfg = config({ toolProfile: 'read-only' });
    assert.equal(isToolAllowed('blitzkrieg_feeds', cfg), true);
    assert.equal(isToolAllowed('clodds_order', cfg), false);
    assert.equal(isToolAllowed('blitzkrieg_order', cfg), false);
  });
});
