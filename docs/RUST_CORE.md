# Rust 交易核心（rust-core）— 架构与 P0 状态

> **P0.5 更新（2026-09-13）**：目录与 crate 已改名：`rust-core/` → `Blitzkrieg_core/`，
> crate `clodds-rust-core` → `blitzkrieg-core`（lib `blitzkrieg_core`）；UI 移入 `ui/`（旧
> `/webchat/hft.html` 仍兼容）。本文档中出现的旧路径/旧 crate 名可按此对照。新的分层与
> 扩展体系见 `docs/blitzkrieg/{ARCHITECTURE,INTERFACES,STRATEGY_GUIDE,EXTENSION_GUIDE,MIGRATION_LOG}.md`。


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
clodds-rust-core  ── Unix Domain Socket ($TMPDIR/clodds-core-$USER.sock)
                   ── 一行一个 JSON 对象（'\n' 分帧），JSON-RPC 2.0
```

- Node→Rust：`{jsonrpc:"2.0",id,method,params}`
- Rust→Node 响应：`{jsonrpc:"2.0",id,result|error}`
- Rust→Node 事件：`{jsonrpc:"2.0",method:"core.event",params:{kind,...}}`
- 契约唯一来源：Rust 的 serde 结构体（`model.rs` / `ipc/schema.rs`）；Node 用 zod 镜像校验
  （`src/core/schema.ts`），非法消息拒绝，不使用 any。
- 凭证：Rust 进程自己读 `POLYMARKET_PRIVATE_KEY` / `POLYMARKET_FUNDER_ADDRESS`；
  Node 不显式传私钥。

## Rust crate 模块（P0）

| 模块 | 职责 |
|---|---|
| `decimal.rs` | 金额/价格/数量的 Decimal 边界编解码（f64/string → Decimal，核心内不用 f64 算钱） |
| `model.rs` | Order/Fill/状态/`FillPolicy`/结构化错误码/事件 |
| `ome.rs` | 权威订单状态机；按 tradeKey 的幂等成交账本、累计 delta、按单 size 封顶、加权均价、FAILED 回滚、未知成交缓冲 |
| `ledger.rs` | USDC 预扣/释放/成交结算，防止超额下单 |
| `risk.rs` | kill switch、单笔名义上限（完整日亏/连亏/冷却在 P1/P2） |
| `sim.rs` | Dry 撮合：taker 即时成交、maker 盘口穿越成交（与 live 共用同一 OME） |
| `marketdata.rs` | 本地 L2 重建：快照/增量/top-of-book，交叉自愈，陈旧判定 |
| `signal.rs` | 纯信号：PriceBuffer、TrendTracker（滚动窗口确认/破裂）、`spread_arb` 评估器（mid/bestBid 定价纪律） |
| `scanner.rs` | 回合/时钟偏移/slug/时间闸；Gamma 字段解析（outcomes/clobTokenIds/outcomePrices） |
| `engine.rs` | 自驱引擎：事件（book/top/spot/round）→ 趋势 → spread_arb → 下单；现货动量过滤；趋势破裂撤单 |
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

**dry / shadow / 回测 / live 同核心**：Dry 模式不模拟成交后另起状态机，而是通过
`orders.place` + 盘口穿越走和 live 完全相同的 OME/账本/风控路径。Live 适配器通过
`place_pending / confirm_live / reject_live / ingest_fill` 把交易所确认和用户 WS
成交喂入同一套代码。

## IPC 方法（P0）

`core.ping` · `core.ready` · `risk.kill` · `risk.resume` ·
`orders.place` · `orders.cancel` · `orders.cancel_all` · `orders.list` ·
`orders.reconcile` · `ledger.balance` · `positions.list` · `positions.exit` ·
`books.snapshot` · `books.top` · `spot.price` · `engine.markets` · `engine.round`
（P3：Node 通过 `books.*`/`spot.price`/`engine.markets` 喂数据，Rust 引擎自驱决策并于内部 tick 下单；
P4 起 Rust 自带 WS 接入后这些桥将退回为可选）。

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
npm run core:build     # cargo build --release（产物 rust-core/target/release/clodds-rust-core）
npm run core:test      # 57 个 Rust 单测：OME/账本/风控/熔断/撮合/对账/出场/持仓/信号/扫描/L2/shadow
npm run core:parity    # 真实 Node RustCoreClient 经 UDS 跑 22 项端到端 parity 断言
npm run core:parity-engines  # 引擎对照：Node 决策管线 vs Rust 自驱引擎，同一输数据逐笔比对
npm run core:observe   # 真实回合 DRY 观察：发现当前回合代币喂给 Rust 引擎并实时监控

# 离线 walk-forward：对影子 JSONL 跑同一份出场策略的网格 + 样本外评估，
# 并按入场价 / 入场时剩余时间分桶（辅助判断入场闸门是否值得调整）
rust-core/target/release/clodds-rust-core --replay data/shadow/positions.jsonl

# 近失回放：评估"放宽入场闸"的净效果（需先有 near-miss 记录，见下）
rust-core/target/release/clodds-rust-core --replay-near-miss data/shadow/near-miss.jsonl
```

Node 客户端：`src/core/rust-core-client.ts`（spawn/就绪握手/崩溃重启/超时/zod 校验/事件订阅/reconcile）。

## Live 运行

```bash
rust-core/target/release/clodds-rust-core --socket <path> --mode live \
  --max-order-notional 2.5 --market 0x<conditionId> [--market 0x...]
```

### 自驱 + Rust 原生行情（P3/P4）

```bash
# Node 只下发参数/回合代币，Rust 自带 WS 取行情并自行决策下单：
clodds-rust-core --socket <path> --mode dry --engine --feed-ws \
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
- Rust 内核需要 `rust-core/target/release/clodds-rust-core`（`npm run core:build`）。
- 相关参数（`HFT_ASSETS`、`HFT_ROUND_SEC`）可选，缺省用项目默认。

> 为什么不是"桥接 ExecutionService 让旧 Node 引擎继续跑"：Node 引擎的成交处理会按 BUY 成交无条件
> 开仓，而 Rust 核心也按成交开平仓——两者叠加会**双重计数仓位**。仓位所有权不可拆分，因此正确的
> 切换是让 Rust 自驱引擎整体接管，而非桥接执行层。详见下方 P5 结论。

#### 近失记录 + 回放（已实现，填补上述缺口）

`--engine` 启动时会自动把**被闸门拦下的信号及其后续价格路径**写入 `data/shadow/near-miss.jsonl`
（可用 `--near-miss-path` 覆盖，进程退出时强制 flush，避免窗口未满丢失）。随后：

```bash
clodds-rust-core --replay-near-miss data/shadow/near-miss.jsonl
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
