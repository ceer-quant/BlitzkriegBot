# #1199 施工图：env 写入面权威清单与 PR-1/2/3 施工面

> 基线：ceer/main @ `25f4c46d`（本文件写作时的仓库状态，所有行号以此为准）。
> 用途：#1199（[P0][0.4.2] 彻底移除内核 env 写入路径：env 只读不写，写路径全迁 SQLite）的
> 重构施工图。本文自包含、可核对：每个结论带 `文件:行号` 证据。
> 勘察方式：只读代码勘察（无源码改动）；行号均来自 `core/blitzkrieg_core`、`core/blitzkrieg_launcher`、
> `ui/webapp/webui`、`scripts` 下的当前工作区。

---

## 0. 结论速览

- 内核共有 **8 个 env 写域、25 个直接 `.env` 写调用点（`upsert_env_vars` / `remove_env_vars`）**，
  分布在 8 个业务模块；上游有 **18 个 IPC 入口（按动词族计）+ 4 个非 IPC 写者**可以触发它们
  （另有 `risk.setLimits` 写 `data/risk-state.json`，非 env 写口，见 §2.9）。
- 其中只有 **2 个域（风控六项 `BK_RISK_*`、出场纪律 `BK_EXIT_*`）有 DB 分支**，且条件是
  「DB 开着才写库、没开才写 env」（`trading_reset.rs:292` / `trading_reset.rs:487`）；
  其余 7 个域**无条件写 `.env`**。
- **读路径存在 7 个「写 DB / 读 env」或「写 env / 读进程环境」的不对称点**，最危险的 3 个：
  ① `risk.limits` 查询时从 `.env` 文件覆盖内存读出（DB 值被 env 旧值盖住）；
  ② env 文件 watcher 对任意 `.env` mtime 变化全量重派生 cockpit 状态（DB 内存值被文件旧键覆盖）；
  ③ launcher 无条件导入 / session 条件开库的「分裂脑」（默认部署导入后不开库，面板写 env，DB 冻结）。
- 回执语义：`persisted`（bool）出现在 10 个 wire 响应结构；`persistence`（"env"/"none"）**只在
  `risk.setSystemic` / `risk.setExit` 两个回执**里由 `handlers/risk.rs:131` 的 `with_persisted` 注入。
  前端消费点：`SettingsRiskLimits.vue:423/:432`、`AccountSwitcher.vue:204`、`api/risk.ts`、
  `api/accounts.ts`、`api/market.ts`、`api/rounds.ts`、`api/strategy.ts`。
- issue #1199 验收清单中的行号：**绝大多数准确**（含 `trading_reset.rs:292/:487/:625`、
  `session.rs:43`、`main.rs:1296`、`SettingsRiskLimits.vue:344/:423/:432`、`SettingsRisk.vue:349-376`、
  `README.md:252`）；**唯「round-duration-check.mjs:69 注释与代码相反」不属实**——:69 的注释与其后
  代码一致，真实的注释过期点在文件头 docblock `:13-14`（详见 §6）。

---

## 1. 写面基础设施（两个原语 + 一个路径解析器）

| 设施 | 位置 | 说明 |
|---|---|---|
| `.env` 写原语 | `core/blitzkrieg_core/src/env_file.rs:43`（`upsert_env_vars`）、`:73`（`remove_env_vars`） | 原子 temp+rename、0600、托管键外逐字节保留；返回写入/删除的键名。**所有** `.env` 写最终都走这两个函数 |
| `.env` 读半边 | `core/blitzkrieg_core/src/env_file.rs:108`（`parse_line`）、`:136`（`read_env_vars`） | 与 launcher 解析器字节级同构 |
| 账户写面路径解析 | `core/blitzkrieg_core/src/service/config.rs:1057`（`account_env_write_paths`） | 返回 `(env_path, legacy_toml)`；`env_file_path` 缺省时取 toml 同目录 `.env`；`accounts_config_path` 为 `None` 时整个写面禁用（回测/进程内测试） |
| DB 侧仓库 | `core/blitzkrieg_core/src/risk/account_store.rs:27`（`AccountRiskDb`） | SQLite：`account_risk`（:86 起 DDL）、`account_exit_policy`、`account_settings`；**仅** `save_risk`（:160）/`save_exit`（:221）两个写方法，读在 `load_risk`（:45）/`load_exits`（:113），只在开库时调用一次 |
| DB 开库条件 | `core/blitzkrieg_core/src/ipc/server/session.rs:43-50` | **仅当** `BLITZKRIEG_DB_PATH` 显式设置才 `open_account_risk_db`；开失败 fail-closed 拒绝启动；未设 = env fallback |

DB 打开时把 DB 行 overlay 到 env 派生的工作副本上（DB 更强源）：
`core/blitzkrieg_core/src/service/assembly.rs:15-27`（`open_account_risk_db`）。

---

## 2. 写点权威清单（按域）

「后端条件」列：**DB-first** = `if let Some(db) = account_risk_db { 写库 } else if env { 写env } else { 内存 }`；
**无条件 env** = 只要 `env_file_path` 在场就写 `.env`（legacy toml 在场时拒绝，见 `book.rs:259`）；
**best-effort env** = 写失败只记日志不拒绝 RPC。

