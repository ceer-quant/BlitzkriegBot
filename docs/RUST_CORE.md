# Rust 交易核心（rust-core）— 架构与 P0 状态

> **P0.5 更新（2026-09-13）**：目录与 crate 已改名：`rust-core/` → `Blitzkrieg_core/`，
> crate `blitzkrieg-rust-core` → `blitzkrieg-core`（lib `blitzkrieg_core`）；UI 移入 `ui/`（旧
> `/webchat/hft.html` 仍兼容）。本文档中出现的旧路径/旧 crate 名可按此对照。新的分层与
> 扩展体系见 `docs/blitzkrieg/{ARCHITECTURE,INTERFACES,STRATEGY_GUIDE,EXTENSION_GUIDE,MIGRATION_LOG}.md`。
>
> **P0.6 更新（2026-09-14，目录规整）**：Rust 工作区改为四层布局，**crate/二进制名不变**
> （仍是 `blitzkrieg-core`，产物仍在仓库根 `target/release/blitzkrieg-core`）：
> - `Blitzkrieg_core/` → **`core/blitzkrieg_core/`**（小写、归入 `core/`）
> - `market_api/` → **`core/market_api/`**
> - `ui_kit/` → **`ui/ui_kit/`**，`ui_kit_panel/` → **`ui/ui_kit_panel/`**
> - `extensions/polymarket/`、`user_layer/` 位置不变
>
> 路径依赖相应更新（core→market_api/extension/user_layer、extension→market_api）。
> 本文档及历史迁移日志中出现的旧路径按 P0.5/P0.6 两条对照即可。


## 架构原则

本项目采用 **Rust 核心 + Node.js 外壳**：

- **所有涉及资金、订单、风控、状态一致性的逻辑在 Rust 侧**（`rust-core/`）。
- **Node 只负责 UI、策略参数编排、人类可读日志**。Node 通过本地 UDS 驱动核心，
  本身**不持有私钥、不直接调用 CLOB、不维护订单状态机、不做资金计算、不吞原始错误**。

## 进程与通信

```
Node (UI / 参数 / 日志)
   │  spawn + 生命周期管理
   ▼
blitzkrieg-core  ── Unix Domain Socket ($TMPDIR/blitzkrieg-core-$USER.sock)
                   ── 一行一个 JSON 对象（'\n' 分帧），JSON-RPC 2.0
```

- Node→Rust：`{jsonrpc:"2.0",id,method,params}`
- Rust→Node 响应：`{jsonrpc:"2.0",id,result|error}`
- Rust→Node 事件：`{jsonrpc:"2.0",method:"core.event",params:{kind,...}}`
- 契约唯一来源：Rust 的 serde 结构体（`model.rs` / `ipc/schema.rs`）；Node 用 zod 镜像校验
  （`src/core/schema.ts`），非法消息拒绝，不使用 any。
- 凭证：Rust 进程自己读 `POLYMARKET_PRIVATE_KEY` / `POLYMARKET_FUNDER_ADDRESS`；
  Node 不显式传私钥。
- **socket 名是四方契约**（内核 / `ui_kit` / `ui_panel` / TS 外壳），任一方算错都会
  「找不到内核 → 再起一个 → 两个内核共用同一份订单/持仓日志」（并触发归档单写者锁）。
  命名与解析因此统一在 `core-socket`：TS `src/core/core-socket.ts`、脚本
  `scripts/lib/core-socket.mjs`、Rust `core/.../main.rs` 与 `ui/ui_kit/src/lib.rs`。
  **兼容期**：`blitzkrieg-core-<user>.sock` 是改名前的名字，仍可发现——旧外壳用 `--socket <旧路径>`
  显式拉起的内核，新客户端会**领养**它而不是另起一个（`resolve_socket_path()` /
  `resolveSocketPath()`）；显式指定 socket 的调用方（夹具/测试）行为不变。
  回归见 `scripts/socket-migration-check.mjs`（`MIGRATION_LOG §38`）。

