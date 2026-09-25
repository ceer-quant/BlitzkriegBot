# 版本管理体系（VERSIONING）

> 适用：BlitzkriegBot（Rust workspace + Lua 沙箱 + 系统级风控 + TUI/WebUI）
> 状态：**规范与实施设计**。本文是唯一权威来源；第 9 节是可直接排期的 Epic 拆分，第 10 节是验收标准。
> 基线：`0.2.0` 已发布（实盘链路已验证），本次目标是把开发分支收敛到规范的 `0.2.x` 补丁线。

---

## 0. 现状核对（逐项确认，不假设任何现有配置正确）

下表每一项都在本仓库上实际看过（命令与结果见「证据」列）。**结论为「需新建」的，是本方案必须做的改动；结论为「已具备」的，方案必须复用它而不是重造。**

| # | 待确认项 | 实际现状 | 证据 | 结论 |
|---|---|---|---|---|
| 0.1 | 根 `Cargo.toml` 是否有 `[workspace.package]` | **不存在**（0 命中） | `grep -c 'workspace.package' Cargo.toml` → `0` | 需新建（版本单事实来源的载体） |
| 0.2 | 成员 crate 的版本声明 | **9 个成员各自写死**，且互相不一致（`blitzkrieg-core`=0.2.0、`strategy-logic`=0.2.0、其余 0.1.0） | `grep -l '^version = ' core/*/Cargo.toml ui/*/Cargo.toml ...` → 9 | 全部改为 `version.workspace = true` |
| 0.3 | 现有 `build.rs` | 仅 2 个：`core/blitzkrieg_core/build.rs`（真做 provenance）、`ui/webapp/src-tauri/build.rs`（tauri-build，与本方案无关） | `find . -name build.rs -not -path '*/target/*'` | 复用并扩展 core 的注入逻辑，不另起一套 |
| 0.4 | 已有注入的环境变量名 | `BLITZKRIEG_GIT_SHA`、`BLITZKRIEG_GIT_DIRTY`（12 位短 sha，`nogit` 兜底） | `core/blitzkrieg_core/build.rs` | **保留这两个名字**，新增 `_VERSION` / `_BUILD_DATE` / `_TARGET` |
| 0.5 | 内核版本常量 | 已有：`pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");` | `core/blitzkrieg_core/src/lib.rs:56` | 复用，改为指向新的 `BUILD_INFO` |
| 0.6 | 运行时 provenance 模块 | 已有 `ipc/build_info.rs`：`GIT_SHA` / `GIT_DIRTY` / `version_string()`=`<semver>+g<sha>` / `provenance_line()`，含 3 个测试 | `core/blitzkrieg_core/src/ipc/build_info.rs` | 改为再导出，4 个调用点（`server.rs`、`main.rs:1838`、`main.rs:1912`、`data_lock.rs` 测试）不动 |
| 0.7 | **provenance 门禁的版本正则** | `/^\d+\.\d+\.\d+\+(?:g[0-9a-f]{4,40}|nogit)$/`——**不接受 `-rc.1` 之类的预发布后缀** | `scripts/lib/core-provenance.mjs` L31 | ⚠️ 冲突：一旦发 `0.2.1-rc.1`，门禁必红。发预发布前必须同改正则 |
| 0.8 | CLI `blitzkrieg version` | **不存在**；且启动器**完全没有** `--version` 分支 | `grep -c '\-\-version' ui/ui_kit_panel/src/bin/blitzkrieg.rs` → `0` | ⚠️ `blitzkrieg --version` 现在会落进 `run_unified` **启动整套交易栈**（#228 的同形缺陷，在启动器这侧仍未修） |
| 0.9 | 内核 `--version` | 已实现，且在 `parse_args` 之前短路 | `core/blitzkrieg_core/src/main.rs:1837` | 保留其单行格式 `<semver>+g<sha>`（门禁依赖） |
| 0.10 | IPC 方法表位置 | `core/blitzkrieg_core/src/ipc/schema.rs` 的 `pub mod method`；分发在 `server.rs:545` 的 `match method` | 已读 | 新方法加在这两处；**线上一律 camelCase** |
| 0.11 | IPC 现有版本字段 | `core.ready` 已返回 `version` / `commit` / `dirty` | `server.rs` READY 分支 | `system.version` 与之不冲突，职责更全（含更新状态） |
| 0.12 | 依赖可用性（HTTP/摘要/semver） | 锁里**已有** `reqwest 0.13.2`(rustls)、`sha2 0.10`、`semver 1.0.28`；但 **core / ui_kit / ui_kit_panel 都没有直接依赖它们** | `Cargo.lock`；三处 `Cargo.toml` | 新增直接依赖**不引入新依赖树**（复用同版本） |
| 0.13 | TUI 结构 | `enum Tab { Overview, Positions, Trades, Plugins, Evolution }`（5 个），无设置页 | `ui/ui_kit_panel/src/app.rs:10` | 需新增 `Settings` 变体 |
| 0.14 | **TUI/WebUI 对等门禁** | `TUI_TAB_SETS` / `WEBUI_TAB_SETS` / `WEBUI_SURPLUS` **硬编码已知布局集合**，出现未知集合即红 | `ui/webapp/webui/scripts/tui-parity.check.mjs` | ⚠️ 只加 Tab 不改门禁 = 门禁必红（这是设计使然，见 V6-4） |
| 0.15 | WebUI 结构 | Vue 3 + shadcn-vue（`components.json` 存在，new-york 风格）；`SettingsPage.vue` 已有 4 块；`components/ui/` 已有 card/badge/button/switch/tooltip/alert/segmented/stat/roll/empty/input | 已读目录与文件 | 版本卡片直接复用既有组件，**不新增 UI 依赖** |
| 0.16 | WebUI 导航 | 7 段：overview/hft/backtest/strategies/evolution/plugins/settings | `ui/webapp/webui/src/App.vue:67-73` | 不加新段，版本信息进 `settings` |
| 0.17 | 分支与 tag 现状 | **0 个 tag**（本地与远端都是）；本地 **118** 个分支；`fix/*` 49、`feat/*` 31 为主；`main` 与 `ceer/main` 为 `0 / 3`（main 严格落后，可 fast-forward） | `git tag -l`、`git for-each-ref`、`git rev-list --left-right --count main...ceer/main` | 需收敛；tag 体系从零建立 |
| 0.18 | 远端名 | 远端叫 **`ceer`**，`origin` 已移除 | `git remote -v` | 文档与脚本里的命令必须写 `ceer` |
| 0.19 | CI 触发与权限 | `on.push.branches: [main, develop]`，`pull_request` 全开；`permissions: contents: read`；**无 tag 触发、无发布任务** | `.github/workflows/ci.yml` | 新增 `release.yml`（tag 触发）+ 把 `release/*` 纳入 CI |
| 0.20 | 既有发布/安装先例 | `scripts/upgrade.sh`（238 行，stop→校验→安装→起栈→验证身份，含 `shasum -a 256` + `SHA256SUMS`）；`scripts/lib/upgrade-artifacts.sh`；shim 会拦截 `backup` 动词 | 已读 | 更新安装**复用**这套语义与 `Supervisor`，不另造一套 |
| 0.21 | 产物与体积预算 | `package-release.mjs`：必需 `blitzkrieg-core`，可选 `ui_kit_web`/`ui_kit_panel`/`ui_kit_app`/`blitzkrieg`；单二进制 ≤50 MB、仓库 ≤100 MB、交付物 ≤500 MB | `scripts/package-release.mjs`、`scripts/binary-size-check.mjs` | Release 资产命名与之对齐 |
| 0.22 | 数据与配置落点 | `data/`（已 gitignore）是运行时状态；`user_layer/configs/*.toml` 是工厂默认；`data/evolution/state.json` 是「运行时开关压过配置文件」的既有先例 | `.gitignore`、`upgrade.sh` 注释 | 更新开关沿用同一形态：`configs/update.toml`（工厂）+ `data/update/state.json`（运行时） |
| 0.23 | 配置优先级 | 已定型为 **CLI > env > TOML > 内置默认** | `core/blitzkrieg_core/src/config.rs` | 更新开关必须遵守同一优先级 |
| 0.24 | 嵌套 workspace | `ui/webapp/src-tauri`、`user_layer/strategies`、`user_layer/parity_strategy` 各有自己的 `[workspace]` 与 `Cargo.lock` | 根 `Cargo.toml` 的 `exclude` + 已读 | ⚠️ 它们**无法**继承根 `[workspace.package]`。策略/对齐件保持独立版本（这是它们存在的意义）；tauri shell 的版本纳入门禁同步检查 |
| 0.25 | CHANGELOG | 仅 `[Unreleased]` 与 `## [0.2.0] - 2026-09-17`；0.2.0 标题随「CloddsBot 快照导入」提交进入，**不存在可指认的 0.2.0 发布提交** | `grep -n '^## ' CHANGELOG.md`；`git log -S` | 不做「追溯打 tag 到猜测的提交」；新体系从 `v0.2.1` 起（见 V7-3） |
| 0.26 | 本地工具链 | `rustc 1.97.1`，`edition 2024` 可用；`date -u` 可用 | `rustc --version` | `2024` 成员不动 edition（不在本次范围） |

### 0.1 已实测验证的三条机制（本方案依赖它们，故先证后写）

在本机用一个隔离的空 workspace（`/tmp/bk-verify/ws`，不影响本仓库）实测：

1. **`[workspace.package].version` + `version.workspace = true` 确实继承**：三个成员 `cargo metadata` 全部解析为 `0.2.1`。
2. **`cargo::rustc-env=...` 注入在 `env!()` 处可编译可读**：`BLITZKRIEG_VERSION` / `_GIT_HASH` / `_BUILD_DATE` / `_TARGET` 四个常量全部取到值。
3. **把注入逻辑做成一个 `publish = false` 的共享 crate、以 build-dependency 引入是可用的**（`cargo test` 通过，含日期换算的精确断言）。

结论：第 3 节给出的代码不是「看起来能编译」，而是同形状已编译通过。

### 0.2 与需求的五处偏离（先声明，避免执行时被当成 bug）

| 需求原文 | 本方案做法 | 理由 |
|---|---|---|
| 「每个 crate 的 `build.rs` 注入」 | **只让 `core/build_info` 一个 crate 注入**，其余 crate 通过依赖读取同一组常量 | 9 份 `build.rs` 会在一次干净构建里拉起 27 次 git 子进程，且 9 份实现有 9 次漂移机会；**同一组常量只有一个编译产物**时「不可能不一致」是结构性保证，而不是纪律保证。若日后确需按 crate 各自盖章，把第 3.4 节的实现整段贴进该 crate 的 `build.rs` 即可，接口不变 |
| `system.version` 响应字段写作 `git_hash`/`build_date`/`update_available` | 线上（JSON）用 **camelCase**：`gitHash`/`buildDate`/`updateAvailable` | 仓库既有契约全部如此（`core.ready` 返回 `startedAtMs`/`peerVerified`/`socketMode`）。Rust 结构体字段仍是 snake_case，由 `#[serde(rename_all = "camelCase")]` 转换；若坚持 snake_case，改一个 serde 属性即可，但要同时改 WebUI 侧类型 |
| 「启动时异步调用 GitHub Releases API」 | 机制在启动时挂载，但**由开关控制，默认关闭**；关闭时启动过程零外联 | 与「更新机制默认关闭」不矛盾且可验证（第 7.6 节的对抗性验收）。交易内核未经许可在启动时对 `api.github.com` 发请求，是必须由操作者点头的副作用 |
| 「原子替换当前二进制」 | 替换动作由**启动器**执行（起栈前，或用 `Supervisor` 停→换→起），内核永不替换自己的二进制 | 内核是持仓进程。让持仓进程改写正在执行的二进制是典型的自伤路径；启动器本来就拥有内核生命周期（`Supervisor::start/stop` 已存在），且 `scripts/upgrade.sh` 的既有语义就是「停→装→起」 |
| 「GPG 签名可选但推荐」 | 校验策略显式化：`require_signature` 默认 `false`（有签名则校验，缺 `gpg` 时**大声警告并继续**）；置 `true` 时「无法验证」= 拒绝安装 | 「有签名但验不了」在两个方向都有代价。默认值选可用性，把严格性交给操作者的一个布尔值，并且两种情形都在 UI 上写明，不做静默降级 |

---

## 1. 版本管理总体设计

### 1.1 一条数据流（只有一条）

```
        root Cargo.toml  [workspace.package].version = "0.2.1"
                        │  (唯一人工编辑的版本号)
                        ▼
        core/build_info  ── build.rs ──►  BLITZKRIEG_VERSION / _GIT_HASH / _GIT_DIRTY
        （唯一的注入点）                    _BUILD_DATE / _TARGET
                        │
                        ▼
              BUILD_INFO 常量（const，零成本）
                        │
      ┌─────────────────┼───────────────────────────────┐
      ▼                 ▼                               ▼
 blitzkrieg-core    blitzkrieg（启动器/TUI）         （WebUI 无直连）
 --version          blitzkrieg version [--json]            │
 +=<semver>+g<sha>  blitzkrieg version --core              │
      │                                                    │
      └────────── system.version（IPC，UDS JSON-RPC）◄──────┘
                          │
              ┌───────────┴───────────┐
              ▼                       ▼
        TUI 设置页              WebUI 设置页（shadcn-vue）
```

**规则：任何地方需要版本号，只有两个合法来源——`BUILD_INFO`（进程内）或 `system.version`（跨进程）。** 任何第三处出现字面版本号即为缺陷，门禁必红（V1-3）。

### 1.2 分层职责（谁做什么，谁绝不做什么）

| 层 | 做 | 绝不做 |
|---|---|---|
| 根 `Cargo.toml` | 声明版本号 | —— |
| `core/build_info` | 盖章 + 提供常量与 JSON 形状 | 不做网络、不读配置、不写文件 |
| 内核（`blitzkrieg-core`） | `--version`；`system.version`；**更新检查**与结果缓存；更新开关的持久化与审计 | **不下载、不替换任何二进制、不重启自己** |
| 启动器（`blitzkrieg`，`ui_kit_panel`） | `version` 子命令；**下载→校验→原子替换**；用 `Supervisor` 停/起内核；`update --install` | 不判定「有没有更新」（那由内核的检查结果回答），不在运行时替换自己正在执行的代码路径以外的东西 |
| TUI / WebUI | 显示 + 触发（读 `system.version`，写 `system.update.configure`） | 不自己 parse 版本、不自己发 HTTP、不自己装文件 |
| CI | tag 守卫、构建、打包、`SHA256SUMS`、可选 GPG | 不用 CI 的版本号覆盖仓库里的版本号 |

### 1.3 三条不变量（写进门禁，不写进口号）

1. **INV-1 唯一来源**：存在且仅存在一处人工编辑的版本号（根 `Cargo.toml`）。所有 workspace 成员的包版本由 `cargo metadata` 证明等于它。
2. **INV-2 三方一致**：`cargo metadata` 的版本 == `blitzkrieg version --json` 的 `version` == 运行中内核 `system.version` 的 `version`，且三者不等于对方的旧值（用 git hash 校验同源）。
3. **INV-3 默认静默**：更新检查与自动更新默认关闭；默认配置下启动一个栈**不产生任何出站连接**。

---

## 2. 版本号规范与分支模型

### 2.1 版本号格式

```
<major>.<minor>.<patch>[-<prerelease>]
```

- 必须能被 `semver 1.x` 解析；`prerelease` 只允许 `rc.N`（本项目约定）。
- 版本号**不带** `v`；tag **带** `v`。映射关系恒为 `tag = "v" + version`，由 V8-2 的守卫强制。
- **构建元数据不进入版本号**。`<semver>+g<sha>` 里的 `+g<sha>` 是 provenance 后缀，由 `build_info` 在运行时拼接，**不写进 `Cargo.toml`**（`Cargo.toml` 里写 `+` 会让 cargo 拒绝或静默丢弃，且会让 semver 比较失去意义）。

### 2.2 与版本哲学对齐

| minor 奇偶 | 语义 | 分支 | tag 示例 |
|---|---|---|---|
| 单数（0.1、0.3） | **功能线**：可引入新能力、可破坏 | 从 `main` 开 `feat/*`，归并到下一个 `release/<minor>` | `v0.3.0` |
| 双数（0.2、0.4） | **RC / 稳定线**：只允许修复与内务，**不接受新功能** | `release/0.2` 只接受 `fix/*`、`chore/*` | `v0.2.0`、`v0.2.1`… |
| patch | 稳定修复：同一条线上的递增 | `release/<minor>` | `v0.2.1`、`v0.2.2` |

当前路径（不发明历史）：

```
0.2.0  已发布（无 tag，见 0.25）  ← 历史
0.2.1  ← 新体系下第一个 tag：本补丁线
0.2.2  ← 继续打补丁
0.3.0  ← 下一条功能线，从 main 开 feat/*
```

### 2.3 预发布（RC）

- 格式：`0.3.0-rc.1`、`0.3.0-rc.2`；tag：`v0.3.0-rc.1`。
- semver 排序天然正确：`0.2.9 < 0.3.0-rc.1 < 0.3.0`。
- ⚠️ **发 RC 之前必须先改 `scripts/lib/core-provenance.mjs` 的 `VERSION_RE`**（现状不接受后缀，见 0.7），否则所有驱动二进制版本的门禁会集体变红。这是 V8-5 的必做项。
- RC 的 tag 只从 `main` 的 RC 分支打，不从 `release/*` 打（`release/*` 的职责是补丁稳定）。

### 2.4 分支模型

```
main ──────────●────────────────●──────────────●─────────► 只接受 release/* 与 rc/* 的合并
               │                ▲              ▲
               │                │              │
release/0.2 ───┴──●────●────●───┘              │           只接受 fix/* 与 chore/*
                  ▲    ▲    ▲                  │
                  │    │    │                  │
              fix/*  fix/*  chore/*            rc/* 或 release/*
               （PR，全绿后合并）                （下一条线的收口）
```

