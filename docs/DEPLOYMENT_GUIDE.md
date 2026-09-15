# Blitzkrieg Deployment Guide

This guide covers production-style deployment for Blitzkrieg using Node.js,
Docker, or a systemd service.

## Prerequisites

- Node.js 22+ (for non-Docker installs)
- Python 3 (required for trading scripts)
- A configured `.env` with your API keys

Required environment variables (minimum):
- `ANTHROPIC_API_KEY`
- `TELEGRAM_BOT_TOKEN` (if using Telegram)

Optional:
- `WEBCHAT_TOKEN` (WebChat auth)
- `SMTP_*` (email alerts)
- `MARKET_INDEX_*` (market index tuning)

## Runtime data locations

Blitzkrieg stores persistent data in the state directory (defaults to the user's
home directory under `~/.blitzkrieg`). You can override it with
`BLITZKRIEG_STATE_DIR`.

- Database: `~/.blitzkrieg/blitzkrieg.db` (or `$BLITZKRIEG_STATE_DIR/blitzkrieg.db`)
- Backups: `~/.blitzkrieg/backups` (or `$BLITZKRIEG_STATE_DIR/backups`)

You can control paths for config and workspace with:
- `BLITZKRIEG_CONFIG_PATH`
- `BLITZKRIEG_WORKSPACE`

## Deployment options

### 1) npm install (recommended)

```bash
npm install -g blitzkrieg
blitzkrieg onboard
```

The `onboard` wizard handles API key setup, channel selection, and config generation. After setup, start anytime with `blitzkrieg start`.

### 2) Node.js (from source)

```
git clone https://github.com/alsk1992/BlitzkriegBot.git && cd BlitzkriegBot
npm ci
npm run build
node dist/index.js
```

For CLI usage after build:

```
node dist/cli/index.js start
```

### 2) Docker (single container)

Build the image:

```
docker build -t blitzkrieg .
```

Run it:

```
docker run --rm \
  -p 51888:51888 \
  -e ANTHROPIC_API_KEY=... \
  -e TELEGRAM_BOT_TOKEN=... \
  -e WEBCHAT_TOKEN=... \
  -v blitzkrieg_data:/data \
  blitzkrieg
```

Note: the container sets `BLITZKRIEG_STATE_DIR=/data`, so the database lives at
`/data/blitzkrieg.db` (backups at `/data/backups`).

### 3) Docker Compose

Use the included `docker-compose.yml`:

```
docker compose up -d --build
```

To pass secrets, add an `.env` file in the same directory and list variables
there, or edit the `environment:` section.

### 4) systemd (Linux)

Create a unit file (example: `/etc/systemd/system/blitzkrieg.service`):

```
[Unit]
Description=Blitzkrieg Gateway
After=network.target

[Service]
Type=simple
WorkingDirectory=/opt/blitzkrieg
EnvironmentFile=/etc/blitzkrieg/blitzkrieg.env
ExecStart=/usr/bin/node /opt/blitzkrieg/dist/index.js
Restart=on-failure
User=blitzkrieg

[Install]
WantedBy=multi-user.target
```

Then:

```
systemctl daemon-reload
systemctl enable blitzkrieg
systemctl start blitzkrieg
```

## Reverse proxy and TLS

If exposing the gateway publicly, put it behind a reverse proxy (nginx, Caddy,
Traefik) and terminate TLS there. Keep the gateway on loopback and only forward
port 51888 from the proxy.

## Updates

```
git pull
npm ci
npm run build
systemctl restart blitzkrieg
```

For Docker:

```
docker compose pull
docker compose up -d --build
```

## Monitoring and health checks

- `GET /health` returns gateway status.
- `blitzkrieg doctor` runs local checks for config and channel health.

## Backups

The SQLite DB is stored at `$BLITZKRIEG_STATE_DIR/blitzkrieg.db` (defaults to
`~/.blitzkrieg/blitzkrieg.db`). Backups are written to
`$BLITZKRIEG_STATE_DIR/backups` (see `BLITZKRIEG_DB_BACKUP_*` in `.env.example`).

## Server Security Hardening

Blitzkrieg includes a built-in server hardening CLI for production Linux servers.

### Quick Start

```bash
# Preview what will be changed (safe)
blitzkrieg secure --dry-run

# Apply all hardening interactively
sudo blitzkrieg secure

# Non-interactive mode
sudo blitzkrieg secure --yes
```

### What it does

| Component | Hardening Applied |
|-----------|-------------------|
| **SSH** | Disable password auth, disable root login, limit auth attempts |
| **Firewall** | Configure ufw with minimal open ports (SSH + your app) |
| **fail2ban** | Protect against brute-force attacks |
| **Auto-updates** | Enable automatic security patches (unattended-upgrades) |
| **Kernel** | Apply sysctl security settings |

### Security Audit

Run an audit without making changes:

```bash
blitzkrieg secure audit
```

This checks:
- SSH password authentication status
- SSH root login status
- Firewall status
- fail2ban status
- Auto-update configuration
- Open ports

### Options

```
--dry-run, -n      Preview changes without applying
--yes, -y          Skip confirmation prompts
--ssh-port=PORT    Set SSH port (default: 22)
--skip-firewall    Skip firewall setup
--skip-fail2ban    Skip fail2ban setup
--skip-ssh         Skip SSH hardening
--skip-updates     Skip auto-updates setup
--skip-kernel      Skip kernel hardening
```

### Important Notes

1. **Test SSH access** - Open a new terminal and verify you can connect before closing your current session
2. **Backup SSH keys** - Ensure you have SSH key access before disabling password auth
3. **Custom ports** - If using a custom SSH port, update your connection commands:
   ```bash
   ssh -p 2222 user@server
   ```

See [SECURITY_AUDIT.md](./SECURITY_AUDIT.md) for full security documentation.
