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
| `KLINE_UPDATE` | K 线推送（E29，`kline` = KlineView）。**OPT-IN**：只有本连接显式 `kline.subscribe` 过的 `(symbol, interval)` 才会收到（§12.2）；断连即退订。闭合 bar 一 bar 一条**永不节流**；未闭合 bar（`isClosed:false`）是预览，限流 ≤1 条/秒/(symbol, interval) |

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
| `trades.history` | `{ limit? }` —— 缺省 50 | `{ "version": "1.1", "trades": [TradeRecord...] }` —— 已平仓历史（Node 兼容记录；`--trade-log` 在时优先读记录文件尾部，否则内存平仓簿尾部），最旧 → 最新 |
| `trades.summary` | `{ accountId? }` —— 缺省 = 全局累计 | `{ "version": "1.1", "summary": <TradeSummary> }` —— 持久化 `summary.json` 的**全期**累计（与窗口式 history 不同）；带 `accountId` 时把该账户行现场折叠成同形状汇总（pre-E28 行 accountId 为空归 default），旧核忽略该字段 |
| `ledger.balance` | `{}` | `{ balance, reserved, available }` |

### 2.2.1 账户（E28 · v1.1）
| method | params | result |
|:---|:---|:---|
| `account.list` | `{}` | `{ "version": "1.1", "active", "accounts": [{ id, name, marketType, status, statusReason?, balance, available, reserved, credentialsLoaded, openPositions, dayRealizedUsd, updatedAtMs }] }` —— `active` 是**本连接**的默认账户（§9.5：无会话切换时 = 进程级）；`credentialsLoaded` 只是布尔存在事实（§9.4：凭证值永不过线） |
| `account.switch` | `{ accountId }` | `{ "active": <id> }` —— **会话级**：只改本连接的默认账户，其他连接与进程级 active 不动（§9.5）；未知 id 拒 `INVALID_PARAMS` |
| `account.status` | `{ accountId, status, reason? }` | `{ id, status, statusReason? }` —— **只能收紧**（tighten-only）：loosen（如 `frozen→active`）一律拒 `INVALID_PARAMS`；`status` 走 wire 词表 `active`/`read_only`/`frozen`/`suspended`，裸 `suspended` 由 `reason` 兜底为 `statusReason` |

### 2.3 行情 / 引擎
| method | params | result |
|:---|:---|:---|
| `books.snapshot` | `{ tokenId, bids:[{price,size}], asks:[...] }` | `{ ok: true }`（含 dry 撮合：会驱动挂单穿越成交） |
| `books.top` | `{ tokenId, bestBid?, bestAsk? }` | `{ ok: true }` |
| `engine.book` | `{ tokenId, bids:[{price,size}], asks:[...] }` | `{ ok: true }`（P-1.2：**直送 `engine_on_data`**，不跑 dry 撮合——`--feed-ws` 原生 feed 与回测重放的同一条路径） |
| `spot.price` | `{ asset, price }` | `{ ok: true }` |
| `engine.markets` | `{ markets: [CryptoMarket...] }` | `{ ok: true }` |
| `engine.round` | `{}` | `{ slot, ageSec, timeLeftSec, markets, canTrade, marketPrices:[{asset,up,down}] }` |
| `engine.stats` | `{}` | `{ books, tops, spots, rounds, evaluations, signals, placeRejected, strategyLimitRejected, blocked:{timing,momentum,byStrategy,declaredExemptions}, confirmed:[...], confirmedDetail:[{token,mid,entry,cap,inBand}], strategies:[...]（P-1.1 按策略分账；E2-a 增 `maxOpenPositions`/`maxOpenNotionalUsd`（未配置为 null）、`sizingSource:"global"|"strategy"`、`effectiveSizeUsd`/`effectiveMinShares`/`effectiveMaxShares`；E2-b 增 `gateExemptions:string[]`（声明豁免的入场闸门，`[]`=全保留）、`blockedTiming`/`blockedMomentum`、`gateExemptedTiming`/`gateExemptedMomentum`）, archive:{ path, events, bytes, dropped, recording, rotateBytes, segmentBytes, segments, freeBytes, stoppedReason:"cap"\|"disk"\|"io"\|"locked"\|null }\|null（P-1.3 归档状态；**内核默认常开**，`--no-event-archive` 关闭 + 分段轮转 + 单写者锁） }`。`blocked.byStrategy` 把每次 timing/momentum 拦截归属到候选单所属策略（`{<name>:{timing,momentum}}`，全 0 省略），`blocked.declaredExemptions` 列出当前生效的全部豁免声明 `[{strategy,gates}]`；原 `timing`/`momentum` 全局总数语义不变。诊断列表按 token 排序、按调用时刻计算（回测报告内用虚拟钟），因此可复现、可 diff。E29 增 `kline`（聚合器计数器 `{symbols, liveBars, closedBars, droppedOutOfOrder, noDataBars, intervals[]}`，无引擎为 `null`——§10.3 的丢弃必须可见） |

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