## Rust crate 模块（P0）

| 模块 | 职责 |
|---|---|
| `decimal.rs` | 金额/价格/数量的 Decimal 边界编解码（f64/string → Decimal，核心内不用 f64 算钱） |
| `model.rs` | Order/Fill/状态/`FillPolicy`/结构化错误码/事件 |
| `ome.rs` | 权威订单状态机；按 tradeKey 的幂等成交账本、累计 delta、按单 size 封顶、加权均价、FAILED 回滚、未知成交缓冲 |
| `ledger.rs` | USDC 预扣/释放/成交结算，防止超额下单 |
| `risk.rs` | kill switch、单笔名义上限（完整日亏/连亏/冷却在 P1/P2） |
| `sim.rs` | Dry 撮合：taker 即时成交、maker 盘口穿越成交（与 live 共用同一 OME）；`FillModel`（taker 滑点 / maker 延迟 / 成交概率，**默认恒等**） |
| `marketdata.rs` | 本地 L2 重建：快照/增量/top-of-book，交叉自愈，陈旧判定 |
| `signal.rs` | 纯信号：PriceBuffer、TrendTracker（滚动窗口确认/破裂）、`spread_arb` 评估器（mid/bestBid 定价纪律） |
| `scanner.rs` | 回合/时钟偏移/slug/时间闸；Gamma 字段解析（outcomes/clobTokenIds/outcomePrices） |
| `strategies/` | 内建策略（宿主化 `EngineStrategy` 实现）：`spread_arb.rs`（抄底腿，默认启用）、`trend_follow.rs`（追涨腿，E4-a，默认禁用，6 个可进化旋钮）、`mean_reversion.rs`（逆向/fade 腿，E4-b / #31，默认禁用，6 个可进化旋钮，momentum 豁免）、`shadow_twin.rs`（影子孪生工厂契约）、`foreign.rs`（C ABI v2 外挂适配） |
| `engine.rs` | 自驱引擎：事件（book/top/spot/round）→ 各策略各自确认 → 候选单 → 共享闸门 → 下单；现货动量过滤；趋势破裂撤单；按策略分账 |
| `feed.rs` | **Rust 原生行情**（P4）：Polymarket 盘口 WS（SDK，含动态重订阅）+ Binance 现货 WS（tokio-tungstenite），自带重连，带内解析 |
| `shadow.rs` | 影子采样 + 回放：记录持仓价格路径，用**同一份 exit_policy** 回放验证（不会与实盘漂移） |
| `exit_policy.rs` | **纯出场决策**（Rust 移植 TS `exit-policy.ts`）：基于可成交 bid 的 TP/SL/移动止盈/棘轮/保本/停滞/深度/时间/强平，bid 闪崩不触发保护性止损 |
| `position.rs` | 持仓账本：开/平仓 PnL（含费）、HWM/退出状态、`can_open` 容量与冷却（asset/loss/exit/stoploss）、手动平仓 |
| `service.rs` | 装配 ome/ledger/risk/breaker/positions/sim 的命令层；dry/live 同构 |
| `reconcile.rs` | **纯对账引擎**：WS 缺口补成交、幽灵单/半成交检测与状态修复（可单测） |
| `venue.rs` | **Live CLOB actor**：封装认证 SDK（下单 GTC/FOK + postOnly、撤单、余额、挂单/成交快照），持有用户 WS 流并转成 core 成交事件 |
| `live.rs` | **Live 桥**：提交未绑定订单、泵送 venue 事件到 core、每 ~5s REST 对账、启动时用真实余额播种账本 |
| `ipc/schema.rs` | JSON-RPC 契约 |
| `ipc/server.rs` | UDS server：多会话、事件广播、维护 tick、信号退出 |
| `data_source.rs` | **数据抽象（P-1.3）**：`DataSource`/`DataSink` trait、JSONL 事件归档（`--engine` 会话默认常开，`--no-event-archive` 关闭）、`ReplaySource`/`SegmentSource` 回放源、事件编解码 |
| `backtest.rs` | **事件驱动回测（P-1.2）**：`Backtester` trait + `EventBacktester`（虚拟时钟驱动真实 `Core`）+ 可序列化报告 |

