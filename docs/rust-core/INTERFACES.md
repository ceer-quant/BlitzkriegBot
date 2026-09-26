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
| `system.version` | `{}` | `{ "version", "gitHash", "gitDirty", "buildDate", "target", "updateAvailable", "latestVersion", "autoUpdate", "checkEnabled", "lastCheckMs", "releaseUrl" }` —— 版本 + 构建来源 + 更新状态。`updateAvailable`/`latestVersion`/`lastCheckMs`/`releaseUrl` 可空；`updateAvailable` 是**三态**：`null` = 尚未检查（更新检查默认关闭）≠ `false` = 已是最新，消费方不得把 `null` 画成「已是最新」。只读、无副作用、**不取 Core 锁**（数据锁未就绪时也必须能回答）。逐字段契约见 `docs/VERSIONING.md` §5.3 |

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

**内核自身不注册任何策略**，因此出厂状态下 `strategy.list` 返回空数组，`engine.stats` 的
`strategies[]` 也是空的。曾经自带的三个（`spread_arb` / `trend_follow` / `mean_reversion`，
以及后来的 `pair_arb` / `dog`）已全部删除（见 [CHANGELOG.md](../../CHANGELOG.md)）；
列表里出现的每一行都来自被加载的 cdylib，且**注册后默认 `enabled:false`**。
门禁豁免声明因此不再是常驻项：`engine.stats.blocked.declaredExemptions` 只在某个已加载库
声明了豁免时才出现（曾经的 `mean_reversion` 条目随之消失）。

开机态也可由 CLI 决定：`--enable-strategy <name>` / `--disable-strategy <name>`
（可重复，`--disable-strategy` 优先），走的是与 `strategy.enable` **同一个** `set_strategy_enabled`，
因此开机选择与运行期切换行为一致；回测/回放复用同一份 `CoreConfig`，同样吃这两个开关。
未知名只记警告日志并忽略（不会让进程失败）——但**一个库都没解析到**时 `--enable-strategy`
会让进程**拒绝启动**，除非显式给 `--allow-zero-strategies`（#265：空注册表 + 显式启用意图
= 配置错误，不该静默跑一个不会交易的内核）。

### 2.5 风控
| method | params | result |
|:---|:---|:---|
| `risk.kill` | `{ reason? }` | `{ "killed": true }` |
| `risk.resume` | `{}` | `{ "killed": false }` |
| `risk.setLimits` | `{ maxOrderNotional?, maxOrderNotionalPct?, maxOpenNotionalUsd?, minShares?, maxShares?, reason? }` | `{ applied: [{field, from, to}], atMs, actor, reason?, persisted: false, note }` |

`risk.setLimits` 是**受限热加载**（Issue #191）：只改**开仓限额**——每笔名义上限
（`maxOrderNotional` / `maxOrderNotionalPct`）、组合开仓上限（`maxOpenNotionalUsd`）与开仓股数区间
（`minShares` / `maxShares`）。白名单之外的一切都要**重启**才生效，且是**明确拒绝**（`-32602`，
报文点名是哪个字段、为什么、以及改它需要重启）：

* 退出阈值（`stopLossPct` / `takeProfitPct`）——已开仓的持仓是在旧阈值下开的，热改会让它们的风险
  预算无声漂移；
* 日亏熔断（`maxDailyLossUsd` / `maxDailyLossEquityPct`）与连亏熔断
  （`maxConsecutiveLosses` / `breakerCooldownSec`）——判定依据是**已经跑起来**的当日已实现亏损账 /
  连亏计数，热改会追溯性地重新判定已经发生的事；
* `maxPositions`（持仓管理器与退出策略同块，需要整体评审）；
* 凭据（`privateKey`；内核从自身环境读取，见文首「Node 不持有私钥」）与账本本金
  （`seedBalance` / 启动的 `--seed-balance`）。

语义（三条，缺一不可）：

1. **只改内存、不落盘**。重启回落启动参数（CLI flag / env / TOML）；响应里的 `persisted: false`
   与 `note` 就是这个承诺的线上表达——运维不要在重启后惊讶限额变回去了。
2. **整个请求全有或全无**。任一字段非法（负数、`minShares > maxShares`、未知字段、白名单外的字段）
   则**一个都不应用**并返回 `-32602`；成功时 `applied` 逐字段给出旧值 → 新值。