#### 2.5.1 生效风控热写：risk.setSystemic / risk.setExit（2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `risk.setSystemic` | `{ maxSingleLossUsd?, maxDailyDrawdownUsd?, maxPositionSize?, maxConsecutiveLosses?, cooldownMinutes?, maxTotalPosition?, maxTotalExposureUsd?, maxCorrelationUsd?, globalKillSwitchLossUsd?, reason? }` —— 九项系统性限额，全部可选（只改点名的字段），金额/股数走 decimal-STRING（`"0"` = off） | `SystemicLimitUpdate`：`{ applied: [{field, from, to, effect}], atMs, actor, reason?, persisted: false, note }` —— `effect` ∈ `live`（六项限额，逐笔入场现场判读）/ `next_session`（回撤预算 + 连亏/冷静期对，启动才武装）；`persisted` 恒 `false`（热写只活本进程）。空 patch（不点任何字段）拒绝 |
| `risk.setExit` | `{ stopLossPct?, takeProfitPct?, forceExitSec?, trailingEnabled?, reason? }` —— Gate 4 逐笔绑定的退出三项 + trailing 总开关。百分比走 decimal-STRING 且必须 > 0（≤0 拒绝）；`forceExitSec: 0` = **关闭强平**（合法的出厂开关位，负值拒绝） | `ExitSetUpdate`：`{ applied: [{field, from, to, effect: "live"}], atMs, actor, reason?, persisted: false, note }` —— 三项对**新入场**立即生效（Gate 4 每意图现场绑定），退出扫描读同一活配置，已开仓位同步跟随；`persisted` 恒 `false`，重启回落启动 flag/env/TOML |
| `risk.limits` | `{}` | `{ version, account: {id, limits}, global: {limits}, exit: {stopLossPct, takeProfitPct, forceExitSec, trailingEnabled} }` —— 生效矩阵 + 来源披露（E26 §4.4）。**取 Core 锁**逐次从活 risk config 组装（`risk.setLimits`/`risk.setSystemic` 热写矩阵，boot 冻结快照会答出已不在生效的值），非只读锁免 |

#### 2.5.2 轮次时间周期 / 行情插件 / 凭据：设置卡三对读写（next_session 族，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `round_duration.get` | `{}` | `{ effectiveSec, source: "cli"\|"env"\|"runtime"\|"toml"\|"default", allowed: [{sec, label?}], nextSessionSec, persisted }` —— 白名单档位（`5m`/`15m`/`1h`/`4h`/`daily`；非标值 `label:null` 照列）；`nextSessionSec` = 持久格若有则格值（无论本会话 source），否则生效值 |
| `round_duration.set` | `{ durationSec?, reset?, reason? }` —— 二选一：`durationSec` 必须是白名单档位（300/900/3600/14400/86400）；`reset` 清格回落启动解析 | `{ nextSessionSec, reset, atMs, actor, reason?, note }` —— **next_session**：轮次网格是坐标系（`round_slot` = UTC 网格索引），热切会错解存量订单的过期时间；只写持久格 `data/runtime/config.json`，本会话网格不动。显式 `--round-sec` outrank 持久格 |
| `market.plugin.get` | `{}` | `{ activePlugin, registered: [{name, type, active}], nextSessionPlugin, persisted, flagPinned, source: "flag"\|"accounts"\|"persisted"\|"fallback", derivedVenues[] }` —— `source` 披露本会话启用集的四层解析；`accounts` 层生效时（绑定即启用）`derivedVenues` 列出派生集合 |
| `market.plugin.set` | `{ name?, reset?, reason? }` —— 二选一：`name` 必须是**已注册**插件名（plan-first，注册表编译在内核里）；`reset` 清格 | `{ nextSessionPlugin, reset, atMs, actor, reason?, note }` —— **next_session**：feed/discovery/executor 生命周期都绑启动选择，热切架构上不可行。**绑定即启用 guard**：本会话启用集由账户 venue 绑定派生（`source=accounts`）时**整体拒绝**（持久格是死格，错误点名编辑面是 `account.venue.set`）；显式 `--market-plugin` outrank 持久格 |
| `market.credentials.status` | `{}` | `{ rows: [{plugin, requiredVars[], resolvedVars[], ready}] }` —— 每个注册插件需要的 env 变量名 + 下一会话是否齐全（真实 env 或持久格，格输给 env）。**只有变量名，值永不过线**（§9.4） |
| `market.credentials.set` | `{ plugin, vars?, reset?, reason? }` —— `vars` 是 env 变量名 → 值的映射；变量名按插件需求表 plan-first 校验（表外名字拒绝，typo 不允许静默变成惰性 executor）；`reset: true` 清该插件的格项；空 `vars` = 清项 | `{ plugin, written[], reset, atMs, actor, reason?, note }` —— **值只落格不上行、回执永不回显**；executor 在 boot 时一次性读环境，本会话不动，重启后 boot 注入（真实 env 优先） |

#### 2.5.3 订单处置补遗（E31-c / #457，2026-10-26 补册）

与 `orders.cancel` 同形三兄弟 —— 各自独立成方法，为的是 ops 轨迹里读得出**操作意图**的差别：

| method | params | result |
|:---|:---|:---|
| `orders.cancel_remaining` | `{ orderId }` | `{ "success": true }` —— E31-c：撤掉部分成交单仍在挂的剩余量，已成交股数不动 |
| `orders.close_filled` | `{ orderId }` | `{ "closed": <0\|1> }` —— E31-c：只平掉该单**已成交累积**的持仓；定价、定寸、风控、提交 SELL 都是内核做，策略只点名订单 |
| `orders.force_close` | `{ orderId }` | `{ "success": true }` —— #457：对 GHOST 单（轮次已过期、永不成交、诚实终态回执未到）做内核侧强平；**活轮拒绝**（那是 `orders.cancel` 的活），审计记 `ForceClosed` 使「我们撤回」与「我们放弃等待」可区分 |

