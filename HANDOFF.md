# HANDOFF — BlitzkriegBot 项目交接入口

> **这份文件是接手的第一入口。** 按 §2 的阅读序读下去，即可在不依赖任何聊天历史的情况下
> 建立完整的项目认知。
>
> **最后核对：2026-09-17，`main` @ `4e93d6e`**（v0.2 的 E17 已落地，见 §3.1）。
> 若 `git log` 顶部已不是这个 hash，请以仓库现状为准，并在读完 §3 后更新本节。

---

## 1. 这是什么（30 秒版）

Polymarket 加密二元 UP/DOWN 市场的自动化交易系统。严格分层：

```
   Node.js 外壳 / Rust UI 面板          ← UI、参数编排、人类可读日志（不做交易决策）
              │  Unix Domain Socket + 换行分帧 JSON-RPC 2.0
              ▼
      blitzkrieg-core（Rust）           ← 撮合/下单/持仓/风控/账本/对账，单二进制
              │  编译期 feature 挂接（核心零市场代码）
              ▼
      extensions/polymarket             ← CLOB 下单 · 行情 · Gamma 轮盘发现
```

**两条不可逾越的架构约束**：

1. **内核零市场代码** —— `core/blitzkrieg_core` 不依赖任何交易所 SDK；
   `grep -ri polymarket core/blitzkrieg_core/src` 只应剩 feature 注册那几行。
2. **扩展不得依赖内核** —— 否则 Cargo 成环。扩展只依赖 `core/market_api` 契约 crate。

另有三条硬边界（详见 [`docs/DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §5）：
**账本语义**未经明确批准不得改；**风控硬边界**（RiskGate / kill switch / 单日亏损帽 /
全局容量 / 配额 / 定寸）**物理上不可被策略豁免**；**ABI vtable 已冻结**（`BK_ABI_VERSION = 2`），
新能力一律走**可选符号**，不得改 vtable。

---

## 2. 阅读序（按这个顺序读，别跳）

| 顺序 | 文档 | 读它是为了知道 |
| --- | --- | --- |
| 1 | **本文件** | 项目是什么、当前什么状态、怎么跑起来 |
| 2 | [`docs/FEATURES.md`](./docs/FEATURES.md) | **功能有哪些、做到什么程度**（证据等级：实盘验证 / 离线验证 / 仅测试覆盖 / 未验证 / 部分 / 规划） |
| 3 | [`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md) | **哪些已知问题还没修**：严重度、证据、影响面。**接手第一件该看的就是它** |
| 4 | [`docs/DEVELOPMENT.md`](./docs/DEVELOPMENT.md) | **怎么改代码、怎么验证**：门禁矩阵、不能碰的边界、本机工具链陷阱 |
| 5 | [`docs/GITHUB_GOVERNANCE.md`](./docs/GITHUB_GOVERNANCE.md) | **身份、分支、提交、Issue/PR、合并流程**（含 REST API 配方） |
| 6 | [`docs/AI_WORKFLOW.md`](./docs/AI_WORKFLOW.md) | 人机协作红线与已授权的长期规则（§2 硬约束、§2.1 第 8/9 条授权） |
| 7 | [`docs/DECISIONS_PENDING.md`](./docs/DECISIONS_PENDING.md) | **需人类拍板的分歧**：背景 / 选项 / 倾向 / **用户裁决**（D-1…D-19） |
| 8 | [`docs/blitzkrieg/MIGRATION_LOG.md`](./docs/blitzkrieg/MIGRATION_LOG.md) | **已修复缺陷的变更史**（§1–§48，1912 行）。查「这行为什么长这样」时读它 |
| 9 | [`docs/ROADMAP_V0_1.md`](./docs/ROADMAP_V0_1.md) | 0.1 里程碑 E1–E7 的验收与进度（**0.1 已完成**） |

**按角色**：