### 2.1 风控矩阵 `BK_RISK_*`（risk.setSystemic）

| 项 | 证据 |
|---|---|
| IPC 入口 | `core/blitzkrieg_core/src/ipc/server/handlers/risk.rs:222`（`RISK_SET_SYSTEMIC`） |
| service 写路径 | `core/blitzkrieg_core/src/service/trading_reset.rs:193`（`apply_systemic_limits`） |
| 后端条件 | **DB-first**：`trading_reset.rs:292`（`save_risk` 进 `account_risk`）→ `trading_reset.rs:302-311`（env 分支调 `persist_cockpit_changes`）→ else 内存 only、`persisted=false`（:312-314） |
| env 键映射 | `handlers/risk.rs:16`（`systemic_env_key`，6 字段）+ 每账户 `cockpit::env_key_for_field_account`（`cockpit.rs:116`，后缀/中缀两种拼写） |
| env 写调用点 | ① `handlers/risk.rs:75`（IPC 兜底 upsert，仅当 service 层 `update.persisted==false` 时触发，`handlers/risk.rs:259-263`）；② `core/blitzkrieg_core/src/service/cockpit_env.rs:154`（`persist_cockpit_changes`，由 `trading_reset.rs:303` 调用） |
| 键族 | `BK_RISK_MAX_SINGLE_LOSS_USD` / `BK_RISK_MAX_DAILY_DRAWDOWN_USD` / `BK_RISK_MAX_POSITION_SIZE` / `BK_RISK_MAX_CONSECUTIVE_LOSSES` / `BK_RISK_COOLDOWN_MINUTES` / `BK_RISK_MAX_ORDER_NOTIONAL_USD`（+ `_<ACCOUNT>` 后缀与 `BK_RISK_<ACCOUNT>_<FIELD>` 中缀） |

### 2.2 出场纪律 `BK_EXIT_*`（risk.setExit）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/risk.rs:274`（`RISK_SET_EXIT`） |
| service 写路径 | `trading_reset.rs:365`（`apply_exit_set`） |
| 后端条件 | **DB-first**：`trading_reset.rs:487-497`（`save_exit` 进 `account_exit_policy`）→ `trading_reset.rs:498-533`（env 分支：先 upsert :514，再 `refresh_cockpit_from_env_file` :524 从文件重派生缓存；写文件失败拒绝整个 apply）→ else 内存 only（:534-563） |
| env 键映射 | `handlers/risk.rs:32`（`exit_env_key`，5 字段） |
| env 写调用点 | ① `handlers/risk.rs:113`（IPC 兜底，条件同上）；② `trading_reset.rs:514`（service env 分支） |
| 键族 | `BK_EXIT_STOP_LOSS_PCT` / `BK_EXIT_TAKE_PROFIT_PCT` / `BK_EXIT_FORCE_EXIT_SEC` / `BK_EXIT_TRAILING_ENABLED` / `BK_EXIT_STRATEGY_EXIT_MAKER_FIRST`（+ 每账户后缀） |

### 2.3 账户簿 `ACCOUNT_*`（account.* 家族 + strategy.enable + 启动迁移）

所有写面函数集中在 `core/blitzkrieg_core/src/account/config/book.rs`，统一经 `env_upsert`（:277）→
`env_file.rs:282` 或 `remove_env_vars`；**无条件 env**（前提：legacy toml 不存在，`book.rs:259`
`refuse_while_legacy_toml`；无 `accounts_config_path` 时 service 层整体拒绝）。