规则（可执行化，见 V7）：

- `main` 的保护规则：**仅允许** `release/*`、`rc/*` 通过 PR 合并；禁止直推；禁止 force push；必须全绿。
- `release/0.2` 的保护规则：仅允许 `fix/*`、`chore/*` 通过 PR 合并。
- 每条补丁线一个长期分支，命名 `release/<major>.<minor>`；**一条线只对应一个**，不允许 `release/0.2` 与 `release/0.2-fix` 并存。
- 短期分支只允许 `feat/*`、`fix/*`、`chore/*`、`docs/*`、`rc/*` 五种前缀。现行仓库里 `tmp-*`、`wip/*`、`trace/*`、`panel-*`、`e9-*` 等前缀一律为不合规，收敛方式见 V7-1（**不批量删除**）。
- 每个 patch 必须打 tag 且 tag 必须带注释（`git tag -a`），tag 消息首行 `BlitzkriegBot <version>`。

### 2.5 tag 规范

| 项 | 规范 |
|---|---|
| 形式 | `v<major>.<minor>.<patch>[-rc.N]` |
| 类型 | 附注 tag（`-a`），必须带签名可选项（`-s`，有 GPG 时推荐） |
| 指向 | **只指向 `main` 或 `release/*` 上的合并提交**，不指向 `feat/*`/`fix/*` |
| 保护 | tags 保护规则：`v*` 禁止删除与强制推送 |
| 触发 | 推送 tag = 触发发布流水线（第 8 节）。**同一 tag 重复推送不允许重跑发布**（用 `concurrency` + 存在性检查） |

---

## 3. 单事实来源与 build.rs 注入方案（含 Rust 代码）

### 3.1 第一步：根 `Cargo.toml` 成为唯一来源

**改动 1**——新增 `[workspace.package]`（现有根清单**没有**这一节，见 0.1）：

```toml
# 根 Cargo.toml
# ── 版本单事实来源（VERSIONING.md §3）────────────────────────────────────────
# 这是全仓库唯一人工编辑的版本号。任何 crate 都不得再写死 version。
# 语义化 `<major>.<minor>.<patch>[-rc.N]`；tag 形态为 v<version>，由 CI 守卫。
# 注意：构建元数据（+g<sha>）不写在这里 —— 它由 build.rs 在运行时拼接。
[workspace.package]
version = "0.2.1"
```

**改动 2**——8 个成员从继承取版本（逐个文件，一个都不许漏）。现状（0.2）与目标：

| 文件 | 现状 | 目标 |
|---|---|---|
| `core/market_api/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |
| `core/blitzkrieg_core/Cargo.toml` | `version = "0.2.0"` | `version.workspace = true` |
| `extensions/polymarket/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |
| `user_layer/strategy_api/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |
| `user_layer/strategy_logic/Cargo.toml` | `version = "0.2.0"` | `version.workspace = true` |
| `user_layer/parity_logic/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |
| `ui/ui_kit/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |
| `ui/ui_kit_panel/Cargo.toml` | `version = "0.1.0"` | `version.workspace = true` |

> `ui/webapp/src-tauri/Cargo.toml`（`0.1.0`）与 `user_layer/strategies`、`user_layer/parity_strategy` 是**独立嵌套 workspace**（0.24），cargo 层面无法继承根 `[workspace.package]`。策略 cdylib 保持自己的版本是对的（它们模拟外部策略作者的项目）；tauri shell 的版本由 V8-6 的同步门禁盯着，一旦要随主线发版再改为读取环境变量注入。
>
> 另注：`patch` 位从 `0.2.0` 直接继承，8 个 crate 的版本会**同时**变成 `0.2.1`。这是有意为之——`0.1.0` 这类内部包号没有对外语义，统一成产品版本后，「哪个 crate 在哪个线上」不再需要单独记忆。

**改动 3**——新增两个 `publish = false` 的内部 crate：

```toml
# 根 Cargo.toml [workspace].members 追加（顺序放在最前，因为它们是构建基础设施）
members = [
    "core/build_support",   # 构建期注入逻辑（build-dependency 专用）
    "core/build_info",      # 运行期常量（BRAND: BUILD_INFO）
    "core/market_api",
    # ... 其余不变
]
```

### 3.2 `core/build_support`：注入逻辑只写一遍

**它存在的理由**：需求原话是「每个 crate 的 build.rs 注入」。逐字照做的话，9 个 `build.rs` × 3 条 git 命令 = 一次干净构建里 27 次子进程，而且 9 份实现有 9 次相互漂移的机会。把逻辑收进一个 crate 后，「所有 crate 的版本来自同一处」是编译期的结构性事实，不是靠纪律维持的约定。

```toml
# core/build_support/Cargo.toml
[package]
name = "blitzkrieg-build-support"
version = "0.1.0"          # ← 故意不继承：它是构建工具，不是产品的一部分
edition = "2021"
publish = false
description = "Build-time provenance stamping (cargo::rustc-env), used from build.rs."

[lib]
name = "build_support"
path = "src/lib.rs"
```

```rust
// core/build_support/src/lib.rs  （节选：注入规则的核心）
#![forbid(unsafe_code)]

use std::process::Command;

/// 在 `build.rs` 里调用。每个 `cargo::rustc-env` 都直接进编译期环境。
pub fn stamp() {
    // 输入变化必须触发重建，否则版本号会停在旧值上（这类 bug 很难发现）。
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=BLITZKRIEG_GIT_SHA");   // 保持兼容（0.4）
    println!("cargo::rerun-if-env-changed=BLITZKRIEG_BUILD_DATE");
    // 版本号本身来自根 Cargo.toml；`rerun-if-changed` 指向它，bump 才会生效。
    for p in ["../../Cargo.toml", "../../Cargo.lock"] {
        println!("cargo::rerun-if-changed={p}");
    }

    emit("BLITZKRIEG_VERSION", &version());
    emit("BLITZKRIEG_GIT_HASH", &git_hash());
    emit("BLITZKRIEG_GIT_DIRTY", if git_dirty() { "1" } else { "0" });
    emit("BLITZKRIEG_BUILD_DATE", &build_date());
    emit("BLITZKRIEG_TARGET", &target());

    // HEAD 每次 commit / checkout 都会变，refs 每次 fetch 都会变。
    for name in ["HEAD", "refs"] {
        if let Some(p) = git_path(name) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
}

fn emit(key: &str, value: &str) {
    println!("cargo::rustc-env={key}={value}");
}

/// cargo 已经把 `[workspace.package].version` 解析到 `CARGO_PKG_VERSION`。
/// 这里不查表、不读 TOML —— 读那个变量，就不可能和 cargo 的答案不一致。
pub fn version() -> String {
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into())
}

pub fn target() -> String {
    std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
}

/// 精确值优先（可复现打包者传 `BLITZKRIEG_BUILD_DATE`），否则读钟。
pub fn resolve_date(explicit: Option<String>) -> String {
    explicit.unwrap_or_else(utc_now)
}

pub fn build_date() -> String {
    resolve_date(non_empty_env("BLITZKRIEG_BUILD_DATE"))
}

/// 修订号：`BLITZKRIEG_GIT_SHA`（显式盖章）> `git rev-parse` > `nogit`。
/// 降级绝不是构建失败 —— 源码 tarball / 无 git 的 CI 镜像也必须能构建。
pub fn git_hash() -> String {
    env_sha().or_else(git_sha).unwrap_or_else(|| "nogit".to_string())
}

pub fn git_dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn env_sha() -> Option<String> {
    let t = non_empty_env("BLITZKRIEG_GIT_SHA")?;
    let head = t.strip_prefix('g').unwrap_or(&t);
    Some(head.chars().take(12).collect())         // 归一成 12 位，与 git 路径同形
}

fn git_sha() -> Option<String> {
    let out = git(&["rev-parse", "--short=12", "HEAD"])?;
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(pkg_dir()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pkg_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()))
}

/// 零依赖 `YYYYMMDDTHHMMSSZ`：`date -u` 可用就用它（精确、且是仓库 ops 脚本的既有口径），
/// 否则退化为纯日历换算 —— 一个时间戳不该需要外部二进制或一个 Cargo 依赖。
fn utc_now() -> String {
    #[cfg(unix)]
    if let Some(s) = Command::new("date").args(["-u", "+%Y%m%dT%H%M%SZ"]).output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.len() == 16)
    {
        return s;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    civil_from_unix(secs as i64)
}

/// days-since-epoch → 公历日期（Howard Hinnant 算法）。附测试，不附信任。
pub fn civil_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}
```

> 上面这段 `stamp()` / `civil_from_unix()` 的实现已在隔离 workspace 实测编译并通过测试（见 0.1），包括 `19700101T000000Z`、`20250921T103959Z`、`20000229T000000Z`、`20380119T031407Z`、`-1 → 19691231T235959Z` 五组精确断言。

### 3.3 `core/build_info`：运行期常量的唯一出口

```toml
# core/build_info/Cargo.toml
[package]
name = "blitzkrieg-build-info"
version.workspace = true          # ← 产品的一部分，继承工作区版本
edition = "2021"
publish = false
description = "The one runtime spelling of the build provenance stamped by build.rs."

[lib]
name = "blitzkrieg_build_info"
path = "src/lib.rs"

[build-dependencies]
blitzkrieg-build-support = { path = "../build_support" }
```

```rust
// core/build_info/build.rs  —— 全部内容就这一行
fn main() {
    build_support::stamp();
}
```

```rust
// core/build_info/src/lib.rs
//! 构建来源信息（运行时）。
//!
//! `build.rs` 盖章，这里是**唯一**的运行时拼写：`--version`、`blitzkrieg version`、
//! `system.version` 与启动横幅都从这里取值，因此它们不可能对「跑的是哪份代码」
//! 给出互相矛盾的答案（这正是 #172 / #179 要防的事）。
//!
//! 版本号硬编码检查：本文件的任何字面量版本号都是缺陷（V3-1 门禁）。

/// 完整构建信息。`const`，取值零成本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// `<major>.<minor>.<patch>[-rc.N]`，来自根 `[workspace.package].version`。
    pub version: &'static str,
    /// 12 位短 git 修订号，或 `nogit`（无 git 且未显式盖章）。
    pub git_hash: &'static str,
    /// 构建期是否跟踪到已修改文件（`"1"`/`"0"`）。
    pub git_dirty: &'static str,
    /// 构建时刻（UTC），形如 `20260925T073811Z`。
    pub build_date: &'static str,
    /// 编译目标三元组，形如 `aarch64-apple-darwin`。
    pub target: &'static str,
}

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: env!("BLITZKRIEG_VERSION"),
    git_hash: env!("BLITZKRIEG_GIT_HASH"),
    git_dirty: env!("BLITZKRIEG_GIT_DIRTY"),
    build_date: env!("BLITZKRIEG_BUILD_DATE"),
    target: env!("BLITZKRIEG_TARGET"),
};

impl BuildInfo {
    pub fn is_dirty(&self) -> bool {
        self.git_dirty == "1"
    }

    /// 构建无法指认自己的修订号（tarball、无 git 环境）。
    pub fn revision_unknown(&self) -> bool {
        self.git_hash == "nogit"
    }

    /// `g<sha>` / `nogit` —— 只属于修订号的那一半。
    pub fn revision_token(&self) -> String {
        if self.revision_unknown() {
            "nogit".to_string()
        } else {
            format!("g{}", self.git_hash)
        }
    }

    /// **门禁依赖的格式**：`<semver>+g<sha>` / `<semver>+nogit`，单行、无空格。
    /// `scripts/lib/core-provenance.mjs` 的 `VERSION_RE` 用它比对一个二进制的自述。
    /// 改这个格式 = 改门禁契约，必须同时改那个正则（V8-5）。
    pub fn version_string(&self) -> String {
        format!("{}+{}", self.version, self.revision_token())
    }

    /// 人类可读的一行：修订号 + 脏标记 + 版本。
    pub fn provenance_line(&self) -> String {
        format!(
            "version {} (git {}, {} build)",
            self.version_string(),
            self.git_hash,
            if self.is_dirty() { "dirty" } else { "clean" }
        )
    }

    /// `blitzkrieg version --json` 与 `system.version` 共用的 JSON 形状。
    /// serde 不介入：这个 crate 保持零依赖，任何进程都能安全链接它。
    ///
    /// 字段名 **camelCase**，与 IPC 线上契约一致（`schema.rs` 全库如此）。
    pub fn to_json(&self) -> String {
        let s = |v: &str| json_string(v);
        format!(
            "{{\"version\":{},\"gitHash\":{},\"gitDirty\":{},\"buildDate\":{},\"target\":{}}}",
            s(self.version),
            s(self.git_hash),
            self.is_dirty(),
            s(self.build_date),
            s(self.target),
        )
    }
}

/// 极小的 JSON 字符串转义：这些值全部来自构建环境（版本号、sha、三元组、
/// 时间戳），理论上不含控制字符；仍然转义而不是直接内插，因为一个来自
/// 环境的怪字符不该能让 JSON 解析器崩掉。
fn json_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_the_format_the_gate_parses() {
        let v = BUILD_INFO.version_string();
        let (semver, meta) = v.split_once('+').expect("must carry +<metadata>");
        assert_eq!(semver, BUILD_INFO.version);
        assert!(
            meta == "nogit" || (meta.starts_with('g') && (4..=40).contains(&(meta.len() - 1))),
            "unexpected revision token: {meta}"
        );
        // 与门禁正则逐字等价：不带后缀的 semver 才算合格，除非预发布（见 V8-5）。
        let ok = meta == "nogit"
            || meta[1..].chars().all(|c| c.is_ascii_hexdigit());
        assert!(ok, "revision token is not hex: {meta}");
    }

    #[test]
    fn the_stamp_is_never_a_placeholder() {
        // 「没有盖章」必须能被分辨出来：0.0.0 / 空值 / 空 target 都是失职。
        assert_ne!(BUILD_INFO.version, "0.0.0", "build.rs did not run");
        assert!(!BUILD_INFO.version.is_empty(), "empty version stamp");
        assert!(!BUILD_INFO.target.is_empty(), "empty target stamp");
        assert_eq!(BUILD_INFO.build_date.len(), 16, "{}", BUILD_INFO.build_date);
        assert!(BUILD_INFO.build_date.ends_with('Z'), "{}", BUILD_INFO.build_date);
    }

    #[test]
    fn the_json_shape_matches_the_ipc_contract() {
        let j = BUILD_INFO.to_json();
        for key in ["\"version\"", "\"gitHash\"", "\"gitDirty\"", "\"buildDate\"", "\"target\""] {
            assert!(j.contains(key), "{j} is missing {key}");
        }
        assert!(j.starts_with('{') && j.ends_with('}'), "{j}");
        // gitDirty 必须是布尔，不是字符串 —— WebUI 直接绑定它。
        assert!(j.contains("\"gitDirty\":true") || j.contains("\"gitDirty\":false"), "{j}");
    }

    #[test]
    fn a_missing_revision_is_named_not_faked() {
        let t = if BUILD_INFO.revision_unknown() {
            "nogit".to_string()
        } else {
            format!("g{}", BUILD_INFO.git_hash)
        };
        assert!(!t.is_empty());
        assert_ne!(t, "g");
    }
}
```

> **`git_dirty` 为什么单独一位而不是拼进版本串**：`<semver>+g<sha>` 要能在 CI 日志、本地构建、部署现场之间**逐字节**比较；`-dirty` 后缀会让每次比较都失败，于是门禁会开始「合理地」变红——这正是 #172 的成因。

### 3.4 接线：内核与启动器都从 `blitzkrieg-build-info` 取值

```toml
# core/blitzkrieg_core/Cargo.toml —— 新增一行
[dependencies]
blitzkrieg-build-info = { path = "../build_info" }

# ui/ui_kit_panel/Cargo.toml —— 启动器自己也要盖章（`blitzkrieg version` 不许依赖内核在跑）
[dependencies]
blitzkrieg-build-info = { path = "../../core/build_info" }
```

```rust
// core/blitzkrieg_core/src/lib.rs —— 改一处（第 56 行）
// 之前： pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub use blitzkrieg_build_info::BUILD_INFO;
pub const CORE_VERSION: &str = BUILD_INFO.version;
```

```rust
// core/blitzkrieg_core/src/ipc/build_info.rs —— 改成再导出层（保持 4 个调用点不变）
//! 构建来源信息（#179）。真正的盖章与格式化在 `blitzkrieg-build-info`；
//! 这个模块只负责让内核内部的既有名字继续可用，避免为了搬家而改动调用点。
pub use blitzkrieg_build_info::BUILD_INFO;

pub const GIT_SHA: &str = BUILD_INFO.git_hash;
pub const GIT_DIRTY: &str = BUILD_INFO.git_dirty;

pub fn is_dirty() -> bool { BUILD_INFO.is_dirty() }
pub fn revision_unknown() -> bool { BUILD_INFO.revision_unknown() }
pub fn revision_token() -> String { BUILD_INFO.revision_token() }
pub fn version_string() -> String { BUILD_INFO.version_string() }
pub fn provenance_line() -> String { BUILD_INFO.provenance_line() }
```

**接线后必须仍然通过的既有调用点（不动代码，只验证）**：

| 调用点 | 期望 |
|---|---|
| `core/blitzkrieg_core/src/ipc/server.rs` READY 分支（`build` / `commit` / `dirty`） | 输出不变 |
| `core/blitzkrieg_core/src/main.rs:1838`（`--version` 单行输出） | `<semver>+g<sha>` |
| `core/blitzkrieg_core/src/main.rs:1912`（启动横幅 `provenance_line()`） | 含版本、sha、dirty/clean |
| `core/blitzkrieg_core/src/data_lock.rs` 测试（`rec.version.starts_with(CORE_VERSION)`） | 仍通过 |
| `scripts/lib/core-provenance.mjs`（驱动二进制的门禁） | `VERSION_RE` 仍匹配 |

### 3.5 用命令验证「唯一来源」（这也是 V3-1 / V3-2 门禁的实现基础）

```bash
# ① 所有成员解析出的版本必须恰好等于根 [workspace.package].version
cargo metadata --no-deps --format-version 1 \
| python3 -c '
import json, sys
d = json.load(sys.stdin)
for p in sorted(d["packages"], key=lambda p: p["name"]):
    print(p["name"], p["version"])
