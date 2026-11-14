# #1199 施工图：env 写入面权威清单与 PR-1/2/3 施工面

> 基线：ceer/main @ `88fe66a3`（本文件写作时的仓库状态）。下列代码定位以 **`模块::函数` 锚点** 为准；行号若出现仅为 88fe66a3 的快照，会随着后续 PR 漂移，定位规则见 §7。
> 用途：#1199（[P0][0.4.2] 彻底移除内核 env 写入路径：env 只读不写，写路径全迁 SQLite）的
> 重构施工图。本文自包含、可核对：每个结论带 `模块::函数` 锚点（已在 88fe66a3 上核对）。
> 勘察方式：只读代码勘察（无源码改动）；锚点均来自 `core/blitzkrieg_core`、`core/blitzkrieg_launcher`、
> `ui/webapp/webui`、`scripts` 下的当前工作区。

---

## 0. 结论速览

- 内核共有 **8 个 env 写域**；写最终都收敛到 `StoreApi`（`store/mod.rs`），EnvStore 后端下落到 `.env`（`env_file.rs` `upsert_env_vars` / `remove_env_vars`）。**其中只有 2 个域（风控六项 `BK_RISK_*`、出场纪律 `BK_EXIT_*`）有 DB 分支**——`trading_reset.rs` `persist_risk_bounds` 的 `save_risk` DB 分支 / `persist_exit` 的 `save_exit` DB 分支——条件是「DB 打开时写库、否则走 Store 的 env 载体」；其余 7 个域**无条件写 env 载体**。
- **读路径存在 7 个「写 DB / 读 env」或「写 env / 读进程环境」的不对称点**，最危险的 3 个：
  ① `risk.limits` 查询时从 `.env` 文件覆盖内存读出（DB 值被 env 旧值盖住）：读出走 `cockpit.rs` `overlay_risk_limits`，写侧 `trading_reset.rs` `persist_risk_bounds` DB 分支**不写 env**；
  ② env 文件 watcher 对任意 `.env` mtime 变化全量重派生 cockpit 状态（`session.rs` 2s mtime poll → `refresh_cockpit_from_env_file`）；
  ③ A3 已由 PR-2（#1204）收口：`session.rs` 启动开库块默认以 `default_db_path` 开库（fail-closed），与 launcher 导入同一数据库路径，默认部署的分裂脑已闭合。
- 回执语义：`persisted`（bool）出现在 10 个 wire 响应结构；`persistence`（"store"/"none"）**只在** `risk.setSystemic` / `risk.setExit` 两个回执**里由** `handlers/risk.rs` `with_persisted` 注入（`schema/mod.rs` `SystemicLimitUpdate`、`ExitSetUpdate`；`api/risk.ts` `persistence?: 'store' | 'none'`）。前端消费点：`SettingsRiskLimits.vue` `setSystemic`/`setExit` 回执文案、`AccountSwitcher.vue`、`api/risk.ts`、`api/accounts.ts`、`api/market.ts`、`api/rounds.ts`、`api/strategy.ts`。
- issue #1199 验收清单中的行号：绝大多数在 25f4c46d 上准确，已在本文重基线到 88fe66a3 并改以函数名锚点（§6 逐条核对）；**唯「round-duration-check.mjs 注释与代码相反」不属实**——真实的注释过期点在文件头 docblock（详见 §6）。

---

## 1. 写面基础设施（两个原语 + 一个路径解析器）

| 设施 | 位置 | 说明 |
|---|---|---|
| `.env` 写原语 | `env_file.rs` `upsert_env_vars` / `remove_env_vars` | 原子 temp+rename、0600、托管键外逐字节保留；返回写入/删除的键名。**所有** `.env` 写最终都走这两个函数（EnvStore 独占调用） |
| `.env` 读半边 | `env_file.rs` `parse_line` / `read_env_vars` | 与 launcher 解析器字节级同构 |
| 账户写面路径解析 | `service/config.rs` `account_env_write_paths` | 返回 `(env_path, legacy_toml)`；`env_file_path` 缺省时取 toml 同目录 `.env`；`accounts_config_path` 为 `None` 时整个写面禁用（回测/进程内测试） |
| DB 侧仓库 | `risk/account_store.rs` `AccountRiskDb` | SQLite：`account_risk`（DDL 于 `db_import.rs`）、`account_exit_policy`、`account_settings`；**仅** `save_risk` / `save_exit` 两个写方法，读在 `load_risk` / `load_exits`，只在开库时调用一次 |
| DB 开库条件 | `session.rs` 启动开库块 | 默认 `db::default_db_path` 打开（fail-closed，开失败拒绝启动；未设 = 不可能，生产启动中 env 回退已退役）；显式 `BLITZKRIEG_DB_PATH` 亦受同一守门 |

