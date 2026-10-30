# env 唯一可信化改造 — 配置面盘点与迁移判决（第一阶段）

> 操作员裁决（2026-10-23 出厂四键退役 PR-1 + 2026-10-24 重申）：全部配置统一到
> `.env` 全局唯一可信源。原话：「重写 env 删除不再需要的老设置 然后开始 env
> 唯一可信化改造 我觉得 toml 和 json 文件可以被删除了」。
>
> 本文档是第一阶段的盘点与判决书。阶段目标：env-first 通路全部接通（PR-1/#503
> 已完成大半）、`.env.example` 与代码全集零漂移、死键有 grep 证据可删、每个
> 配置文件的去向有明确判决。**live `.env` 本身的重写不在本 PR 内**——那是
> 生产升级时由主窗口执行的动作（`.env` 是 git-ignored 的操作员私有文件）。

---

## 1. 配置面盘点表（文件 → 判决）

| 文件 | 身份 | 判决 | 依据 |
| --- | --- | --- | --- |
| `.env`（git-ignored） | **唯一可信配置源**（凭证 + 运行键） | **保留并加冕**；生产重写由主窗口在升级时执行 | 本文档 §3 逐键判定 |
| `.env.example` | env 全集模板（随发布打包） | **保留，本 PR 补齐漂移**（§5） | `scripts/package-release.mjs:267` |
| `user_layer/configs/default.toml` | 内核配置文件壳（出厂四键已于 #588 退役，现零显式键） | **退役为纯注释文档**（2026-10-28）：不再含任何有效配置行；引擎配置走 .env 的 BK_* 变量，exit/risk 保持编译默认。文件缺失内核照常启动（`FileConfig::load` 对 NotFound 回落编译缺省） | PR-4 完成：文档壳退役为注释-only；读取点保留（缺失容忍） |
| `user_layer/configs/accounts.toml` | **账户簿持久态 + 账户合同**（`[[account]]`、status 姿态、strategies 启用集、rounds 族、`[cross_venue_arb]` 腿） | **保留，绝不迁 .env**：面板热写回（`account.venue.set` / `account.rounds.set` / `account.strategies.set`）以它为持久化目标——`.env` 是「人写、进程只读」的文件，进程写回的键放 `.env` 会制造第二个可信源。缺文件时内核以单一 `default` 账户照常启动（`core/blitzkrieg_core/src/account/mod.rs:334` `single_default`） | §6 处置提案 |
| `user_layer/configs/shadow_evolution.toml` | 进化开关与参数；`enabled`/`auto_evolve` 两开关的**唯一可信源**（面板翻转写回本文件） | **保留**（写回型持久态，同 accounts.toml 理由）。`BK_SE_*` env 是它的上层覆盖，已在 `.env.example` 全列出 | `core/blitzkrieg_core/src/config.rs`（load 侧边文件） |
| `user_layer/configs/update.toml` | 更新检查出厂缺省（读序 CLI > `BLITZKRIEG_UPDATE_*` env > 此文件 > 内置） | **保留为出厂缺省面**；运行态在 `data/update/state.json`，env 面已存在 | `core/blitzkrieg_core/src/main.rs:1810-1835` |
| `user_layer/configs/execution_policy.toml` | 执行策略合同（账户级 sizing/门禁），`BUDGET_RATIO[_<ID>]`/`MAX_BUDGET_USD[_<ID>]` env 在其上层 | **保留**：per-account 复杂结构用 toml 表达；env 只覆盖两个标量（已补进 `.env.example`）。缺文件 = 中性内核（文件头明示） | `core/blitzkrieg_core/src/execution_policy.rs:65` |
| `extensions/binance_spot/config.toml` | 扩展结构声明（[meta] 与链接的 dylib 对账） | **保留为包合同** | `core/blitzkrieg_core/src/config.rs:566` |
| `user_layer/strategies_lua/*/manifest.json` | **策略包合同**，不是配置：`api` 版本握手、`entry` 入口、`sha256` 完整性、`tunables` 可进化域声明（Lock 0 的依据）、`modes` 市场声明 | **保留为包合同**——它描述包的身份与能力，如同 Cargo.toml 之于 crate；env 无法承载逐包声明 | `core/blitzkrieg_core/src/strategy_engine/lua_loader.rs:4` |
| `data/runtime/config.json` | 运行态投影（WebUI 写、内核读；现仅 `roundDurationSec` 白名单格） | **保留**：这是「面板点出来的持久意图」，与 `data/strategy-state.json` 同族；env 显式设置时它被压制（issue 499 语义） | `core/blitzkrieg_core/src/ipc/runtime_config.rs:1` |
| `data/update/state.json`、`data/strategy-state.json`、`data/evolution/state.json` | 运行态投影家族 | **保留**（同上） | — |
| `bin/` 下 shim | 无此目录；shim 是 `scripts/install-blitzkrieg-shim.sh` 生成的用户机文件（`~/.local/bin/blitzkrieg`） | 不属仓库配置面 | — |