- **要跑起来看** → §3 → §4。
- **要改代码** → [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §2（目录职责）+ §3（门禁矩阵）。
- **要开 Issue/PR** → [`GITHUB_GOVERNANCE.md`](./docs/GITHUB_GOVERNANCE.md) §5 / §6。
- **要写策略** → [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §9 + [`docs/blitzkrieg/STRATEGY_GUIDE.md`](./docs/blitzkrieg/STRATEGY_GUIDE.md)。
- **要新增交易所** → [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §10 + [`docs/blitzkrieg/EXTENSION_GUIDE.md`](./docs/blitzkrieg/EXTENSION_GUIDE.md)。

> **全局文档地图在 [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §11**（四层分级，含「不要再建重复文档」的约定）。
> 仓库里还有大量早期文档（`docs/TRADING.md`、`docs/API.md` 等，合计 22k+ 行）——多数是迁移前的产物，
> **权威性低于上面这 9 份**。冲突时以上面 9 份为准。

---

## 3. 当前状态（2026-09-17 实测）

### 3.1 里程碑

| 里程碑 | 状态 |
| --- | --- |
| **v0.1 底座（E1–E7）** | ✅ **已完成**；GitHub Milestone *v0.1* 已关闭（22 个 Issue） |
| **E8（#57）Web 前端重构** | 🚧 进行中：Vue 3 + shadcn-vue 面板已上线运行；图表层与插件面对等性未全完成 |
| **E9（#59）产品化补完** | 🚧 进行中：E9-a/b/c/d 大部分已交付；**E9-h 延期 0.3**；**E9-d 的示例改写未达成**（KI-7） |
| **v0.2 RC-Hardening（E10–E17）** | 🚧 **已开工**。**E17 账户精度已落地**（`fe9c871` 实现 + `4e93d6e`/PR #89 补门禁，变更史 §48）；**E10–E16 尚未开始** |

**E17 的验收状态（诚实版，勿当成已关闭）**：`MakerThenTaker` 角色判定与现金流记账已修透，
夹具欠账 `0.0817722` → **0**，`account:parity` 69/69、dry↔live 逐位一致。
但**「72h DryRun 无漂移」这一项未达标**（只做了单次抽样冒烟），
且 **E17 没有修 KI-1**（dry 仍不跑穿越撮合）——那仍是一道 🔴 阻断级测量基准问题。

**开放的 Issue 只有 #57 与 #59 —— 都不要关。** 另有 3 个长期开放的 Dependabot PR（#1/#2/#3）。

### 3.2 正在运行的实例（⚠️ **不要杀**）

当前跑着一个 **DRY 实例**，由 Rust 面板托管：

| 角色 | PID | 说明 |
| --- | --- | --- |
| 面板 / 网关 | **90747** | `./target/release/ui_kit_web --socket …/blitzkrieg-core-fancer.sock --addr 127.0.0.1:51888 --manage` |
| 交易内核 | **16697** | 90747 的**子进程**（`--manage` 托管），`--mode dry`，已运行约 10 小时 |

```
http://127.0.0.1:51888/panel          → 200（登录页）
http://127.0.0.1:51888/api/snapshot   → 401（鉴权确实生效，不是坏的）
```

内核启动参数（实测，勿手改）：

```
--socket $TMPDIR/blitzkrieg-core-fancer.sock --mode dry --tick-ms 50
--seed-balance 1000 --max-order-notional 6.00
--assets BTC,ETH,SOL,XRP --round-sec 900 --min-round-age 30 --min-time-left 180
--max-positions 2 --min-shares 10 --max-shares 10 --engine --feed-ws
```

**注意**：Node 外壳（`node dist/index.js`）**当前没有运行**（`run.log` 最后一次
`Graceful shutdown complete` 是 2026-09-16 17:43）。现在扮演网关角色的是上面的
Rust 面板 `ui_kit_web --manage`。这是与早期文档（把 Node 外壳当生产入口）**最大的现实差异**。

### 3.3 运行数据

- `data/trades/trades.jsonl` —— **330 笔**成交记录；
- `data/archive/` —— 行情归档**常开**，每段 256 MB 轮转（UTC 命名、**只改名不删除**）；
  当前 3 个满段（各约 268 MB）+ 1 个在写段；
- `data/backup-*` —— 20 个历史备份目录（重启/清理前的快照），**不要删**。

### 3.4 治理现状（会让人意外，务必知道）

- 仓库 **private** + **GitHub Free 计划** ⇒ **服务端分支保护不可用**
  （API 返回 `403 Upgrade to GitHub Pro`；原生 Secret Scanning / Push Protection 亦不可用）。
  **「main 受保护」目前只是流程约定，不是平台强制。**（KI-17）
- `gh` CLI **未安装** ⇒ Issue / PR / 标签 / 合并全部走 **curl + GitHub REST API**。
- 远端名是 **`ceer`**（不是 `origin`；`origin` 已按用户裁决移除）。
- 本地 31 个分支 / 远端 44 个分支 / 14 个本地标签**从未推送**；合并后**不自动**删分支。

---

## 4. 跑起来（从零到看见行情）

```bash
# 1) 构建（工作区全量；产物固定在根 target/）
cargo build --release --workspace --locked

# 2) 起面板（自带托管内核：--manage 让它拉起并管住 dry 内核）
./target/release/ui_kit_web --addr 127.0.0.1:51888 --manage

# 3) 打开（用户名/密码来自 env，不是硬编码）
open http://127.0.0.1:51888/panel
```

面板鉴权凭据来自环境变量（见 `.env`，**该文件未入库、含真实机密，务必保留**）：
`BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD`。

其他入口：

```bash
cargo run -p blitzkrieg-ui-panel        # 终端面板（TUI，ratatui）
npm install && npm run build && npm start   # Node 外壳（早期入口，当前未在跑）
```

**常用开关**：

| 标志 | 含义 |
| --- | --- |
| `--engine` | 启用自驱动引擎 |
| `--feed-ws` | Rust 原生行情源（Polymarket 走 REST `POST /books` 轮询；Binance 现货仍为 WS） |
| `--mode dry\|live` | **`live` 未经明确授权不得使用** |
| `--shadow-evolution` | 启用影子进化（默认关闭；配套 `--se-min-samples` 等） |
| `--enable-strategy` / `--disable-strategy` | 启动期策略选择（可重复，后者优先；**新策略一律默认关闭**） |
| `--strategy-limit <name>:<maxOpen>:<maxNotional>` | 按策略限额（全局值仍为兜底与天花板） |
| `--backtest <archive.jsonl>` | 用**同一个 `Core`** 在虚拟时钟上全链路重放 |

---

## 5. 当前策略参数（改出场参数前必读）

权威来源是 `core/blitzkrieg_core/src/exit_policy.rs::ExitConfig::default()`：

| 参数 | 值 | 来历 |
| --- | --- | --- |
| `stop_loss_pct` | **12** | 回测最优档；旧的 50% 是最大亏损源（MIGRATION_LOG §26–§28） |
| `take_profit_pct` | **100** | 仅兜底；正常靠移动止盈 |
| `min_trail_pct` | **8** | 回吐下限（D-19 的 X2 自适应方案**未落地**，维持此全局值） |
| `trailing_min_high_pct` | 15 | 移动止盈触发线 |
| 入场 | `trend_max_entry_price=0.45`、`trend_entry_factor=0.98`、趋势确认 60s / ≥0.55 | §14 |

每笔股数由 `min_shares=max_shares` 固定（当前 **10 股**，单笔成本约 $4.3–4.5）。
可用 `--min-shares` / `--max-shares` 覆盖。小资金场景设 `--max-shares 4` 可把每笔降到约 4 股。

> ⚠️ **调参前先读 [`KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md) KI-1**：dry 行情路径不跑穿越撮合，
> 入场全部按 taker 计 1.7% 费。**在这个基准修好之前，用 dry 数据调出场参数得到的结论不可靠。**

---

## 6. 交接触手须知：五条最容易踩的

1. **不要在 `main` 上直接提交。** 走「分支 → 本地门禁 → PR → CI 全绿 → squash 合并」。
   由于没有服务端保护（§3.4），这条**只能靠自觉**。完整流程见
   [`GITHUB_GOVERNANCE.md`](./docs/GITHUB_GOVERNANCE.md) §4。
2. **不要杀 PID 90747 / 16697。** 那是用户正在看的实例。要重启只重启**内核**
   （`SIGTERM` 后 `autoRestart` 约 1 秒内拉起），且**仅限 dry** —— 授权见
   [`AI_WORKFLOW.md`](./docs/AI_WORKFLOW.md) §2.1 第 8 条（前置条件：门禁全绿）。
3. **不要用 Bash 写 Rust 源码。** 本机 Mimosa hook 会拦；用 Write/Edit 工具。
   它还会在**只读命令提到某些源码路径**、或**提交信息里出现受保护文件名**时误报 ——
   提交信息的绕法是：用 Write 写进临时文件，再 `git commit -F <file>`。
4. **不要声称「项目已通过安全审计」。** 上一次安全扫描结果是 `scanner_enobufs`（未完成）；
   在完整审计重跑成功前，任何安全声明都是不实的。措辞约定见
   [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §8.2。
5. **squash 合并后 `git branch -d` 会报「not fully merged」——这是正常现象，不是内容丢失。**
   正确判定：`git diff --name-only main <branch>`，输出为空才可 `git branch -D`。

**六条硬约束（违反即失败）**，完整版见 [`AI_WORKFLOW.md`](./docs/AI_WORKFLOW.md) §2：

> ① 禁止启用 Live 交易 ② 禁止修改真实凭证/私钥/API Key ③ 禁止删除未备份文件
> ④ 禁止未验证就提交主分支 ⑤ 禁止「顺手优化」业务逻辑 ⑥ 不确定项写入 `DECISIONS_PENDING.md` 而非自行拍板

---

## 7. 待办与未完成项（**交接重点**）

完整清单见 [`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md)（22 项，含严重度与证据）。摘要：

### 7.1 最先该处理的（顺序理由见 KNOWN_ISSUES §9）

| 编号 | 内容 | 为什么优先 |
| --- | --- | --- |
| **KI-9** | **22 个专项门禁只有 1 个接进 CI**（其余全靠人工本地跑） | 成本最低、收益最大；不修则所有回归保护都不完整 |
| **KI-1** | **dry 行情路径不跑穿越撮合**（挂单永不成交 → 入场全 taker，1.7% 费） | **测量基准是错的**，dry 的经济结论都建立其上 |
| **KI-7** | **`dog_strategy` 仍 327 行 / 45 处 `unsafe`**（E9 验收要求 ≤40 行） | E9 关闭的唯一硬项，也是策略 SDK 可用性的唯一实证 |

### 7.2 已裁决、待实现（口径已定，等排期）

| 编号 | 裁决内容 |
| --- | --- |
| KI-10（D-18） | 连亏熔断**按策略分片**；日亏上限与 kill switch **保持全局** —— 0.3 验收项 |
| KI-11（D-1） | 引入配置文件作为权威配置源，优先级由实现方定（建议 CLI > env > TOML） |
| KI-13（D-6） | 待核心开发告一段落后，做一次**独立**的 fmt/clippy 清理专项 PR，完成后 CI 转阻塞 |
| KI-14 + KI-15（D-4/D-9） | 旧 Node 交易域**不提前删**，等自有面板成熟后替换；86 个依赖漏洞随之下线 |
| KI-2 / KI-12（D-5） | 实盘小额验证：等策略重构与多策略之后再定 —— **须用户在场并授权，不得自行开启** |

### 7.3 有意**不做**的（勿当 bug 修）

加密盐 `clodds-secrets-v1`、链上备注 `clodds:ledger:<hash>`、已移除的 `origin` remote、
被 gitignore 的 `ui/hft.html` —— 均已用户裁决保留，理由见
[`KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md) §7。

**另有一处文档与现实矛盾**：`docs/AI_WORKFLOW.md:141` 称「仓库启用 Secret Scanning +
Push Protection」，与实测（GitHub Free 私有仓不可用）不符。改文档即可，不涉代码。

### 7.4 长期运行的注意

- `scripts/soak-health.sh` / `soak-health-loop.sh` / `soak-monitor.mjs` 是零 token 巡检脚本，
  但**当前未注册到 launchd/cron**（`~/Library/LaunchAgents` 下无对应项）。
  无人值守前应重新挂上。
- 归档常开且**只改名不删除**；磁盘可用空间 <5 GB 时自动停录。长期跑需关注空间。

---

## 8. 已修复缺陷速查（14 项；查「为什么这样写」用）

完整变更史在 [`docs/blitzkrieg/MIGRATION_LOG.md`](./docs/blitzkrieg/MIGRATION_LOG.md)（§1–§48）。

| # | 问题 | 章节 |
| --- | --- | --- |
| 1 | 开仓即强平（到期时间用了回合末） | §12 |
| 2 | 名义上限 = `sizeUsd`，拒掉 100% 订单 | §13 |
| 3 | 持仓价格/盈亏冻结（books 未镜像） | §12 / §16.2 |
| 4 | 面板卡片未接通（只读内存计数） | §17 |
| 5 | 盘口价格卡住（SDK 丢弃 `price_change`） | §18 |
| 6 | 二进制路径错位（workspace 输出位置） | §16.1 |
| 7 | 并发 start 竞态 + 崩溃重启自锁循环 | §16.3 / §25 |
| 8 | 测试数据污染生产账本 | §21 / §22 |
| 9 | **孤儿订单**（订单状态不持久化） | §29 |
| 10 | **持仓失管**（持仓状态不持久化） | §32 |
| 11 | **回测维护节拍被事件密度绑架** | §35 |
| 12 | **归档轮转同秒撞名静默覆盖** + 序号字典序错排致重放乱序 | §36 |
| 13 | 「常开默认」只做在外壳会静默失效 | §37 |
| 14 | socket 直接改名会分裂成两个内核 | §38 |

> **模式观察**：上表多个缺陷是**被真实运行逮到的，不是被测试逮到的**。
> 这正是 KI-6（缺系统性故障注入）被列为高风险的原因。

---

## 9. 面板与前端现状

| 面 | 位置 | 状态 |
| --- | --- | --- |
| **Web 面板（现役）** | `ui/webapp/webui/`（Vue 3 + shadcn-vue + Tailwind v4） | 5 页签：总览 / 行情面板 / 回放复盘 / 策略 / 插件 |
| **Rust 面板 / 网关** | `ui/ui_kit/`（bin `ui_kit_web`）、`ui/ui_kit_panel/`（TUI，ratatui） | 现役网关即 `ui_kit_web --manage` |
| **Tauri 桌面** | `ui/webapp/src-tauri/` | 脚手架在，**端到端未验证**（KI-4） |
| **旧单文件面板** | `ui/hft.html`、`ui/hft-dashboard.html` | **被 gitignore、从未入库**；本地遗留，E8 要替换的正是它 |

面板检查门禁（**bare-Node ESM**，`node --experimental-strip-types`）：

```bash
cd ui/webapp/webui
npm run check:all      # 全部套件
npm run check:round    # 单套（33 项断言）
```

八套断言数（2026-09-17 实测）：`theme` 11 · `balance` 15 · `lifecycle` 7 ·
`rejections` 10 · `feed` 12 · `session` 16 · `history` 13 · `round` 33。

---

## 10. 本机工具链陷阱（会浪费你几小时的那种）

完整清单见 [`DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §8；此处列最高频三条：

1. **嵌套 Cargo workspace**：`user_layer/strategies` 与 `user_layer/parity_strategy` 是**独立的
   workspace**（各有 `Cargo.lock`），**不是**根 workspace 成员。跑
   `BK_REQUIRE_DYLIB=1 cargo test` 前必须先各自构建，否则 dylib 相关测试会被静默跳过。
2. **CI 的 Node 是 22，本机是 26.8.2**。本机能过不代表 CI 能过。
3. **Mimosa hook** 的三类误报（见 §6 第 3 条）。

---

## 11. 安全红线速查

- 私钥 / API Key / 助记词 / `.env` **永不入库**；只允许 `.env.example`。
- 发现疑似泄漏 → **立即轮换凭证**，走私下披露（[`SECURITY.md`](./SECURITY.md)），**不要开公开 Issue**。
- **禁止启用 Live 交易**（未经明确授权）。
- **不得声称项目已通过安全审计**（见 §6 第 4 条）。

---

## 12. 会话记忆（ZCode）

对话历史存于 `~/.zcode/v2/tasks-index.sqlite`，**按 workspace 绝对路径归属**。

项目根目录现为 **`/Volumes/Hard Disk/BlitzkriegBot/`**（已与历史遗留的嵌套目录分家）。
换路径后历史不会消失，但**不会自动出现在新路径**（新路径 = 新 workspace）。

接回方式：新会话说明「读取 `sess_<id>` 的上下文」；
或直接读本文件 + §2 的阅读序（**更稳，不依赖会话库**）。

---

_维护约定：项目发生结构性变化（路径、里程碑、运行入口、权威文档集合）时更新本文件，
并同步更新 §顶部 的「最后核对」hash。_