| 写面函数（book.rs） | 键 | service 入口（accounts/persist.rs 除注明外） | IPC 入口（handlers/account.rs 除注明外） |
|---|---|---|---|
| `env_persist_account_venue`（:307） | `ACCOUNT_<ID>_VENUE`（空=删键 :320） | `set_account_venue_persisting`（persist.rs:28） | `account.venue.set`（account.rs:237→:271） |
| `env_persist_account_create`（:337） | `ACCOUNT_LIST` + `ACCOUNT_<ID>_NAME/_VENUE/_API_KEY_ENV/_SECRET_ENV/_PASSPHRASE_ENV/_PRIVATE_KEY_ENV` | `create_account_persisting`（persist.rs:1038） | `account.create`（account.rs:741→:783） |
| `env_persist_account_trading`（:394） | `ACCOUNT_<ID>_TRADING_ENABLED`（true=删键 :406） | `set_account_trading_persisting`（persist.rs:512） | `account.trading.set`（account.rs:582→:592） |
| `env_persist_account_status`（:423） / `env_persist_account_active`（:458） | `ACCOUNT_<ID>_STATUS`（Active=删键；unfreeze 一次删 STATUS+TRADING_ENABLED 两键 :469） | `set_account_status_persisting`（persist.rs:578） | `account.unfreeze`（account.rs:653 → `risk_admission/freeze.rs:353` `unfreeze_account` → :373 调 persist） |
| `env_persist_account_strategies`（:513） | `ACCOUNT_<ID>_STRATEGIES`（空=删键 :525） | ① `set_account_strategies_persisting`（persist.rs:729→:833）；② `strategies.rs:181` `set_scoped_strategy_enabled`（persist :243）；③ 启动一次性迁移 persist.rs:925→:957 | ① `account.strategies.set`（account.rs:682→:695）；② `strategy.enable/disable`（handlers/strategy.rs:68；镜像写入门 :141 + 调用 :235） |
| `env_persist_account_rounds`（:541） | `ACCOUNT_<ID>_ROUNDS`（空=删键 :553） | `set_account_rounds_persisting`（persist.rs:90） | `account.rounds.set`（account.rs:364→:469） |
| `env_persist_account_remove`（:580） | `ACCOUNT_LIST` 重写 + 该 id 全部 `ACCOUNT_<ID>_*` 清扫（:634）+ `ACCOUNT_DISPLAY` 重派生 | `remove_account_persisting`（persist.rs:1168） | `account.remove`（account.rs:844→:854） |
| `env_persist_display_account`（:645） | `ACCOUNT_DISPLAY` | `set_display_account_persisting`（persist.rs:667） | `display.account.set`（account.rs:716→:722） |
| `env_persist_cross_venue_arb`（:659） / `env_clear_cross_venue_arb`（:694） | `ACCOUNT_ARB_LEG_A_VENUE/_ACCOUNT/_LEG_B_VENUE/_ACCOUNT/_PAIRS` 五键 | `arb_config_set_persisting`（persist.rs:173→:192）/ `arb_config_clear_persisting`（persist.rs:478→:487） | `arb.config.set`（handlers/arb.rs:17→:53）；clear 无 IPC 臂（仅测试调用） |
| `env_persist_account_opening_equity`（:483） | `ACCOUNT_<ID>_OPENING_EQUITY/_AT/_SOURCE`（只写一次） | **非 IPC**：`realign_venue_cash`（accounts/mod.rs:86→:119，由 venue 现金报告触发，`market/host.rs:749`） | — |

### 2.4 执行策略 `POLICY_*`（execution_policy.set / reset）

| 项 | 证据 |
|---|---|
| IPC 入口 | `core/blitzkrieg_core/src/ipc/server/handlers/execution_policy.rs:60`（set）/ `:66`（reset） |
| 写函数 | `handlers/execution_policy.rs:325`（`policy_section_write`）：先清扫该账户全部 section 键（:408 `remove_env_vars`），再 upsert `Some` 字段（:414） |
| 键族 | `POLICY_BUDGET_RATIO` / `POLICY_MIN_BUDGET_USD` / `POLICY_MAX_BUDGET_USD` / `POLICY_MIN_EQUITY_USD` / `POLICY_MAX_POSITIONS_PER_ASSET` / `POLICY_RULES`（+ `_<ACCOUNT>` 后缀；defaults 选择器写无后缀裸键），键清单的权威定义在 `core/blitzkrieg_core/src/execution_policy/env_state.rs:79`（`section_env_state`，:107-117 六键 + :122-133 defaults 幽灵键清扫） |
| 后端条件 | **无条件 env**（无 `env_file_path` 拒绝 :334-341；被编辑过的 legacy toml 拒绝 :342-380；内核自动生成模板 toml 会被顺手删除 :354） |
| 附带写 | 每次 set/reset 后 `reload_execution_policy`（handlers/execution_policy.rs:161/:221）从文件重读 |

### 2.5 轮次周期 `ACCOUNT_<ID>_ROUND_DURATION_SEC`（round_duration.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `core/blitzkrieg_core/src/ipc/server/handlers/round.rs:29`（`ROUND_DURATION_SET`）→ :41 |
| service 写路径 | `trading_reset.rs:625`（`apply_round_duration_set`），**持久优先**两段提交：env 先写（reset→`remove_env_vars` :715；set→`upsert_env_vars` :723），失败拒绝且无账簿副作用；随后撤单→市价强平（:737 `cancel_and_flatten_account`）→内存+引擎网格（:739-770） |
| 键 | `ACCOUNT_<ID>_ROUND_DURATION_SEC`（id 折叠为大写字母数字，`:702-712`） |
| 后端条件 | **无条件 env**；无 `env_file_path`（回测/进程内测试）跳过持久化直接内存执行（:696 的 `if let` 无 else 拒绝） |
| 相关死写面 | `RUNTIME_ROUND_DURATION_SEC`：`runtime_config.rs:224`（`persist_round_duration_to_with_env`，upsert :259 / remove :261）**无任何生产调用者**（全仓 grep 仅 runtime_config 自身与测试）；读侧 `core/blitzkrieg_launcher/src/main.rs:1233` 仍读（已标 deprecated） |

