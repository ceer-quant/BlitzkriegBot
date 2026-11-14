# #1199（0.4.2）第六轮审计报告：env 写入路径根除 + SQLite 唯一后端

## 1. 审计范围与基线

| 项 | 事实（可追溯） |
|---|---|
| 工作树 | `/Volumes/Hard Disk/bk-worktrees/wt-env-readonly` |
| 分支 | `fix/env-readonly-sql-only`（基于 `41d77f09`，堆叠在 `58047212` 上） |
| 版本必检 | `grep -m1 '^version' Cargo.toml` → `version = "0.4.2"`（符合） |
| 提交风格 | 约定式提交，如 `fix(core): ... (#1221 #1223)`（见 `git log --oneline -5`） |
| 操作者裁决 2026-11-14 | 内核删除一切 env 写入路径，所有数据写 SQLite；`.env` 只读，唯一写入者是安装器 |
| 本轮定位 | #1199 第六轮审计（收尾 + 审计交付），`store/` 层既有债务与 cap 读口 `.ok()` 等**不属本轮**，不在此修改 |

审计结论以**实际编译/测试结果**为准，不编造数字，不可追溯处标注「未知」。

---

## 2. 改动清单

`git diff --stat`（工作树相对 `41d77f09`）：

- 40 个文件变更，**1590 行插入 / 3537 行删除**（净 -1947 行）
- 新增文件 1 个：`core/blitzkrieg_core/src/ipc/server/tests/tests_env_readonly.rs`

主要文件（按 diff stat）：

| 文件 | 增/删 | 本轮涉及点 |
|---|---|---|
| `core/blitzkrieg_core/src/account/config/book.rs` | 0 / 517 | env 写入函数全删（`EnvStore` 书写面） |
| `core/blitzkrieg_core/src/account/config/config_tests.rs` | 0 / 1268 | 随 `book.rs` env 写面一并退役的测试 |
| `core/blitzkrieg_core/src/env_file.rs` | +/−251 | 只读化，写原语已删（仅余 `parse_line` / `read_env_vars`） |
| `core/blitzkrieg_core/src/db/credentials.rs` | +/−105 | 凭证 AES-256-GCM 落 `kernel_meta.credential.*` + boot 注入 `BLITZKRIEG_DB_KEY` |
| `core/blitzkrieg_core/src/risk/account_store.rs` | +209 | `save_account_row` / `save_credential_names` 新增；fail-closed；`load_settings()`/`load_accounts()`/`load_exits()` SQL 读回 |
| `core/blitzkrieg_core/src/service/assembly.rs` | +/−472 | `with_account_book` 以 `config.db_path` 装配 `SqliteStore` + `install_risk_db`（账簿物化、轮次锚点、策略集合、进化开关句柄同一连接） |
| `core/blitzkrieg_core/src/service/config.rs` | +/−31 | `db_path` 字段（SQLite 唯一后端，`Some` 时 fail-closed 装配） |
| `core/blitzkrieg_core/src/service/accounts/persist.rs` | +/−14 | `set_account_venue`/`set_account_trading`/`set_account_status`/`set_display_account`/`create_account_persisting`/`remove_account_persisting` 皆检查 `self.store`，写 SQLite，不再写 `.env` |
| `core/blitzkrieg_core/src/ipc/server/handlers/execution_policy.rs` | +49 | `policy_toml_gate`（内核模板清扫 / operator 文件按名拒绝），set/reset 均调用 |
| `core/blitzkrieg_core/src/store/sqlite.rs` | +/−143 | `SqliteStore` 持久层；`save_policy_section` reset 语义改为「account_settings 无 POLICY_ 行 → 按名拒绝且不 ensure 行」 |
| `core/blitzkrieg_core/src/store/mod.rs` | +/−408 | `EnvStore` 后端退役，`StoreApi` 以 `SqliteStore` 为唯一实现 |
| `core/blitzkrieg_core/src/service/trading_reset.rs` | 0 / 23 | env 写分支删除 |
| `core/blitzkrieg_core/src/runtime_config.rs` | +/−173 | env 写分支删除 |
| `core/blitzkrieg_core/src/market/mod.rs` | +/−31 | env 写分支删除 |
| `core/blitzkrieg_launcher/src/main.rs` | 0 / 29 | 启动链中 env 写入分支删除（`db_path` 由 `session::run` 设为 `db::default_db_path()`） |
| `ui/ui_kit_panel/src/bin/blitzkrieg.rs` | +/−48 | 面板二进制随店后端一致 |
| `.env.example` | +/−30 | 更新为 SQLite 口径（安装器唯一写入者） |
| 测试文件 11 个 | 各 ±2～108 | fixture 补 `db_path: Some(dir.join("store.sqlite"))`，env 读回断言 → SQL 读回（`load_accounts()` / `load_settings()` / `load_risk()` / `load_exits(&ExitConfig::default())`） |

已迁移并验证绿的测试文件：`tests_account_books`、`tests_account_settings_write`、`tests_blueprint`、`tests_execution_policy`、`tests_feed_reuse`、`tests_risk_rounds`、`tests_risk_rounds_carrier`；本轮新修的 7 个失败测试（见 §4）。

---

## 3. 编译门证据

全部在 `CARGO_TARGET_DIR="/Volumes/Hard Disk/BlitzkriegBot/worktrees/wt-1140-account-round-duration/target"` 下执行，包名 `blitzkrieg-core`：