DB 打开时把 DB 行 overlay 到 env 派生的工作副本上（DB 更强源）：`assembly.rs` `open_account_risk_db`。

---

## 2. 写点权威清单（按域）

「后端条件」列：**DB-first** = `if let Some(db) = &self.account_risk_db { 写库 } else { 写 env 载体 } else { 内存 }`；
**无条件 env** = 只要 `env_file_path` / `store` 在场就走 env 载体（legacy toml 在场时拒绝，见 `book.rs` `refuse_while_legacy_toml`）；
**best-effort env** = 写失败只记日志不拒绝 RPC。

### 2.1 风控矩阵 `BK_RISK_*`（risk.setSystemic）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/risk.rs` `method::RISK_SET_SYSTEMIC` |
| service 写路径 | `trading_reset.rs` `apply_systemic_limits` |
| 后端条件 | **DB-first**：`trading_reset.rs` `persist_risk_bounds`（`save_risk` 进 `account_risk`）→ `store/mod.rs` `StoreApi::save_risk_bounds`（EnvStore impl）→ `.env`，else 内存 only、`persisted=false` |
| env 键映射 | `cockpit.rs` `env_key_for_field` / `env_key_for_field_account`（后缀/中缀两种拼写） |
| env 写调用点 | ① `handlers/risk.rs` IPC 兜底 upsert（仅当 service 层 `update.persisted==false` 时触发）；② `trading_reset.rs` `persist_risk_bounds` → `store.save_risk_bounds`（EnvStore → `.env`） |
| 键族 | `BK_RISK_MAX_SINGLE_LOSS_USD` / `BK_RISK_MAX_DAILY_DRAWDOWN_USD` / `BK_RISK_MAX_POSITION_SIZE` / `BK_RISK_MAX_CONSECUTIVE_LOSSES` / `BK_RISK_COOLDOWN_MINUTES` / `BK_RISK_MAX_ORDER_NOTIONAL_USD`（+ `_<ACCOUNT>` 后缀与 `BK_RISK_<ACCOUNT>_<FIELD>` 中缀） |

### 2.2 出场纪律 `BK_EXIT_*`（risk.setExit）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/risk.rs` `method::RISK_SET_EXIT` |
| service 写路径 | `trading_reset.rs` `apply_exit_set` |
| 后端条件 | **DB-first**：`trading_reset.rs` `persist_exit`（`save_exit` 进 `account_exit_policy`）→ env 分支：`store/mod.rs` `StoreApi::save_exit`（EnvStore impl），再 `refresh_cockpit_from_env_file` 从文件重派生缓存；写文件失败拒绝整个 apply → else 内存 only |
| env 键映射 | `cockpit.rs` `env_key_for_field*`（5 字段） |
| env 写调用点 | ① `handlers/risk.rs` IPC 兜底，条件同上；② `trading_reset.rs` `persist_exit` → `store.save_exit` |
| 键族 | `BK_EXIT_STOP_LOSS_PCT` / `BK_EXIT_TAKE_PROFIT_PCT` / `BK_EXIT_FORCE_EXIT_SEC` / `BK_EXIT_TRAILING_ENABLED` / `BK_EXIT_STRATEGY_EXIT_MAKER_FIRST`（+ 每账户后缀） |

### 2.3 账户簿 `ACCOUNT_*`（account.* 家族 + strategy.enable + 启动迁移）

所有写面函数集中在 `book.rs`，统一经 `env_upsert` → `env_file.rs` `upsert_env_vars` / `remove_env_vars`；**无条件 env**（前提：legacy toml 不存在，`book.rs` `refuse_while_legacy_toml`；无 `accounts_config_path` 时 service 层整体拒绝）。

| 写面函数（book.rs） | service 入口（`service/accounts/persist.rs` 除注明外） | IPC 入口（`handlers/account.rs` 除注明外） |
|---|---|---|
| `env_persist_account_venue` | `set_account_venue_persisting`（persist.rs） | `account.venue.set`（account.rs） |
| `env_persist_account_create` | `create_account_persisting`（persist.rs） | `account.create`（account.rs） |
| `env_persist_account_trading` | `set_account_trading_persisting`（persist.rs） | `account.trading.set`（account.rs） |
| `env_persist_account_status` / `env_persist_account_active` | `set_account_status_persisting`（persist.rs） | `account.unfreeze`（account.rs → `risk_admission/freeze.rs` `unfreeze_account` → persist） |
| `env_persist_account_strategies` | ① `set_account_strategies_persisting`（persist.rs）；② `service/strategies.rs` `set_scoped_strategy_enabled`（persist）；③ 启动一次性迁移 persist.rs | ① `account.strategies.set`（account.rs）；② `strategy.enable/disable`（handlers/strategy.rs） |
| `env_persist_account_rounds` | `set_account_rounds_persisting`（persist.rs） | `account.rounds.set`（account.rs） |
| `env_persist_account_remove` | `remove_account_persisting`（persist.rs） | `account.remove`（account.rs） |
| `env_persist_display_account` | `set_display_account_persisting`（persist.rs） | `display.account.set`（account.rs） |
| `env_persist_cross_venue_arb` / `env_clear_cross_venue_arb` | `arb_config_set_persisting`（persist.rs） / `arb_config_clear_persisting`（persist.rs） | `arb.config.set`（handlers/arb.rs）；clear 无 IPC 臂（仅测试调用） |
| `env_persist_account_opening_equity` | **非 IPC**：`service/accounts/mod.rs` `realign_venue_cash`（由 venue 现金报告触发，`market/host.rs`） | — |