#### 2.5.4 策略热参数（Issue #481，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `strategy.params.get` | `{ name }` | `{ name, found, tunables: [{name, type?, value, min, max}], evolvable, paramsAttached, note? }` —— schema 源是策略**自己的**旋钮声明（Lua manifest `tunables`），内核不发明域；数字全部 decimal-STRING；`tunables: []` = 该策略不可进化（不是错误状态）；`paramsAttached: false` = 写路径仍可用但值要等内核下次 set 的 attach 才上台 |
| `strategy.params.set` | `{ name, params: { <knob>: <decimal-string>, … }, reason? }` | `{ name, applied: [{knob, from, to, effect: "live"}], atMs, actor, reason?, persisted: false, note }` —— 逐旋钮对照声明域校验（plan first, write later）；多旋钮 patch **原子**（一个坏值整体拒绝、什么都不动）；热生效：策略下个 evaluate 周期合并 registry cell；重启回落 manifest 缺省（+已采纳的进化晋升） |
| `strategy.unload` | `{ name }` | 文本回执：`"<name> unloaded (engine dispatch removed); library freed"` / `"not found: …"` / `"rejected: …"`（仍启用、in-tree 或仍有持仓时拒绝） |
| `strategy.reload` | `{ name, path }` | 文本回执（E9-b 原子换装）：先装新库、同名字解析成功才卸旧实例；失败旧实例原样活着 —— 坏构建永远不能把在跑的 dispatch 洗白 |

#### 2.5.5 统一映射确认面（RFC 568 · ④ 操作员裁决，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `unified.list` | `{}` | `{ version, events: [<已确认映射>...] }` —— 每事件：`id`/`title`/`status`/`settlementSource`/`venues`（持腿方）/偏差标记。只读快照，面板配对视图数据源 |
| `unified.candidates` | `{}` | `{ version, candidates: [{a, b, score, expiryDeltaMs}] }` —— 本轮跨所配对候选（自动匹配提议、操作员裁决），只读；`a`/`b` 是 `PlatformListing` 原样（confirm 往返不改字节） |
| `unified.confirm` | `{ title, listings: [PlatformListing…≥2], actor? }` —— `listings` 必须来自**不同** venue；`actor` 缺省 `"panel"` | `{ version, event: {id, title, status, venues[]} }` —— 账本侧全部重校验（标题、资产、一挂牌一事件）；被拒的 confirm 原样报错、不动任何东西 |
| `unified.revoke` | `{ eventId, actor? }` | `{ version, revoked: "<eventId>" }` —— 撤回已确认映射（账本审计 + Engine 副本同步移除，策略立即看不见） |

#### 2.5.6 跨所套利腿配置：arb.config.set / arb.config.get（Wire contract v1，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `arb.config.set` | `{ version: 1, legAVenue, legAAccount, legBVenue, legBAccount, pairs: [asset…], actor? }` —— fail-closed 逐字段校验：`version` 必须为 1、四腿字段非空、两 venue 必须不同、账户必须存在且（绑定时）覆盖本腿 venue、`pairs` 非空；每条拒绝都是 APPLICATION 错误**点名**出错字段 | `{ version, configured: true }` —— 写入 = accounts.toml `[cross_venue_arb]` + 内存镜像 + engine push **同一事务**（被拒的写什么都不动）；下次 boot 重读文件（手改坏文件 fail-closed） |
| `arb.config.get` | `{}` | 配置态：`{ version, configured: true, legAVenue, legAAccount, legBVenue, legBAccount, pairs }`；未配置态：`{ version, configured: false, legAVenue: null, legAAccount: null, legBVenue: null, legBAccount: null, pairs: [] }` —— **永无部分形状** |

### 2.10 账户簿运行时管理 + 显示指针（2026-10-26 补册）

账户配置面在 §2.2.1（E28 三读一换）之外的全部写路径 + 钱包流水读面：