### 2.6 凭据（插件声明的凭据变量名）（market.credentials.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `core/blitzkrieg_core/src/ipc/server/handlers/market.rs:56` |
| 写函数 | `core/blitzkrieg_core/src/market/mod.rs:586`（`apply_credentials_set`）：reset→`remove_env_vars`（:647），set→`upsert_env_vars`（:654） |
| 键族 | 由插件 `required_credential_env()` 声明（如 `POLY_API_KEY` 等），逐插件校验（:628-638） |
| 后端条件 | **无条件 env**（无 `env_file_path` 拒绝 :617-625）；next_session 语义 |
| 启动一次性迁移 | `runtime_config.rs:275`（`migrate_credentials_cell_to_env`，upsert :307），launcher `main.rs:1271` 调用，把旧 config.json credentials 格排空进 `.env` |

### 2.7 行情插件选择 `RUNTIME_MARKET_PLUGIN`（market.plugin.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/market.rs:27`（`MARKET_PLUGIN_SET`） |
| 写函数 | `market/mod.rs:501`（`persist_cell`）→ `runtime_config.rs:174`（`persist_market_plugin_to_with_env`）：**同时**写 JSON 格（`data/runtime/config.json`）与 env 键（upsert :204 / remove :206） |
| 后端条件 | 无条件双写（JSON 格 + env）；无 `runtime_config_path` 拒绝 |
| 启动迁移 | `market/mod.rs:85`（`boot_enablement`）在账户 venue 绑定派生生效时清持久格（:121 调 `persist_market_plugin_to_with_env(None)`）——这是又一处 env 写 |

### 2.8 影子进化开关 `SHADOW_ENABLED` / `SHADOW_AUTO_EVOLVE`

| 项 | 证据 |
|---|---|
| IPC 入口 | `core/blitzkrieg_core/src/ipc/server/handlers/se.rs:17`（`shadow_evolution.enable`）→ :19；`:22`（disable）→ :23；`:216`（`shadow_evolution.set_auto`）→ :223 |
| 写函数 | `core/blitzkrieg_core/src/shadow_evolution/mod.rs:1218`（`persist_switch`，upsert :1235） |
| 键 | `SHADOW_ENABLED`（:556/:567）、`SHADOW_AUTO_EVOLVE`（:1181） |
| 后端条件 | **best-effort env**（无 `env_path` 只 debug 日志；写失败 error 日志不拒绝——:1235-1241）；无 DB 分支 |

### 2.9 （非 env，列出以免误伤）risk.setLimits → risk-state.json

`risk.setLimits`（`handlers/risk.rs:182`）→ `trading_reset.rs:25`（`apply_risk_limits`）→
`risk_admission/persist.rs:213`（`risk_state_record_limits`）写 `data/risk-state.json`（**不是 .env**）。
`persisted` 回执 = 该 JSON 文件写结果（`trading_reset.rs:139`）。此域不在 #1199 的 env 迁移范围内，
但 PR-1 的 StoreApi 抽象若要「全域覆盖」需决定是否把它一并纳入（建议纳入，否则又多一个直写文件的域）。

### 2.10 统计

- **直接 `.env` 写调用点：25 个**（非测试代码）：
  `runtime_config.rs:204/:206/:259/:261/:307`（5）、`handlers/risk.rs:75/:113`（2）、
  `handlers/execution_policy.rs:408/:414`（2）、`market/mod.rs:647/:654`（2）、
  `trading_reset.rs:514/:715/:723`（3）、`account/config/book.rs:282/:320/:406/:436/:469/:525/:553/:634/:696`（9）、
  `service/cockpit_env.rs:154`（1）、`shadow_evolution/mod.rs:1235`（1）。
- 按域分布（合计 25）：账户簿 `ACCOUNT_*` 9（book.rs）、轮次 4（`trading_reset.rs:715/:723` +
  deprecated `RUNTIME_ROUND_DURATION_SEC` 的 `runtime_config.rs:259/:261`）、凭据 3
  （`market/mod.rs:647/:654` + 迁移 `runtime_config.rs:307`）、风控 2（`handlers/risk.rs:75` +
  `cockpit_env.rs:154`）、出场 2（`handlers/risk.rs:113` + `trading_reset.rs:514`）、执行策略 2、插件选择 2、影子进化 1。
- 可触发写 `.env` 的 IPC 入口：**18 个**（按动词族计；`strategy.enable/disable` 算一族、
  `shadow_evolution` 三动词算一族）——`risk.setSystemic`、`risk.setExit`、`market.credentials.set`、
  `market.plugin.set`、`execution_policy.set`、`execution_policy.reset`、`round_duration.set`、
  `account.venue.set`、`account.rounds.set`、`account.trading.set`、`account.unfreeze`、
  `account.strategies.set`、`display.account.set`、`account.create`、`account.remove`、`arb.config.set`、
  `strategy.enable/disable`、`shadow_evolution.enable/disable/set_auto`。另有 `risk.setLimits`
  （`handlers/risk.rs:182`）写 `data/risk-state.json`，**不是** env 写口（见 §2.9）。
- 非 IPC 写者：**4 个**——venue 现金报告（opening equity，`accounts/mod.rs:119`，经 `market/host.rs:749`）、
  启动策略启用迁移（`persist.rs:957`）、`boot_enablement` 清持久格（`market/mod.rs:121`）、
  凭据格迁移（launcher `main.rs:1271`）。

---

## 3. 读路径勘察：写 DB / 读 env 的不对称点（迁移隐性地雷）