**dry / shadow / 回测 / live 同核心**：Dry 模式不模拟成交后另起状态机，而是通过
`orders.place` + 盘口穿越走和 live 完全相同的 OME/账本/风控路径。Live 适配器通过
`place_pending / confirm_live / reject_live / ingest_fill` 把交易所确认和用户 WS
成交喂入同一套代码。

## IPC 方法（P0）

`core.ping` · `core.ready` · `risk.kill` · `risk.resume` ·
`orders.place` · `orders.cancel` · `orders.cancel_all` · `orders.list` ·
`orders.reconcile` · `ledger.balance` · `positions.list` · `positions.exit` ·
`books.snapshot` · `books.top` · `spot.price` · `engine.markets` · `engine.round` · `engine.book`
（P3：Node 通过 `books.*`/`spot.price`/`engine.markets` 喂数据，Rust 引擎自驱决策并于内部 tick 下单；
P4 起 Rust 自带 WS 接入后这些桥将退回为可选。P-1.2 新增 **`engine.book`**：把 L2 盘口**直送
`engine_on_data`**、**不跑** dry 撮合——与 `--feed-ws` 原生 feed 同一条路径，也是回测重放的路径；
`books.snapshot` 则保留 dry 撮合语义。两者差异见 D-11）。

事件：`ORDER_UPDATE`、`FILL`（含权威 `FillDelta`）、`POSITION_CLOSED`、`RISK_ALERT`、
`RECONCILE_REPORT`、`ERROR`、`READY`。
应用错误：`error.code=-32000`，`error.data.coreCode` 为结构化码（如 `RISK_REJECTED`、
`INSUFFICIENT_FUNDS`、`KILL_SWITCH_ACTIVE`），`raw` 永远保留交易所原文。

## 下单模式 FillPolicy

- `taker`：FOK 式即时全成交。
- `maker`：post-only 挂单，盘口穿越（BUY 时 bestAsk≤限价 / SELL 时 bestBid≥限价）才成交。
- `maker_then_taker`：先挂 maker，`makerTimeoutMs` 未成交则撤单按 taker 吃单。

## 构建与验收

```bash
npm run core:build     # cargo build --release（产物 target/release/blitzkrieg-core）
npm run core:test      # Rust 单测（core 库 131 个：OME/账本/风控/熔断/撮合/对账/出场/持仓/信号/扫描/L2/data_source/回测）
npm run core:parity    # 真实 Node RustCoreClient 经 UDS 跑端到端 parity 断言
npm run core:parity-engines  # 引擎对照：Node 决策管线 vs Rust 自驱引擎，同一输数据逐笔比对
npm run core:observe   # 真实回合 DRY 观察：发现当前回合代币喂给 Rust 引擎并实时监控
node scripts/cycle-check.mjs      # 回合全链路（发现→入场→出场→账本）离线检查
node scripts/backtest-check.mjs   # P-1.2 验收：捕获真实 core 的行情流，离线重放要求结果一致
node scripts/secret-scan.sh       # 凭证/密钥泄漏扫描（提交前必跑）

# 离线 walk-forward：对影子 JSONL 跑同一份出场策略的网格 + 样本外评估，
# 并按入场价 / 入场时剩余时间分桶（辅助判断入场闸门是否值得调整）
target/release/blitzkrieg-core --replay data/shadow/positions.jsonl

# 近失回放：评估"放宽入场闸"的净效果（需先有 near-miss 记录，见下）
target/release/blitzkrieg-core --replay-near-miss data/shadow/near-miss.jsonl
```

Node 客户端：`src/core/blitzkrieg-core-client.ts`（spawn/就绪握手/崩溃重启/超时/zod 校验/事件订阅/reconcile）。