'
# 期望：默认成员的版本全部为 0.2.1（build_support 例外，它是构建工具）

# ② 仓库里不许再有字面量版本号（退出码 1 表示「找到了」= 门禁红）
grep -rn '^version = "0\.' \
    core/*/Cargo.toml ui/*/Cargo.toml extensions/*/Cargo.toml \
    user_layer/strategy_api/Cargo.toml user_layer/strategy_logic/Cargo.toml \
    user_layer/parity_logic/Cargo.toml \
&& echo "FAIL: literal version still present" || echo "ok"

# ③ 二进制自述必须与仓库版本一致（须先 cargo build --release）
./target/release/blitzkrieg version --json \
| python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])'
# 期望：与 ① 相同
```

### 3.6 上游版本的同步点（容易漏，先写下来）

改了根 `[workspace.package].version` 之后，以下位置**不会**自动跟随，必须逐个确认：

| 位置 | 是否跟随 | 处置 |
|---|---|---|
| `Cargo.lock`（workspace 内部包条目） | 自动（cargo 会重写） | 无需手工，但 `--locked` 的 CI 会在有人忘提交 lock 时红——这是好事 |
| `ui/webapp/src-tauri/Cargo.toml`（`0.1.0`） | **不跟随**（独立 workspace） | V8-6 门禁比对 `tauri.conf.json` 与它；要随主线发版时再引入环境变量注入 |
| `ui/webapp/src-tauri/tauri.conf.json` 的 `"version"` | **不跟随** | 同上；tauri 打包用它，不修就会打出旧版本号的安装包 |
| `user_layer/strategies/spread_arb/Cargo.toml`（`0.2.0`） | 不跟随（独立 workspace，刻意为之） | 保持独立——它模拟外部策略作者的项目 |
| `CHANGELOG.md` | **不跟随** | V7-4 要求 bump 的同一次提交里写 changelog |
| `docs/` 里正文引用的版本号 | **不跟随** | 只在「当前版本」这类会腐烂的地方引用；历史叙述写具体版本号是允许的 |

---

## 4. `blitzkrieg version` 命令实现（含 Rust 代码）

### 4.1 命令表面

```
blitzkrieg version [--json] [--core] [--socket <path>]
```

| 层次 | 来源 | 是否需要内核在跑 | 解决的场景 |
|---|---|---|---|
| 默认 | 启动器自身的 `BUILD_INFO`（编译期常量） | 否 | 「我装的是哪一版」——永远可用，包括内核起不来的时候 |
| `--json` | 同上，结构化输出 | 否 | 脚本与门禁消费 |
| `--core` | 经 IPC 向 socket 上的内核询问 | 是 | 「**正在跑**的是哪一版」——与磁盘上的不是一回事 |

**为什么必须区分这两个问题**：仓库里已经有过一次代价高昂的教训（`scripts/lib/core-provenance.mjs` 的注释记录了它）——门禁脚本与它驱动的二进制来自不同代码状态，于是绿色的结论描述的是另一份代码。`blitzkrieg version` 回答「盘上是什么」，`blitzkrieg version --core` 回答「跑的是什么」，两者都要能单独问。

### 4.2 与既有 `--version` 的关系（先解决现状 0.8 的坑）

现状：`ui/ui_kit_panel/src/bin/blitzkrieg.rs` 的 `tokio_main()` 里**没有 `--version` 分支**，`Some(other) if other.starts_with('-')` 会把 `--version` 当作「无子命令的 FLAGS」交给 `run_unified()`，从而**启动整套交易栈**。内核侧（`main.rs:1837`）修过这个洞，启动器侧没修。

必须做的两件事：

1. 新增 `version` 子命令（本节）。
2. **同时**给启动器加 `--version` / `-V` / `-v` 短路（返回单行 `<semver>+g<sha>`，与内核格式一致、退出码 0）。这不是附赠功能：没有它，`blitzkrieg --version` 会拉起一个交易栈——正是 #228 记录过的同形风险。加上之后，`--version` 与 `version` 两个入口都安全，且 `--version` 的单行格式与内核一致，门禁与运维脚本可以统一解析。

### 4.3 实现（`ui/ui_kit_panel/src/bin/blitzkrieg.rs`）

**改动 A**——`tokio_main` 里加两个分支（放在 `help` 之后、`core` 之前；`version` 必须能在 tokio 之前完成，所以显式短路）：

```rust
#[tokio::main]
async fn tokio_main() -> std::process::ExitCode {
    let mut raw_args: Vec<String> = std::env::args().skip(1).collect();
    let first = raw_args.first().map(|s| s.as_str());

    match first {
        Some("help") | Some("--help") | Some("-h") => {
            println!("{HELP_TEXT}");
            std::process::ExitCode::SUCCESS
        }
        // 与内核 `--version` 同一格式、同一来源：`<semver>+g<sha>`，单行。
        // 在 tokio 之前短路，所以它在「内核起不来」的环境里也照样工作。
        Some("--version") | Some("-V") | Some("-v") => {
            println!("{}", blitzkrieg_build_info::BUILD_INFO.version_string());
            std::process::ExitCode::SUCCESS
        }
        // 新增：`blitzkrieg version [--json] [--core] [--socket <path>]`
        Some("version") => {
            raw_args.remove(0);
            run_version_subcommand(raw_args).await
        }
        Some("core") => { /* 不变 */
            raw_args.remove(0);
            report(run_core_subcommand(raw_args).await)
        }
        // ... 其余分支不变 ...
    }
}
```

**改动 B**——子命令本体与它的输出。注意三点：`--core` 走既有的 `IpcClient`（复用，不重造 socket 逻辑）；JSON 输出用 `BUILD_INFO::to_json()` 而不是手拼；退出码有语义（内核不可达是 1，参数错误是 2）。

```rust
/// `blitzkrieg version [--json] [--core] [--socket <path>]` —— 盘上装的是什么，
/// 或（`--core`）正在跑的是什么。
///
/// 默认回答「盘上」：读编译期盖章，不需要任何进程在跑。`--core` 才经 IPC 询问
/// 运行中的内核 —— 两个问题不同，答案也可能不同（这正是要能分别问的原因）。
async fn run_version_subcommand(args: Vec<String>) -> std::process::ExitCode {
    let mut json = false;
    let mut ask_core = false;
    let mut socket: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--core" => ask_core = true,
            "--socket" => match it.next() {
                Some(p) => socket = Some(p),
                None => {
                    eprintln!("blitzkrieg: version: --socket needs a path");
                    return std::process::ExitCode::from(2);
                }
            },
            "--help" | "-h" => {
                println!(
                    "blitzkrieg version [--json] [--core] [--socket <path>]\n\n\
                     默认：打印本二进制（磁盘上这一份）的版本与构建信息。\n\
                     --core：改问 socket 上正在运行的内核 —— 「跑的是哪一版」。\n\
                     --json：结构化输出，字段为 version/gitHash/gitDirty/buildDate/target，\n\
                     \x20        --core 时另含 updateAvailable/latestVersion（尚未检查则为 null）。"
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("blitzkrieg: version: unknown argument '{other}'");
                eprintln!("See 'blitzkrieg version --help'.");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let info = blitzkrieg_build_info::BUILD_INFO;

    if !ask_core {
        if json {
            println!("{}", info.to_json());
        } else {
            println!("BlitzkriegBot {}", info.version);
            println!("  git      {} ({})", info.git_hash, if info.is_dirty() { "dirty" } else { "clean" });
            println!("  built    {}", info.build_date);
            println!("  target   {}", info.target);
        }
        return std::process::ExitCode::SUCCESS;
    }

    // `--core`：与内核对话。socket 解析复用既有规则，不另发明一套。
    let socket = socket.unwrap_or_else(resolve_socket_path);
    let mut client = IpcClient::new(socket.clone());
    match client.system_version() {
        Ok(v) => {
            if json {
                // 内核已经把形状给全了；原样转发即为最忠实的答案。
                println!("{}", v.raw_json);
            } else {
                println!("BlitzkriegBot {} (running core)", v.version);
                println!("  git      {} ({})", v.git_hash, if v.git_dirty { "dirty" } else { "clean" });
                println!("  built    {}", v.build_date);
                println!("  target   {}", v.target);
                println!("  socket   {socket}");
                match (&v.update_available, &v.latest_version) {
                    (Some(true), Some(latest)) => {
                        println!("  update   {latest} available (auto-update: {})", v.auto_update)
                    }
                    (Some(false), _) => println!("  update   up to date"),
                    _ => println!("  update   not checked (更新检查默认关闭)"),
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            // 退出码 1 = 「问不到」，与参数错误（2）区分；脚本据此判断是
            // 「没装对」还是「没在跑」。
            eprintln!("blitzkrieg: version: no core on {socket}: {e}");
            eprintln!("  (只想知道磁盘上装的是哪一版，去掉 --core。)");
            std::process::ExitCode::FAILURE
        }
    }
}
```

**改动 C**——`HELP_TEXT` 里补一行（现状的帮助文本没有 version 子命令）：

```
  version        Print the build version / git hash / build date / target
                 (--json for scripts; --core to ask the running kernel)
```

### 4.4 `IpcClient::system_version()`（`ui/ui_kit/src/core/ipc_client.rs`）

```rust
/// `system.version` 的视图。字段与内核契约一一对应（camelCase 已在线上，
/// 这里用 serde 重命名，不手工解 JSON）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemVersionView {
    pub version: String,
    #[serde(default)]
    pub git_hash: String,
    #[serde(default)]
    pub git_dirty: bool,
    #[serde(default)]
    pub build_date: String,
    #[serde(default)]
    pub target: String,
    /// `None` = 尚未检查（更新检查默认关闭），或对端是旧内核。
    #[serde(default)]
    pub update_available: Option<bool>,
    #[serde(default)]
    pub latest_version: Option<String>,
    /// 自动更新开关的当前值 —— UI 的开关状态来自内核，不来自本地猜测。
    #[serde(default)]
    pub auto_update: bool,
    /// 上次检查更新的时刻（UTC 毫秒），未检查为 `None`。
    #[serde(default)]
    pub last_check_ms: Option<u64>,
    /// 上一句原始 JSON，供 `blitzkrieg version --core --json` 忠实转发。
    #[serde(skip)]
    pub raw_json: String,
}

impl IpcClient {
    /// `system.version` —— 版本 + 构建来源 + 更新状态。
    ///
    /// 对端是旧内核（方法不存在）时返回错误而不是空值：调用方据此说
    /// 「这个内核不认识 system.version」，比编造一个空版本号诚实。
    pub fn system_version(&mut self) -> Result<SystemVersionView, IpcError> {
        let raw = self.call(method::SYSTEM_VERSION, serde_json::json!({}))?;
        let mut v: SystemVersionView = serde_json::from_value(raw.clone())
            .map_err(|e| IpcError::Decode(format!("system.version: {e}")))?;
        v.raw_json = raw.to_string();
        Ok(v)
    }
}
```

`ui/ui_kit/src/core/types.rs` 的 `ReadyView` **不**改（它有既有消费者）；`SystemVersionView` 是新增类型，语义更全。`ui/ui_kit/src/core/mod.rs` 无需改动（类型挂在既有的 `types.rs` 与 `ipc_client.rs` 上）。

### 4.5 输出样例（可直接当作验收基准）

```
$ blitzkrieg version
BlitzkriegBot 0.2.1
  git      96fcf51a1b2c (clean)
  built    20260925T073811Z
  target   aarch64-apple-darwin

$ blitzkrieg version --json
{"version":"0.2.1","gitHash":"96fcf51a1b2c","gitDirty":false,"buildDate":"20260925T073811Z","target":"aarch64-apple-darwin"}

$ blitzkrieg --version
0.2.1+g96fcf51a1b2c

$ blitzkrieg version --core
BlitzkriegBot 0.2.1 (running core)
  git      96fcf51a1b2c (clean)
  built    20260925T073811Z
  target   aarch64-apple-darwin
  socket   /tmp/blitzkrieg-core.sock
  update   not checked (更新检查默认关闭)

$ blitzkrieg version --core --json
{"version":"0.2.1","gitHash":"96fcf51a1b2c","gitDirty":false,"buildDate":"20260925T073811Z","target":"aarch64-apple-darwin","updateAvailable":null,"latestVersion":null,"autoUpdate":false,"lastCheckMs":null}

$ blitzkrieg version --core ; echo "exit=$?"
blitzkrieg: version: no core on /tmp/blitzkrieg-core.sock: connect: Connection refused
  (只想知道磁盘上装的是哪一版，去掉 --core。)
exit=1
```

> 三个格式各有明确消费者，都不许随手改：`version` 的 4 行文本给人读；`--json` 给脚本（字段名是契约）；`--version` 的单行给门禁的 `VERSION_RE`（第 3.3 节）。要加字段就加，不要改名。

---

## 5. `system.version` IPC 契约（含 JSON 示例）

### 5.1 方法常量（`core/blitzkrieg_core/src/ipc/schema.rs` 的 `pub mod method`）

```rust
pub mod method {
    // ... 既有常量不变 ...

    /// 版本与构建来源 + 更新状态（VERSIONING.md §5）。
    /// 请求：空参数 `{}`；响应：`SystemVersion`。只读、无副作用、不加锁。
    pub const SYSTEM_VERSION: &str = "system.version";
    /// 更新开关的读写（见 §7.4）。写操作会落审计记录。
    pub const SYSTEM_UPDATE_CONFIGURE: &str = "system.update.configure";
    /// 手动触发一次更新检查（UI 的「检查更新」按钮）。
    pub const SYSTEM_UPDATE_CHECK: &str = "system.update.check";
}
```

### 5.2 请求 / 响应

**请求**（空参数；`params` 缺省与 `{}` 等价）：

```json
{ "jsonrpc": "2.0", "version": "1.1", "id": 7, "method": "system.version", "params": {} }
```

**响应**（更新检查尚未发生 —— 也就是默认状态）：

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "version": "0.2.1",
    "gitHash": "96fcf51a1b2c",
    "gitDirty": false,
    "buildDate": "20260925T073811Z",
    "target": "aarch64-apple-darwin",
    "updateAvailable": null,
    "latestVersion": null,
    "autoUpdate": false,
    "checkEnabled": false,
    "lastCheckMs": null,
    "releaseUrl": null
  }
}
```

**响应**（已检查过、发现新版本）：

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "version": "0.2.1",
    "gitHash": "96fcf51a1b2c",
    "gitDirty": false,
    "buildDate": "20260925T073811Z",
    "target": "aarch64-apple-darwin",
    "updateAvailable": true,
    "latestVersion": "0.2.2",
    "autoUpdate": false,
    "checkEnabled": true,
    "lastCheckMs": 1790318400000,
    "releaseUrl": "https://github.com/ceer-quant/BlitzkriegBot/releases/tag/v0.2.2"
  }
}
```

### 5.3 字段契约（逐字段定死）

| 字段 | 类型 | 可空 | 语义 | 消费方必须如何处理 |
|---|---|---|---|---|
| `version` | string | 否 | 语义化版本，等于根 `[workspace.package].version` | 直接显示；**不可**用于比较更新（见下） |
| `gitHash` | string | 否 | 12 位短 sha，或 `nogit` | `nogit` 时 UI 要显示「无法指认修订号」，不能显示成 sha |
| `gitDirty` | bool | 否 | 构建期跟踪文件是否有改动 | true 时显示「dirty」，提示这不是干净的发布构建 |
| `buildDate` | string | 否 | `YYYYMMDDTHHMMSSZ`（UTC） | 原样显示；**不要**在 UI 侧做时区换算后再声称是本地时间 |
| `target` | string | 否 | 目标三元组 | 更新选包时用它匹配资产名 |
| `updateAvailable` | bool \| **null** | **是** | `null` = 尚未检查；`true`/`false` = 检查结论 | **三态**，不是布尔。把 null 当 false 画成「已是最新」是在撒谎（这正是 `net_check` 那套「未知状态原样回显」的既有规则） |
| `latestVersion` | string \| null | 是 | 远端最新 tag 去掉 `v` 后的版本 | null 时不要编造 |
| `autoUpdate` | bool | 否 | 自动更新开关的**当前**值 | UI 开关状态以此为准，不以本地缓存为准 |
| `checkEnabled` | bool | 否 | 是否允许出网检查 | 关闭时 UI 的「检查更新」按钮应说明「检查已关闭」而不是静默失败 |
| `lastCheckMs` | number \| null | 是 | 上次检查的 UTC 毫秒 | UI 显示「上次检查：<时间>」 |
| `releaseUrl` | string \| null | 是 | Release 页地址 | 给「查看发布说明」用 |

### 5.4 三条契约规则

**规则 R1——方法名/字段名一经发布不得改名。** 只允许新增字段。删字段或改名 = 破坏性变更，必须撞大版本并同步改 `ui_kit` 的 `SystemVersionView` 与 `schema.rs`。

**规则 R2——`PROTOCOL_VERSION` 不动。** 现状 `schema.rs` 的 `PROTOCOL_VERSION = "1.1"` 描述的是**信封**形状，`system.version` 是纯新增方法，不触碰信封。为它单独 bump 协议版本会让所有既有客户端做无谓的升级判断。只有信封变了才 bump。

**规则 R3——版本比较只在内核做一处，且用 semver 而不是字符串。** 字符串比较会把 `0.10.0` 判成小于 `0.9.0`。内核侧实现（第 7.2 节）是唯一比较点；UI 只显示 `updateAvailable` 的结论。

### 5.5 分发位置（`server.rs` 的 `match method` 内新增，位置紧邻 `READY` 分支）

```rust
// VERSIONING.md §5：版本与构建来源 + 更新状态。只读、零副作用、不取 Core 锁 ——
// 它在数据锁尚未就绪、Core 正在启动时也必须能回答（「跑的是哪一版」不该等启动完成）。
method::SYSTEM_VERSION => Ok(crate::ipc::version::system_version_payload(
    update_state.snapshot(),
)),
```

`update_state` 是新增的进程级状态（第 7.3 节），与 `Core` 分离——版本问题与交易状态无关，混进 `Core` 锁会让这个调用被持仓更新阻塞。

### 5.6 兼容性：对端是旧内核时

旧内核没有这个常量，`match` 会走到兜底的「unknown method」分支。**不新增兼容分支**：`ui_kit` 的 `system_version()` 返回 `Err`，UI 显示「这个内核不认识 system.version（旧版本）」，而不是画一个空版本号。理由与 0.8 一致——宁可说不会，不可编一个答案。

### 5.7 Node 侧示例（门禁脚本与外部消费者）

```js
// 最小消费者：一个门禁怎么问运行中的内核「你是哪一版」
import { requestOnce as rpc } from './lib/core-client.mjs';

const reply = await rpc(sock, 'system.version', {});
if (reply.error) throw new Error(`system.version: ${reply.error.message}`);
const { version, gitHash, gitDirty, buildDate, target, updateAvailable } = reply.result;

// 门禁的用法：把版本与修订号钉进结论，而不是只报「绿灯」。
console.log(`  core ${version}+g${gitHash}${gitDirty ? ' (dirty)' : ''} built ${buildDate} for ${target}`);

// 三态必须显式处理 —— null 不许当成 false。
if (updateAvailable === null) console.log('  note 更新状态未知（检查默认关闭，或尚未检查）');
else if (updateAvailable) console.log('  note 有新版本可用（这是发布线，不是本门禁的结论）');
```

---

## 6. TUI 与 WebUI 设置页设计（含界面草图）

### 6.1 两侧的分工（同一事实、同一语气）

沿用仓库既有的对等规则（`ui/webapp/webui/scripts/tui-parity.check.mjs` 已经把它写成可执行门禁）：**TUI 有的面，WebUI 必须有家；WebUI 独占的面，必须写明它在面板外的等价入口。** 版本信息属于「两侧都必须有」，不是某一侧的增强。

| 事实 | TUI | WebUI | 两侧必须一致的地方 |
|---|---|---|---|
| 版本号 | `0.2.1` | `0.2.1` | 都来自 `system.version`，不各自 parse |
| 修订号 | `96fcf51a1b2c (clean)` | `96fcf51a1b2c (clean)` | `nogit` 与 dirty 的措辞 |
| 构建时间/平台 | `20260925T073811Z` / `aarch64-apple-darwin` | 同 | 原样显示 UTC |
| 更新徽章 | `UPDATE 0.2.2` | Badge `可更新 0.2.2` | **三态**：有更新 / 已最新 / 未知（未检查） |
| 「检查更新」 | `c` 键 | Button | 关闭检查时的措辞一致（见 5.3 的 `checkEnabled`） |
| 「自动更新」开关 | `a` 键 | Switch | 默认关闭；状态来源是内核（`autoUpdate`）而不是本地缓存 |

### 6.2 TUI：新增第 6 个 Tab `Settings`

**新增需要改的三处（TUI 侧）**：

1. `ui/ui_kit_panel/src/app.rs`：`enum Tab` 加 `Settings`，并同步 `titles()` / `index()` / `next()` 三个方法（现状这三处各有一份手写映射——加变体时漏改任何一处都会让 Tab 循环错位）。
2. `ui/ui_kit_panel/src/lib.rs`：Tab 快捷键映射加 `Some("6") | Some("settings") => Tab::Settings`；主循环加一次 `system_version` 拉取（低频：与快照同周期即可，版本不会在运行中变化）。
3. `ui/ui_kit_panel/src/ui.rs`：新增 `fn render_settings(...)`——门禁按这个名字查找（`fn render_<tab>`），命名必须精确。

```rust
// ui/ui_kit_panel/src/app.rs —— Tab 三处映射同步（节选）
pub enum Tab { Overview, Positions, Trades, Plugins, Evolution, Settings }

impl Tab {
    pub fn titles() -> Vec<&'static str> {
        vec![
            "1 Overview", "2 Positions", "3 Trades",
            "4 Plugins", "5 Evolution", "6 Settings",
        ]
    }
    pub fn index(self) -> usize {
        match self {
            Tab::Overview => 0, Tab::Positions => 1, Tab::Trades => 2,
            Tab::Plugins => 3, Tab::Evolution => 4, Tab::Settings => 5,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Positions, Tab::Positions => Tab::Trades,
            Tab::Trades => Tab::Plugins, Tab::Plugins => Tab::Evolution,
            Tab::Evolution => Tab::Settings, Tab::Settings => Tab::Overview,
        }
    }
}
```

**TUI 界面草图**（`render_settings`）：

```
┌─ 6 Settings ────────────────────────────────────────────────────────────────┐
│                                                                             │
│  VERSION                                                                    │
│    BlitzkriegBot 0.2.1                                                      │
│    git     96fcf51a1b2c (clean)                                             │
│    built   20260925T073811Z                                                 │
│    target  aarch64-apple-darwin                                             │
│    core    0.2.1+g96fcf51a1b2c   ← 运行中的内核（--core 同一来源）          │
│                                                                             │
│  UPDATE                                                                     │
│    status    ▲ 0.2.2 available        ← 三态：▲可更新 / ✓已最新 / …未知     │
│    last      2026-09-25 07:41Z                                              │
│    auto      [ ] 关闭                 ← 默认关闭；`a` 切换（需显式确认）     │
│    check     [x] 允许出网检查         ← 关闭时下面一行说明为什么不按键无反应 │
│                                                                             │
│  [c] 检查更新   [a] 切换自动更新   [i] 立即安装（仅自动更新开启时可用）      │
│                                                                             │
│  未知状态说明：检查关闭或尚未检查时，状态显示「…未知」，绝不画成「已最新」。 │
└─────────────────────────────────────────────────────────────────────────────┘
```

**TUI 的键位与确认**（安全相关，沿用仓库既有习惯——破坏性动作要确认）：

- `c` 检查更新：调用 `system.update.check`；结果三态上屏。关闭检查时提示「检查已关闭 —— 在配置文件或设置里开启」，不静默。
- `a` 切换自动更新：**必须弹确认浮层**（复用既有的 `render_confirm`）。开启后写盘并落审计；关闭则立即生效。这是唯一会改变「以后是否自动替换二进制」的开关，不该一键无声翻转。
- `i` 立即安装：仅在 `autoUpdate` 开启时可用；安装由启动器执行（见 7.5），TUI 只触发与展示进度。

### 6.3 WebUI：`SettingsPage.vue` 新增「版本与更新」卡片

**现状**：`SettingsPage.vue` 已有 4 块（会话与访问 token / Gateway 指令台 / 网络诊断 / 外观与节奏）。版本卡片作为**第 5 块**追加，位置放在「网络诊断」之后、「外观与节奏」之前（版本是运维信息，与网络诊断同类）。

**复用既有组件，不新增依赖**：`Card` / `CardHeader` / `Badge` / `Button` / `Switch` / `Tooltip` / `AlertBanner` / `EmptyState` 都已存在（`components/ui/`）。

```vue
<!-- ui/webapp/webui/src/pages/SettingsPage.vue —— 追加一块（节选，保持本文件既有风格） -->
<script setup lang="ts">
// ...既有 import 不变...
import { versionBadge, readVersion, type VersionDoc } from '@/lib/version'
import type { SystemVersion } from '@/api/client'

