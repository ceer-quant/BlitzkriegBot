/**
 * HTTP + WebSocket server
 */

import express, { Request, Router } from 'express';
import { WebSocketServer, WebSocket } from 'ws';
import { createServer as createHttpServer, Server, IncomingMessage } from 'http';
import { join } from 'path';
import { readFileSync, writeFileSync, existsSync } from 'fs';
import { logger } from '../utils/logger';
import type { Config, Session } from '../types';
import type { WebhookManager } from '../automation/webhooks';
import { createWebhookMiddleware } from '../automation/webhooks';
import { createX402Server, type X402Middleware } from '../payments/x402';
import {
  runHealthCheck,
  getErrorStats,
  getRequestMetrics,
  type HealthStatus,
} from '../utils/production';
import type { TickStreamer } from '../services/tick-streamer';
import type { FeatureEngineering } from '../services/feature-engineering';

export interface GatewayServer {
  start(): Promise<void>;
  stop(): Promise<void>;
  getWebSocketServer(): WebSocketServer | null;
  setChannelWebhookHandler(handler: ChannelWebhookHandler | null): void;
  setMarketIndexHandler(handler: MarketIndexHandler | null): void;
  setMarketIndexStatsHandler(handler: MarketIndexStatsHandler | null): void;
  setMarketIndexSyncHandler(handler: MarketIndexSyncHandler | null): void;
  setPerformanceDashboardHandler(handler: PerformanceDashboardHandler | null): void;
  setBacktestHandler(handler: BacktestHandler | null): void;
  setTicksHandler(handler: TicksHandler | null): void;
  setOHLCHandler(handler: OHLCHandler | null): void;
  setOrderbookHistoryHandler(handler: OrderbookHistoryHandler | null): void;
  setTickRecorderStatsHandler(handler: TickRecorderStatsHandler | null): void;
  setTickStreamer(streamer: TickStreamer | null): void;
  setFeatureEngineering(service: FeatureEngineering | null): void;
  setBittensorRouter(router: Router | null): void;
  setTradingApiRouter(router: Router | null): void;
  setPercolatorRouter(router: Router | null): void;
  setShieldRouter(router: Router | null): void;
  setAuditRouter(router: Router | null): void;
  setDCARouter(router: Router | null): void;
  setTwapRouter(router: Router | null): void;
  setBracketRouter(router: Router | null): void;
  setTriggerRouter(router: Router | null): void;
  setCopyTradingRouter(router: Router | null): void;
  setOpportunityRouter(router: Router | null): void;
  setWhaleRouter(router: Router | null): void;
  setRiskRouter(router: Router | null): void;
  setRoutingRouter(router: Router | null): void;
  setFeedsRouter(router: Router | null): void;
  setMonitoringRouter(router: Router | null): void;
  setAltDataRouter(router: Router | null): void;
  setAlertsRouter(router: Router | null): void;
  setQueueRouter(router: Router | null): void;
  setWebhooksRouter(router: Router | null): void;
  setPaymentsRouter(router: Router | null): void;
  setEmbeddingsRouter(router: Router | null): void;
  setCronRouter(router: Router | null): void;
  setLaunchRouter(router: Router | null): void;
  setCommandListHandler(handler: CommandListHandler | null): void;
  setHooksHandler(handler: HooksHandler | null): void;
  setOnSessionDelete(handler: ((key: string) => void) | null): void;
  setChatConnectionHandler(handler: ((ws: WebSocket, req: IncomingMessage) => void) | null): void;
}

/** Handler for /hooks/wake and /hooks/agent endpoints */
export type HooksHandler = {
  wake(text?: string): void;
  agentTurn(message: string, options?: { deliver?: boolean; channel?: string }): Promise<string>;
};

export type CommandListHandler = () => Array<{ name: string; description: string; category: string; subcommands?: Array<{ name: string; description: string; category: string }> }>;

export type ChannelWebhookHandler = (
  platform: string,
  event: unknown,
  req: Request
) => Promise<unknown>;

export type MarketIndexHandler = (
  req: Request
) => Promise<{ results: unknown[] } | { error: string; status?: number }>;

export type MarketIndexStatsHandler = (
  req: Request
) => Promise<{ stats: unknown } | { error: string; status?: number }>;

export type MarketIndexSyncHandler = (
  req: Request
) => Promise<{ result: unknown } | { error: string; status?: number }>;

export type BacktestHandler = (
  req: Request
) => Promise<{
  result: {
    strategyId: string;
    metrics: {
      totalReturnPct: number;
      annualizedReturnPct: number;
      totalTrades: number;
      winRate: number;
      sharpeRatio: number;
      sortinoRatio: number;
      maxDrawdownPct: number;
      profitFactor: number;
    };
    trades: unknown[];
    equityCurve: Array<{ timestamp: string; equity: number }>;
    dailyReturns: Array<{ date: string; return: number }>;
  };
} | { error: string; status?: number }>;

export type PerformanceDashboardHandler = (
  req: Request
) => Promise<{
  stats: {
    totalTrades: number;
    winRate: number;
    totalPnl: number;
    avgPnlPct: number;
    sharpeRatio: number;
    maxDrawdown: number;
  };
  recentTrades: Array<{
    id: string;
    timestamp: string;
    market: string;
    side: string;
    size: number;
    entryPrice: number;
    exitPrice?: number;
    pnl?: number;
    pnlPct?: number;
    status: string;
  }>;
  dailyPnl: Array<{ date: string; pnl: number; cumulative: number }>;
  byStrategy: Array<{ strategy: string; trades: number; winRate: number; pnl: number }>;
} | { error: string; status?: number }>;

export type TicksHandler = (
  req: Request
) => Promise<{
  ticks: Array<{
    time: string;
    platform: string;
    marketId: string;
    outcomeId: string;
    price: number;
    prevPrice: number | null;
  }>;
} | { error: string; status?: number }>;

export type OHLCHandler = (
  req: Request
) => Promise<{
  candles: Array<{
    time: number;
    open: number;
    high: number;
    low: number;
    close: number;
    tickCount: number;
  }>;
} | { error: string; status?: number }>;

export type OrderbookHistoryHandler = (
  req: Request
) => Promise<{
  snapshots: Array<{
    time: string;
    platform: string;
    marketId: string;
    outcomeId: string;
    bids: Array<[number, number]>;
    asks: Array<[number, number]>;
    spread: number | null;
    midPrice: number | null;
  }>;
} | { error: string; status?: number }>;

export type TickRecorderStatsHandler = (
  req: Request
) => Promise<{
  stats: {
    ticksRecorded: number;
    orderbooksRecorded: number;
    ticksInBuffer: number;
    orderbooksInBuffer: number;
    lastFlushTime: number | null;
    dbConnected: boolean;
    platforms: string[];
  };
} | { error: string; status?: number }>;

export interface ServerDb {
  query: <T>(sql: string) => T[];
  getSessionById?(id: string): Session | undefined;
  createSession?(session: Session): void;
  updateSession?(session: Session): void;
  deleteSession?(key: string): void;
  listWebchatSessions?(userId: string): Array<{ id: string; title: string | undefined; updatedAt: number; messageCount: number; lastMessage: string | undefined }>;
  updateSessionTitle?(key: string, title: string): void;
  getSessionMessages?(sessionId: string, options?: { limit?: number; before?: number }): Array<{ id: string; role: string; content: string; timestamp: number }>;
}

