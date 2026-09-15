# 内核 ↔ 用户层接口（INTERFACES）

> 传输：Unix Domain Socket，一行一个 JSON 对象（`\n` 分帧），JSON-RPC 2.0。
> **每条消息都带 `version` 字段**（当前 `1.1`）；Rust serde 结构体是唯一契约，Node 侧用 zod 镜像校验。
> Node 不持有私钥：凭证由内核进程从自身环境读取。

## 0. 信封

请求（Node → 内核）：
```json
{ "jsonrpc": "2.0", "version": "1.1", "id": <any>, "method": "<name>", "params": { ... } }
```
成功响应：`{ "jsonrpc": "2.0", "id": <any>, "result": { ... } }`
失败响应：`{ "jsonrpc": "2.0", "id": <any>, "error": { "code": <int>, "message": "...", "data": { "coreCode": "<CODE>", "raw": "<venue text>" } } }`
事件推送：`{ "jsonrpc": "2.0", "method": "core.event", "params": { "kind": "...", ... } }`

错误码：`-32700` 解析 / `-32600` 非法请求 / `-32601` 未知方法 / `-32602` 参数错 / `-32000` 应用错误（详见 `data.coreCode`）。

`coreCode` 取值：`INVALID_PARAMS` `UNKNOWN_ORDER` `WOULD_CROSS` `INVALID_TICK_SIZE` `INVALID_SIZE`
`INSUFFICIENT_FUNDS` `RISK_REJECTED` `KILL_SWITCH_ACTIVE` `MARKET_HALTED` `NOT_AUTHENTICATED`
`VENUE_ERROR` `TIMEOUT` `INTERNAL`。

## 1. 内核 → 用户层 事件（`core.event`）

| kind | 说明 |
|:---|:---|
| `READY` | 内核就绪（version/mode） |
| `ORDER_UPDATE` | 订单状态变化（含完整 TrackedOrder） |
| `FILL` | 成交回报（含权威 FillDelta + 订单快照） |
| `POSITION_CLOSED` | 持仓平仓（含已实现 PnL、日 PnL） |
| `RISK_ALERT` | 风控告警（含 coreCode） |
| `RECONCILE_REPORT` | 对账结果（补成交 / 标记 / 幽灵单） |
| `ERROR` | 结构化错误（code + message + raw） |

> 规划中（占位）：`position.update`、`balance.update`、`extension.status`、`strategy.signal`（策略信号审计）。

## 2. 用户层 → 内核 请求

### 2.1 基础
| method | params | result |
|:---|:---|:---|
| `core.ping` | `{}` | `{ "pong": true, "ts": <ms> }` |
| `core.ready` | `{}` | `{ "version", "mode", "authenticated", "signer", "funder" }` |

### 2.2 订单 / 持仓 / 账本
| method | params | result |
|:---|:---|:---|
| `orders.place` | `{ tokenId, conditionId, side, mode, price, size, internalKey, strategy, asset, direction, roundSlot, makerTimeoutMs? }` | `{ "orderId", "status" }` |
| `orders.cancel` | `{ orderId }` | `{ "success": true }` |
| `orders.cancel_all` | `{ tokenId? }` | `{ "cancelled": <n> }` |
| `orders.list` | `{}` | `{ "orders": [TrackedOrder...] }` |
| `orders.reconcile` | `{ openOrderIds?, trades? }` | `{ filled, markedFilled, markedCancelled, ghostIds }` |
| `positions.list` | `{}` | `{ "positions": [PositionView...] }` |
| `positions.exit` | `{ positionId? }` | `{ "closed": <n> }` |
| `ledger.balance` | `{}` | `{ balance, reserved, available }` |

### 2.3 行情 / 引擎
| method | params | result |
|:---|:---|:---|
| `books.snapshot` | `{ tokenId, bids:[{price,size}], asks:[...] }` | `{ ok: true }`（含 dry 撮合：会驱动挂单穿越成交） |
| `books.top` | `{ tokenId, bestBid?, bestAsk? }` | `{ ok: true }` |
| `engine.book` | `{ tokenId, bids:[{price,size}], asks:[...] }` | `{ ok: true }`（P-1.2：**直送 `engine_on_data`**，不跑 dry 撮合——`--feed-ws` 原生 feed 与回测重放的同一条路径） |
| `spot.price` | `{ asset, price }` | `{ ok: true }` |
| `engine.markets` | `{ markets: [CryptoMarket...] }` | `{ ok: true }` |
| `engine.round` | `{}` | `{ slot, ageSec, timeLeftSec, markets, canTrade, marketPrices:[{asset,up,down}] }` |
| `engine.stats` | `{}` | `{ books, tops, spots, rounds, evaluations, signals, placeRejected, strategyLimitRejected, blocked:{timing,momentum}, confirmed:[...], confirmedDetail:[{token,mid,entry,cap,inBand}], strategies:[...]（P-1.1 按策略分账；E2-a 增 `maxOpenPositions`/`maxOpenNotionalUsd`（未配置为 null）、`sizingSource:"global"|"strategy"`、`effectiveSizeUsd`/`effectiveMinShares`/`effectiveMaxShares`）, archive:{ path, events, bytes, dropped, recording, rotateBytes, segmentBytes, segments, freeBytes, stoppedReason:"cap"\|"disk"\|"io"\|"locked"\|null }\|null（P-1.3 归档状态；**内核默认常开**，`--no-event-archive` 关闭 + 分段轮转 + 单写者锁） }`。诊断列表按 token 排序、按调用时刻计算（回测报告内用虚拟钟），因此可复现、可 diff |

