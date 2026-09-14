# UI Kit 迁移报告 — Node 外壳职责盘点 + UI Kit 落地 + CloddsBot 删除评估

- **日期**：2026-09-14
- **模式**：全程 DRY / 只读展示层，**未触发 Live、未改动凭证**
- **新增代码**：`ui_kit/`（新 workspace crate，`blitzkrieg-ui-kit`）
- **证据**：`docs/reports/data/uikit/{tui_prod.txt, app_prod.txt, web_prod.html, web_prod.json, dryrun_order_chain.log}`
- **一句话结论**：UI Kit（core/web/tui/app 四层）**已实现并三前端跑通**（均读取真实内核数据）；但**`src/` 不能删除** —— 它仍是**正在运行的生产外壳**，UI Kit 目前只替换了「展示层」，尚未接管 gateway/命令派发，删除会中断生产。故按任务「不确定即记录」原则，删除动作**暂缓并写入待决策**。

---

## 1. 关键事实更正：`CloddsBot/` 目录已不存在

任务书 Step 4 要求「删除 CloddsBot 目录」。**核查结果**：

- `/Volumes/Hard Disk/BlitzkriegBot/CloddsBot/` **不存在** —— 更早的迁移（commit `fc6e93c`）已把项目根上移、扁平化，`CloddsBot` 这一层已消失。
- 今天仍存在的「Node 侧」是 **`src/`（584 个 .ts 文件）+ `dist/`（编译产物）**，即真正在跑的外壳。
- 当前生产进程正是 `node dist/index.js`（PID 72966，:18789）。

因此「删除 CloddsBot」的实际含义 = **删除 `src/` 这 584 文件的外壳**。这是一个比任务书设想大得多的动作，且与「禁止删除未经备份的文件、禁止破坏运行」直接冲突 → **不做，记录待决策（D-4）**。

---

## 2. Step 1：`src/` 职责盘点（含迁移状态）

图例：**HFT 角色** — (a) HFT 交易路径（Rust 已接管）／(b) UI 展示／(c) 聊天/Agent（与交易无关）／(d) 基础设施／(e) 其他区块链域。
**状态** — ✅ 已迁移 Rust／🟡 部分／❌ 未迁移／N/A 不在「删外壳」范围。

### 2.1 HFT 交易路径（Rust 已接管 → 属可删候选）

| 目录 | 职责 | 状态 | 依据 |
|---|---|---|---|
| `src/strategies/crypto-hft/` | Node HFT 引擎 6890 行（动量/均值回归/到期衰减/价差套利） | ❌ 已被 Rust `engine.rs`+`signal.rs`+`scanner.rs` 取代 | `HANDOFF.md` §2 |
| `src/strategies/hft-divergence/` | 背离套利策略 | ❌ 未迁移（Rust 无对应） | 同上 |
| `src/execution/` | Node 下单/智能路由/持仓管理/熔断/TWAP/DCA | ❌ 已被 Rust OME/ledger/risk/position 取代 | `MIGRATION_LOG §29`；`rust-clob-executor.ts` 已硬门控 |
| `src/feeds/crypto/`、`src/feeds/polymarket/` | Node 行情源 | 🟡 Rust `--feed-ws` 接管，Node 侧遗留 | `HANDOFF.md` §2 |
| `src/risk/` | Node 风控/特征熔断 | ❌ 已被 Rust `risk.rs` 取代 | — |
| `src/trading/` | bots/orchestrator/safety/adapters/copy-trading/backtest | ❌ Node 交易编排 | — |
| `src/history/`、`src/portfolio/`、`src/signal-router/`、`src/ml-pipeline/`、`src/opportunity/`、`src/arbitrage/` | Node 历史/持仓/信号路由/ML/机会/套利 | ❌ 均属 Node 交易域 | — |
| `src/bin/worker.ts` | BullMQ 执行 worker | ❌ 遗留 | — |

### 2.2 UI / 展示层（UI Kit 的目标替换对象）

| 目录 | 职责 | 状态 |
|---|---|---|
| `ui/hft.html`（737 行） | HFT 面板：`ws://…/chat` 发 `/crypto-hft status|positions|start|stop` | ✅ **UI Kit web 已可替代展示部分** |
| `public/webchat/` | 聊天 SPA（HFT 面板以 iframe 嵌入） | 🟡 展示可由 UI Kit 替代；命令派发仍需 gateway |
| `src/web/` | 内联 `CHAT_HTML` 的重复 HTTP 服务（无引用者，死代码） | N/A 死代码 |
| `src/tui/`、`src/terminal/` | Node 终端 UI（仅互相引用，无 gateway/CLI 引用者） | N/A 死代码 |
| `src/gateway/`（`server.ts`/`index.ts`） | Express+WS 网关：服务 `/webchat`、`/ui/*`、`/chat` WS、命令派发 | 🟡 **展示已可替代；网关+派发尚未由 UI Kit 接管** |