3. **审计走既有日志**（`target: "risk"`，INFO）：每个变更字段一行，带
   `actor`（对端 uid）/ `field` / `from` / `to` / `reason`，与响应里的 `actor`/`atMs` 是同一份事实。
   不新增事件类型、不新增审计文件。

`risk.kill` / `risk.resume` 仍是进程级冻结；本方法只动限额，不改 kill 状态。

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

### 2.8 网络诊断（Net Check）

「是它还是我们？」——在一次「连不上」发生时区分**场馆侧故障**与**本机出网故障**。只读：
不碰订单、不碰账本、不落盘，唯一副作用是四个出站探测请求。

| method | params | result |
|:---|:---|:---|
| `net.check` | `{}` | `NetCheckReport`（见下） |

```
NetCheckReport = {
  ok,          // 所有已探测路径都通过；unsupported/rejected 不算通过
  tsMs,        // 报告生成时刻（毫秒时间戳）
  hintCode,    // 结论分类（按优先级取唯一值）：
               // unsupported / ok / tls_blocked / dns_failed / proxy_env / fake_ip / partial
  hint,        // 该分类的中文/英文一句话解释，可直接展示
  proxyEnv[],  // 进程环境里出现的代理变量名（只有名字，绝无取值）
  items[] = {
    name,      // venue-rest / discovery / spot-ws / venue-ws
    target,    // 被探测的 URL（已脱敏，不含凭证）
    ok,
    status,    // ok / dns_failed / tcp_refused / timeout / tls_cert / tls_error /
               // http_error / transport_error / unsupported / rejected
    addrs[],   // 解析出的地址（DNS 阶段的结果）
    fakeIp,    // 命中伪造 IP（GFW 式 DNS 污染）为 true
    ms,        // 该路径耗时
    detail     // 走到哪一步、哪一步失败的叙述
  }
}
```

路径含义：`venue-rest` = CLOB REST，`discovery` = Gamma 发现，`spot-ws` = Binance 现货
（动量过滤的参考价流），`venue-ws` = CLOB 用户成交流（仅实盘模式，但静默的流等于静默的账本）。

`venue-ws` 探的是**客户端真正拨的那个端点**：`POLYMARKET_WS_URL` 是 base，通道路径由 SDK 追加
（其 `normalize_base_endpoint` 先剥掉尾部 `/ws[/market|/user]`，`channel_endpoint` 再拼上
`/ws/user`），探针照抄这两步——拨 base 本身只会命中 CDN 的 404，那是探针问错了问题，不是流坏了。

三条契约：

1. **不探测需要凭证的东西**。缺 key 导致的失败与网络不通无法区分——而区分这两者正是它
   存在的理由。鉴权可达性是交易自检（trading self-check）的职责。
2. **`unsupported` / `rejected` 不算通过**（`ok=false`，但也不读作「网络故障」）。前者表示
   该市场插件不提供探测，后者表示被探测的 URL 未能通过主机校验（loopback/私有段/保留地址），
   因此**根本没有拨号**——`detail` 里带着校验器自己的拒绝理由。
3. **任何凭证都不入报告**：代理变量只以变量名出现（`proxyEnv[]`），且 `target` 在打印前剥掉
   URL 的 `user:password@` userinfo——环境变量常带凭证，而报告会出现在终端、网页和 JSON 里。

接口能力：探测实现在市场插件侧（`MarketPlugin::net_check`，默认返回 `unsupported`），因此
把 venue 换成别的市场，探测跟着换。

消费方（三处读取同一份报告，规则同源）：

| 界面 | 入口 |
|:---|:---|
| CLI | `blitzkrieg net-check [--json] [--socket <path>]`（走运行中的内核 IPC）；无内核时 `blitzkrieg-core --net-check`（一次性、输出 JSON、退出码 0=全通过 / 1=有失败） |
| TUI | `n` 键覆盖层；命令行 `netcheck` |
| WebUI | 设置页「网络诊断」卡片；`GET /api/netcheck`（读缓存，过期后台起探测）、`POST /api/netcheck/probe`（丢弃时间戳强制重探） |

> WebUI 不在请求里内联拨号：面板的 `serve()` 单线程 accept，一次十秒探测会冻结进程内所有
> 其它请求（包括面板自己的快照轮询）。因此路由只读缓存，探测在后台线程用独立连接完成。

### 2.9 v0.3 接口冻结（#329 Wave 0）