## 2. launcher 与内核 CLI 旗标（盘点，无改动）

- **launcher 自加载**：`ui/ui_kit_panel/src/bin/blitzkrieg.rs:337` 在 tokio 启动前调
  `load_cwd_env()` 读 `<cwd>/.env`；既存环境变量永远赢过文件（dotenv 惯例，与内核
  CLI > env > file > default 读序一致，`ui/ui_kit_panel/src/env_file.rs:11`）。
- **launcher 旗标 env**（scripts/ 面全部真实在读）：`BLITZKRIEG_UPGRADE_SOURCE`、
  `BLITZKRIEG_UPGRADE_DEADLINE`（`scripts/upgrade.sh`）、`BLITZKRIEG_SHIM_DIR`
  （`scripts/install-blitzkrieg-shim.sh`）、`BLITZKRIEG_BACKUP_DIR`
  （`scripts/data-backup-cli.sh`）、`BLITZKRIEG_ALLOWED_ORIGINS`、
  `BLITZKRIEG_ALERT_NOTIFY`、`UIKIT_MANAGE`、`UIKIT_WEB_TRACE`、
  `UIKIT_CORE_BIN/CWD/EXTRA_ARGS`、`HFT_ASSETS/ROUND_SEC/MIN_SHARES/MAX_SHARES`
  （`ui/ui_kit/src/gateway/supervisor.rs`）。全部已入 `.env.example`。
- **内核 CLI 旗标**（`core/blitzkrieg_core/src/cli.rs`）：`--config/--no-config`、
  `--assets/--round-sec/--min-round-age/--min-time-left`、`--mode`、`--exit-*`、
  `--max-*` 风险族、`--shadow-evolution/--se-*`、`--enable/disable-strategy`、
  `--strategy-dir/--lua-strategy-dir`、回放/回测族等。旗标是 env 之上的最高层，
  不迁 env（一次性实验入口）。
- **内核 env 读点**：`BK_*` 28 键经 `EnvVars::from_process`（`main.rs:612`，只拷
  `BK_` 前缀）+ `DRY_RUN`（`main.rs:2749`）+ `BLITZKRIEG_GITHUB_TOKEN`
  （`ipc/server.rs:1003`）+ `BLITZKRIEG_ALLOW_SHARED_DATA`（`main.rs:3330`）+
  `BLITZKRIEG_UPDATE_CHECK/AUTO/INTERVAL_SECS`（`main.rs:1810-1835`）。
  凭证注入链：面板凭据卡写 `data/runtime/config.json` → 内核启动注入，真实 env
  永远赢（`main.rs:3492`）。全部通路已存在，**本阶段无需新增 Rust 接线**。

## 3. live `.env` 逐键判定（grep 证据）

对 live `.env` 全部 14 键逐一 `grep -rn` 全仓（排除 target/.git/node_modules）：

| 键 | 判定 | 证据 |
| --- | --- | --- |
| `POLYMARKET_PRIVATE_KEY` | **活键，保留** | `extensions/polymarket/src/venue.rs:300`、`redeem.rs:179`、`live.rs:50`；凭据脱敏扫描 `risk.rs:172` |
| `POLYMARKET_API_KEY` | **死键，删除** | 全仓 grep **零读点**（core/extensions/ui/scripts/user_layer 全空）。L2 凭据由私钥派生：`venue.rs:317` `create_or_derive_api_key(&signer, None)`（注释原文「so Node never needs API keys at all」）。Node 层遗留，随 0dc2e02 扫荡后无消费者 |
| `POLYMARKET_API_SECRET` | **死键，删除** | 同上，零读点 |
| `POLYMARKET_API_PASSPHRASE` | **死键，删除** | 同上，零读点 |
| `POLYMARKET_FUNDER_ADDRESS` | **活键，保留** | `extensions/polymarket/src/venue.rs:303` |
| `DRY_RUN` | **活键，保留** | `core/blitzkrieg_core/src/main.rs:2749`；`ui/ui_kit/src/gateway/supervisor.rs:398` |
| `HFT_CORE` | **死键，删除** | 全仓 grep 仅剩 `.github/ISSUE_TEMPLATE/bug_report.yml:50` 的说明文字（本 PR 一并清除）。Node 时代引擎路由旗标，`62b16c8` 删 Node 层后无代码读点 |
| `HFT_ROUND_SEC` | 活键但**保持 unset**（live 中已是注释行，正确） | `supervisor.rs:428`；issue 499：launcher 转发它会压制面板轮次卡的持久格 |
| `HFT_MIN_SHARES` | **活键，保留** | `supervisor.rs:397` |
| `HFT_MAX_SHARES` | **活键，保留** | `supervisor.rs:396` |
| `BLITZKRIEG_PANEL_USER` | **活键，保留** | `ui/ui_kit/src/web/gateway_run.rs:68` 等（manage 模式强制项） |
| `BLITZKRIEG_PANEL_PASSWORD` | **活键，保留** | 同上（`web/mod.rs:1269`：两键必须成对） |
| `BLITZKRIEG_PANEL_ADDR` | **活键，保留** | `ui/ui_kit_panel/src/bin/blitzkrieg.rs:965` |
| `BLITZKRIEG_GITHUB_TOKEN` | **活键，保留** | `core/blitzkrieg_core/src/ipc/server.rs:1003`（更新检查配额） |

