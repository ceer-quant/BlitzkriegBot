#!/usr/bin/env node
/**
 * Demo GIF capture script
 * Uses Puppeteer to create an animated demo GIF
 */

const puppeteer = require('puppeteer');
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const ASSETS_DIR = path.join(__dirname, '..', 'assets');
const FRAMES_DIR = path.join(ASSETS_DIR, 'frames');

// Ensure directories exist
if (!fs.existsSync(FRAMES_DIR)) {
  fs.mkdirSync(FRAMES_DIR, { recursive: true });
}

// Clean old frames
fs.readdirSync(FRAMES_DIR).forEach(f => fs.unlinkSync(path.join(FRAMES_DIR, f)));

const conversation = [
  { type: 'user', text: 'search bitcoin polymarket' },
  { type: 'bot', text: `<span style="color:#22d3ee">🔍 Found 4 markets:</span>
<pre>
<span style="color:#fbbf24">1. BTC $150k by June 2026</span>
   YES: <span style="color:#4ade80">38¢</span>  NO: <span style="color:#f87171">62¢</span>  Vol: $4.2M

<span style="color:#fbbf24">2. BTC $200k by Dec 2026</span>
   YES: <span style="color:#4ade80">22¢</span>  NO: <span style="color:#f87171">78¢</span>  Vol: $1.8M

<span style="color:#fbbf24">3. BTC flips gold market cap</span>
   YES: <span style="color:#4ade80">8¢</span>  NO: <span style="color:#f87171">92¢</span>  Vol: $620K
</pre>` },
  { type: 'user', text: 'find arbitrage' },
  { type: 'bot', text: `<span style="color:#22d3ee">⚡ Scanning 9 platforms...</span>
<pre>
<span style="color:#4ade80">Found 3 arbitrage opportunities:</span>

<span style="color:#22d3ee">1. Fed Rate Cut March</span>
   Polymarket: <span style="color:#4ade80">42¢</span> | Kalshi: <span style="color:#f87171">55¢</span>
   <span style="color:#fbbf24">→ 3.0% arb • Kelly: 6.1%</span>

<span style="color:#22d3ee">2. BTC $150k June</span>
   Betfair: <span style="color:#4ade80">35¢</span> | Polymarket: <span style="color:#f87171">61¢</span>
   <span style="color:#fbbf24">→ 4.0% arb • Kelly: 8.2%</span>
</pre>` },
  { type: 'user', text: 'buy 500 YES btc 150k polymarket' },
  { type: 'bot', text: `<span style="color:#22d3ee">🔄 Executing on Polymarket...</span>
<pre>
<span style="color:#4ade80">✅ Order filled!</span>
   500 YES @ $0.38 • Cost: $190
   Max payout: $500 • EV: +$120
   <span style="color:#94a3b8">💾 Trade logged to database</span>
</pre>` },
  { type: 'user', text: 'long ETH 25x $500 hyperliquid' },
  { type: 'bot', text: `<span style="color:#22d3ee">📈 Opening leveraged position...</span>
<pre>
<span style="color:#4ade80">✅ Position opened on Hyperliquid!</span>

   Long ETH-PERP @ $3,420
   Size: $500 × 25x = $12,500 notional
   Liq: $3,280 | TP: $3,600 | SL: $3,350
   <span style="color:#94a3b8">💾 Logged • Strategy: momentum_breakout</span>
</pre>` },
  { type: 'user', text: 'swap 200 USDC to SOL jupiter' },
  { type: 'bot', text: `<span style="color:#22d3ee">🔄 Routing via Jupiter...</span>
<pre>
<span style="color:#4ade80">✅ Swap complete!</span>
   200 USDC → 1.04 SOL @ $192.30
   Slippage: 0.05% • Route: USDC→SOL
   <span style="color:#94a3b8">🛡️ MEV protected via Jito bundle</span>
</pre>` },
  { type: 'user', text: 'show whales' },
  { type: 'bot', text: `<span style="color:#22d3ee">🐋 Top Whales (24h)</span>
<pre>
<span style="color:#fbbf24">1. 0x7c22...d795</span> <span style="color:#4ade80">+$52,400</span>
   72% win rate • 156 trades
   Latest: 8000 YES "ETH ETF" @ 0.62

<span style="color:#fbbf24">2. Theo</span> <span style="color:#4ade80">+$31,200</span>
   68% win rate • 89 trades
</pre>` },
  { type: 'user', text: 'copy 0x7c22 10%' },
  { type: 'bot', text: `<span style="color:#4ade80">✅ Copy trading enabled!</span>
<pre>
Tracking: 0x7c22...d795
Size: 10% of positions • Max: $100/trade
<span style="color:#94a3b8">Auto-mirroring trades with 2s delay</span>
</pre>` },
  { type: 'user', text: 'alert me when btc 150k hits 50c' },
  { type: 'bot', text: `<span style="color:#4ade80">🔔 Alert created!</span>
<pre>
Market: BTC $150k by June
Trigger: YES price ≥ $0.50
Channel: Telegram + WebChat
<span style="color:#94a3b8">You'll be notified instantly</span>
</pre>` },
  { type: 'user', text: 'backtest momentum strategy btc' },
  { type: 'bot', text: `<span style="color:#22d3ee">📊 Backtesting momentum_breakout...</span>
<pre>
Period: 90 days • 847 simulated trades

<span style="color:#4ade80">Results:</span>
   Win rate: 62.4% • Profit factor: 1.84
   Sharpe: 2.1 • Max drawdown: -8.2%
   Total return: <span style="color:#4ade80">+34.6%</span>

<span style="color:#94a3b8">💾 Results saved to strategy_backtests</span>
</pre>` },
  { type: 'user', text: 'compare with mean reversion' },
  { type: 'bot', text: `<span style="color:#22d3ee">🔬 A/B Strategy Comparison</span>
<pre>
              │ Momentum │ Mean Rev
──────────────┼──────────┼──────────
Win Rate      │   62.4%  │   58.1%
Sharpe Ratio  │    2.1   │    1.7
Max Drawdown  │   -8.2%  │  -12.4%
Total Return  │  <span style="color:#4ade80">+34.6%</span>  │  <span style="color:#4ade80">+28.2%</span>

<span style="color:#fbbf24">→ Momentum outperforms by +6.4%</span>
<span style="color:#94a3b8">💾 Comparison saved to ab_tests table</span>
</pre>` },
  { type: 'user', text: 'portfolio' },
  { type: 'bot', text: `<span style="color:#22d3ee">📊 Portfolio Summary</span>
<pre>
┌────────────────────────────────────────┐
│ Position        │ Side  │ Entry │ P&L │
├────────────────────────────────────────┤
│ <span style="color:#22d3ee">BTC $150k Jun</span>   │500 YES│ $0.38 │<span style="color:#4ade80">+$35</span>│
│ <span style="color:#22d3ee">ETH-PERP 25x</span>    │ Long  │$3,420 │<span style="color:#4ade80">+$68</span>│
│ <span style="color:#22d3ee">SOL</span>             │ 1.04  │$192.30│<span style="color:#4ade80">+$12</span>│
├────────────────────────────────────────┤
│ <span style="color:#fbbf24">Total: +$115 (+8.4%)</span> Sharpe: 2.3   │
│ <span style="color:#94a3b8">Alerts: 1 • Copying: 1 wallet</span>       │
│ <span style="color:#94a3b8">💾 All trades logged to SQLite</span>      │
└────────────────────────────────────────┘
</pre>` },
];