### 3.1 全部 env 读面（谁还在读 env / 进程环境）

| 读面 | 位置 | 读什么 |
|---|---|---|
| 全局风控 env 面 | `risk/limits.rs:225`（`from_env`）、`:332`（`from_env_with_accounts`） | 进程环境 `BK_RISK_*`（含每账户后缀/中缀）；launcher `main.rs:262` 启动时调用 |
| 每账户出场覆盖 | `config.rs:269`（`account_exit_overrides`，经 `env_lookup`） | `BK_EXIT_*_<ACCOUNT>` |
| 账户簿启动读 | `account/config/book.rs:14`（`load_account_ledgers_from_env`）+ `env_lookup_from_sources`（:182，进程环境优先、`.env` 文件兜底） | `ACCOUNT_LIST`、`ACCOUNT_<ID>_*`、`ACCOUNT_ARB_*` |
| 启动期账户标量 | `service/assembly.rs:103`（`env_lookup_from_sources`） | `ACCOUNT_<ID>_ROUND_DURATION_SEC`（:139-161，不在 ROUNDS 白名单则 panic 拒启动）、`ACCOUNT_<ID>_MIN_SHARES/_MAX_SHARES`（:173-208）、`ACCOUNT_<ID>_MARKET_PLUGIN`（:209-215）、`STRATEGY_<NAME>_ROUND_DURATION_SEC`（:218 起） |
| cockpit 文件面 | `cockpit_env.rs:46`（`refresh_cockpit_from_env_file`）→ `env_file.rs:136`（`read_env_vars`，**只读文件、不读进程环境**） | `COCKPIT_KEYS` 11 键（`cockpit.rs:44`）+ 每账户行（`cockpit_env.rs:88-121`） |
| risk.limits 读出 | `handlers/risk.rs:151-167`：先取内存快照再 `overlay_risk_limits`（`cockpit.rs:165`）从**文件**覆盖 | 同上 11 键 |
| 执行策略读 | `execution_policy/mod.rs:1041`（`load_from_str`，进程环境）与 `:1055-1069`（`load_single_source`，进程环境优先 + `.env` 文件兜底，issue #751/#967） | `POLICY_*` + 裸别名 |
| 凭据状态 | `market/mod.rs:565`（`std::env::var_os`，**只读进程环境**） | 插件凭据变量 |
| 插件选择 | launcher `main.rs:1246`（`RUNTIME_MARKET_PLUGIN` 进程环境，outranks JSON 格） | `RUNTIME_MARKET_PLUGIN` |
| 轮次（deprecated） | launcher `main.rs:1233` | `RUNTIME_ROUND_DURATION_SEC` |
| 影子进化 | `shadow_evolution/mod.rs:248-262`（启动 config：`.env` 的 `SHADOW_ENABLED`/`SHADOW_AUTO_EVOLVE` 或 legacy `BK_SE_*`） | `SHADOW_*` / `BK_SE_*` |
| DB 读 | `risk/account_store.rs:45/:113`，仅在 `assembly.rs:15` `open_account_risk_db` 时一次 | `account_risk`、`account_exit_policy` |
| db_import | `db_import.rs:341`（`env_lookup`）+ `:481`（`account_ids`） | 一次性导入 `ACCOUNT_*` / `BK_RISK_*` / `POLICY_*` → SQLite（`kernel_meta.import_v1` 幂等标记） |

### 3.2 不对称点清单（7 个）