const version = ref<SystemVersion | null>(null)
const versionErr = ref<string | null>(null)

async function loadVersion(): Promise<void> {
  versionErr.value = null
  try {
    version.value = await api.systemVersion()
  } catch (e) {
    // 旧内核不认识 system.version：说「不认识」，不画空版本号（契约 R1/§5.6）。
    version.value = null
    versionErr.value = e instanceof Error ? e.message : String(e)
  }
}
onMounted(loadVersion)

const badge = computed(() => versionBadge(version.value))
</script>

<template>
  <Card>
    <CardHeader>版本与更新</CardHeader>

    <AlertBanner v-if="versionErr" variant="warn">
      这个内核不认识 <code>system.version</code>（{{ versionErr }}）。版本信息不可用。
    </AlertBanner>

    <EmptyState v-else-if="!version" title="读取中…" />

    <template v-else>
      <dl class="grid grid-cols-[8rem_1fr] gap-y-2 text-sm">
        <dt class="text-faint-fg">版本</dt>
        <dd class="font-mono">
          {{ version.version }}
          <Badge v-if="badge" :variant="badge.variant">{{ badge.text }}</Badge>
        </dd>

        <dt class="text-faint-fg">修订号</dt>
        <dd class="font-mono">
          {{ version.gitHash === 'nogit' ? '无法指认修订号' : version.gitHash }}
          <span v-if="version.gitDirty" class="text-primary">dirty</span>
          <span v-else class="text-faint-fg">clean</span>
        </dd>

        <dt class="text-faint-fg">构建时间</dt>
        <dd class="font-mono">{{ version.buildDate }}</dd>

        <dt class="text-faint-fg">目标平台</dt>
        <dd class="font-mono">{{ version.target }}</dd>

        <dt class="text-faint-fg">上次检查</dt>
        <dd class="font-mono">{{ version.lastCheckMs ? dateTime(version.lastCheckMs) : '—' }}</dd>
      </dl>

      <div class="mt-4 flex items-center gap-3">
        <Button @click="checkNow" :disabled="!version.checkEnabled">
          检查更新
        </Button>
        <Tooltip v-if="!version.checkEnabled" content="出网检查已在配置中关闭；开启后此按钮才可用。">
          <span class="text-faint-fg">检查已关闭</span>
        </Tooltip>

        <Switch
          :model-value="version.autoUpdate"
          @update:model-value="toggleAutoUpdate"
          label="自动更新"
        />
        <span class="text-faint-fg text-xs">默认关闭；开启后启动器会校验并替换二进制</span>
      </div>
    </template>
  </Card>
</template>
```

**`ui/webapp/webui/src/lib/version.ts`——三态徽章的唯一实现**（与 TUI 的判据同源，两侧不各写一份）：

```ts
import type { SystemVersion } from '@/api/client'

export interface VersionBadge {
  text: string
  variant: 'up' | 'down' | 'default'
}

/**
 * 三态徽章。**null 不是 false** —— 把「尚未检查」画成「已是最新」是在撒谎，
 * 这条规则与内核侧 net_check 的「未知状态原样回显」是同一条纪律。
 */
export function versionBadge(v: SystemVersion | null): VersionBadge | null {
  if (!v) return null
  if (v.updateAvailable === null) return { text: '未检查', variant: 'default' }
  if (v.updateAvailable === false) return { text: '已是最新', variant: 'up' }
  return { text: `可更新 ${v.latestVersion ?? ''}`.trim(), variant: 'down' }
}
```

**WebUI 界面草图**：

```
┌─ 设置 ───────────────────────────────────────────────────────────────────────┐
│  …（会话与访问 token）…                                                       │
│  …（Gateway 指令台）…                                                         │
│  …（网络诊断）…                                                               │
│                                                                              │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │ 版本与更新                                                             │  │
│  │                                                                        │  │
│  │  版本        0.2.1   [ 可更新 0.2.2 ]   ← 三态徽章                     │  │
│  │  修订号      96fcf51a1b2c  clean                                       │  │
│  │  构建时间    20260925T073811Z                                          │  │
│  │  目标平台    aarch64-apple-darwin                                      │  │
│  │  上次检查    2026-09-25 07:41                                          │  │
│  │                                                                        │  │
│  │  [ 检查更新 ]   ( 检查已关闭 )      自动更新  (○)  默认关闭            │  │
│  │                                                                        │  │
│  │  ⚠ 找到 0.2.2。安装会把当前二进制替换为 0.2.2（校验 SHA256 后原子替换），│  │
│  │    需要重启内核生效。                                                  │  │
│  └────────────────────────────────────────────────────────────────────────┘  │
│                                                                              │
│  …（外观与节奏）…                                                             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.4 TUI/WebUI 对等门禁必须同改（否则 V6-4 必红）

`ui/webapp/webui/scripts/tui-parity.check.mjs` 里三处硬编码集合必须同步扩展，并新增一条对等断言：

```js
// 1) TUI 布局集合：加入第 6 个 tab
const TUI_TAB_SETS = [
  ['Overview', 'Positions', 'Trades', 'Plugins'],
  ['Overview', 'Positions', 'Trades', 'Plugins', 'Evolution'],
  ['Overview', 'Positions', 'Trades', 'Plugins', 'Evolution', 'Settings'],  // 新增
]

// 2) TUI face → WebUI home：Settings 对到 settings，并在两个文件里各钉一个活体标记
const TUI_TO_WEBUI = [
  // ...既有四/五条...
  ['Settings', ['settings'], 'SettingsPage.vue', '版本与更新'],   // 新增
]

// 3) 共享控制面：版本信息两侧同源、同三态
check('TUI 设置页 ↔ WebUI 设置页版本卡片同源', () => {
  assert.ok(/fn render_settings\(/.test(ui), 'TUI render_settings 缺失')
  const settings = read('src', 'pages', 'SettingsPage.vue')
  assert.ok(settings.includes('版本与更新'), 'WebUI 版本卡片缺失')
  // 同一事实、同一条三态规则：两侧都要能从内核问版本
  assert.ok(read('..', '..', 'ui_kit', 'src', 'core', 'ipc_client.rs').includes('fn system_version'))
  // TUI 侧也必须经同一常量/接口取值，不自己 parse 版本字符串
  assert.ok(read('..', '..', 'ui_kit_panel', 'src', 'ui.rs').includes('git_hash'))
})
```

> 第 3 条断言刻意只用「存在性」判据（不含渲染细节），因为渲染细节容易随样式变化——门禁该钉的是**契约的存在**，不是像素。

### 6.5 默认值：三处默认必须一致地为「关闭」

| 位置 | 默认 | 谁来翻转 |
|---|---|---|
| `user_layer/configs/update.toml`（工厂默认） | `check_enabled = false`、`auto_update = false` | 操作者手工编辑（工厂默认） |
| `data/update/state.json`（运行时开关） | 不存在 = 采用工厂默认 | UI 开关 / IPC 写操作 |
| 内核内置默认（编译进二进制） | 两者都为 `false` | 不允许翻转（这是 INI-3，见 1.3） |

三者一致的证明方式是 V6-5 的对抗性验收，而不是靠这三行文字。

---

## 7. 更新检查、下载、校验、安装流程（含 Rust 代码）

### 7.1 完整时序（谁在哪个进程、什么顺序）

```
内核启动
  ├─ 读配置（CLI > env > TOML > 默认）→ check_enabled? auto_update?
  ├─ 不阻塞启动：spawn 一个 detached 任务（仅当 check_enabled）
  │     └─ GET https://api.github.com/repos/ceer-quant/BlitzkriegBot/releases/latest
  │           → 解析 tag_name
  │           → semver 比较（唯一比较点）
  │           → 写 UpdateState（内存 + data/update/state.json）
  │           → 失败绝不致命：记录状态，UI 显示未知
  └─ 服务继续；system.version 立刻可答（即使检查还在飞）

UI（TUI / WebUI 设置页）
  ├─ 读 system.version → 显示版本 + 三态徽章
  ├─ 点「检查更新」→ system.update.check（手动，绕过 check_enabled？不——见 7.4）
  └─ 切「自动更新」→ system.update.configure { autoUpdate: true }（写盘 + 审计）

启动器（blitzkrieg update --install / TUI 的 `i`）
  1. 读 system.version（经 IPC，拿到 version/target/updateAvailable）
  2. 校验：updateAvailable === true，否则拒绝并说明
  3. 解析资产名（target 匹配）→ 下载到临时文件
  4. 校验 SHA256（对照 Release 的 SHA256SUMS 资产）
  5. 校验 GPG（可选；策略见 7.5）
  6. 备份现值 → 原子替换（同目录 rename）→ 校验落盘字节
  7. 打印「需要重启内核生效」；不自动重启（重启是另一件事）
```

### 7.2 更新检查（内核侧，`core/blitzkrieg_core/src/ipc/version.rs`）

