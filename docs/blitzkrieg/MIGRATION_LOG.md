# 迁移日志（MIGRATION_LOG）

> 范围：P0.5 — 结构重构 / 通用化抽象 / 插件化扩展。
> 硬性约束：不改业务逻辑、不改 UDS 协议（仅新增 `version` 字段）、不改 DryRun 链路行为。

## 0. 基线（改动前）

| 项 | 结果 |
|:---|:---|
| `cargo build --release` | 通过 |
| `cargo test --lib` | 61 passed |
| `npm run typecheck` | 0 errors |
| `npm run core:parity`（22 项） | 退出码 0 |

## 1. 目标一：`rust-core` → `Blitzkrieg_core`

改动：
- 目录 `rust-core/` → `Blitzkrieg_core/`
- `Cargo.toml`：`package.name` = `blitzkrieg-core`；`[lib] name` = `blitzkrieg_core`；description 更新
- Rust 源码：`clodds_rust_core` → `blitzkrieg_core`；`clodds-rust-core` → `blitzkrieg-core`；日志前缀 `rust-core:` → `blitzkrieg-core:`
- Node：`src/core/rust-core-client.ts` → `blitzkrieg-core-client.ts`；`rust-core-runner.ts` → `blitzkrieg-core-runner.ts`；类名 `RustCoreClient/Runner` → `BlitzkriegCoreClient/Runner`；二进制路径指向 `Blitzkrieg_core/target/release/blitzkrieg-core`
- scripts：`rust-core-parity.mjs` → `core-parity.mjs`；`dry-observe.mjs`/`parity-engines.mjs` 路径与类名同步；删除过时的 `rust-core-smoke.cjs`
- `package.json`：`core:build/test` 指向 `Blitzkrieg_core/Cargo.toml`；`core:parity` 指向新脚本名
- UDS socket 路径**未变**（`clodds-core-<user>.sock`）

验证：
- `cargo build --release` 通过；`cargo test --lib` **61 passed**
- `npm run typecheck` **0 errors**；`npm run build` 通过
- `node scripts/core-parity.mjs`（22 项，经新二进制名与客户端）**退出码 0**
- `grep -rn "rust-core" src scripts package.json Blitzkrieg_core/src` **为空**

## 2. 目标二：UI 迁移

改动：
- `public/webchat/hft.html` → `ui/hft.html`；根目录 `hft-dashboard.html`、`hft-server.js` → `ui/`
- gateway `server.ts`：新增 `app.use('/ui', express.static('../../ui'))`；新增 `/webchat/hft.html` 兼容路由（`sendFile ui/hft.html`）
- `docs/CRYPTO_HFT.md`：面板路径更新为 `ui/hft.html`

验证：
- `GET /ui/hft.html` → **200**；`GET /webchat/hft.html`（兼容）→ **200**；`GET /ui/hft-dashboard.html` → **200**
- 浏览器打开面板：轮次/倒计时/四资产价格/统计卡片正常渲染（截图确认）
- `grep -r "POLYMARKET_PRIVATE_KEY" ui/` 为空

## 3. 目标三：通用量化底座抽象

新增（纯新增，不改既有实现）：
- `order/mod.rs`：`OrderIntent` + `MarketType`/`OrderKind`/`TimeInForce` + `validate()`
- `market/mod.rs`：`MarketAdapter` trait + `AdapterFuture`/`AdapterOrderResult`/`AdapterBalance`/`AdapterPosition`/`AdapterCapabilities`
- `market/polymarket.rs`：`PolymarketAdapter`（包装既有 `LiveVenue`）
- `market/binance_spot.rs`：`BinanceSpotAdapter` 结构空壳（`submit_order` 明确报错）
- `risk_context.rs`：`RiskContext` trait + `PredictionRiskContext` + `FuturesRiskContext`
- `ledger_api.rs`：`LedgerApi` trait；既有 `Ledger` 增加该 trait 的实现（委托既有方法）
- `strategy_engine/mod.rs`：`Strategy`/`Signal`/`MarketTick`/`StrategyEngine`/`validate_signal`
- `strategy_engine/builtins.rs`：`SpreadArbStrategy`（包装既有评估器）〔**已于 §34 删除**：除自身定义外无任何引用的第三份实现〕
- `strategy_engine/loader.rs`：`libloading` 动态加载 + `policy_allows` 安全策略
- 用户层示例：`user_layer/strategies/dog_strategy.rs`、`user_layer/strategies/trend_strategy.toml`、`user_layer/configs/default.toml`

验证：`cargo test --lib` **74 passed**（新增 13 个：OrderIntent 校验、Adapter 空壳 ×2、RiskContext ×2、Strategy engine ×3、loader ×3、其余）；`cargo build --features strategy-loading` 通过且 `libloading v0.8.9` 被编入。

## 4. 目标四：扩展体系

新增：
- `extension/mod.rs`：`Extension` trait、`ExtensionRegistry`（生命周期 + 隔离分发）、`ExtensionContext`、`ExtensionType`/`ExtensionState`
- `extensions/binance_spot/config.toml`、`extensions/README.md`

验证：`cargo test --lib` 含 2 个扩展测试（完整生命周期 install→enable→dispatch→disable→uninstall；`on_load` 失败记 `Failed` 而不 panic）。

## 5. IPC 扩展

改动：
- `Request` 新增 `version` 字段（默认 `PROTOCOL_VERSION = "1.1"`）
- 新增方法：`strategy.list` / `strategy.enable` / `strategy.load` / `extension.list`；`engine.stats` 增 `blocked`
- `Core` 新增 `strategy_engine`、`extensions` 字段与查询/开关方法

验证（真实内核进程 + IPC）：
```
strategy.list  → {"strategies":[{"name":"spread_arb","enabled":true}],"version":"1.1"}
extension.list → {"extensions":[],"version":"1.1"}
strategy.enable→ {"enabled":true,"found":true,"name":"spread_arb"}
```

## 6. 全量回归

| 项 | 结果 |
|:---|:---|
| `cargo build --release` | 通过 |
| `cargo build --release --features strategy-loading` | 通过 |
| `cargo test --lib` | 74 passed |
| `npm run typecheck` | 0 errors |
| `npm run build` | 通过 |
| `node scripts/core-parity.mjs` | 22/22，退出码 0 |
| `node scripts/parity-engines.mjs` | PARITY OK |
| DryRun 面板（`ui/hft.html`） | 渲染正常，Rust 内核自驱运行 |

## 7. 追加完成（P0.5 收尾）

### 7.1 `extension.enable` / `extension.disable` IPC 接线（完成）
- `Core::enable_extension` / `disable_extension`（含 `CoreExtensionContext`——扩展仅得事件广播 + 只读策略名 + 日志，无凭据/venue/socket/内部状态）。
- 内建示例扩展 `extension::builtins::BinanceSpotExtension`（默认 installed）。
- IPC 实测：`installed → enable → enabled → disable → disabled`；未知扩展返回 `extension not installed: <name>`。

### 7.2 策略动态库 C ABI 冻结（完成）
- 新增 crate `user_layer/strategy_api`（`blitzkrieg-strategy-api`，`#[repr(C)]`，`crate-type=["rlib","cdylib"]`）：`BK_ABI_VERSION=1`、`BkTick`/`BkSignal`/`BkSide`/`BkStrategyVtable`、符号 `bk_strategy_create` / `bk_strategy_abi_version`。
- `strategy_engine/loader.rs`：真实 `dlopen` → ABI 版本协商（不匹配即拒绝）→ 读取 vtable → 包装为内部 `DynamicStrategy: Strategy`（tick 编组为 C 结构、signal 立即复制、`Drop` 调 `destroy`）。默认构建**不编译** `libloading` 路径（`--features strategy-loading` 启用）。
- 示例用户层策略 `user_layer/strategies/dog_strategy.rs`（独立 crate）编译出 `libdog_strategy.dylib`。
- 端到端测试 `Blitzkrieg_core/tests/dynamic_strategy.rs`（2 passed）：加载 → `name()=="dog_strategy"` → 高位返回 Hold、跌到 0.42 返回 `Buy@0.42 x10` → `on_round` 不 panic；凭据样式路径被 `Rejected`。
- IPC 实测：`strategy.load` 成功返回 `Loaded { path, name: dog_strategy, version: 0.1.0 }` 并经 `strategy.list` 确认注册；`secret.dylib` 被拒。

## 8. 影子进化（Shadow Evolution，v1.0）

新增 `Blitzkrieg_core/src/shadow_evolution/`：`config`（Mutable/Immutable 分离）、`variants`（虚拟账本 +
变异策略，复用 `exit_policy`）、`evaluator`（六条件触发）、`guard`（渐变 ≤5% + 风控不可变）、`hot_swap`
（`ArcSwap`）、`audit`（JSONL + 环形历史）、`signal`。
- 依赖新增 `arc-swap = "1"`。
- Engine：`set_hot_params` + `effective_spread_arb()` 每轮叠加热参数；`engine.on_data` 把 tick 喂给变体；
  `engine_evaluate` 先评估进化再下单（下一 tick 生效）。
- Core/IPC：`shadow_evolution.enable/disable/status/history/rollback`、`--shadow-evolution` 与
  `--se-min-samples/--se-cooldown-secs/--se-min-obs-secs`；事件 `EVOLUTION_SIGNAL/APPLIED/REJECTED`。
- 默认关闭（opt-in）；配置样例 `user_layer/configs/shadow_evolution.toml`；命令
  `/crypto-hft shadow-evolution …`。
- 测试：**95 passed**（含渐变拒绝、风控不可变、热切换到达引擎、回滚、崩溃隔离、审计完整性）。
- 文档：`docs/blitzkrieg/SHADOW_EVOLUTION.md`。

## 9. 面板打通 + 严重 bug 修复（v1.1 追加）

### 9.1 Rust 落盘成交（面板历史订单数据源）
- 新增 `Blitzkrieg_core/src/trade_db.rs`：`close()` 时把已平仓交易写入 `data/trades/trades.jsonl`
  （27 字段，与 Node `TradeRecord` 逐字段兼容；`exitReason` 用 serde snake_case 如 `take_profit`）
  并更新 `summary.json`。
- 新增 IPC `trades.history { limit }` → `{ version, trades: [...] }`；Node 客户端 `tradesHistory()`。

### 9.2 Rust 模式 `positions` 对齐面板
- `ui/hft.html` 的「历史订单」表只在回复含 `Last` + `Trades:` 时解析，且要求特定行格式。
- 技能层 Rust 模式 `positions` 现在输出 `**Last N Trades:**` + 面板期望的每行格式
  （`BTC UP +49.3% (+$1.97) [spread_arb] 23:23:20->23:24:24 $0.40->$0.60 (+0.200) 64s`）。
  该行已用面板的**原正则**验证匹配。

### 9.3 严重 bug：持仓被立即强平（"永远不交易"的真因）
- 症状：E2E 中开仓后立刻 `force_exit hold=0s`；soak 期间零成交。
- 根因：`service.rs::project_fill_delta` 用 `expires = round_slot * round_duration`——这是回合的
  **开始**时刻（已过去），持仓一建立即判定到期。扫描器用的是 `(slot+1)*duration`（回合**结束**）。
- 修复：改为 `(slot + 1) * round_duration_sec * 1000`，与 `scanner::slot_expiry_ms` 一致。
- 回归测试：`round_expiry_tests::filled_entry_expires_at_round_end_not_start`；并修正 2 个依赖旧
  错误语义的既有测试。
- 验证：修复后 E2E 持仓正常（`tLeft=235s`），并在 0.95 获利平仓（`take_profit`, `net $5.20`）。

> 结论修正：此前"零成交"的主因是此 expiry bug（结构上无法持仓），而非仅时间窗偏窄。修复后
> 引擎具备正常持仓/出场能力。