| # | 不对称 | 证据 | 后果 |
|---|---|---|---|
| A1 | **risk.limits 读出从 `.env` 文件覆盖内存**（DB 开着也一样） | `handlers/risk.rs:157-166`（查询时 `read_env_vars` + `overlay_risk_limits`）；写侧 DB 分支**不写 env**（`trading_reset.rs:292-301`） | DB 开库 + `.env` 残留旧 `BK_*/BK_EXIT_*` 键时：面板读出 = env 旧值，引擎实际用 DB 新值。写 DB 后读 env 的**直接分裂** |
| A2 | **env 文件 watcher 全量重派生 cockpit**（任意 `.env` 写都触发） | `ipc/server/session.rs:222-262`（2s mtime 轮询 → `refresh_cockpit_from_env_file`）；`cockpit_env.rs:58-123`（`apply_cockpit_vars` 对文件在场键逐个覆盖内存，含每账户行） | 任何无关 `.env` 写（账户开关、凭据、轮次、POLICY 保存——它们都重写同一个文件）都会让 watcher 把 DB 载入的内存值**用文件旧键覆盖**，直到下次重启/写 DB |
| A3 | **launcher/session 开库条件分裂**（issue 恶性缺陷） | launcher 无条件导入：`main.rs:1296-1301`（默认路径 `db::DEFAULT_DB_PATH`，`import_once` 幂等）；core 只在显式 `BLITZKRIEG_DB_PATH` 开库：`session.rs:43-50` | 默认部署：导入跑、库不开 → 面板写 env；之后设了 DB_PATH 开库 → `assembly.rs:15-27` DB 行 overlay 更强源 → **env-only 期间的面板修改静默丢弃** |
| A4 | **账户簿：写 env，导入 DB 后无读侧** | 写：`book.rs` 全部写面；导入：`db_import.rs:391`（`insert_account`）；运行期读 `accounts` 表：**无**（全仓 `FROM accounts` 仅 `db_import_tests.rs:548/:551` 与 `db/mod.rs` 测试） | 导入的 `accounts` 表是孤儿数据；任何 `.env` 账户写后 DB 与 env 永久分叉 |
| A5 | **RUNTIME_ROUND_DURATION_SEC：死写面 / 活读面** | 写：`runtime_config.rs:259/:261` 无生产调用者；读：`main.rs:1233` 仍在读（deprecated 警告） | PR-1 删写面时不能连读面一起删（兼容旧部署），需成对处理 |
| A6 | **execution_policy：导入 DB 后无运行期读** | 导入：`db_import.rs:317-333`（`SETTING_KEYS` 含 `BUDGET_RATIO`/`MIN_BUDGET_USD`/`MAX_BUDGET_USD`/`MIN_EQUITY_USD`/`MAX_POSITIONS_PER_ASSET`/`RULES`，落 `account_settings`）；读：`execution_policy/mod.rs:1055`（env 单源） | 同 A4：DB 中 policy 键无读者；PR-2 需补读侧或删导入 |
| A7 | **凭据 status 读进程环境，写 `.env` 文件** | 写：`market/mod.rs:654`；读：`market/mod.rs:565`（`std::env::var_os`） | 非 DB 不对称，但同属「写面/读面不同层」：写后本进程 `credentials.status` 不翻绿（重启才载）——`AddAccountDialog.vue:117-120` 的文案即此语义 |

**最危险的 3 个**：A1（面板读出错值，操作员基于假读数决策）、A2（无关写操作静默复活旧配置）、
A3（默认部署的数据分叉 + 后续开库静默丢修改，即 issue 所指 P0 缺陷）。

---

## 4. 回执语义（persisted / persistence）

### 4.1 内核侧

| 字段 | 出现位置 | 语义 |
|---|---|---|
| `persisted`（bool） | `schema/mod.rs:1000`（`RiskLimitUpdate`，risk.setLimits→risk-state.json）、`:1096`（`SystemicLimitUpdate`）、`:1195`（`ExitSetUpdate`）；`schema/views.rs:740`（`AccountVenueSetResult`）、`:789`（`AccountRoundsSetResult`）、`:886`（`AccountTradingSetResult`）、`:970`（`AccountStrategiesSetResult`）、`:1006`（`DisplayAccountSetResult`）、`:1052`（`AccountCreateResult`）、`:1094`（`AccountRemoveResult`） | 落盘是否成功；账户家族写失败会拒绝整个 RPC（persisted 恒 true），风控两域可能为 false（无 env 文件/写失败仅告警） |
| `persisted`（恒 false） | `service/config/fee.rs:175`（`RoundDurationResult`，round_duration.get 读出） | 硬编码 false——该卡片的真实持久化在 `round_duration.set` 回执侧没有对应字段 |
| `persistence`（"env"/"none"） | **仅** `handlers/risk.rs:131-141`（`with_persisted`），注入 `risk.setSystemic` / `risk.setExit` 两个回执 | 拼写载体；`schema/mod.rs:1086-1087`、`:1184-1186` 有契约注释 |

### 4.2 前端消费（grep ui/）

| 消费点 | 位置 | 用法 |
|---|---|---|
| `api/risk.ts` | `:86-98`（setSystemic 回执类型，`persistence?: 'env'\|'none'`，注释明令「禁止只显示已保存」）、`:128-139`（setExit 同） | 类型 + 契约注释 |
| `SettingsRiskLimits.vue` | `:423`（`riskReceipt.persisted \|\| riskReceipt.persistence === 'env' ? '已持久化到 .env' : '未持久化，重启回到启动配置'`）、`:432`（exitReceipt 同） | **「已持久化到 .env」文案违约点**（#1199 违约点 2） |
| `AccountSwitcher.vue` | `:204`（`res.persisted ? '（已落盘）' : ''`） | 存储中性，合规 |
| `api/accounts.ts` | `:56/:105/:118/:133/:164/:175/:194`（venue/display/trading/strategies/create/remove/rounds 回执的 `persisted`） | 类型声明 |
| `api/market.ts` | `:64`（`persisted`）、`:72`（`source?: 'accounts'\|'persisted'\|'flag'\|'fallback'`） | 类型声明 |
| `api/rounds.ts` | `:22`（`RoundDurationDoc.persisted`，内核恒 false） | 类型声明（当前无 .vue 消费方——`api/rounds` 尚无导入者） |
| `api/strategy.ts` | `:59`（`persisted`） | 类型声明 |
| `stores/settings.ts`、`HftPage.vue:58`、`OnboardingTour.vue`、`lib/theme.ts` 等 | — | 均为 localStorage 前端偏好（E8-d），与内核 env/SQL 无关，不在迁移面 |

---

## 5. PR-1 / PR-2 / PR-3 施工面