| 门禁 | 命令 | 结果 | 证据 |
|---|---|---|---|
| lib 全量测试 | `cargo test -p blitzkrieg-core --lib` | **1417 passed / 0 failed** | 与 handoff 预期「约 1417」一致；守门测试 `the_write_battery_never_touches_the_env_file` 含其中并为绿 |
| workspace 编译 | `cargo check --workspace` | **无 error** | `blitzkrieg-core` / `blitzkrieg-launcher` / `blitzkrieg-ui-panel` 均通过 |
| env 写残留扫描 | `grep -rn "upsert_env_vars\|remove_env_vars\|EnvStore\|env_persist_" core/`，剥离 `path:lineno:` 前缀与注释行后 | **0 处代码级残留** | 命中项全部为 `//!`/`///`/`//` 文档或历史注释；`env_file.rs` 仅余 `parse_line` / `read_env_vars` 两只读原语 |

守门测试契约（`tests_env_readonly.rs`：`the_write_battery_never_touches_the_env_file`）：连续发送 `round_duration.set`、`account.venue.set`、`market.credentials.set` 三笔写后，`.env` 必须**逐字节不动**（`KEEP=1` / `POLYMARKET_PRIVATE_KEY="legacy"` 保持 seed）；全部事实必须落进 SQLite（轮次锚点 `300` → `account_settings`，venue 绑定 → `accounts` 行，凭证 → 加密 `kernel_meta`，明文 `stub-value` 不得出现在任一文件）。该测试为绿，且任何一次失败即按「env 写入路径复活」的违约处理。

---

## 4. 待办 B 逐条收尾（已全部验证绿）

先跑全量确认（1410 passed / 7 failed），7 个失败精确对应 handoff 清单；每条按「固定修法」：fixture `CoreConfig` 加 `db_path: Some(dir.join("store.sqlite"))`，env 读回断言 → SQL 读回，env 逐字节不动守卫保留。

| # | 测试 | 失败根因 | 修法 | 现状 |
|---|---|---|---|---|
| 1 | `journal_cursor_tests::an_account_remove_kills_only_its_own_venues_cursors` | fixture 缺 `db_path`，`remove_account_persisting` 因无 store 拒绝 | 补 `db_path` | 绿 |
| 2 | `tests_venue_binding::venue_set_mounts_a_never_booted_venues_data_plane_at_runtime` | fixture 缺 `db_path`（CoreConfig ~73） | 补 `db_path` | 绿 |
| 3 | `account_gate_tests::unfreeze_writes_env_back_and_entries_stay_open_after_rebuild` | status/trading env 半边；`.env` 读回断言 | 补 `db_path`；断言改为 `AccountRiskDb::load_accounts()` 的 `status=active` / `trading_enabled=true` 行，并保留 `.env` 逐字节不动守卫 | 绿 |
| 4 | `tests_strategy_verbs::default_enable_persists_the_env_half_without_the_legacy_toml` | 耐久半边是 `account_settings.strategies` 行 | 补 `db_path`；读回用 `load_settings()` | 绿 |
| 5 | `dispatch_boot_enablement_tests::account_create_seeds_a_runtime_mint_on_a_locally_settling_core` | fixture 缺 `db_path` | 补 `db_path`（dry 与 live 核心各用独立 store 文件） | 绿 |
| 6 | `dispatch_boot_enablement_tests::display_account_set_persists_and_validates_plan_first` | fixture 缺 `db_path`；`ACCOUNT_DISPLAY` env 断言过时 | 补 `db_path`（c2 用独立 `store-c2.sqlite`）；断言改为 `load_kernel_meta(META_DISPLAY_ACCOUNT)` 与同 store 再开一核心的 `display_id()` 复原，`.env` 改为逐字节不动守卫 | 绿 |
| 7 | `tests_account_reset::account_remove_sweeps_both_sides_and_the_archaeology` | fixture 缺 `db_path` | 补 `db_path` | 绿 |

待办 A 两个文件（`tests_risk_rounds.rs`、`tests_risk_rounds_carrier.rs`）在本轮起已是 SQL 读回形态，本次全量中均为绿，无需改动。

---

## 5. 遗留与下一步（不属本轮，不在此修改）

按 handoff 约定，以下既有遗留**不计入本轮**，不弱化任何断言，也不静默降级：

- `store/` 层既有 6 条债务：具体条目未在此次审计中枚举（**未知**，不编造）。
- cap 读口 `.ok()` 等既有遗留：**未知**具体形状，不属本轮范围。
- `.env` 除安装器外零写入是本轮硬约束，已由守门测试 + grep 扫描兜底。

后续建议：在上述既有债务上单独开轮次处理，不与 #1199 第六轮合并。

---

## 6. 审计结论

**通过。** 内核已无代码级 env 写入路径（`upsert_env_vars` / `remove_env_vars` / `EnvStore` / `env_persist_` 零代码级残留）；所有账户/风控/轮次/策略/凭证写面落进 SQLite；`.env` 对内核只读，由守门测试 `the_write_battery_never_touches_the_env_file` 钉死。编译门全绿（`cargo test -p blitzkrieg-core --lib` 1417/0，`cargo check --workspace` 无 error），版本 `0.4.2` 符合必检项。