## 10. 剩余边界（非阻塞）

- 动态策略加载默认未编译（需 `--features strategy-loading`），以保持默认产物体积与依赖精简。
- 内建策略目前仅 `spread_arb`（与既有行为一致）；更多模板随扩展体系接入。
- 策略动态库的 signal → 下单流转仍由内核 `validate_signal` + 风控/账本闸门控制（用户层无法绕过）。
- **P0.5 全程未改动任何交易/风控/订单业务逻辑**；DryRun 与 live 链路逐项回归通过。

## 11. P0.6（Polymarket 扩展化）— 已建脚手架，主体暂缓（方案 A）

### 已完成（安全脚手架，不影响运行中的二进制）
- 根 `Cargo.toml`：建立 workspace，members = `Blitzkrieg_core`、`extensions/polymarket`、
  `user_layer/strategy_api`；exclude `user_layer/strategies`（独立嵌套 workspace）。
- `extensions/polymarket/`：独立 crate（`polymarket-extension`，`crate-type=["rlib","cdylib"]`），
  依赖 `rs-clob-client-v2`，通过 path 依赖 `blitzkrieg-core`（default-features=false）。
- 验证：`cargo build -p blitzkrieg-core` 干净通过；正在跑的 soak 进程使用既有二进制，不受影响。

### 关键技术结论（决定后续做法）
- 文档 §3.3 的 `*mut dyn Extension` 跨 dylib 传 Rust trait object **不可行**（fat pointer / vtable
  无稳定 ABI）。正确做法是 `#[repr(C)]` C 函数指针表（沿用 P0.5 策略动态库已验证的 ABI 模式）。
- Polymarket 不是单一适配器：SDK 集中在 3 文件（`venue.rs` 420 行签名/CLOB、`feed.rs` Poly WS、
  `discovery.rs` Gamma），但深耦合三条热路径（行情→engine、发现→标的、Poly1271 actor→OME 对账）。
  要"内核 grep 不到 polymarket 且运行时加载"，须将行情/发现/下单全改为 trait 对象注册。

### 决策（已与指挥官确认，方案 A）
- **暂缓 P0.6 主体**：先用 5m soak 验证"持仓秒平"修复在真实行情下成立。
- 顺序：5m soak 通过 → 独立回合做 trait 边界（DataFeed / MarketDiscovery / OrderExecutor）
  + 独立 crate + feature 接线（内核零市场代码）→ 再做 C-ABI dylib 热加载（P0.7）。
- 风险原因：刚修复交易链路，5m soak 仅 20 分钟，不宜同时动 venue/feed 热路径并重建。

### P0.6 待办（下一回合）
1. 内核定义 `DataFeed` / `MarketDiscovery` trait（与 `MarketAdapter` 并列），`ExtensionContext` 提供注册。
2. 把 venue/feed/discovery 物理迁入 `extensions/polymarket/src/{adapter,poly1271,clob_client,gamma_scanner,error_map}`。
3. 内核 Cargo.toml 移除 `rs-clob-client-v2` 直依赖，改 `polymarket` feature 引入扩展 crate。
4. main/server 经 trait + 注册表装配；manifest.json/config.toml；扩展 enable/disable/unload 跑通。
5. P0 全量回归（DryRun/Live/对账/影子进化/策略加载）；`grep -r polymarket Blitzkrieg_core/src` 仅注释。
6. P0.7：在干净 trait 边界上加 C-ABI dylib（版本协商 + catch_unwind 隔离）。

## 12. 更正：回合时长应保持 900s（15m），5m 会显著减少机会

### 结论（用数据推翻我此前的建议）
此前我建议把回合改成 5m（`HFT_ROUND_SEC=300`），理由是"持仓中位 130s，短回合更匹配"。
**这个推断是错的**。实证数据：

- 历史（Node）62 条影子记录的 `context.timeLeftSec`：min 266、中位 576、**max 779**，
  其中 **60/62 > 300s** → 说明 Node 当时跑的是 **15m 回合**（`roundDurationSec=900`，
  与 `CRYPTO_HFT.md` 一致）。
- Node 成交频率：61 笔 / 20.3 小时 = **约 3 笔/小时**。

### 为什么 5m 更差（关键：min-time-left 是绝对秒数）
`minTimeLeftSec=180` 与 `minRoundAgeSec=30` 是绝对值，因此可交易窗口为 round 的 `[30s, dur-180s]`：

| 回合 | 可交易窗口(age) | 60s 趋势确认后可等回调的时间 |
|:--|:--|:--|
| 15m (900s) | 30s – 720s | **约 660 秒** |
| 5m (300s) | 30s – 120s | **约 30 秒** |

把回合从 900s 改成 300s，等于把"趋势确认后等待回调"的窗口缩小约 **22 倍**。这就是切到 5m 后
`signals=0`、交易停止的直接原因。

近失数据佐证：8 条被拦信号中 6 条因 timing 被拒（tLeft 35–160s，均在 5m 窗口外），
其中 `SOL down 0.41→0.98`、`BTC down 0.28→0.87` 本可盈利。

### 处置
- **回退**：`.env` 设 `HFT_ROUND_SEC=900`，恢复与 Node 历史一致的 15m 回合。
- 验证：重启后 `--round-sec 900`，`canTrade=true`、`blocked.timing=0`（窗口不再被时间闸挤压）。
- 教训：**改回合时长前必须核对策略的时间窗需求，而不是只看持仓时长**。


## 13. 严重 bug #2：单笔名义上限拒绝 100% 订单（"毫无动静"的真因）

### 症状
切到 15m 后仍无成交。实时 `engine.stats` 显示 **`signals=799、placeRejected=799`**——
引擎产生了 799 个候选订单，**每一个都被 `place()` 拒绝**；`orders.list` 恒为 0。

### 根因
切换内核时，Node 侧 runner 把核心的"单笔名义上限"传成了 `sizeUsd`（$2.5）：
```
--max-order-notional 2.5
```
但真实订单是 `maxShares(10) × price(≈0.43) = $4.3`（`minShares=maxShares=10` 使 sizeUsd 实际不生效）。
于是 `RiskGate` 的单笔名义上限 `$4.3 > $2.5` → **全部 RiskRejected**。
每个评估 tick 重试同一 token（`pending_tokens` 仅在 place 成功后才记录），遂累积到 799 次拒绝。

Node 时代不存在此问题：`sizeUsd` 只参与股数计算且被 clamp，名义上限不是用它。

### 修复（Node 侧，无需重建内核）
`src/core/blitzkrieg-core-runner.ts`：名义上限改为按**真实最坏订单**定价，而非策略名义 size：
```
maxOrderNotional = max(sizeUsd, maxShares × 0.6)   // 10 × 0.6 = 6 > 10 × 0.45 上限
```
并传入 `--max-order-notional 6`；runner 配置新增 `maxShares`，技能层从 `DEFAULT_CONFIG.maxShares` 注入。

### 验证
- 生产 cmdline：`--max-order-notional 6`（原 2.5）。
- 一次性内核（同参数）实测：`signals=1 placeRejected=0`，订单 `BTC LIVE@0.43` **被接受**。

## 14. 剩余的"无成交"归因（非 bug）

修复后仍可能长时间无成交，原因是**动量过滤器**（与 Node 同逻辑、同参数）：
- `engine.stats.blocked.momentum` 持续增长（单回合达 108 次），`blocked.timing=0`。
- 动量过滤：UP 候选要求现货 30s 内跌幅 ≤0.03%，DOWN 候选要求涨幅 ≤0.03%——容差极紧，
  强单边行情下会拦掉绝大多数候选。这是**策略设计**（避免在现货逆行时抄底），非引擎缺陷。
- 结论：Rust 引擎在"能下单"层面已与 Node 等价（名义上限修复后）；成交频率取决于行情是否符合
  "趋势确认 + 深回调 + 现货不逆行"三条件。历史 Node 61 笔即在该三条件下产生（约 3 笔/小时）。

## 15. 基准澄清：Node 时代本身大多回合不成交（避免误判）

用 61 笔历史成交反推 Node 的真实频率：
- **81 个 15m 回合中只有 36 个回合有成交**（56% 的回合零成交）。
- 平均 **0.75 笔/回合**、3.0 笔/小时。
- 相邻成交间隔：中位 12 分钟，**最大 206 分钟（3.4 小时）**；>60 分钟的间隔有 4 次。

结论：**"连续几个回合没有成交"是 Node 时代的正常状态**，不是引擎故障。要判断 Rust 是否与 Node 等价，
必须观察 **数小时（至少覆盖多次成交间隔）**，而不是几分钟或一两个回合。
（注：修复 bug#2 前，Rust 是 100% 拒单、永不可能成交；修复后才具备与 Node 等价的成交能力。）

## 16. 二进制路径错位 + bug#3 修复验证 + 单实例守护（2026-09-13 23:0x）

### 16.1 根因：生产跑的是旧二进制（本轮"所有修复看起来无效"的真正原因）
Cargo workspace 化后输出落在**根 `target/release/blitzkrieg-core`**，但运行路径仍指向
**`Blitzkrieg_core/target/release/blitzkrieg-core`**（21:22 的旧构建，不含任何修复）。
23:52 前所有"验证"其实都在测旧二进制。

- 修复 `src/core/blitzkrieg-core-client.ts` `defaultBinaryPath()`：候选顺序改为根 `target/release` 优先。
- 同步修 `scripts/core-parity.mjs` / `dry-observe.mjs` / `parity-engines.mjs`。
- 证据：重启后 `ps` 显示核心来自 `.../CloddsBot/target/release/blitzkrieg-core`（22:52 构建）。

### 16.2 bug#3（持仓价格/盈亏冻结）已在**部署二进制**上闭环验证
新增 `scripts/cycle-check.mjs`：私有 socket、短确认窗口，确定性驱动
`回合 → 趋势确认 → 回调 → 挂单 → maker 成交 → 开仓 → 盘口上行 → 实时重估`。
实跑结果（正确百分比单位）：
```
[2] confirmed=1
[3] dip -> live orders: 1    bid 0.4300 x 10 (maker_then_taker)
[4] crossed -> open: 1       entry=0.4300 cur=0.4100 pnl=-4.65%
[5] bid 0.50 -> cur=0.50 pnl=+16.28%
    bid 0.60 -> cur=0.60 pnl=+39.53%
    bid 0.70 -> cur=0.70 pnl=+62.79%
    bid 0.85 -> cur=0.85 pnl=+97.67%
RESULT: PASS
```
即：开仓后 cur/pnl 随盘口实时变动——正是用户截图缺失的行为。

### 16.3 单实例守护（并发 start 竞态）
症状：23:57:44 日志出现两条 `Rust core engine started` 相隔 1ms，其一因
`Address already in use (os error 48)` 退出。根因：`Runner.start()` 在 `client` 检查与赋值之间有
`await`，两个并发 `/crypto-hft start` 都通过检查 → 各起一个核心；内核启动时会
`remove_file(socket)`，后起者可能**静默接管/顶掉**健康内核的 socket。

双重加固：
- **Rust**（`ipc/server.rs`）：绑定前先 `UnixStream::connect` 探测；若已有活内核在听则
  `bail!("another blitzkrieg-core is already listening")`，绝不 unlink 活 socket；陈旧的
  （连接失败）才删除。实测：第二个核心 exit=1，第一个存活且保留 socket。
- **Node**（`blitzkrieg-core-runner.ts`）：`start()` 用 `starting` Promise 串行化，并发调用复用同一
  in-flight 启动；`stop()` 先 await 在途启动再停止，避免"停完又冒出一个无人管理的核心"。

