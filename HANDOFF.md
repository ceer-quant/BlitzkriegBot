# HANDOFF — Blitzkrieg Core（项目交接/迁移说明）

> 生成于 2026-09-14，项目准备更换路径时。**项目根目录现为 `/Volumes/Hard Disk/BlitzkriegBot/`**
> （已与 CloddsBot 分家，不再有 `CloddsBot/` 子目录；`/Volumes/Hard Disk/BlitzkriegBot/CloddsBot`
> 已不存在）。新会话先读本文件 + `docs/blitzkrieg/MIGRATION_LOG.md`（§1–§29）+ `git log`，
> 即可恢复完整项目知识，**不依赖聊天历史**。

## 1. 这是什么

Polymarket 二元 UP/DOWN 市场的加密 HFT 机器人。架构（P0.6 后）：

```
Node/TS（UI、参数、日志）──IPC(UDS JSON-RPC)──▶ Rust 内核 blitzkrieg-core
                                                   │ 编译期链接
                                                   ▼
                                        extensions/polymarket（市场插件）
                                                   │ 依赖
                                                   ▼
                                        market_api（契约，无内部依赖）
```

**硬约束**：内核零市场代码；`grep -ri polymarket core/blitzkrieg_core/src` 仅剩 2 行 feature 注册；
扩展不得依赖内核（否则 Cargo 成环）。

## 2. 关键目录

| 路径 | 说明 |
|---|---|
| `core/blitzkrieg_core/` | Rust 内核（OME/风控/账本/持仓/引擎/scanner/IPC） |
| `core/market_api/` | 市场插件契约（DTO + DataFeed/MarketDiscovery/OrderExecutor/MarketHost） |
| `extensions/polymarket/` | Polymarket 插件（venue/live/feed/discovery/gamma/plugin） |
| `src/` | Node/TS 外壳（gateway、webchat、skills；`HFT_CORE=rust` 时只做 UI/参数/日志） |
| `ui/hft.html` | HFT 面板（**被 .gitignore 忽略**，仅存在于磁盘） |
| `ui/ui_kit/` | UI Kit（纯展示层 + gateway 命令通道；core/web/tui/app 四层，无交易逻辑） |
| `ui/ui_kit_panel/` | 交互式命令行面板（ratatui + crossterm + tokio，复用 UI Kit 数据层；`ui_kit_panel --manage` 可管内核生命周期） |
| `scripts/` | 验证/分析脚本（见 §5） |
| `docs/blitzkrieg/MIGRATION_LOG.md` | **权威变更史**（§1–§29），必读 |
| `data/` | 运行时数据（不提交，但随目录移动）：`trades/`、`orders/`、`positions/`、`soak/`、`shadow/`、`backup-*` |

## 3. 运行配置（当前生产）

`.env`（**含私钥，未提交，务必随目录一起保留**）关键项：
```
DRY_RUN=true          # 当前仍是模拟盘
HFT_CORE=rust
HFT_ROUND_SEC=900     # 15 分钟回合（若变 300 表示 5m 回退，须报警）
POLYMARKET_PRIVATE_KEY / POLYMARKET_API_* / POLYMARKET_FUNDER_ADDRESS
```

启动方式（从项目根目录）：
```bash
npm run build && node dist/index.js >> run.log 2>&1 &   # Node 外壳
# HFT 引擎通过 webchat 发 `/crypto-hft start` 启动（或面板按钮）
nohup node scripts/soak-monitor.mjs --hours 12 --interval-sec 300 >> data/soak/monitor.out 2>&1 &
```

内核起动参数（由 `blitzkrieg-core-runner.ts` 拼装，勿手改）：
`--engine --feed-ws --assets BTC,ETH,SOL,XRP --round-sec 900 --min-round-age 30
--min-time-left 180 --max-positions 2 --max-order-notional 6`
（可选 `--strategy-limit <name>:<maxOpen>:<maxNotional>` 由环境变量 `HFT_STRATEGY_LIMITS`
逗号分隔透传，P-1.1；不设置则无该参数、行为不变。）

## 4. 当前策略参数（`core/blitzkrieg_core/src/exit_policy.rs::ExitConfig::default()`）

| 参数 | 值 | 来历 |
|---|---|---|
| `stop_loss_pct` | **12** | 回测最优档；旧 50% 是最大亏损源（§26–§28） |
| `take_profit_pct` | **100** | 仅兜底；正常靠移动止盈 |
| `min_trail_pct` | **8** | 回吐下限 |
| `trailing_min_high_pct` | 15 | 移动止盈触发线 |
| 入场 | `trend_max_entry_price=0.45`、`trend_entry_factor=0.98`、趋势确认 60s/≥0.55 | §14 |