function generateHTML(messages) {
  // Match landing page dark theme with cyan accents
  return `
<!DOCTYPE html>
<html>
<head>
  <title>Blitzkrieg WebChat</title>
  <style>
    * { box-sizing: border-box; }
    body {
      font-family: system-ui, -apple-system, sans-serif;
      max-width: 650px;
      margin: 0 auto;
      padding: 16px;
      background: linear-gradient(180deg, #0f172a 0%, #1e293b 50%, #0f172a 100%);
      min-height: 100%;
      color: #e2e8f0;
    }
    .header {
      display: flex;
      align-items: center;
      gap: 12px;
      margin-bottom: 16px;
      padding: 14px;
      background: rgba(0,0,0,0.3);
      border-radius: 12px;
      border: 1px solid #334155;
    }
    .header img { width: 40px; height: 40px; border-radius: 10px; }
    .header h1 {
      margin: 0;
      font-size: 20px;
      background: linear-gradient(180deg, #fff 0%, #22d3ee 100%);
      -webkit-background-clip: text;
      -webkit-text-fill-color: transparent;
    }
    #messages {
      height: 500px;
      overflow-y: auto;
      border: 1px solid #334155;
      padding: 12px;
      margin-bottom: 12px;
      background: rgba(30, 41, 59, 0.5);
      border-radius: 12px;
      display: flex;
      flex-direction: column;
    }
    .msg { margin: 10px 0; padding: 12px 16px; border-radius: 12px; line-height: 1.5; }
    .user {
      background: linear-gradient(135deg, #0891b2 0%, #06b6d4 100%);
      color: #0f172a;
      text-align: right;
      margin-left: 20%;
      font-weight: 500;
    }
    .bot {
      background: #334155;
      color: #e2e8f0;
      margin-right: 10%;
      border: 1px solid #475569;
    }
    .bot pre {
      margin: 8px 0 0 0;
      font-family: 'SF Mono', Monaco, monospace;
      font-size: 11px;
      white-space: pre-wrap;
      line-height: 1.5;
      color: #cbd5e1;
    }
    .system {
      background: rgba(34, 211, 238, 0.1);
      border: 1px solid rgba(34, 211, 238, 0.3);
      color: #22d3ee;
      font-size: 0.85em;
      text-align: center;
    }
    #input-area { display: flex; gap: 10px; }
    #input {
      flex: 1;
      padding: 14px 18px;
      border: 1px solid #334155;
      border-radius: 12px;
      font-size: 14px;
      background: #1e293b;
      color: #e2e8f0;
    }
    #input::placeholder { color: #64748b; }
    button {
      padding: 14px 28px;
      background: linear-gradient(135deg, #06b6d4 0%, #0891b2 100%);
      color: #0f172a;
      border: none;
      border-radius: 12px;
      font-weight: 600;
      cursor: pointer;
    }
  </style>
</head>
<body>
  <div class="header">
    <h1>Blitzkrieg WebChat</h1>
  </div>
  <div id="messages">
    <div class="msg system">Connected. Ready!</div>
    ${messages.map(m => `<div class="msg ${m.type}">${m.text}</div>`).join('\n')}
  </div>
  <div id="input-area">
    <input type="text" id="input" placeholder="Ask about prediction markets..." />
    <button>Send</button>
  </div>
  <script>
    document.getElementById('messages').scrollTop = document.getElementById('messages').scrollHeight;
  </script>
</body>
</html>`;
}