export function createServer(
  config: Config['gateway'] & { x402?: Config['x402'] },
  webhooks?: WebhookManager,
  db?: ServerDb
): GatewayServer {
  const app = express();
  let httpServer: Server | null = null;
  let wss: WebSocketServer | null = null;
  let ipCleanupInterval: NodeJS.Timeout | null = null;
  let channelWebhookHandler: ChannelWebhookHandler | null = null;
  let marketIndexHandler: MarketIndexHandler | null = null;
  let marketIndexStatsHandler: MarketIndexStatsHandler | null = null;
  let marketIndexSyncHandler: MarketIndexSyncHandler | null = null;
  let performanceDashboardHandler: PerformanceDashboardHandler | null = null;
  let backtestHandler: BacktestHandler | null = null;
  let ticksHandler: TicksHandler | null = null;
  let ohlcHandler: OHLCHandler | null = null;
  let orderbookHistoryHandler: OrderbookHistoryHandler | null = null;
  let tickRecorderStatsHandler: TickRecorderStatsHandler | null = null;
  let tickStreamer: TickStreamer | null = null;
  let featureEngineering: FeatureEngineering | null = null;
  let commandListHandler: CommandListHandler | null = null;
  let hooksHandler: HooksHandler | null = null;
  let onSessionDelete: ((key: string) => void) | null = null;
  let chatConnectionHandler: ((ws: WebSocket, req: IncomingMessage) => void) | null = null;

  // Auth middleware for sensitive endpoints
  const authToken = process.env.BLITZKRIEG_TOKEN;
  const requireAuth = (req: Request, res: express.Response, next: express.NextFunction) => {
    if (!authToken) {
      // No token configured - allow access (for development)
      return next();
    }
    const providedToken = req.headers.authorization?.replace('Bearer ', '') || req.query.token;
    if (providedToken !== authToken) {
      res.status(401).json({ error: 'Unauthorized - provide valid token via Authorization header or ?token= param' });
      return;
    }
    next();
  };

  const corsConfig = config.cors ?? false;
  app.use((req, res, next) => {
    if (!corsConfig) {
      return next();
    }

    const originHeader = req.headers.origin;
    let origin = '';
    let allowCredentials = false;

    if (Array.isArray(corsConfig)) {
      // Security: Only allow specific origins from allowlist
      if (originHeader && corsConfig.includes(originHeader)) {
        origin = originHeader;
        allowCredentials = true; // Safe to allow credentials with specific origin
      }
    } else if (corsConfig === true) {
      // Security: Wildcard origin - do NOT allow credentials
      origin = '*';
      allowCredentials = false;
    }

    if (origin) {
      res.setHeader('Access-Control-Allow-Origin', origin);
      res.setHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, DELETE, PATCH, OPTIONS');
      res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Authorization');
      // Security: Only allow credentials with specific origins, never with wildcard
      if (allowCredentials) {
        res.setHeader('Access-Control-Allow-Credentials', 'true');
      }
    }

    if (req.method === 'OPTIONS') {
      res.status(204).end();
      return;
    }

    next();
  });

  // IP-based rate limiting
  const ipRequestCounts = new Map<string, { count: number; resetAt: number }>();
  const IP_RATE_LIMIT = parseInt(process.env.BLITZKRIEG_IP_RATE_LIMIT || '100', 10); // requests per minute
  const IP_RATE_WINDOW_MS = 60 * 1000; // 1 minute

  app.use((req, res, next) => {
    // Skip rate limiting for health checks
    if (req.path === '/health') return next();

    const ip = req.ip || req.socket.remoteAddress || 'unknown';
    const now = Date.now();
    let record = ipRequestCounts.get(ip);

    if (!record || now > record.resetAt) {
      record = { count: 0, resetAt: now + IP_RATE_WINDOW_MS };
      ipRequestCounts.set(ip, record);
    }

    record.count++;

    // Set rate limit headers
    res.setHeader('X-RateLimit-Limit', IP_RATE_LIMIT);
    res.setHeader('X-RateLimit-Remaining', Math.max(0, IP_RATE_LIMIT - record.count));
    res.setHeader('X-RateLimit-Reset', Math.ceil(record.resetAt / 1000));

    if (record.count > IP_RATE_LIMIT) {
      logger.warn({ ip, count: record.count }, 'Rate limit exceeded');
      res.status(429).json({
        error: 'Too many requests',
        retryAfter: Math.ceil((record.resetAt - now) / 1000),
      });
      return;
    }

    next();
  });

  // Cleanup old IP records every 5 minutes
  ipCleanupInterval = setInterval(() => {
    const now = Date.now();
    for (const [ip, record] of ipRequestCounts) {
      if (now > record.resetAt + IP_RATE_WINDOW_MS) {
        ipRequestCounts.delete(ip);
      }
    }
  }, 5 * 60 * 1000);

  // HTTPS enforcement & security headers
  app.use((req, res, next) => {
    // HSTS header (only send over HTTPS or if explicitly enabled)
    const hstsEnabled = process.env.BLITZKRIEG_HSTS_ENABLED === 'true';
    const isSecure = req.secure || req.headers['x-forwarded-proto'] === 'https';

    if (hstsEnabled || isSecure) {
      // 1 year HSTS with includeSubDomains
      res.setHeader('Strict-Transport-Security', 'max-age=31536000; includeSubDomains');
    }

    // Additional security headers
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('X-Frame-Options', 'SAMEORIGIN');
    res.setHeader('X-XSS-Protection', '1; mode=block');

    // Redirect HTTP to HTTPS if forced
    const forceHttps = process.env.BLITZKRIEG_FORCE_HTTPS === 'true';
    if (forceHttps && !isSecure) {
      const allowedHosts = new Set([
        process.env.BLITZKRIEG_PUBLIC_HOST || 'localhost',
        'localhost',
        '127.0.0.1',
      ].filter(Boolean));
      const host = req.headers.host?.split(':')[0] || 'localhost';
      if (!allowedHosts.has(host)) {
        res.status(400).send('Invalid host');
        return;
      }
      res.redirect(301, `https://${req.headers.host}${req.url}`);
      return;
    }

    next();
  });

  app.use(express.json({
    limit: '1mb',
    verify: (req, _res, buf) => {
      // Capture raw body for webhook signature verification
      (req as any).rawBody = buf.toString();
    },
  }));

  // x402 payment middleware for premium endpoints
  let x402: X402Middleware | null = null;
  if (config.x402?.server?.payToAddress) {
    x402 = createX402Server(
      {
        payToAddress: config.x402.server.payToAddress,
        network: config.x402.server.network || 'solana',
        facilitatorUrl: config.x402.facilitatorUrl,
      },
      {
        'POST /api/compute': { priceUsd: 0.01, description: 'Compute request' },
        'POST /api/backtest': { priceUsd: 0.05, description: 'Strategy backtest' },
        'GET /api/features': { priceUsd: 0.002, description: 'Feature snapshot' },
        'POST /api/launch/token': { priceUsd: 1.00, description: 'Token launch' },
        'POST /api/launch/swap': { priceUsd: 0.10, description: 'Bonding curve swap' },
        'POST /api/launch/claim-fees': { priceUsd: 0.10, description: 'Claim creator fees' },
      }
    );
    logger.info({ network: config.x402.server.network || 'solana' }, 'x402 payment middleware enabled');

    // Apply x402 middleware to premium routes
    app.use(['/api/compute', '/api/backtest', '/api/features', '/api/launch/token', '/api/launch/swap', '/api/launch/claim-fees'], x402.middleware);
  }

  // Health check endpoint (enhanced for production)
  app.get('/health', async (req, res) => {
    const deep = req.query.deep === 'true';

    if (!db) {
      // Simple health check if no DB provided
      res.json({ status: 'healthy', timestamp: Date.now() });
      return;
    }

    try {
      const health: HealthStatus = await runHealthCheck(db, {
        checkExternalApis: deep,
      });

      const httpStatus = health.status === 'healthy' ? 200 :
                         health.status === 'degraded' ? 200 : 503;

      res.status(httpStatus).json(health);
    } catch (err) {
      logger.error({ err }, 'Health check failed');
      res.status(503).json({
        status: 'unhealthy',
        timestamp: Date.now(),
        error: 'Health check failed',
      });
    }
  });

  // Simple wallet balance endpoint for HFT panel
  app.get('/api/trading/balance', async (_req, res) => {
    try {
      const address = process.env.POLYMARKET_FUNDER_ADDRESS || '';

      // Prefer the Rust SDK's authenticated balance (works for Poly1271 / deposit
      // wallets, where the plain HMAC /balance endpoint fails).
      try {
        const { rustBalance } = await import('../execution/rust-clob-executor.js');
        const r = await rustBalance();
        if (r.success && typeof r.balance === 'number') {
          res.json({ address, usdcBalance: r.balance.toFixed(2) });
          return;
        }
      } catch { /* fall through to the HMAC method below */ }

      const apiKey = process.env.POLYMARKET_API_KEY || '';
      const apiSecret = process.env.POLYMARKET_API_SECRET || '';
      const apiPassphrase = process.env.POLYMARKET_API_PASSPHRASE || '';
      
      if (!address || !apiKey) {
        res.json({ address: '', usdcBalance: '0', error: 'Not configured' });
        return;
      }

      // Get balance from CLOB API with HMAC auth
      const crypto = await import('crypto');
      const timestamp = Math.floor(Date.now() / 1000).toString();
      const path = '/balance';
      const body = '';
      const hmac = crypto.createHmac('sha256', Buffer.from(apiSecret, 'base64'));
      hmac.update(timestamp + 'GET' + path + body);
      const signature = hmac.digest('base64');

      const headers = {
        POLY_ADDRESS: address,
        POLY_API_KEY: apiKey,
        POLY_PASSPHRASE: apiPassphrase,
        POLY_TIMESTAMP: timestamp,
        POLY_SIGNATURE: signature,
      };

      const response = await fetch(`https://clob.polymarket.com${path}`, { headers });
      if (!response.ok) {
        res.json({ address, usdcBalance: '0', error: `HTTP ${response.status}` });
        return;
      }

      const data = await response.json() as { balance?: string; allowance?: string };
      res.json({
        address,
        usdcBalance: data.balance || '0',
        allowance: data.allowance || '0',
      });
    } catch (err) {
      res.json({ address: '', usdcBalance: '0', error: 'Failed to fetch balance' });
    }
  });

  // Metrics endpoint (for monitoring) - requires auth if BLITZKRIEG_TOKEN is set
  app.get('/metrics', requireAuth, (_req, res) => {
    const requestMetrics = getRequestMetrics();
    const errorStats = getErrorStats();
    const memUsage = process.memoryUsage();

    res.json({
      timestamp: Date.now(),
      requests: requestMetrics,
      errors: {
        recentCount: errorStats.recentCount,
        topErrors: errorStats.topErrors,
      },
      memory: {
        heapUsedMB: Math.round(memUsage.heapUsed / 1024 / 1024),
        heapTotalMB: Math.round(memUsage.heapTotal / 1024 / 1024),
        rssMB: Math.round(memUsage.rss / 1024 / 1024),
      },
    });
  });

  // x402 payment stats endpoint
  app.get('/api/x402/stats', requireAuth, (_req, res) => {
    if (!x402) {
      res.json({ enabled: false });
      return;
    }
    res.json({ enabled: true, ...x402.getStats() });
  });

  // API info endpoint
  app.get('/', (_req, res) => {
    res.json({
      name: 'blitzkrieg',
      version: process.env.npm_package_version || '0.1.0',
      description: 'BlitzkriegBot trading core gateway',
      endpoints: {
        websocket: '/ws',
        panel: '/panel',
        tickStream: '/api/ticks/stream',
        health: '/health',
        healthDeep: '/health?deep=true',
        metrics: '/metrics',
        dashboard: '/dashboard',
        tickStreamerStats: '/api/tick-streamer/stats',
        features: '/api/features/:platform/:marketId',
        featuresAll: '/api/features',
        featuresStats: '/api/features/stats',
      },
    });
  });

  // Command list for slash command palette
  app.get('/api/commands', (_req, res) => {
    if (!commandListHandler) {
      res.json({ commands: [] });
      return;
    }
    res.json({ commands: commandListHandler() });
  });

  // ── Environment Setup API ──

  const ENV_VAR_SCHEMA = [
    {
      category: 'Core',
      vars: [
        { key: 'ANTHROPIC_API_KEY', label: 'Anthropic API Key', secret: true, required: true, helpUrl: 'https://console.anthropic.com' },
        { key: 'BLITZKRIEG_LOCALE', label: 'Language (en, es, zh, ja, ko...)', secret: false, required: false },
      ],
    },
    {
      category: 'Channels',
      vars: [
        { key: 'TELEGRAM_BOT_TOKEN', label: 'Telegram Bot Token', secret: true, required: false, helpUrl: 'https://t.me/BotFather' },
        { key: 'DISCORD_BOT_TOKEN', label: 'Discord Bot Token', secret: true, required: false, helpUrl: 'https://discord.com/developers/applications' },
        { key: 'DISCORD_APP_ID', label: 'Discord App ID', secret: false, required: false },
        { key: 'SLACK_BOT_TOKEN', label: 'Slack Bot Token', secret: true, required: false, helpUrl: 'https://api.slack.com/apps' },
        { key: 'SLACK_APP_TOKEN', label: 'Slack App Token', secret: true, required: false },
        { key: 'PANEL_CHAT_TOKEN', label: 'Panel Auth Token', secret: true, required: false },
        { key: 'MATRIX_HOMESERVER_URL', label: 'Matrix Homeserver URL', secret: false, required: false },
        { key: 'MATRIX_ACCESS_TOKEN', label: 'Matrix Access Token', secret: true, required: false },
        { key: 'MATRIX_USER_ID', label: 'Matrix User ID', secret: false, required: false },
        { key: 'LINE_CHANNEL_ACCESS_TOKEN', label: 'LINE Access Token', secret: true, required: false, helpUrl: 'https://developers.line.biz/console/' },
        { key: 'LINE_CHANNEL_SECRET', label: 'LINE Channel Secret', secret: true, required: false },
        { key: 'TEAMS_APP_ID', label: 'Teams App ID', secret: false, required: false },
        { key: 'TEAMS_APP_PASSWORD', label: 'Teams App Password', secret: true, required: false },
        { key: 'SIGNAL_PHONE_NUMBER', label: 'Signal Phone Number', secret: false, required: false },
      ],
    },
    {
      category: 'Prediction Markets',
      vars: [
        { key: 'POLY_API_KEY', label: 'Polymarket API Key', secret: true, required: false, helpUrl: 'https://polymarket.com/settings/api' },
        { key: 'POLY_API_SECRET', label: 'Polymarket API Secret', secret: true, required: false },
        { key: 'POLY_API_PASSPHRASE', label: 'Polymarket Passphrase', secret: true, required: false },
        { key: 'POLY_PRIVATE_KEY', label: 'Polymarket Private Key', secret: true, required: false },
        { key: 'POLY_FUNDER_ADDRESS', label: 'Polymarket Funder Address', secret: false, required: false },
        { key: 'KALSHI_API_KEY', label: 'Kalshi API Key', secret: true, required: false, helpUrl: 'https://kalshi.com/account/api' },
        { key: 'KALSHI_API_SECRET', label: 'Kalshi API Secret', secret: true, required: false },
        { key: 'MANIFOLD_API_KEY', label: 'Manifold API Key', secret: true, required: false, helpUrl: 'https://manifold.markets/account' },
        { key: 'METACULUS_TOKEN', label: 'Metaculus Token', secret: true, required: false },
        { key: 'BETFAIR_APP_KEY', label: 'Betfair App Key', secret: true, required: false },
        { key: 'BETFAIR_USERNAME', label: 'Betfair Username', secret: false, required: false },
        { key: 'BETFAIR_PASSWORD', label: 'Betfair Password', secret: true, required: false },
        { key: 'SMARKETS_API_TOKEN', label: 'Smarkets API Token', secret: true, required: false },
        { key: 'OPINION_API_KEY', label: 'Opinion API Key', secret: true, required: false },
        { key: 'DRIFT_PRIVATE_KEY', label: 'Drift Private Key', secret: true, required: false },
        { key: 'DRIFT_GATEWAY_URL', label: 'Drift Gateway URL', secret: false, required: false },
      ],
    },
    {
      category: 'Crypto Exchanges',
      vars: [
        { key: 'BINANCE_API_KEY', label: 'Binance API Key', secret: true, required: false },
        { key: 'BINANCE_API_SECRET', label: 'Binance API Secret', secret: true, required: false },
        { key: 'BINANCE_FUTURES_KEY', label: 'Binance Futures Key', secret: true, required: false },
        { key: 'BINANCE_FUTURES_SECRET', label: 'Binance Futures Secret', secret: true, required: false },
        { key: 'BYBIT_API_KEY', label: 'Bybit API Key', secret: true, required: false },
        { key: 'BYBIT_API_SECRET', label: 'Bybit API Secret', secret: true, required: false },
        { key: 'MEXC_API_KEY', label: 'MEXC API Key', secret: true, required: false },
        { key: 'MEXC_API_SECRET', label: 'MEXC API Secret', secret: true, required: false },
        { key: 'HYPERLIQUID_PRIVATE_KEY', label: 'Hyperliquid Private Key', secret: true, required: false },
        { key: 'HYPERLIQUID_WALLET', label: 'Hyperliquid Wallet', secret: false, required: false },
      ],
    },
    {
      category: 'Blockchain RPCs',
      vars: [
        { key: 'SOLANA_RPC_URL', label: 'Solana RPC URL', secret: false, required: false },
        { key: 'SOLANA_PRIVATE_KEY', label: 'Solana Private Key', secret: true, required: false },
        { key: 'ETH_RPC_URL', label: 'Ethereum RPC URL', secret: false, required: false },
        { key: 'EVM_PRIVATE_KEY', label: 'EVM Private Key', secret: true, required: false },
        { key: 'BASE_RPC_URL', label: 'Base RPC URL', secret: false, required: false },
        { key: 'ARBITRUM_RPC_URL', label: 'Arbitrum RPC URL', secret: false, required: false },
        { key: 'OPTIMISM_RPC_URL', label: 'Optimism RPC URL', secret: false, required: false },
        { key: 'POLYGON_RPC_URL', label: 'Polygon RPC URL', secret: false, required: false },
        { key: 'BSC_RPC_URL', label: 'BSC RPC URL', secret: false, required: false },
        { key: 'AVALANCHE_RPC_URL', label: 'Avalanche RPC URL', secret: false, required: false },
      ],
    },
    {
      category: 'AI Providers',
      vars: [
        { key: 'OPENAI_API_KEY', label: 'OpenAI API Key', secret: true, required: false },
        { key: 'GEMINI_API_KEY', label: 'Gemini API Key', secret: true, required: false },
        { key: 'GROQ_API_KEY', label: 'Groq API Key', secret: true, required: false },
        { key: 'FIREWORKS_API_KEY', label: 'Fireworks API Key', secret: true, required: false },
        { key: 'TOGETHER_API_KEY', label: 'Together API Key', secret: true, required: false },
        { key: 'ELEVENLABS_API_KEY', label: 'ElevenLabs API Key', secret: true, required: false },
        { key: 'VOYAGE_API_KEY', label: 'Voyage API Key', secret: true, required: false },
      ],
    },
    {
      category: 'External Services',
      vars: [
        { key: 'TWITTER_BEARER_TOKEN', label: 'Twitter Bearer Token', secret: true, required: false },
        { key: 'BRAVE_SEARCH_API_KEY', label: 'Brave Search API Key', secret: true, required: false },
        { key: 'ODDS_API_KEY', label: 'Odds API Key', secret: true, required: false },
        { key: 'NEYNAR_API_KEY', label: 'Neynar (Farcaster) API Key', secret: true, required: false },
        { key: 'SMTP_HOST', label: 'SMTP Host', secret: false, required: false },
        { key: 'SMTP_PORT', label: 'SMTP Port', secret: false, required: false },
        { key: 'SMTP_USER', label: 'SMTP User', secret: false, required: false },
        { key: 'SMTP_PASS', label: 'SMTP Password', secret: true, required: false },
      ],
    },
    {
      category: 'Gateway & Security',
      vars: [
        { key: 'BLITZKRIEG_TOKEN', label: 'Gateway Auth Token', secret: true, required: false },
        { key: 'BLITZKRIEG_CREDENTIAL_KEY', label: 'Credential Encryption Key', secret: true, required: false },
        { key: 'BLITZKRIEG_PUBLIC_HOST', label: 'Public Hostname', secret: false, required: false },
        { key: 'BLITZKRIEG_PUBLIC_SCHEME', label: 'Public Scheme (http/https)', secret: false, required: false },
        { key: 'LOG_LEVEL', label: 'Log Level (debug/info/warn/error)', secret: false, required: false },
        { key: 'BLITZKRIEG_IP_RATE_LIMIT', label: 'IP Rate Limit (req/min)', secret: false, required: false },
        { key: 'BLITZKRIEG_FORCE_HTTPS', label: 'Force HTTPS', secret: false, required: false },
      ],
    },
  ];

  const RESTART_KEYS = new Set(['TELEGRAM_BOT_TOKEN', 'DISCORD_BOT_TOKEN', 'SLACK_BOT_TOKEN', 'SLACK_APP_TOKEN']);
  const ALL_SCHEMA_KEYS = new Set(ENV_VAR_SCHEMA.flatMap(c => c.vars.map(v => v.key)));

  app.get('/api/config/env', requireAuth, (_req, res) => {
    const schema = ENV_VAR_SCHEMA.map(cat => ({
      category: cat.category,
      vars: cat.vars.map(v => {
        const val = process.env[v.key] || '';
        return {
          key: v.key,
          label: v.label,
          secret: v.secret,
          required: v.required,
          helpUrl: (v as any).helpUrl,
          set: val.length > 0,
          masked: v.secret && val.length > 4
            ? '****' + val.slice(-4)
            : v.secret && val.length > 0
              ? '****'
              : val || '',
        };
      }),
    }));
    res.json({ schema });
  });

  app.post('/api/config/env', requireAuth, (req, res) => {
    const updates = req.body?.vars;
    if (!updates || typeof updates !== 'object') {
      res.status(400).json({ error: 'Missing vars object' });
      return;
    }

    let needsRestart = false;
    for (const key of Object.keys(updates)) {
      if (!ALL_SCHEMA_KEYS.has(key)) {
        res.status(400).json({ error: `Unknown env var: ${key}` });
        return;
      }
      // Reject newlines in values (prevent .env injection)
      const val = String(updates[key] || '');
      if (val.includes('\n') || val.includes('\r')) {
        res.status(400).json({ error: `Invalid value for ${key}: newlines not allowed` });
        return;
      }
      if (RESTART_KEYS.has(key)) needsRestart = true;
    }

    try {
      const envPath = join(process.cwd(), '.env');
      let lines: string[] = [];
      if (existsSync(envPath)) {
        lines = readFileSync(envPath, 'utf-8').split('\n');
      }

      const updatedKeys: string[] = [];
      for (const [key, value] of Object.entries(updates)) {
        const val = String(value || '');
        const lineIdx = lines.findIndex(l => l.startsWith(key + '='));
        if (lineIdx >= 0) {
          lines[lineIdx] = `${key}="${val}"`;
        } else {
          lines.push(`${key}="${val}"`);
        }
        process.env[key] = val;
        updatedKeys.push(key);
      }

      writeFileSync(envPath, lines.join('\n'));
      logger.info({ keys: updatedKeys }, 'Env vars updated via settings panel');
      res.json({ success: true, restartRequired: needsRestart, updated: updatedKeys });
    } catch (err) {
      logger.error({ err }, 'Failed to save env vars');
      res.status(500).json({ error: 'Failed to write .env file' });
    }
  });

  // ── Session REST API for webchat ──

  // GET /api/chat/sessions — list sessions for a user
  app.get('/api/chat/sessions', (req, res) => {
    const userId = (req.query.userId as string) || '';
    if (!userId || !db?.listWebchatSessions) {
      res.json({ sessions: [] });
      return;
    }
    try {
      const sessions = db.listWebchatSessions(userId);
      res.json({ sessions });
    } catch (error) {
      logger.error({ error }, 'Failed to list panel sessions');
      res.status(500).json({ error: 'Failed to list sessions' });
    }
  });

  // GET /api/chat/sessions/:id — load session with messages
  app.get('/api/chat/sessions/:id', (req, res) => {
    if (!db?.getSessionById) {
      res.status(404).json({ error: 'Not found' });
      return;
    }
    try {
      const session = db.getSessionById(req.params.id);
      if (!session) {
        res.status(404).json({ error: 'Session not found' });
        return;
      }

      // Read from messages table (paginated)
      const limit = Math.min(parseInt(req.query.limit as string, 10) || 500, 1000);
      const before = parseInt(req.query.before as string, 10) || undefined;
      let messages: Array<{ id: string; role: string; content: string; timestamp: number }> = [];

      if (db.getSessionMessages) {
        messages = db.getSessionMessages(session.id, { limit, before });
      } else {
        // Fallback to JSON blob if migration hasn't run yet
        messages = (session.context.conversationHistory || []).map((m: { role: string; content: string; timestamp?: number }, i: number) => ({
          id: `${session.id}-legacy-${i}`,
          role: m.role,
          content: m.content,
          timestamp: m.timestamp || session.createdAt.getTime(),
        }));
      }

      res.json({
        id: session.id,
        title: session.title,
        messages,
        updatedAt: session.updatedAt.getTime(),
      });
    } catch (error) {
      logger.error({ error }, 'Failed to get panel session');
      res.status(500).json({ error: 'Failed to get session' });
    }
  });

  // POST /api/chat/sessions — create new session
  app.post('/api/chat/sessions', (req, res) => {
    if (!db?.createSession) {
      res.status(500).json({ error: 'Database not available' });
      return;
    }
    try {
      const userId = req.body?.userId || 'web-anonymous';
      const now = new Date();
      const sessionId = crypto.randomUUID();
      const session: Session = {
        id: sessionId,
        key: `agent:main:panel:dm:${sessionId}:${userId}`,
        userId,
        channel: 'panel',
        chatId: sessionId,
        chatType: 'dm',
        context: {
          messageCount: 0,
          lastMarkets: [],
          preferences: {},
          conversationHistory: [],
        },
        history: [],
        lastActivity: now,
        createdAt: now,
        updatedAt: now,
      };
      db.createSession(session);
      res.json({
        session: {
          id: session.id,
          title: undefined,
          updatedAt: now.getTime(),
          messageCount: 0,
          lastMessage: undefined,
        },
      });
    } catch (error) {
      logger.error({ error }, 'Failed to create panel session');
      res.status(500).json({ error: 'Failed to create session' });
    }
  });

  // DELETE /api/chat/sessions/:id — delete a session
  app.delete('/api/chat/sessions/:id', (req, res) => {
    if (!db?.getSessionById || !db?.deleteSession) {
      res.status(500).json({ error: 'Database not available' });
      return;
    }
    try {
      const session = db.getSessionById(req.params.id);
      if (!session) {
        res.status(404).json({ error: 'Session not found' });
        return;
      }
      db.deleteSession(session.key);
      // Also clean up in-memory session cache
      if (onSessionDelete) {
        try { onSessionDelete(session.key); } catch { /* ignore cleanup errors */ }
      }
      res.json({ ok: true });
    } catch (error) {
      logger.error({ error }, 'Failed to delete panel session');
      res.status(500).json({ error: 'Failed to delete session' });
    }
  });

  // PATCH /api/chat/sessions/:id — rename session
  app.patch('/api/chat/sessions/:id', (req, res) => {
    if (!db?.getSessionById || !db?.updateSessionTitle) {
      res.status(500).json({ error: 'Database not available' });
      return;
    }
    try {
      const session = db.getSessionById(req.params.id);
      if (!session) {
        res.status(404).json({ error: 'Session not found' });
        return;
      }
      const title = req.body?.title;
      if (typeof title === 'string') {
        db.updateSessionTitle(session.key, title);
      }
      res.json({ ok: true });
    } catch (error) {
      logger.error({ error }, 'Failed to update panel session');
      res.status(500).json({ error: 'Failed to update session' });
    }
  });

  // ── /panel: the Blitzkrieg panel (ui/ tree; the live gateway view is ui_kit_web) ──
  app.use('/panel', express.static(join(__dirname, '../../ui'), {
    setHeaders: (res) => {
      res.setHeader('Cache-Control', 'no-cache, no-store, must-revalidate');
    },
  }));

  // ── User-layer UI (ui/) — HFT panel and dashboards, separated from Node logic ──
  app.use('/ui', express.static(join(__dirname, '../../ui'), {
    setHeaders: (res) => {
      res.setHeader('Cache-Control', 'no-cache, no-store, must-revalidate');
    },
  }));

  if (webhooks) {
    const webhookMiddleware = createWebhookMiddleware(webhooks);
    app.post('/webhook/*', webhookMiddleware);
    app.post('/webhook', webhookMiddleware);
  }

  // Hook endpoints — wake heartbeat or run agent turn
  app.post('/hooks/wake', requireAuth, (req, res) => {
    if (!hooksHandler) {
      res.status(503).json({ ok: false, error: 'Hooks not configured' });
      return;
    }
    const { text } = req.body || {};
    hooksHandler.wake(text);
    res.json({ ok: true });
  });

  app.post('/hooks/agent', requireAuth, async (req, res) => {
    if (!hooksHandler) {
      res.status(503).json({ ok: false, error: 'Hooks not configured' });
      return;
    }
    const { message, deliver, channel } = req.body || {};
    if (!message || typeof message !== 'string') {
      res.status(400).json({ ok: false, error: 'Missing "message" string in body' });
      return;
    }
    const runId = `run_${Date.now().toString(36)}`;
    // Return immediately with runId, execute async
    res.status(202).json({ ok: true, runId });
    try {
      await hooksHandler.agentTurn(message, { deliver, channel });
    } catch (error) {
      logger.error({ error, runId }, 'Hook agent turn failed');
    }
  });

  // Channel webhooks (Teams, Google Chat, etc.)
  app.post('/channels/:platform', async (req, res) => {
    if (!channelWebhookHandler) {
      res.status(404).json({ error: 'Channel webhooks not configured' });
      return;
    }

    const platform = req.params.platform;
    try {
      const result = await channelWebhookHandler(platform, req.body, req);

      if (result === null || result === undefined) {
        res.status(200).send();
        return;
      }

      if (typeof result === 'string') {
        res.json({ text: result });
        return;
      }

      res.json(result);
    } catch (error) {
      logger.error({ error, platform }, 'Channel webhook handler failed');
      res.status(500).json({ error: 'Channel webhook error' });
    }
  });

  // Market index search endpoint
  app.get('/market-index/search', async (req, res) => {
    if (!marketIndexHandler) {
      res.status(404).json({ error: 'Market index handler not configured' });
      return;
    }

    try {
      const result = await marketIndexHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Market index handler failed');
      res.status(500).json({ error: 'Market index error' });
    }
  });

  app.get('/market-index/stats', async (req, res) => {
    if (!marketIndexStatsHandler) {
      res.status(404).json({ error: 'Market index handler not configured' });
      return;
    }

    try {
      const result = await marketIndexStatsHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Market index stats handler failed');
      res.status(500).json({ error: 'Market index error' });
    }
  });

  app.post('/market-index/sync', async (req, res) => {
    if (!marketIndexSyncHandler) {
      res.status(404).json({ error: 'Market index handler not configured' });
      return;
    }

    try {
      const result = await marketIndexSyncHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Market index sync handler failed');
      res.status(500).json({ error: 'Market index error' });
    }
  });

  // Backtest API endpoint
  app.post('/api/backtest', async (req, res) => {
    if (!backtestHandler) {
      res.status(404).json({ error: 'Backtest handler not configured' });
      return;
    }

    try {
      const result = await backtestHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Backtest handler failed');
      res.status(500).json({ error: 'Backtest error' });
    }
  });

  // Performance dashboard API endpoint
  app.get('/api/performance', async (req, res) => {
    if (!performanceDashboardHandler) {
      res.status(404).json({ error: 'Performance dashboard not configured' });
      return;
    }

    try {
      const result = await performanceDashboardHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Performance dashboard handler failed');
      res.status(500).json({ error: 'Performance dashboard error' });
    }
  });

  // Tick recorder endpoints
  app.get('/api/ticks/:platform/:marketId', async (req, res) => {
    if (!ticksHandler) {
      res.status(404).json({ error: 'Tick recorder not enabled' });
      return;
    }

    try {
      const result = await ticksHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Ticks handler failed');
      res.status(500).json({ error: 'Ticks query error' });
    }
  });

  app.get('/api/ohlc/:platform/:marketId', async (req, res) => {
    if (!ohlcHandler) {
      res.status(404).json({ error: 'Tick recorder not enabled' });
      return;
    }

    try {
      const result = await ohlcHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'OHLC handler failed');
      res.status(500).json({ error: 'OHLC query error' });
    }
  });

  app.get('/api/orderbook-history/:platform/:marketId', async (req, res) => {
    if (!orderbookHistoryHandler) {
      res.status(404).json({ error: 'Tick recorder not enabled' });
      return;
    }

    try {
      const result = await orderbookHistoryHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Orderbook history handler failed');
      res.status(500).json({ error: 'Orderbook history query error' });
    }
  });

  app.get('/api/tick-recorder/stats', async (req, res) => {
    if (!tickRecorderStatsHandler) {
      res.status(404).json({ error: 'Tick recorder not enabled' });
      return;
    }

    try {
      const result = await tickRecorderStatsHandler(req);
      if ('error' in result) {
        res.status(result.status ?? 400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      logger.error({ error }, 'Tick recorder stats handler failed');
      res.status(500).json({ error: 'Tick recorder stats error' });
    }
  });

  // Tick streamer stats endpoint
  app.get('/api/tick-streamer/stats', (_req, res) => {
    if (!tickStreamer) {
      res.status(404).json({ error: 'Tick streamer not enabled' });
      return;
    }

    const stats = tickStreamer.getStats();
    res.json({ stats });
  });

  // Feature engineering endpoints
  app.get('/api/features/:platform/:marketId', (req, res) => {
    if (!featureEngineering) {
      res.status(404).json({ error: 'Feature engineering not enabled' });
      return;
    }

    const { platform, marketId } = req.params;
    const outcomeId = typeof req.query.outcomeId === 'string' ? req.query.outcomeId : undefined;

    const features = featureEngineering.getFeatures(platform, marketId, outcomeId);
    if (!features) {
      res.status(404).json({ error: 'No features available for this market' });
      return;
    }

    res.json({ features });
  });

  app.get('/api/features', (_req, res) => {
    if (!featureEngineering) {
      res.status(404).json({ error: 'Feature engineering not enabled' });
      return;
    }

    const snapshots = featureEngineering.getAllFeatures();
    res.json({ snapshots, count: snapshots.length });
  });

  app.get('/api/features/stats', (_req, res) => {
    if (!featureEngineering) {
      res.status(404).json({ error: 'Feature engineering not enabled' });
      return;
    }

    const stats = featureEngineering.getStats();
    res.json({ stats });
  });

  // Telegram Mini App
  app.get('/miniapp', (_req, res) => {
    res.send(`<!DOCTYPE html>
<html>
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
  <title>Blitzkrieg</title>
  <script src="https://telegram.org/js/telegram-web-app.js"></script>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
      background: var(--tg-theme-bg-color, #0f1419);
      color: var(--tg-theme-text-color, #e7e9ea);
      min-height: 100vh;
      padding: 16px;
    }
    .header { text-align: center; margin-bottom: 24px; }
    .header h1 { font-size: 24px; font-weight: 600; }
    .header p { color: var(--tg-theme-hint-color, #71767b); margin-top: 4px; }
    .tabs {
      display: flex;
      gap: 8px;
      margin-bottom: 16px;
      padding: 4px;
      background: var(--tg-theme-secondary-bg-color, #16202a);
      border-radius: 12px;
    }
    .tab {
      flex: 1;
      padding: 10px;
      text-align: center;
      border-radius: 8px;
      cursor: pointer;
      font-size: 14px;
      font-weight: 500;
      transition: all 0.2s;
    }
    .tab.active {
      background: var(--tg-theme-button-color, #1d9bf0);
      color: var(--tg-theme-button-text-color, #fff);
    }
    .section { display: none; }
    .section.active { display: block; }
    .card {
      background: var(--tg-theme-secondary-bg-color, #16202a);
      border-radius: 16px;
      padding: 16px;
      margin-bottom: 12px;
    }
    .card-title { font-size: 14px; color: var(--tg-theme-hint-color, #71767b); margin-bottom: 8px; }
    .card-value { font-size: 28px; font-weight: 700; }
    .card-value.positive { color: #00ba7c; }
    .card-value.negative { color: #f91880; }
    .list-item {
      display: flex;
      justify-content: space-between;
      align-items: center;
      padding: 12px 0;
      border-bottom: 1px solid var(--tg-theme-secondary-bg-color, #2f3336);
    }
    .list-item:last-child { border-bottom: none; }
    .list-item .name { font-weight: 500; }
    .list-item .value { font-size: 14px; }
    .badge {
      display: inline-block;
      padding: 4px 8px;
      border-radius: 8px;
      font-size: 12px;
      font-weight: 600;
    }
    .badge.buy { background: rgba(0, 186, 124, 0.2); color: #00ba7c; }
    .badge.sell { background: rgba(249, 24, 128, 0.2); color: #f91880; }
    .badge.arb { background: rgba(29, 155, 240, 0.2); color: #1d9bf0; }
    .btn {
      width: 100%;
      padding: 14px;
      border-radius: 12px;
      border: none;
      background: var(--tg-theme-button-color, #1d9bf0);
      color: var(--tg-theme-button-text-color, #fff);
      font-size: 16px;
      font-weight: 600;
      cursor: pointer;
      margin-top: 16px;
    }
    .btn:hover { opacity: 0.9; }
    .loading { text-align: center; padding: 40px; color: var(--tg-theme-hint-color, #71767b); }
    .empty { text-align: center; padding: 40px; }
    .empty-icon { font-size: 48px; margin-bottom: 12px; }
    .empty-text { color: var(--tg-theme-hint-color, #71767b); }
    .search { width: 100%; padding: 12px 16px; border-radius: 12px; border: none; background: var(--tg-theme-secondary-bg-color, #16202a); color: var(--tg-theme-text-color, #e7e9ea); font-size: 16px; margin-bottom: 16px; }
    .search::placeholder { color: var(--tg-theme-hint-color, #71767b); }
  </style>
</head>
<body>
  <div class="header">
    <h1>Blitzkrieg</h1>
    <p>Prediction Markets AI</p>
  </div>

  <div class="tabs">
    <div class="tab active" data-tab="portfolio">Portfolio</div>
    <div class="tab" data-tab="markets">Markets</div>
    <div class="tab" data-tab="arb">Arbitrage</div>
  </div>

  <div id="portfolio" class="section active">
    <div class="card">
      <div class="card-title">Total Value</div>
      <div class="card-value" id="total-value">$0.00</div>
    </div>
    <div class="card">
      <div class="card-title">P&L</div>
      <div class="card-value" id="pnl">$0.00</div>
    </div>
    <div class="card">
      <div class="card-title">Positions</div>
      <div id="positions"><div class="loading">Loading...</div></div>
    </div>
  </div>

  <div id="markets" class="section">
    <input type="text" class="search" placeholder="Search markets..." id="market-search">
    <div id="market-list"><div class="loading">Loading...</div></div>
  </div>

  <div id="arb" class="section">
    <div class="card">
      <div class="card-title">Active Opportunities</div>
      <div id="arb-list"><div class="loading">Loading...</div></div>
    </div>
    <button class="btn" onclick="scanArb()">Scan Now</button>
  </div>

  <script>
    const Telegram = window.Telegram.WebApp;
    Telegram.ready();
    Telegram.expand();

    const baseUrl = window.location.origin;

    // Tab switching
    document.querySelectorAll('.tab').forEach(tab => {
      tab.addEventListener('click', () => {
        document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
        document.querySelectorAll('.section').forEach(s => s.classList.remove('active'));
        tab.classList.add('active');
        document.getElementById(tab.dataset.tab).classList.add('active');
      });
    });

    // Format helpers
    function formatUSD(val) {
      const sign = val >= 0 ? '' : '-';
      return sign + '$' + Math.abs(val).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    }

    function formatPct(val) {
      const sign = val >= 0 ? '+' : '';
      return sign + val.toFixed(1) + '%';
    }

    // Load portfolio
    async function loadPortfolio() {
      try {
        const res = await fetch(baseUrl + '/api/performance');
        if (!res.ok) throw new Error('Failed to load');
        const data = await res.json();

        document.getElementById('total-value').textContent = formatUSD(data.stats.totalPnl + 10000);
        const pnlEl = document.getElementById('pnl');
        pnlEl.textContent = formatUSD(data.stats.totalPnl) + ' (' + formatPct(data.stats.avgPnlPct) + ')';
        pnlEl.className = 'card-value ' + (data.stats.totalPnl >= 0 ? 'positive' : 'negative');

        if (data.recentTrades.length === 0) {
          document.getElementById('positions').innerHTML = '<div class="empty"><div class="empty-icon">📊</div><div class="empty-text">No positions yet</div></div>';
        } else {
          document.getElementById('positions').innerHTML = data.recentTrades.slice(0, 5).map(t => \`
            <div class="list-item">
              <div>
                <div class="name">\${t.market.slice(0, 30)}\${t.market.length > 30 ? '...' : ''}</div>
                <div class="value">\${formatUSD(t.size)} @ \${(t.entryPrice * 100).toFixed(0)}%</div>
              </div>
              <span class="badge \${t.side.toLowerCase()}">\${t.side}</span>
            </div>
          \`).join('');
        }
      } catch (err) {
        document.getElementById('positions').innerHTML = '<div class="empty"><div class="empty-text">Failed to load portfolio</div></div>';
      }
    }

    // Load markets
    async function loadMarkets(query = '') {
      try {
        const url = baseUrl + '/market-index/search?q=' + encodeURIComponent(query || 'election');
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to load');
        const data = await res.json();

        if (!data.results || data.results.length === 0) {
          document.getElementById('market-list').innerHTML = '<div class="empty"><div class="empty-icon">🔍</div><div class="empty-text">No markets found</div></div>';
          return;
        }

        document.getElementById('market-list').innerHTML = data.results.slice(0, 10).map(m => \`
          <div class="list-item">
            <div>
              <div class="name">\${m.question?.slice(0, 40) || m.title?.slice(0, 40) || 'Market'}\${(m.question || m.title || '').length > 40 ? '...' : ''}</div>
              <div class="value">\${m.platform}</div>
            </div>
            <div>\${m.yesPrice ? ((m.yesPrice * 100).toFixed(0) + '%') : '-'}</div>
          </div>
        \`).join('');
      } catch (err) {
        document.getElementById('market-list').innerHTML = '<div class="empty"><div class="empty-text">Failed to load markets</div></div>';
      }
    }

    // Load arbitrage opportunities
    async function loadArb() {
      document.getElementById('arb-list').innerHTML = '<div class="empty"><div class="empty-icon">⚡</div><div class="empty-text">Use the Scan button to find opportunities</div></div>';
    }

    async function scanArb() {
      document.getElementById('arb-list').innerHTML = '<div class="loading">Scanning...</div>';
      Telegram.HapticFeedback.impactOccurred('medium');

      // Simulate scan (would call real API)
      setTimeout(() => {
        document.getElementById('arb-list').innerHTML = '<div class="empty"><div class="empty-icon">✅</div><div class="empty-text">No opportunities found above 1% edge</div></div>';
      }, 1500);
    }

    // Search handler
    let searchTimeout;
    document.getElementById('market-search').addEventListener('input', (e) => {
      clearTimeout(searchTimeout);
      searchTimeout = setTimeout(() => loadMarkets(e.target.value), 500);
    });

    // Initialize
    loadPortfolio();
    loadMarkets();
    loadArb();
  </script>
</body>
</html>`);
  });

  // Performance dashboard HTML UI
  app.get('/dashboard', (_req, res) => {
    res.send(`<!DOCTYPE html>
<html>
<head>
  <title>Blitzkrieg Performance Dashboard</title>
  <script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #0f1419; color: #e7e9ea; }
    .header { background: #16202a; padding: 20px 30px; border-bottom: 1px solid #2f3336; display: flex; justify-content: space-between; align-items: center; }
    .header h1 { font-size: 24px; font-weight: 600; }
    .header .refresh { background: #1d9bf0; color: white; border: none; padding: 10px 20px; border-radius: 20px; cursor: pointer; font-weight: 600; }
    .header .refresh:hover { background: #1a8cd8; }
    .container { max-width: 1400px; margin: 0 auto; padding: 30px; }
    .stats-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 20px; margin-bottom: 30px; }
    .stat-card { background: #16202a; border-radius: 16px; padding: 24px; border: 1px solid #2f3336; }
    .stat-card .label { color: #71767b; font-size: 14px; margin-bottom: 8px; }
    .stat-card .value { font-size: 32px; font-weight: 700; }
    .stat-card .value.positive { color: #00ba7c; }
    .stat-card .value.negative { color: #f91880; }
    .charts-grid { display: grid; grid-template-columns: 2fr 1fr; gap: 20px; margin-bottom: 30px; }
    .chart-card { background: #16202a; border-radius: 16px; padding: 24px; border: 1px solid #2f3336; }
    .chart-card h3 { margin-bottom: 20px; font-size: 18px; }
    .chart-container { position: relative; height: 300px; }
    .trades-table { width: 100%; border-collapse: collapse; }
    .trades-table th, .trades-table td { padding: 12px 16px; text-align: left; border-bottom: 1px solid #2f3336; }
    .trades-table th { color: #71767b; font-weight: 500; font-size: 14px; }
    .trades-table tr:hover { background: #1c2732; }
    .badge { display: inline-block; padding: 4px 10px; border-radius: 12px; font-size: 12px; font-weight: 600; }
    .badge.buy { background: rgba(0, 186, 124, 0.2); color: #00ba7c; }
    .badge.sell { background: rgba(249, 24, 128, 0.2); color: #f91880; }
    .badge.win { background: rgba(0, 186, 124, 0.2); color: #00ba7c; }
    .badge.loss { background: rgba(249, 24, 128, 0.2); color: #f91880; }
    .badge.open { background: rgba(29, 155, 240, 0.2); color: #1d9bf0; }
    .loading { text-align: center; padding: 60px; color: #71767b; }
    .error { text-align: center; padding: 60px; color: #f91880; }
    @media (max-width: 900px) { .charts-grid { grid-template-columns: 1fr; } }
  </style>
</head>
<body>
  <div class="header">
    <h1>Performance Dashboard</h1>
    <button class="refresh" onclick="loadData()">Refresh</button>
  </div>
  <div class="container">
    <div id="content" class="loading">Loading...</div>
  </div>

  <script>
    let pnlChart = null;
    let strategyChart = null;

    async function loadData() {
      const content = document.getElementById('content');
      content.innerHTML = '<div class="loading">Loading...</div>';

      try {
        const res = await fetch('/api/performance');
        if (!res.ok) throw new Error('Failed to load data');
        const data = await res.json();
        render(data);
      } catch (err) {
        content.innerHTML = '<div class="error">Failed to load performance data. Make sure trading is enabled.</div>';
      }
    }

    function formatCurrency(val) {
      const sign = val >= 0 ? '+' : '';
      return sign + '$' + Math.abs(val).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    }

    function formatPercent(val) {
      const sign = val >= 0 ? '+' : '';
      return sign + val.toFixed(2) + '%';
    }

    function render(data) {
      const { stats, recentTrades, dailyPnl, byStrategy } = data;

      const html = \`
        <div class="stats-grid">
          <div class="stat-card">
            <div class="label">Total Trades</div>
            <div class="value">\${stats.totalTrades}</div>
          </div>
          <div class="stat-card">
            <div class="label">Win Rate</div>
            <div class="value \${stats.winRate >= 50 ? 'positive' : 'negative'}">\${stats.winRate.toFixed(1)}%</div>
          </div>
          <div class="stat-card">
            <div class="label">Total P&L</div>
            <div class="value \${stats.totalPnl >= 0 ? 'positive' : 'negative'}">\${formatCurrency(stats.totalPnl)}</div>
          </div>
          <div class="stat-card">
            <div class="label">Avg P&L %</div>
            <div class="value \${stats.avgPnlPct >= 0 ? 'positive' : 'negative'}">\${formatPercent(stats.avgPnlPct)}</div>
          </div>
          <div class="stat-card">
            <div class="label">Sharpe Ratio</div>
            <div class="value \${stats.sharpeRatio >= 1 ? 'positive' : stats.sharpeRatio < 0 ? 'negative' : ''}">\${stats.sharpeRatio.toFixed(2)}</div>
          </div>
          <div class="stat-card">
            <div class="label">Max Drawdown</div>
            <div class="value negative">\${formatPercent(-stats.maxDrawdown)}</div>
          </div>
        </div>

        <div class="charts-grid">
          <div class="chart-card">
            <h3>Cumulative P&L</h3>
            <div class="chart-container"><canvas id="pnlChart"></canvas></div>
          </div>
          <div class="chart-card">
            <h3>By Strategy</h3>
            <div class="chart-container"><canvas id="strategyChart"></canvas></div>
          </div>
        </div>

        <div class="chart-card">
          <h3>Recent Trades</h3>
          <table class="trades-table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Market</th>
                <th>Side</th>
                <th>Size</th>
                <th>Entry</th>
                <th>Exit</th>
                <th>P&L</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              \${recentTrades.map(t => \`
                <tr>
                  <td>\${new Date(t.timestamp).toLocaleString()}</td>
                  <td>\${t.market.slice(0, 40)}\${t.market.length > 40 ? '...' : ''}</td>
                  <td><span class="badge \${t.side.toLowerCase()}">\${t.side}</span></td>
                  <td>$\${t.size.toLocaleString()}</td>
                  <td>\${(t.entryPrice * 100).toFixed(1)}%</td>
                  <td>\${t.exitPrice ? (t.exitPrice * 100).toFixed(1) + '%' : '-'}</td>
                  <td class="\${(t.pnl || 0) >= 0 ? 'positive' : 'negative'}">\${t.pnl != null ? formatCurrency(t.pnl) : '-'}</td>
                  <td><span class="badge \${t.status === 'win' ? 'win' : t.status === 'loss' ? 'loss' : 'open'}">\${t.status}</span></td>
                </tr>
              \`).join('')}
            </tbody>
          </table>
        </div>
      \`;

      document.getElementById('content').innerHTML = html;

      // Cumulative P&L chart
      if (pnlChart) pnlChart.destroy();
      const pnlCtx = document.getElementById('pnlChart').getContext('2d');
      pnlChart = new Chart(pnlCtx, {
        type: 'line',
        data: {
          labels: dailyPnl.map(d => d.date),
          datasets: [{
            label: 'Cumulative P&L',
            data: dailyPnl.map(d => d.cumulative),
            borderColor: '#1d9bf0',
            backgroundColor: 'rgba(29, 155, 240, 0.1)',
            fill: true,
            tension: 0.3,
          }]
        },
        options: {
          responsive: true,
          maintainAspectRatio: false,
          plugins: { legend: { display: false } },
          scales: {
            x: { grid: { color: '#2f3336' }, ticks: { color: '#71767b' } },
            y: { grid: { color: '#2f3336' }, ticks: { color: '#71767b', callback: v => '$' + v } }
          }
        }
      });

      // Strategy breakdown chart
      if (strategyChart) strategyChart.destroy();
      const stratCtx = document.getElementById('strategyChart').getContext('2d');
      strategyChart = new Chart(stratCtx, {
        type: 'doughnut',
        data: {
          labels: byStrategy.map(s => s.strategy),
          datasets: [{
            data: byStrategy.map(s => s.trades),
            backgroundColor: ['#1d9bf0', '#00ba7c', '#f91880', '#ffd400', '#7856ff'],
          }]
        },
        options: {
          responsive: true,
          maintainAspectRatio: false,
          plugins: {
            legend: { position: 'bottom', labels: { color: '#e7e9ea' } }
          }
        }
      });
    }

    loadData();
  </script>
</body>
</html>`);
  });

  return {
    async start() {
      return new Promise((resolve) => {
        httpServer = createHttpServer(app);

        // WebSocket server - handles both /ws and /chat
        wss = new WebSocketServer({ noServer: true, maxPayload: 1024 * 1024 });

        // Handle upgrade requests
        httpServer.on('upgrade', (request: IncomingMessage, socket, head) => {
          const pathname = request.url?.split('?')[0] || '';

          if (pathname === '/ws' || pathname === '/chat') {
            wss!.handleUpgrade(request, socket, head, (ws) => {
              wss!.emit('connection', ws, request);
            });
          } else if (pathname === '/api/ticks/stream') {
            // Tick streaming WebSocket endpoint
            if (!tickStreamer) {
              socket.write('HTTP/1.1 503 Service Unavailable\r\n\r\n');
              socket.destroy();
              return;
            }
            wss!.handleUpgrade(request, socket, head, (ws) => {
              tickStreamer!.handleConnection(ws);
            });
          } else {
            socket.destroy();
          }
        });

        // Single connection handler — dispatches /chat via mutable callback
        // This prevents listener accumulation across channel rebuilds
        wss.on('connection', (ws: WebSocket, request: IncomingMessage) => {
          const reqPath = (request.url || '').split('?')[0];
          if (reqPath === '/chat') {
            if (chatConnectionHandler) chatConnectionHandler(ws, request);
            return;
          }

          logger.info('WebSocket API client connected');

          ws.on('message', (data) => {
            try {
              const message = JSON.parse(data.toString());
              logger.debug({ message }, 'WS API message received');

              ws.send(
                JSON.stringify({
                  type: 'res',
                  id: message.id,
                  ok: true,
                  payload: { echo: message },
                })
              );
            } catch (err) {
              logger.error({ err }, 'Failed to parse WS message');
            }
          });

          ws.on('close', () => {
            logger.info('WebSocket API client disconnected');
          });
        });

        httpServer.listen(config.port, () => {
          resolve();
        });
      });
    },

    async stop() {
      if (ipCleanupInterval) {
        clearInterval(ipCleanupInterval);
        ipCleanupInterval = null;
      }
      return new Promise<void>((resolve) => {
        wss?.close();
        if (httpServer) {
          httpServer.close(() => resolve());
        } else {
          resolve();
        }
      });
    },

    getWebSocketServer(): WebSocketServer | null {
      return wss;
    },

    setChannelWebhookHandler(handler: ChannelWebhookHandler | null): void {
      channelWebhookHandler = handler;
    },

    setMarketIndexHandler(handler: MarketIndexHandler | null): void {
      marketIndexHandler = handler;
    },
    setMarketIndexStatsHandler(handler: MarketIndexStatsHandler | null): void {
      marketIndexStatsHandler = handler;
    },
    setMarketIndexSyncHandler(handler: MarketIndexSyncHandler | null): void {
      marketIndexSyncHandler = handler;
    },
    setPerformanceDashboardHandler(handler: PerformanceDashboardHandler | null): void {
      performanceDashboardHandler = handler;
    },
    setBacktestHandler(handler: BacktestHandler | null): void {
      backtestHandler = handler;
    },
    setTicksHandler(handler: TicksHandler | null): void {
      ticksHandler = handler;
    },
    setOHLCHandler(handler: OHLCHandler | null): void {
      ohlcHandler = handler;
    },
    setOrderbookHistoryHandler(handler: OrderbookHistoryHandler | null): void {
      orderbookHistoryHandler = handler;
    },
    setTickRecorderStatsHandler(handler: TickRecorderStatsHandler | null): void {
      tickRecorderStatsHandler = handler;
    },
    setTickStreamer(streamer: TickStreamer | null): void {
      tickStreamer = streamer;
    },
    setFeatureEngineering(service: FeatureEngineering | null): void {
      featureEngineering = service;
    },
    setBittensorRouter(router: Router | null): void {
      if (router) {
        app.use('/api/bittensor', requireAuth, router);
      }
    },
    setTradingApiRouter(router: Router | null): void {
      if (router) {
        app.use('/api', requireAuth, router);
      }
    },
    setPercolatorRouter(router: Router | null): void {
      if (router) {
        app.use('/api/percolator', requireAuth, router);
      }
    },
    setShieldRouter(router: Router | null): void {
      if (router) {
        app.use('/api/shield', requireAuth, router);
      }
    },
    setAuditRouter(router: Router | null): void {
      if (router) {
        app.use('/api/audit', requireAuth, router);
      }
    },
    setDCARouter(router: Router | null): void {
      if (router) {
        app.use('/api/dca', requireAuth, router);
      }
    },
    setTwapRouter(router: Router | null): void {
      if (router) {
        app.use('/api/twap', requireAuth, router);
      }
    },
    setBracketRouter(router: Router | null): void {
      if (router) {
        app.use('/api/bracket', requireAuth, router);
      }
    },
    setTriggerRouter(router: Router | null): void {
      if (router) {
        app.use('/api/triggers', requireAuth, router);
      }
    },
    setCopyTradingRouter(router: Router | null): void {
      if (router) {
        app.use('/api/copy-trading', requireAuth, router);
      }
    },
    setOpportunityRouter(router: Router | null): void {
      if (router) {
        app.use('/api/opportunities', requireAuth, router);
      }
    },
    setWhaleRouter(router: Router | null): void {
      if (router) {
        app.use('/api/whales', requireAuth, router);
      }
    },
    setRiskRouter(router: Router | null): void {
      if (router) {
        app.use('/api/risk', requireAuth, router);
      }
    },
    setRoutingRouter(router: Router | null): void {
      if (router) {
        app.use('/api/routing', requireAuth, router);
      }
    },
    setFeedsRouter(router: Router | null): void {
      if (router) {
        app.use('/api/feeds', requireAuth, router);
      }
    },
    setMonitoringRouter(router: Router | null): void {
      if (router) {
        app.use('/api/monitoring', requireAuth, router);
      }
    },
    setAltDataRouter(router: Router | null): void {
      if (router) {
        app.use('/api/alt-data', requireAuth, router);
      }
    },
    setAlertsRouter(router: Router | null): void {
      if (router) {
        app.use('/api/alerts', requireAuth, router);
      }
    },
    setQueueRouter(router: Router | null): void {
      if (router) {
        app.use('/api/queue', requireAuth, router);
      }
    },
    setWebhooksRouter(router: Router | null): void {
      if (router) {
        app.use('/api/webhooks', requireAuth, router);
      }
    },
    setPaymentsRouter(router: Router | null): void {
      if (router) {
        app.use('/api/payments', requireAuth, router);
      }
    },
    setEmbeddingsRouter(router: Router | null): void {
      if (router) {
        app.use('/api/embeddings', requireAuth, router);
      }
    },
    setCronRouter(router: Router | null): void {
      if (router) {
        app.use('/api/cron', requireAuth, router);
      }
    },
    setLaunchRouter(router: Router | null): void {
      if (router) {
        app.use('/api/launch', requireAuth, router);
      }
    },
    setCommandListHandler(handler: CommandListHandler | null): void {
      commandListHandler = handler;
    },
    setHooksHandler(handler: HooksHandler | null): void {
      hooksHandler = handler;
    },
    setOnSessionDelete(handler: ((key: string) => void) | null): void {
      onSessionDelete = handler;
    },

    setChatConnectionHandler(handler: ((ws: WebSocket, req: IncomingMessage) => void) | null): void {
      chatConnectionHandler = handler;
    },
  };
}