| method | params | result |
|:---|:---|:---|
| `account.create` | `{ id, name?, venue?, actor?, reason? }` —— `default` 保留拒绝（零配置部署与 active 回退格共同持有它）、空 id 拒绝、重复 id 拒绝；非空 `venue` 必须是已注册插件（plan-first）；**无任何凭据字段**（§9.4：凭据建户后走 `market.credentials.set` 的 env 名路径） | `{ id, persisted: true, atMs, actor, reason?, note, runtimeEnabled }` —— 身份落内存簿 AND accounts.toml（persist 先行，落盘失败整体拒绝）；状态恒 `active`（运行时铸一个受限账户 = 变相放宽，收紧是 `account.status` 的活）；`runtimeEnabled` = 绑定即启用的运行时半边：带 venue 建户即绑定，该 venue 的 feed/discovery 同请求挂进本会话 |
| `account.remove` | `{ id, actor?, reason? }` | `{ id, persisted: true, atMs, actor, reason?, note }` —— 实现裁决：**`default` 可删**（零配置回退下次 boot 自再派生），但**最后一个账户**与**仍有持仓**的账户拒绝；删进程级 active 账户时回退首本存活账户；同名重建是新账本（流水文件随删，旧文件残留会幽灵回放上一世流水） |
| `account.freeze` | `{ accountId, frozen, reason? }` | `{ accountId, frozen, reason?, note }` —— 账户级急停：`frozen: true` 拒该账户**新订单**（手动 + 策略入场，引擎丢弃其候选），EXIT 照常流动；**会话级**（自动冻结的解除同在内存，重启清零重探测）；重复冻结保留**首个** reason |
| `account.trading.set` | `{ accountId, enabled, reason? }` | `{ accountId, enabled, persisted: true, atMs, reason?, note }` —— `false` = 该账户**新开仓**拒绝、既有仓位继续策略管理到平（EXIT 照常）；落 accounts.toml，重启重读 |
| `account.strategies.set` | `{ accountId, strategies: [name…], reason? }` —— 名字对**已注册**集合 plan-first 校验 | `{ accountId, strategies, keptExitsOnly: [name…], persisted: true, atMs, reason?, note }` —— 集合点名哪些 (strategy, account) scope 在跑；被移出但仍有持仓的策略进 `keptExitsOnly`，exits-only 跑到平仓（移除策略永不孤儿化持仓） |
| `account.rounds.set` | `{ accountId, rounds: [sec…], reason? }` —— **整集替换**（不是增量），白名单校验 + 去重；空集 = 从 accounts.toml 移除该键（无限制拼写） | `{ accountId, rounds, persisted: true, atMs, actor, reason?, note, runtimeEnabled }` —— 轮次多族 B2：写 (venue, duration) 族集，**一个事务**（persist 先行）；删除仍有持仓的族**整体拒绝**；无暴露的删除软卸该族产出；新增族运行时即挂 discovery 实例（`runtimeEnabled: true`） |
| `account.venue.get` | `{ accountId? }` —— 缺省/空 = 全部账户 | `{ version, venues: [{accountId, venue, updatedAtMs, actor}], active }` —— 绑定读数；空 `venue` = 未绑定（可交易任何 venue）；从未重绑的行 `actor: "boot"`；`active` = 本连接缺省账户（§9.5 链） |
| `account.venue.set` | `{ accountId, venue, reason? }` —— 非 `venue` 空串必须是已注册插件（plan-first）；空串 = 解绑；同一 venue 绑多账户是部署模型（一所多 API 套件），**不查唯一性** | `{ accountId, venue, persisted: true, atMs, actor, reason?, note, runtimeEnabled }` —— 绑定落内存 AND accounts.toml 一事务；此绑定门 `orders.place` 跨所（拒跨所单守卫读内存格）；`runtimeEnabled` = 本会话 feed/discovery 挂载事实（已挂/解绑时 `false`） |
| `display.account.set` | `{ accountId, reason? }` | `{ accountId, displayVenue, persisted: true, atMs, reason?, note }` —— **纯展示指针**：把快照 `displayAccount`/`displayVenue` 指向指定账户；不接 active/记账/策略启用轴，无任何交易语义；内核校验账户存在且 `active`（展示一本不能交易的账户 = 撒谎的快照），落 accounts.toml 顶层 `display_account` 键 |
| `account.journal` | `{ accountId?, limit? }` —— 缺省账户 = 本连接缺省（§9.5 链）；`limit` 缺省 100、夹取 1..=500 | `{ accountId, rows: [{atMs, kind, amount, balanceAfter, ref}], truncated }` —— #509 流水读面：append-only `JournalEntry` 尾窗（升序，内存窗口不碰文件）；`truncated: true` = 窗口不是全史 |

### 2.11 执行策略 #363（execution policy，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `execution_policy.list` | `{}` | `{ loaded, defaults: <section-view\|null>, accounts: [{accountId, section}] }` —— 每账户 section + 生效全局缺省（自身 override 折叠过 globals 后的**生效视图**）。只读 |
| `execution_policy.get` | `{ accountId }` | 生效 section 视图（`{budgetRatio?, minBudgetUsd?, maxBudgetUsd?, minEquityUsd?, maxPositionsPerAsset, rules?}`，decimal 走 string 最短规则）；无 section 且无 policy → `INVALID_PARAMS` |
| `execution_policy.set` | `{ accountId, budgetRatio?, minBudgetUsd?, maxBudgetUsd?, minEquityUsd?, maxPositionsPerAsset?, rules? }` —— `null` 字段继承 globals；`rules` 出现即**整组替换**（`[]` = 清空）；`cooldownSec` 是账户常备冷却 | `{ accountId }` —— section-replace 写 TOML（不重写其他账户）→ 内存 full-reload → 一行审计。文件 re-parse 失败时内存保**最后好策略**（fail-closed：坏写永不裸奔交易）并答 `INVALID_PARAMS` |
| `execution_policy.reset` | `{ accountId }` | `{ accountId }` —— 移除该账户 section（回落 globals）、full-reload、审计 |
| `execution_policy.history` | `{ accountId? }` —— 缺省 = 全部 | `[<PolicyAuditRecord>...]`：`[{tsMs, actor, action, accountId, before?, after?, error?}]` |
| `execution_policy.preview` | `{ accountId }` | `{ considered, skipped, avgBudgetUsd?, rows: [{tsMs, symbol, balance, price, verdict, detail?}] }` —— #364：当前 section 在该账户近 100 笔已平仓上重放（`evaluate` 本尊裁决，预览永不与内核分歧）；`verdict` ∈ `place`/`skip`/`cooldown`/`refused`。只读、无审计行 |

