# BlitzkriegBot Agent Integration Guide

**For agents:** BlitzkriegBot is a self-hosted, high-frequency trading core for
prediction markets (Polymarket, Kalshi) with a pluggable, full-fidelity strategy
interface. There is no hosted compute/marketplace service — every endpoint below
runs on your own deployment. Version 0.1 runs **dry (paper) mode only**; live
trading is intentionally disabled.

---

## Quick Start

```bash
# Install and run the gateway locally (dry mode is the default)
npm install -g blitzkrieg-bot
blitzkrieg start

# Health check against your own deployment (default port 18789)
curl http://127.0.0.1:18789/health

# Built-in web console
open http://127.0.0.1:18789/webchat/
```

The gateway binds to `gateway.port` in the config file (default `18789`). Do not
expose it to the public internet without an auth token
(`BLITZKRIEG_GATEWAY_TOKEN`) and TLS.

---

## Gateway API (self-hosted)

**Base URL**: `http://<your-host>:<port>` (your deployment only)

### Authentication

Endpoints requiring auth expect the gateway bearer token:

```bash
curl -H "Authorization: Bearer $BLITZKRIEG_GATEWAY_TOKEN" \
  http://127.0.0.1:18789/api/trading/balance
```

### Health

```bash
curl http://127.0.0.1:18789/health
```

### Account / trading (read-only observations in dry mode)

```bash
curl -H "Authorization: Bearer $BLITZKRIEG_GATEWAY_TOKEN" \
  http://127.0.0.1:18789/api/trading/balance
```

### Market data

```bash
# Search the market index
curl 'http://127.0.0.1:18789/market-index/search?q=BTC'

# Stored ticks / OHLC / order-book history for a market
curl 'http://127.0.0.1:18789/api/ticks/polymarket/<marketId>'
curl 'http://127.0.0.1:18789/api/ohlc/polymarket/<marketId>'
curl 'http://127.0.0.1:18789/api/orderbook-history/polymarket/<marketId>'
```

### Backtest & performance

```bash
curl -X POST http://127.0.0.1:18789/api/backtest \
  -H 'Content-Type: application/json' \
  -d @backtest-params.json

curl http://127.0.0.1:18789/api/performance
```

### Observability

```bash
# Prometheus metrics (auth required)
curl -H "Authorization: Bearer $BLITZKRIEG_GATEWAY_TOKEN" \
  http://127.0.0.1:18789/metrics
```

See [docs/API.md](../docs/API.md) and the OpenAPI descriptor in
[docs/openapi.yaml](../docs/openapi.yaml) for the full surface.

---

## MCP (Model Context Protocol)

Agents can connect over MCP instead of HTTP. Tools are advertised with the
`blitzkrieg_` prefix (the legacy `clodds_` prefix is accepted inbound for one
release):

```bash
blitzkrieg mcp list
blitzkrieg mcp add my-server "npx -y @modelcontextprotocol/server-filesystem $HOME"
blitzkrieg mcp test my-server
blitzkrieg mcp remove my-server
```

MCP descriptors live in `./.mcp.json`, `./mcp.json`, or
`~/.config/blitzkrieg/mcp.json` (a legacy `~/.config/clodds/mcp.json` is still
read for one release).

---

## Configuration

All configuration uses `BLITZKRIEG_*` environment variables (the old `CLODDS_*`
names are accepted as deprecated aliases for one release):

| Variable | Purpose |
| --- | --- |
| `BLITZKRIEG_STATE_DIR` | State directory (sockets, wallets, DB, archives) |
| `BLITZKRIEG_WORKSPACE` | Workspace root |
| `BLITZKRIEG_GATEWAY_TOKEN` | Bearer token for auth-required endpoints |

Trading defaults to dry/paper (`dryRun: true`); 0.1 has no live execution path.

---

## Support

- **Repository**: https://github.com/ceer-quant/BlitzkriegBot
- **Issues**: https://github.com/ceer-quant/BlitzkriegBot/issues
- **Docs**: [docs/](../docs/)

---

**Version**: 0.1.0
**Status**: Dry-mode base release — live trading disabled by design.