async function captureDemo() {
  console.log('Launching browser...');
  const browser = await puppeteer.launch({
    headless: 'new',
    args: ['--no-sandbox', '--disable-setuid-sandbox']
  });

  try {
    const page = await browser.newPage();
    await page.setViewport({ width: 700, height: 700 });

    let frameNum = 0;
    const captureFrame = async (duration = 1) => {
      for (let i = 0; i < duration; i++) {
        const framePath = path.join(FRAMES_DIR, `frame_${String(frameNum++).padStart(4, '0')}.png`);
        await page.screenshot({ path: framePath, type: 'png' });
      }
    };

    // Start with empty chat
    console.log('Capturing frames...');
    await page.setContent(generateHTML([]));
    await captureFrame(10); // Pause at start

    // Add messages one by one
    const visibleMessages = [];
    for (const msg of conversation) {
      visibleMessages.push(msg);
      await page.setContent(generateHTML(visibleMessages));

      if (msg.type === 'user') {
        await captureFrame(8); // Short pause for user message
      } else {
        await captureFrame(25); // Longer pause to read bot response
      }
    }

    // Final pause
    await captureFrame(20);

    console.log(`Captured ${frameNum} frames`);

    // Generate GIF with ffmpeg
    console.log('Generating GIF...');
    const gifPath = path.join(ASSETS_DIR, 'demo.gif');

    try {
      execSync(`ffmpeg -y -framerate 10 -i "${FRAMES_DIR}/frame_%04d.png" -vf "fps=10,scale=700:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=256[p];[s1][p]paletteuse=dither=floyd_steinberg" -loop 0 "${gifPath}"`, {
        stdio: 'inherit'
      });
      console.log('✅ Demo GIF created:', gifPath);

      // Copy to docs public
      const docsGif = path.join(__dirname, '..', 'apps', 'docs', 'public', 'demo.gif');
      fs.copyFileSync(gifPath, docsGif);
      console.log('✅ Copied to:', docsGif);

      // Show file size
      const stats = fs.statSync(gifPath);
      console.log(`   Size: ${(stats.size / 1024).toFixed(1)} KB`);
    } catch (e) {
      console.error('ffmpeg failed:', e.message);
      console.log('Try: brew install ffmpeg');
    }

  } finally {
    await browser.close();
  }
}

captureDemo().catch(console.error);
