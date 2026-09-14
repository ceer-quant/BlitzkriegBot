# BlitzkriegBot 0.1 底座路线图

> 目标：**0.1 版本底座完成度高**（可自证、可回放、可扩展），为 0.3 开源打地基。
> 当前目标**仅为 0.1**——不在本路线图内的事项一律不做，只登记不执行。
> 本文件是 0.1 的唯一计划源；每一项工作都在 GitHub 上以 **Issue（需求/验收）+ PR（实现/证据）**
> 留痕，PR 必须引用 Issue 编号，Issue 关闭必须附证据（门禁输出/报告/迁移日志章节）。

---

## 0. 执行约定（可溯源）

| 环节 | 约定 |
| --- | --- |
| 需求 | 每个增量先开 Issue，写清「目标 / 范围 / 验收 / 风险 / 依赖」，贴 `priority/*`、`module/*`、`type/*` 标签 |
| 实现 | 功能分支（`feat/` `fix/` `chore/`）→ 全门禁绿灯 → PR（正文含 `Closes #N` 与证据）→ CI 全绿 → squash/merge |
| 证据 | 门禁原始输出、`docs/blitzkrieg/MIGRATION_LOG.md` 章节号、报告文件路径；「完成」必须可被第三方复跑验证 |
| 决策 | 拿不准的写入 `docs/DECISIONS_PENDING.md`，不阻塞主线、不臆断 |
| 硬约束 | 见 `docs/AI_WORKFLOW.md §2`：**不启用 Live**、不动真实凭证、不删未备份文件、不直推主分支、不顺手改业务逻辑 |