```rust
//! 版本与更新状态（VERSIONING.md §5/§7）。
//!
//! 三件事，边界写死在这里：
//!   1. `system.version` 的载荷（只读、无锁、随时可答）；
//!   2. 远端最新版本的**唯一比较点**（semver，不是字符串）；
//!   3. 检查开关与自动更新开关的运行时状态（持久化 + 审计）。
//!
//! 这个模块**不下载、不替换任何二进制**。安装是启动器的职责（见 7.5）：
//! 持仓进程改写正在执行的二进制是典型的自伤路径。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

/// 失败原因按「操作者能做什么」分类，不做笼统的 `io::Error`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckOutcome {
    /// 尚未检查（默认状态，检查关闭时保持此值）。
    NotChecked,
    /// 检查完成，且已是最新。
    UpToDate,
    /// 检查完成，有更新。
    Available,
    /// 检查失败：DNS/TLS/超时/HTTP 非 2xx/JSON 不合形。
    /// `detail` 面向操作者，绝不包含凭证。
    Failed { detail: String },
}

/// 运行时更新状态。进程级、与 `Core` 分离 —— 版本问题不该被持仓更新阻塞。
#[derive(Debug, Default)]
pub struct UpdateState {
    outcome: Mutex<CheckOutcome>,
    latest: Mutex<Option<String>>,
    release_url: Mutex<Option<String>>,
    last_check_ms: AtomicU64,
    check_enabled: AtomicBool,
    auto_update: AtomicBool,
}

impl UpdateState {
    pub fn new(check_enabled: bool, auto_update: bool) -> Self {
        // 内置默认恒为 false；这两行是「配置说了什么就是什么」的落点，
        // 不是「默认为真」的捷径。
        Self {
            outcome: Mutex::new(CheckOutcome::NotChecked),
            latest: Mutex::new(None),
            release_url: Mutex::new(None),
            last_check_ms: AtomicU64::new(0),
            check_enabled: AtomicBool::new(check_enabled),
            auto_update: AtomicBool::new(auto_update),
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        let outcome = self.outcome.lock().map(|g| g.clone()).unwrap_or(CheckOutcome::NotChecked);
        UpdateSnapshot {
            outcome,
            latest: self.latest.lock().ok().and_then(|g| g.clone()),
            release_url: self.release_url.lock().ok().and_then(|g| g.clone()),
            last_check_ms: self.last_check_ms.load(Ordering::Relaxed),
            check_enabled: self.check_enabled.load(Ordering::Relaxed),
            auto_update: self.auto_update.load(Ordering::Relaxed),
        }
    }

    pub fn set_enabled(&self, check: Option<bool>, auto: Option<bool>) {
        if let Some(v) = check { self.check_enabled.store(v, Ordering::Relaxed); }
        if let Some(v) = auto { self.auto_update.store(v, Ordering::Relaxed); }
    }

    fn apply(&self, outcome: CheckOutcome, latest: Option<String>, url: Option<String>, now_ms: u64) {
        if let Ok(mut g) = self.outcome.lock() { *g = outcome; }
        if let Ok(mut g) = self.latest.lock() { *g = latest; }
        if let Ok(mut g) = self.release_url.lock() { *g = url; }
        self.last_check_ms.store(now_ms, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSnapshot {
    pub outcome: CheckOutcome,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub last_check_ms: u64,
    pub check_enabled: bool,
    pub auto_update: bool,
}

/// `system.version` 的最终载荷。字段名 camelCase —— 线上契约（§5）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemVersion {
    pub version: String,
    pub git_hash: String,
    pub git_dirty: bool,
    pub build_date: String,
    pub target: String,
    /// **三态**：`null` = 未检查；`true`/`false` = 结论。不是布尔。
    pub update_available: Option<bool>,
    pub latest_version: Option<String>,
    pub auto_update: bool,
    pub check_enabled: bool,
    pub last_check_ms: Option<u64>,
    pub release_url: Option<String>,
}

/// 把 `UpdateSnapshot` 与构建信息拼成载荷。纯函数 → 可单测，不需进程。
pub fn system_version_payload(
    build: blitzkrieg_build_info::BuildInfo,
    s: &UpdateSnapshot,
) -> SystemVersion {
    // 「已检查过」的判据是 last_check_ms 非零 **且** 有结论；仅仅开关打开
    // 不算检查过 —— 否则 UI 会把「开了开关」画成「已是最新」。
    let (available, latest, url) = match (&s.outcome, s.last_check_ms) {
        (CheckOutcome::UpToDate, ms) if ms > 0 => (Some(false), None, None),
        (CheckOutcome::Available, ms) if ms > 0 => (Some(true), s.latest.clone(), s.release_url.clone()),
        _ => (None, None, None),   // 未检查 / 失败 → 三态的 null，绝不猜
    };
    SystemVersion {
        version: build.version.to_string(),
        git_hash: build.git_hash.to_string(),
        git_dirty: build.is_dirty(),
        build_date: build.build_date.to_string(),
        target: build.target.to_string(),
        update_available: available,
        latest_version: latest,
        auto_update: s.auto_update,
        check_enabled: s.check_enabled,
        last_check_ms: (s.last_check_ms > 0).then_some(s.last_check_ms),
        release_url: url,
    }
}

/// remote tag → 版本号。`v0.2.2` → `Some("0.2.2")`。
/// 严格：不是 `v<semver>` 形状的一律拒绝，绝不「尽力而为地」抽出数字 ——
/// 一个被误读的 tag 会变成一次错误的更新提示。
pub fn parse_release_tag(tag: &str) -> Option<String> {
    let v = tag.trim().strip_prefix('v')?;
    semver::Version::parse(v).ok().map(|p| p.to_string())
}

/// 远端是否比本地新。**唯一比较点**；调用方不得再自己比字符串。
/// 预发布语义由 semver 决定：`0.3.0-rc.1 < 0.3.0`，且默认不把预发布当作「更新」，
/// 除非本地自己就是预发布（同一条线上的 rc 才提醒 rc）。
pub fn is_newer(remote: &str, local: &str) -> bool {
    let (Ok(r), Ok(l)) = (semver::Version::parse(remote), semver::Version::parse(local)) else {
        // 任一侧不可解析 → 不宣布更新。宁可少提醒，不可误提醒。
        return false;
    };
    if !r.pre.is_empty() && l.pre.is_empty() {
        // 本地是正式版、远端是预发布：不算更新。预发布要先上线到正式 tag 才提示。
        return r.major == l.major && r.minor == l.minor && r.patch == l.patch && false;
    }
    r > l
}

/// 检查一次。`fetch` 注入是为了让测试用假 HTTP 而不是真网。
///
/// 失败**绝不**返回 Err 给调用方以外的东西：更新检查的失败不该影响交易进程，
/// 它只写进状态，由 UI 显示为「未知」。
pub async fn check_once(
    state: &Arc<UpdateState>,
    local_version: &str,
    now_ms: u64,
    fetch: impl std::future::Future<Output = Result<String, String>>,
) {
    if !state.snapshot().check_enabled {
        // 关闭时不发任何请求（INV-3 的结构性保证）。
        return;
    }
    match fetch.await {
        Ok(body) => match parse_latest_release(&body) {
            Ok((tag, url)) => match parse_release_tag(&tag) {
                Some(remote) if is_newer(&remote, local_version) => {
                    state.apply(CheckOutcome::Available, Some(remote), Some(url), now_ms)
                }
                Some(_) => state.apply(CheckOutcome::UpToDate, None, None, now_ms),
                None => state.apply(
                    CheckOutcome::Failed { detail: format!("release tag not v<semver>: {tag}") },
                    None, None, now_ms,
                ),
            },
            Err(detail) => state.apply(CheckOutcome::Failed { detail }, None, None, now_ms),
        },
        Err(detail) => state.apply(CheckOutcome::Failed { detail }, None, None, now_ms),
    }
}

/// 从 GitHub Releases API 的 JSON 里取出 (`tag_name`, `html_url`)。
/// 只认这两个字段：多读一个字段就多一个被上游格式变动影响的面。
fn parse_latest_release(body: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("releases/latest: not JSON: {e}"))?;
    let tag = v.get("tag_name").and_then(|t| t.as_str())
        .ok_or_else(|| "releases/latest: no tag_name".to_string())?;
    let url = v.get("html_url").and_then(|t| t.as_str()).unwrap_or("").to_string();
    Ok((tag.to_string(), url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_tag_is_not_a_version() {
        assert_eq!(parse_release_tag("v0.2.2").as_deref(), Some("0.2.2"));
        assert_eq!(parse_release_tag("0.2.2"), None, "缺少 v 前缀必须拒绝");
        assert_eq!(parse_release_tag("vlatest"), None);
        assert_eq!(parse_release_tag("v0.2"), None);
        // 预发布是可解析的，但排序与提示策略另算。
        assert_eq!(parse_release_tag("v0.3.0-rc.1").as_deref(), Some("0.3.0-rc.1"));
    }

    #[test]
    fn comparison_is_semver_not_lexicographic() {
        // 字符串比较会把 0.10.0 判成小于 0.9.0 —— 这正是不能用字符串的理由。
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.1.9", "0.2.1"));
    }

    #[test]
    fn pre_releases_do_not_nag_a_stable_install() {
        assert!(!is_newer("0.3.0-rc.1", "0.2.1"), "正式版不该被 rc 提示");
        assert!(!is_newer("0.3.0-rc.1", "0.3.0"), "同号正式版比 rc 新");
        assert!(is_newer("0.3.0", "0.3.0-rc.1"));
    }

    #[test]
    fn garbage_never_becomes_an_update_notice() {
        assert!(!is_newer("latest", "0.2.1"));
        assert!(!is_newer("0.2.2", "garbage"));
    }

    #[test]
    fn the_payload_is_three_state() {
        let b = blitzkrieg_build_info::BUILD_INFO;
        // 从未检查：updateAvailable 必须是 None，不能被写成 Some(false)。
        let s = UpdateSnapshot {
            outcome: CheckOutcome::NotChecked, latest: None, release_url: None,
            last_check_ms: 0, check_enabled: false, auto_update: false,
        };
        let p = system_version_payload(b, &s);
        assert_eq!(p.update_available, None);
        assert_eq!(p.last_check_ms, None);

        // 检查失败同样是「未知」，不是「已最新」。
        let s = UpdateSnapshot {
            outcome: CheckOutcome::Failed { detail: "dns".into() }, latest: None,
            release_url: None, last_check_ms: 1, check_enabled: true, auto_update: false,
        };
        assert_eq!(system_version_payload(b, &s).update_available, None);

        // 结论存在时才落到布尔。
        let s = UpdateSnapshot {
            outcome: CheckOutcome::UpToDate, latest: None, release_url: None,
            last_check_ms: 1, check_enabled: true, auto_update: false,
        };
        assert_eq!(system_version_payload(b, &s).update_available, Some(false));
    }

    #[tokio::test]
    async fn a_disabled_check_makes_no_request() {
        let state = Arc::new(UpdateState::new(false, false));
        // 这个 future 一旦被 await 就 panic —— 关闭时必须根本不轮询它。
        let must_not_run = async {
            panic!("check_enabled=false 时不得发起任何请求（INV-3）");
            #[allow(unreachable_code)] Ok::<String, String>(String::new())
        };
        check_once(&state, "0.2.1", 1, must_not_run).await;
        assert_eq!(state.snapshot().outcome, CheckOutcome::NotChecked);
    }

    #[tokio::test]
    async fn an_api_error_is_recorded_not_raised() {
        let state = Arc::new(UpdateState::new(true, false));
        check_once(&state, "0.2.1", 42, async { Err("connection refused".to_string()) }).await;
        let s = state.snapshot();
        assert!(matches!(s.outcome, CheckOutcome::Failed { .. }));
        assert_eq!(system_version_payload(blitzkrieg_build_info::BUILD_INFO, &s).update_available, None);
    }

    #[tokio::test]
    async fn a_newer_release_becomes_an_available_state() {
        let state = Arc::new(UpdateState::new(true, false));
        let body = r#"{"tag_name":"v0.2.2","html_url":"https://example/v0.2.2"}"#;
        check_once(&state, "0.2.1", 7, async move { Ok(body.to_string()) }).await;
        let s = state.snapshot();
        assert_eq!(s.outcome, CheckOutcome::Available);
        assert_eq!(s.latest.as_deref(), Some("0.2.2"));
        assert_eq!(system_version_payload(blitzkrieg_build_info::BUILD_INFO, &s).update_available, Some(true));
    }
}
```

### 7.3 出网实现与会话状态

```rust
// core/blitzkrieg_core/src/ipc/version.rs（续）—— HTTP 只在这里落地
//
// 依赖选择：`reqwest`（rustls）**已在 Cargo.lock 里**（0.13.2，由
// polymarket-extension 拉入），新增直接依赖不引入新的依赖树（现状 0.12）。
// 不用 `curl` 子进程：一个交易内核不该靠外部二进制的存在来回答「有没有新版本」。

const RELEASES_LATEST: &str =
    "https://api.github.com/repos/ceer-quant/BlitzkriegBot/releases/latest";

/// 真实的一次抓取。GitHub 要求 User-Agent；超时短（检查失败无害，
/// 不能拖住任何东西）。
async fn fetch_latest_release() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("BlitzkriegBot/", env!("BLITZKRIEG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(RELEASES_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET releases/latest: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // 404 = 私有仓库或还没有 Release（前者需要令牌，见下），403/429 = 限流。
        return Err(format!("GET releases/latest: HTTP {status}"));
    }
    resp.text().await.map_err(|e| format!("read body: {e}"))
}

/// 挂载启动检查：**不阻塞启动**，且只在开关打开时才有动作。
/// 私有仓库/限流场景通过 `BLITZKRIEG_GITHUB_TOKEN` 支持（GitHub API 可接受
/// Bearer 或 `?access_token=`；用 header 而不是 URL，令牌不进日志）。
pub fn spawn_startup_check(state: Arc<UpdateState>, local_version: String, token: Option<String>) {
    if !state.snapshot().check_enabled {
        return; // 关闭 → 不 spawn、不请求、不发任何包（INV-3）
    }
    tokio::spawn(async move {
        let fetch = async move {
            match token {
                None => fetch_latest_release().await,
                Some(t) => fetch_latest_release_with_token(&t).await,
            }
        };
        let now = crate::now_ms();
        check_once(&state, &local_version, now, fetch).await;
        if let Err(e) = persist(&state) {
            tracing::warn!("update state persist failed (non-fatal): {e}");
        }
    });
}

async fn fetch_latest_release_with_token(token: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("BlitzkriegBot/", env!("BLITZKRIEG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(RELEASES_LATEST)
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)                    // 令牌只走 header
        .send()
        .await
        .map_err(|e| format!("GET releases/latest: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET releases/latest: HTTP {}", resp.status()));
    }
    resp.text().await.map_err(|e| format!("read body: {e}"))
}
```

**持久化落点**（沿用 `data/evolution/state.json` 的既有形态，现状 0.22）：

```rust
/// `data/update/state.json` —— 运行时开关（压过工厂默认的 update.toml）。
/// 只存开关与上次检查摘要；**不存**任何令牌。
fn state_path() -> std::path::PathBuf { std::path::PathBuf::from("data/update/state.json") }

pub fn persist(state: &UpdateState) -> std::io::Result<()> {
    let s = state.snapshot();
    let path = state_path();
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    let doc = serde_json::json!({
        "checkEnabled": s.check_enabled,
        "autoUpdate": s.auto_update,
        "lastCheckMs": s.last_check_ms,
        "latest": s.latest,
    });
    // 原子写：同目录临时文件 + rename，避免半截文件让下次启动读到坏 JSON。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, &path)
}
```

### 7.4 `system.update.configure` 与 `system.update.check`

```rust
// server.rs 内新增两个分支
method::SYSTEM_UPDATE_CONFIGURE => {
    let req: UpdateConfigureParams = serde_json::from_value(params.clone())
        .map_err(|e| (Failure::INVALID_PARAMS, format!("{e}"), None))?;
    // 审计：谁在什么时候把「以后会自动替换二进制」打开了。这是安全相关的状态变更，
    // 与 risk.kill 同级记录 —— actor 取内核记录的 peer uid，不取调用者的自称。
    let actor = match peer {
        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
        _ => "uid:unknown".to_string(),
    };
    update_state.set_enabled(req.check_enabled, req.auto_update);
    if let Err(e) = crate::ipc::version::persist(&update_state) {
        // 写盘失败要**告诉调用者**：一个没落盘的开关在重启后会静默变回关闭，
        // 而操作者以为打开过。
        return Err((Failure::APPLICATION, format!("update state not persisted: {e}"), None));
    }
    crate::audit_update_configure(&actor, req.check_enabled, req.auto_update);
    Ok(serde_json::json!({
        "checkEnabled": update_state.snapshot().check_enabled,
        "autoUpdate": update_state.snapshot().auto_update,
    }))
}

method::SYSTEM_UPDATE_CHECK => {
    // 手动检查同样受 check_enabled 约束：开关的语义是「是否允许出网」，
    // 而不是「是否自动检查」。这样「检查更新」按钮在关闭时给出明确答复，
    // 而不是绕过开关偷偷出网。
    if !update_state.snapshot().check_enabled {
        return Err((
            Failure::APPLICATION,
            "update checking is disabled (checkEnabled=false); enable it in \
             user_layer/configs/update.toml or via system.update.configure"
                .to_string(),
            None,
        ));
    }
    let local = crate::ipc::build_info::BUILD_INFO.version.to_string();
    let token = std::env::var("BLITZKRIEG_GITHUB_TOKEN").ok().filter(|t| !t.trim().is_empty());
    crate::ipc::version::check_once(
        &update_state, &local, crate::now_ms(),
        crate::ipc::version::fetch_latest(token).await,   // Ok/Err 都是值，不是异常
    ).await;
    let s = update_state.snapshot();
    Ok(serde_json::to_value(crate::ipc::version::system_version_payload(
        crate::ipc::build_info::BUILD_INFO, &s,
    )).unwrap_or(Value::Null))
}
```

**审计记录**（沿用仓库既有的 JSONL 追加规则 —— `jsonl::append`，且已收敛为单处实现）：

```jsonl
{"ts":1758780000000,"event":"update.configure","actor":"uid:501","checkEnabled":false,"autoUpdate":true}
{"ts":1758780600000,"event":"update.check","actor":"uid:501","outcome":"available","latest":"0.2.2"}
```

### 7.5 下载、校验、安装（启动器侧，`blitzkrieg update --install`）

**为什么在启动器**：内核是持仓进程，替换正在执行的二进制是自伤。启动器已拥有内核生命周期（`Supervisor::start/stop`），且 `scripts/upgrade.sh` 的既有语义就是「停→校验→装→起→验证身份」。

