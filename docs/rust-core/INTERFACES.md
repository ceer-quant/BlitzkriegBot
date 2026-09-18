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
| `engine.stats` | `{}` | `{ books, tops, spots, rounds, evaluations, signals, placeRejected, strategyLimitRejected, blocked:{timing,momentum,byStrategy,declaredExemptions}, confirmed:[...], confirmedDetail:[{token,mid,entry,cap,inBand}], strategies:[...]（P-1.1 按策略分账；E2-a 增 `maxOpenPositions`/`maxOpenNotionalUsd`（未配置为 null）、`sizingSource:"global"|"strategy"`、`effectiveSizeUsd`/`effectiveMinShares`/`effectiveMaxShares`；E2-b 增 `gateExemptions:string[]`（声明豁免的入场闸门，`[]`=全保留）、`blockedTiming`/`blockedMomentum`、`gateExemptedTiming`/`gateExemptedMomentum`）, archive:{ path, events, bytes, dropped, recording, rotateBytes, segmentBytes, segments, freeBytes, stoppedReason:"cap"\|"disk"\|"io"\|"locked"\|null }\|null（P-1.3 归档状态；**内核默认常开**，`--no-event-archive` 关闭 + 分段轮转 + 单写者锁） }`。`blocked.byStrategy` 把每次 timing/momentum 拦截归属到候选单所属策略（`{<name>:{timing,momentum}}`，全 0 省略），`blocked.declaredExemptions` 列出当前生效的全部豁免声明 `[{strategy,gates}]`；原 `timing`/`momentum` 全局总数语义不变。诊断列表按 token 排序、按调用时刻计算（回测报告内用虚拟钟），因此可复现、可 diff |

### 2.4 策略（P0.5）
| method | params | result |
|:---|:---|:---|
| `strategy.list` | `{}` | `{ "version": "1.1", "strategies": [{ "name", "enabled" }] }` |
| `strategy.enable` | `{ name, enabled }` | `{ name, enabled, found }` |
| `strategy.load` | `{ path }` | 成功 `"<name>@<version> registered into the engine dispatch (disabled)"`（注册后默认禁用，需再 `strategy.enable`；E2-b 起若该库导出可选符号 `bk_strategy_gate_exemptions`，回执在启用前显式追加 `; declares gate exemptions: timing[,momentum]`）；失败返回 `"Rejected { path, reason }"`（路径策略）/ `"Failed { path, reason }"`（dlopen/协商/`create` 失败）。走 **C ABI v2**：`bk_strategy_abi_version()` 必须为 2（无 v1 兼容层）；新能力一律以「按名字解析的可选符号」追加、vtable 结构体冻结，故 E2-b 不需要 ABI v3。`strategy-loading` 自 E7 起默认开启 |

`strategy.list` 自 E4-b 起返回**三个**内建：`spread_arb`（默认 `enabled:true`）、
`trend_follow`（默认 `enabled:false`，E4 的追涨腿，6 个可进化入场旋钮、
不声明任何门禁豁免）与 `mean_reversion`（默认 `enabled:false`，E4-b / #31 的逆向/fade 腿，
6 个可进化旋钮，**声明 `momentum` 门禁豁免**——`engine.stats.blocked.declaredExemptions`
因此常驻一条 `{"strategy":"mean_reversion","gates":["momentum"]}`）。开机态也可由 CLI 决定：`--enable-strategy <name>` / `--disable-strategy <name>`
（可重复，`--disable-strategy` 优先），走的是与 `strategy.enable` **同一个** `set_strategy_enabled`，
因此开机选择与运行期切换行为一致；回测/回放复用同一份 `CoreConfig`，同样吃这两个开关。
未知名只记警告日志并忽略（不会让进程失败）。

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

