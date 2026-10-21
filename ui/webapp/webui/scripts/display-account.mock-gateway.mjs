#!/usr/bin/env node
/**
 * issue 559 视觉验证 mock 网关 —— 绑 127.0.0.1:51888（#422 的 mock 网关视觉
 * 验证法），扮演 ui_kit_web 的面板契约：/api/ping、/api/login、/api/snapshot、
 * /api/plugins、/api/command、/api/display-account/set（本 PR 的新代理路由）。
 * 用法：node scripts/display-account.mock-gateway.mjs [distDir]
 * 只回环、只假数据、只活在测试进程 —— 与生产进程/生产文件无关。
 */
import http from 'node:http'
import { readFileSync, existsSync } from 'node:fs'
import { join, extname } from 'node:path'

const DIST = process.argv[2] ?? join(process.cwd(), 'dist')
const PORT = 51888
const TOKEN = 'mock-token'

// ── mock state ───────────────────────────────────────────────────────────────
// 两个账户：kalshi-like 绑「venue-b」，polymarket-like 绑「venue-a」。
// 初始 displayAccount = acct-a（venue-a）—— 会话账户也是 acct-a（两态一致）。
// 视觉验证剧本：切到 acct-b 后，倒计时卡身份应随 display.account.set 生效
// （下一拍快照 displayVenue 变 venue-b），而不是停在 venue-a。
const state = {
  active: 'acct-a',
  displayAccount: 'acct-a',
  persistError: null,
}
const ACCOUNTS = [
  { id: 'acct-a', name: '显示A', marketType: 'prediction', status: 'active', balance: '1000.00', available: '950.00', venue: 'venue-a', credentialsLoaded: true, tradingEnabled: true, strategies: [], openPositions: 1, dayRealizedUsd: '12.5' },
  { id: 'acct-b', name: '显示B', marketType: 'prediction', status: 'active', balance: '500.00', available: '480.00', venue: 'venue-b', credentialsLoaded: true, tradingEnabled: true, strategies: ['strat-1'], openPositions: 0, dayRealizedUsd: '0' },
]
const venueOf = (id) => ACCOUNTS.find((a) => a.id === id)?.venue ?? ''