### 2.3 与交易无关（保留，不在删除范围）

`agents/`、`channels/`、`commands/`、`sessions/`、`db/`、`config/`、`utils/`、`types/`、`logging/`、`security/`、`alerts/`、`monitoring/`、`mcp/`、`memory/`、`cron/`、`automation/` 及绝大多数 (c)/(d)/(e) 目录（详见子代理盘点：区块链域 bankr/evm/solana/… 、聊天域 browser/canvas/voice/… ）—— **这些不属「删除 CloddsBot」范围**，是通用 Agent 平台能力。

### 2.4 盘点小结

- **仅 8 个目录**属「Rust 已接管的 HFT 交易路径」（§2.1），是真正可删候选。
- **未迁移**的是 Node 特有的次要策略（hft-divergence）与周边（ml-pipeline/arbitrage/opportunity）。
- **关键阻塞**：`src/gateway` 在模块顶层**静态 import** 了整张 Node 交易图（execution/trading/risk/feeds），
  即便 `HFT_CORE=rust` 这些 import 也必须能解析，否则进程起不来 → 删除需同步改造 `src/gateway/index.ts`。

---

## 3. Step 2：UI Kit 设计（已实现）

### 3.1 分层（严格按任务书布局）

```
ui_kit/
├── Cargo.toml                 # crate blitzkrieg-ui-kit（新 workspace 成员）
├── src/
│   ├── lib.rs                 # 契约说明 + 公共导出
│   ├── core/                  # ← 共享：数据模型 + IPC 客户端 + 事件总线
│   │   ├── types.rs           #   与内核 IPC 消息一一对应的 DTO（含 CoreEvent）
│   │   ├── ipc_client.rs      #   阻塞式 UDS JSON-RPC 2.0 客户端（零依赖）
│   │   └── event_bus.rs       #   多订阅者事件环形总线
│   ├── web/mod.rs             # ← Web 适配器：HTML/JSON 渲染 + 零依赖 HTTP 服务
│   ├── tui/mod.rs             # ← TUI 适配器：ANSI 渲染 + 轮询循环
│   ├── app/mod.rs             # ← 本地 App 适配器：view-model 边界 + 无头渲染
│   └── bin/{web,tui,app}.rs   #   三个可执行前端
```

### 3.2 设计原则（逐条落实）

| 原则 | 落实方式 |
|---|---|
| 只做数据渲染与事件订阅 | 全部方法只读；**不提供任何下单/撤单 API** |
| 不持有交易逻辑 | crate **不依赖** `blitzkrieg_core`；DTO 自持，故内核可自由替换 |
| 经 UDS JSON-RPC 通信 | `core/ipc_client.rs` 直连内核 socket，协议与 `ipc/schema.rs` 对齐 |
| 纯展示层、可替换 | 三适配器共享同一 `UiSnapshot`/`EventBus`，替换前端不动内核 |
| 三种前端共用一层 | `main` 渲染路径完全一致：`IpcClient::snapshot()` → 各适配器 `render*()` |

### 3.3 各前端职责

- **web**：`WebServer` 提供 `GET /`（自动刷新 HTML 面板）与 `GET /api/snapshot`（JSON）。
- **tui**：`render()` 输出 ANSI 屏幕；`run()` 轮询。支持 `--once` 快照模式。
- **app**：定义 `AppView`(render-ready view model) + `AppViewModel::refresh()` 与 `describe_event()`，
  为未来 Tauri/egui 提供**已冻结的数据边界**；`render_headless()` 在无 GUI 工具链下验证该边界。

### 3.4 编译与测试

- `cargo build --release` 整个 workspace 通过；`ui_kit` 4 个 bin 全部产出。
- `cargo test -p blitzkrieg-ui-kit` → **3 项事件总线单测通过**（独立游标 / 环形淘汰快进 / 原始通知解码）。
- 前端**零额外依赖**（仅 `serde`+`serde_json`），无 GUI/网络库，可无头验证。

---