### 16.4 soak
`scripts/soak-monitor.mjs --hours 12 --interval-sec 300` 后台运行，写
`data/soak/soak.jsonl`。判据：core=up、ping=ok、feed 计数增长、err 不增；成交频率对比 Node 基准
（0.75 笔/回合、3 笔/小时、56% 回合零成交）。

## 17. bug#4: 面板顶部卡片未接通（盈亏/胜率/笔数/今日/交易量）

### 症状
机器人自 23:03 起真实交易（核心账本 64 笔 / 36W28L / 净 +$22.37），但面板顶部四张卡显示
`盈亏 $+0.00`、`交易笔数 1`、`今日 +$0.00`、`交易量 $0.00`；而下方"历史订单"却能显示单笔
`-$2.31`。用户据此判断"顶部板块没实现数据联通"——判断正确。

### 根因
Node 引擎的 `status` 有一条明确规则（原注释）：
```
// Cumulative stats come from persisted trades (survive restarts); the
// in-memory engine stats reset to zero on every start.
const persisted = computePersistedStats();
```
而 Rust 分支的 `status` **丢掉了这层持久化聚合**：
- `Trades/Gross/Net/Today` 全部取 `runner` 进程内计数器——每次 Node/核心重启归零，
  且只统计本进程内发生的平仓；顶部 `交易笔数` 因此=1（本轮首笔）而非累计 64。
- `Fees / Best / Worst / Volume / Avg` 直接**硬编码为 0**（`$0.00 | Avg: $0.0000`）。
- `positions`（历史订单）走的是核心 `trades.history`，所以那张表是对的——正是这个"表对卡不对"
  的割裂暴露了问题。

### 修复
1. 把 Node 的聚合逻辑抽成共享函数 `aggregateTrades(trades)`，`computePersistedStats()` 复用它。
2. Rust 分支 `status` 改为从**核心权威账本**聚合：`client.tradesHistory(0)`（0=全部）→
   `aggregateTrades` → 覆盖 `totalTrades/wins/losses/winRate/gross/fees/net/daily/best/worst/volume/avg`；
   取不到时回退到内存计数器。字段单位与 Node 完全一致（`netPnlPct` 是百分数，同 §16.2 的 `pnl_pct`）。
3. `positions` 默认 `limit` 由 `50` 改为 `0`（=全部），与 Node 引擎一致，避免历史订单被截断。

### 验证
- `tsc` 0 error，build ok。
- 重启后 `/crypto-hft status` 实输出：
  `Trades: 64 (36W/28L) 56% WR` / `Gross: +$30.20 | Fees: $7.83 | Net: +$22.37` /
  `Today: +$24.60 | Best: +145.7% | Worst: -99.5%` / `Volume: $270.10 | Avg: $0.4287`。
- 用 `ui/hft.html` 的四个正则对上述文本离线解析：四张卡全部得到真实数值（盈亏 22.37 / 胜率 56% /
  笔数 64 / 今日 24.60，量 270.10，均价 0.4287）。

### 附带加固：cycle-check 隔离
`cycle-check.mjs` 会把核心的**相对**路径 `data/trades/trades.jsonl` 写入当前工作目录，
若不隔离，测试的合成成交会污染生产账本。已改为在 `mkdtemp` 出的临时目录里 spawn
（`cwd: WORKDIR`）。实测：跑完 cycle-check 前后生产 `trades.jsonl` 行数不变（64 → 64）。

### 说明：截图那笔 BTC 做空
23:24:05 入场 / 23:24:19 出场、$0.43 → $0.21、net -$2.31、reason=stop_loss——这是**机器人在
DRY 模拟盘上的真实记录**（核心账本可查），不是人工/测试写入。

## 18. bug#5: 盘口价格会"卡住"——SDK 丢弃 price_change 增量（面板价格刷新停滞）

### 症状
用户对比面板与 polymarket.com：同一时刻面板 XRP=0.58，真实已 0.74~0.85，且长时间不动。
实测复现（旧二进制，生产核心）：XRP 面板价**冻结在 0.4650 约 27s**，而真实中价一路
0.585 → 0.645 → 0.655；同期 BTC/ETH/SOL 正常跳动。即**按 token 选择性停滞**，活跃的 BTC 更新频繁、
清淡的 XRP 几乎不动。

### 根因（关键）
Polymarket 市场频道的行为是：订阅时推**一次完整 `book` 快照**，之后只推 `price_change` 增量
（抓包确认：`price_change` 每项都带 `best_bid`/`best_ask`）。
而 SDK 的 `subscribe_orderbook` 实现是：
```rust
Ok(WsMessage::Book(book)) => Some(Ok(book)),
Err(e) => Some(Err(e)),
_ => None,          // <-- PriceChange / BestBidAsk 被静默丢弃
```
我们**只**消费了这个流。结果：本地 book 只在罕见的整快照时刷新，事后全靠增量跳动的部分完全收不到。
清淡 token 的增量少、快照间隔长，于是卡几十秒；BTC 增量多，看起来正常。SDK 另有一条
`subscribe_prices()` 流专门产出 `PriceChange`，我们从未订阅。

### 修复（`Blitzkrieg_core/src/feed.rs`）
在 `poly_orderbook_loop` 里**同时**订阅两路流，`tokio::select!` 一起收：
- `subscribe_orderbook` → 整快照 → `FeedEvent::Book`（原样）；
- `subscribe_prices` → 每个 `price_change` 的 `best_bid`/`best_ask` → `FeedEvent::TopOfBook`。
引擎的 `TopOfBook` → `LocalBook::update_top` 早已实现（会挤掉穿越档位，保持 best 正确），只是此前
没有数据源。两路订阅共享同一个 MARKET channel、对 asset 做引用计数，互不干扰（读 SDK
`subscribe_market_with_options` 确认 interest 置为整个 MARKET）。

### 验证
- 隔离诊断核心（同参数、独立 socket/cwd）：日志出现 `poly orderbook+price subscribed 8 tokens`。
- 修复前后（同一探测脚本，XRP）：
  | | 50s 内更新次数 | 最大 Δ |
  |---|---|---|
  | 修复前 | 3–4 | **0.19**（冻结 ~27s）|
  | 修复后 | 13 | 0.065（单次瞬时，采样与 CLOB REST 的时序差）|
- 部署到生产后复测：XRP 50s 内更新 9 次，各标的 Δ 常态 ≤0.02，无停滞。
- 残留的偶发 0.03–0.07 Δ 是"快速行情下 REST 中价 vs 内核 WS 时刻差"，非死档。

### 影响（为什么重要）
这不只是显示问题：**持仓重估、止盈/移动止损都读同一个本地 book**。价格卡住会让
`unrealizedPct`/HWM 失真，进而延迟或错过出场。修复后 feed 与真实盘口同步。

## 19. P0.6 落地：Polymarket SDK 插件化（Stage 1 接缝 + Stage 2 物理迁移）

§11 曾把主体暂缓（方案 A）。本轮完成，验收标准达成：**内核零市场代码**、
`grep -ri polymarket Blitzkrieg_core/src` 只剩 2 行 feature 注册，且 SDK 依赖已从内核移除。

### 关键结构约束与解法
Cargo 不允许依赖环：扩展若依赖内核，内核就不能再依赖扩展。解法沿用既有
`user_layer/strategy_api` 先例——**再建一个无内部依赖的契约 crate**：
```
blitzkrieg-market-api (新, 无内部依赖: serde/rust_decimal/tokio)
   ↑                                  ↑
blitzkrieg-core                 extensions/polymarket (只依赖 market-api + SDK)
   ↑__________________________________/
             (core 通过 `polymarket` feature 静态链接扩展)
```

### Stage 1 — 接缝（行为零变化）
- 新增 crate `market_api/`：DTO（`Side`/`FillPolicy`/`OrderStatus`/`Fill`/`CoreError`/`OrderIntent`
  /`MarketDescriptor`/`PendingOrder`/`MarketFill`/`ReconcileSnapshot`…）+ 三专职 trait
  `DataFeed`/`MarketDiscovery`/`OrderExecutor` + `MarketPlugin` 打包 + `MarketHost`（内核唯一出入口）
  + `SubscriptionControl`（feed 订阅控制）。全部 boxed-future，对象安全，无 async-trait 依赖。
- **类型单一来源**：`model.rs`/`order/mod.rs` 改为 re-export market_api 的 primitives，消除了重复枚举
  （此前内核与 api 各有一份 `Side`/`FillPolicy`…）。
- 内核 `src/market/host.rs`：`CoreHost` 实现 `MarketHost`，每个方法**纯转发**到既有
  `Core` 方法（`engine_on_data`/`pending_unbound`/`bind_venue`/`ingest_fill`/`reconcile`…）。
- `feed.rs`/`discovery.rs`/`live.rs` 改为直说 `MarketHost`，不再持有 `Core`；`Core.feed` 抽象为
  `Arc<dyn SubscriptionControl>`。
- `src/market/registry.rs`：`MarketPluginRegistry`（与审计用 `ExtensionRegistry` 分离）。
- IPC 新增 `market.list`；Node 客户端加 `listMarketPlugins()`。

### Stage 2 — 物理迁移（内核去 SDK）
- `git mv` 迁移：`venue.rs`、`live.rs`、`feed.rs`、`discovery.rs` → `extensions/polymarket/src/`；
  Gamma slug/解析拆到新 `gamma.rs`（内核只留回合时序数学 `scanner.rs`）。
- 新增 `extensions/polymarket/src/plugin.rs`：`PolymarketPlugin` 组装三组件。
- 内核 `Cargo.toml`：**删除 `polymarket-client-sdk-v2` 与 clob/ws/data/gamma features**；
  新增 optional `polymarket-extension` + `polymarket` feature（默认开启）。
- 装配：`market::register_builtin_markets()` 是内核**唯一**点名具体市场之处，`#[cfg(feature="polymarket")]`。
- 清理死代码：删除孤儿 `MarketAdapter` trait、`PolymarketAdapter`、`BinanceSpotAdapter`
  （零调用点，已被 plugin 接缝取代）。

### 验收证据
| 检查 | 结果 |
|---|---|
| `cargo build --release` / `--workspace` | 通过，零 warning |
| `cargo build -p blitzkrieg-core --no-default-features` | **通过（无市场代码也能编）** |
| 内核测试 | 93 passed（+扩展 4 = 97；差异 2 为删除的孤儿 adapter 测试） |
| `tsc --noEmit` / `npm run build` | 0 error / ok |
| `core-parity.mjs` | 22 ok |
| `parity-engines.mjs` | PARITY OK（identical token/direction/price）|
| `cycle-check.mjs` | PASS（确认→挂单→成交→实时重估）|
| `grep -ri polymarket Blitzkrieg_core/src` | 仅 2 行（`market/mod.rs` feature 注册）|
| 运行期 | 日志显示 `polymarket_extension::feed` 驱动订单簿；`market.list` 返回 polymarket/enabled；books/spots 增长 |

### 附带修复
`scripts/parity-engines.mjs` 注入合成市场却未禁原生 discovery，导致真实回合覆盖注入数据、
比对不确定；补 `--no-discovery`（此为既存测试脚手架缺陷，非本次引入）。

## 20. 下一步（未做）
- 运行期 C-ABI dylib 热加载（P0.7）：扩展已 `crate-type=["rlib","cdylib"]`，需 `#[repr(C)]` vtable +
  `libloading` + 版本协商 + `catch_unwind`（可复用 `strategy_engine/loader.rs` 模式）。
- `--market <name>` 多市场选择（当前 `active_market_plugin` 取第一个已注册）。

## 21. 生产账本混入测试合成数据（core-parity 未隔离工作目录）