## 事件驱动回测 + 数据归档（P-1.2 / P-1.3）

没有第二套模拟引擎：`EventBacktester` 持有真实的 `Core`（强制 `Dry`、无落盘、无 feed、无 shadow），
在虚拟时钟上跑 **同一条** 行情→信号→风控→账本→OME→出场链路，事件经 **同一个 `engine_on_data`** 入口投递。

```bash
# 采集：真实 core 把喂给引擎的原始事件按到达顺序落盘（live 侧零改动，只多一个 writer）
# 常开默认：--engine 会话无需任何归档参数，自动落 data/archive/events.jsonl
# （rotate 256 MB / 无会话上限 / 保留 ≥5 GB 空闲）；--no-event-archive 关闭。
target/release/blitzkrieg-core --mode dry --engine --feed-ws --assets BTC,ETH,SOL,XRP

# 需要换路径或调参时显式给出（显式值优先于默认）：
target/release/blitzkrieg-core --mode dry --engine --feed-ws --assets BTC,ETH,SOL,XRP \
  --event-archive data/archive/events.jsonl \
  --event-archive-rotate-mb 256 --event-archive-max-mb 0 --event-archive-min-free-mb 5120

# 回放：同一份二进制、同一组策略参数（务必一致，否则比较无意义）
target/release/blitzkrieg-core --backtest data/archive/events.jsonl \
  --assets BTC,ETH,SOL,XRP --min-round-age 30 --min-time-left 180 \
  --backtest-tick-ms 50 --backtest-tail-ms 0 --backtest-report report.json
```

- **默认常开**：只要带 `--engine`，采集即默认打开（路径 `data/archive/events.jsonl`，相对内核 cwd）。
  这是唯一**事后无法重建**的数据——某笔止损发生时的盘口路径；"默认关"意味着需要时它一定不在。
  `--no-event-archive` 显式关闭（供测试/夹具使用），显式 `--event-archive <path>` 覆盖默认路径；
  调参 `--event-archive-{max,rotate,min-free}-mb` 显式给出即生效。
- **单写者**：归档文件加排他 advisory 锁。第二个指向同一归档的内核不会交错写行、也不会把文件从对方
  脚下轮转走，而是干净地停录（`stoppedReason:"locked"`）。
- **归档格式**：JSONL，每行一个事件 `{"at":<ms>,"k":"book|top|spot|round",...}`，Decimal 全部以字符串
  精确编码（无浮点损失）。到上限**丢弃新事件**、绝不删除已有行；`engine.stats.archive` 暴露
  `{path,events,bytes,dropped,recording,rotateBytes,segmentBytes,segments,freeBytes,stoppedReason}`。
- **分段轮转（常开采集的前提）**：`--event-archive-rotate-mb 256` 到量即把当前段改名成 UTC 时间戳兄弟
  文件（`events.jsonl` → `events.20250914T140000Z.jsonl`）并续写新 `events.jsonl`；**只改名、不删除**，
  同秒多次轮转用零填充序号消歧。`--event-archive-min-free-mb 5120` 在每次轮转点检查可用空间，低于阈值
  即停录（`stoppedReason:"disk"`）——真实 feed 吞吐 **≈11 MB/分钟 ≈16 GB/天**，无限期采集的界是磁盘而
  非文件大小，`--event-archive-max-mb 0`（无会话上限）+ 轮转 + 磁盘护栏才是"常开"的正确组合。
- **多段重放**：`--backtest events.jsonl` 自动读入该归档**及其全部轮转段**（按名称时间戳排序、live 段在
  后），无需人工拼接；`sourceStats` 跨段汇总。
- **维护节拍**：回放的 `tick + engine_evaluate` 跑在**自己的 `tick_ms` 定时表**上（与 live 的
  `ipc::server` interval 同频），**与事件密度无关**。真实 feed 是亚毫秒级突发，若按"事件间隙"驱动
  评估周期，13 分钟真实归档只会跑 803 个周期（应为 15 610），出场检查变粗、`blockedTiming/Momentum`
  计数被饿死——已修复并加回归测试（`dense_stream_keeps_live_evaluation_cadence`）。