```rust
// ui/ui_kit_panel/src/bin/blitzkrieg.rs（或拆到 ui_kit_panel/src/update.rs）
//! `blitzkrieg update` —— 下载 → 校验 → 原子替换。三件事，一件都不能省。

/// 安装策略。默认值就是「安全但可用」：要求 SHA256，GPG 有则验、无则响亮警告。
#[derive(Debug, Clone)]
pub struct InstallPolicy {
    /// 是否要求 GPG 签名。true = 无法验证即拒绝安装。
    /// 默认 false：有签名就验，没签名就大声警告并继续（见 §0.2 的取舍）。
    pub require_signature: bool,
    /// 允许覆盖的目标（默认当前可执行文件）。
    pub target: std::path::PathBuf,
}

impl Default for InstallPolicy {
    fn default() -> Self {
        Self {
            require_signature: false,
            // `current_exe()` 在 shim 场景下解析到仓库里的真实二进制 ——
            // 这正是要替换的那一份（见 scripts/install-blitzkrieg-shim.sh）。
            target: std::env::current_exe().unwrap_or_else(|_| "blitzkrieg".into()),
        }
    }
}

/// 资产名：`blitzkrieg-<version>-<target>.tar.gz`（与 package-release 的产物对齐）。
/// 用 `target` 三元组匹配，而不是「猜平台」——猜错会装上一个不能执行的二进制。
pub fn asset_name(version: &str, target: &str) -> String {
    format!("blitzkrieg-{version}-{target}.tar.gz")
}

/// 期望的 SHA256：来自 Release 的 `SHA256SUMS` 资产（一行一文件：`<hex>  <name>`）。
/// 找不到对应行 = 校验不可能通过 = 拒绝安装（绝不在缺摘要时「跳过着校验」）。
pub fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let (hex, name) = (it.next()?, it.next()?);
        let name = name.trim_start_matches('*');
        (name == asset && hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

/// SHA256 比对。常量时间比较不必要（这里不是密钥），但**必须**比完整 64 位。
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::digest(bytes));
    if got == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!("sha256 mismatch: got {got}, want {expected}"))
    }
}

/// GPG 校验（可选）。三种回答必须可分辨：已验证 / 无法验证（缺 gpg 或缺签名）/ 验证失败。
/// 前两者按 `require_signature` 决定放行，第三者**永远拒绝**——坏签名不等于没签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureVerdict {
    Verified { key: String, user: String },
    Unavailable { why: String },
    Invalid { why: String },
}

pub fn verify_signature(asset: &std::path::Path, sig: &std::path::Path) -> SignatureVerdict {
    if !sig.exists() {
        return SignatureVerdict::Unavailable { why: format!("no signature file {}", sig.display()) };
    }
    match std::process::Command::new("gpg")
        .args(["--status-fd", "1", "--verify"])
        .arg(sig)
        .arg(asset)
        .output()
    {
        Err(e) => SignatureVerdict::Unavailable { why: format!("gpg not runnable: {e}") },
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() && !text.contains("GOODSIG") {
                // 有签名文件、gpg 跑起来了、结果不是 GOODSIG → 明确失败，永不降级。
                return SignatureVerdict::Invalid {
                    why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                };
            }
            let key = text.lines().find_map(|l| l.strip_prefix("[GNUPG:] VALIDSIG "))
                .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
                .unwrap_or_default();
            let user = text.lines().find_map(|l| l.strip_prefix("[GNUPG:] GOODSIG "))
                .map(|s| s.split_once(' ').map(|(_, n)| n.trim().to_string()).unwrap_or_default())
                .unwrap_or_default();
            SignatureVerdict::Verified { key, user }
        }
    }
}

/// 原子替换：写同目录临时文件 → `rename`。
/// 同目录是必须的：跨文件系统 rename 会退化成复制，那就不是原子了。
pub fn atomic_replace(target: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| {
        std::io::Error::other(format!("no parent dir for {}", target.display()))
    })?;
    let tmp = dir.join(format!(
        ".{}.new-{}",
        target.file_name().and_then(|s| s.to_str()).unwrap_or("blitzkrieg"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    // 先给可执行位再 rename：rename 之后到 chmod 之间有一个窗口，
    // 期间那个名字指向一个不可执行的文件。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, target)?;
    // 落盘字节自证：读了再比，不看写入调用的返回值。
    let after = std::fs::read(target)?;
    if after != bytes {
        return Err(std::io::Error::other(format!(
            "post-install byte mismatch on {}", target.display()
        )));
    }
    Ok(())
}

/// 安装全流程。返回 `Ok(())` 之前，每一个前提都被验证过；任何一步失败都
/// 是 `Err`，且**不留下半完成状态**（替换是原子的，之前的步骤只写临时目录）。
pub fn install(
    version: &str,
    target_triple: &str,
    policy: &InstallPolicy,
    files: UpdateFiles,
) -> Result<InstallReport, String> {
    // ① 摘要必须在，且必须匹配。
    let want = expected_sha256(&files.sums, &asset_name(version, target_triple))
        .ok_or_else(|| format!("SHA256SUMS has no entry for {version} {target_triple} — refusing"))?;
    verify_sha256(&files.bytes, &want)?;

    // ② 签名：有则必须有效；无法验证时按策略决定。
    let sig_tmp = files.dir.join("asset.sig");
    std::fs::write(&sig_tmp, &files.sig_bytes).map_err(|e| e.to_string())?;
    let asset_tmp = files.dir.join("asset.bin");
    std::fs::write(&asset_tmp, &files.bytes).map_err(|e| e.to_string())?;
    let verdict = verify_signature(&asset_tmp, &sig_tmp);
    match &verdict {
        SignatureVerdict::Verified { .. } => {}
        SignatureVerdict::Invalid { why } => {
            return Err(format!("GPG signature INVALID: {why} — refusing to install"))
        }
        SignatureVerdict::Unavailable { why } => {
            if policy.require_signature {
                return Err(format!("GPG required but unavailable: {why}"));
            }
            eprintln!("warning: GPG signature not verified ({why}); SHA256 matched.");
        }
    }

    // ③ 备份现值（脚本层已有的习惯：升级留回滚点）。
    let backup = policy.target.with_extension("prev");
    std::fs::copy(&policy.target, &backup)
        .map_err(|e| format!("backup {}: {e}", policy.target.display()))?;

    // ④ 原子替换 + 字节自证。
    atomic_replace(&policy.target, &files.bytes).map_err(|e| e.to_string())?;

    Ok(InstallReport {
        version: version.to_string(),
        target: policy.target.clone(),
        backup,
        sha256: want,
        signature: verdict,
        restart_required: true,
    })
}

#[derive(Debug)]
pub struct UpdateFiles {
    pub dir: std::path::PathBuf,
    pub bytes: Vec<u8>,
    pub sig_bytes: Vec<u8>,
    pub sums: String,
}

#[derive(Debug)]
pub struct InstallReport {
    pub version: String,
    pub target: std::path::PathBuf,
    pub backup: std::path::PathBuf,
    pub sha256: String,
    pub signature: SignatureVerdict,
    /// 安装完成 ≠ 生效。内核是持仓进程，必须由操作者决定何时重启。
    pub restart_required: bool,
}
```

**CLI 表面与输出**：

```
$ blitzkrieg update --check
BlitzkriegBot 0.2.1 → 0.2.2 available
  release  https://github.com/ceer-quant/BlitzkriegBot/releases/tag/v0.2.2
  asset    blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz
  (dry run; 安装请用 --install)

$ blitzkrieg update --install
  downloading blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz … 9.8 MB
  sha256   ok  (expected 3f9c…, got 3f9c…)
  gpg      GOODSIG Blitzkrieg Release <release@blitzkrieg.local>
  backup   /path/to/blitzkrieg.prev
  installed /path/to/blitzkrieg
  ⚠ 需要重启内核生效：blitzkrieg stop && blitzkrieg run
```

**绝不自动重启**：安装与重启是两件事。重新起栈可能立刻改变持仓状态，这个决定必须由操作者做（也是 `TUI` 的 `i` 只提示「需要重启」的原因）。

**安装前必须停内核吗？** 替换是原子的，运行中的进程继续用它已映射的旧 inode；新进程用新文件。所以技术上不必先停。但**要**在安装前用 IPC 检查这个内核是否持有持仓（`positions.list` 非空）并在有持仓时要求显式 `--yes-while-holding`，否则拒绝。让操作者在有持仓时糊里糊涂换掉二进制，是这份设计里最贵的一种「方便」。

### 7.6 更新机制的对抗性验收（先写测试，再写功能）

这些断言是 V7-2 门禁的实现内容，共同保证「默认关闭」不是口号：

| # | 断言 | 手段 |
|---|---|---|
| A1 | 默认配置下启动内核，**零出站连接** | 用一个指向黑洞的 `HTTPS_PROXY=http://127.0.0.1:1` 启动内核，再断言启动后 `checkEnabled === false` 且代理日志为空；或对 `api.github.com` 做 DNS 拦截后启动，断言启动成功且 `lastCheckMs === null` |
| A2 | `checkEnabled=false` 时 `system.update.check` **返回明确错误**，且不产生请求 | 7.2 的 `a_disabled_check_makes_no_request`（future 被 await 即 panic） |
| A3 | 检查失败**不影响启动、不影响交易** | 断网启动，断言内核 ready、快照正常、`updateAvailable === null` |
| A4 | 手动检查关闭时 UI 有明确文案 | 断言 WebUI 渲染「检查已关闭」；TUI 按 `c` 得到同样结论 |
| A5 | `autoUpdate` 默认 false，且**重启后仍是 false** | 写盘测试：不改配置 → 重启 → `system.version.autoUpdate === false` |
| A6 | 开关写盘失败**必须报错**，不许静默 | 把 `data/update` 设为只读，断言 configure 返回错误且内存状态未翻转 |
| A7 | 下载的字节 SHA256 不匹配时**绝不安装** | 篡改 1 字节，断言 `install` 返回 Err、目标文件字节未变 |
| A8 | 有签名但签名错误时**绝不安装**（即使 `require_signature=false`） | 用另一把密钥签，断言 `SignatureVerdict::Invalid` 且拒绝 |
| A9 | `require_signature=true` 且无 gpg 时**拒绝安装** | 把 `gpg` 从 PATH 移出，断言 Err |
| A10 | 缺 `SHA256SUMS` 条目时**拒绝安装** | 摘要里删掉该资产行，断言 Err |
| A11 | 替换是原子的：中途失败不留下半截二进制 | 让 rename 前失败（只读目录），断言目标文件与替换前逐字节相同 |
| A12 | 版本比较用 semver | `0.10.0 > 0.9.0`（字符串比较会判反） |
| A13 | 三态不许塌成两态 | 未检查/失败 → `updateAvailable === null`；断言 WebUI 徽章文案是「未检查」而不是「已是最新」 |
| A14 | 有持仓时不得静默替换 | `positions.list` 非空 + 无 `--yes-while-holding` → 拒绝 |

---

## 8. Git Tag 与 GitHub Actions 发布流程（含 YAML 示例）

### 8.1 现状（0.17 / 0.19，逐项确认）

| 项 | 现状 | 影响 |
|---|---|---|
| tag 数量 | **0**（本地与远端） | tag 体系从零建立，没有历史需要迁移 |
| CI 触发 | `push: [main, develop]` + `pull_request` | `release/*` 的 PR 会被 CI 覆盖（`pull_request` 全开），但**推送到 `release/*` 不触发** |
| CI 权限 | `contents: read` | 发布需要 `contents: write`，且必须**只给发布任务**，不能给整个工作流 |
| 远端名 | `ceer` | YAML 里如出现远端名必须是 `ceer`；Actions 里用 `github.repository` 而非硬编码远端名 |
| 既有作业 | `rust-check` / `panel-check` / `core-gates` / `exit-economics` / `release-bundle` / `secret-scan` / `ops-gates` | 发布流水线**复用**这些门禁，不另起一套判定 |

### 8.2 发布顺序（一次 patch 的完整生命周期）

```
①  从 release/0.2 开 fix/*
       git switch release/0.2 && git pull --ff-only ceer release/0.2
       git switch -c fix/rounding-on-zero-size

②  改代码 + 写 CHANGELOG + bump 版本（同一次提交里）
       # 根 Cargo.toml: version = "0.2.1" → "0.2.2"
       # CHANGELOG.md: [Unreleased] → [0.2.2] - <date>
       git commit -m "chore(release): 0.2.2 —— <一句话>"

③  推送 fix/*，开 PR 到 release/0.2；全绿后合并（squash 或 merge 由仓库既有习惯）

④  在 release/0.2 上打 tag（附注 tag，指向该合并提交）
       git switch release/0.2 && git pull --ff-only ceer release/0.2
       git tag -a v0.2.2 -m "BlitzkriegBot 0.2.2"
       # 有 GPG 时推荐 -s 代替 -a

⑤  push tag → 触发 release.yml
       git push ceer v0.2.2

⑥  release.yml 做四件事：守卫 → 门禁 → 构建打包 → 发布 Release（含 SHA256SUMS）

⑦  release/0.2 合并回 main（CI 的 `push: [main]` 会再跑一遍全量门禁）
       git switch main && git merge --ff-only release/0.2 && git push ceer main
```

⚠️ **现状的对齐问题**：`main` 与 `ceer/main` 现在是 `0 / 3`（main 严格落后 3 个提交，可 fast-forward）。**在建立 `release/0.2` 之前必须先把它补齐**，否则新线会从一个落后 3 个提交的 base 上长出来：

```bash
git switch main
git merge --ff-only ceer/main          # 0/3 → 可 ff；不可 ff 时停下来人工看，不要强推
git push ceer main
git switch -c release/0.2              # 补丁线自此处长出
git push -u ceer release/0.2
```

### 8.3 tag 守卫（发布流水线的第一道，也是最重要的一道）

`.github/workflows/release.yml`：