### 症状
面板「历史订单」出现 ADA/DOT 做多记录，且**同一对记录重复 3 次**：`$0.40→$0.99`（force_exit）
与 `$0.40→$0.40`（manual），均 `hold=0s`。ADA/DOT 根本不在交易资产（BTC/ETH/SOL/XRP）内，
且 `0s 持有 + 固定价格` 是典型合成数据特征。

### 根因
内核在**相对路径** `data/trades/trades.jsonl` 落盘。两个测试脚手架
（`scripts/core-parity.mjs`、`scripts/parity-engines.mjs`）用 `BlitzkriegCoreClient` 起内核时
**未设置工作目录**，于是继承了仓库根 → 测试下的合成单直接写进了**生产账本**。
`core-parity.mjs` 恰好断言 ADA/DOT 持仓，所以每次运行都追加这一对记录。
（`cycle-check.mjs` 早前已用 `mkdtemp` 隔离；这两个更早的脚本漏了。）

### 修复（两层）
1. **客户端能力**：`BlitzkriegCoreOptions` 新增 `cwd`，`boot()` 传给 `spawn`——测试可把内核跑到
   临时目录；生产不设该字段，行为不变。
2. **各脚手架隔离**：`core-parity.mjs`（3 个客户端）、`parity-engines.mjs`、`dry-observe.mjs`
   全部改为 `cwd: mkdtempSync(...)`。
   - 验证：跑 `core-parity.mjs` 前后生产 `trades.jsonl` 行数不变（11 → 11）。

### 数据清理
- 从生产账本删除 6 行合成记录（ADA/DOT ×3 对），保留 5 行真实成交；`summary.json` 按剩余真实
  记录重算。原始（含假数据）账本另存 `data/backup-with-fake-<ts>.jsonl` 备查。
- 重启内核后 `/crypto-hft status`：`Trades: 5 (3W/2L) 60% WR`，面板不再出现 ADA/DOT。

### 教训
任何会 spawn 内核的脚本都必须隔离 `cwd`，或改用绝对路径的 `--trade-log` / `--near-miss-path`
覆盖；否则测试合成数据会污染生产账本。后续若给内核加 `--trade-log` CLI 覆盖项，可进一步根治。

## 22. 内核新增 `--trade-log` / `--no-trade-log`（根治测试污染）

### 背景
§21 用「隔离工作目录」修补了测试污染，但那仍依赖「内核用相对路径」这一隐含约定。
本轮给内核加**显式 CLI 覆盖**，让测试不再依赖 cwd。

### 改动
- `Blitzkrieg_core/src/main.rs`：新增 `--trade-log <path>`（覆盖默认
  `data/trades/trades.jsonl`）与 `--no-trade-log`（完全关闭成交持久化）；装配进
  `CoreConfig.trade_log_path`。
- `src/core/blitzkrieg-core-client.ts`：`BlitzkriegCoreOptions` 增 `tradeLogPath` / `noTradeLog`，
  `boot()` 据此拼参数（`noTradeLog` 优先）。
- 所有测试脚手架（`core-parity.mjs` 3 个客户端、`parity-engines.mjs`、`dry-observe.mjs`）
  改用 `noTradeLog: true`——它们只用事件/持仓断言，不需要持久化账本；`cycle-check.mjs`
  加 `--no-trade-log`。这样与 cwd 无关，**双保险**。

### 验证
- 新增 `scripts/trade-log-flag-check.mjs`：`--trade-log <tmp>` 只写临时文件（核心仍见 1 笔），
  `--no-trade-log` 只留内存态；两次运行生产账本 **7 → 7 不变**。
- 依次跑 core-parity / parity-engines / cycle-check / flag-check，生产账本 7 → 7 不变。
- 生产核心无 `--trade-log`/`--no-trade-log` 参数 → 仍用默认常规账本（行为不变）。

## 23. P0.6 收尾完善（多市场选择 + 文档对齐）

P0.7（运行期 dylib 热加载）按用户决定**暂缓**；本轮只把 P0.6 的松散点补齐。

### 功能
1. **运行期市场选择**：内核新增 `--market-plugin <name>`，`CoreConfig.market_plugin`，
   `active_market_plugin(registry, preferred)` 按名选择；未注册则回退第一个并打印告警（不 brick）。
   Node 客户端加 `marketPlugin` 选项。
2. **`market.list` 标注生效项**：`PluginInfo` 增 `active`；结果增顶层 `active`。
   内核实测：`{"active":"polymarket","plugins":[{"name":"polymarket",...,"enabled":true,"active":true}]}`。
   新增 `scripts/market-plugin-check.mjs` 覆盖显式/默认/未知三种选择（PASS）。
3. Node zod：`MarketListSchema` / `MarketPluginInfoSchema`。

### 文档对齐（此前与实现脱节）
- `ARCHITECTURE.md`：版本改 P0.6；分层图改为「内核 → market_api → extensions/polymarket」；
  删除已移除的 `MarketAdapter`/`PolymarketAdapter`/`BinanceSpotAdapter` 章节，改为
  三专职 trait + `MarketHost` 契约；新增 §5「两套插件系统（Extension vs MarketPlugin）」对照表；
  目录树更新（market_api / extensions/polymarket）。
- `EXTENSION_GUIDE.md`：新增 §2.5 市场插件契约；`config.toml` 明确标注**尚未实现（仅文档）**；
  IPC 表加 `market.list`；验收清单改为实际情况（配置驱动加载、P0.7 未做）。
- `INTERFACES.md` §2.6：加 `market.list` 契约，区分 `extension.*` 与 `market.*`。
- `extensions/README.md`：重写为两套插件对照 + 依赖方向硬约束 + 配置仅文档说明；
  修正失效的 `docs/EXTENSION_GUIDE.md` 路径。

### 验收
93 内核测试 / 4 扩展测试 / 0 warning；`--no-default-features` 可编；tsc 0 error；
core-parity 22 ok；parity-engines PARITY OK；cycle-check PASS；market-plugin-check PASS。
生产重启后单实例、`market.list` active=polymarket。

## 24. Soak 观察期（进行中）

生产运行在新的插件化构建上，`scripts/soak-monitor.mjs --hours 12 --interval-sec 300`
每 5 分钟写 `data/soak/soak.jsonl`（人类可读汇总 `data/soak/soak.log`）。

### 健康判据（每周期应满足）
- `node=up core=up ping=ok`
- `books`/`spots` 计数持续增长（feed 活性）
- `err+0`（无新增 ERROR）
- 单核心（`pgrep -f clodds-core-fancer.sock` = 1）

### 待对比的经营指标
1. **成交频率 vs Node 基准**：Node 历史约 0.75 笔/回合、3 笔/小时，56% 回合零成交。
   需覆盖 ≥3–4 小时（Node 时代最大成交间隔 3.4h）才具可比性。
2. **胜率 / 盈亏比 / 净额**：当前真实样本仅个位数，不足以判断策略优劣。
3. **出场原因分布**：重点看是否有 `holdTimeSec=0` 的 `force_exit`（秒平 bug 复发的信号，
   若出现须立即排查——历史上该 bug 已修，但需持续确认）。
4. **entry-gate 松紧**：`blocked.timing` 与 `blocked.momentum` 的累计值，配合
   `data/shadow/near-miss.jsonl` 用 `--replay-near-miss` 离线评估是否值得放宽。

### 复看命令
```
tail -40 data/soak/soak.log
grep -c "⚠" data/soak/soak.log        # 异常周期数
wc -l data/trades/trades.jsonl          # 真实成交数（清账后从 5 起）
node -e "const fs=require('fs');const r=fs.readFileSync('data/trades/trades.jsonl','utf8').trim().split('\n').map(JSON.parse).slice(-10);console.log('n',r.length,'wins',r.filter(t=>t.netPnlUsd>=0).length,'avgHold',Math.round(r.reduce((a,b)=>a+b.holdTimeSec,0)/r.length)+'s','reasons',[...new Set(r.map(t=>t.exitReason))].join(','),'net',r.reduce((a,b)=>a+b.netPnlUsd,0).toFixed(2))"
```

## 25. 【已修复】核心崩溃重启自锁循环（"not connected" 故障）

### 症状
`/crypto-hft status` 报 `Error: blitzkrieg-core not connected`。日志显示 5 分钟内
**~1300 次** `blitzkrieg-core process exited`，紧凑循环。

### 根因链
日志里每个崩溃都带：
```
Error: another blitzkrieg-core is already listening on .../clodds-core-fancer.sock
code: 1
```
即 **§16.3 加的 socket 单实例守护**（本该是好事）与 Node 客户端的自动重启逻辑相互作用，
引爆了一个潜伏缺陷：

1. 存在一个健康核心占用 socket（本例是 00:51 启动的孤儿，其父进程已不在）。
2. Node 客户端 `spawn` 新核心 → 新核心探测到 socket 已被占用 → **bail (exit 1)**。
3. `handleExit` 立刻无退避、无上限地 `start()` 重试 → 再 spawn → 再 exit 1……
   每轮仅 ~15ms，形成 ~80 次/秒的 spawn/crash 风暴。
4. 同时 `connectWithRetry` 又连上了那个**健康**核心 → `connected` 短暂 true，
   进程一退又 false。状态查询永远拿不到稳定连接 → "not connected"。

历史对比：修复前（无单实例守护）第二个核心会**静默 unlink 并抢占 socket**，
于是循环"自愈"——正是 §16.3 修掉的那个静默接管 bug。守护本身正确，暴露的是客户端侧问题。

### 四个客户端缺陷（均已修，`src/core/blitzkrieg-core-client.ts`）
1. **失败的 `start()` 遗弃客户端**：`doStart` 里 `await client.start()` reject 时，
   `this.client` 仍为 null 且**这个 client 无人引用**，但它的 auto-restart 已在后台跑；
   `runner.stop()` 只停 `this.client`，够不着它 → 孤儿循环。
   修复：`start()` reject 时 `teardownProc()` 杀掉子进程；runner 捕获并保持 `client=null`。
2. **重启无退避、无上限**：改为指数退避（300ms→30s 封顶）+ 硬上限 5 次；
   超限 emit `fatal` 并停止（可恢复为可见错误，而非无限风暴）；连接成功即清零计数。
3. **socket close 与 process exit 双重调度重启**：加 `restartTimer` 去重，单次调度；
   并用 `activeSocket` 忽略旧 socket 的 stale close。
4. **"已被占用"时无限重试**：识别 stderr 中的 `already listening/in use` →
   **改为接管（adopt）现有核心**（只连接、不 kill），而非继续 spawn。
   `stop()` 只杀自己 spawn 的进程（`ownsProc`），接管的核心不动。

### 验证
新增 `scripts/core-adopt-check.mjs`：健康核心 + 第二个客户端指向同一 socket →
客户端**接管成功**（`adopted the existing blitzkrieg-core`）、无 spawn 循环、
`stop()` 后核心仍存活。PASS。
回归：core-parity 22 ok / parity-engines OK / cycle-check PASS / market-plugin-check PASS；
重启生产后崩溃计数 **0**。

### 运维注意
若再次出现 `not connected`：`pgrep -f blitzkrieg-core` 看是否有多个核心；
`grep -c "process exited" run.log` 看是否在循环；正常情况下客户端现在会接管而非循环。

## 26. 基于交易+影子数据的策略调优（出场参数）

### 数据与方法
- `data/trades/trades.jsonl`（59 笔）+ `data/shadow/near-miss.jsonl`（61 条被拦候选）
  + `data/backup-20260914-001139/shadow/positions.jsonl`（62 笔**带完整价格路径**的记录）。
- 用内核自带的 `--replay`（`shadow::walk_forward_file`，逐 tick 重放真实退出策略）做
  走前验证；入场用 `--replay-near-miss` 做反事实。分析脚本 `scripts/analyze-strategy.mjs`。