- **乱序**：多流（Binance spot + CLOB book/top）到达顺序天然抖动——实测 1 025 963 事件中 73 332 个
  时间戳回退，中位滞后 11 ms / p99 767 ms / 最大 8.6 s。`ReplaySource` 不重排，只把时钟**钳制**为不回退
  （`outOfOrderEvents` 计数上报），与 live 的"到达顺序即真实顺序"一致。
- **摩擦旋钮**（默认恒等 = 与 live 逐位可比）：`--slippage-ticks`（买价上浮/卖价下压，1 tick=0.001）、
  `--latency-ms`（maker 挂单延迟生效）、`--fill-prob-bps`（按订单 id 确定性抽签的成交概率）。
- **报告**：`orders{orders,filled,cancelled,rejected,failed,liveAtEnd}`、`fills`、`trades`（含盈亏、
  胜率、盈亏比、最大回撤）、`strategies[]`（按策略分账）、`feed`（计数器 + `blocked` + `confirmed`）、
  `riskAlerts`/`errors`、`tradeLines`。报告**逐字节可复现**：诊断列表按 token 排序（`HashSet` 迭代序
  每进程随机），诊断取**虚拟时钟**而非宿主时钟（否则离线回放的盘口全部"过期"，mid 显示为 0）。
- **验收**：`scripts/backtest-check.mjs` 同一驱动两侧（`engine.book` 采集）**21/21 逐位相等**——订单/成交/
  平仓/净盈亏/分策略账本全部一致，同样事件数、无乱序；真实 feed 归档（1 025 963 事件 / 13 分钟）重放
  **19/19**：归档逐类计数 = 回放 feed 计数、`evaluations`=span/tick、`blocked.momentum` 88=88、
  `blocked.timing` 810 vs 805（≤1%，宿主定时器相位）、`confirmed` 集合与 mid/entry/cap/inBand 逐值一致。
  已知非对称：live 的 `engine.stats` 快照比 SIGTERM 早约 5 ms，比归档少 6 个 top 事件（归档含、回放全量消费）。

## Live 运行

```bash
target/release/blitzkrieg-core --socket <path> --mode live \
  --max-order-notional 2.5 --market 0x<conditionId> [--market 0x...]
```

### 自驱 + Rust 原生行情（P3/P4）

```bash
# Node 只下发参数/回合代币，Rust 自带 WS 取行情并自行决策下单：
target/release/blitzkrieg-core --socket <path> --mode dry --engine --feed-ws \
  --min-round-age 30 --min-time-left 180
```

- `--engine`：启用自驱引擎（事件→趋势→spread_arb→下单→趋势破裂撤单）。
- `--feed-ws`：Rust 自带 Binance 现货 WS（构造时/boot 即连）与 Polymarket 盘口 WS
  （收到 `engine.markets` 后按当前回合代币订阅，回合切换自动重订阅）。缺网时自动重连。
- `--round-sec 300|900|...`：回合时长，必须与所交易市场一致（影响 slot 与时间闸；默认 900）。
- 未开 `--feed-ws` 时，仍可用 IPC `books.*` / `spot.price` / `engine.markets` 手动喂数据
  （测试与运维覆盖用）。

### 真实回合 DRY 观察

```bash
# 发现当前回合代币 → 喂给 Rust 自驱引擎（DRY）→ 实时监控订单/成交/风控/错误
node scripts/dry-observe.mjs --assets BTC,ETH --duration-sec 900
```

- 自动从 Gamma 按 slug 发现当前回合并识别回合时长；回合翻转时自动重新发现并重订阅。
- 强制 DRY：若检测到 `DRY_RUN=false` 会拒绝运行（除非 `--force`），避免误下真单。
- 注意：5m 回合 + `--min-time-left 180` 意味着可交易窗口仅回合开始后 30–120s；
  在回合中段运行会（正确地）看不到入场。观察入场请在回合开始时启动。