### 2.4 执行策略 `POLICY_*`（execution_policy.set / reset）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/execution_policy.rs` `set` / `reset` |
| 写函数 | `handlers/execution_policy.rs` `policy_section_write`：先清扫该账户全部 section 键，再 upsert `Some` 字段 |
| 键族 | `POLICY_BUDGET_RATIO` / `POLICY_MIN_BUDGET_USD` / `POLICY_MAX_BUDGET_USD` / `POLICY_MIN_EQUITY_USD` / `POLICY_MAX_POSITIONS_PER_ASSET` / `POLICY_RULES`（+ `_<ACCOUNT>` 后缀；defaults 选择器写无后缀裸键），键清单的权威定义在 `execution_policy/env_state.rs` `section_env_state` |
| 后端条件 | **无条件 env**（无 `env_file_path` 拒绝；被编辑过的 legacy toml 拒绝；内核自动生成模板 toml 会被顺手删除） |
| 附带写 | 每次 set/reset 后 `reload_execution_policy` 从文件重读 |

### 2.5 轮次周期 `ACCOUNT_<ID>_ROUND_DURATION_SEC`（round_duration.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/round.rs` `ROUND_DURATION_SET` |
| service 写路径 | `trading_reset.rs` `apply_round_duration_set`，**持久优先两段提交**：`store/mod.rs` `StoreApi::save_account_round_duration`（EnvStore → `.env`），失败拒绝且无账簿副作用；随后撤单→市价强平→内存+引擎网格 |
| 键 | `ACCOUNT_<ID>_ROUND_DURATION_SEC`（id 折叠为大写字母数字） |
| 后端条件 | **无条件 env**；无 `env_file_path`（回测/进程内测试）跳过持久化直接内存执行 |
| 相关死写面 | `RUNTIME_ROUND_DURATION_SEC`：`runtime_config.rs` `persist_round_duration_to_with_env`，**无任何生产调用者**（全仓 grep 仅 runtime_config 自身与测试）；读侧 `main.rs` `RUNTIME_ROUND_DURATION_SEC` 读块（已标 deprecated） |

### 2.6 凭据（插件声明的凭据变量名）（market.credentials.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/market.rs` |
| 写函数 | `market/mod.rs` `apply_credentials_set`：耐久半边经 `core.store`（StoreApi，EnvStore → `.env`），不再直调 `.env` 写原语 |
| 键族 | 由插件 `required_credential_env()` 声明（如 `POLY_API_KEY` 等），逐插件校验 |
| 后端条件 | **无条件 env**（无 `store`/env 路径拒绝）；next_session 语义 |
| 启动一次性迁移 | `runtime_config.rs` `migrate_credentials_cell_to_env`，launcher `main.rs` 启动调用，把旧 config.json credentials 格排空进 `.env` |

