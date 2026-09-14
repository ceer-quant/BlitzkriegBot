# Blitzkrieg Deployment Guide

Complete guide for deploying Blitzkrieg in production environments.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Environment Variables](#environment-variables)
- [Deployment Options](#deployment-options)
- [Database Setup](#database-setup)
- [Production Checklist](#production-checklist)
- [Monitoring](#monitoring)
- [Backups](#backups)
- [Security Hardening](#security-hardening)
- [Troubleshooting](#troubleshooting)

---

## Prerequisites

| Requirement | Version | Notes |
|-------------|---------|-------|
| Node.js | 22+ | Required for all deployments |
| npm | 10+ | Bundled with Node.js |
| Python | 3.8+ | Required for some trading scripts |
| Docker | 24+ | Optional, for containerized deployment |
| Git | 2.x | For source deployment |

---

## Environment Variables

### Required (Minimum to Run)

```bash
# Anthropic API Key - Required for AI functionality
# Get from: https://console.anthropic.com
ANTHROPIC_API_KEY=sk-ant-...
```

### Messaging Channels

At least one channel is recommended. WebChat works immediately without configuration.

```bash
# Telegram (Recommended - easiest setup)
# Get from: https://t.me/BotFather
TELEGRAM_BOT_TOKEN=

# Discord
# Get from: https://discord.com/developers/applications
DISCORD_BOT_TOKEN=
DISCORD_APP_ID=

# Slack
# Get from: https://api.slack.com/apps
SLACK_BOT_TOKEN=
SLACK_APP_TOKEN=

# WebChat (built-in browser client)
WEBCHAT_TOKEN=optional-auth-token

# WhatsApp (requires baileys library)
# Uses QR code authentication via CLI
WHATSAPP_ENABLED=true

# Matrix
MATRIX_HOMESERVER_URL=https://matrix.org
MATRIX_ACCESS_TOKEN=
MATRIX_USER_ID=@bot:matrix.org

# Signal (requires signal-cli)
SIGNAL_PHONE_NUMBER=+15551234567
SIGNAL_CLI_PATH=/usr/bin/signal-cli

# Microsoft Teams
TEAMS_APP_ID=
TEAMS_APP_PASSWORD=

# Google Chat
GOOGLECHAT_CREDENTIALS_PATH=/path/to/credentials.json

# LINE
LINE_CHANNEL_ACCESS_TOKEN=
LINE_CHANNEL_SECRET=

# iMessage (macOS only)
IMESSAGE_ENABLED=false
```

### Trading Credentials

```bash
# Polymarket
POLY_API_KEY=
POLY_API_SECRET=
POLY_API_PASSPHRASE=
POLY_PRIVATE_KEY=0x...
POLY_FUNDER_ADDRESS=0x...

# Kalshi
KALSHI_API_KEY=
KALSHI_API_SECRET=

# Manifold
MANIFOLD_API_KEY=

# Betfair
BETFAIR_APP_KEY=
BETFAIR_USERNAME=
BETFAIR_PASSWORD=

# Smarkets
SMARKETS_TOKEN=

# Hyperliquid (Perpetuals)
HYPERLIQUID_WALLET=0x...
HYPERLIQUID_PRIVATE_KEY=0x...

# Binance Futures
BINANCE_API_KEY=
BINANCE_API_SECRET=

# Bybit Futures
BYBIT_API_KEY=
BYBIT_API_SECRET=

# Solana DeFi
SOLANA_PRIVATE_KEY=base58-encoded-key
SOLANA_RPC_URL=https://api.mainnet-beta.solana.com

# Percolator (On-Chain Solana Perps)
PERCOLATOR_ENABLED=false
PERCOLATOR_SLAB=                    # Market slab account pubkey
PERCOLATOR_ORACLE=                  # Chainlink/Pyth oracle pubkey
PERCOLATOR_MATCHER_PROGRAM=         # Matcher program pubkey
PERCOLATOR_MATCHER_CONTEXT=         # Matcher context account
PERCOLATOR_PROGRAM_ID=2SSnp35m7FQ7cRLNKGdW5UzjYFF6RBUNq7d3m5mqNByp
PERCOLATOR_LP_INDEX=0               # Preferred LP index
PERCOLATOR_KEEPER_ENABLED=false     # Run background crank
PERCOLATOR_DRY_RUN=true             # Simulate trades (default: true)
PERCOLATOR_RPC_URL=                 # Falls back to SOLANA_RPC_URL

# EVM DeFi
EVM_PRIVATE_KEY=0x...
ALCHEMY_API_KEY=
```

### Gateway Configuration

```bash
# Server binding
BLITZKRIEG_PORT=18789
BLITZKRIEG_HOST=127.0.0.1

# State directory (database, backups)
BLITZKRIEG_STATE_DIR=~/.blitzkrieg
BLITZKRIEG_CONFIG_PATH=~/.blitzkrieg/blitzkrieg.json

# Security
BLITZKRIEG_TOKEN=your-secret-token
BLITZKRIEG_IP_RATE_LIMIT=100
BLITZKRIEG_FORCE_HTTPS=false
BLITZKRIEG_HSTS_ENABLED=false

# Webhooks
BLITZKRIEG_WEBHOOK_SECRET=your-webhook-secret
BLITZKRIEG_WEBHOOK_REQUIRE_SIGNATURE=1
```

### Database Configuration

```bash
# SQLite (default, auto-created)
BLITZKRIEG_DB_PATH=~/.blitzkrieg/blitzkrieg.db

# Backup settings
BLITZKRIEG_DB_BACKUP_ENABLED=true
BLITZKRIEG_DB_BACKUP_INTERVAL=86400000
BLITZKRIEG_DB_BACKUP_KEEP=7
```

### Market Index

```bash
MARKET_INDEX_ENABLED=true
MARKET_INDEX_SYNC_INTERVAL=300000
MARKET_INDEX_LIMIT_PER_PLATFORM=1000
```

### External Data Sources

```bash
# OpenAI (for embeddings)
OPENAI_API_KEY=

# Twitter/X (for news feeds)
TWITTER_BEARER_TOKEN=
X_BEARER_TOKEN=

# External probability sources
CME_FEDWATCH_ACCESS_TOKEN=
FIVETHIRTYEIGHT_FORECAST_URL=
SILVER_BULLETIN_FORECAST_URL=
ODDS_API_KEY=

# Whale tracking
BIRDEYE_API_KEY=
ALCHEMY_API_KEY=
```

### Marketplace (Escrow)

```bash
# Solana RPC endpoint (mainnet or devnet)
SOLANA_RPC_URL=https://api.mainnet-beta.solana.com

# USDC SPL token mint address
USDC_MINT=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v

# Platform wallet public key (receives 5% marketplace fees)
MARKETPLACE_PLATFORM_WALLET=

# Platform wallet private key (base58) — pays ATA rent for escrow transfers
# Generate with: solana-keygen new --no-bip39-passphrase
MARKETPLACE_PLATFORM_KEY=

# Encryption key for escrow keypair storage (REQUIRED — no fallback)
ENCRYPTION_KEY=
```

### Telemetry

```bash
OTEL_ENABLED=true
OTEL_SERVICE_NAME=blitzkrieg
OTEL_ENDPOINT=http://localhost:4318
OTEL_METRICS_PORT=9090
OTEL_SAMPLE_RATE=1.0
```

---

## Deployment Options

### 1. npm Install (Recommended)

```bash
npm install -g blitzkrieg
blitzkrieg onboard
```

The `onboard` wizard handles API key setup, channel selection, and config generation. After setup, start anytime with `blitzkrieg start`.

### 2. From Source

```bash
# Clone repository
git clone https://github.com/alsk1992/BlitzkriegBot.git
cd BlitzkriegBot

# Install dependencies
npm ci

# Create environment file
cp .env.example .env
# Edit .env with your API keys

# Build
npm run build

# Start
npm start
# Or: node dist/index.js
```

### 3. Docker (Single Container)

**Build:**
```bash
docker build -t blitzkrieg .
```

**Run:**
```bash
docker run --rm \
  -p 18789:18789 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e TELEGRAM_BOT_TOKEN=... \
  -e WEBCHAT_TOKEN=... \
  -v blitzkrieg_data:/data \
  blitzkrieg
```

The container sets `BLITZKRIEG_STATE_DIR=/data`, so:
- Database: `/data/blitzkrieg.db`
- Backups: `/data/backups`

### 4. Docker Compose

**docker-compose.yml:**
```yaml
version: '3.8'

services:
  blitzkrieg:
    build: .
    ports:
      - "18789:18789"
    environment:
      - ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}
      - TELEGRAM_BOT_TOKEN=${TELEGRAM_BOT_TOKEN}
      - WEBCHAT_TOKEN=${WEBCHAT_TOKEN}
    volumes:
      - blitzkrieg_data:/data
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:18789/health"]
      interval: 30s
      timeout: 10s
      retries: 3

volumes:
  blitzkrieg_data:
```

**Start:**
```bash
docker compose up -d --build
```

**Update:**
```bash
docker compose pull
docker compose up -d --build
```

### 5. Systemd Service (Linux)

Create `/etc/systemd/system/blitzkrieg.service`:

```ini
[Unit]
Description=Blitzkrieg Gateway
After=network.target

[Service]
Type=simple
User=blitzkrieg
Group=blitzkrieg
WorkingDirectory=/opt/blitzkrieg
EnvironmentFile=/etc/blitzkrieg/blitzkrieg.env
ExecStart=/usr/bin/node /opt/blitzkrieg/dist/index.js
Restart=on-failure
RestartSec=5
StandardOutput=journal
StandardError=journal

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/blitzkrieg
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

**Setup:**
```bash
# Create user
sudo useradd -r -s /sbin/nologin blitzkrieg

# Create directories
sudo mkdir -p /opt/blitzkrieg /var/lib/blitzkrieg /etc/blitzkrieg
sudo chown blitzkrieg:blitzkrieg /var/lib/blitzkrieg

# Copy application
sudo cp -r dist/* /opt/blitzkrieg/

# Create environment file
sudo cp .env /etc/blitzkrieg/blitzkrieg.env
sudo chmod 600 /etc/blitzkrieg/blitzkrieg.env

# Enable and start
sudo systemctl daemon-reload
sudo systemctl enable blitzkrieg
sudo systemctl start blitzkrieg

# Check status
sudo systemctl status blitzkrieg
sudo journalctl -u blitzkrieg -f
```

### 6. Vercel (Serverless)

Blitzkrieg includes a Cloudflare Worker variant for serverless deployment. See `apps/blitzkrieg-worker/`.

**For Vercel deployment of the docs site:**
```bash
cd apps/docs
vercel --prod --yes
```

---

## Database Setup

### SQLite (Default)

SQLite is used by default with the sql.js library (pure JavaScript, no native dependencies).

```bash
# Database location
~/.blitzkrieg/blitzkrieg.db

# Or specify custom path
BLITZKRIEG_DB_PATH=/var/lib/blitzkrieg/blitzkrieg.db
```

The database is created automatically on first run with all required tables.

### Schema Overview

| Table | Purpose |
|-------|---------|
| `users` | User accounts and settings |
| `sessions` | Chat session history |
| `trades` | Trade execution logs |
| `positions` | Open positions |
| `pnl_snapshots` | P&L history |
| `markets` | Market index cache |
| `ticks` | Price tick history |
| `orderbooks` | Orderbook snapshots |
| `webhooks` | Webhook configurations |
| `cron_jobs` | Scheduled tasks |
| `memories` | Semantic memory store |
| `ledger` | Decision audit trail |

### Backup Configuration

```bash
# Enable automatic backups
BLITZKRIEG_DB_BACKUP_ENABLED=true

# Backup interval (default: daily)
BLITZKRIEG_DB_BACKUP_INTERVAL=86400000

# Number of backups to keep
BLITZKRIEG_DB_BACKUP_KEEP=7

# Backup directory
BLITZKRIEG_DB_BACKUP_PATH=~/.blitzkrieg/backups
```

**Manual backup:**
```bash
cp ~/.blitzkrieg/blitzkrieg.db ~/.blitzkrieg/backups/blitzkrieg-$(date +%Y%m%d).db
```

---

## Production Checklist

### Pre-Deployment

- [ ] Node.js 22+ installed
- [ ] `.env` file created with required variables
- [ ] `ANTHROPIC_API_KEY` configured
- [ ] At least one messaging channel configured
- [ ] Trading credentials encrypted (if trading enabled)
- [ ] Webhook secrets set

### Security

- [ ] Gateway bound to loopback (`127.0.0.1`)
- [ ] Reverse proxy configured (nginx/Caddy) with TLS
- [ ] `BLITZKRIEG_WEBHOOK_REQUIRE_SIGNATURE=1`
- [ ] `BLITZKRIEG_TOKEN` set for metrics endpoint
- [ ] Firewall configured (SSH + app ports only)
- [ ] SSH password auth disabled
- [ ] fail2ban installed and configured

### Monitoring

- [ ] Health check endpoint monitored (`/health`)
- [ ] `blitzkrieg doctor` runs without errors
- [ ] Log aggregation configured
- [ ] Alerts set for service downtime
- [ ] OpenTelemetry configured (if needed)

### Database

- [ ] Database backup enabled
- [ ] Backup retention configured
- [ ] Disk space monitored

### Performance

- [ ] Market index enabled with reasonable limits
- [ ] Tick recorder buffer sizes configured
- [ ] Rate limits appropriate for traffic

---

## Monitoring

### Health Check

```bash
# HTTP health check
curl http://localhost:18789/health
# {"status":"ok","timestamp":1706500000000}

# CLI diagnostics
blitzkrieg doctor
```

### Prometheus Metrics

Enable OpenTelemetry:
```json
{
  "telemetry": {
    "enabled": true,
    "metricsPort": 9090
  }
}
```

Access at `http://localhost:9090/metrics`.

**Key Metrics:**
| Metric | Description |
|--------|-------------|
| `blitzkrieg_requests_total` | Total HTTP requests |
| `blitzkrieg_request_duration_ms` | Request latency |
| `llm_tokens_total` | LLM token usage |
| `llm_requests_total` | LLM API calls |
| `trades_total` | Total trades executed |
| `positions_value` | Open position value |

### Logging

Logs use Pino format (JSON by default).

**Pretty print in development:**
```bash
npm run dev | npx pino-pretty
```

**Log levels:**
```bash
# Set log level
LOG_LEVEL=debug  # trace, debug, info, warn, error
```

### Alerts

Configure alert targets in `blitzkrieg.json`:

```json
{
  "monitoring": {
    "alertTargets": [
      { "platform": "telegram", "chatId": "123456789" },
      { "platform": "slack", "channelId": "C12345" }
    ]
  }
}
```

---

## Reverse Proxy

### nginx

```nginx
upstream blitzkrieg {
    server 127.0.0.1:18789;
}

server {
    listen 443 ssl http2;
    server_name blitzkrieg.example.com;

    ssl_certificate /etc/letsencrypt/live/blitzkrieg.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/blitzkrieg.example.com/privkey.pem;

    location / {
        proxy_pass http://blitzkrieg;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 86400;
    }
}
```

### Caddy

```caddyfile
blitzkrieg.example.com {
    reverse_proxy localhost:18789
}
```

---

## Security Hardening

### Built-in Hardening CLI

```bash
# Preview changes (safe)
blitzkrieg secure --dry-run

# Apply all hardening
sudo blitzkrieg secure

# Non-interactive
sudo blitzkrieg secure --yes

# Run audit only
blitzkrieg secure audit
```

### What Gets Hardened

| Component | Changes |
|-----------|---------|
| SSH | Disable password auth, root login, limit attempts |
| Firewall | Configure ufw with minimal ports |
| fail2ban | Block brute-force attempts |
| Auto-updates | Enable unattended-upgrades |
| Kernel | Apply sysctl hardening |

### Manual Security Steps

```bash
# 1. Disable SSH password auth
sudo sed -i 's/#PasswordAuthentication yes/PasswordAuthentication no/' /etc/ssh/sshd_config
sudo systemctl restart sshd

# 2. Configure firewall
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow ssh
sudo ufw allow 443/tcp
sudo ufw enable

# 3. Install fail2ban
sudo apt install fail2ban
sudo systemctl enable fail2ban

# 4. Enable auto-updates
sudo apt install unattended-upgrades
sudo dpkg-reconfigure -plow unattended-upgrades
```

---

## Troubleshooting

### Common Issues

**Gateway won't start:**
```bash
# Check logs
journalctl -u blitzkrieg -f

# Run diagnostics
blitzkrieg doctor

# Check if port is in use
lsof -i :18789
```

**Channel not responding:**
1. Verify token in `.env`
2. Check channel enabled in config
3. Verify pairing approved (for DMs)
4. Check bot permissions on platform

**Database errors:**
```bash
# Check file permissions
ls -la ~/.blitzkrieg/

# Verify database exists
sqlite3 ~/.blitzkrieg/blitzkrieg.db ".tables"

# Reset database (loses data!)
rm ~/.blitzkrieg/blitzkrieg.db
```

**Memory issues:**
```bash
# Increase Node.js memory
NODE_OPTIONS="--max-old-space-size=4096" npm start
```

**WebSocket disconnections:**
- Check reverse proxy timeout settings
- Ensure WebSocket upgrade headers are forwarded
- Verify keepalive pings are not filtered

### Debug Mode

```bash
# Enable debug logging
LOG_LEVEL=debug npm run dev

# Enable Node.js debugging
node --inspect dist/index.js
```

### Support

- GitHub Issues: https://github.com/alsk1992/BlitzkriegBot/issues
- Repository: https://github.com/ceer-quant/BlitzkriegBot
- Issues: https://github.com/ceer-quant/BlitzkriegBot/issues