- 状态行含诊断字段 `books/spots/conf/sigs/blocked(timing,mom)`（盘口与现货事件计数、已确认趋势数、
  信号数、被时间/动量闸拦下的近失信号数），并对每个已确认趋势打印 `mid/entry/cap/inBand`——可直接判断
  "为何没入场"。**近失计数**是回答"时间闸是否过严"的关键数据：它记录那些 spread_arb 本已触发、
  却被 `min-time-left`/动量闸拦下的信号。

#### --replay 入场侧分析结论（已实测，附局限）

`--replay` 现输出三段：出场参数网格、walk-forward 样本外、以及按**入场价**与**入场时剩余时间**
分桶的已实现盈亏。在当前 58 条影子数据上：
- 出场侧：紧止损（SL15 系列）持续优于现行 SL50，样本外 $14.6 vs 样本内 $19.2，量级一致。
- 入场侧：`entryPrice >= 0.45` 档贡献 $11.26（中位 +$0.445，57% 胜率），`timeLeft 420–600s` 档
  贡献 $15.12（中位 +$1.355，71% 胜率）——初看提示"放宽入场上限 / 集中在回合中段入场"可能有利。
- **但交叉表显示两档高度重叠**：`>=0.45 且 420–600s` 这一格 n=9、$10.57，几乎撑起两个"好桶"；
  而同区间的 `0.43–0.45 × 420–600s` 反而 −$5.46。**单格仅 9 笔，不足以支撑改参结论。**
- **更根本的局限**：影子数据只记录**已通过入场闸门**的交易，从未记录被时间闸/入场上限**拒绝**的
  机会。因此"放宽 `min-time-left` 是否更好"用这份数据**无法回答**。

#### 从 Node 切换到 Rust 内核（已实现，可回退）

环境变量 `HFT_CORE=rust`（或 `=uds`）即把交易引擎切换到 Rust 内核：

```bash
HFT_CORE=rust node dist/index.js
# 面板/命令：/crypto-hft start [ASSETS] --dry-run | status | positions | stop
```

切换后 **Rust 内核全权接管**：自己发现回合（Gamma slug 查询，`discovery.rs`）、自己拉行情
（Binance 现货 + Polymarket 盘口 WS）、自己算信号/风控/下单/持仓/出场。Node 仅：
启动/停止子进程、下发参数（assets/size/dry-run/round-sec）、渲染状态与日志。

- **默认仍是 Node 引擎**：不设 `HFT_CORE` 时行为与以前完全一致——切换不会静默发生，可随时
  去掉该变量回退到原路径。
- 未设 `HFT_CORE` 时 `/crypto-hft status` 走原 Node 引擎；设了则走 Rust 内核，两条路径互不干扰。
- Rust 内核需要 `target/release/blitzkrieg-core`（`npm run core:build`）。
- 相关参数（`HFT_ASSETS`、`HFT_ROUND_SEC`）可选，缺省用项目默认。

> 为什么不是"桥接 ExecutionService 让旧 Node 引擎继续跑"：Node 引擎的成交处理会按 BUY 成交无条件
> 开仓，而 Rust 核心也按成交开平仓——两者叠加会**双重计数仓位**。仓位所有权不可拆分，因此正确的
> 切换是让 Rust 自驱引擎整体接管，而非桥接执行层。详见下方 P5 结论。

#### 近失记录 + 回放（已实现，填补上述缺口）

`--engine` 启动时会自动把**被闸门拦下的信号及其后续价格路径**写入 `data/shadow/near-miss.jsonl`
（可用 `--near-miss-path` 覆盖，进程退出时强制 flush，避免窗口未满丢失）。随后：

```bash
target/release/blitzkrieg-core --replay-near-miss data/shadow/near-miss.jsonl
```

