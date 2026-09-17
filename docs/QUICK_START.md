# Quick Start

Get Blitzkrieg running in 2 commands.

## Install & Setup

```bash
npm install -g blitzkrieg
blitzkrieg onboard
```

The setup wizard will:
1. Ask for your [Anthropic API key](https://console.anthropic.com)
2. Let you pick a messaging channel (WebChat, Telegram, Discord, or Slack)
3. Write your config to `~/.blitzkrieg/`
4. Offer to start the gateway immediately

Once running, open **http://localhost:51888/panel** in your browser.

## Try It Out

Ask anything:
- "What markets are trending on Polymarket?"
- "Show me my portfolio"
- "Find arbitrage opportunities"

## Verify Setup

```bash
blitzkrieg doctor       # Full system diagnostics
blitzkrieg creds test   # Check credentials are working
```

## From Source (Alternative)

If you prefer to build from source:

```bash
git clone https://github.com/ceer-quant/BlitzkriegBot.git
cd BlitzkriegBot
npm install
cp .env.example .env
# Add ANTHROPIC_API_KEY to .env
npm run build
npm start
```

## Common Issues

### "ANTHROPIC_API_KEY not set"
Run `blitzkrieg onboard` again — it will prompt for your key and save it to `~/.blitzkrieg/.env`.

### "Port 51888 is in use"
Another instance is running. Kill it or change the port:
```bash
lsof -i :51888 | grep LISTEN | awk '{print $2}' | xargs kill

# Or change port
blitzkrieg config set gateway.port 18790
```

### Telegram bot not responding
1. Make sure you're messaging your bot directly (not a group)
2. Run `blitzkrieg doctor` to check connectivity
3. If using pairing mode, approve access first

## Next Steps

- **Add channels**: Run `blitzkrieg onboard` again to add more messaging platforms
- **Trading**: See [TRADING.md](TRADING.md) to connect trading accounts
- **Arbitrage**: See [OPPORTUNITY_FINDER.md](OPPORTUNITY_FINDER.md) for cross-platform arbitrage
- **All commands**: See [USER_GUIDE.md](USER_GUIDE.md) for the full CLI and chat reference

## Need Help?

```bash
blitzkrieg doctor                  # Full diagnostics
blitzkrieg creds test polymarket   # Test specific credentials
```

Report issues: https://github.com/ceer-quant/BlitzkriegBot/issues