v0.3 的全部 IPC 变更是**加项**：新增方法、新增事件变体、既有响应加字段。
**协议版本号保持 `1.1` 不变**（与 §4 惯例一致：加项不改版本号）。设计真相见
`DEV_V0_3.md` §12；本节只写「已冻结、可依赖」的部分。

#### 2.9.1 两个只读信封（Wave 0 已落地）

两个方法与信封形状在 Wave 0 冻结并已进 `server.rs`。都只读、无副作用、**不取 Core 锁**
（对齐 `system.version` 的姿势）。二者的「空」是**如实回答**，不是占位符：

| method | params | result |
|:---|:---|:---|
| `kline.history` | `{ "symbol", "interval", "limit"? }` | `{ "symbol", "interval", "klines": [KlineView...] }` |
| `intent.audit.tail` | `{ "limit"?: 50, "accountId"?, "strategy"?, "decision"?: "rejected" }` | `{ "records": [IntentAuditRecord...], "total"? }` |

`kline.history`：`interval` 取 `sec1` / `sec5` / `sec15` / `min1` / `min5` / `min15` /
`hour1` / `hour4` / `day1`（共享类型 `KlineInterval` 的 wire 拼写）。
`KlineView` = `Kline` 的 camelCase 形状：`{ symbol, interval, openTimeMs, closeTimeMs,
open, high, low, close, volume, tradeCount, isClosed }`（Decimal 一律字符串）。
按 `openTimeMs` 升序；`limit` 默认 200、上限 1000——上限由聚合器执行，随 E29 落地。
**Wave 0 的事实**：聚合器尚不存在（E29），本方法只回照请求的 `symbol`/`interval` 与
**空 `klines` 列表**；E29 换成真实尾部数据时信封不变。

`intent.audit.tail`：`records` 为 `data/audit/intents.jsonl` 中**落盘记录的原样**（camelCase
对象，字段见 `DEV_V0_3.md` §3.4 的 `IntentAuditRecord`；`decision` 是带 `status`
标签的枚举对象）——不引入镜像类型，审计只有一种拼写。返回**最新** `limit` 条（默认 50），
在窗口内保持文件顺序（旧 → 新）；`total` 为过滤后的总条数。`decision` 过滤值与
`decision.status` 大小写不敏感匹配（`approved` / `modified` / `rejected`）。
**Wave 0 的事实**：写入方尚未存在（E25）；缺文件 = 空列表（JSONL 的既有规则，
坏行跳过不致命）。

相关事件（`KLINE_UPDATE` / `INTENT_DECISION`，形状见 `DEV_V0_3.md` §12.2/§12.3）
随各自 Epic（E29 / E25）落地——Wave 0 不预声明无发射方的变体。

#### 2.9.2 v0.3 其余契约（冻结方向，随 Epic 落地）

以下形状已在 `DEV_V0_3.md` §12 冻结；各 Epic 只实现、不再改形状。**在对应 Epic 合并前，
调用方不得假设它们存在**（旧内核返回 unknown method）：

| 变更 | 归属 Epic | 契约位置 |
|:---|:---|:---|
| `account.list` / `account.switch` / `account.status`（`AccountView`） | E28 | §12.1 |
| `kline.subscribe` / `kline.unsubscribe`（**会话级**，断连自动退订） | E29 | §12.2 |
| `risk.limits`（生效值 + `source` 四值 `default/toml/env/flag`） | E26 | §12.4、§4.4 |
| `market.list` 加 `structure`/`capabilities`/`capabilitiesBits`；`strategy.list` 加 `modes`/`compatible`/`incompatibleReason`；`strategy.load` 回执加 `; API 1.0 (line protocol 2)` | E27 / E24 | §12.5 |
| `positions.list` / `orders.list` 行加 `accountId`；`orders.place` / `ledger.balance` 加可选 `accountId`；`engine.stats` 加 `accounts`/`kline`；`core.ready` 加 `apiVersion`/`accountId` | E28 / E26 / E29 | §12.6 |