它回放**两种**情形并对照：
- **A) 只放宽入场闸**（保持原出场时间闸）——被拦信号 `tLeft < min_time_left`，会立刻 time_exit，
  PnL 接近 0。这正是**入场/出场时间闸耦合**：只放宽入场没有意义。
- **B) 入场与出场时间闸一起放宽**——这才是"放宽 `min-time-left`"的真实净效果。

**首次实证（真实时段，1 条近失样本）**：被拦的 ETH 回调单在 A) 下 −$0.37、在 B) 下 −$1.55——
即该机会在下行，**放宽闸门会亏更多，时间闸拦对了**。样本 n=1 不足以定论，但工具链已闭环：
`--engine` 记录 → `--replay-near-miss` 评估，可在攒到几十条后给出统计结论。

#### 真实时段观察结论（已实测）

在真实 5m 美股时段实测多回合，Rust 原生 feed 稳定（单回合盘口事件 2500+、现货 3900+），
趋势确认逻辑正常（`confirmed` 会随行情出现/消失）。**未产生入场**的原因经诊断为策略设计而非缺陷：
`spread_arb` 要求"趋势确认（mid ≥ 0.55）"之后**回调到 `mid·0.98 ≤ 0.45`**；实测回合的强势代币
一路走强（mid 0.555→0.595）或已高度确定（mid 0.98/0.0），从未回落到 0.46 以下。用 Node 决策
管线在同样 mid 上复算，结论一致（强势不回调 → 双方都不入场；回调到 0.43 → 双方都入场 `@0.43`）。
并且在 15 分钟 / 3+ 回合的连续观察中捕获到 15 次"趋势已确认且 mid 落入入场带（0.395–0.435）"的时刻，
但**全部发生在 `tLeft` 67–169s**——即入场截止线（`min-time-left 180`）之后，引擎（正确地）拒绝临期入场。
5m 回合的实际可入场窗口只有 `tLeft 180–270s`（回合开始后 30–120s，仅 90 秒），比其他时长市场窄得多。
结论：**Rust 引擎在真实行情下行为正确且与 Node 等价；观察期内无成交是"回调恰好都落在入场窗口之外"所致，
而非引擎缺陷。** 加装"近失计数"后再测，单次约 3 分钟观察即记录到 **315 次**本已触发却被时间闸拦下的
有效回调信号——这从数量上证实了 `min-time-left 180`（5m 回合仅 90s 可入场窗口）会拦掉大量机会。
是否放宽属于**策略参数决策**，需在更多数据上评估（且要区分"被拦信号若成交是否会亏"），不建议直接改。

- 凭证 **只** 由 core 从 `POLYMARKET_PRIVATE_KEY` / `POLYMARKET_FUNDER_ADDRESS` 读取；
  用户 WS 的 L2 凭据由私钥在 core 内派生（`create_or_derive_api_key`），Node 完全不需要 API Key。
- 缺凭证时 live 二进制仍可启动，桥进入 inert，订单停留在 Pending（安全）。
- 桥每 500ms：提交未绑定订单 → 泵送用户 WS 成交/订单事件 → 每 ~5s 拉 REST 快照对账
  （补 WS 缺口成交、修复幽灵单/半成交）。
- `orders.reconcile` 可由 Node 手动触发对账（测试/运维）。

## P0/P1 边界与未完成（下一阶段）

- **P0（完成）**：交易底层骨架——OME/幂等账本/资金/最小风控/dry 撮合/UDS 协议/Node 客户端，
  DRY 端到端验证（18 Rust 单测 + 16 项 parity），默认不影响现有运行路径。
- **P1（完成，未上真钱）**：live 通道编译级就绪——CLOB 下单/撤单/余额、用户 WS 权威成交、
  纯对账引擎（补成交/幽灵单）、live 桥（提交/事件泵/周期 REST 对账/余额播种）。
  真实 live 冒烟**尚未进行**，需最小仓位、你签字后才会启用；不达标则保持 default dry。
