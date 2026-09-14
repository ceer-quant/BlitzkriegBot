# Crypto HFT Bot —— 完整文档

> Polymarket 加密二元市场（UP/DOWN）自动交易机器人。
> 基于 Clodds（AI 交易终端）的 `crypto-hft` 技能，本仓库为专注 Polymarket 15 分钟加密市场的定制版。
>
> 文档版本：v1.8.0 ｜ 适用目录：`polymarket-5m-bot/CloddsBot`

---

## 目录

1. [概述](#1-概述)
2. [系统架构与数据流](#2-系统架构与数据流)
3. [目录结构](#3-目录结构)
4. [环境要求与安装](#4-环境要求与安装)
5. [环境变量](#5-环境变量)
6. [构建与运行](#6-构建与运行)
7. [控制面板](#7-控制面板)
8. [市场与回合机制](#8-市场与回合机制)
9. [策略详解](#9-策略详解)
10. [入场逻辑](#10-入场逻辑)
11. [出场逻辑](#11-出场逻辑)
12. [风险控制](#12-风险控制)
13. [订单生命周期与成交处理](#13-订单生命周期与成交处理)
14. [Rust 执行器与 Poly1271](#14-rust-执行器与-poly1271)
15. [配置参考（完整默认值）](#15-配置参考完整默认值)
16. [命令参考](#16-命令参考)
17. [数据文件与分析脚本](#17-数据文件与分析脚本)
18. [故障排查](#18-故障排查)
19. [开发与测试](#19-开发与测试)
20. [安全注意事项](#20-安全注意事项)

---

## 1. 概述

本机器人交易 **Polymarket 的加密二元市场**：每 15 分钟（默认）为一个回合（round），每个资产（BTC/ETH/SOL/XRP）有 **UP / DOWN** 一对代币，回合结束时按 Chainlink 价格结算为 0 或 1。

核心特性：

- **默认 DRY_RUN（模拟）**：不真实下单，但也完整走订单生命周期，便于零成本验证。
- **主策略 `spread_arb`**：在"趋势确认"的代币回调时，用 **maker 挂单** 低吸（0 手续费），是当前主要盈利来源。
- **确定性风控**：动态止损、移动止盈、冷却、连亏熔断、日亏损上限。
- **订单引擎防重/防漏**：幂等成交账本、未知成交缓冲、重启对账、孤儿单清理。
- **影子引擎（Shadow）**：记录每笔持仓持有期内"本侧/对侧"价格路径，供事后反事实分析（验证 edge 是否真实）。
- **Web 控制面板**：实时状态、持仓、成交、盈亏曲线、声音提醒。

> **重要**：引擎 **不会随进程自动启动**。进程只启动网关/面板，需要用户在面板点击 **“启动”**（或发命令 `/crypto-hft start`）后引擎才开始交易。

---

## 2. 系统架构与数据流

```
┌────────────────────┐        ┌──────────────────────┐
│ Binance 现货 WS     │──────▶ │ CryptoFeed           │  现货价格缓冲(spot)
└────────────────────┘        └──────────┬───────────┘
                                          │ spotMove5/30/60
┌────────────────────┐        ┌──────────▼───────────┐
│ Polymarket WS 盘口  │──────▶ │ Orderbook / Trend     │  盘口快照(book)
│ (market channel)   │        │ Tracker / PriceBuffer │  趋势确认(rolling)
└────────────────────┘        └──────────┬───────────┘
                                          │
┌────────────────────┐        ┌──────────▼───────────┐
│ Gamma API (市场)    │──────▶ │ MarketScanner        │  识别当前回合 UP/DOWN 代币
└────────────────────┘        └──────────┬───────────┘
                                          │
                              ┌───────────▼────────────┐
                              │ Strategy Evaluators    │  evaluateAll()
                              │ spread_arb / momentum  │
                              │ sharp_reversal / ...   │
                              └───────────┬────────────┘
                                          │ TradeSignal
                              ┌───────────▼────────────┐
                              │ Entry Gate             │  冷却/熔断/最大持仓/动量过滤/盘口新鲜度
                              └───────────┬────────────┘
                                          │
                              ┌───────────▼────────────┐
                              │ ExecutionService       │  下单/撤单/查成交
                              │ (Rust Poly1271 / HMAC) │
                              └───────────┬────────────┘
                                          │ fills
                              ┌───────────▼────────────┐
                              │ PositionManager        │  持仓、动态止损、移动止盈
                              └───────────┬────────────┘
                                          │ closed trades / signals / shadow
                              ┌───────────▼────────────┐
                              │ data/*.jsonl           │  + 面板 WS 推送
                              └────────────────────────┘
```

关键运行时组件：

| 组件 | 文件 | 职责 |
|------|------|------|
| 引擎 | `src/strategies/crypto-hft/index.ts` | 编排一切：扫描、评估、下单、出场、风控 |
| 策略 | `src/strategies/crypto-hft/strategies.ts` | 各策略的进出场判定 |
| 趋势确认 | `src/strategies/crypto-hft/trend-tracker.ts` | 滚动窗口趋势（above-ratio） |
| 持仓 | `src/strategies/crypto-hft/positions.ts` | `checkExits`、PnL、动态止损（委托给 exit-policy） |
| 出场策略 | `src/strategies/crypto-hft/exit-policy.ts` | **纯函数**：bid 口径的止盈/止损/移动止盈判定，线上与回测共用 |
| 市场扫描 | `src/strategies/crypto-hft/market-scanner.ts` | 回合/代币识别、时钟偏移 |
| 订单管理 | `src/strategies/crypto-hft/order-manager.ts` | 订单状态机、落盘 |
| 执行 | `src/execution/index.ts` | 真实/模拟下单、成交事件、对账 |
| Rust 执行器 | `rust-executor/src/main.rs` | Poly1271(签名类型3) 下单/余额 |
| 技能入口 | `src/skills/bundled/crypto-hft/index.ts` | 命令、feed 接线、状态输出 |

---

## 3. 目录结构

```
CloddsBot/
├── src/
│   ├── index.ts                     # 进程入口：启动 gateway + 各服务
│   ├── gateway/                      # HTTP + WebSocket 网关（默认端口 18789）
│   │   ├── index.ts
│   │   └── server.ts                # /webchat 静态、/api/trading/balance、/chat WS
│   ├── skills/bundled/crypto-hft/
│   │   ├── index.ts                 # 技能命令实现
│   │   └── SKILL.md
│   ├── strategies/crypto-hft/       # ★ 机器人核心
│   │   ├── index.ts                 # 引擎（~2000 行）
│   │   ├── strategies.ts            # 策略判定
│   │   ├── positions.ts             # 持仓/出场
│   │   ├── trend-tracker.ts         # 趋势确认
│   │   ├── market-scanner.ts        # 回合识别
│   │   ├── order-manager.ts         # 订单状态
│   │   ├── orderbook.ts             # 盘口快照
│   │   ├── local-orderbook.ts       # 本地盘口缓存
│   │   ├── presign-pool.ts          # 预签名池（降低下单延迟）
│   │   ├── shadow-engine.ts         # 影子反事实记录
│   │   ├── signal-log.ts            # 原始信号记录
│   │   ├── trade-db.ts              # 成交流水
│   │   ├── state-machine.ts         # 仓位状态机
│   │   ├── presets.ts               # 预设
│   │   └── types.ts                 # 类型与默认值
│   ├── execution/
│   │   ├── index.ts                 # 下单/撤单/成交/对账
│   │   └── rust-clob-executor.ts    # Rust 子进程封装
│   └── feeds/
│       ├── crypto/index.ts          # Binance 现货
│       └── polymarket/index.ts      # Poly 盘口 WS
├── rust-executor/                   # Rust CLOB 执行器（Poly1271）
├── ui/hft.html          # 控制面板
├── scripts/
│   ├── analyze-shadow.mjs           # 影子反事实分析
│   └── analyze-signals.mjs          # 信号分析
├── data/                            # 运行时数据（trades/orders/signals/shadow）
├── docs/CRYPTO_HFT.md               # 本文档
├── .env / .env.example
├── run.log                          # nohup 运行日志
└── package.json
```

---

## 4. 环境要求与安装

- **Node.js ≥ 22**（`package.json` engines）
- **Rust**（用于编译 `rust-executor`，仅 Poly1271/签名类型3 钱包需要）
- macOS / Linux

```bash
git clone <repo> && cd polymarket-5m-bot/CloddsBot
npm install
cp .env.example .env      # 然后填写下方变量
```

编译 Rust 执行器：

```bash
cd rust-executor
cargo build --release
# 产物：rust-executor/target/release/clodds-rust-executor
```

> 若未编译 Rust 二进制，签名类型 3（Poly1271 / 充值钱包）无法下单；普通 EOA(HMAC) 不受影响。

---

## 5. 环境变量

代码实际读取的键名（见 `src/skills/bundled/crypto-hft/index.ts` 的 `getExecution()`）：

| 变量 | 必填 | 说明 |
|------|------|------|
| `POLYMARKET_PRIVATE_KEY` | 是（实盘） | 签名私钥，`0x...`；回退 `PRIVATE_KEY` |
| `POLYMARKET_FUNDER_ADDRESS` | 是（签名类型3） | 代理/充值钱包地址（持有资金） |
| `POLYMARKET_API_KEY` | L2 认证 | CLOB API Key |
| `POLYMARKET_API_SECRET` | L2 认证 | CLOB API Secret |
| `POLYMARKET_API_PASSPHRASE` | L2 认证 | CLOB API Passphrase |
| `DRY_RUN` | 否 | `true` = 模拟盘（默认）；`false` = 实盘 |
| `ANTHROPIC_API_KEY` | 运行网关 | Claude 模型调用 |
| `CLODDS_TOKEN` | 否 | 网关 API 令牌 |
| `LOG_LEVEL` | 否 | `debug/info/warn/error` |

> **注意**：`.env.example` 中写的是 `POLY_*`，但本定制版代码读取的是
> `POLYMARKET_*`（技能内的 `getExecution`）。请以本表为准。

当前账号为 **Poly1271 充值钱包**，故代码固定 `signatureType: 3`，下单走 Rust 执行器。

---

## 6. 构建与运行

```bash
# 类型检查 + 编译到 dist/
npm run build

# 运行（生产）
node dist/index.js          # 或 npm start
```

后台运行与日志：

```bash
nohup node dist/index.js > run.log 2>&1 &
lsof -nP -iTCP:18789 -sTCP:LISTEN      # 确认端口监听
```

重启（改代码后）：

```bash
ps aux | grep "node dist/index.js" | grep -v grep | awk '{print $2}' | xargs -r kill -TERM
sleep 3
nohup node dist/index.js > run.log 2>&1 &
```

开发模式：`npm run dev`（tsx 热重载）。

> **启动后引擎仍处于停止态**：需要打开面板点击 **“启动”**，或发送 `/crypto-hft start`。

---

## 7. 控制面板

- 地址：`http://localhost:18789/ui/hft.html`
- 面板通过 WebSocket `ws://<host>/chat` 连接，周期性发送：
  - `/crypto-hft status`（统计/回合/持仓/价格）
  - `/crypto-hft positions`（最近成交）

面板内容：

- **P&L 卡片**：净盈亏、今日盈亏、胜率、交易数、成交量、均价。
- **回合倒计时**、熔断状态、钱包 USDC 余额（`/api/trading/balance`，优先走 Rust）。
- **持仓列表**：资产/方向/入场价→现价/浮盈/剩余时间。
- **成交表**：每笔的胜负着色（`.win` / `.loss`）。
- **声音提醒**（Web Audio 合成）：
  | 事件 | 函数 | 声音 |
  |------|------|------|
  | 新建挂单 | `playOrder()` | 短促上扬"嘀"（880→1320Hz） |
  | 盈利平仓 | `playProfit()` | 上行琶音 C5-E5-G5-C6 |
  | 亏损平仓 | `playDang()` | 低音"咚"（220→110Hz） |
  | 熔断恢复 | `playDing()` | 高音叮 |
  | 熔断/错误告警 | `playWuwu()` | 双音警笛 |

  > 胜负判定使用状态中的累计 `wins/losses` 计数增量（不读 DOM），避免与成交表渲染时序错位。

**启动 / 停止按钮**：按钮通过 WS 发送 `/crypto-hft start` / `/crypto-hft stop`。

---

## 8. 市场与回合机制

- 每个回合（默认 `roundDurationSec = 900`，即 15 分钟）每个资产一对代币：
  `upTokenId` / `downTokenId`，`conditionId` 标识市场。
- 回合切换时 `scanner` 重新解析代币；`trendTracker.resetIfNewRound(slot)` 清空趋势状态。
- 结算价来自 Chainlink；代币最终为 0 或 1。
- 机器人不持有到结算（除非极端情况），通常在回合内通过 TP/SL/到期前强平离场。

排序键：`roundSlot = expiresAt / roundDuration`。

---

## 9. 策略详解

引擎启用的策略（`enabled`，见 `index.ts` 顶部）：

| 策略 | 默认 | 入场条件 | 订单模式 |
|------|------|----------|----------|
| `spread_arb` | ✅ 开 | 趋势确认后回调低吸 | maker_then_taker |
| `sharp_reversal` | ✅ 开 | 高价代币急跌反转 | maker |
| `momentum` | ❌ 关 | 现货动了、盘口滞后 | maker_then_taker |
| `mean_reversion` | ❌ 关 | 定价偏离、现货平静 | maker |
| `penny_clipper` | ❌ 关 | 区间震荡、低于均值 | maker |
| `expiry_fade` | ❌ 关 | 临近到期、定价偏斜 | taker |

### 9.1 `spread_arb`（主策略）

思路：**"确认的强势代币回调时低吸"**。

1. **趋势确认**：代币中价在滚动窗口内 **≥ `trendMinPrice`（0.55）** 的样本占比 ≥ `trendRatio`（0.8），且窗口已满（≥ `trendConfirmSec`×0.9）。由 `TrendTracker` 在每次盘口/价格更新时维护。
2. **回调入场**：确认后，当代币中价回落到 ≤ `trendMaxEntryPrice`（0.45）附近时，按 **中价 × `trendEntryFactor`（0.98）** 计算挂单价，且：
   - 不得高于实时 `bestBid`（maker 纪律）；
   - 必须严格低于中价；
   - 不得高于 `trendMaxEntryPrice`。
3. **必须新鲜双边盘口**：`book.bids` 与 `book.asks` 均存在且 `midPrice > 0`，否则不产单（防止用陈旧价成交，历史事故：0.41 买在 mid 0.23）。
4. **动量过滤**：`passesMomentumFilter` 要求 Binance 现货近 `momentumFilterWindowSec`（30s）不能明显逆势（容忍 `momentumFilterMinPct`，0.03%）。
5. **订单模式**：`maker_then_taker` —— 先挂单 `entryOrder.makerTimeoutMs`（5s），未成交则撤单转吃单。

### 9.2 `sharp_reversal`

高价代币（`highThreshold`）短期急跌到 `entryPrice` 附近时挂单买入，固定目标离场。适合捕捉恐慌错杀。

---

## 10. 入场逻辑

评估循环对每个资产调用 `evaluateAll()`：

1. **就绪条件**：现货缓冲 ≥ 3 个点。
2. **回合条件**：`round.ageSec ≥ minRoundAgeSec`（30s）；`timeLeftSec > minTimeLeftSec + entryCutoffSec`。
3. **盘口新鲜**：`getBook()` 要求 `now - timestamp ≤ maxOrderbookStaleMs`（默认 8000ms）。
4. **策略产生信号** → **动量过滤** → 进入信号队列。
5. **入场闸门**（`state-machine` + 引擎）：
   - 熔断未恢复（`haltNewEntries`）→ 拒绝；
   - 已达 `maxPositions`（含挂单中的入场单）→ 拒绝；
   - 冷却：`assetCooldownSec`（同资产）、`lossCooldownSec`（亏损后同资产）、`exitCooldownSec`（同币种方向）、`stopLossCooldownSec`、`breakerCooldownSec`；
   - 日亏损 `maxDailyLossUsd` 达上限。
6. **下单**：
   - 记录订单到 `order-manager`；
   - DRY_RUN 下：taker 立即成交；maker 挂单等待；maker_then_taker 5s 后转 taker；
   - 实盘：走 `execution.buyLimit`（Poly1271 时走 Rust）。

---

## 11. 出场逻辑

`positionMgr.checkExits()` 被定时调用（约 50ms），当前默认 **简单出场模式**（`simpleExitEnabled = true`）。

> **估值/触发口径（v1.9 起）**：所有出场判定与高水位（HWM）都基于**可成交 bestBid**，不再用中间价（mid）。
> 旧实现用 mid 触发止盈、却用 bid 成交，薄盘口下出现过"mid 显示 +100% 触发、实际 bid 只成交在 +44%"的失真。
> 止损另有保护：只有当 bid 距 mid 不超过 `maxBidWickPct`（默认 8%）时才触发，避免被一tick的假挂单砸止损。
> 出场规则全部抽成纯函数 `exit-policy.ts`，线上与影子回测共用同一份实现（见第 17 节）。

1. **强制离场** `force_exit`：剩余时间 ≤ `forceExitSec`（120s）——绝对截止；即使盘口暂时无 bid 也会用最后已知价离场，绝不拖到结算。
2. **止盈** `take_profit`：PnL ≥ `takeProfitPct`（100，仅作兜底）。非紧急，默认 **maker-first**（`makerFirstExitEnabled`）：先在 ask 挂 0 手续费卖单，`exitOrder.makerTimeoutMs`（默认 2s）未成交再撤单转 taker。
3. **动态止损** `stop_loss`：PnL ≤ `effectiveStopPct(...)`（保护性，立即 taker，且要求 bid 经 mid 确认非闪崩）。
   - `dynamicStopEnabled`：随到期临近而收紧：从 `stopLossPct`(50%) 线性收紧到 `stopMinPct`(10%)，在剩余 `stopTightenStartSec`(300s) 内生效。
4. **移动止盈** `trailing_stop`：浮盈高点 ≥ `trailingMinHighPct`(15%) 后武装，回撤 ≥ `max(minTrailPct(10), min(表值, 时间值))` 触发；高浮盈按高点的 `proportionalTrailPct`(15%) 回撤锁利。
5. **时间退出** `time_exit`：剩余 ≤ `minTimeLeftSec`(180s)，强制类，立即 taker。

> 历史数据证明"固定 TP40% 会砍掉 +80~126% 的大行情"，因此默认改为"高止盈兜底 + 移动止盈锁利"。

非简单模式下的其它出场（可切换）：`ratchet_floor`、`breakeven_lock`、`depth_collapse`、`stale_profit`、`stagnant_profit`、`spot_reversal`、`quick_profit`。

**出场执行**：`exitOrder.mode`（默认 taker）→ 以实时 `bestBid` 吃单，保证成交。出场前有 `exitGraceSec`(3s) 宽限。

---

## 12. 风险控制

| 机制 | 参数 | 说明 |
|------|------|------|
| 连亏熔断 | `MAX_CONSECUTIVE_LOSSES=3`, `breakerCooldownSec=300` | 连亏 3 笔：**停止开新仓**并撤入场单，持仓仍由 TP/SL 管理（不再强平，避免搁置仓位） |
| 动态止损 | `dynamicStopEnabled` | 越接近到期止损越紧 |
| 冷却 | `assetCooldownSec=90` / `lossCooldownSec=180` / `exitCooldownSec=60` | 防止连续踩同一坑 |
| 日亏损上限 | `maxDailyLossUsd=200` | 达到即停止开仓 |
| 最大持仓 | `maxPositions=2`（含挂单中入场单） | |
| 单笔上限 | `sizeUsd=2.5`, `maxPositionUsd=2.5`, `minShares/maxShares=10` | |
| 成交前复核 | `maxFillVsMidPct=3` | 买单价高于当前 mid 3% 以上则撤单而非成交（防恶价成交） |
| 临期清挂单 | `entryCutoffSec=120`, `roundEndClearSec=180`, `staleOrderAgeSec=25` | 避免临到期成交 |
| 趋势破裂撤单 | `trendBrokenPrice=0.35` | 已确认趋势跌破 0.35 → 视为反转，撤入场挂单（不平仓） |

---

## 13. 订单生命周期与成交处理

订单状态：`SUBMITTED → LIVE → FILLED / PARTIAL / CANCELLED / FAILED`（`order-manager.ts`）。

关键健壮性设计（`index.ts`）：

- **幂等成交账本** `appliedFills`：以 `tradeKey` 去重，支持 `MATCHED→MINED→CONFIRMED` 多次回报，只按增量入账，并按订单 size 设上限。
- **未知/遗漏成交缓冲** `pendingFills`：成交先于订单记录到达时暂存 30s 重试。
- **失败回滚**：下单失败（FAILED）会回滚仓位/订单状态。
- **启动/超时对账**：`reconcileOnStart` 扫描交易所挂单，取消未知订单（限定当前回合代币），认领未跟踪持仓。
- **锁单**：`orderInFlight` 防止重复下单。
- **止损/止盈卖出**去重，避免重复卖单。

DRY_RUN 与实盘共用同一套成交处理路径，确保行为一致。

---

## 14. Rust 执行器与 Poly1271

- 位置：`rust-executor/`，产物 `rust-executor/target/release/clodds-rust-executor`。
- 协议：stdin/stdout 逐行 JSON（`{id, method, params}` → `{id, ok, result|error}`）。
- 方法：`auth_check`、`open_orders`、`balance`、`place_limit`、`cancel`。
- TS 封装：`src/execution/rust-clob-executor.ts`（`rustPlaceLimitOrder` / `rustBalance` / `rustAuthCheck`）。
- 触发条件：`execution.buyLimit` 检测到 `privateKey && funderAddress && signatureType === 3 && orderType === 'GTC'` 时改走 Rust。
- 余额接口：面板 `/api/trading/balance` 优先调用 `rustBalance()`（6 位小数换算 USDC）。

---

## 15. 配置参考（完整默认值）

来源：`DEFAULT_CONFIG`（`src/strategies/crypto-hft/index.ts`）。

### 资产与仓位
| 键 | 默认 | 含义 |
|----|------|------|
| `assets` | `[BTC,ETH,SOL,XRP]` | 交易资产 |
| `sizeUsd` | `2.5` | 单笔名义金额 |
| `minShares` / `maxShares` | `10` / `10` | 股数上下限 |
| `maxPositionUsd` | `2.5` | 单持仓上限 |
| `maxPositions` | `2` | 最大同时持仓（含挂单入场） |

### 回合时序
| 键 | 默认 | 含义 |
|----|------|------|
| `roundDurationSec` | `900` | 回合时长（15 分钟） |
| `minTimeLeftSec` | `180` | 少于该剩余时间不入场 |
| `entryCutoffSec` | `120` | 距 `minTimeLeftSec` 前停止挂入场单 |
| `minRoundAgeSec` | `30` | 回合开始前 N 秒不入场 |
| `forceExitSec` | `120` | 剩余该秒数强制离场 |
| `roundEndClearSec` | `180` | 回合末清挂单窗口 |
| `staleOrderAgeSec` | `25` | 挂单超过该秒数在末段被清 |
| `warmupSec` | `0` | 启动后预热 |

### 执行
| 键 | 默认 | 含义 |
|----|------|------|
| `entryOrder.mode` | `taker` | 默认入场模式（spread_arb 信号自带 `maker_then_taker`） |
| `entryOrder.makerTimeoutMs` | `5000` | maker 转 taker 超时 |
| `entryOrder.takerBufferCents` | `0.01` | taker 价格缓冲 |
| `exitOrder.mode` | `taker` | 出场模式 |
| `exitOrder.makerTimeoutMs` | `2000` | 止盈 maker 挂单多久未成交转 taker |
| `makerFirstExitEnabled` | `true` | 止盈先挂 0 费 maker，超时转 taker |
| `maxBidWickPct` | `8` | 止损要求 bid 距 mid 不超过该 %，防假挂单 |
| `maxOrderbookStaleMs` | `8000` | 盘口最大陈旧毫秒 |
| `sellCooldownMs` | `1000` | 卖出冷却 |

### 止盈止损 / 移动止盈
| 键 | 默认 | 含义 |
|----|------|------|
| `takeProfitPct` | `100` | 止盈（兜底，让赢家奔跑） |
| `stopLossPct` | `50` | 基础止损 |
| `trailingEnabled` | `true` | 移动止盈开关 |
| `trailingMinHighPct` | `15` | 高点 ≥ 该值才武装 |
| `minTrailPct` | `10` | 最小回撤点数 |
| `proportionalTrailEnabled` | `true` | 按高点比例回撤 |
| `proportionalTrailPct` | `15` | 高点回撤比例(%) |
| `simpleExitEnabled` | `true` | 简单出场模式 |
| `dynamicStopEnabled` | `true` | 动态止损 |
| `stopTightenStartSec` | `300` | 剩余该秒数开始收紧止损 |
| `stopMinPct` | `10` | 最紧止损点 |

### spread_arb / 趋势
| 键 | 默认 | 含义 |
|----|------|------|
| `spreadArbEntryFactor` | `0.95` | 旧版入场因子 |
| `trendMinPrice` | `0.55` | 趋势确认价 |
| `trendConfirmSec` | `60` | 趋势窗口 |
| `trendRatio` | `0.8` | 窗口内高于阈值的占比 |
| `trendBrokenPrice` | `0.35` | 跌破视为反转 |
| `trendEntryPrice` | `0` | 固定挂单价（0=用因子） |
| `trendEntryFactor` | `0.98` | 挂单价 = 中价 × 因子 |
| `trendMaxEntryPrice` | `0.45` | 挂单最高价 |
| `momentumFilterEnabled` | `true` | 动量对齐过滤 |
| `momentumFilterWindowSec` | `30` | 现货动量窗口 |
| `momentumFilterMinPct` | `0.03` | 允许的逆势幅度(%) |
| `maxFillVsMidPct` | `3` | 成交前高于 mid 的容差(%) |
| `cancelStaleBids` / `staleBidPct` | `false` / `5` | 陈旧挂单撤单 |

### 风控 / 冷却
| 键 | 默认 | 含义 |
|----|------|------|
| `maxDailyLossUsd` | `200` | 日亏损上限 |
| `stopLossCooldownSec` | `180` | 止损后冷却 |
| `breakerCooldownSec` | `300` | 熔断冷却 |
| `exitCooldownSec` | `60` | 同方向出场后冷却 |
| `assetCooldownSec` | `90` | 同资产冷却 |
| `lossCooldownSec` | `180` | 亏损后同资产冷却 |
| `negRisk` | `true` | 负风险市场 |
| `dryRun` | `true` | 模拟盘 |

---

## 16. 命令参考

技能命令（面板或任意渠道）：

```text
/crypto-hft start [ASSETS] [--size N] [--dry-run] [--preset NAME]
/crypto-hft stop
/crypto-hft status
/crypto-hft positions [N]
/crypto-hft markets
/crypto-hft round
/crypto-hft enable <strategy>
/crypto-hft disable <strategy>
/crypto-hft preset list | save <name> | load <name> | delete <name>
```

`config` 可在运行中动态改（节选）：

```text
/crypto-hft config --tp 100 --sl 50
/crypto-hft config --size 5 --max-pos 2 --max-loss 200
/crypto-hft config --trend-price 0.55 --trend-confirm 60 --trend-ratio 0.8
/crypto-hft config --trend-entry-factor 0.98 --trend-max-entry 0.45
/crypto-hft config --momentum-filter on --momentum-window 30 --momentum-tol 0.03
/crypto-hft config --max-fill-vs-mid 3
/crypto-hft config --min-age 30 --min-time-left 180 --entry-cutoff 120
/crypto-hft config --dyn-stop on --stop-tighten-start 300 --stop-min 10
/crypto-hft config --simple-exit on --exit-grace 3
/crypto-hft config --asset-cooldown 90 --loss-cooldown 180 --breaker-cooldown 300
/crypto-hft config --prop-trail on --prop-trail-pct 15 --prop-trail-min 15
/crypto-hft config --trail-min-high 15 --min-trail 10
```

完整正则见 `src/skills/bundled/crypto-hft/index.ts` 的 `config` 分支。

---

## 17. 数据文件与分析脚本

持久化目录（相对项目根）：

| 文件 | 内容 |
|------|------|
| `data/trades/trades.jsonl` | 每笔已平仓交易（含净 PnL、出场原因、是否 maker） |
| `data/trades/summary.json` | 累计汇总（胜率、总盈亏、最好/最差） |
| `data/orders/orders.jsonl` | 订单流水（提交/撤单/成交） |
| `data/signals/signals.jsonl` | 原始 spread_arb 信号（含上下文与 postMax/postMin） |
| `data/shadow/positions.jsonl` | 影子记录：持仓窗口内本侧(&对侧)价格路径 |

**影子引擎（`shadow-engine.ts`）**：从入场起采样到**本回合强制离场时刻**（`expiresAt - forceExitSec`，约到回合末前 120s；旧版固定 240s），≥500ms/次，同时记录本侧与对侧，并对每个样本保存可成交价 `b`(bid)/`a`(ask)（无则回退 mid），窗口结束落盘。用于回答：

- 当前出场策略是否过早/过晚？（用**同一份** `exit-policy.ts` 回放不同参数）
- 方向是否应反着做（动量假设）？

**分析脚本**：

```bash
node scripts/analyze-shadow.mjs     # 共享策略回放：实际 vs 参数网格 vs walk-forward 样本外
node scripts/analyze-signals.mjs    # 信号分桶：按 spot/trendAge/spread 等
```

> `analyze-shadow.mjs`（v3）直接 import 编译后的 `dist/.../exit-policy.js`，因此**回测的就是线上在跑的同一份出场代码**；运行前需 `npm run build`。
> 它同时给出三段数：实际净利、全样本网格最优（乐观上界）、以及 **walk-forward 样本外**（只用过去记录选参数、在下一条未见过的路径上验证），后者用于识别过拟合。样本不足时脚本会提示继续攒数据。

> 清理数据前建议备份：`data/backup-<时间戳>/`。

---

## 18. 故障排查

### 18.1 长时间没有成交

按顺序检查：

1. **引擎是否已启动**：进程运行 ≠ 引擎运行，需在面板点“启动”。
   ```bash
   grep -n "Crypto HFT engine starting" run.log | tail -1
   ```
2. **盘口是否新鲜**：日志中 `Evaluate context` 的 `upBook/downBook` 是否经常为 `false`。
   - 若大量 `false`：检查 Poly WS 连接与订阅。
   - `maxOrderbookStaleMs` 过小（默认 8000）会误判安静盘口为陈旧。
3. **Poly WS 重连后订阅是否丢失**：
   ```bash
   grep -c "No subscriptions to resubscribe" run.log
   grep -n "subscriptionCount" run.log | tail
   ```
   - 启动时应看到 `subscriptionCount: 8`（4 资产 × 2 代币）。
   - 若为 0：订阅未恢复，检查 `wirePolyWsToEngine` 的 `resubscribe()`（每 10s 重确认）。
4. **信号是否被过滤**：
   ```bash
   grep -c "spread_arb signal generated" run.log
   grep -c "Momentum filter: rejected" run.log
   ```
5. **挂单是否被频繁撤**：`trend broken — cancelled bids`、`Stale bid above mid` 太多说明被撤在成交前。

### 18.2 恶价成交（买贵）

历史现象：挂单成交价 0.41，而当时中价仅 0.23。
原因：用了扫描器的陈旧"最后价"挂单。
修复：`evaluateSpreadArb` **必须新鲜双边盘口**、按 `midPrice` 定价、`maxFillVsMidPct` 成交前复核。

### 18.3 Rust 执行器报错

- `Rust executor not built`：执行 `cargo build --release`。
- 下单 `auth` 失败：确认 `POLYMARKET_*` 与 `POLYMARKET_FUNDER_ADDRESS`，以及签名类型为 3。
- 手测认证：调用 `rustAuthCheck()` 或 `node -e` 小脚本。

### 18.4 面板无声音

- 浏览器需先有一次用户交互（音频上下文解锁）。
- 确认不是"下单/盈利/亏损"三类音混淆：下单=短嘀、盈利=琶音、亏损=咚。
- 胜率判定依赖状态里的 `wins/losses`；若引擎重启导致计数重置，面板会自动重新基线（不补放旧音）。

### 18.5 数据/计数异常

- 面板统计来自 **持久化 trades**（重启不丢），引擎内存统计每次启动清零。
- 需要重置：备份并清空 `data/{trades,orders,signals,shadow}/*.jsonl`。

---

## 19. 开发与测试

```bash
npm run typecheck          # tsc --noEmit
npm test                   # node --test（部分环境相关用例可能失败）
npm run build              # 编译 + 复制 SKILL.md
npm run ci                 # typecheck + test + build
```

约定：

- 修改 `src/**` 后必须 `npm run build` 再重启。
- `ui/hft.html` 为静态文件（no-cache），改后仅需刷新页面。
- 关键改动建议用影子/信号数据做前后对比。

---

## 20. 安全注意事项

- **不要提交 `.env`** 或任何私钥到仓库。
- 私钥仅通过环境变量注入；Rust 子进程继承 `process.env`。
- 实盘前务必先跑 DRY_RUN 验证若干回合。
- 先小额（`sizeUsd` 小、`maxPositions` 小），确认订单引擎与对账无孤儿单后再放大。
- 网络不稳定时 Poly WS 会频繁重连；确认订阅自愈生效后再长期运行。
- 面板无鉴权时不要暴露到公网（默认绑定 localhost）。

---

## 附：核心默认行为速查

- 主策略：`spread_arb`（趋势确认后回调低吸，maker_then_taker）。
- 方向过滤：现货动量对齐（容忍 0.03% 逆势）。
- 出场：简单模式（100% 止盈兜底 + 动态止损 + 15% 移动止盈 + 到期强平）。
- 风控：连亏 3 熔断 5 分钟、同资产/亏损/方向冷却、日损 $200 上限、最多 2 仓。
- 单笔 $2.5、10 股、超 0.45 不买。
- 默认 **DRY_RUN**，需在面板手动启动。
