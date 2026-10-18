# cross_venue_arb 验收矩阵（feat/cross-arb-atomic）

PR 验收章素材。正向 3 条 + 反向 5 条，每条给：判据、对应测试名 / 静态核对位置、当前状态。

- 「绿」= 测试已在本 worktree 跑过并通过（命令见文末）。
- 「牙（已验红）」= 反向测试已验证当前确实失败（有牙），并以
  `#[ignore = "EXPECTED-RED-WAITING: ..."]` 挂起，等在途产物落地后摘除
  ignore 转绿；`cargo test -- --ignored` 可随时复现红。
- 「待收口」= 依赖 Agent-内核 / Agent-UI 在途产物，落定后补验。

状态基线：2026-10-18，工作树仅含验收官新增的两个文件（`tests/cross_arb_reverse_check.rs`、本文档）；manifest 尚无 `atomic_batch` 字段，`run_pair_orphan_sweep` 尚未出现在 `core/blitzkrieg_core/src/`。

## 正向（机制应当工作）

| # | 判据 | 测试 / 核对位置 | 状态 |
|---|------|----------------|------|
| P1 | **配对纪律**：仅 `status=paired` 且零 discrepancy 的 unified 映射开价；缺腿、带差异标记、无映射、无费用表一律不开（fail closed）。REAL 包经 `LuaEngineAdapter` 驱动 | `tests/cross_venue_arb_strategy_check.rs`：`refuses_an_event_with_discrepancies`、`refuses_a_single_leg_mapping`、`refuses_without_a_unified_mapping`、`refuses_without_a_fee_schedule` | 绿（既有测试） |
| P2 | **经济与容量**：折价 > `spread_open_pct` 才开；两腿各按自己 venue 的 ask、等量、双腿 taker 费用计入成本；容量 = min(两腿 ask 深度)；完整 live 引擎回放两腿都成交且不超细腿 | `tests/cross_venue_arb_strategy_check.rs`：`a_discounted_cross_venue_pair_suggests_both_legs`、`capacity_never_exceeds_either_legs_depth`、`refuses_a_discount_thinner_than_the_open_spread`；`tests/cross_venue_replay_check.rs`：`replayed_pair_fills_both_legs_and_never_exceeds_the_thin_leg` | 绿（既有测试） |
| P3 | **生命周期**：持有中折价回落至 `spread_close_pct` 带内 ⇒ 两腿经 exit 释放；回放中 enter+release 全链闭合 | `tests/cross_venue_arb_strategy_check.rs`：`a_held_pair_releases_when_the_discount_falls_back`；`tests/cross_venue_replay_check.rs`：`replayed_release_flattens_the_pair` | 绿（既有测试） |

## 反向（必须红 / 必须有牙）

| # | 判据 | 测试 / 核对位置 | 状态 |
|---|------|----------------|------|
| R1 | **同所仍允许启动 → 拒**：operator 对两个同 venue listing 调 `unified.confirm` ⇒ `Err` 且文案含 "same venue"，映射数为 0；对照组：同标题跨所对确认成功（证明拒的是同所而非标题） | `tests/cross_arb_reverse_check.rs`：`confirm_refuses_two_listings_from_the_same_venue`（机制：`market_api/src/unified/ledger.rs` `ConfirmError::DuplicateVenue` → `service.rs` `confirm_error_text`） | 绿（机制已有） |
| R2 | **无预占致裸腿 → 原子批整组拒**：`cross_venue_arb` 装上 pot（`CoreConfig::virtual_limits`，cap 4.00 < 两腿合计）⇒ 0 腿到 OME、`strategy_stats.rejectionCauses["limit.virtualCap"] == 2`、pot 分文不占；对照组 cap 放宽后两腿放行 | `tests/cross_arb_reverse_check.rs`：`atomic_batch_refuses_the_whole_pair_when_pot_is_too_small`（依赖 manifest `"atomic_batch": true`） | 牙（已验红：现逐腿路径放行 2 腿——正是要咬的洞）。待收口：Agent-内核给 manifest 打标 |
| R3 | **Fallback 未触发 → 看护必须动作**：一腿成仓、对手腿终态消失（无持仓/无 live BUY/不在 inflight）、过 5s 宽限 ⇒ 一次 tick ⇒ RiskAlert 到达且裸腿被 flatten（持仓消失） | `tests/cross_arb_reverse_check.rs`：`orphan_sweep_flattens_a_naked_leg_after_the_grace_window`（依赖 `run_pair_orphan_sweep`） | 牙（已验红：宽限后 tick 裸腿存活）。待收口：Agent-内核落地看护 |
| R4 | **DryRun 假完美成交（风险层孪生）**：FillModel 恒成交吃满深度是仓库级已知披露（memory 有案）；此处钉其风险层孪生——`virtual_batch_fits` DRY-RUN 预检后 `virtual_used == 0`（check-then-hold 分离，×2 双重计费不复活）。单元侧 `batch_is_all_or_nothing`、`release_is_precise_per_leg` 同守此不变量 | `tests/cross_arb_reverse_check.rs`：`dry_run_precheck_never_holds_the_pot`；`src/risk/virtual_account.rs` 单元测试 | 绿 |
| R5 | **UI 只改状态后端未监听**：逐按钮静态核对——每个动作的 IPC 动词在内核 `schema.rs` method 表有常量、`ipc/server.rs` 有 match 臂、失败路径在 UI 有呈现（不得静默吞错）。核对表见下节 | 本文档「UI 静态核对表」 + `tests/cross_arb_reverse_check.rs` 的内核侧近亲 `flatten_of_an_unknown_position_moves_nothing` | 待收口（Agent-UI 产物未在工作树） |