自 E2-c（[#28](https://github.com/ceer-quant/BlitzkriegBot/issues/28)）起**参数、评估、审计、回滚
全部按策略隔离**。因此 `apply`/`rollback` **必须**带 `strategy`；`history` 的 `strategy` 可选
（不给 = 全部策略）。协议版本号不变，为向后兼容的加项。

| method | params | result |
|:---|:---|:---|
| `shadow_evolution.enable` | `{}` | `{ "enabled": true }` |
| `shadow_evolution.disable` | `{}` | `{ "enabled": false }`（摘除覆盖层；已应用的值本身不变） |
| `shadow_evolution.status` | `{}` | `{ version, status, currentParams, variantCount, variants[], evolutionsApplied, evolutionsRejected, secondsSinceLastEvolution, strategies[] }` |
| `shadow_evolution.history` | `{ limit?, strategy? }` | `{ version, history: [AuditRecord...] }` |
| `shadow_evolution.apply` | `{ strategy, params }` | `{ "applied": true, "strategy": "…" }` |
| `shadow_evolution.rollback` | `{ strategy }` | `{ "rolledBack": true, "strategy": "…" }`（无历史则返回 error） |

`status` 的聚合键（`currentParams`/`variantCount`/…）**语义与形状保持不变**，另加
`strategies[]` 逐策略明细：

```
strategies[] = {
  strategy,                       // 策略名
  status,                         // 该策略的 EvolutionStatus；null = 未声明可进化旋钮
  params,                         // 该策略当前参数 { knob: "decimal-string" }；null = 不可进化
  knobs[],                        // 该策略声明的旋钮 { name, value, min, max }（字符串）
  evolutionsApplied, evolutionsRejected, secondsSinceLastEvolution
}
```

`params === null`（或 `status === null`）就是**「该策略不可进化」的线上表达**——不是错误状态。
`apply`/`rollback` 传入未声明旋钮的策略、或 `params` 里出现未声明的旋钮 / 越出 `[min,max]` /
单步超过 `max_gradient`，一律返回 error 且**不改动任何值**（多策略袋子也是全有或全无）。

新增事件：`EVOLUTION_SIGNAL`、`EVOLUTION_APPLIED`、`EVOLUTION_REJECTED`（均带 `strategy`）。
审计分文件：`data/evolution/<strategy>.jsonl`。
详见 `SHADOW_EVOLUTION.md`、`STRATEGY_GUIDE.md §3.6`。

## 3. 调用示例

### Node（门禁脚本内的裸 Node 客户端）
```js
import { CoreClient } from '../../scripts/lib/core-client.mjs';
const c = new CoreClient({ bin: './target/release/blitzkrieg-core' });
await c.boot();
await c.request('order.place', { tokenId: '123', conditionId: '0x..', side: 'buy',
  mode: 'taker', price: 0.42, size: 10, internalKey: 'k1', strategy: 'spread_arb',
  asset: 'BTC', direction: 'up', roundSlot: 1 });
console.log(await c.request('engine.stats'));
console.log(await c.request('strategy.list'));
await c.stop();
```

> 旧 TypeScript 客户端 `src/core/blitzkrieg-core-client.ts` 已随 Node 源码层删除
> （`62b16c88`）。现在唯一的 Node 侧实现是 `scripts/lib/core-client.mjs`
> ——零依赖、仅供门禁驱动临时内核使用，不是产品组成部分。

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
| 1.1 | E2-a：`engine.stats.strategies[]` 增 per-strategy 配额与生效定寸字段（协议加项，向后兼容，版本号不变） |
| 1.1 | E2-b：`engine.stats.blocked` 增 `byStrategy`/`declaredExemptions`，`strategies[]` 增 `gateExemptions`/blocked/gateExempted 字段；`strategy.load` 回执追加豁免声明。外挂新增可选符号 `bk_strategy_gate_exemptions`（未导出=不声明），vtable 与 `BK_ABI_VERSION=2` 冻结 |
| 1.1 | E2-c：`shadow_evolution.status` 增 `strategies[]`（每策略 status/params/knobs/计数），`history` 增可选 `strategy`，新增 `shadow_evolution.apply`，`rollback` 改为**必需** `strategy`；热参改为按策略命名空间（`ParamRegistry`），关闭进化时覆盖层**摘除**；审计分文件 `data/evolution/<strategy>.jsonl`。外挂新增可选符号 `bk_strategy_evolvable_knobs`（未导出=不可进化），vtable 与 `BK_ABI_VERSION=2` 仍冻结 |
| 1.1 | E7：策略接口全功能化，C ABI v2 全量钩子（含出场意图/多 tick 盘口/热参数/孪生工厂）；`strategy-loading` 默认开启（Issue #38） |
| 1.1 | E4-a：`Engine::new` 注册第三个内建策略 `trend_follow`（默认关闭）；CLI 增 `--enable-strategy`/`--disable-strategy`，回测/回放同吃（Issue #30） |
| 1.1 | E4-b：`Engine::new` 注册第四个内建策略 `mean_reversion`（默认关闭，E2-b momentum 豁免的第一个内建使用者——`blocked.declaredExemptions` 常驻 `{"strategy":"mean_reversion","gates":["momentum"]}`）；无任何 RPC schema 变化（Issue #31） |