let slot = 4242
const snapshot = () => ({
  connected: true,
  mode: 'dry',
  lastError: null,
  displayAccount: state.displayAccount,
  displayVenue: venueOf(state.displayAccount),
  marketPlugins: [
    { name: 'venue-a', kind: 'prediction' },
    { name: 'venue-b', kind: 'prediction' },
  ],
  marketActiveName: 'venue-a',
  marketActiveType: 'prediction',
  round: {
    slot, ageSec: 120, timeLeftSec: 780, canTrade: true,
    prices: [
      { venue: 'venue-a', asset: 'ASSET-A1', up: 0.62, down: 0.40 },
      { venue: 'venue-b', asset: 'ASSET-B1', up: 0.55, down: 0.47 },
    ],
    venueRounds: [
      { venue: 'venue-a', slot, ageSec: 120, timeLeftSec: 780 },
      { venue: 'venue-b', slot, ageSec: 45, timeLeftSec: 855 },
    ],
  },
  books: [
    { venue: 'venue-a', asset: 'ASSET-A1', up: { bestBid: 0.6, bestAsk: 0.64, spread: 0.04, obi: 0.2, bids: [], asks: [], upTokenId: 'tok-a-up' }, down: { bestBid: 0.38, bestAsk: 0.42, spread: 0.04, obi: -0.1, bids: [], asks: [], downTokenId: 'tok-a-dn' } },
    { venue: 'venue-b', asset: 'ASSET-B1', up: { bestBid: 0.53, bestAsk: 0.57, spread: 0.04, obi: 0.1, bids: [], asks: [], upTokenId: 'tok-b-up' }, down: { bestBid: 0.45, bestAsk: 0.49, spread: 0.04, obi: -0.2, bids: [], asks: [], downTokenId: 'tok-b-dn' } },
  ],
  positions: [],
  trades: { totalTrades: 0 },
  tradeRows: [],
  stats: {},
})
const plugins = () => ({
  connected: true,
  strategies: [], extensions: [],
  marketPlugins: [{ name: 'venue-a', kind: 'prediction' }, { name: 'venue-b', kind: 'prediction' }],
  marketActive: 'venue-a',
  lastError: null,
})

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.ico': 'image/x-icon', '.woff2': 'font/woff2' }
const json = (res, code, doc) => { const b = JSON.stringify(doc); res.writeHead(code, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(b) }); res.end(b) }

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1')
  const p = url.pathname
  const auth = req.headers['x-auth-token'] === TOKEN
  console.log(`[mock] ${req.method} ${p}`)

  if (p === '/api/ping') return json(res, 200, { ok: true, authRequired: true })
  if (p === '/api/login') return json(res, 200, { ok: true, token: TOKEN, user: 'mock' })

  // static panel BEFORE the auth gate（页面壳不需要会话；/api/* 才要）
  const rel = p === '/panel/' || p === '/panel' ? 'index.html' : p.replace(/^\/panel\//, '')
  const file = join(DIST, rel)
  if (!p.startsWith('/api/') && existsSync(file)) {
    const b = readFileSync(file)
    res.writeHead(200, { 'Content-Type': MIME[extname(file)] ?? 'application/octet-stream' })
    return res.end(b)
  }

  if (!auth) return json(res, 401, { error: 'unauthorized (mock)' })

  if (p === '/api/snapshot') { slot += (Date.now() % 2); return json(res, 200, snapshot()) }
  if (p === '/api/plugins') return json(res, 200, plugins())

  if (p === '/api/display-account/set' && req.method === 'POST') {
    // 代理纪律验证点：body 应当是 {accountId, reason?} 的 JSON 对象。
    let body = ''
    req.on('data', (c) => { body += c })
    req.on('end', () => {
      try {
        const params = JSON.parse(body || '{}')
        if (typeof params.accountId !== 'string' || !params.accountId) {
          return json(res, 200, { error: 'mock: accountId required' })
        }
        state.displayAccount = params.accountId
        console.log(`[mock] display.account.set → ${params.accountId}`)
        return json(res, 200, {
          accountId: state.displayAccount,
          displayVenue: venueOf(state.displayAccount),
          persisted: true, atMs: Date.now(),
          note: 'mock gateway: display_account applied',
        })
      } catch (e) {
        return json(res, 400, { error: `mock: body must be JSON object: ${e.message}` })
      }
    })
    return
  }

  if (p === '/api/command') {
    let body = ''
    req.on('data', (c) => { body += c })
    req.on('end', () => {
      const cmd = body.startsWith('{') ? (JSON.parse(body).cmd ?? body) : body
      if (cmd === 'accounts') {
        return json(res, 200, { ok: true, data: { version: '1', active: state.active, accounts: ACCOUNTS } })
      }
      if (cmd.startsWith('account ')) {
        const id = cmd.slice('account '.length).trim()
        if (!ACCOUNTS.some((a) => a.id === id)) return json(res, 200, { ok: false, message: `mock: unknown account ${id}` })
        state.active = id
        return json(res, 200, { ok: true, message: `mock: active → ${id}` })
      }
      return json(res, 200, { ok: true, message: `mock: ${cmd}` })
    })
    return
  }

  // static panel（漏网的静态路径；/api/* 不落这里）
  const rel2 = p === '/panel/' || p === '/panel' ? 'index.html' : p.replace(/^\/panel\//, '')
  const file2 = join(DIST, rel2)
  if (!p.startsWith('/api/') && existsSync(file2)) {
    const b = readFileSync(file2)
    res.writeHead(200, { 'Content-Type': MIME[extname(file2)] ?? 'application/octet-stream' })
    return res.end(b)
  }
  res.writeHead(404); res.end('not found')
})

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[mock] display-account mock gateway on http://127.0.0.1:${PORT}/panel/`)
})