### R5 · UI 静态核对表（逐按钮）

桥接链：`webui/src/api/client.ts` → `ui/ui_kit/src/web/mod.rs` HTTP 桥 → IPC method 常量 → `ipc/server.rs` handler → `Core`。

| 按钮/动作 | client.ts | HTTP 桥 | IPC 动词 | 内核 handler | 判据 | 状态 |
|-----------|-----------|---------|----------|--------------|------|------|
| 候选确认（弹窗确认） | `unifiedConfirm` POST `/unified/confirm` | `web/mod.rs` `POST /api/unified/confirm` → `"unified.confirm"` | `unified.confirm` | `server.rs:1896` → `Core::unified_confirm` | 拒绝文案（含 "same venue"）必须原样浮到弹窗，不得吞错；确认成功刷新 `unifiedList` | 待收口（CreateArbDialog.vue 未交付；client.ts / 桥 / handler 三段已就位） |
| 映射撤销 | `unifiedRevoke` POST `/unified/revoke` | `web/mod.rs` → `"unified.revoke"` | `unified.revoke` | `server.rs` → `Core::unified_revoke` | 撤销后 `unified.list` 立即少一行 | 待收口（同上） |
| 配对视图（只读） | `unifiedList` GET `/unified` | `web/mod.rs` → `"unified.list"` | `unified.list` | `server.rs:1874` | venue 行为数据渲染，无 venue 分支 | 读路径已就位；视图归属 Agent-UI 产物 |
| 候选视图（只读） | `unifiedCandidates` GET `/unified-candidates` | `web/mod.rs` → `"unified.candidates"` | `unified.candidates` | `server.rs:1883` | 同上 | 同上 |
| 平仓/flatten 动作 | `positions.exit` 调用方 | （UI 侧待 Agent-UI 产物核对） | `positions.exit` | `server.rs:1581` → `Core::flatten` | 后端不认识的目标返回 `closed: 0`（零动作），UI 不得渲染成「已平仓」 | 内核侧语义已由 `flatten_of_an_unknown_position_moves_nothing` 钉住（绿）；UI 侧待收口 |
| `--virtual-limit` 旗标（如 CLI 暴露） | — | — | — | `CoreConfig::virtual_limits` 解析（`service.rs:235`） | 配置进 pot、`R2` 测试消费同一形状 | 待收口（Agent-内核 CLI 段） |

核对方法：对上表每一行，读 Agent-UI 产物中的按钮 handler，确认它调用的正是表中动词；凡 UI 自改本地状态而不发动词（或动词拼写不在 `schema.rs` method 表）即为「只改状态后端未监听」，必须打回。

## 复现命令

```sh
CARGO_TARGET_DIR='/Volumes/Hard Disk/bk-cross-arb/target' \
  cargo test -p blitzkrieg-core --test cross_arb_reverse_check          # 默认面（绿）
CARGO_TARGET_DIR='/Volumes/Hard Disk/bk-cross-arb/target' \
  cargo test -p blitzkrieg-core --test cross_arb_reverse_check -- --ignored   # 两颗牙（当前红）
```

收口动作清单（在途产物落地后）：摘除 R2/R3 的 `#[ignore]` → 全绿 → 本表状态列更新 → 验收章随 PR 正文提交。