### 诊断（交易数据）
- 总体 WR 66%(39W/20L) 但净 **−$6.69**，PF **0.83**：赢家 +$0.81/笔，输家 **−$1.91/笔**，**不对称**。
- **17 笔 stop_loss 全亏、合计 −$36.85**，每笔 lowPnl 达 −30~−60%（打到 −50% 止损）；
  平均亏损 −$2.17 vs 平均盈利 +$0.77。**结论：问题在出场，不在入场。**
- 近失反事实：放宽入场/时点闸门只值 **+$1.16/61 笔（≈持平）** → 入场闸门没做错，不动它。

### 反事实（路径回放，按 62 笔）
| 配置 | 胜率 | 盈亏比 PF | 净额 |
|---|---|---|---|
| 旧 SL50/trail10/TP100 | 66% | 1.55 | $15.89 |
| **新 SL15/trail8/TP20** | **66%** | **2.69** | **$27.01** |

- 止损收紧到 **15%**：把单笔最大亏损从 ~−50% 压到 −15%（约 −$2.4 → −$0.7）。
- `min_trail_pct` 由 10→**8**：减少赢家回吐（该项是回吐**下限**，10 会强制多吐）。
- 止盈由"关闭"→**20%**：把边际赢家落袋，**同时抬高胜率**（TP 是提升 WR 的杠杆）。
- 稳健性：去掉贡献最大的 2 笔后仍优于旧配置（PF 2.19 vs 2.28→旧 1.28）；去掉最差 2 笔同样更好。
  止损曲线在 8% 以下转差，15% 为稳定中位（未取 12% 的过拟合尖峰）。
- **胜率 vs 盈亏比 是权衡**：TP=15 可把 WR 提到 68% 但 PF 降到 2.49；TP=20 保 WR 66% 且 PF 最高。
  采用 TP=20 兼顾"胜率持平/略优 + 盈亏比大幅提升"。加 breakeven 止损会再抬 PF 但**降** WR，未采用。

### 改动
`Blitzkrieg_core/src/exit_policy.rs` `ExitConfig::default()`：`take_profit_pct` 100→20、
`stop_loss_pct` 50→15、`min_trail_pct` 10→8（含解释注释）。`shadow::default_grid()` 重写为
"旧配置 / 新配置 / 邻居点"用于持续验证。更新受影响的 1 个测试。
验收：93 tests、0 warning、core-parity 22 ok、parity-engines OK、cycle-check PASS；生产已重启生效。

### 适用性与局限
样本 62 笔、单一 regime（多为低波动），个位数显著性；**需在新参数下继续 soak 验证**，
若 WR/PF 不及回放预期则回退（改回 50/10/100）。入场侧无稳健可用的过滤因子
（方向不对称在修好出场后即消失——说明它本是出场问题；入场上下文各桶样本 n≤5，不可用）。

## 27. 止盈改为「固定兜底 + 移动止盈」，与 Node 对齐

用户要求止盈设计回到 Node 一致：**固定止盈仅作兜底，正常由移动止盈锁利**。
核对后发现 Rust 的移动止盈逻辑**本就与 Node 完全一致**（`get_profit_trail_pct`
表 15/12/9/6/4/3/2、`get_time_trail_pct` 12/8/6、`trailing_min_high_pct=15`、
`proportional_trail_*` 同值）；Node 侧的 `trailingLatePct/trailingMidPct/trailingWidePct`
是**定义了但从未使用**的死配置。唯一真正的分歧是我 §26 调的三个值。

### 改动（`ExitConfig::default()`）
| 参数 | Node | §26 我改的 | 现在 | 理由 |
|---|---|---|---|---|
| `take_profit_pct` | 100 | 20 | **100** | 固定止盈回归"只作兜底"；正常利润交给移动止盈 |
| `trailing_min_high_pct` | 15 | 15 | **15** | 移动止盈触发线，本就一致 |
| `min_trail_pct` | 10 | 8 | **10** | 移动止盈回吐下限，与 Node 一致 |
| `stop_loss_pct` | 50 | 15 | **15** | **唯一保留的偏离**：数据证明 50% 是最大亏损源 |

### 为什么保留止损偏离（而非全套照抄 Node）
62 笔路径回放（`--replay`）：
- Node 原配（TP100/SL50/tr10）：WR 66%、PF **1.55**、净 $15.89、平均亏损 −$1.37。
- 本次部署（TP100/**SL15**/tr10）：WR ~61-63%、PF **2.44-2.51**、净 $24-25、平均亏损 −$0.70。

即：止盈对齐 Node **不损失**收益；真正止血的是止损 50→15。两者的**组合**就是
"固定兜底 + 移动止盈 + 合理止损"。已向用户说明；若要求 100% 与 Node 一致，
把 `stop_loss_pct` 改回 50 并 `SHADOW`/重启即可（不建议，回归 PF 1.55）。

### 其它
`shadow::default_grid()` 的基准点更新为「旧 / 现部署 / 邻居」，用于持续走前验证。
93 tests、0 warning；生产已重启（binary 08:31:05，核心 08:31 起，单实例，0 崩溃）。

## 28. 【更正 §26/§27】出场参数定稿：SL12 / trail8 / TP100（追求最终利润）

### 更正声明
§26 报告的「新 SL15/trail8/TP20 → WR66% / PF2.69 / net$27.01」**无法用规范模拟器复现**，
属**错误数字**（当时用了两套不一致的脚本，TP 触发价按 `pct` 而非封顶到 `c.tp`，
且净额换算有误）。以 `scripts/reconcile-exits.mjs`（单一实现、镜像 Rust `decide_exit`）重算：

| 配置 | 胜率 | 盈亏比 | 净额 | 平均盈/亏 |
|---|---|---|---|---|
| 旧 SL50/tr10/TP100 | 66% | 1.53 | $15.11 | +$1.07 / −$1.37 |
| §26 TP20/SL15/tr8 | 66% | 1.60 | **$9.64** | +$0.63 / −$0.76 |
| 现部署前 TP100/SL15/tr10 | 61% | 2.39 | $23.39 | +$1.06 / −$0.70 |

即 **§26 把净额夸大了近 3 倍**。TP20 提前截断赢家，净额反而从 $15 掉到 $9.6，是错的。
§27 的止盈"回到 Node 兜底 TP100"方向正确（纠正了 §26）。

### 定稿（按用户目标：最大化最终利润）
单一规范模拟器在 `SL∈{8,10,12,15,20} × trail∈{6,8,10,12} × TP∈{15..100}` 上全网格搜索：

| 配置 | 胜率 | 盈亏比 | 净额 | 去最赚2 | 去最赚5 | 去最亏5 |
|---|---|---|---|---|---|---|
| **SL12/tr8/TP100（部署）** | 63% | 2.93 | **$27.35** | P2.44/$20.3 | P1.93/$13.1 | P3.90/$30.8 |
| SL12/tr6/TP100 | 63% | 2.97 | $27.89 | P2.48/$20.9 | P1.96/$13.6 | P3.95/$31.4 |
| SL8/tr6/TP100 | 56% | 3.02 | $25.33 | — | — | — |
| 前部署 SL15/tr10 | 61% | 2.39 | $23.39 | — | — | — |

**选择 SL12/tr8**：净额 $27.35 与冠军 tr6($27.89) 相差仅 ~$0.5（噪音级，且 tr6 偏离 Node 更大），
却明显优于 SL15/tr10（+$4 净额、PF 2.39→2.93），且在四次稳健性检验中均不劣。

### 关键澄清（回答"能否 75% 胜率 + 放大盈亏比"）
- **75% 胜率可达，但只能牺牲盈亏比**：`TP10/SL12` → WR76% 但 PF1.08、去最赚5 笔后 PF<1（不赚钱）。
  胜率与盈亏比在此样本上是**跷跷板**，二者不可兼得。
- **保本锁（breakeven）在修正语义后并不提升净额**：完整期货回放显示`SL12/tr6/BE@8` 净额 $24.74
  **低于**不带 BE 的 $27.89（BE 把本会反弹的赢家提前扫出）。故**未启用**。
  （§之前的 $27.22 是 BE 在成本价成交的过乐观假设所致。）
- 结论：**追求最终利润 = SL12 / trail8 / TP100**（已部署）。

### 改动与验收
`exit_policy.rs::ExitConfig::default()`：`stop_loss_pct` 15→**12**、`min_trail_pct` 10→**8**，
`take_profit_pct` 保持 100。93 tests、0 warning；生产已重启（binary 08:53:07，单实例，0 崩溃）。
分析脚本：`scripts/reconcile-exits.mjs`（权威）、`scripts/final-exit-opt.mjs`（含稳健性）。

## 29. 【实盘安全层】订单持久化 + 崩溃恢复 + 启动清算（修复孤儿订单）

### 背景 / 根因
用户反馈：早期 Node↔Rust SDK 桥接时亏掉约 12u（链上核实：该 funder 15 笔成交，买 $33.14/卖 $22.19，
净 **−$10.95**；含一笔 59.99 股 SOL 在 −53% 清仓 ≈ 当年 SL50 病），且"机器人不知道自己下了单、
无提示、全是孤儿订单、最后人工平仓"。

根因：**Rust 内核的订单状态只在内存**（`ome.rs` 无任何持久化），而 Node 时代的
`order-manager.ts` 本来有 `data/orders/orders.jsonl` + `loadAllOrders()` 重启恢复——重写时丢了。
于是：重启/崩溃 → 内存清空 → 交易所仍有挂单 → 内核不认识 → 不撤、不报 → 孤儿。

### 实现（全部落地）
1. **`order_db.rs`（新）**：`TrackedOrder` 快照追加到 `data/orders/orders.jsonl`（append-only，含
   `venueOrderId`）；`load()` 折叠为每单最新状态；`compact()` 重写为仅存未结单；损坏/旧格式行跳过不致命。
2. **`Ome::restore()` + `known_venue_ids()`**：从持久化记录重建订单状态；导出"本地认为在挂的 venue id"。
3. **Core 接线**：`CoreConfig.order_log_path`（默认 `data/orders/orders.jsonl`）；`emit_order`/`emit_fill`
   两个汇聚点调用 `persist_order`，覆盖 submit/confirm/reject/bind/cancel/fill **所有**状态变更；
   `restore_orders()` 启动时恢复未结单并把日志 compact。
4. **启动清算**：`live.rs` 启动时拉交易所快照 → 凡"交易所有、本地 `known_venue_ids` 里没有"的，
   `cancel` 掉（孤儿归零）并 `note_orphan_cancelled` 上报。每次 live 启动只跑一次。
5. **可见性**：孤儿撤销、恢复 N 单等均经 `Event::RiskAlert` 推送（不再静默）。
6. **拆除无状态下单路径**：`rustPlaceLimitOrder` 加硬闸——默认**拒绝**下单并返回明确错误，
   仅 `ALLOW_STATELESS_RUST_EXECUTOR=true` 才放行（该 executor 签单但不追踪，正是孤儿根源）。
7. **CLI**：`--order-log <path>` / `--no-order-log`。

### 验收
- 新增 `scripts/order-recovery-check.mjs`：放置挂单 → `SIGKILL` 内核 → 新内核同 order-log 启动 →
  **挂单被恢复**（status=LIVE，key 一致）。**PASS**。
- 95 内核测试（含 order_db 的 append/load-latest/legacy-skip 2 项）+ 4 扩展测试、0 warning、tsc 0。
- 全部脚本门禁：core-parity 22 / parity-engines OK / cycle-check PASS / market-plugin-check PASS /
  order-recovery PASS / core-adopt PASS。
- 生产已重启（binary 09:07:34，单实例，0 崩溃）；订单库将在首次下单时生成。