### 2.4 策略（P0.5）
| method | params | result |
|:---|:---|:---|
| `strategy.list` | `{}` | `{ "version": "1.1", "strategies": [{ "name", "enabled" }] }` |
| `strategy.enable` | `{ name, enabled }` | `{ name, enabled, found }` |
| `strategy.load` | `{ path }` | 成功 `"<name>@<version> registered into the engine dispatch (disabled)"`（注册后默认禁用，需再 `strategy.enable`）；失败返回 `"Rejected { path, reason }"`（路径策略）/ `"Failed { path, reason }"`（dlopen/协商/`create` 失败）。走 **C ABI v2**：`bk_strategy_abi_version()` 必须为 2（无 v1 兼容层）。`strategy-loading` 自 E7 起默认开启 |

### 2.5 风控
| method | params | result |
|:---|:---|:---|
| `risk.kill` | `{ reason? }` | `{ "killed": true }` |
| `risk.resume` | `{}` | `{ "killed": false }` |

### 2.6 扩展与市场插件（P0.5 / P0.6）
| method | params | result |
|:---|:---|:---|
| `extension.list` | `{}` | `{ "version": "1.1", "extensions": [{ "name", "type", "state" }] }` |
| `extension.enable` | `{ name }` | `{ "name", "enabled": true }`（失败返回 `error`） |
| `extension.disable` | `{ name }` | `{ "name", "enabled": false }` |
| `market.list` | `{}` | `{ "version": "1.1", "active": "polymarket", "plugins": [{ "name", "type", "hasDataFeed", "hasDiscovery", "hasExecutor", "enabled", "active" }] }` |

`extension.*` = 通用审计/生命周期扩展（`Extension`，无交易能力）；
`market.list` = 市场插件（`MarketPlugin`，驱动下单/行情/发现）。市场选择用内核 CLI `--market-plugin <name>`。

### 2.7 影子进化（Shadow Evolution，opt-in）

| method | params | result |
|:---|:---|:---|
| `shadow_evolution.enable` | `{}` | `{ "enabled": true }` |
| `shadow_evolution.disable` | `{}` | `{ "enabled": false }` |
| `shadow_evolution.status` | `{}` | `{ version, status, currentParams, variantCount, variants[], evolutionsApplied, evolutionsRejected, secondsSinceLastEvolution }` |
| `shadow_evolution.history` | `{ limit? }` | `{ version, history: [AuditRecord...] }` |
| `shadow_evolution.rollback` | `{}` | `{ "rolledBack": true }`（无历史则返回 error） |

新增事件：`EVOLUTION_SIGNAL`、`EVOLUTION_APPLIED`、`EVOLUTION_REJECTED`。
详见 `SHADOW_EVOLUTION.md`。

## 3. 调用示例

### TypeScript（Node IPC client）
```ts
import { BlitzkriegCoreClient } from './core/blitzkrieg-core-client.js';
const c = new BlitzkriegCoreClient({ mode: 'dry' });
await c.start();
await c.placeOrder({ tokenId: '123', conditionId: '0x..', side: 'buy', mode: 'taker',
  price: 0.42, size: 10, internalKey: 'k1', strategy: 'spread_arb', asset: 'BTC',
  direction: 'up', roundSlot: 1 });
console.log(await c.stats());
console.log(await c.request('strategy.list'));
await c.stop();
```

### Rust（内核内 / 测试）
```rust
use blitzkrieg_core::service::{Core, CoreConfig};
let mut core = Core::new(CoreConfig { risk: /*...*/, ..Default::default() });
let (id, status) = core.place(req, 5000, now_ms)?;
let names = core.strategy_names();
core.set_strategy_enabled("spread_arb", false);
```

## 4. 版本变更记录

| 版本 | 变更 |
|:---|:---|
| 1.0 | P0–P4：orders/positions/ledger/books/engine/risk 方法；事件 ORDER_UPDATE/FILL/POSITION_CLOSED/RISK_ALERT/RECONCILE_REPORT/ERROR |
| 1.1 | P0.5：所有消息新增 `version` 字段；新增 `strategy.list/enable/load`、`extension.list`、`extension.enable/disable`；`engine.stats` 增 `blocked` |
| 1.1 | Shadow Evolution（opt-in）：`shadow_evolution.enable/disable/status/history/rollback` + `EVOLUTION_*` 事件 |