## 4. Step 3：迁移剩余职责 — 现状与归属

| 职责 | 归属决策 | 状态 |
|---|---|---|
| 策略参数编排 | **UI Kit**（参数经 `/crypto-hft` 命令下发） | 🟡 展示已迁；命令下发仍走 Node skill |
| 日志渲染 | **UI Kit**（`web/tui/app` 渲染 trades/positions/events） | ✅ 已实现 |
| 命令解析 | UI Kit（未来）／当前 Node gateway | 🟡 未迁（见 D-4） |
| 任何交易逻辑 | **Rust 内核** | ✅ 内核本就独占（Node 已无交易决策） |

**结论**：剩余「未迁移」的都是**非交易逻辑**（网关、命令派发、聊天），属 Agent 平台能力；
按任务「无法确定归属内核还是 UI Kit 即记录、暂不迁移」→ 记入 D-4。

---

## 5. Step 4：删除评估（结论：**暂缓**）

| 前置条件（任务书） | 现状 |
|---|---|
| 先禁用 CloddsBot 入口、保留代码 | `CloddsBot/` 已不存在；禁用 `src/` 入口会**停掉生产进程**（当前 PID 72966 在跑），违反「禁止破坏运行」→ 不做 |
| 验证 Rust 内核 + UI Kit 完整替代 | ✅ 内核替代交易路径；✅ UI Kit 替代展示；❌ **网关/命令派发未由 UI Kit 接管** |
| 验证通过后删除目录 | **前置未全满足 → 不删** |
| 删除前完整备份 commit hash | 本次改动将独立提交（见 §7），可作为回滚点 |

**判定**：删除 `src/` 目前**不安全**，会中断（a）生产进程，（b）webchat 的 `/crypto-hft` 命令通道。
正确路径是**分阶段**（已写入 `docs/DECISIONS_PENDING.md` D-4）：

1. 冻结 `src/` 中 HFT 交易路径（§2.1）为只读、不再演进；
2. 让 UI Kit 增补**命令下发**（start/stop/status/positions）与最小网关；
3. 把 `ui/hft.html` 切到 UI Kit web；
4. 逐目录删除 §2.1 的 Node 交易域并 **每步跑 `cycle-check` 验证 DryRun 下单链路**；
5. 最后再评估 `src/gateway` 与聊天域是否保留。

---

## 6. 验收证据

- [x] `src/` 职责清单（含迁移状态）：本报告 §2（+ 完整逐目录表见随附子代理盘点）
- [x] 三前端跑通证据（均读**真实内核**）：
  - `docs/reports/data/uikit/tui_prod.txt` — 生产内核，Round #1988168 TRADING，50 笔、WR 70%、净 −0.73
  - `docs/reports/data/uikit/app_prod.txt` — 无头 view model，同一内核数据
  - `docs/reports/data/uikit/web_prod.html` + `web_prod.json` — 面板 5248 字节；JSON `trades.count=69, winRate=68.1%`
- [x] DryRun 下单链路仍正常：`docs/reports/data/uikit/dryrun_order_chain.log`
  （`[3] live orders: 1 → [4] open positions: 1 → RESULT: PASS`）
- [x] 全部门禁 PASS：core-parity 22 / parity-engines / cycle-check / order-recovery /
  **position-recovery** / market-plugin-check / core-adopt
- [x] 内核测试 98 项 + ui_kit 3 项，`cargo build --release` 通过，`npm run typecheck` 通过
- [x] 删除前备份点：见 §7 的 commit hash
- [x] 生产未被扰动：单实例内核、`/health` 200、DRY、未停止

---

## 7. Commit / 回滚说明

**执行前基线**：`fc6e93c`（clean tree 起始点）。分支：`rust-core-p0`（非 main）。

本次全托管执行产生 **4 个独立提交**（每个可单独回滚）：

| # | commit | 内容 |
|---|---|---|
| 1 | `80b4dc2` | `feat(core)`：股数 CLI（`--min-shares/--max-shares`）+ 持仓持久化 |
| 2 | `3f28e0d` | `test(shadow)`：影子进化确定性 A/B 复现器 + 报告 |
| 3 | `61067db` | `feat(ui-kit)`：UI Kit（core/web/tui/app）+ 迁移报告 |
| 4 | `b8fc023` | `docs`：机构级路线图 + 待决策清单 |

回滚方式：