### 2.7 行情插件选择 `RUNTIME_MARKET_PLUGIN`（market.plugin.set）

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/market.rs` `MARKET_PLUGIN_SET` |
| 写函数 | `market/mod.rs` `persist_cell` ↔ `runtime_config.rs` `persist_market_plugin_to_with_env`：**同时**写 JSON 格（`data/runtime/config.json`）与 env 键 |
| 后端条件 | 无条件双写（JSON 格 + env）；无 `runtime_config_path` 拒绝 |
| 启动迁移 | `market/mod.rs` `boot_enablement` 在账户 venue 绑定派生生效时清持久格——这是又一处 env 写 |

### 2.8 影子进化开关 `SHADOW_ENABLED` / `SHADOW_AUTO_EVOLVE`

| 项 | 证据 |
|---|---|
| IPC 入口 | `handlers/se.rs` `shadow_evolution.enable` / `disable` / `set_auto` |
| 写函数 | `shadow_evolution/mod.rs` `persist_switch` |
| 键 | `SHADOW_ENABLED` / `SHADOW_AUTO_EVOLVE` |
| 后端条件 | **best-effort env**（无 `env_path` 只 debug 日志；写失败 error 日志不拒绝）；无 DB 分支 |

### 2.9 （非 env，列出以免误伤）risk.setLimits → risk-state.json

`risk.setLimits`（`handlers/risk.rs`）→ `trading_reset.rs` `apply_risk_limits`→
`service/risk_admission/persist.rs` `risk_state_record_limits`（注意：模块在 `service/` 子目录下）写 `data/risk-state.json`（**不是 .env**）。
`persisted` 回执 = 该 JSON 文件写结果（`trading_reset.rs` `apply_risk_limits`）。此域不在 #1199 的 env 迁移范围内，
但 PR-1 的 StoreApi 抽象若要「全域覆盖」需决定是否把它一并纳入（建议纳入，否则又多一个直写文件的域）。

### 2.10 统计

- **env 写调用点**：业务层不再直接调 `.env` 写原语；全部收敛到 `StoreApi`，EnvStore 后端独占调用 `env_file.rs` `upsert_env_vars` / `remove_env_vars`（`store/mod.rs` EnvStore impl）。当前写面：`runtime_config.rs` `persist_market_plugin_to_with_env` / `persist_round_duration_to_with_env` / `migrate_credentials_cell_to_env`、`handlers/risk.rs` IPC 兜底、`handlers/execution_policy.rs` `policy_section_write`、`market/mod.rs` `apply_credentials_set`、`trading_reset.rs` `persist_risk_bounds` / `persist_exit` / `apply_round_duration_set`（`store.save_*` 簇）、`persist.rs` `env_persist_*` 簇、`shadow_evolution/mod.rs` `persist_switch`。
- 按域分布：账户簿 `ACCOUNT_*`（`book.rs` + `persist.rs` 簇）、轮次（`trading_reset.rs` `apply_round_duration_set` + deprecated `runtime_config.rs` `persist_round_duration_to_with_env`）、凭据（`market/mod.rs` `apply_credentials_set` + `runtime_config.rs` `migrate_credentials_cell_to_env`）、风控（`handlers/risk.rs` IPC 兜底 + `trading_reset.rs` `persist_risk_bounds`）、出场（`handlers/risk.rs` IPC 兜底 + `trading_reset.rs` `persist_exit`）、执行策略（`handlers/execution_policy.rs` `policy_section_write`）、插件选择（`market/mod.rs` `persist_cell`）、影子进化（`shadow_evolution/mod.rs` `persist_switch`）。
- 可触发写 `.env` 的 IPC 入口：**18 个**（按动词族计）——`risk.setSystemic`、`risk.setExit`、`market.credentials.set`、`market.plugin.set`、`execution_policy.set`、`execution_policy.reset`、`round_duration.set`、`account.venue.set`、`account.rounds.set`、`account.trading.set`、`account.unfreeze`、`account.strategies.set`、`display.account.set`、`account.create`、`account.remove`、`arb.config.set`、`strategy.enable/disable`、`shadow_evolution.enable/disable/set_auto`。另有 `risk.setLimits`（`handlers/risk.rs`）→ `trading_reset.rs` `apply_risk_limits` 写 `data/risk-state.json`，**不是** env 写口（见 §2.9）。
- 非 IPC 写者：**4 个**——venue 现金报告（opening equity，`service/accounts/mod.rs` `realign_venue_cash`，经 `market/host.rs`）、启动策略启用迁移（`persist.rs`）、`boot_enablement` 清持久格（`market/mod.rs` `boot_enablement`）、凭据格迁移（launcher `main.rs` `migrate_credentials_cell_to_env` 调用）。

---

## 3. 读路径勘察：写 DB / 读 env 的不对称点（迁移隐性地雷）

### 3.1 全部 env 读面（谁还在读 env / 进程环境）

| 读面 | 位置 | 读什么 |
|---|---|---|
| 全局风控 env 面 | `risk/limits.rs` `from_env` / `from_env_with_accounts` | 进程环境 `BK_RISK_*`（含每账户后缀/中缀）；launcher `main.rs` `SystemicRiskLimits::from_env` 启动时调用 |
| 每账户出场覆盖 | `config.rs` `account_exit_overrides`（经 `env_lookup_from_sources`） | `BK_EXIT_*_<ACCOUNT>` |
| 账户簿启动读 | `book.rs` `load_account_ledgers_from_env` + `env_lookup_from_sources`（进程环境优先、`.env` 文件兜底） | `ACCOUNT_LIST`、`ACCOUNT_<ID>_*`、`ACCOUNT_ARB_*` |
| 启动期账户标量 | `service/assembly.rs` 启动解析块 | `ACCOUNT_<ID>_ROUND_DURATION_SEC`、`ACCOUNT_<ID>_MIN_SHARES/_MAX_SHARES`、`ACCOUNT_<ID>_MARKET_PLUGIN`、`STRATEGY_<NAME>_ROUND_DURATION_SEC` |
| cockpit 文件面 | `cockpit_env.rs` `refresh_cockpit_from_env_file` → `env_file.rs` `read_env_vars`（**只读文件、不读进程环境**） | `COCKPIT_KEYS` 11 键（`cockpit.rs` `COCKPIT_KEYS`）+ 每账户行（`cockpit_env.rs` `apply_cockpit_vars`  per-account 段） |
| risk.limits 读出 | `handlers/risk.rs` risk.limits 处理块：先取内存快照再 `overlay_risk_limits`（`cockpit.rs` `overlay_risk_limits`）从**文件**覆盖 | 同上 11 键 |
| 执行策略读 | `execution_policy/mod.rs` `load_from_str`（进程环境）与 `load_single_source`（进程环境优先 + `.env` 文件兜底，issue #751/#967） | `POLICY_*` + 裸别名 |
| 凭据状态 | `market/mod.rs` `var_os`（**只读进程环境**） | 插件凭据变量 |
| 插件选择 | launcher `main.rs` `RUNTIME_MARKET_PLUGIN` 进程环境，outranks JSON 格 | `RUNTIME_MARKET_PLUGIN` |
| 轮次（deprecated） | launcher `main.rs` `RUNTIME_ROUND_DURATION_SEC` 读块 | `RUNTIME_ROUND_DURATION_SEC` |
| 影子进化 | `shadow_evolution/mod.rs` 启动 config（`.env` 的 `SHADOW_ENABLED`/`SHADOW_AUTO_EVOLVE` 或 legacy `BK_SE_*`） | `SHADOW_*` / `BK_SE_*` |
| DB 读 | `risk/account_store.rs` `load_risk` / `load_exits`，仅在 `assembly.rs` `open_account_risk_db` 时一次 | `account_risk`、`account_exit_policy` |
| db_import | `db_import.rs` `env_lookup` + `account_ids` | 一次性导入 `ACCOUNT_*` / `BK_RISK_*` / `POLICY_*` → SQLite（`kernel_meta.import_v1` 幂等标记） |

### 3.2 不对称点清单（7 个）

| # | 不对称 | 证据 | 后果 |
|---|---|---|---|
| A1 | **risk.limits 读出从 `.env` 文件覆盖内存**（DB 开着也一样） | `handlers/risk.rs` risk.limits 处理块（`read_env_vars` + `overlay_risk_limits`）；写侧 DB 分支**不写 env**（`trading_reset.rs` `persist_risk_bounds`） | DB 开库 + `.env` 残留旧 `BK_*/BK_EXIT_*` 键时：面板读出 = env 旧值，引擎实际用 DB 新值。写 DB 后读 env 的**直接分裂** |
| A2 | **env 文件 watcher 全量重派生 cockpit**（任意 `.env` 写都触发） | `session.rs` env-file watcher（2s mtime poll → `refresh_cockpit_from_env_file`）；`cockpit_env.rs` `apply_cockpit_vars` 对文件在场键逐个覆盖内存，含每账户行 | 任何无关 `.env` 写（账户开关、凭据、轮次、POLICY 保存——它们都重写同一个文件）都会让 watcher 把 DB 载入的内存值**用文件旧键覆盖**，直到下次重启/写 DB |
| A3 | **已收口（#1204 PR-2）** | `session.rs` 启动开库块（默认 `default_db_path` 开库，fail-closed）与 launcher `main.rs` 导入同一路径 | 默认部署导入后即开库，不再存在“面板写 env、DB 冻结”的分裂脑 |
| A4 | **账户簿：写 env，导入 DB 后无读侧** | 写：`book.rs` 全部写面；导入：`db_import.rs` `insert_account`；运行期读 `accounts` 表：**无**（全仓 `FROM accounts` 仅 `db_import_tests.rs` 与 `db/mod.rs` 测试） | 导入的 `accounts` 表是孤儿数据；任何 `.env` 账户写后 DB 与 env 永久分叉 |
| A5 | **RUNTIME_ROUND_DURATION_SEC：死写面 / 活读面** | 写：`runtime_config.rs` `persist_round_duration_to_with_env` 无生产调用者；读：`main.rs` `RUNTIME_ROUND_DURATION_SEC` 读块（deprecated 警告） | PR-1 删写面时不能连读面一起删（兼容旧部署），需成对处理 |
| A6 | **execution_policy：导入 DB 后无运行期读** | 导入：`db_import.rs` `SETTING_KEYS`（`BUDGET_RATIO` 等，落 `account_settings`）；读：`execution_policy/mod.rs` `load_single_source`（env 单源） | 同 A4：DB 中 policy 键无读者；PR-2 需补读侧或删导入 |
| A7 | **凭据 status 读进程环境，写 `.env` 文件** | 写：`market/mod.rs` `apply_credentials_set`（经 store）；读：`market/mod.rs` `var_os`（`std::env::var_os`） | 非 DB 不对称，但同属「写面/读面不同层」：写后本进程 `credentials.status` 不翻绿（重启才载）——`AddAccountDialog.vue` 文案即此语义 |

**最危险的 3 个**：A1（面板读出错值，操作员基于假读数决策）、A2（无关写操作静默复活旧配置）、A3（已收口，见上）。

---

## 4. 回执语义（persisted / persistence）

### 4.1 内核侧

| 字段 | 出现位置 | 语义 |
|---|---|---|
| `persisted`（bool） | `schema/mod.rs` `RiskLimitUpdate` / `SystemicLimitUpdate` / `ExitSetUpdate`；`schema/views.rs` `AccountVenueSetResult` / `AccountRoundsSetResult` / `AccountTradingSetResult` / `AccountStrategiesSetResult` / `DisplayAccountSetResult` / `AccountCreateResult` / `AccountRemoveResult` | 落盘是否成功；账户家族写失败会拒绝整个 RPC（persisted 恒 true），风控两域可能为 false（无 env 文件/写失败仅告警） |
| `persisted`（恒 false） | `service/risk_admission/fee.rs`（`RoundDurationResult`，round_duration.get 读出） | 硬编码 false——该卡片的真实持久化在 `round_duration.set` 回执侧没有对应字段 |
| `persistence`（"store"/"none"） | **仅** `handlers/risk.rs` `with_persisted`，注入 `risk.setSystemic` / `risk.setExit` 两个回执 | 拼写载体；`schema/mod.rs` 对应字段有契约注释 |

### 4.2 前端消费（grep ui/）

| 消费点 | 位置 | 用法 |
|---|---|---|
| `api/risk.ts` | `setSystemic`/`setExit` 回执类型，`persistence?: 'store' \| 'none'`，注释明令「禁止只显示「已保存」」 | 类型 + 契约注释 |
| `SettingsRiskLimits.vue` | `setSystemic` / `setExit` 回执文案（“已持久化” / “未持久化，重启回到启动配置”，按 `persistence` 值域） | #1199 违约点 2 已由 #1202 收口（文案存储中性） |
| `AccountSwitcher.vue` | `res.persisted ? '（已落盘）' : ''` | 存储中性，合规 |
| `api/accounts.ts` | venue/display/trading/strategies/create/remove/rounds 回执的 `persisted` | 类型声明 |
| `api/market.ts` | `persisted`、`source?: 'accounts'\|'persisted'\|'flag'\|'fallback'` | 类型声明 |
| `api/rounds.ts` | `RoundDurationDoc.persisted`（内核恒 false） | 类型声明（当前无 .vue 消费方——`api/rounds` 尚无导入者） |
| `api/strategy.ts` | `persisted` | 类型声明 |
| `stores/settings.ts`、`HftPage.vue`、`OnboardingTour.vue`、`lib/theme.ts` 等 | — | 均为 localStorage 前端偏好（E8-d），与内核 env/SQL 无关，不在迁移面 |

---

## 5. PR-1 / PR-2 / PR-3 施工面

### PR-1：StoreApi trait + EnvStore 实现（行为不变，纯重构）

**目标**：所有业务写口改调 `StoreApi`；后端仍是 EnvStore（写 `.env`，逐字节不变）；门禁绿。

| 施工项 | 面 | 证据（改动点） |
|---|---|---|
| 定义 trait | 新增 store 模块 | 按域分组：账户簿 / 风控 / 出场 / 交易开关 / venue / rounds / strategies / strategy.enable / execution_policy / 面板设置（round duration / market plugin / shadow 开关 / 凭据）。契约不带 env/SQL 语义 |
| EnvStore 实现 | 包装现有原语 | `env_file.rs:43`/`:73` 不动；把 `book.rs` 的 `env_persist_*` 簇 + `env_clear_cross_venue_arb`、`cockpit_env.rs`（`persist_cockpit_changes` 已退役，现仅 `refresh_cockpit_from_env_file` 读）、`handlers/execution_policy.rs` `policy_section_write`、`runtime_config.rs` `persist_market_plugin_to_with_env` / `persist_round_duration_to_with_env` / `migrate_credentials_cell_to_env`、`market/mod.rs` `apply_credentials_set`、`shadow_evolution/mod.rs` `persist_switch` 全部改写为 trait impl 后的调用 |
| 业务写口改造 | 18 个 IPC 入口 + 4 个非 IPC 写者 | §2 各表的「service 入口」列；`trading_reset.rs` 的 `if let Some(db)` DB 双分支已归零为单一 store seam（`persist_risk_bounds` / `persist_exit` / `apply_round_duration_set`），DB 分支留给 PR-2 的 SqliteStore |
| 读面 | **不动**（EnvStore 读 = 现状） | A1/A2 的不对称在 PR-1 保留（行为不变），PR-2 随读侧迁移一并修复 |
| 测试 | 内存 Store 验证 | issue 验收「换后端 = 改一处装配」需要一组内存 Store 测试（`tests/store_backend_parity.rs`） |

**注意**：`risk.setLimits`（risk-state.json）与 `strategy-state.json`（`persist.rs` 默认账户镜像）、`data/runtime/config.json` 格（`runtime_config.rs`）是**非 env 文件写面**，PR-1 需决定是否纳入 trait（建议纳入「面板设置」域，否则 PR-3 的「业务层无直接文件写」红线挂一漏万）。

### PR-2：SqliteStore 补齐 + 默认开库统一 + 导入幂等（切换默认后端）

| 施工项 | 现状 | 缺口 |
|---|---|---|
| 风控/出场表 | `account_risk`（`account_store.rs` DDL）、`account_exit_policy`、`account_settings` | 已有读写；需接 SqliteStore 并删除 env 回退（`assembly.rs` `open_account_risk_db` 改为唯一源） |
| 账户簿表 | `accounts` 表已由 db_import 导入（`db_import.rs` DDL / `insert_account`） | **无运行期读侧**（A4）；需写 SqliteStore 读写并让 `book.rs` `load_account_ledgers_from_env` 退役 |
| 执行策略 | db_import 已导入 POLICY_*（A6） | 无运行期读侧；需 store 读写 |
| 轮次周期 | 无表无读 | 需新表 + 读写（含 `ACCOUNT_<ID>_ROUND_DURATION_SEC` 启动解析退役，`service/assembly.rs` 启动解析块） |
| 凭据 | `db/credentials.rs` 已有 AES-256-GCM 密文设施；README 已宣称 | 需接 store；`market/mod.rs` 状态读改走解密读（`var_os` 仍只读进程环境，见 A7） |
| 影子开关 / market plugin / round 设置 | 无表 | 需表 + 读写；`RUNTIME_*` 读面（`main.rs`）退役或保留兼容 |
| 默认开库统一 | 已统一（#1204）：`session.rs` 启动开库块默认 `default_db_path`，fail-closed | 导入保持幂等（`import_v1` 标记，`db_import.rs`）；A3 已消除 |
| DB 未开库 fail-closed | SqliteStore 缺库时**拒绝写入**（issue 验收红线）；A1/A2 的读侧 overlay 随 store 读迁移一并拆除 | — |

### PR-3：UI 文案清理 + env 运行期写入 CI 门禁（收口，#1202 已合入）

| 施工项 | 位置（88fe66a3 快照） | 说明 |
|---|---|---|
| 「已持久化到 .env」 | `SettingsRiskLimits.vue` `setSystemic`/`setExit` 回执文案 | #1202 已改存储中性（“已持久化” / “未持久化，重启回到启动配置”，按 `persistence` 值域）；`api/risk.ts` 注释已同步（`persistence?: 'store' | 'none'`） |
| 「全局级限额」分组 | `SettingsRiskLimits.vue` 风险分组 | #983 起风控只有账户级，#1202 已清 |
| 配置停用/闸门 .env 文案 | `SettingsRisk.vue`、`AddAccountDialog.vue`、`SettingsGeneral.vue` | 「改 .env 并重启」全套改 API 口径（#1202） |
| 凭据 .env 文案 | `AddAccountDialog.vue` | 同上（#1202） |
| envReload / 凭据格文案 | `SettingsGeneral.vue` | 同上（#1202） |
| README 口径 | `README.md`「路线 A」/「Q：数据存哪里？」 | 已与本次审计一致化（见任务 P1-6：账本/交易改 JSONL，账户簿/风控 SQLite/.env） |
| round-duration 门禁 | `scripts/round-duration-check.mjs` | 断言改走 API 口径；过期注释清理（见 §6） |
| env 写入 grep gate | `scripts/store-layer-check.mjs` | 业务层出现 `env_persist_*` / `upsert_env_vars` / `remove_env_vars` / 直接 SQL = 红灯（仅 store 实现模块豁免） |

---

## 6. issue #1199 验收清单行号核对（重基线：88fe66a3；锚点为函数名）

| issue 声明 | 核对结果 | 现行证据（88fe66a3） |
|---|---|---|
| `trading_reset.rs:292`（setSystemic DB 条件） | 行号漂移，**锚点有效** | `trading_reset.rs` `persist_risk_bounds` → `db.save_risk`（`account_risk` DB 分支） |
| `handlers/risk.rs:242`（env path 读取） | 行号漂移，**锚点有效** | `handlers/risk.rs` `risk.setSystemic` 处理块（`env_file_path`） |
| `trading_reset.rs:487`（setExit DB 条件） | 行号漂移，**锚点有效** | `trading_reset.rs` `persist_exit` → `db.save_exit`（`account_exit_policy` DB 分支） |
| `persist.rs:512`（account.trading.set 无条件 env） | 行号漂移，**锚点有效** | `service/accounts/persist.rs` `set_account_trading_persisting` |
| `trading_reset.rs:625` 起（round_duration.set） | 行号漂移，**锚点有效** | `trading_reset.rs` `apply_round_duration_set` → `store.save_account_round_duration` |
| `execution_policy/env_state.rs`（POLICY_* 写面） | **准确**（写函数在 handlers，键清单在此） | 写：`handlers/execution_policy.rs` `policy_section_write`；键：`execution_policy/env_state.rs` `section_env_state` |
| `session.rs:43`（条件开库） | **行为已变**：默认开库（#1204） | `session.rs` 启动开库块（`default_db_path`，fail-closed） |
| `main.rs:1296`（launcher 无条件导入） | 行号漂移，**锚点有效** | `main.rs` `import_once` 调用块（默认 `DEFAULT_DB_PATH`） |
| `assembly.rs:10`（DB 行更强源） | **准确** | `assembly.rs` `open_account_risk_db` 文档注释 |
| `SettingsRiskLimits.vue:344`（「全局级限额」） | **已收口（#1202）** | 风控分组已无「全局」残留 |
| `SettingsRiskLimits.vue:423/:432`（「已持久化到 .env」） | **已收口（#1202）** | 文案改存储中性，按 `persistence` 值域显示 |
| `SettingsRisk.vue:349-376` | **已收口（#1202）** | `.env` 文案改 API 口径 |
| `AddAccountDialog.vue` | **已收口（#1202）** | 凭据 `.env` 文案改 API 口径 |
| `SettingsGeneral.vue` | **已收口（#1202）** | envReload / 凭据格文案改 API 口径 |
| `README.md:252` | **已一致** | README「配置与旋钮」段：账户与风控在 SQLite，与默认开库行为一致 |
| `scripts/round-duration-check.mjs:69`「注释与代码相反」 | **不属实** | `:69-71` 断言与代码一致；真实过期点在**文件头 docblock**（第 3 条仍写「立即生效：本会话 perAccount.default=300（内存 map 即时更新）」，而 #1184 起代码是破坏性的秒切秒换，docblock 漏改） |

---

## 7. 给后续 agent 的注意事项

1. **不要按行号定位代码**。本文件余下的 `:NNN` 只是 88fe66a3 的快照，会随着 PR 漂移；改动前以 `模块::函数` 锚点定位（如 `trading_reset.rs` `persist_risk_bounds`），再用 `rg "fn <name>"` 核对当前行号。
2. **不要动 `env_file.rs` 的写原语**：PR-1 的 EnvStore 直接包装它；它同时是读契约（launcher 解析器同构，`env_file.rs` 测试钉住 round-trip）。
3. **`account_env_write_paths`（`service/config.rs`）是账户写面的总闸**：`persist.rs` 的 `env_persist_*` 簇 + `service/strategies.rs` `set_scoped_strategy_enabled` + `service/accounts/mod.rs` `realign_venue_cash`（opening equity）经它解析路径；trait 化时它是天然的 EnvStore 装配点。
4. **legacy toml 拒绝逻辑**（`book.rs` `refuse_while_legacy_toml`、`handlers/execution_policy.rs` `policy_section_write`）是行为的一部分，迁移时保留语义（EnvStore 内）。
5. **watcher（`session.rs` 2s mtime poll → `refresh_cockpit_from_env_file`）与 `risk.limits` 读出（`cockpit.rs` `overlay_risk_limits`）是 A1/A2 的两半**：PR-2 拆 env 读侧时必须成对处理，否则拆一半更危险。
6. **`round_duration.get` 的 `persisted` 恒 false**（`schema/mod.rs` `RoundDurationResult`）是既存小谎，PR-3 文案清理时顺带处理。
7. **`market/mod.rs` 凭据 status 读只走进程环境（`var_os`），写经 store 走 `.env`**（A7）：写后本进程不翻绿，重启才载——不要在文案里暗示即时生效。