```yaml
name: Release

# 只有 tag 触发。**不用** push.branches —— 发布是「一个版本身份」的产物，
# 而不是「某个分支恰好绿了」的副产物。
on:
  push:
    tags:
      - 'v*'

# 同一 tag 的重复推送不并行；但也不 cancel —— 一个已经发出的 Release
# 不该被后一次运行半途掐掉。
concurrency:
  group: release-${{ github.ref }}
  cancel-in-progress: false

# 最小权限：只有发布作业需要写；其余作业（门禁）保持只读。
permissions:
  contents: read

jobs:
  # ── 守卫：tag 与仓库里的版本号必须严格一致 ────────────────────────────────
  # 这道守卫存在的理由：tag 与 Cargo.toml 不一致时，构建出来的二进制会
  # 自述成另一个版本 —— 而用户正是靠那个自述判断「我装的是哪一版」。
  # 事后无法从产物发现，所以必须在构建之前拦住。
  guard:
    name: tag-guard
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.v.outputs.version }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0        # tag 必须指向真实提交，浅克隆会让下面几项失去意义
      - name: Resolve the tag and the manifest version
        id: v
        run: |
          set -eu
          TAG="${GITHUB_REF_NAME}"
          MANIFEST=$(grep -m1 '^version' Cargo.toml | sed -n 's/.*"\(.*\)".*/\1/p')
          echo "tag=$TAG manifest=$MANIFEST"
          echo "version=$MANIFEST" >> "$GITHUB_OUTPUT"
          # ① 形态：v<semver>，tag 不带 +metadata
          case "$TAG" in
            v*) ;;
            *) echo "FAIL: tag '$TAG' must start with 'v'"; exit 1 ;;
          esac
          EXPECTED="v$MANIFEST"
          if [ "$TAG" != "$EXPECTED" ]; then
            echo "FAIL: tag '$TAG' != 'v' + [workspace.package].version ('$EXPECTED')"
            echo "      bump the version in the SAME commit the tag points at"
            exit 1
          fi
          # ② 预发布：-rc.N 只允许在 0.x 的 prepatch/prerelease 位置（本项目约定）
          case "$TAG" in
            *-rc.*) echo "note: pre-release tag $TAG" ;;
          esac
      - name: The version must exist in only ONE place
        run: |
          set -eu
          # 8 个成员必须全部继承；任何 'version = "0.' 都是漏改。
          # 检查器与门禁共用同一实现，避免两处判定漂移。
          node scripts/version-guard.mjs --manifest-only
      - name: The tag must sit on a release line (not a feature branch)
        run: |
          set -eu
          # tag 必须指向 main 或 release/*，不指向 feat/*、fix/*。
          # 这里的判定用「提交是否可从 main 或 release/* 到达」而不是分支名，
          # 因为 tag 本身不携带分支信息。
          git fetch --no-tags origin '+refs/heads/*:refs/remotes/origin/*' 2>/dev/null || true
          git fetch --no-tags ceer '+refs/heads/*:refs/remotes/ceer/*' 2>/dev/null || true
          ok=0
          for ref in refs/remotes/origin/main refs/remotes/ceer/main \
                     refs/remotes/origin/release refs/remotes/ceer/release; do
            :
          done
          # 明确枚举：main 与所有 release/* 分支
          for ref in $(git for-each-ref --format='%(refname)' \
                       'refs/remotes/**/main' 'refs/remotes/**/release/*'); do
            if git merge-base --is-ancestor "$GITHUB_SHA" "$ref"; then
              echo "ok: $GITHUB_SHA is reachable from $ref"
              ok=1
            fi
          done
          if [ "$ok" != 1 ]; then
            echo "FAIL: $GITHUB_SHA is not reachable from main or any release/* branch"
            echo "      a tag on an unreleased line would publish code that no branch owns"
            exit 1
          fi

  # ── 门禁：复用既有 CI 的判定，不另造绿灯 ──────────────────────────────────
  gates:
    name: gates
    needs: guard
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      # 发布构建必须接受与 PR 完全相同的门禁。任何「发布时豁免一下」的
      # 想法，都是把 CI 的绿灯变成装饰。
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: (cd user_layer/parity_strategy && cargo build --release --locked)
      - run: cargo build --release --workspace --locked
      - run: BK_REQUIRE_DYLIB=1 cargo test --workspace --locked
      - uses: actions/setup-node@v6
        with:
          node-version: 22
      - run: node scripts/binary-size-check.mjs --verbose
      - name: Version guard (the full check)
        run: node scripts/version-guard.mjs

  # ── 构建并发布 ────────────────────────────────────────────────────────────
  publish:
    name: publish
    needs: [guard, gates]
    runs-on: ubuntu-latest
    # 仅此作业可写。范围最小化，且它不做判定 —— 判定已完成。
    permissions:
      contents: write
    strategy:
      fail-fast: false
      matrix:
        include:
          # 三个平台，资产名与 package-release.mjs 的产物对齐（0.21）。
          - runner: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - runner: macos-latest
            target: aarch64-apple-darwin
          - runner: macos-13
            target: x86_64-apple-darwin
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0      # build.rs 需要 .git 才能盖章；浅克隆会让它退化成 nogit
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - uses: Swatinem/rust-cache@v2

      # BLITZKRIEG_GIT_SHA 显式盖章：即便 checkou 出来的 .git 形状不同，
      # 产物也必须能指认自己的修订号（build_info 的既有约定）。
      - name: Build (release)
        env:
          BLITZKRIEG_GIT_SHA: ${{ github.sha }}
          BLITZKRIEG_BUILD_DATE: ${{ github.run_started_at }}
        run: cargo build --release --workspace --locked --target ${{ matrix.target }}

      - name: Assemble the archive
        env:
          VERSION: ${{ needs.guard.outputs.version }}
          TARGET: ${{ matrix.target }}
        run: |
          set -eu
          STAGE="blitzkrieg-${VERSION}-${TARGET}"
          mkdir -p "$STAGE/bin"
          # 必需产物：内核。可选产物存在则打包（与 package-release.mjs 同一分类）。
          cp "target/${TARGET}/release/blitzkrieg-core" "$STAGE/bin/"
          for b in blitzkrieg ui_kit_web ui_kit_panel ui_kit_app; do
            [ -f "target/${TARGET}/release/$b" ] && cp "target/${TARGET}/release/$b" "$STAGE/bin/" || true
          done
          cp LICENSE "$STAGE/" 2>/dev/null || true
          tar -czf "${STAGE}.tar.gz" "$STAGE"
          shasum -a 256 "${STAGE}.tar.gz" | awk '{print $1"  '$STAGE'.tar.gz"}' > /tmp/sums.txt

      - name: GPG-sign the archive (skipped when the secret is absent)
        id: gpg
        env:
          GPG_PRIVATE_KEY: ${{ secrets.RELEASE_GPG_PRIVATE_KEY }}
        run: |
          set -eu
          if [ -z "${GPG_PRIVATE_KEY:-}" ]; then
            # 签名不可用**不**让发布失败（默认策略是推荐而非必需，见 §0.2），
            # 但必须留下可见的说明，而不是静默降级。
            echo "::warning::RELEASE_GPG_PRIVATE_KEY is not configured — publishing UNSIGNED (SHA256SUMS only)"
            echo "signed=0" >> "$GITHUB_OUTPUT"
            exit 0
          fi
          echo "$GPG_PRIVATE_KEY" | gpg --batch --import
          STAGE="blitzkrieg-$(echo '${{ needs.guard.outputs.version }}')-$(echo '${{ matrix.target }}')"
          gpg --batch --yes --detach-sign --armor "${STAGE}.tar.gz"
          echo "signed=1" >> "$GITHUB_OUTPUT"

      - uses: actions/upload-artifact@v5
        with:
          name: dist-${{ matrix.target }}
          path: |
            *.tar.gz
            *.tar.gz.asc

  # ── 汇总：把三个平台的产物合成一份 Release ──────────────────────────────
  release:
    name: release
    needs: [guard, publish]
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/download-artifact@v5
        with:
          path: dist
          merge-multiple: true
      - name: Build SHA256SUMS over everything
        run: |
          set -eu
          cd dist
          # 一行一文件，`<hex>  <name>` —— 与 scripts/lib/upgrade-artifacts.sh 的
          # SHA256SUMS 形态一致，也与 install 的 expected_sha256 解析一致。
          shasum -a 256 *.tar.gz | awk '{print $1"  "$2}' | sort -k2 > SHA256SUMS
          echo "--- SHA256SUMS ---"; cat SHA256SUMS
      - name: Refuse to publish an empty or duplicate Release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          set -eu
          # 同一 tag 重复推送时，绝不用新产物覆盖已发布的字节 ——
          # 一个「0.2.2」必须永远指同一批文件，否则校验与回溯全是空的。
          if gh release view "$GITHUB_REF_NAME" >/dev/null 2>&1; then
            echo "::error::release $GITHUB_REF_NAME already exists; refusing to overwrite artifacts"
            echo "        cut a NEW tag (v0.2.3) instead of republishing a released version"
            exit 1
          fi
      - name: Create the Release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          set -eu
          PRERELEASE=""
          case "$GITHUB_REF_NAME" in *-rc.*) PRERELEASE="--prerelease" ;; esac
          NOTES=$(awk -v v="${GITHUB_REF_NAME#v}" '
            $0 ~ "^## \\["v"\\]" {on=1; next}
            on && /^## \[/ {exit}
            on {print}
          ' CHANGELOG.md)
          if [ -z "$NOTES" ]; then
            echo "::error::no CHANGELOG section for $GITHUB_REF_NAME — the release notes would be empty"
            exit 1
          fi
          gh release create "$GITHUB_REF_NAME" \
            --title "BlitzkriegBot ${GITHUB_REF_NAME#v}" \
            --notes "$NOTES" $PRERELEASE \
            dist/*.tar.gz dist/*.asc dist/SHA256SUMS 2>/dev/null || \
          gh release create "$GITHUB_REF_NAME" \
            --title "BlitzkriegBot ${GITHUB_REF_NAME#v}" \
            --notes "$NOTES" $PRERELEASE \
            dist/*.tar.gz dist/SHA256SUMS
```

### 8.4 tag 与分支保护（手动配置，不进 YAML）

| 对象 | 规则 |
|---|---|
| `main` | 禁止直推；仅 `release/*`、`rc/*` 可经 PR 合并；必须 `guard`+`gates` 全绿；禁止 force push |
| `release/0.2` | 禁止直推；仅 `fix/*`、`chore/*` 可经 PR 合并；禁止 force push |
| tags `v*` | 禁止删除；禁止 force push（一个已发布的版本号不可回收） |

> 这些是 GitHub 仓库设置，不是仓库文件，因此**无法靠 CI 自我验证**。V7-1 的任务里含「逐项截图/`gh api` 导出确认」这一步，因为「文档说设了」不等于设了。

### 8.5 版本守卫脚本 `scripts/version-guard.mjs`（守卫与本地门禁共用一份）

```js
#!/usr/bin/env node
/**
 * 版本一致性守卫（VERSIONING.md §3 / §8）。
 *
 * 四件事，每一件都能单独红：
 *   1. 根 Cargo.toml 有 [workspace.package].version，且是合法 semver；
 *   2. 每个 workspace 成员要么继承（version.workspace = true），要么在
 *      允许清单里（构建工具 / 独立嵌套 workspace）；
 *   3. cargo metadata 解析出的所有成员版本 === 根版本；
 *   4. 仓库里没有第二处字面量版本号（README/CHANGELOG 里的历史叙述除外）。
 *
 * 用法：
 *   node scripts/version-guard.mjs                # 全量检查
 *   node scripts/version-guard.mjs --manifest-only # 只查清单（CI 的 tag 守卫用）
 *
 * 退出码：0 全绿 · 1 有不一致 · 2 用法/环境错误
 */
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const manifestOnly = args.includes('--manifest-only');

let failures = 0;
const check = (label, fn) => {
  try { fn(); console.log(`  ok   ${label}`); }
  catch (e) { failures++; console.log(`  FAIL ${label}\n       ${e.message}`); }
};

const read = (...p) => readFileSync(join(ROOT, ...p), 'utf8');

/** 允许不继承版本的 crate：构建工具与独立嵌套 workspace。理由写在旁边。 */
const NO_INHERIT = new Set([
  'build_support',      // 构建期工具，不是产品的一部分
  'blitzkrieg-webapp',  // 独立嵌套 workspace（tauri shell），无法继承根 [workspace.package]
]);

// ── 1. 根清单 ────────────────────────────────────────────────────────────────
const rootManifest = read('Cargo.toml');
const wsPkg = rootManifest.match(/^\[workspace\.package\]([\s\S]*?)^\[/m)?.[1] ?? '';
const version = wsPkg.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1] ?? null;

check('根 Cargo.toml 有 [workspace.package].version', () => {
  if (!wsPkg) throw new Error('no [workspace.package] table — the single source does not exist yet');
  if (!version) throw new Error('no version key inside [workspace.package]');
});

check('版本号是合法 semver（可带 -rc.N）', () => {
  if (!/^\d+\.\d+\.\d+(-rc\.\d+)?$/.test(version ?? '')) {
    throw new Error(`not <major>.<minor>.<patch>[-rc.N]: ${JSON.stringify(version)}`);
  }
});

if (manifestOnly) {
  console.log(failures ? `\nversion-guard: ${failures} failure(s)` : '\nversion-guard: ok');
  process.exit(failures ? 1 : 0);
}

// ── 2. 成员清单：成员要么继承，要么在允许清单里 ────────────────────────────
const members = [...rootManifest.matchAll(/^\s*"((?:core|ui|extensions|user_layer)\/[\w\-/]+)",\s*$/gm)]
  .map((m) => m[1]);

check('每个 workspace 成员都从工作区继承版本', () => {
  const offenders = [];
  for (const m of members) {
    let text;
    try { text = read(m, 'Cargo.toml'); } catch { continue; }   // 不存在=别的检查负责
    const name = text.match(/^name\s*=\s*"([^"]+)"/m)?.[1] ?? m;
    if (NO_INHERIT.has(name)) continue;
    if (!/^\s*version\.workspace\s*=\s*true/m.test(text)) offenders.push(`${m} (${name})`);
  }
  if (offenders.length) {
    throw new Error(`members with a literal version: ${offenders.join(', ')}\n       use \`version.workspace = true\``);
  }
});

check('没有成员还写着字面量版本号', () => {
  const offenders = [];
  for (const m of members) {
    let text;
    try { text = read(m, 'Cargo.toml'); } catch { continue; }
    const name = text.match(/^name\s*=\s*"([^"]+)"/m)?.[1] ?? m;
    if (NO_INHERIT.has(name)) continue;
    const lit = text.match(/^\s*version\s*=\s*"([^"]+)"/m);
    if (lit) offenders.push(`${m} = ${lit[1]}`);
  }
  if (offenders.length) throw new Error(offenders.join(', '));
});

// ── 3. cargo metadata 的实际解析结果 ────────────────────────────────────────
check('cargo 解析出的每个成员版本 === 根版本', () => {
  const meta = JSON.parse(execFileSync('cargo',
    ['metadata', '--no-deps', '--format-version', '1'], { cwd: ROOT, encoding: 'utf8' }));
  const bad = meta.packages
    .filter((p) => !NO_INHERIT.has(p.name))
    .filter((p) => p.version !== version)
    .map((p) => `${p.name}=${p.version}`);
  if (bad.length) throw new Error(`expected all = ${version}: ${bad.join(', ')}`);
});

// ── 4. 运行时自述必须与仓库一致（二进制存在时才查）──────────────────────────
check('target/release/blitzkrieg 的自述版本与仓库一致', () => {
  const bin = join(ROOT, 'target', 'release', 'blitzkrieg');
  let out;
  try { out = execFileSync(bin, ['version', '--json'], { encoding: 'utf8', timeout: 20_000 }).trim(); }
  catch { console.log('  note binary not built — run `cargo build --release --workspace --locked` to enable this check'); return; }
  const got = JSON.parse(out).version;
  if (got !== version) throw new Error(`binary says ${got}, manifest says ${version} — rebuild`);
});

console.log(failures ? `\nversion-guard: ${failures} failure(s)` : '\nversion-guard: ok');
process.exit(failures ? 1 : 0);
```

### 8.6 把版本守卫接进既有 CI（小改动，别新开工作流）

`.github/workflows/ci.yml` 的两处最小改动：

```yaml
# 改动 1：release/* 的推送也跑 CI（现状只跑 main/develop）
on:
  push:
    branches: [main, develop, 'release/**']
  pull_request:

# 改动 2：rust-check 作业里加一步（在 fmt/clippy 之后，不需要重新构建）
      - name: Version guard — one source of truth
        run: node scripts/version-guard.mjs --manifest-only