```bash
# 回滚「某一个」提交（生成反向提交，保留历史）：
git revert <sha>
# 例如只回滚 UI Kit：git revert 61067db
# 或彻底回到执行前基线：
git reset --hard fc6e93c
```

**天然保护**：UI Kit 是独立 crate，回滚提交 3 只需从 workspace `members` 移除 `ui_kit` 并 `git revert 61067db`；
提交 1 与 2 分别只触碰内核安全层与一个 `examples/` 文件，互不依赖，可任意单独撤销。

> 说明：`docs/reports/data/**` 为实验原始数据与前端证据（约 36K、确定性可复现），
> 已通过 `.gitignore` 的 `!docs/reports/data/**` 例外**纳入版本控制**；运行时 `data/` 仍被忽略。

---

## 8. 结论与建议

1. **UI Kit 已可用**：四层结构落地，三前端均能读取真实内核并正确渲染，且不持有任何交易逻辑。
   它已经是 `ui/hft.html` 与 `src/tui` 的合法替代**展示层**。
2. **`src/` 暂不可删**：网关与命令派发未迁，且它是运行中的生产外壳。
3. **下一步最高优先级**：让 UI Kit 增补**命令下发 + 最小网关**（D-4 的第 2 步），
   这是解锁「删除 Node 交易域」的唯一前置。

---

## 9. 追加：命令下发 + 最小网关（D-4 第②步，已完成）

**状态**：✅ 完成。这是删除 Node 交易域的唯一前置。

### 9.1 新增模块 `ui_kit/src/gateway/`

| 文件 | 职责 |
|---|---|
| `gateway/supervisor.rs` | 内核**进程**生命周期：spawn / stop / **adopt**。含"绝不重复起核"守卫（socket 已被占用即接管而非重启，对齐 Node `BlitzkriegCoreClient::tryAdopt`）。仅对**自己 spawn** 的进程发信号；adopt 的进程 `stop()` 不杀。SIGTERM 经 `/bin/kill`（零依赖），超时升级 SIGKILL（持久化按状态变更落盘，SIGKILL 安全）。 |
| `gateway/command.rs` | 命令解析与派发：`start [ASSETS] [--size N] [--dry-run]`、`stop`、`status`、`positions [N]`、`help`。语义对齐 Node 的 Rust-core 路径（`src/skills/bundled/crypto-hft/index.ts::executeRust`）。**无任何下单动词**。 |

### 9.2 网关 HTTP 面（`ui_kit_web --manage`）

```
GET  /                     HTML 面板（含命令控制台；始终只读）
GET  /api/snapshot         JSON 快照
GET  /api/command?cmd=…    派发一条命令 → JSON
POST /api/command          body = 命令文本 或 {"cmd":"…"} → JSON
```

- 无 `--manage` 时命令面**只读**（`start/stop` 被拒并提示加 `--manage`）；`status/positions/help` 恒可用。
- 环境变量：`UIKIT_MANAGE=1` 同 `--manage`；`UIKIT_CORE_BIN`/`UIKIT_CORE_CWD`/`UIKIT_CORE_EXTRA_ARGS`
  用于测试隔离（钉二进制、隔离 `data/`、追加 `--no-*-log`）。

### 9.3 验收证据

- **端到端脚本** `scripts/ui-kit-gateway-check.mjs`（隔离 socket + 临时 workdir + 关闭三类日志）：
  `status(down)` → `start` → `status(managed pid, dry, round slot)` → `positions` → `start`(必须 **adopted**) →
  `stop` → `status(down)` → 未知动词拒绝 → `help`。**14 项全 ok，RESULT: PASS**。
- `cargo test -p blitzkrieg-ui-kit` → **13 项通过**（新增 supervisor/command/web 10 项）。
- 内核 98 项测试不受影响；`cargo build --release` 通过。
- **无下单 API**：`grep -rniE "orders\.place|place_order" ui_kit/src` 无命中；IPC 只用
  `ready/balance/round/stats/positions.list/orders.list/trades.history`（后三者皆读）。
- **生产未被扰动**：隔离 socket/workdir，测试期间生产内核（PID 73052）与账本照常。

### 9.4 尚未完成（留给第③步）

- UI Kit gateway 目前是 **HTTP 命令面**，尚未接入 `ui/hft.html` 现用的 `ws://…/chat` 通道——
  第③步「HFT 面板切到 UI Kit web」时一并处理。
- webchat 聊天/网关（`/chat`、命令派发）仍在 Node；它属 Agent 平台能力，按 §5 分阶段处理。