### 2.12 回测作业面 #353（backtest jobs，2026-10-26 补册）

六臂全部前置内核内作业注册表：WebUI 只点名数据集/资产/策略名——**永不触碰本地文件系统、永不点名可执行代码**（硬边界）。作业是本进程内的 tokio 任务；作业表在内存（随进程死），数据集与报告 sidecar 落盘可跨重启。

| method | params | result |
|:---|:---|:---|
| `backtest.onchain.pull` | `{ wallet, start, end, assets?, market? }` —— `start`/`end` 是 `YYYY-MM-DD`（UTC，end 含）或 epoch 秒；空 `assets` = 该钱包全部交易（一个 pull）；点名资产各扇出一个**可续传、缓存寻址**的 pull（每资产一个 jobId） | `{ jobIds: [<id>…] }` —— 异步起作业；输出根是内核数据集目录，**不是调用方参数**（WebUI 可达面无任意路径写） |
| `backtest.onchain.list` | `{}` | `{ outDir, datasets: [{dataset, eventsPath, manifest}] }` —— 数据集根全量清单（CLI pull 同布局同根），最旧在前；半写 manifest 跳过 |
| `backtest.run` | `{ archive, mode?, verifyLatencyMs?, slippageTicks?, tickMs?, tailMs?, strategies? }` —— `archive` 必须是 pull 产出的数据集（内核数据集目录下）；`mode` ∈ `mine`（缺省）\|`verify`\|`sweep`（#351 阶梯）；`slippageTicks` 低于 1 拒绝（诚实摩擦下限）；`strategies` 只收**名字**（未知名是作业失败信息，永不 shell） | `{ jobId }` —— 回放用活内核 config 的**快照**驱动独立 replay 实例（交易侧继续跑，无 #199 ledger-dir 争用） |
| `backtest.status` | `{ id }` | `JobView`（`{jobId, kind, state, phase, detail, progress, startedAtMs, finishedAtMs?, error?}`）+ 完成时附 `result` —— `state` ∈ `running`/`done`/`failed`；pull 的 `phase` ∈ trades/markets/prices/convert/done，回放恒 `replay` |
| `backtest.result` | `{ id }` | 完整结果文档（pull：`{asset, trades, events, cacheHit, tradesPath, eventsPath, manifestPath}`；回测：含 `archive` 的报告 JSON）；未完成作业报错并提示轮询 status |
| `backtest.export` | `{ id }` | `{ fileName: "<stem>.report.json", content: <pretty JSON 字符串> }` —— 命名报告文档，浏览器负责保存（WebUI 本身不碰文件系统） |

### 2.13 蓝图编辑器面 #362/#393（blueprint，2026-10-26 补册）

WebUI 画布产出蓝图 JSON，一切碰盘的动作都请内核做——UI 永不直接触文件系统（仓库铁律）：

| method | params | result |
|:---|:---|:---|
| `blueprint.compile` | `{ json }` —— 蓝图文档以 **STRING** 过线（不是嵌套对象），编译错误才能引用编辑器发来的精确字节 | `{ lua }` —— 生成的 Lua 源码原文；拒绝即编译器错误（自带 node id），wire 类 `INVALID_PARAMS` |
| `blueprint.save` | `{ name, json, overwrite? }` —— `name` 是包身份（目录名 = manifest 名）；内核**现场重编译**（调用方发陈旧 `lua` 不是东西）；`overwrite` 必须显式才能替换 loader 已扫描的包（静默覆盖可能换掉操作员以为认识的策略） | `{ name, packageDir, blueprintPath, luaPath, manifestPath, luaSha256, bytes: [bp, lua, manifest], tunables? }` —— 路径全部**相对**内核策略根；`luaSha256` 与 manifest 内同源（回执可对文件复核）；包落 `blueprint.json` + `strategy.lua` + `manifest.json`，下次启动照常发现 |
| `blueprint.load` | `{ name }` | `{ name, blueprintPath, json }` —— #393 存档包的蓝图文档原样读回（编辑器 preload），只读；名字过 save 同款闸门（读不出策略根） |
| `blueprint.loadSource` | `{ name }` | `{ name, packageDir, luaPath, manifestPath, lua, sha256, manifest }` —— 手写包「在蓝图中打开」（用户裁决：编辑入口必须能开所有 Lua 包）：入口文件原样 + manifest 元数据（`modes`/`author`/`description` 走原样 JSON 供往返）；`sha256` 是当前入口文件的 manifest 摘要 |
| `blueprint.saveSource` | `{ name, lua, overwrite? }` —— 目标包必须**已存在**（编辑不创建，创建走 blueprint.save）；`overwrite` 显式，与 save 同裁决 | `{ name, packageDir, luaPath, manifestPath, luaSha256, bytes: [lua, manifest] }` —— 写回后 sha256 现场重算进 manifest（装载器照常验字节） |