```

> `--manifest-only` 用在 PR 门禁里：PR 阶段往往没构建 release 二进制（构建要几分钟），而清单一致性是纯文本检查、毫秒级。完整检查（含二进制自述）留给发布流水线的 `gates` 作业。

---

## 9. 执行顺序与任务拆分（Epic 级别）

排序原则：**先把「不可能不一致」的结构立起来，再做 UI 与更新**。E-V1 不做完就做 E-V4，会得到「三个地方各自显示版本号」的现状加强版。

### E-V1 版本号单事实来源（P0，其余全部依赖它）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V1-1 | 根清单成为唯一来源 | 加 `[workspace.package].version = "0.2.1"`；8 个成员改 `version.workspace = true`；`--locked` 下的 `Cargo.lock` 同步提交 | — |
| V1-2 | 注入逻辑只有一份 | 新建 `core/build_support`（`publish = false`）；把 core 现有 `build.rs` 的 git 逻辑迁入，保留 `BLITZKRIEG_GIT_SHA` / `_GIT_DIRTY` 两个既有名字 | V1-1 |
| V1-3 | 运行期常量唯一出口 | 新建 `core/build_info` + 1 行 `build.rs`；`BuildInfo` / `BUILD_INFO` / `version_string()` / `provenance_line()` / `to_json()` + 6 个单测 | V1-2 |
| V1-4 | 接线且不破坏既有契约 | `lib.rs` 的 `CORE_VERSION` 指向 `BUILD_INFO`；`ipc/build_info.rs` 改再导出；**验证 4 个既有调用点**（READY / `--version` / 启动横幅 / data_lock 测试）行为不变；`core-provenance.mjs` 的门禁仍绿 | V1-3 |
| V1-5 | 守卫可执行化 | `scripts/version-guard.mjs` + 接进 `ci.yml`；`release/**` 纳入 CI 触发 | V1-4 |

### E-V2 `blitzkrieg version` 命令（P0）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V2-1 | 堵住启动器的 `--version` 坑 | `tokio_main` 新增 `--version`/`-V`/`-v` 短路（单行 `<semver>+g<sha>`，退出码 0）；加一条测试断言它**不**启动任何进程（现状 0.8） | V1-4 |
| V2-2 | `version` 子命令 | `run_version_subcommand`（`--json` / `--core` / `--socket` / `--help`）；退出码 0/1/2 三分；`HELP_TEXT` 补一行 | V2-1 |
| V2-3 | IPC 客户端方法 | `IpcClient::system_version()` + `SystemVersionView`（含 `update_available: Option<bool>` 三态） | V1-4 |
| V2-4 | 格式钉住 | 三个格式（人读文本 / `--json` / `--version` 单行）各写一条快照式测试；`--json` 的键名测试 | V2-2, V2-3 |

### E-V3 `system.version` IPC（P0）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V3-1 | 常量与载荷 | `schema.rs` 加 `SYSTEM_VERSION`；`ipc/version.rs` 的 `SystemVersion` / `system_version_payload()`（纯函数 + 单测覆盖三态） | V1-4 |
| V3-2 | 分发（不取 Core 锁） | `server.rs` 加分支，读独立的 `UpdateState`；验证「数据锁未就绪时也能回答」 | V3-1 |
| V3-3 | 契约文档 | `docs/rust-core/INTERFACES.md` 的 `§2.1 基础` 补方法条目 + `§4 版本变更记录` 补一行（新增非破坏） | V3-2 |
| V3-4 | Node 侧消费 | 门禁脚本用 `system.version` 打印内核自述（复用 `core-provenance.mjs` 的既有判据） | V3-2 |

### E-V4 更新检查（P1）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V4-1 | semver 比较（唯一比较点） | `parse_release_tag` / `is_newer` + 反向测试（`0.10.0 > 0.9.0`、rc 不打扰正式版、垃圾输入不产生提示） | V3-1 |
| V4-2 | 出网与状态 | `reqwest` 直连依赖；`check_once(fetch)` 注入式设计；`UpdateState` + 持久化到 `data/update/state.json`（原子写） | V4-1 |
| V4-3 | 启动挂载（默认静默） | `spawn_startup_check`：`checkEnabled=false` 时**不 spawn**；开关来源 CLI > env > `configs/update.toml` > 默认 false | V4-2 |
| V4-4 | 两个 IPC 动词 | `system.update.check` / `system.update.configure`（含审计、落盘失败必须报错） | V4-3 |
| V4-5 | 出网零泄漏验收 | 7.6 的 A1/A2/A3/A4/A5/A6/A13 全部落地为可跑断言 | V4-4 |

### E-V5 TUI 设置页（P1）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V5-1 | 第 6 个 Tab | `Tab::Settings` + `titles/index/next` 三处同步；快捷键 `6`/`settings`；主循环低频拉 `system_version` | V3-2 |
| V5-2 | `render_settings` | 版本块（4 行）+ 更新块（三态徽章 / 上次检查 / 两个开关）+ 键位提示 | V5-1 |
| V5-3 | 键位与确认 | `c` 检查（关闭时明确文案）、`a` 切换（复用 `render_confirm` 弹确认）、`i` 安装（仅自动更新开启时可用） | V5-2 |

### E-V6 WebUI 设置页（P1，与 E-V5 同批）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V6-1 | 客户端方法与类型 | `client.ts` 加 `systemVersion()` / `updateCheck()` / `updateConfigure()` + `SystemVersion` 接口 | V3-2 |
| V6-2 | `lib/version.ts` | `versionBadge()` 三态实现（与 TUI 判据同源）；`readVersion()` 容错（旧内核 → 明确错误，不画空版本） | V6-1 |
| V6-3 | 版本与更新卡片 | `SettingsPage.vue` 第 5 块；复用既有 Card/Badge/Button/Switch/Tooltip/AlertBanner/EmptyState，**不新增依赖** | V6-2 |
| V6-4 | 对等门禁同步 | `tui-parity.check.mjs` 的三处集合扩展 + 新增对等断言（否则门禁必红，见 0.14） | V6-3 |
| V6-5 | WebUI 回归检查 | 新增 `scripts/version-panel.check.mjs` 并加进 `package.json` 的 `check:all`（三态文案、旧内核容错、开关默认关闭） | V6-4 |

### E-V7 下载 / 校验 / 安装（P1）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V7-1 | 资产名与摘要解析 | `asset_name` / `expected_sha256`（缺条目 = 拒绝）+ 单测 | V4-2 |
| V7-2 | SHA256 校验 | `verify_sha256` + 篡改 1 字节的测试（必须 Err 且目标未变） | V7-1 |
| V7-3 | GPG（可选但可分辨） | `SignatureVerdict` 三态；`Invalid` **永远拒绝**；`require_signature` 默认 false；缺 `gpg` 大声警告 | V7-2 |
| V7-4 | 原子替换 | `atomic_replace`（同目录 tmp → chmod → rename → 读回自证）+ 备份 `*.prev` | V7-3 |
| V7-5 | `blitzkrieg update` 动词 | `--check` / `--install` / `--yes-while-holding`；有持仓无标志 → 拒绝；**不自动重启** | V7-4 |
| V7-6 | 对抗性验收 | 7.6 的 A7–A12、A14 全部落地 | V7-5 |

### E-V8 发布流水线与文档（P1，最后）

| 编号 | 目标 | 关键任务 | 依赖 |
|---|---|---|---|
| V8-1 | 分支收敛 | ① `main` ff 到 `ceer/main`（0/3 局面）；② 建 `release/0.2`；③ 存量 118 个分支**逐个分类**：合过则删、未合且无价值则列清单让操作者拍板（**绝不批量删**） | — |
| V8-2 | `release.yml` | tag 守卫 / 门禁 / 三平台构建 / `SHA256SUMS` / GPG 可选 / Release 去重 | E-V1..V7 |
| V8-3 | 分支与 tag 保护 | GitHub 设置层逐项确认（含 `gh api` 导出证据） | V8-1 |
| V8-4 | 首个 tag | `v0.2.1`：走 8.2 的完整七步 | V8-2, V8-3 |
| V8-5 | 预发布路径打通 | 改 `core-provenance.mjs` 的 `VERSION_RE` 接受 `-rc.N`；加 rc 的 CI 用例（**不发 rc 之前必须做完**，见 0.7） | V8-4 |
| V8-6 | 外部版本同步门禁 | `version-guard.mjs` 加一条：`src-tauri/Cargo.toml` 与 `tauri.conf.json` 的 `version` 必须相等（两处都可能被漏改） | V8-2 |
| V8-7 | 文档 | 本文件 + `README.md` 的 §6 分支模型与 §7 文档表更新；`INTERFACES.md` 契约条目（V3-3 已含，此处只核对） | 全部 |

### 9.1 最小可用路径（如果只做一半）

只做 **E-V1 → E-V2 → E-V3 → E-V6-4**：得到「版本号唯一、`blitzkrieg version` 可用、UI 能显示、门禁一致」，**不含任何更新能力**。这条路径不引入出网代码，风险最低，且已经解决了需求里最痛的部分（分支混乱与无版本号）。更新机制（E-V4/V7）可以在任何后续时间点独立加入，因为它不改变上面的任何契约。

---

## 10. 验收标准（含反向验收测试）

### 10.1 正向验收

| # | 断言 | 手段 |
|---|---|---|
| F1 | `cargo metadata` 里默认成员版本全部相等且等于根版本 | `node scripts/version-guard.mjs` |
| F2 | `blitzkrieg version --json` 的 `version` 与 F1 相同 | 脚本解析 |
| F3 | `blitzkrieg version --core --json` 的 `version`/`gitHash` 与运行中内核一致 | 起内核后解析 |
| F4 | `blitzkrieg --version` 输出单行且匹配 `<semver>+g<sha>`，退出码 0，**未启动任何进程** | 断言 stdout 单行 + `pgrep blitzkrieg-core` 为空 |
| F5 | `system.version` 的 12 个字段齐全且类型正确 | JSON schema 断言 |
| F6 | TUI 第 6 个 tab 显示版本 4 项 + 更新三态 | `tui-demo-check.mjs` 风格的 PTY 断言 |
| F7 | WebUI 设置页第 5 块显示同 4 项 + 三态徽章 | `version-panel.check.mjs` |
| F8 | TUI/WebUI 对等门禁绿 | `npm run check:parity` |
| F9 | 检查更新能发现新版本（用可控的假 API） | 注入假 body |
| F10 | 安装后二进制自述为新版本，备份文件存在 | 端到端脚本 |
| F11 | `git tag -l` 有 `v0.2.1`，且 `v0.2.1` 与 `Cargo.toml` 一致 | `git tag` + 守卫 |

### 10.2 反向验收（**改坏实现，测试必须变红**）

这是本节的硬要求：每条都必须「故意改坏 → 断言测试红 → 改回 → 断言测试绿」。只写正向测试的验收不算通过。

| # | 故意改坏 | 必须变红的东西 | 为什么这条重要 |
|---|---|---|---|
| N1 | 把某个成员的 `version.workspace = true` 改回 `version = "9.9.9"` | `version-guard.mjs` 的成员检查 + `cargo metadata` 比对 | 唯一来源的头号敌人就是「顺手写一个」 |
| N2 | 把根版本改成 `0.2.5` 但不重建 | `version-guard.mjs` 的第 4 项（二进制自述 vs 仓库） | 证明「盘上/源码」两处真的在比对，而不是各自自言自语 |
| N3 | 把 `version_string()` 的格式改成 `<semver> <sha>`（空格代替 `+`） | `core-provenance.mjs` 的 `VERSION_RE` 门禁 + `build_info` 的格式单测 | 这是 #179 的原始契约，破了会让所有门禁失去「在测哪份代码」的能力 |
| N4 | 让 `BUILD_INFO.version` 硬编码成 `"0.2.1"`（不走 `env!`） | V1-3 的「不是占位符」单测在 bump 后仍绿 —— 所以**额外**需要一条 diff 检查：`build_info/src/lib.rs` 中不得出现 `"0.` 字面量 | 硬编码是唯一来源最隐蔽的破坏方式：它让所有测试继续绿 |
| N5 | `system_version_payload` 把「未检查」写成 `Some(false)` | `the_payload_is_three_state` 单测 + WebUI 徽章文案断言 | 把「不知道」画成「已是最新」是这份设计最想消灭的谎言 |
| N6 | `is_newer` 改成字符串比较 | `comparison_is_semver_not_lexicographic`（`0.10.0` vs `0.9.0`） | 字符串比较会静默地永远不提示 0.10 系 |
| N7 | `spawn_startup_check` 去掉 `check_enabled` 早退 | `a_disabled_check_makes_no_request`（future 被 await 即 panic）+ A1 的出网断言 | 「默认关闭」必须是结构性事实，不是 if 写对了 |
| N8 | `verify_sha256` 改成只比前 8 位 | 篡改第 9 位的测试必须红 | 短比较看起来「也验了」，实际形同虚设 |
| N9 | 把 `Invalid` 签名当作 `Unavailable` 处理 | A8（坏签名必须拒绝） | 「验不了」与「验错了」是两个完全不同的结论 |
| N10 | `expected_sha256` 在找不到条目时返回 `Some("")` 或 `None`→跳过 | A10 | 缺摘要时「跳过着校验」是最常见的降级路径 |
| N11 | `atomic_replace` 改成直接 `fs::write` 到目标 | A11（中途失败留下半截二进制） | 原子性是这条链路上唯一能救回一个坏下载的东西 |
| N12 | 去掉 `--yes-while-holding` 检查 | A14 | 有持仓时静默换二进制 = 用未验证的代码接管未平仓风险 |
| N13 | `release.yml` 的守卫删掉「tag == v + manifest」比对 | 故意打 `v0.2.9`（清单写 0.2.1）→ 发布必须失败 | tag 与产物自述不一致，事后无法从产物发现 |
| N14 | Release 重复发布改成覆盖 | 同一 tag 二次运行 → 必须失败 | 一个版本号必须永远指向同一批字节 |
| N15 | TUI 加了 `Settings` 但不动 `tui-parity.check.mjs` | `npm run check:parity` 必须红（未知 tab 集合） | 证明对等门禁真的在拦「悄悄多一面」 |
| N16 | WebUI 徽章把 null 画成「已是最新」 | `version-panel.check.mjs` 的三态断言 | 同 N5，但失败发生在 UI 侧 —— 两侧都要能红 |

### 10.3 验收的元规则

1. **每条反向验收必须实际跑过**：在 PR 描述里附上「改坏后的失败输出」片段。没附 = 未验证。
2. **不许为了绿而放宽断言**：如果某条反向验收做不到红，说明它测的东西没有守护者 —— 补守护者，不要删断言。
3. **区分「测试通过」与「行为正确」**：N4 就是例子（硬编码能让所有单测继续绿），所以需要 diff 层面的检查。
4. **不只跑单测**：`cargo test` 不会发现 `version-guard.mjs` 漏了一个成员。开工单里每一层的门禁都要跑。
5. **涉及出网与替换的验收，必须在隔离环境跑**：7.6 的 A1/A7/A8/A11 会真的下载与替换文件；用临时目录 + 假资产，**绝不拿生产二进制试**。

---

## 11. 禁止事项

### 11.1 版本与构建

| # | 禁止 | 理由 |
|---|---|---|
| P1 | 在任何 crate 的 `Cargo.toml` 里写死 `version = "..."` | 唯一来源立刻变成两处；N1 就是这条的守卫 |
| P2 | 在 `Cargo.toml` 里写 `+g<sha>` 之类的构建元数据 | cargo 不接受这种版本号；`+` 后缀只属于运行时的自述字符串 |
| P3 | 把版本号或 sha 硬编码进 Rust 源码 | 最隐蔽的漂移方式（N4）。必须经 `env!()` 取自盖章 |
| P4 | 修改 `version_string()` 的格式而不改 `core-provenance.mjs` | 所有门禁失去「在测哪份代码」的能力（#179 的原始事故） |
| P5 | 让 `build.rs` 因拿不到 git 而构建失败 | 源码 tarball / 无 git 的镜像必须能构建；`nogit` 是可见的诚实答案 |
| P6 | 让 `build_info` 引入 serde 或任何运行期依赖 | 它会被内核与启动器同时链接；零依赖是它可被任何进程安全使用的前提 |

### 11.2 更新机制

| # | 禁止 | 理由 |
|---|---|---|
| P7 | 让更新检查默认开启 | INV-3；默认配置下启动一个栈必须零出站连接 |
| P8 | 在检查关闭时「顺手」发一个请求（包括匿名遥测式的 GET） | 关闭的语义是**一个包都不发**，不是「发得少一点」 |
| P9 | 跳过 SHA256 校验，或摘要缺失时降级为「不校验」 | 这是安装链路上唯一能拦住篡改/损坏下载的东西 |
| P10 | 把 `SignatureVerdict::Invalid` 当作 `Unavailable` | 坏签名 ≠ 没签名（N9） |
| P11 | 把下载的字节直接 `write` 到正在执行的目标路径 | 中途失败会留下半截二进制，且正在运行的进程内存里的页可能被换掉（N11） |
| P12 | 由**内核**下载或替换二进制 | 持仓进程改写正在执行的二进制是自伤路径；安装归启动器 |
| P13 | 安装后自动重启栈 | 重启会立刻改变持仓状态；这是操作者的决定，不是安装的副作用 |
| P14 | 有持仓时无显式确认就替换二进制 | N12/A14 |
| P15 | 收集或上报任何环境/凭证信息到更新端点 | 更新检查只读一个 release 元数据；任何其他数据外流都是另一件事，需要另一份设计 |
| P16 | 把 GitHub 令牌写进 URL 或日志 | 用 header；令牌走 `BLITZKRIEG_GITHUB_TOKEN` 环境变量，绝不入库 |

### 11.3 分支与发布

| # | 禁止 | 理由 |
|---|---|---|
| P17 | 在 `main` 上直接提交或强推 | 分支模型的基础；违反即失去「main 是可发布状态」的保证 |
| P18 | 从 `feat/*` / `fix/*` 直接打 tag | tag 必须落在 `release/*` 或 `main`；否则发布的是「没有分支拥有」的代码（V8-2 的守卫） |
| P19 | 复用已发布的 tag（删除后重打、或覆盖 Release 资产） | 一个版本号必须永远指向同一批字节（N14） |
| P20 | 批量删除存量分支（如 `git branch | grep -v main | xargs -r git branch -D`） | 仓库里有未合并的工作；分支收敛必须逐个分类 + 人工确认（V8-1） |
| P21 | 新增 `release/0.2-fix`、`release/0.2.1` 之类的分支 | 一条线只对应一个 `release/<major>.<minor>`；同线多分支会让「哪个是权威」变成考古 |
| P22 | 让 tag 触发的工作流复用 `contents: write` 给所有作业 | 权限最小化；只有 `publish`/`release` 需要写 |

### 11.4 范围红线（本次明确不动）

| # | 禁止 | 理由 |
|---|---|---|
| P23 | 改动任何交易逻辑、风控逻辑、策略逻辑 | 需求原文的硬约束。本方案触碰的最敏感代码是 `server.rs` 的 `match`（新增只读分支）与 `lib.rs` 的常量再导出 |
| P24 | 改动 `PROTOCOL_VERSION` | `system.version` 是纯新增方法，不动信封（5.4 的 R2） |
| P25 | 改动 `data_lock` 的锁语义、`risk.*` 的任何行为、`strategy.*` 的任何行为 | 非本次范围；`data_lock.rs` 只被测试断言触及，不改实现 |
| P26 | 引入新的重量级依赖（如完整的 HTTP 客户端 + TLS 栈之外的东西） | `reqwest`/`sha2`/`semver` 都已在 `Cargo.lock` 里（0.12），新增**直接**依赖不扩大依赖树；此外一律不开新面 |
| P27 | 改动 `ui/webapp/src-tauri` 与策略嵌套 workspace 的构建方式 | 它们是刻意独立的（0.24）；只加版本同步检查，不改它们的结构 |

---

## 12. 版本记录

### 12.1 本文档自身的版本

| 文档版本 | 日期 | 变更 |
|---|---|---|
| 1.0 | 2026-09-25 | 初版：E-V1..E-V8 设计与验收。基于对当前仓库的逐项核对（第 0 节，26 项），并实测验证三条依赖机制（0.1） |

### 12.2 适用于本文档的版本线

| 版本 | 语义 | 本文档的对应状态 |
|---|---|---|
| `0.2.0` | 已发布（实盘链路已验证）；**无 tag**（0.25） | 历史基线，本文档不追溯 |
| `0.2.1` | 新体系下第一个 patch：本方案的 E-V1..V3 落地（版本单来源 + `version` 命令 + `system.version`） | 建议作为 **V8-4 的首个 tag** |
| `0.2.2` | 文档（`docs/VERSIONING.md` / `README.md` / `INTERFACES.md`）+ 守卫脚本（`version-guard.mjs`）+ 对等门禁 | 可与 0.2.1 合并发布，视 PR 粒度而定 |
| `0.2.3` | TUI/WebUI 设置页（E-V5/V6） | UI 变更单独一个 patch，便于回滚 |
| `0.2.4` | 更新检查（E-V4）——**不引入安装能力**，风险最低的一半 | 先上「能看见版本与更新」，再上「能替换文件」 |
| `0.2.5` | 下载/校验/安装（E-V7）+ 发布流水线（E-V8） | 安装能力与发布流水线同批，因为前者需要后者产出的校验资产 |
| `0.3.0` | 下一条功能线（单数 minor）：从 `main` 开 `feat/*`，走 RC 路径 | 本文档定义的模型在 0.3 上第一次被完整使用（含 `-rc.N` 与 V8-5 的正则修正） |

### 12.3 变更记录（本文档约定：破坏性变更必须撞大版本并写在这里）

| 日期 | 变更 | 影响 |
|---|---|---|
| 2026-09-25 | 建立：`<semver>+g<sha>` 自述格式被定为门禁契约 | 与 `scripts/lib/core-provenance.mjs` 的 `VERSION_RE` 双向绑定；改任一侧必须同时改另一侧（V8-5） |
| 2026-09-25 | 建立：`system.version` 的 `updateAvailable` 三态 | WebUI 徽章与 TUI 状态行都依赖 `null ≠ false` |
| 2026-09-25 | 建立：`<semver>+g<sha>` **不带** `-dirty` 后缀，脏状态走 `gitDirty` 布尔 | 保持版本串可逐字节比较（`build.rs` 已有注释记录这一决定） |
| 2026-09-25 | 建立：更新检查/自动更新默认关闭，且关闭时**零出站连接** | 任何降低这项保证的改动都必须在 12.3 留一行，并在 10.2 补一条反向验收 |
| 2026-09-25 | 建立：安装由启动器执行，内核永不替换自身二进制 | 内核与启动器的职责边界；越界即为 P12 |

### 12.4 待决项（已知但未拍板，执行时不要擅自决定）

| # | 待决 | 影响面 | 建议 |
|---|---|---|---|
| D-V1 | 首个 tag 是 `v0.2.1` 还是补发 `v0.2.0` | 历史叙述；`v0.2.0` 无对应提交（0.25） | 建议 `v0.2.1`，本文档按此写 |
| D-V2 | 是否给 `ui/webapp/src-tauri` 的版本跟随主线 | tauri 安装包的版本号 | 建议先只加同步门禁（V8-6），等要发桌面安装包时再引入环境变量注入 |
| D-V3 | `--json` 的字段名是 camelCase 还是 snake_case | 契约的每个消费者 | 建议跟随仓库既有合同（camelCase）；若操作者坚持 snake_case，改 serde 属性 + WebUI 类型 + 5.3 表格 |
| D-V4 | 发布三个平台还是先只发 macOS arm64 | 流水线复杂度 | 建议先全量，因为矩阵里多一个平台只多一行 YAML，而少一个平台会在「别人装不上」时才被发现 |
| D-V5 | 存量 118 个分支的处置清单 | 仓库整洁度 | 见 V8-1：逐个分类、人工确认；本文档不预判任何一个分支的去留 |


---