里程碑：GitHub Milestone **[v0.1 #1](https://github.com/ceer-quant/BlitzkriegBot/milestone/1)**。
所有 0.1 范围 Issue 都挂到该里程碑，下表为 Issue 索引。

| 史诗 | Issue | 子任务 |
| --- | --- | --- |
| E1 Clodds 清零 | [#15](https://github.com/ceer-quant/BlitzkriegBot/issues/15) | [#21](https://github.com/ceer-quant/BlitzkriegBot/issues/21) 裁决 · [#22](https://github.com/ceer-quant/BlitzkriegBot/issues/22) socket · [#23](https://github.com/ceer-quant/BlitzkriegBot/issues/23) 配置面 · [#24](https://github.com/ceer-quant/BlitzkriegBot/issues/24) 协议/化妆 · [#25](https://github.com/ceer-quant/BlitzkriegBot/issues/25) remote |
| E2 多策略底座 | [#16](https://github.com/ceer-quant/BlitzkriegBot/issues/16) | [#26](https://github.com/ceer-quant/BlitzkriegBot/issues/26) 按策略资金 · [#27](https://github.com/ceer-quant/BlitzkriegBot/issues/27) 门禁 opt-out · [#28](https://github.com/ceer-quant/BlitzkriegBot/issues/28) 进化按策略化 |
| E3 策略重构（HFT） | [#17](https://github.com/ceer-quant/BlitzkriegBot/issues/17) | [#29](https://github.com/ceer-quant/BlitzkriegBot/issues/29) 先设计后实现 |
| E4 对冲策略 | [#18](https://github.com/ceer-quant/BlitzkriegBot/issues/18) | [#30](https://github.com/ceer-quant/BlitzkriegBot/issues/30) 趋势 · [#31](https://github.com/ceer-quant/BlitzkriegBot/issues/31) 逆向 |
| E5 TUI + 插件管理器 | [#19](https://github.com/ceer-quant/BlitzkriegBot/issues/19) | [#32](https://github.com/ceer-quant/BlitzkriegBot/issues/32) 插件管理器 · [#33](https://github.com/ceer-quant/BlitzkriegBot/issues/33) 推送 · [#34](https://github.com/ceer-quant/BlitzkriegBot/issues/34) TUI 收敛 |
| E6 Tauri WebUI | [#20](https://github.com/ceer-quant/BlitzkriegBot/issues/20) | [#35](https://github.com/ceer-quant/BlitzkriegBot/issues/35) 脚手架 + 鉴权 |
| E7 策略接口全功能化 | [#38](https://github.com/ceer-quant/BlitzkriegBot/issues/38) | ABI v2：完整盘口 + 盘口回调 + 出场意图 + 热参 + 诊断 + 加载门禁 |

---

## 1. 范围总览

用户在 2026-09-14 追加的五项要求 + 盘点发现的一项前置依赖：

| # | 史诗 | 内容 | 依赖 |
| --- | --- | --- | --- |
| **E1** | Clodds 清零 | 彻底移除 cloddsbot 遗留，运行时零相关性 | — （最先做，否则新代码会继承旧命名） |
| **E2** | 多策略底座 | 按策略资金分配 + 门禁按策略 opt-out + 影子进化按策略化 | — （E3/E4 的前置） |
| **E3** | 策略重构（疯狗/HFT） | 用影子进化优化并**允许完全重构**现有主策略，真正吃高频特性 | E2 |
| **E4** | 对冲策略 | ≥1 个趋势策略 + ≥1 个逆向策略，与主策略对冲 | E2 |
| **E5** | TUI + 插件管理器 | 基于已有 `ui_kit`（Ratatui + Crossterm）做完整 TUI 与插件管理器 | E2（要管的就是策略/扩展/市场插件） |
| **E6** | Tauri WebUI | 用 Tauri 完成 WebUI | E5（复用同一份 view-model 与网关命令） |
| **E7** | 策略接口全功能化 | 策略接口全功能/全实现；策略必须能**外挂**（dylib）——「树内 / 外挂」只是加载方式不同，能力必须一致 | E2（按策略参数）；与 E3/E4 联动 |

**推荐执行序**：`E1 → E7 → E2 → E4 → E3 → E5 → E6`。
理由：E1 是确定性重命名，越晚做返工越多；**E7 提到 E2 之前**，因为它决定 E3/E4 写出来的策略
「长在哪个接口上」——先有全功能接口，E2 的分账/门禁/进化参数才有统一的挂载点，
否则 E4 的两个新策略会先写死在树内、E7 再改一遍。E2 是 E3/E4 的共同前置；E4 的两个策略逻辑比 E3 的主策略 HFT 化更直白，
先用它把「多策略真正并发 + 分账 + 门禁差异」这条链路跑通，E3 再在已验证的链路上重构主策略；UI 放最后，
因为 UI 只是视图，不该在底座还在动的时候定型。

---

## 2. E1 — Clodds 清零

**现状（盘点结论）**：约 260 个文件、905 行命中（不含 `dist/`；`dist/` 另有 221 个编译产物）。
绝大多数是文案/注释，但以下**承载运行时契约**，必须按「有迁移路径」处理而不是直接改名。

**进度**

| 子任务 | 状态 |
| --- | --- |
| [#22](https://github.com/ceer-quant/BlitzkriegBot/issues/22) E1-b socket 改名 + 兼容期 | ✅ 已交付（`MIGRATION_LOG §38`，PR #37）——`blitzkrieg-core-<user>.sock` 为规范名，旧名可发现并**领养**而非另起内核；回归 `scripts/socket-migration-check.mjs` |
| [#21](https://github.com/ceer-quant/BlitzkriegBot/issues/21) E1-a 加密盐 / 链上备注 | ⏸ 待裁决（`DECISIONS_PENDING` D-13） |
| [#23](https://github.com/ceer-quant/BlitzkriegBot/issues/23) E1-c `CLODDS_*` / `~/.clodds` | ✅ 已交付（`MIGRATION_LOG §39`）——`BLITZKRIEG_*` 为规范名，旧名保留一个发布周期的启动期镜像别名；路径裁决「规范存在用规范，仅旧存在则沿用旧」，零数据移动；36 个新测试 |
| [#24](https://github.com/ceer-quant/BlitzkriegBot/issues/24) E1-d 协议面 + 化妆项 | ⏳ 未开始 |
| [#25](https://github.com/ceer-quant/BlitzkriegBot/issues/25) E1-e `origin` remote | ⏸ 待裁决（`DECISIONS_PENDING` D-13） |

### 2.1 分级（决定改法）

| 级别 | 对象 | 处理方式 |
| --- | --- | --- |
| **A 危险·不可直接改** | 加密盐 `clodds-secrets-v1`（`src/security/index.ts:437`）；链上账本备注 `clodds:ledger:<hash>`（`src/ledger/anchor.ts`） | **不改**。改盐 = 既有密文永不可解；改备注 = 历史锚点无法校验。写入 Issue 并标注 `special/needs-decision`，等用户裁决是否接受数据迁移成本 |
| **B 跨语言契约** | UDS socket 名 `clodds-core-$USER.sock`（Rust `main.rs` / `ui_kit` / `ui_kit_panel` / TS client / 6 个脚本） | 一次性同步改 + 兼容期兜底（新名优先、旧名可发现）；改完必须重启相关进程 |
| **C 用户配置面** | `CLODDS_*` 环境变量（60+）、`~/.clodds` 状态目录、`clodds.json` / `clodds.db` / `.clodds.json`、`~/.config/clodds/mcp.json` | 新名为主，**旧名保留一个版本作为废弃别名**，启动时告警；状态目录支持「旧目录存在则继续用」或显式迁移命令 |
| **D 接线协议** | MCP 工具命名空间 `clodds_<skill>`、`clodds://session/<id>`、Copilot integration id、User-Agent 串、health `name` | 版本化变更；MCP 命名空间需同时接受新旧前缀一个版本 |
| **E 纯化妆** | 文档、i18n、HTML/公开文案、注释、技能 markdown、npm 包名、仓库字段、docker 服务名、度量/遥测前缀 | 可整批改，风险低 |
| **F 构建产物** | `dist/` 下 221 个文件 | 源码清零后重新构建即自然清零，**不手工编辑** |
| **G 仓库元数据** | git remote `origin` = `alsk1992/CloddsBot`（禁止推送，403） | 走 `git remote remove origin`（需用户确认，因为它是历史上游） |

### 2.2 验收

- `git grep -i clodds` 在**源码 + 配置 + 文档 + 脚本 + 测试**中为 0（`docs/blitzkrieg/MIGRATION_LOG.md`、
  `docs/reports/*` 等**历史记录例外**，见 2.3）。
- `npm run build` 后 `dist/` 无 clodds。
- 全部门禁绿灯；`ui_kit` / `ui_kit_panel` / Rust 内核 / TS 外壳四方对 socket、env、状态目录的新名一致。
- A 级两项若用户选择保留，则在 `DECISIONS_PENDING.md` 记录为**有意保留**并从本史诗验收中豁免。

### 2.3 例外（有意保留）

`MIGRATION_LOG.md`、`docs/reports/*GOVERNANCE*`、`CHANGELOG.md` 中的 clodds 是**迁移历史**，
删除等于篡改记录。约定：这些文件保留原文，并在文件顶部标注「历史记录，命名已废弃」。

---

## 3. E2 — 多策略底座（E3/E4 前置）

盘点发现三个硬阻塞，不解决则「三个策略并发」只是名义上的：

1. **无按策略资金分配**：`size_usd` / `min_shares` / `max_shares` / `max_positions` 全是全局单值
   （`engine.rs`），三策略共用 `max_positions=2` 会互相饿死。
2. **全局门禁会误杀逆向策略**：spot 动量过滤（`engine.rs`）在「现货正逆着持仓走」时拒绝下单——
   而这**正是**逆向策略要入场的情形；`min_round_age` / `min_time_left` 同理。需要按策略 opt-out。
3. **影子进化只能碰一个策略**：`MutableParams` 全局且被写死成 spread_arb 的四个旋钮，
   `Variant` 硬编码 spread_arb 入场逻辑，`UserStrategyAdapter` 忽略 hot params。
   → 需要「按策略的可变参数集 + 按策略评估 + 按策略审计」。

另需：策略注册表持久化与启用状态（promotion/canary 属 0.3，本阶段只做「可持久化的启用/参数」）。

### 3.1 验收

- 三个策略可同时启用，各自独立 sizing 与 `max_positions`，`engine.stats.strategies[]` 分账正确。
- 逆向策略可显式关闭动量门禁（配置项，默认关 = 行为不变）。
- 影子进化对**每个**启用策略独立评估、独立审计（`data/evolution/<strategy>.jsonl`），
  应用后热更新只影响该策略。
- 全部现有门禁保持绿灯；默认配置下行为与改动前**逐位一致**（无策略新增时）。

---

## 4. E3 — 策略重构：疯狗 / HFT 化

**现状（关键事实）**：名字叫 `crypto-hft`，但主策略 `spread_arb` 是**慢速做市抄底**——
60 秒趋势确认、900 秒回合、每 token 每回合一次入场、固定 10 股。**它没有高频特性**。
字面叫「疯狗」的 `dog_strategy.rs` 只是未接线的示例 dylib。

用户判断：**胜率不够高、盈亏比一般，允许完全重构**。

### 4.1 方向（Issue 内细化，需影子进化实证后才定稿）

- 明确「高频」在这类二元市场里的真实含义：亚回合级再入场、盘口队列/撤单节奏、
  多 token 联动、事件（现货突破）驱动的抢跑，而不是把 900 秒回合当 HFT。
- 影子进化从「4 个旋钮」扩到「结构可选」：至少能比较**入场族**（抄底 / 突破 / 事件驱动）
  与**出场族**（固定 TP-SL / 移动止盈 / 时间衰减），而不只是同族内微调。
- **验收必须用真实归档回放 + 冻结样本外**（`--backtest`），不是虚拟 PnL 自说自话。

### 4.2 验收

- 新策略/新参数在**样本外**回放上相对现状有可复现的改进（胜率 / 盈亏比 / 期望值，附报告）。
- 改动不触碰风控与出场安全边界（`ImmutableConfig` 仍不可变）。
- 影子进化全程有审计留痕；应用/回滚均可复现。
- 若无稳健改进，**如实报告「不改」**——不为了交付而调参。

---

## 5. E4 — 对冲策略（趋势 + 逆向）

至少两个新策略，与主策略构成对冲：

| 策略 | 方向 | 与主策略的关系 |
| --- | --- | --- |
| **趋势跟随** | 顺势追（突破/动量确认后入场） | 主策略抄底失效（单边下跌）时它是另一条腿 |
| **逆向/均值回归** | 逆势接（超跌反弹、假突破回落） | 与趋势策略天然对冲；需关闭动量门禁 |

要点（**2026-09-14 用户修正后**）：策略一律实现**全功能策略接口**，且**必须能外挂**（dylib）——
「外挂」与「树内」只是**加载方式**的差别，**不是能力**的差别（E7）。

原先本节写「两者都走内核原生 `EngineStrategy`、不走 C-ABI dylib（那条路无法表达出场、看不到多 tick 盘口）」，
该表述**只描述当时的 ABI v1 现状，不是设计目标**，已作废。用户裁定：策略接口必须全功能、全实现，
策略必须能外挂出去，这才是解耦/标准化——所以正确做法是**把 ABI 补成全保真**（E7），
而不是把策略都塞回树内。

TS 侧已有的 `momentum` / `mean_reversion` 只能作**逻辑参考**，不是可调用实现。

验收：各策略独立可启停、独立分账、有独立单元测试与回放证据；与主策略并发时不会互相饿死；
组合层面有明确的净敞口/相关性观测（最小实现即可）。

---

## 6. E5 — TUI + 插件管理器

**现状**：`Ratatui 0.30` + `Crossterm 0.29` **已在根 workspace**；`ui_kit_panel` 已是可交互 TUI
（3 个 tab：Overview / Positions / Trades，`:` 命令行、滚动日志、PTY 实测过）。
即这是**扩展**而非从零。

欠账：无插件管理器概念；`EventBus` 是死代码（UI 纯轮询，无推送）；
`ui_kit` 里另有一套手写 ANSI 的 TUI（与 `ui_kit_panel` 的 ratatui 实现重复）；
内核已有 `market.list` / `extension.list|enable|disable` / `strategy.list|enable|load` 等 IPC，
但 `IpcClient` 与 Dispatcher 一个都没暴露。

### 6.1 范围

- 插件管理器 tab：列出/启用/停用 **策略**、**市场插件**、**扩展**三类，状态可见（discovered/enabled/…）。
- 补齐 IPC 客户端与命令面（`orders.*`、`risk.*`、`strategy.*`、`extension.*`、`market.*`）。
- 实时推送：把 `EventBus` 接上内核事件流，替代纯轮询（保留轮询兜底）。
- 收敛两套 TUI 实现为一套（保留 ratatui），删除手写 ANSI 那套。
- 布局参考成熟项目（如 `btop` / `lazygit` 的分区+按键提示风格），写进 Issue 再实现。

验收：PTY 冒烟测试覆盖每个 tab 与每个插件操作；无核心时 UI 必须优雅降级而非崩溃；
`ui_kit` 保持**零 GUI 依赖**的契约（重依赖只进 `ui_kit_panel`）。

---

## 7. E6 — Tauri WebUI

**现状**：仓库内**零** Tauri 痕迹（无依赖、无 `src-tauri/`、无 `tauri.conf.json`）。
唯一接缝是 `ui_kit` 里有意的 headless `AppViewModel` + `render_headless()`——它被文档标注为
「native-app (Tauri/egui) 的接缝」；零依赖契约要求 Tauri 依赖**不得**进 `blitzkrieg_ui_kit`。

同时网关**完全没有鉴权**（`WebServer` 只绑 loopback，无 token/CORS/TLS），且只有 4 条 HTTP 路由、
无 WebSocket/推送、无静态资源服务。任何 webview 后端目前**无处认证**。

### 7.1 范围

- 新建 `src-tauri/`（或独立 crate）依赖 `blitzkrieg-ui-kit`；前端复用 E5 的 view-model，不重写数据层。
- 鉴权与传输：先做最小可信边界（绑定 loopback + 一次性 token + 明确 CORS），推送用 WS 或 SSE。
- 复用 E5 的插件管理器命令面，Web/TUI 命令语义一致。
- 打包与启动路径写文档；不引入需要真实凭证的步骤。

验收：`cargo tauri dev` 可起，能读快照、能下发只读命令与插件启停；鉴权有测试；
不把 Tauri 依赖带进 `blitzkrieg_ui_kit`；未启用 Live。

---

## 8. E7 — 策略接口全功能化 + 外挂标准化（Issue [#38](https://github.com/ceer-quant/BlitzkriegBot/issues/38)）

**用户裁定（2026-09-14）**：「策略接口必须是全功能、全实现的；策略必须外挂出去，这样才是解耦、标准化设计。」

**现状**：内核有**两套能力不对等**的策略接缝——树内 `EngineStrategy`（10 个方法，全功能）
与外部 C ABI v1（4 个方法，只收 `best_bid/ask/mid`，`Sell` 被丢弃，`on_book` 空实现，
且 `strategy-loading` 非默认 feature）。结果是「全功能」只能长在树内，外挂即残废——与裁定相反。

**范围（ABI v2）**：
1. `BkTick` → 完整市场视图：档位数组 + `bid_depth`/`ask_depth`/`obi`/`spread`/`spread_pct`。
2. vtable 补齐 `on_book`（每次盘口更新回调，而非每评估周期一次）与 `on_config`；
   诊断/热参用 JSON 字符串过界（不把 serde 类型泄漏进 ABI）。
3. 出场以**意图**过界：外挂策略可表达平仓请求，但报价/仓位/风控/下单/签名仍全部归内核。
4. 外挂策略声明**自己的**可进化旋钮集 → 对齐 E2-c 的按策略 `MutableParams`。
5. 树内策略与外挂策略实现**同一契约**；两个新对冲策略（E4）直接按此契约写。
6. ABI 版本协商与明确拒绝路径；`strategy-loading` 纳入常规构建，**至少一个真实 dylib 进 CI 被加载驱动**。

**安全边界（不可豁免）**：外挂策略拿不到凭证/订单管理器/UDS socket，也不能绕过
`RiskGate`/`ImmutableConfig`/kill switch。

验收见 Issue #38「验收」小节（核心一条：同一策略逻辑，树内实现与外挂 dylib 实现回放对拍逐信号一致）。

---

## 9. 不在 0.1 范围（登记不执行）

来自 `ROADMAP_INSTITUTIONAL.md §5` 的 P-2..P-6 大部分事项（组合级风险量化、资金效率与策略生命周期
的完整形态、密钥治理与合规审计、HA 与低延迟优化）**不在 0.1**。0.1 只做上表 E1–E7，
外加已有 P-1 系列的收尾（归档/回测已落地，见 `MIGRATION_LOG §35–§37`）。

**明确不做**：启用 Live、任何真实资金操作、把新策略直接放到生产启用而不经回放验证。