### PR-1：StoreApi trait + EnvStore 实现（行为不变，纯重构）

**目标**：所有业务写口改调 `StoreApi`；后端仍是 EnvStore（写 `.env`，逐字节不变）；门禁绿。

| 施工项 | 面 | 证据（改动点） |
|---|---|---|
| 定义 trait | 新增 store 模块 | 按域分组：账户簿 / 风控 / 出场 / 交易开关 / venue / rounds / strategies / strategy.enable / execution_policy / 面板设置（round duration / market plugin / shadow 开关 / 凭据）。契约不带 env/SQL 语义 |
| EnvStore 实现 | 包装现有原语 | `env_file.rs:43/:73` 不动；把 `book.rs` 的 11 个 `env_persist_*` + `env_clear_cross_venue_arb`、`cockpit_env.rs:130`（`persist_cockpit_changes`）、`handlers/execution_policy.rs:325`（`policy_section_write`）、`runtime_config.rs:174/:224`、`market/mod.rs:586`、`shadow_evolution/mod.rs:1218` 全部改写为 trait impl 后的调用 |
| 业务写口改造 | 18 个 IPC 入口 + 4 个非 IPC 写者 | §2 各表的「service 入口」列；删除 `if let Some(db)` 分裂分支（`trading_reset.rs:292/:487`）——EnvStore 后端下该分支退化为单一 env 路径（DB 分支留给 PR-2 的 SqliteStore） |
| 读面 | **不动**（EnvStore 读 = 现状） | A1/A2 的不对称在 PR-1 保留（行为不变），PR-2 随读侧迁移一并修复 |
| 测试 | 内存 Store 验证 | issue 验收「换后端 = 改一处装配」需要一组内存 Store 测试 |

**注意**：`risk.setLimits`（risk-state.json）与 `strategy-state.json`（`persist.rs:861` 默认账户镜像）、
`data/runtime/config.json` 格（`runtime_config.rs:143-265`）是**非 env 文件写面**，PR-1 需决定是否
纳入 trait（建议纳入「面板设置」域，否则 PR-3 的「业务层无直接文件写」红线挂一漏万）。

### PR-2：SqliteStore 补齐 + 默认开库统一 + 导入幂等（切换默认后端）

| 施工项 | 现状 | 缺口 |
|---|---|---|
| 风控/出场表 | `account_risk`（`account_store.rs:86` DDL）、`account_exit_policy`、`account_settings` | 已有读写（`account_store.rs:45/:113/:160/:221`）；需接 SqliteStore 并删除 env 回退（`assembly.rs:15-27` 改为唯一源） |
| 账户簿表 | `accounts` 表已由 db_import 导入（`db_import.rs:59` DDL / :391 插入） | **无运行期读侧**（A4）；需写 SqliteStore 读写并让 `load_account_ledgers_from_env` 退役 |
| 执行策略 | db_import 已导入 POLICY_*（A6） | 无运行期读侧；需 store 读写 |
| 轮次周期 | 无表无读 | 需新表 + 读写（含 `ACCOUNT_<ID>_ROUND_DURATION_SEC` 启动解析退役，`assembly.rs:139-161`） |
| 凭据 | `db/credentials.rs:187` 已有 AES-256-GCM 密文设施；README.md:172 已宣称 | 需接 store；`market/mod.rs:565` 状态读改走解密读 |
| 影子开关 / market plugin / round 设置 | 无表 | 需表 + 读写；`RUNTIME_*` 读面（`main.rs:1233/:1246`）退役或保留兼容 |
| 默认开库统一 | launcher 无条件导入（`main.rs:1296`）vs session 条件开库（`session.rs:43`） | session 改默认路径开库（fail-closed 语义保留）；导入保持幂等（`import_v1` 标记，`db_import.rs`）；消除 A3 分裂脑 |
| DB 未开库 fail-closed | 现状 env 静默回落 | SqliteStore 缺库时**拒绝写入**（issue 验收红线）；A1/A2 的读侧 overlay 随 store 读迁移一并拆除 |

### PR-3：UI 文案清理 + env 运行期写入 CI 门禁（收口）

| 施工项 | 位置（现行行号） | 说明 |
|---|---|---|
| 「已持久化到 .env」 | `SettingsRiskLimits.vue:423`、`:432` | 改存储中性措辞（如「已保存」+ 由 API 层给载体词）；`api/risk.ts:91` 注释同步 |
| 「全局级限额」分组 | `SettingsRiskLimits.vue:340`（`['账户','全局']`）→ `:344` 渲染 | #983 起风控只有账户级，分组文案违约 |
| 配置停用/闸门 .env 文案 | `SettingsRisk.vue:349-350`、`:367`、`:375-376` | 「改 .env 并重启」全套改 API 口径 |
| 凭据 .env 文案 | `AddAccountDialog.vue:7-8`、`:117-120`、`:202` | 同上 |
| envReload / 凭据格文案 | `SettingsGeneral.vue:42`、`:220`、`:222`、`:325`、`:373` | 同上 |
| README 口径 | `README.md:132`、`:252` | 与代码一致化（当前 README 说 SQLite、代码写 env——文档在撒谎的另一边） |
| round-duration 门禁 | `scripts/round-duration-check.mjs` | 断言改走 API 口径；过期注释清理（见 §6） |
| env 写入 grep gate | 新增 CI 检查 | 业务层出现 `env_persist_*` / `upsert_env_vars` / `remove_env_vars` / 直接 SQL = 红灯（仅 store 实现模块豁免） |