### 2.14 自更新与交易环境重置（#379 / #458，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `system.update.configure` | `{ checkEnabled?, autoUpdate? }` | `{ checkEnabled, autoUpdate }` —— 更新开关读写（§7.4）；写落审计记录**且必须持久化**（静默回退的开关是撒谎的开关），持久化失败如实报错 |
| `system.update.check` | `{}` | `system.version` 同形状 payload —— 手动检查一次；`checkEnabled=false` 时拒绝（开关含义是「可以出去查」，不是「可以自动装」）；结论**持久化**（手动臂历史上跳过落盘，失败检查重启即蒸发——已修）；失败明细进 `data/update/audit.jsonl` |
| `system.update.stage` | `{}` | `StageState`：`{phase, detail?, version?, asset?, sha256?, stagedAtMs?}` —— #379：下载+校验新版到 `data/update/staging/`；`autoUpdate=false` 时拒绝（调度器与手动调用同一开关）；**替换是 launcher 的活**（§7.5）——本臂只推进状态机到 restart-ready，永不触碰在跑的二进制 |
| `system.reset_trading` | `{ confirm: "RESET", reason?, accountId? }` —— `confirm` 必须逐字 `"RESET"`（自由文本确认门，不是勾选框）；`accountId` 缺省 = 本连接缺省账户（§9.5 链，与 `orders.place` 同一解析），显式 id 照字面 | `ResetTradingResult`：`{ disabledStrategies[], cancelledOrders, clearedOpenPositions, clearedClosedTrades, accountId, balances: [[id, seed]…], preSummary: {orderLogLines, tradeLogLines, positionLogLines, openPositions, liveOrders, balance}, atMs, actor, reason? }` —— 停用策略、撤挂单、清该账户的订单/成交/持仓记录与内存盘口、现金重置为 seed；**Live 模式拒绝**（真金白银记录是 append-only 证据）；每次调用追加 manual-ops 审计行 |

### 2.15 零散补册（费用报价 / 盘口深度 / 影子进化工作流，2026-10-26 补册）

| method | params | result |
|:---|:---|:---|
| `core.feeQuote` | `{ price? }` —— 缺省 0.5（费率表最宽处） | `{ model, rate, exponent, price, feePerShare, feePctOfPrice, makerFeePerShare: 0, modelMatches }` —— #182：内核实收 taker 费率表的只读报价；门禁脚本不再自带费率公式副本（副本察觉不到内核换默认）；`modelMatches: false` = 仓库钉死的参数与实收曲线分裂（分裂是**上报**的，不静默糊掉） |
| `engine.books` | `{}` | `[<AssetBooksView>…]`：`[{venue, asset, upTokenId, downTokenId, up: <BookSideView>, down: <BookSideView>}]`，`BookSideView = {bids[], asks[], bestBid?, bestAsk?, midPrice?, obi?, spread?, spreadPct?}` —— 引擎实盘同款 L2 深度读数（纯观察、零副作用，盘口深度卡数据源）；全部指标无盘口时 `null`（从不假装 0）；行带 venue 标签（含镜像盘合成行） |
| `shadow_evolution.proposals` | `{ limit? }` —— 缺省 100 | `{ version, proposals: [<EvolutionProposal>…] }` —— E13：全部已知提案的最新态（UI 对比表数据源） |
| `shadow_evolution.decide` | `{ id, decision }` —— `decision` ∈ `accept`/`reject`/`defer` | `{ decision: "accepted"\|"rejected"\|"deferred", proposal }` —— 操作员对单个被 HOLD 提案的裁决；热路径零改动 |
| `shadow_evolution.set_auto` | `{ enabled: <bool> }` | `{ autoEvolve: <bool> }` —— 自动进化开关（**持久化**，重启保持所择模式） |

### 2.16 版本对账门禁（2026-10-26，Issue #644）