- **P2（完成，未上真钱）**：完整风控与持仓/出场在 Rust——纯出场策略（与 TS 逐条对齐、可回放）、
  持仓账本与 PnL、容量/冷却闸、日亏上限、连亏熔断、`positions.list`/`positions.exit` 与
  `POSITION_CLOSED` 事件；tick 驱动出场并自动下平仓单（`--no-auto-exits` 可关）。
- **P3（完成）**：行情/信号进 Rust——本地 L2 重建、价格缓冲、趋势跟踪、`spread_arb` 评估器、
  回合扫描/时钟/时间闸，以及**自驱引擎**（事件→趋势→信号→下单→趋势破裂撤单）。Node 只需
  喂 `books.*` / `spot.price` / `engine.markets`；`--engine` 打开后 Rust 在内部 tick 自行评估下单。
- **P4（完成）**：Rust 自带行情接入——Polymarket 盘口 WS（SDK `ws`，动态重订阅新回合代币）+
  Binance 现货 WS（tokio-tungstenite，自带重连）；`--feed-ws` 打开后 Node 不再需要推
  `books.*`/`spot.price`。影子采样与回放（`shadow.rs`）复用同一份 `exit_policy`，回测不会与实盘漂移。
- **P5（部分完成）**：
  - ✅ 影子落盘（Node 分析脚本兼容的 JSONL）+ `--replay <file>`：Rust 直接对现有影子数据跑
    walk-forward（网格内样本 + 扩展窗口样本外）。已在真实 `data/shadow/positions.jsonl` 上验证。
  - ⚠️ **未做且刻意不做**：把 `ExecutionService` 桥接到 Rust、让**旧 Node 引擎**继续跑，此路不通——
    Node 引擎的 `applyFillDelta` 在 BUY 成交时会无条件 `positionMgr.open`，而 Rust 核心（P2）也按成交
    开平仓；两者叠加会**双重计数仓位**。仓位所有权无法拆分：要么 Node 全管，要么 Rust 全管。
  - **正确的切换路径**：不是"桥接执行层"，而是**让 Rust 自驱引擎接管整个交易循环**（`--engine`
    `--feed-ws`，已具备），Node 只做 UI/参数/日志与回合代币下发。
  - ✅ **切换前对照已执行**：`npm run core:parity-engines` 用**同一份行情输入**驱动 Node 决策管线
    （`createTrendTracker` + `evaluateSpreadArb`，引擎实际调用的模块）与 **Rust 自驱引擎**（真实进程、
    经 UDS、DRY），逐笔比对 token/方向/挂单价。结果：**一致**（`UP_TOKEN / up / 0.43`，双方均只挂单未成交），
    多次运行稳定。差异仅出现过一次，原因是趋势窗口裁剪的边界（场景时间跨度不足），非逻辑差异。
  - 判定：Rust 引擎在决策层面与现有 Node 引擎等价，具备接管条件。剩余步骤是在**真实市场时段**做一次
    端到端 dry 观察（真实代币、真实盘口、真实回合切换），确认无回归后再切默认——需你在场。

### 尚未接线（需你决定）
- `HFT_CORE=uds` 让 HFT 引擎默认走 core：`RustCoreClient` 已可用，但引擎切换到 core 下单
  需先完成 live 冒烟（或有明确的 dry 演练目标），因此当前 **默认仍走原 Node execution 路径**。
- 用户 WS 断线重连：当前 actor 内流结束即记录 Fatal，未做指数退避重连（P2 补齐）。

## 安全不变量

1. Node 不出现私钥字段（IPC schema 无 key/secret）。
2. 任何下单必经 `risk.check` 与 `ledger.reserve`，无旁路。
3. 成交只通过幂等账本（tradeKey 去重、按单 size 封顶、FAILED 可回滚）改变持仓效应。
4. 强制退出/风控事件不被卖出冷却或挂单阻塞（核心侧 kill switch 立即生效）。