---

## 6. issue #1199 验收清单行号核对（基线 25f4c46d）

| issue 声明 | 核对结果 | 现行证据 |
|---|---|---|
| `trading_reset.rs:292`（setSystemic DB 条件） | **准确** | `trading_reset.rs:292` `let persisted: bool = if let Some(db) = &self.account_risk_db {` |
| `handlers/risk.rs:242`（env path 读取） | **准确** | `handlers/risk.rs:242` `let env_file = core.lock().await.config().env_file_path.clone();` |
| `trading_reset.rs:487`（setExit DB 条件） | **准确** | `trading_reset.rs:487` `let exit_persisted: bool = if let Some(db) = self.account_risk_db.clone() {` |
| `persist.rs:512`（account.trading.set 无条件 env） | **准确** | `service/accounts/persist.rs:512` `pub fn set_account_trading_persisting(` |
| `trading_reset.rs:625` 起（round_duration.set） | **准确** | `trading_reset.rs:625` `pub fn apply_round_duration_set(` |
| `execution_policy/env_state.rs`（POLICY_* 写面） | **准确**（写函数在 handlers，键清单在此） | 写：`handlers/execution_policy.rs:325-417`；键：`execution_policy/env_state.rs:79-158` |
| `session.rs:43`（条件开库） | **准确** | `ipc/server/session.rs:43` `if let Ok(path) = std::env::var("BLITZKRIEG_DB_PATH")` |
| `main.rs:1296`（launcher 无条件导入） | **准确** | `main.rs:1296-1301`（默认 `DEFAULT_DB_PATH` + `import_once`） |
| `assembly.rs:10`（DB 行更强源） | **准确** | `assembly.rs:15-27`（doc 注释在 :10-13） |
| `SettingsRiskLimits.vue:344`（「全局级限额」） | **准确** | `:340` 分组数组 `['账户', '全局']`，`:344` 渲染 `{{ group }}级限额` |
| `SettingsRiskLimits.vue:423/:432`（「已持久化到 .env」） | **准确** | `:423`、`:432` 原文所在行 |
| `SettingsRisk.vue:349-376` | **准确** | `:349-350`（配置停用 .env 文案）、`:367`、`:375-376`（配置态闸门 .env 文案） |
| `AddAccountDialog.vue` | **准确**（文件级） | `:7-8`、`:117-120`、`:202` |
| `SettingsGeneral.vue` | **准确**（文件级） | `:42`、`:220`、`:222`、`:325`、`:373` |
| `README.md:252` | **准确** | `:252`「配置口径：账户与风控在 SQLite…」；另 `:132` 同口径 |
| `scripts/round-duration-check.mjs:69`「注释与代码相反」 | **不属实（就 :69 而言）** | `:69-71` 注释（"The pre-write readout: compiled default, no per-account overrides, the declared union…with its labels"）与 `:72-81` 代码（断言 `source==='default'`、`perAccount==='{}'`、labels 300/900/86400）**一致**。真实的注释过期点在**文件头 docblock**：`:13-14` 第 3 条仍写「立即生效：本会话 perAccount.default=300（**内存 map 即时更新**）」，而 #1184（25b2f528）起代码是破坏性的秒切秒换（撤单→市价强平→切网格，body 注释 `:137-138` 与 `trading_reset.rs:736-737` 已更新、docblock 漏改）；另 `:8-9` 声称五个档位标签（5m/15m/1h/4h/daily，与 `extensions/polymarket/src/plugin.rs:68-83` 的声明一致），而断言只查三个（`:79-81`）——是断言 under-check，不是相反。issue 该行号如需引用建议改指 docblock `:13-14` |

---

## 7. 给后续 agent 的注意事项

1. **不要动 `env_file.rs` 的写原语**：PR-1 的 EnvStore 直接包装它；它同时是读契约（launcher 解析器同构，`env_file.rs:288-319` 测试钉住）。
2. **`account_env_write_paths`（`service/config.rs:1057`）是账户写面的总闸**：`accounts/persist.rs` 11 处 + `strategies.rs:234` + `accounts/mod.rs:117`（opening equity）经它解析路径；trait 化时它是天然的 EnvStore 装配点。
3. **legacy toml 拒绝逻辑**（`book.rs:259`、`handlers/execution_policy.rs:342-380`）是行为的一部分，迁移时保留语义（EnvStore 内）。
4. **watcher（`session.rs:222-262`）与 `risk.limits` 读出（`handlers/risk.rs:151-167`）是 A1/A2 的两半**：PR-2 拆 env 读侧时必须成对处理，否则拆一半更危险。
5. **`round_duration.get` 的 `persisted` 恒 false**（`fee.rs:175`）是既存小谎，PR-3 文案清理时顺带处理。