反向漂移（代码在读而 example 缺，本 PR 补齐）：`BLITZKRIEG_PANEL_ADDR`、
`BLITZKRIEG_ALLOWED_ORIGINS`、`BLITZKRIEG_ALERT_NOTIFY`、`BLITZKRIEG_UPDATE_*`
三键、`BUDGET_RATIO`/`MAX_BUDGET_USD`、`BK_SIZE_PCT`/`BK_MAX_ORDER_NOTIONAL_PCT`/
`BK_MAX_DAILY_LOSS[_PCT]`/`BK_NTP_*`/`BK_MAX_ORDERBOOK_STALE_MS`/
`BK_[LUA_]STRATEGY_DIR`、`BK_SE_AUTO_EVOLVE/CYCLE_SECS/TTL_SECS/DEEP_DIMS`、
`POLYMARKET_REDEEM_ENABLED`、`POLYGON_RPC_URL`、`BLITZKRIEG_ALLOW_SHARED_DATA`、
`KALSHI_API_URL/POLL_MS/NO_PROXY`、`PREDICT_JWT_TOKEN/API_URL/POLL_MS/NO_PROXY`、
`POLYSIM_API_URL/POLL_MS/NO_PROXY`、launcher 脚本旗标
`BLITZKRIEG_UPGRADE_SOURCE/UPGRADE_DEADLINE/BACKUP_DIR/SHIM_DIR`。

## 4. 死键删除清单（生产 `.env` 重写，主窗口执行）

live `.env` 中删除以下 4 行（grep 证据见 §3）：

```
POLYMARKET_API_KEY=…
POLYMARKET_API_SECRET=…
POLYMARKET_API_PASSPHRASE=…
HFT_CORE=rust
```

其余 10 键（含 `HFT_ROUND_SEC` 的注释行——它的存在本身就是给操作员的提醒，
issue 499）原样保留。**值只在 live `.env` 里，永不进入 PR、文档、日志。**

## 5. `.env.example` 的漂移修补（本 PR 实现）

比对「代码真实读取的键全集」与 `.env.example`，发现并补齐 5 组缺失：

| 缺失键 | 读点 |
| --- | --- |
| `BLITZKRIEG_PANEL_ADDR` | `ui/ui_kit_panel/src/bin/blitzkrieg.rs:965`（live `.env` 在用而 example 缺——最实质的漂移） |
| `BLITZKRIEG_ALLOWED_ORIGINS` | `ui/ui_kit/src/web/gateway_run.rs:128` |
| `BLITZKRIEG_ALERT_NOTIFY` | `ui/ui_kit/src/gateway/supervisor.rs:53`（崩溃告警通道） |
| `BLITZKRIEG_UPDATE_CHECK` / `BLITZKRIEG_UPDATE_AUTO` / `BLITZKRIEG_UPDATE_INTERVAL_SECS` | `core/blitzkrieg_core/src/main.rs:1810-1835` |
| `BUDGET_RATIO` / `MAX_BUDGET_USD`（含 `_<ACCOUNT_ID>` 后缀形） | `core/blitzkrieg_core/src/execution_policy.rs:65` |

顺带补各扩展的 URL/轮询调优键（`KALSHI_API_URL`、`*_POLL_MS`、`*_NO_PROXY`、
`PREDICT_JWT_TOKEN` 等——全部有 grep 读点）。`.env.example` 中没有任何过时键
（现存的 `UIKIT_*`/`HFT_*`/`BK_*` 均有读点），故无删除项。