### 结论
"机器人不知道自己下单"的失效链已断：**订单落盘 → 重启恢复 → 孤儿自动撤销 → 全程可见**。
这是 4.8u 实盘的前置条件，现已具备。

## 30. 迁移后清理 + 订单落盘线上确认（新根目录）

### 背景
`fc6e93c` 把项目根从 `/Volumes/Hard Disk/BlitzkriegBot/CloddsBot` 上移一层。迁移留下两处
残留，且 §29 的订单库"首次下单才生成"尚未在真实运行中确认。

### 清理动作
1. **删除 `polymarket-5m-bot/`**：迁移时的 cwd 锚点目录，内容只剩 `.DS_Store`，无任何进程
   cwd 指向它（`ps` 核实），已删。
2. **游离凭据文件 `.env.other-project-backup-2025-11`**（属另一个项目 *Hummingbot Deploy*，
   含其真实 USERNAME/PASSWORD/BROKER_*/DATABASE_URL/AWS_* 凭据）：代码中无引用（`.gitignore`
   除外），本机附近也不存在该 Hummingbot 项目 → 已**移入废纸篓**（`~/.Trash/`）而非直接销毁，
   可恢复。`.gitignore` 的 `.env.other-project-backup-*` 规则保留，防止误提交同类文件。

### 线上确认（§29 订单持久化）
- 恢复 soak 后，`data/orders/orders.jsonl` 已由空增长到多次快照：`dry_1..dry_6`，
  覆盖 `LIVE→CANCELLED`、`FILLED` 等终态，说明 `emit_order`/`emit_fill` 落盘链路在真实运行时
  正常工作（不再只存在于 DRY 单元验证）。
- 生产账本 `data/trades/trades.jsonl`：**67 笔 / 45 胜（67% WR）/ 净 −$4.79**
  （出场分布：trailing_stop 43 / stop_loss 19 / take_profit 3 / time_exit 2）。
- 健康 `http://localhost:18789/health` → 200。

### 仍未闭环
- **live 启动清算（孤儿扫单）只在 DRY 验证过**：首次真实 live 启动须确认日志出现
  `startup sweep cancelled N orphan order(s)`。
- **`--min-shares`/`--max-shares` CLI 尚未加**：当前硬编码每笔 10 股（≈$4.5/笔），
  4.8u 资金只够 1 仓；若要在该资金下持 2 仓需降到 ~4 股。
- **`OpenPosition` 仍未持久化**：与 §29 同失效类——内核重启会忘记未平仓持仓，
  可能错过止损/止盈。建议下一步按 `order_db` 同款实现。

## 31. 每笔股数可配：`--min-shares` / `--max-shares`（4.8u 实盘前置）

### 背景
§30 遗留项：每笔固定 10 股是硬编码（`min_shares=max_shares=10`），无 CLI 可调。
小资金实盘（4.8u）只够 1 仓；要在该资金下持 2 仓必须能把单笔降到 ~4 股。

### 改动
1. **内核 CLI**（`main.rs`）：新增 `--min-shares <n>` / `--max-shares <n>`，解析为 `Decimal`
   并接入 `CoreConfig.min_shares/max_shares`（`engine.rs::compute_shares` 已消费该字段）。
   防御：解析失败/缺省回落 10；若 `min>max` 则告警并把 min 夹到 max。
2. **Node 透传**（`blitzkrieg-core-runner.ts`）：`BlitzkriegRunConfig` 增 `minShares`；
   `extraArgs` 追加 `--min-shares`/`--max-shares`；`maxOrderNotional` 由 `maxShares*0.6` 推导，
   已随之下调（4 股 → 上限 2.4，不再误拒小单）。
3. **Skill 参数**（`crypto-hft/index.ts`）：`/crypto-hft start` 读取 `HFT_MIN_SHARES`/`HFT_MAX_SHARES`
   环境变量（缺省用 `DEFAULT_CONFIG` 的 10/10），启动回显 `Lot: min–max sh`。
4. **文档**：`.env.example` 增 HFT 段；`HANDOFF.md` §4/§7 更新。

### 验收
- 内核测试 **96 项**通过（新增 `compute_shares_honours_custom_bounds`：4 股固定档 + 宽档 [2,20]）。
- `tsc --noEmit` 干净；门禁脚本全 PASS：core-parity 22 / parity-engines / cycle-check /
  order-recovery / market-plugin-check。
- 新 CLI 冒烟：传入 `--min-shares 4 --max-shares 4` 启动无 `unknown arg`，正常监听。

### 用法
```bash
# 经 Node（推荐）：
HFT_MAX_SHARES=4 HFT_MIN_SHARES=4 node dist/index.js   # /crypto-hft start
# 直接跑内核：
./target/release/blitzkrieg-core --socket <path> --min-shares 4 --max-shares 4 ...
```

## 32. 【实盘安全层·续】持仓持久化 + 崩溃恢复（修复"重启后持仓失管"）

### 背景 / 根因
§29 修好了**订单**的孤儿问题，但**未平仓持仓**仍是内存态（`PositionManager`）。内核重启/崩溃后
持仓清空 → 不再估值、不再跑出场规则、也不知道自己持有该 token → 该笔交易"漂到"到期无人管理。
这与孤儿订单是同一失效类，只是对象从"挂单"换成"已成交持仓"。

### 实现
1. **`position_db.rs`（新）**：`save()` 每次把当前**未平仓集合**整体重写为
   `data/positions/positions.jsonl`（每行一个 `OpenPosition` 快照）；`load()` 读回。
   持仓数受 `max_positions` 限制且生命周期短，故用"整体重写"而非追加日志——平仓无需墓碑行即可表达。
   损坏/外来行跳过不致命（与 `order_db` 一致）。
2. **类型**：`OpenPosition` 与 `ExitState` 加 `Serialize/Deserialize`（`ExitState` 带
   HWM/确认计数等，必须一起恢复，否则出场逻辑从头开始）。
3. **`PositionManager::restore_open()`**：替换内存 open 书，并把 `next_id` 推进到恢复的
   `hft-N` 之后，避免新仓与恢复仓 id 冲突。
4. **Core 接线**：`CoreConfig.position_log_path`（默认 `data/positions/positions.jsonl`）；
   在 `project_fill_delta` 的**每个** open/adjust/close 分支后调用 `persist_positions()`；
   `restore_positions()` 启动时恢复（`ipc/server.rs` 中在 `restore_orders()` 之后）并经
   `Event::RiskAlert` 上报。
5. **CLI**：`--position-log <path>` / `--no-position-log`。
6. **测试隔离**（回归修复）：`BlitzkriegCoreClient` 新增 `noOrderLog`/`noPositionLog`；
   core-parity / parity-engines / core-adopt 各自关闭订单/持仓日志——否则同一 WORKDIR 下
   前一个 harness 的持仓会被下一个内核恢复进来（与 §21/§22 账本污染同类）。

### 验收
- **新增 `scripts/position-recovery-check.mjs`**：开仓 → `SIGKILL` 内核 → 新内核同 position-log
  启动 → **持仓被恢复且继续估值**（`cur=0.70`，unrealized 62.8%）。**PASS**。
- 内核测试 **98 项**通过（新增 position_db 的 round-trip 与 corrupt-skip 2 项）。
- 门禁全 PASS：core-parity 22 / parity-engines / cycle-check / order-recovery /
  **position-recovery** / market-plugin-check / core-adopt。
- `tsc --noEmit` 干净、`npm run build` OK。

### 生效说明
生产当前进程（09:22 启动）跑的是**旧二进制**；新持仓日志将在**下次内核重启**后开始写入。
不改动正在进行的 soak，重启时机由用户决定。

### 结论
"重启后持仓失管"链已断：**持仓落盘 → 重启恢复 HWM/出场状态 → 继续估值与出场**。
与 §29 合起来，订单与持仓两条崩溃恢复路径均已闭环。

---

## 33. 【影子进化保真度】D-2/D-3 修复：同刻观测 + 定向变异 + 统一 ExitConfig + 样本跨回合

### 背景
§shadow（`docs/reports/SHADOW_EVOLUTION_REPORT.md` 首版，提交 `3f28e0d`）曾报告：影子进化机制正确且安全，
但在整机 A/B 中 **0 次触发**（B 组 ≡ A 组）。深入定位后确认这不是工况偶然，而是**三处建模保真度缺陷**
（DECISIONS_PENDING 的 D-2/D-3）。本次全部修复并复跑，进化为整机 A/B 带来真实增益。

### 三处缺陷与修复
1. **变体等比例缩放四参数**（`variants.rs::build_variants`）：收紧入场价上限的同时也下调了入场因子，
   两个方向对入场价**相消** → 变体决策与基准逐位相同，永远无法"更优"。
   **修复**：改为**定向单旋钮变异**——每个变体只动 4 个可变参数中的 1 个（奇偶交替保守/激进），
   差异可归因到单一旋钮。
2. **变体每回合重建**（`mod.rs::on_round`）：每回合重置样本，生产 900s 回合内凑不满 `min_sample_count=30`，
   进化**结构上不可达**。**修复**：`on_round` 只更新到期时间 + `Variant::retain_tokens`（丢弃已过期 token 的
   未平仓虚拟仓），**保留 closed 历史 → 样本跨回合累积**。
3. **(a) 影子滞后一档观测；(b) 变体出场用 50% 硬止损而 live 用 12%**。
   **修复**：(a) `service.rs::engine_on_data` 改为在引擎**消费该 tick 之后**喂入**同刻盘口与同刻确认**；
   (b) `ShadowEvolutionConfig` 新增 `exit_cfg: ExitConfig`，由 `Core::new` 从 `config.positions.exit` 注入，
   变体一律复用 **live 的 `ExitConfig`**。

### 防自强化
应用一次进化后**按新参数重建变体集**：此时旧历史描述的是已作废的参数，若沿用会让基准的历史亏损
持续让同一旋钮"获胜"、形成每冷却周期下调一档的**棘轮**。重建不是旧的"每回合清零"缺陷——
样本现已跨回合累积，进化仍可达。

### 验收
- **整机 A/B（修复后）**：A=`70 trades / WR 85.7% / PF 4.09 / net 78.36 / dd 2.532`；
  **B=`65 trades / WR 92.3% / PF 8.19 / net 91.02 / dd 2.532`**。进化 **applied=1**，
  轨迹单调：`trend_max_entry_price 0.45 → 0.4365`（−3%，在 ±5% 锁内）。**B>A 且回撤不变**。
- **安全锁**：`+20%` 手工下发仍被 Lock 1 硬拒（`gradient too large ... 0.20 > 0.05`）。
- **EXP-C**（组件级，直接驱动生产 `ShadowEvolution`）：`applied=1`，best variant `wr 1.0 / pnl 54.93` vs
  baseline `wr 0.75 / pnl 24.55`。
- 新增单测 4 项（定向单旋钮 / live ExitConfig / 历史跨回合保留 / 配置含 exit_cfg）；内核测试 **101 项**通过。
- 门禁全 PASS：`cargo build --release`、`cycle-check`、`core-parity`、`parity-engines`、
  `order-recovery`、`position-recovery`、`npm run typecheck`。
- 生产未扰动：内核单实例（PID 73052）照常、`/health` healthy、DRY、未改动 `.env`；实验全程关闭四类日志。

### 生效说明
生产当前进程跑的是**旧二进制**；本次改动将在**下次内核重启**后生效。是否在生产开启影子进化
（默认仍 `enabled=false`）由用户决定（见报告 §6）。

## 34. 【P-1.1 多策略执行打通】engine.rs 宿主化 + 统一注册表 + 按策略分账/限额