`scripts/interfaces-methods-check.mjs` 是本表与实现的**结构对账门禁**：从
`core/blitzkrieg_core/src/ipc/schema.rs` 的 `pub mod method` 提取全部 `pub const *_METHOD` 常量
（`EVENT_METHOD` 与错误码 i32 常量除外），从本文件 §2 提取已记载方法名，差集非空即
exit 1 并点名缺失清单——schema.rs 新增方法常量而 INTERFACES.md 没有对应表格行的
PR 直接红。接入 `.github/workflows/ci.yml` 的 `core-gates`（§A.4 反向验收风格：
先红后绿的静态门禁，无需内核二进制）。

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
open, high, low, close, volume, tradeCount, isClosed }`（Decimal 走 `market_api`
decimal 序列化：有限值为 JSON number，非有限值退化为字符串）。
按 `openTimeMs` 升序；`limit` 默认 200、上限 1000。
**E29 已落地**：内核内置 K 线聚合器（§10.3：数据驱动闭合、乱序丢弃计数、静默空桶计数），
本方法回答「闭合尾 + 生长 bar 最后」的真实序列——最后一根 `isClosed:false` 是正在生长的
bar；无引擎（或该序列无数据）时回**空 `klines` 列表**，信封不变。喂入源是既有数据通路
（预测 token 按盘口 mid、spot 按现货价），非独立成交 tape。

`intent.audit.tail`：`records` 为 `data/audit/intents.jsonl` 中**落盘记录的原样**（camelCase
对象，字段见 `DEV_V0_3.md` §3.4 的 `IntentAuditRecord`；`decision` 是带 `status`
标签的枚举对象）——不引入镜像类型，审计只有一种拼写。返回**最新** `limit` 条（默认 50），
在窗口内保持文件顺序（旧 → 新）；`total` 为过滤后的总条数。`decision` 过滤值与
`decision.status` 大小写不敏感匹配（`approved` / `modified` / `rejected`）。
**Wave 0 的事实**：写入方尚未存在（E25）；缺文件 = 空列表（JSONL 的既有规则，
坏行跳过不致命）。

相关事件：`KLINE_UPDATE` 已随 E29 落地（见 §1——OPT-IN、闭合不节流、预览 ≤1/s）；
`INTENT_DECISION` 随 E25 落地。形状见 `DEV_V0_3.md` §12.2/§12.3。

#### 2.9.2 v0.3 其余契约（冻结方向，随 Epic 落地）

以下形状已在 `DEV_V0_3.md` §12 冻结；各 Epic 只实现、不再改形状。**在对应 Epic 合并前，
调用方不得假设它们存在**（旧内核返回 unknown method）：

| 变更 | 归属 Epic | 契约位置 |
|:---|:---|:---|
| `account.list` / `account.switch` / `account.status`（`AccountView`） | E28 | §12.1（其余账户管理写路径已落地：§2.10） |
| ~~`kline.subscribe` / `kline.unsubscribe`（**会话级**，断连自动退订）~~ **✅ E29 已落地**（§2.9.3） | E29 | §12.2 |
| `risk.limits`（生效值 + `source` 四值 `default/toml/env/flag`） | E26 | §12.4、§4.4（**✅ 已落地**，读数记档：§2.5.1；`source` 来源披露现由 `round_duration.get` / `market.plugin.get` 各自承载，见 §2.5.2） |
| `market.list` 加 `structure`/`capabilities`/`capabilitiesBits`；`strategy.list` 加 `modes`/`compatible`/`incompatibleReason`；`strategy.load` 回执加 `; API 1.0 (line protocol 2)` | E27 / E24 | §12.5 |
| `positions.list` / `orders.list` 行加 `accountId`；`orders.place` / `ledger.balance` 加可选 `accountId`；`engine.stats` 加 ~~`accounts`~~/`kline`（**`kline` ✅ E29 已落地**：聚合器计数器 `{symbols, liveBars, closedBars, droppedOutOfOrder, noDataBars, intervals[]}`，无引擎为 `null`）；`core.ready` 加 `apiVersion`/`accountId` | E28 / E26 / E29 | §12.6 |

#### 2.9.3 K 线订阅（E29 已落地）

| method | params | result |
|:---|:---|:---|
| `kline.subscribe` | `{ "symbols": [..], "intervals": [KlineInterval..] }` | `{ "subscribed": <本会话集合大小> }` |
| `kline.unsubscribe` | `{ "symbols": [..], "intervals": [KlineInterval..] }` | `{ "subscribed": <本会话集合大小> }` |

**会话级**：订阅集活在本连接的会话状态里（§9.5 同域），`kline.subscribe` 只写本连接的
集合——两个面板各自订阅互不相扰；**断连即退订**（集合随会话死亡，重连从零开始，不存在
双推）。`KLINE_UPDATE` 是唯一需要 OPT-IN 的事件（§1）：不订阅就一条 K 线都收不到。
同一 `(symbol, interval)` 重复 subscribe 幂等（集合去重，计数不涨）。锁中毒（对端任务
panic 时持有集合）→ 拒 `INTERNAL`，如实报错而不是编造计数。

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
| 1.1 | v0.3 E24 — BlitzkriegStrategy API 1.0 封口（#330）：① `strategy.load` 成功回执**追加** ` ; API 1.0 (line protocol 2)`（保留既有文本不替换）；② 可选符号 `bk_strategy_declare_modes` 的载荷由 §7.4 校验器验证，**非法 → 拒载**，错误含 `index` 与 `got`（`{"modes":[]}` = `Empty` 非法，不是「未声明」；未导出符号 / NULL = 未声明）；③ 三个保留键 `suggested_stop_loss` / `suggested_take_profit` / `suggested_max_hold_sec` **封口**（从未存在于 0.2 代码）：intent 解析遇键 → `warn!(target:"strategy")` 记录策略名/键名/token 并**丢弃该键**、不拒单；④ 门禁 `strategy:no-stop-loss-check`（策略源码零止损逻辑 + intent 零保留键）与 `strategy:declaration-check`（合法 3 例 / 非法 5 例，真实内核）接入 `core-gates`；⑤ 官方 Rust 示例 `user_layer/examples/momentum_alpha/`（嵌套 workspace，声明 `prediction/binary_outcome_wheel` + websocket_feed/level2_snapshot，零止损逻辑）。协议加项，向后兼容，`BK_ABI_VERSION=2` 不变 |
| 1.1 | v0.3 E28 — 多市场多账户一等公民（§2.2.1）：新增 `account.list` / `account.switch` / `account.status` 三方法（§9.3-9.5：`AccountLedgers` 每账户独立账本；`account.switch` 只写**会话级** active，进程级不动；`account.status` **只能收紧**，loosen 拒 `INVALID_PARAMS`）。`accounts.toml` 缺省 → 单一 `default`（0.2 部署零改动）；`orders.place` 未带 `accountId` 时注入本连接默认（显式 id 永远获胜）；`positions.list` 每行增 `accountId`；Gate 2 按账户姿态放行（`permits_order`），跨账户平仓显式拒绝（从不改道）。协议加项，向后兼容，版本号不变 |
| 1.1 | v0.3 E29 — K 线聚合器与呈现（§12.2/§10）：① 内核内置 `KlineAggregator`（§10.3：数据驱动闭合、乱序输入丢弃且计数 `droppedOutOfOrder`、静默空桶计 `noDataBars`、闭合历史有界 deque 上限 1000 对齐 `kline.history` 上限）；喂入源=既有数据通路（`engine.book`/`books.*` 臂按盘口 mid 喂 token、`spot.price` 喂 asset；价格 ≤0 跳过）。② `EngineStrategy` 增 `on_kline(&Kline)` 默认方法——只推**闭合** bar（§10.4），默认 no-op；Lua 适配层同步转发（E30 的 `bk_on_kline` 由此得数据）。③ 新增 `kline.subscribe` / `kline.unsubscribe`（会话级，断连自动退订；§2.9.3）+ `KLINE_UPDATE` 事件（唯一 OPT-IN 事件：闭合 bar 永不节流、未闭合预览 ≤1/s/(symbol,interval)）。④ `kline.history` 从 Wave-0 空信封换成真实尾部（信封不变）；`engine.stats` 增 `kline` 聚合器计数器。⑤ 门禁 `kline-aggregation-check`（--teeth 6 变异 + 真跑 11 断言）接入 core-gates。协议加项，向后兼容，版本号不变 |
| 1.1 | 2026-10-26 对账（Issue #644）：本表与 `schema.rs` 的方法差集一次性清零（对账基线 102/102 全记载，新增 §2.5.1–§2.5.6、§2.10–§2.16）。E29 之后积压未入册的面全部补齐：生效风控热写 `risk.setSystemic` / `risk.setExit`（含 trailing 开关的资金敏感三态语义，以实现记档）、`risk.limits` 读数；设置卡三对 `round_duration.*` / `market.plugin.*` / `market.credentials.*`（next_session 族 + 绑定即启用 guard）；E31-c/#457 的 `orders.cancel_remaining` / `close_filled` / `force_close`；#481 的 `strategy.params.get/set` 与 E9-b 的 `strategy.reload/unload`；RFC 568 的 `unified.list/candidates/confirm/revoke` 与跨所腿配置 `arb.config.get/set`；账户簿运行时管理九臂（`account.create/remove/freeze/trading.set/strategies.set/rounds.set/venue.get/venue.set/journal`）+ `display.account.set`；#363 执行策略六臂；#353 回测作业六臂；#362/#393 蓝图五臂（含 `blueprint.loadSource`/`saveSource`）；#379/#458 的 `system.update.check/configure/stage` 与 `system.reset_trading`；#182 `core.feeQuote`、`engine.books`、E13 影子进化工作流三臂（`shadow_evolution.proposals/decide/set_auto`）。**本票只补文档，零 wire/行为改动**；文档与实现如有语义出入以实现记档（见 PR 正文「语义存疑清单」）。新增结构对账门禁 `scripts/interfaces-methods-check.mjs`（schema.rs 方法常量 ↔ §2 表行差集，接入 core-gates；§2.16） |
| 1.1 | v0.3 RFC 568 阶段一 + E-VA —— cdylib 策略能力面与 Lua 拉平（#642）：新增四个可选符号（**未导出=缺省**，缺省语义与既有 6 符号同型；vtable 15 槽与 `BK_ABI_VERSION=2` 仍冻结）：① `bk_strategy_on_kline(void* h, const BkKlineView*)`——K 线推拉平（§10.4 同一分派点：只推**闭合** bar、闭合序；`BkKlineView` 为借用 decimal-STRING 镜像，`interval` 用 `KlineInterval` 线拼写，与 Lua `bk_on_kline` 表逐字段同构）；② `bk_strategy_entry_pair(void* h) -> char*`——装载期 pair 缺省 `{"key":..,"leg":..}`（RFC 568 透传语义：`key` 仅可读、防重不依赖，`leg` 腿标签；两通道：evaluate JSON 的 per-entry `pair` 字段恒赢过缺省，两者皆缺 = `pair: None` 旧行为逐位不变），Lua 侧 `pair={key=,leg=}` 透传同型；③ `bk_strategy_trailing_exit(void* h) -> i32`——三态声明 `-1`=随内核 / `1`=trail / `0`=never（出界读作 -1），对齐 Lua manifest `trailing_exit`；④ `bk_strategy_atomic_batch(void* h) -> i32`——E-VA 整批声明（`1`=原子批，其余=逐单），对齐 Lua manifest `atomic_batch`。四项声明在 `strategy.load` 回执与 trait 面（`wants_trailing_exit`/`atomic_batch`/`entry_pair`）同答；反向验收 `tests/foreign_optional_symbols_check.rs` 经真实 loader 驱动手写 fixture（导出全部四符号）与 parity 库（一概未导出）差分。协议加项，向后兼容，版本号不变 |