每笔股数由 `min_shares=max_shares` 固定（默认 10 股，单笔成本 ≈ $4.3–4.5）。可用
`--min-shares`/`--max-shares` CLI 覆盖，或经 Node 侧环境变量 `HFT_MIN_SHARES`/`HFT_MAX_SHARES`
（`/crypto-hft start` 读取并透传给内核）。小资金实盘（如 4.8u 持 2 仓）设 `HFT_MAX_SHARES=4`
即可把每笔降到 ~4 股（≈$1.8/笔）。

## 5. 验证/分析脚本（都用 `--no-trade-log` 隔离，不污染生产账本）

```bash
cargo test -p blitzkrieg-core --lib          # 95 项
node scripts/core-parity.mjs                 # 22 项订单/账本语义
node scripts/parity-engines.mjs              # Node↔Rust 决策等价
node scripts/cycle-check.mjs                 # 确认→挂单→成交→实时重估
node scripts/market-plugin-check.mjs         # 市场插件选择
node scripts/order-recovery-check.mjs        # 崩溃后订单恢复（孤儿防护）
node scripts/position-recovery-check.mjs     # 崩溃后持仓恢复（失管防护）
node scripts/core-adopt-check.mjs            # 重复客户端接管，无重启风暴
node scripts/analyze-strategy.mjs            # 交易/近失分析
node scripts/reconcile-exits.mjs             # **权威**出场模拟器（改参数前用它）
node scripts/final-exit-opt.mjs              # 出场参数全网格+稳健性
```

## 6. 已修复的关键 bug（详见 MIGRATION_LOG）

| # | 问题 | 章节 |
|---|---|---|
| 1 | 开仓即强平（到期时间用回合末） | §12 |
| 2 | 名义上限=sizeUsd 拒 100% 订单 | §13 |
| 3 | 持仓价格/盈亏冻结（books 未镜像） | §12/§16.2 |
| 4 | 面板卡片未接通（只读内存计数） | §17 |
| 5 | 盘口价格卡住（SDK 丢弃 price_change） | §18 |
| 6 | 二进制路径错位（workspace 输出） | §16.1 |
| 7 | 并发 start 竞态 + 崩溃重启自锁循环 | §16.3/§25 |
| 8 | 测试数据污染生产账本 | §21/§22 |
| 9 | **孤儿订单**（订单状态不持久化） | **§29** |
| 10 | **持仓失管**（持仓状态不持久化） | **§32** |
| 11 | **回测维护节拍被事件密度绑架**（真实数据暴露：803 vs 15 610 周期） | **§35** |
| 12 | **归档轮转同秒撞名静默覆盖前段** + 序号字典序错排致重放乱序（测试逮到，均会静默丢数据） | **§36** |
| 13 | **"常开默认"只做在外壳会静默失效**（外壳内存旧代码时 `autoRestart` 复用旧参数；UI-kit 网关不认） | **§37** |

## 7. 未完成 / 待办

- **每 2 小时 soak 健康巡检**（本会话既定目标，已配 ZCode 定时任务）：检查进程存活、`/health` 200、
  崩溃/孤儿告警、账本增长与净盈亏趋势。
- **P-1.1 多策略执行打通**：**已完成**（`MIGRATION_LOG §34`）——`engine.rs` 成为多策略宿主，
  订单/持仓带 `strategy` 标签，`engine.stats.strategies[]` 按策略分账，`--strategy-limit` 可选限额；
  用户策略（dylib）注册后默认禁用需显式 `strategy.enable`。行为等价已过 parity 硬门槛。
  已合并 main/develop（PR #9 → `ab6a13c`），并随 **2026-09-14 18:53 dry 内核重启**生效
  （新 pid 36014，`engine.stats.strategies[]` 已在线；证据见 `MIGRATION_LOG §34` 生效说明）。
- **P-1.2/1.3 事件驱动回测 + 数据抽象**：**已完成**（`MIGRATION_LOG §35`）——`--event-archive` 把
  引擎消费的每个行情事件镜像成 JSONL（Decimal 精确、到顶只停不删），`--backtest` 用**同一个 `Core`**
  在虚拟时钟上全链路重放；`FillModel` 提供滑点/延迟/成交概率（默认恒等 = 行为不变）；
  新增 IPC `engine.book` 把盘口直送 `engine_on_data`（与 `--feed-ws` 同路径、不跑 dry 撮合）。
  验收：`scripts/backtest-check.mjs` **21/21**（含成交的 live vs 回放**逐位相等**：净盈亏 5.12208717）；
  真实 feed 归档（1 025 963 事件 / 13 分钟 / 145.7 MB）重放 **19/19**：归档逐类行数 == 回放计数、
  `blocked.momentum` 88=88、`blocked.timing` 810 vs 805（0.6%，定时器相位）、`confirmed` 诊断逐值一致
  （该窗口无成交，故订单/PnL 为 0=0）；同参数两次回放报告**逐字节相同**。
  真机回放同时暴露并修掉两个**只影响回测保真度**的缺陷：维护节拍被事件密度绑架（803 → 15 610 周期）、
  报告不可复现（HashSet 迭代序 + 宿主时钟诊断）。**不改任何 live 行为**——生产 dry 内核（pid 36014）
  无需为此重启，下次常规重启即随二进制生效。