### 背景
`ROADMAP_INSTITUTIONAL.md` §1/§3 的最大结构性债务：**两套分裂的注册表**——`strategy_engine/`
（C ABI 用户策略，`register/enable/on_tick`）与生产路径 `engine.rs`（硬编码 `spread_arb`）。
用户策略抽象"已写好但从不影响交易"。本次把 `engine.rs` 改造成**多策略宿主**，保持行为逐位等价。

### 实现
- **新模块 `strategies/`**：
  - `EngineStrategy`（宿主契约，`Send + Sync`）：`on_book`/`on_round`/`find_candidates`/`take_breaks`/
    `confirmed_tokens`/`diagnostics`/`set_hot_params`/`spread_arb_view`/`on_config`；只读视图 `StrategyCtx`
    （markets、回合 slot/剩余时间、`fresh_book` 新鲜度过滤闭包）。
  - `SpreadArbBuiltin`：现役 `spread_arb` 的宿主化实现（趋势跟踪 + 热参数覆盖 + 候选评估 +
    `{token,mid,entry,cap,inBand}` 诊断），`internal_key` 格式 `spread_arb:{asset}:{dir}:{slot}` 不变。
  - `UserStrategyAdapter`：C-ABI `Strategy` → 宿主策略（`MarketTick` 由 `StrategyCtx.fresh_book` 构造，
    过 `validate_signal` 闸；`Sell` 仍由内核退出策略管理，适配器忽略）。**注册后默认 `enabled=false`**。
- **`engine.rs` 宿主化**：`Engine` 持有 `Vec<HostedStrategy{strategy, enabled, source}>`，`new()` 注册
  builtin（enabled、`source="builtin"`）。`on_data` 把 Book/TopOfBook/RoundMarkets 转发给每个策略
  （再喂 near-miss 记录器），`take_breaks` 取并集。`evaluate()` 汇总各启用策略候选，**共享闸门**：
  pending token 抑制 → **每 token 每周期至多一单**（注册序先到先得）→ 回合时序闸 → 现货动量闸 →
  `compute_shares` 定仓 → 下单。注册表 API：`supported_strategies`/`enabled_strategies`/
  `set_strategy_enabled`/`register_user_strategy`（重名拒绝）/`strategy_source`。
- **`strategy_engine/` 收敛**：删除死代码 `builtins.rs`（第三份 `SpreadArbStrategy`，无引用）；
  新增 `loader::load_boxed`（返回 `LoadedStrategy{strategy,name,version}`，供引擎注册）。
  `load_strategy_lib`：引擎在位 → 注册进引擎调度（disabled）；否则退回独立注册表。
  删除前用 `git grep "SpreadArbStrategy" main -- '*.rs'` 确认**除该文件自身定义外全树无引用**。
- **按策略分账（`service.rs`）**：`StrategyAccounting{placed,rejected,limit_rejected,closed_trades,
  wins,losses,fees_usd,net_pnl_usd}`（会话级）；入场在 `engine_evaluate` 记账；平仓在
  `on_position_closed` 按 `closed.strategy` 记 PnL/费用/胜负。`engine.stats` 新增
  `strategyLimitRejected` 与 **`strategies[]`**（含实况敞口 `openPositions`/`openNotionalUsd`，
  金额为 JSON 数字，Node 侧类型同步）。
- **按策略限额**：`CoreConfig.strategy_limits: HashMap<String, StrategyLimit{max_open_positions?,
  max_open_notional_usd?}>`（默认空 = 完全不改行为）；CLI `--strategy-limit name:max_open:max_notional`
  （可重复，`-`/空段 = 不限，畸形值告警跳过）。超限入场在 `place` 之前被拒，**不产生订单**。
  语义：`limitRejected` 按「被拒的入场尝试」计数（每次评估一次，同 `placeRejected` 的口径），
  候选持续存在时会逐周期累加——这是刻意与既有计数器口径一致。
- **Node 侧透传（生产可运维）**：`BlitzkriegRunConfig.strategyLimits?: string[]` →
  `blitzkrieg-core-runner.ts` 逐条转 `--strategy-limit`；`/crypto-hft start` 读取环境变量
  **`HFT_STRATEGY_LIMITS`**（逗号分隔，如 `spread_arb:2:20`）并在启动回执中列出。未设置 = 无参数 = 行为不变。
  客户端 `stats()` 类型同步新增 `strategyLimitRejected` 与 `strategies[]`（金额为数字）。

### 行为等价（硬门槛）
- 单策略（默认配置）下逐位等价：候选顺序、`internal_key`、near-miss/blocked 遥测（时序/动量）、
  新鲜度规则（`is_fresh` + 非空簿 + `max_orderbook_stale_ms`）全部保持；`emitted` 去重对单策略是 no-op。
- `node scripts/parity-engines.mjs` → **`PARITY OK: identical token/direction/price`**（Node 参考实现 vs Rust）。
- `node scripts/cycle-check.mjs` → **`RESULT: PASS — order placed, filled, and position valued`**。
- `node scripts/core-parity.mjs` → **`RUST CORE PARITY OK`**。
- 端到端限额验证（临时 harness，/tmp）：无限制 → 1 张 LIVE 单、`ordersPlaced=1`；
  `--strategy-limit spread_arb:0:-` → **0 张单**、`strategyLimitRejected>=1`、`ordersPlaced=0`、敞口 0。

### 测试与门禁
- 内核单测 **110 项**通过（基线 102 + 引擎宿主 4 + 服务分账/限额 4）：
  注册表初始仅 builtin/未知开关拒绝、用户策略默认禁用+启用后带标签下单、双策略并行各自记账、
  禁用 spread_arb 即停单、无限额行为不变、仓位上限拒绝且计数、名义上限拒绝/放行、
  平仓落账（closedTrades/wins/fees/netPnl/敞口归零）。
- `cargo build --release`（默认特性）与 `cargo build --release --features strategy-loading` 均通过；
  特性下 `cargo test --lib` 110 项通过。
- `npm run typecheck` / `npm test`（135 项）/ `npm run build` / `scripts/secret-scan.sh` 全通过。

### 默认安全
- 无 `--strategy-limit`、无动态库 → 生产行为与改动前一致。
- 动态库策略**注册即禁用**，需显式 `strategy.enable`；Live 未启用、未改任何凭证。

### 生效说明（已重启验证）
生产 dry 内核已于 2026-09-14 18:53 重启（本地门禁 + PR #9 CI 全绿后，按 AI_WORKFLOW §2.1 第 8 条授权）：

- 新进程 pid 36014（`--mode dry`）；`core.ready` → `mode: "Dry"`，`version: 0.1.0`。
- `engine.stats` 已出现 **`strategies[]`**：`[{name:"spread_arb", enabled:true, source:"builtin", 各项计数 0}]`
  与新增计数器 `strategyLimitRejected: 0`；重启后计数归零属预期（分账是会话级）。
- 重启后 feed 正常：25s 内 `tops=32170`、`books=88`、`spots=761`、`evaluations=615`，无新增 ERROR/WARN。
- 重启前状态快照：`data/backup-20260914-185256-prerestart-p1.1/`（positions/orders/trades/evolution/shadow/signals）。
- 未配置 `--strategy-limit`、未加载任何动态库 → 生产行为与改动前一致；Live 未启用。

## 35. 【P-1.2/P-1.3 回测地基】事件驱动回测器 + 数据抽象（归档/重放）+ 真实数据暴露的两个缺陷修复

### 背景
`ROADMAP_INSTITUTIONAL.md` §5 P-1 交付物 2/3：**没有回测，任何策略/参数改动都不可验证**。
目标是"回测与 live 共用同一 feed 接口"，验收是**同一策略在 live 与回测上对同一历史区间给出一致 PnL**。

### 实现
- **`data_source.rs`（P-1.3）**：`DataSource`/`DataSink` trait、`TimedEvent`、`SourceStats`；
  JSONL 事件归档（Decimal 一律字符串精确编码：`{"at":<ms>,"k":"book|top|spot|round",...}`）；
  `EventArchive::open/record_at/flush_if_due`（到上限**丢弃新事件、绝不删除已有行**）；
  `ReplaySource` 流式读取：跳过畸形行并计数（`malformedLines`），时间戳回退只计数（`outOfOrderEvents`）并钳制时钟。
- **`backtest.rs`（P-1.2）**：`Backtester` trait（`run`/`describe`）、`BacktestConfig{core,tick_ms,tail_ms}`、
  `VecSource`（内存源，测试/合成用）、`EventBacktester`——持有**真实 `Core`**，强制 `Dry`：无 trade/order/position
  落盘、无 near-miss、无 discovery、无 feed-ws、无 shadow；`BacktestReport` 可序列化 + `render()` 文本报告。
- **`sim.rs`**：`FillModel{taker_slippage_ticks, maker_latency_ms, maker_fill_prob_bps}`，默认**恒等**（0/0/10000）
  → 重放与 live 逐位可比；`apply_slippage`（买上浮/卖下压，钳制 0.01–0.99）、`maker_eligible_at_ms`、
  `maker_fill_wins`（按订单 id 的确定性 FNV-1a 抽签，可复现）。
- **`service.rs`**：单一引擎装配入口 `CoreConfig::engine_config()`/`install_engine()`——**live server 与回测器共用**
  （杜绝两套参数映射漂移）；`engine_on_data` 在**任何消费者之前**归档原始事件；`tick()` 按 1s 触发归档 flush；
  `engine.stats` 新增 `archive{path,events,bytes,dropped,recording}`；新增 `engine_stats_at(as_of_ms)`。
- **CLI**：`--event-archive`、`--event-archive-max-mb`（默认 512）、`--entry-maker-timeout-ms`、
  `--backtest <archive>`、`--backtest-report`、`--backtest-tick-ms`（默认 50）、`--backtest-tail-ms`、
  `--slippage-ticks`、`--latency-ms`、`--fill-prob-bps`。`--backtest` 恒为 dry，且强制关闭归档。
- **IPC/Node**：新增 **`engine.book`**（L2 直送 `engine_on_data`，**不跑** dry 撮合——与 `--feed-ws` 同路径，
  也就是回测重放的路径；`books.snapshot` 保留 dry 撮合语义，差异见 D-11）+ 客户端 `engineBook()`。

### 真实数据暴露的两个缺陷（已修复 + 回归测试）
1. **维护节拍被事件密度绑架**（fidelity bug）：原 `run()` 只在"到下一事件的间隙 ≥ `tick_ms`"时推进维护周期，
   而真实 feed 是**亚毫秒级突发**（1 025 963 事件 / 780.5 s），13 分钟归档只跑了 **803** 个维护周期
   （live 同区间 **15 618**，`span/tick = 15 610`）。后果：出场检查（TP/SL/追踪/强平）在回测里粗了 ~19 倍
   → PnL 保真度受损；`blocked` 逐周期计数被饿死（28 vs 805）。
   **修复**：维护跑在**独立的 `tick_ms` 定时表**上（= live `ipc::server` 的 interval），与事件密度解耦；
   事件仍按自身时间戳投递、晚到事件钳制不回退。回归测试：
   `dense_stream_keeps_live_evaluation_cadence`（2 002 个 1 ms 间隔事件 → **40** 个周期，晚到事件不多买一拍）、
   `sparse_stream_evaluates_every_due_cycle`（10 000 ms → **200** 个周期，静市也要评估出场）。
2. **报告不可复现**：`confirmed`/`confirmedDetail` 源自 `HashSet`（迭代序每进程随机）→ 同参数两次回放报告不同；
   且诊断用**宿主时钟**取盘口新鲜度 → 离线回放里所有盘口"过期"、`mid` 显示为 0。
   **修复**：`engine_stats_at(as_of_ms)`（回测传**虚拟钟**）+ 两个诊断列表按 token 排序。
   现在同参数两次回放报告**逐字节相同**（`diff` 为空），且 `confirmedDetail` 与 live 快照逐值一致。