## 6. accounts.toml 处置提案（只提案，用户批准才动）

**提案：保持现状，不迁、不删。** 理由：

1. 它是面板热写回的持久化目标（venue 绑定、rounds 族、strategies 启用集、
   display_account 指针的运行时动词全部落盘于此，`account/config.rs:817`
   `persist_account_rounds` 等）。env 单源的对象是「人配置、进程只读」的键；
   进程写回的持久态放 `.env` 会破坏「.env 不被进程改写」的纪律，制造合并冲突
   式的双写面。
2. 它的语义是**账户合同 + 账户状态**，不是引擎参数：`status` 姿态（active/
   read_only/frozen/suspended，放宽唯一入口是改文件+重启）、per-account
   strategies 启用集（非空 = 逐字生效）、`rounds` 族声明（空 = 不限制、缺失 =
   旧形状；值在解析期对内核白名单 fail-closed 校验）、`[cross_venue_arb]`
   腿配置（解析成功但不自洽 = 拒绝启动）。
3. 缺文件兼容已满足：文件缺失 = 单一 `default` 账户启动（§9.5 语义，
   `account/mod.rs:334`），不存在「必须迁 env 才能跑」的依赖。

若未来操作员希望「账户簿也 env 化」，可行路径是 env 提供凭证键名
（`api_key_env` 等本就只存 env 变量名，`account/config.rs:49`）之外不变；
本轮不建议。

## 7. 键命名规范（`.env`）

三个家族，只有内核读前两类，launcher 读全部：

```
BK_*            内核自身配置。读序：CLI 旗标 > BK_* env > user_layer/configs/*.toml > 编译缺省
BLITZKRIEG_*    面板 / 网关 / 更新检查 / 启动脚本进程设置
<扩展名>_*      市场扩展（POLYMARKET_*、KALSHI_*、PREDICT_*、POLYSIM_*、CLOB_API_URL）
```

规则：
- 大写 + 下划线；扩展族以扩展名为前缀；per-account 形用 `_<账户ID大写，非字母数字→_>` 后缀
  （`BUDGET_RATIO_PAPER`）。
- 凭证值只存在于 live `.env` / 进程环境 / `data/runtime/config.json` 凭据格；
  一切配置文件只允许**命名**凭证 env 变量（`api_key_env = "KALSHI_API_KEY_ID"`），
  绝不携带值。检查 `.env` 只打印键名：`grep -o '^[A-Z_]*=' .env`。
- 一键一义：`.env` 里出现的名字必须有真实读点（grep 可证）；删代码时连带清键。

## 8. 分阶段迁移步骤

- **PR-1（#588，已完成）**：出厂四键退役，编译缺省同值零漂移；`default.toml`
  的 `[engine]` 清空。
- **PR-2（本 PR，第一阶段）**：`.env.example` 零漂移对齐 + 死键清单 + 本判决文档。
  **读序维持现状（env 优先、旧文件回退）**：CLI > env > toml > 编译缺省。这是
  兼容期读序也是终态读序——toml 层保留为「写回型持久态 + 显式 act 面」，与
  env 不构成双可信源（toml 里不再有 env 能覆盖的出厂引擎键）。
- **PR-3（生产重写，主窗口执行）**：按 §4 清单重写 live `.env`（删 4 死键），
  随下一次生产升级落地；升级前 `grep -o '^[A-Z_]*=' .env` 留档键名清单。
- **PR-4（后续，可选）**：评估删除 `default.toml` 文档壳（内核已容忍缺失）；
  评估 `bug_report.yml` 等文档面残留。**不删除**：accounts.toml、
  shadow_evolution.toml、update.toml、execution_policy.toml、manifest.json、
  data/runtime/config.json（判决见 §1）。

## 9. 「toml/json 可以删了吗」的直接回答

- **可以删（待生产验证后）**：live `.env` 里的 4 个死键（§4）。
- **不能删**：`accounts.toml`（写回型持久态）、`shadow_evolution.toml`（两开关
  唯一可信源）、`data/runtime/config.json`（面板持久意图）——它们是进程**写**
  的文件，env 做不了这件事。
- **不该删**：`manifest.json`（包合同）、`execution_policy.toml`（结构化合同）、
  `update.toml`（出厂缺省）、`default.toml`（显式 act 面文档；PR-4 可再评估）。
  它们各有 env 之上的独立身份，删除只会把结构化数据压平成 env 键名拼接。