共享类型（`Kline`/`AccountId`/`MarketStructure`/`StrategyMode` 等）已于 Wave 0 落在
`market_api` / `strategy_api` 并由内核 re-export——与上面的方法不同，它们是**已编译的
既有类型**，可以直接依赖。

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
core.set_strategy_enabled("my_strategy", false);
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
| 1.1 | D-31：`timing` 豁免增可选**剩余时间下限**。`engine.stats.strategies[]` 增 `gateExemptionTimingFloorSec`（未声明为 `null`）；`blocked.declaredExemptions[]` 在声明了下限时增 `timingFloorSec`；`strategy.load` 回执在同一行打印 `(timing floor Ns)`。外挂侧复用**既有**可选符号 `bk_strategy_gate_exemptions` 的 JSON 增键 `timing_min_time_left_sec`（旧库不写=沿用内核默认），vtable 与 `BK_ABI_VERSION=2` 仍冻结 |
| 1.1 | E7：策略接口全功能化，C ABI v2 全量钩子（含出场意图/多 tick 盘口/热参数/孪生工厂）；`strategy-loading` 默认开启（Issue #38） |
| 1.1 | E4-a：`Engine::new` 注册第三个内建策略 `trend_follow`（默认关闭）；CLI 增 `--enable-strategy`/`--disable-strategy`，回测/回放同吃（Issue #30） |
| 1.1 | E4-b：`Engine::new` 注册第四个内建策略 `mean_reversion`（默认关闭，E2-b momentum 豁免的第一个内建使用者——`blocked.declaredExemptions` 常驻 `{"strategy":"mean_reversion","gates":["momentum"]}`）；无任何 RPC schema 变化（Issue #31） |
| 1.1 | 网络诊断：新增只读方法 `net.check`（`NetCheckReport`，见 §2.8），以及内核一次性开关 `--net-check`（输出 JSON、不起内核、不碰 `data/`）、启动器子命令 `blitzkrieg net-check [--json] [--socket]`、面板路由 `GET /api/netcheck` + `POST /api/netcheck/probe`。协议加项，向后兼容，版本号不变；探测能力来自市场插件（`MarketPlugin::net_check`，默认 `unsupported`） |
| 1.1 | 受限配置热加载（Issue #191）：新增 `risk.setLimits`（见 §2.5），只允许热改**开仓限额**（每笔/组合名义上限 + 开仓股数区间），白名单之外一律**明确拒绝**并要求重启；**只改内存、不落盘**（响应 `persisted: false`），审计走既有 `target: "risk"` INFO 日志（actor/字段/旧值/新值/reason）。协议加项，向后兼容，版本号不变 |
| 1.1 | 内核零策略（2026-09-23）：删除自带的 5 个策略（`spread_arb`/`trend_follow`/`mean_reversion`/`pair_arb`/`dog`）。**协议本身无变化**——`strategy.list` 的形状、`strategy.load` 的回执、`engine.stats` 的字段都没动，变的只是「出厂时列表为空」。上面几条 E4-a/E4-b 的记录保留为历史：它们描述的是当时的注册行为，那些注册点已不存在 |
| 1.1 | 版本单事实来源（docs/VERSIONING.md E-V1..V3，2026-09-25）：新增只读方法 `system.version`（版本 + 构建来源 + 更新三态；`updateAvailable: null=未检查 ≠ false=已是最新`；不取 Core 锁，数据锁未就绪也可回答），字段契约见 `docs/VERSIONING.md` §5.3。启动器同步补 `--version`/`-V`/`-v` 短路（不再落入 `run_unified` 启动路径，#228 同形风险）与 `blitzkrieg version [--json] [--core] [--socket]` 子命令（`--core` 即问本方法；旧内核返回 unknown method，UI 不得编造空版本）。workspace 全成员版本继承根 `[workspace.package].version`，由 `scripts/version-guard.mjs` 守卫。协议加项，向后兼容，版本号不变 |
| 1.1 | v0.3 Wave 0 接口冻结（#329）：§2.9 落进本文件——两个只读信封先落地（`kline.history` / `intent.audit.tail`，见 §2.9.1）；共享类型（`Kline`/`KlineInterval`/`AccountId`/`AccountStatus`/`CredentialKeys`/`MarketStructure`/`MarketCapabilities`/`MarketMode`/`StrategyMode`）落在 `market_api` / `strategy_api`，内核 re-export；C ABI 增第六个可选符号 `bk_strategy_declare_modes`（未导出=不声明；vtable 与 `BK_ABI_VERSION=2` 冻结）；`SafeStrategy` 增 `declare_modes()` / `on_kline()` 两个带默认实现的签名。§2.9.2 其余契约（`account.*`、`kline.subscribe`、`risk.limits`、既有方法字段扩展）随各 Epic 落地。协议加项，向后兼容，版本号不变 |