### 验证（离线，无网络/无凭证）
- **`scripts/backtest-check.mjs` → 21/21 PASS**：同一 `engine.book` 驱动两侧（采集 core vs 离线回放），
  事件数 20=20、无乱序、订单 3（2 成交 + 1 超时撤单）、平仓 1、净盈亏 **5.12208717 逐位相等**、
  分策略账本一致、回放强制 dry 且不写 trade/order/position 日志。
- **真实 feed 归档重放 → 19/19 PASS**（`--feed-ws` 采集 1 025 963 事件 / 780.5 s / 145.7 MB；临时 harness，
  未提交；归档留在 `$TMPDIR/blitzkrieg-real-*`）：
  - 归档**逐类行数 == 回放 feed 计数**（book 8 598 / top 982 502 / spot 34 860 / round 3）→ 重放零丢失；
  - live 快照 vs 回放：`books/spots/rounds/signals/placeRejected/strategyLimitRejected` 完全相等，
    `trades/orders/fills/net PnL/持仓/分策略账本`完全相等（该 13 分钟窗口无成交，故均为 0——PnL 一致性在此窗口
    是"零对零"，真正的**含成交逐位相等**由 `backtest-check.mjs` 覆盖）；
  - `evaluations` 15 610 = span/tick（live 15 618，宿主定时器相位/启动差 8 拍）；
    `blocked.momentum` **88 == 88**；`blocked.timing` 810 vs 805（0.6%，逐周期累计量在两个独立相位的定时器上
    天然 ±1/区间）；`confirmed` 集合与每 token 的 `mid/entry/cap/inBand` **逐值一致**；
  - 把 `--backtest-tick-ms` 减半（25 ms）→ 周期 31 220（正好 2×）、timing 1 617（≈2×810）、momentum 177（≈2×88）：
    证实计数差异是**节拍相位**而非状态分歧；两次同参数回放**逐字节相同**（决定性）。
  - **已知非对称（测量口径，非缺陷）**：live 的 `engine.stats` 快照比 SIGTERM 早 ~5 ms，因此比归档少
    6 个 top 事件（归档含这 6 个、回放全量消费）；归档内 **73 332** 个乱序到达（中位滞后 11 ms / p90 18 ms /
    p99 767 ms / 最大 8.6 s）是真实多流 feed 的结构属性（Binance spot + CLOB book/top 到达序抖动），
    重放**不重排、只钳制**，与 live "到达顺序即真实顺序"的语义一致。
- **归档速率实测** ≈11 MB/分钟 ≈16 GB/天 → 生产常开归档必须配 `--event-archive-max-mb` + 按天轮转（D-12 附注）。

### 测试与门禁
- Rust：`cargo test --workspace` **150 项**通过（core **131** = 基线 129 + 节拍回归 2；ui_kit 13；panel 2；polymarket 4），
  0 失败；`cargo build --release`（默认特性）与 `--features strategy-loading` 均通过（生产二进制恢复为**默认特性**构建）。
- Node/工具链：`npx tsc --noEmit` 干净；`npm test` **135/135**；`npm run build` OK；
  `scripts/secret-scan.sh` **OK: no secrets detected**；`parity-engines` **PARITY OK**；`core-parity` **RUST CORE PARITY OK**；
  `cycle-check` **PASS**；`backtest-check` **21/21**。

### 默认安全
- 不设 `--event-archive` / `--backtest` → 生产行为与改动前一致（归档与回测都是显式 opt-in）。
- `--backtest` 恒 dry、强制关闭归档与 discovery；`FillModel` 默认恒等 → 不改任何 live 决策。
- 未启用 Live、未改任何凭证；临时 harness 只在私有 socket + 临时目录里跑，不碰生产数据文件。

---

## 36. 【生产常开行情归档】分段轮转 + 磁盘护栏 + 多段重放（P-1.3 运维化）

**背景**：用户要求给生产 dry 内核**常开** `--event-archive`。直接加一个 flag 是不安全的——实测吞吐
≈11 MB/min ≈16 GB/天，原来的归档只支持"到顶即停"（`--event-archive-max-mb`），512 MB 上限下
**约 46 分钟就静默停录**，之后所有"这笔单为什么亏"的问题永久失去盘口证据。常开的前提是先让归档
**可以无限期跑下去**。

**实现**
1. **分段轮转**（`data_source.rs`）：`--event-archive-rotate-mb <N>` 到量即把当前段改名为带 UTC
   时间戳的同目录兄弟文件（`events.jsonl` → `events.20250914T140000Z.jsonl`），再打开新的
   `events.jsonl` 继续写。**只改名、绝不删除**；同一秒内多次轮转用零填充序号消歧
   （`-0002`…`-0010`，避免字典序把 `-10` 排到 `-2` 之前）。改名前先释放 fd，改名失败则原地继续写。
2. **磁盘护栏**（`--event-archive-min-free-mb <N>`，默认生产值 5120）：每次轮转点做一次 `statvfs`
   （热路径零成本），可用空间低于阈值即停录并记录原因——无限期采集的真正的界是磁盘，不是文件大小。
3. **停止原因可观测**：`engine.stats.archive` 增加 `rotateBytes/segmentBytes/segments/freeBytes/
   stoppedReason("cap"|"disk"|"io")`；`/crypto-hft status` 打印录制状态，停录时显示**加粗告警**。
4. **多段重放**：`SegmentSource` + `open_replay_all`——`--backtest events.jsonl` 现在自动按写入顺序
   读入该归档的**全部轮转段**（按名称解析时间戳排序，live 段排最后），无需人工拼接；源统计跨段汇总。
5. **生产接通**（`blitzkrieg-core-runner.ts` + `crypto-hft` skill）：**默认开**，路径
   `data/archive/events.jsonl`、每段 256 MB、无会话上限、保留 ≥5 GB 空闲；
   `HFT_EVENT_ARCHIVE=off`（或 `0`/`none`）关闭，`HFT_EVENT_ARCHIVE_{ROTATE,MAX,MIN_FREE}_MB` 可调。
6. **巡检**（`soak-health.sh`）：新增归档新鲜度检查——最新段 5 分钟无写入即报警，日志出现
   `event archive stopped recording` 即报警；健康行新增 `archive=ok(3s)|stale|off|no-segments`。

**验证**
- `cargo test --workspace`：**136** core + 13 ui_kit + 2 panel + 4 polymarket = 155 passed
  （新增 5 项：轮转不丢事件、UTC 命名、磁盘护栏、多段按序重放、异目录文件不误读 + 拼错路径报错）。
- CLI 实测（临时 socket + 临时目录，不碰生产）：20 001 个 book 事件 / 5 段 → `--backtest` 单段
  **20 001 events / 0 malformed / 0 out-of-order**，与捕获计数逐位一致。
- 过程中测试**逮到两个真缺陷**并修复：（a）同秒轮转撞名会**静默覆盖**前一段；（b）`-0002` 与
  `-0010` 字典序错排 → 重放乱序。两者都会"静默丢一段数据"，是常开归档的致命失效模式。
- 全门禁：`npm test` 135、`npx tsc --noEmit` 0 error、`npm run build` OK、`parity-engines` PARITY OK、
  `core-parity` RUST CORE PARITY OK、`cycle-check` PASS、`backtest-check` **21/21**、`secret-scan` OK。

**默认安全**
- 归档**只镜像行情事件**（book/top/spot/round），不写订单/成交/持仓，不读任何凭证。
- 不改任何交易决策：`FillModel` 恒等、无 live 行为变化；归档关闭时行为与改动前完全一致。
- 未启用 Live、未改凭证；`data/` 已在 `.gitignore`（归档不入版本库）。

---

## 37. 【归档默认收到内核】常开不再依赖外壳 + 单写者锁（P-1.3 收尾）

**背景**：§36 把"常开"做在了 **node 外壳**（`blitzkrieg-core-runner.ts`）里——归档 flag 由外壳
spawn 时拼进命令行。上线后发现这在两个场景下都会**静默失效**：

1. **外壳内存里没有新代码**：生产外壳（`node dist/index.js`）早于 §36 启动，`BlitzkriegCoreClient`
   的 `autoRestart` 复用**内存中的** `extraArgs`。此时 `SIGTERM` 内核只会用旧参数原样拉起——
   **重启无论多少次都不会让归档生效**。
2. **有别的外壳能拉起内核**：UI-kit 网关（`ui_kit` supervisor）也 spawn `blitzkrieg-core`，
   它根本不知道归档这回事。

结论：**"生产常开"这种属性属于内核，不属于某一个外壳**。只在外壳里实现的默认，等于"谁记得传参数谁才有"。

**实现**
1. **内核默认常开**（`main.rs`）：`--engine` 会话下归档默认打开（`data/archive/events.jsonl`，
   相对内核 cwd），无需任何 flag。`--no-event-archive` 显式关闭，显式 `--event-archive <path>`
   覆盖默认路径。解析集中在纯函数 `resolve_event_archive`，6 项单测覆盖：engine 默认开、非 engine
   不建空文件、显式路径在非 engine 下也生效、显式调参优先、**显式 `0` 不被默认值吃掉**、关闭优先。
   调参 flag 由 `u64` 改为 `Option<u64>`：`0` 是有意义的请求（无上限/不轮转/无护栏），只有**缺省**
   才回落到默认。
2. **单写者锁**（`data_source.rs`）：归档文件加排他 advisory 锁（`File::try_lock`）。两个内核指向
   同一归档时，第二个**干净停录**（`stoppedReason:"locked"`）而不是交错写行、更不会把文件从对方
   脚下轮转走；锁随 owner 释放，下一个写入者可正常接管。轮转的"改名→重开"之间也重新取锁，防止那个
   窗口里被人抢走。
3. **外壳改为显式关断**（`blitzkrieg-core-runner.ts`）：`eventArchive: null`（`HFT_EVENT_ARCHIVE=off`）
   现在**下发 `--no-event-archive`**——否则"省略 flag"会被内核默认重新打开，env 开关形同虚设。
4. **夹具显式关断**：所有 spawn `--engine` 的脚本（`core-parity`/`parity-engines`/`cycle-check`/
   `position-recovery-check`/`dry-observe`/`ui-kit-gateway-check`）加 `--no-event-archive`，
   防止未来的门禁把合成数据写进生产归档路径。（它们的 cwd 都是临时目录，实际早已隔离。）

**验证**
- `cargo test --workspace`：137 core（含 +1 锁测试）+ **6** main（+6 解析测试）+ 13 ui_kit + 2 panel = **158**。
- **跨进程实测**：两个**独立** `blitzkrieg-core` 进程、同 cwd（→ 同默认归档路径）——A `recording=true`，
  B `recording=false / stoppedReason="locked"`，无交错写行。
- **端到端实测**（临时 cwd，不碰生产）：只传 `--engine`（不带任何归档参数）→ `engine.stats.archive
  .recording=true`、`rotateBytes=268435456`、默认路径落盘；`--no-event-archive` → `archive=null`；
  `--event-archive data/archive/custom.jsonl` → 路径被覆盖。三项全过。
- 全门禁复跑：`npm test` 135、`tsc --noEmit` 0、`build` OK、`parity-engines` OK、`core-parity` OK、
  `cycle-check` PASS、`backtest-check` **21/21**、`position-recovery-check` PASS、
  `ui-kit-gateway-check` PASS、`secret-scan` OK。

**遗留**
- 本轮仍是"方案与实现"而非"已生效"：生效需要**内核重启**，而当前外壳内存里是旧代码（见背景 1），
  单纯 `SIGTERM` 内核会被旧参数拉起。故先合入 + 重建，再由运维择机重启外壳使新默认落地。