- **行情归档常开（P-1.3 运维化）**：**已完成**（`MIGRATION_LOG §36`、`§37`）——内核默认归档到
  `data/archive/events.jsonl`：每段 256 MB **轮转**（UTC 命名、只改名不删除）、无会话上限、
  可用空间 <5 GB 自动停录；`--backtest` 自动读入**全部轮转段**（无需人工拼接）。
  `HFT_EVENT_ARCHIVE=off` 关闭该功能；`HFT_EVENT_ARCHIVE_ROTATE_MB/MAX_MB/MIN_FREE_MB` 可调。
  巡检 `scripts/soak-health.sh` 新增 `archive=` 新鲜度（>5 分钟无写入报警）。
  **默认在内核侧**（`--engine` 会话无需 flag；`--no-event-archive` 关闭），因此 UI-kit 网关等
  其他外壳拉起的内核同样默认归档；归档文件加**单写者锁**，第二个写入者干净停录而非交错写。
  **需重启内核生效**（归档是启动参数，且外壳内存须先载入新代码）；只镜像行情事件，
  不碰凭证、不改任何交易决策。逐笔复盘：`--backtest data/archive/events.jsonl ...`（自动含历史段）。
- **实况复盘（2026-09-14 晚，8 笔 dry spread_arb）**：6W/2L、+$2.90；两笔 stop_loss 落袋
  −16.6%/−23.2% net（12% 触发线被 bid 在相邻观测间一步跳过，再加 3.2–3.5% 往返 taker 费），
  一笔 trailing_stop 峰值 +18.2% 回吐 13.7 点后仅 +1.07% net。机制复盘与待选项见
  `DECISIONS_PENDING.md` **D-12**；入场全部升级 taker 的成因见 **D-11**。
- **P0.7**：运行期 dylib 热加载（C-ABI vtable + 版本协商 + catch_unwind）——暂缓。
- **实盘未验证**：全程 DRY；live 链路（Poly1271 签名/授权/启动清算）**首次真实下单才能验证**。
  尤其 **live 启动孤儿扫单**只在 DRY 验证过，首次 live 启动须确认日志 `startup sweep cancelled N orphan order(s)`。
- **实盘资金门槛**：每笔 10 股 ≈ $4.5 → 1 仓需 ~$5、2 仓需 ~$10，建议 $12–15。若只给 4.8u，
  设 `HFT_MAX_SHARES=4`（或内核 `--max-shares 4`）把每笔降到 ~4 股（≈$1.8），两仓约需 $3.6。
  （CLI 已实现，见 MIGRATION_LOG §31。）
- **`OpenPosition` 持久化**：**已实现**（MIGRATION_LOG §32）——`position_db.rs` + `restore_positions()`，
  与 §29 订单恢复同类闭环。已随 2026-09-14 18:53 内核重启加载新二进制。
- **胜率 vs 盈亏比**：不可兼得（§28）。当前取舍：盈亏比优先（PF~2.9，WR~63–67%）。
- `ui/hft.html` 在 .gitignore 中（历史遗留），迁移后仍在磁盘。

## 7.1 迁移后清理（已完成，见 MIGRATION_LOG §30）

- 已删 `polymarket-5m-bot/`（仅剩 `.DS_Store`，无进程占用）。
- 已将游离的 `.env.other-project-backup-2025-11`（另一项目 Hummingbot 的真实凭据）
  移入废纸篓（可恢复）；`.gitignore` 规则保留。
- 订单库 `data/orders/orders.jsonl` 已在真实运行中确认落盘（`dry_1..dry_6`，含终态）。
## 8. ⚠️ 迁移（换路径）注意事项

1. **先 commit**（本次已做）——之前整个 Rust core 都未入库，风险极高。
2. **停进程**：`pkill -f 'node dist/index.js'; pkill -f blitzkrieg-core; pkill -f soak-monitor`
3. 移动整个 `CloddsBot/` 目录（含未提交的 `data/`、`ui/`、`.env`、`node_modules/`）。
4. **搬后必须 `cargo clean && cargo build --release`**：`target/` 内是绝对路径产物，不重编会错乱。
5. Node 若异常：`npm ci`。
6. 从**项目根**启动（二进制/相对数据路径靠 `process.cwd()` 解析）。
7. `.env` 含私钥，不要提交、不要丢。

### 会话记忆（ZCode）
- 对话历史存于 `~/.zcode/v2/tasks-index.sqlite`，**按 workspace 绝对路径归属**（`workspace_path` 列）。
- 换路径后：**历史不会消失，但不会自动出现在新路径**（新路径=新 workspace）。
- 接回方式：新会话中说「读取 `sess_b10620ee-2405-46ef-9fc3-bb630d8d3f0a` 的上下文」；
  或直接读本文件 + MIGRATION_LOG（更稳，不依赖会话库）。
