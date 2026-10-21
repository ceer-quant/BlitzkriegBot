# arb 架构（Issue #540）验收矩阵 —— 监控与升格观测面

对应 Issue #540（P1-arch）验收标准 3/4 的文档落点：⑥ 的两条 P0 升格触发、②③ 的 20% 侵蚀线、
方案 1→2a / 2a→3 的升级触发，全部写成**可观测核对行**。风格对齐 `docs/cross-arb-acceptance.md`
（正向/反向两节，每条给判据 + 核对位置 + 状态列）。

- 本文是**测试设计文档**，不改任何代码。所有锚点已于 2026-10-20 在本 worktree
  （feat/arb-batch-precheck）用 grep/sed 逐一验证存在；行号以本 worktree 为准，
  与 Issue #540 正文不符处见文末「锚点勘误」。
- 路径缩写：`service.rs` = `core/blitzkrieg_core/src/service.rs`；`host.rs` =
  `core/blitzkrieg_core/src/market/host.rs`；`engine.rs` = `core/blitzkrieg_core/src/engine.rs`；
  `ledger.rs` = `core/market_api/src/unified/ledger.rs`；`live.rs` = `extensions/polymarket/src/live.rs`；
  `executor.rs` = `extensions/kalshi/src/executor.rs`；订单日志默认 `data/orders/orders.jsonl`
  （service.rs:202/:883），成交 `data/trades/trades.jsonl`（service.rs:882），持仓
  `data/positions/positions.jsonl`（service.rs:884），账本流水 `data/ledger/<account>.journal.jsonl`
  （service.rs:302），对账审计 `reconcile.jsonl`（service.rs:286，audit_interval_sec 默认 30s）。
- 订单日志行是 `TrackedOrder` 的 camelCase JSON（model.rs:204-205）：关键字段
  `internalKey` / `submittedAtMs` / `avgFillPrice` / `filledSize` / `status` / `accountId` / `venue` / `mode`。

## 一、正向：监控基建应当可读（观测面的存在性）

| # | 判据 | 核对位置 / 验证命令 | 状态 |
|---|------|--------------------|------|
| F1 | 订单日志按 `internalKey` 可归因：每笔 taker 成交行带成交均价与成交量 | `jq -r '.internalKey' data/orders/orders.jsonl \| sort -u` 应含下列前缀/后缀；字段形状由 `TrackedOrder`（model.rs:204-207 起，`filled_size`/`avg_fill_price` :222/:224）保证 | 绿（静态核对 2026-10-20） |
| F2 | **flatten 归因键**：裸腿平仓走 manual exit 路径（`flatten`，service.rs:12564；SELL Taker :12603；`note_exit_reason(ExitReason::Manual)` :12610），订单键 `flatten:{position_id}`（:12618）；orphan sweep 的平仓入口就是它（`self.flatten(Some(&id), now_ms)` :10016） | `grep -n 'format!("flatten:{id}")' service.rs` → :12618；`grep -n 'self.flatten(Some(&id)' service.rs` → :10016 | 绿 |
| F3 | **chase 归因键**：orphan sweep chase 臂的补腿单 `internal_key = pair-chase:{naked_token}:{missing_token}:{slot}`（service.rs:10097，fn `pair_chase_missing_leg` :10079），dry-FOK Taker、经 `place_escalated` 下发（:10145）；guard 拒绝日志 :10141「pair chase refused by the guard」，成功日志 :10156「pair chase placed」 | `grep -n 'pair-chase:' service.rs` → :10097 | 绿。**注意：chase 键不是 `:escalated` 后缀**（见勘误 E5） |
| F4 | escalation taker 腿命名：maker→taker 升级（`run_escalation_sweep` service.rs:13757）与 reject 后重发（`reject_live_result` 段）两条路径的 taker 腿键均为 `{id}:escalated`（:13822 / :7936），守卫侧按 `ends_with(":escalated")` 过滤（:16208/:16258/:17095/:27173） | `grep -n '":escalated"' service.rs` | 绿 |
| F5 | **venue 余额覆盖现场可见**：`venue_free_balance` 无挂单时直接改写 active 簿并打 stderr `core: venue cash reconcile: ledger {balance} -> venue free {free}`（host.rs:480），有挂单只报 `core: venue cash drift …(resting orders; not auto-corrected)`（host.rs:487-489）；内核侧每次改写记 `audit.last_realign`（service.rs:1229/:2784）并随审计落 `ledgerRealigned`（:8267），另有 tracing::info「ledger cash set from the venue (no resting commitment)」（:2787-2792） | `grep -n 'venue cash reconcile\|venue cash drift' host.rs` → :480/:489 | 绿 |
| F6 | place-timeout 退休可见：`resolve_place_timeouts`（service.rs:9730，由任一 executor 的 `host.on_reconcile` host.rs:415 驱动、处理器内 :9463 调用）对 30s（`PLACE_TIMEOUT_VERDICT_MS` :14586）无 venue 回执的 Pending 腿打 warn「place timed out and the venue never acknowledged it — order retired, reservation released」并释放预占（:9744-9748） | `grep -n 'PLACE_TIMEOUT_VERDICT_MS\|place timed out' service.rs` | 绿 |
| F7 | 孤腿事件形态：orphan sweep 命中时先 `RiskAlert`（message 含 "pair leg orphaned: strategy=… account=… held=… partner_missing=…"，service.rs:9998-10010）再 flatten | `grep -n 'pair leg orphaned' service.rs` → :9999/:10004 | 绿（事件消费测试见 `core/blitzkrieg_core/tests/cross_arb_reverse_check.rs:387` `orphan_sweep_flattens_a_naked_leg_after_the_grace_window`） |
| F8 | 凭证就绪只读面：`market.credentials.status`（IPC 常量 ipc/schema.rs:161；处理器 ipc/server.rs:1436 → `market::credentials_status` market/mod.rs:555）返回每插件 required/resolved/ready，值不出进程 | `grep -n 'credentials_status' core/blitzkrieg_core/src/market/mod.rs` → :555 | 绿 |
| F9 | 已实现收益可读：closed trades 追加进 `data/trades/trades.jsonl`（service.rs:199-200/:882），退出原因随平仓记录（:12610）；同期套利已实现收益 = 同窗口内 `cross_venue_arb` 已平仓对 realized PnL（含 $1 结算收敛） | `jq -r 'select(.strategy=="cross_venue_arb")' data/trades/trades.jsonl`（字段名以实际 trade 行为准，见 F9 核对步骤） | 绿（机制）；口径公式见反向 R-SLIP |

## 二、反向：升格触发（任一命中即执行升格动作）

### R-⑥ P0 升格两条（Issue #540「⑥ 升格 P0 的可观测触发条件」）

**机制事实链（全部已验锚点）**：两 venue bridge 各自把真实余额 seed/realign 进**同一本 active 账簿**
（polymarket live.rs:91/:440、kalshi executor.rs:265-270/:479-484 → `host.seed_balance`/`venue_free_balance`
host.rs:461-494 → `set_balance` service.rs:2774-2796，:2795-2796 只写 `active_ledger_mut()`，无 venue 维度）；
`venue_free` 是单格（service.rs:1548）；风控裁决基准 = 提交时 active 簿余额
（`check_with_equity`：place_gated 内 service.rs:11724-11725、place_pending 内 :7767-7768）。
⇒ 两笔真实余额 ~5s last-writer-wins 交替覆盖，`check_with_equity` 读数在两者间震荡。

| # | 升格判据 | 可执行核对方法 | 状态 |
|---|---------|---------------|------|
| R-⑥a | 出现第一笔「风控读到错误 venue 余额而放行的超额订单」（对账/工单证据） | ① 从工单/告警拿订单号 O，取提交时刻与名义价值：`jq 'select(.orderId=="O") \| {internalKey,accountId,submittedAtMs,price,size,status,avgFillPrice}' data/orders/orders.jsonl`；② 找 O 提交前 ~10s 的余额覆盖现场：stderr `grep 'core: venue cash reconcile'`（host.rs:480）与 `grep 'ledger cash set from the venue'`（service.rs:2787-2792）；③ 取审计面同窗记录：`jq 'select(.ledgerRealigned != null)' <audit 文件>`（reconcile.jsonl，service.rs:8267，30s 间隔）；④ 用 `data/ledger/<account>.journal.jsonl` 复原 O 所落 venue 的真实余额 | 核对方法就绪；触发与否待生产数据（有据即升格 P0） |
| R-⑥b | `ledger.rs:181-185` 的 asset 冲突静默放行**被修复后**，仍出现映射错误导致的结算不收敛工单 | 当前静默分支已核：`asset_conflict = match (&ids[i].asset, &ids[j].asset) { (Some(x), Some(y)) => !x.eq_ignore_ascii_case(y), _ => false }`（ledger.rs:181-185，任一侧 None ⇒ false 放行）；工单侧判据 = 同一 unified 映射两腿结算合计 ≠ $1。修复 PR 合入时间戳之后的第一笔 ⇒ 命中 | 静态核对绿；触发待修复后生产数据 |

**R-⑥a 判定规则**：journal 复原的「O 所落 venue 真实余额」< O 名义价值（price×size），且 O 提交前
5s 内存在另一 venue 的 realign 证据（②③④）⇒ 判定成立。注意限定窗口：有挂单（reserved≠0）时
host.rs:487-489 只报 drift **不改簿**，那类区间不构成「读数被覆盖」证据。

### R-SLIP ②③ 滑点 20% 侵蚀线（Issue #540「②③ 滑点成本量化义务」）

分子（每笔 taker 滑点成本）：`|avgFillPrice − 参照价| × filledSize + taker 费`。
- flatten 单：`internalKey` 前缀 `flatten:`（F2）；参照价 = 该 position 入场成本均价
  （`data/positions/positions.jsonl` 的 costUsd/shares）。
- chase 单：`internalKey` 前缀 `pair-chase:`（F3）；参照价 = OME 历史死腿原价
  （`pair_chase_target` service.rs:10039 从 OME `all()` 取最近 BUY）。
- 佐证面（不计入分子，防重复计数）：`:escalated` 后缀单是两条 escalation 路径的 taker 腿
  （F4），属常态开仓成本，仅当其对应批发生 ②/③ 失败时才并入。

| # | 升格判据 | 可执行核对方法 | 状态 |
|---|---------|---------------|------|
| R-SLIP | 实测触发频率使套利预期收益被侵蚀 **>20%** ⇒ 升格 P0 | 触发频率：`jq -r 'select(.internalKey\|startswith("flatten:") or .internalKey\|startswith("pair-chase:")) \| [.internalKey,.submittedAtMs,.avgFillPrice,.filledSize] \| @tsv' data/orders/orders.jsonl`，行数/同期开仓对数；侵蚀率 = Σ(分子) ÷ 同期 `cross_venue_arb` 已实现收益（F9，trades.jsonl 同窗口 closed trades）；分母**明确为同期套利已实现收益**，不含未结算浮盈 | 公式与归因键就绪；数值待首轮统计窗口 |

### R-UPG 方案升级触发（Issue #540「升级触发条件」原文，逐条可观测化）

| # | 触发判据（Issue 原文） | 观测点 | 状态 |
|---|----------------------|--------|------|
| R-1→2a.a | 第一笔「面板已启用、后台每轮挂出→退休循环」操作员工单 | F6 的 retire warn + F8 `ready=false` 同现于同一策略、同 internalKey ≥2 轮循环；工单与日志时间对齐 | 核对方法就绪 |
| R-1→2a.b | 第一笔凭证缺失导致的裸腿 flatten 滑点损失（对账证据） | `flatten:` 成交行的时间戳 ∩ 该 venue 插件 `credentials.status ready=false` 时间窗；滑点额按 R-SLIP 分子公式 | 核对方法就绪 |
| R-1→2a.c | 新增第二个跨所策略需重打面板补丁 | `user_layer/strategies_lua/` 新增含 `atomic_batch` 的包（当前唯一：cross_venue_arb/manifest.json:15）+ P1-a PR 出现第二次面板 diff | 核对方法就绪 |
| R-2a→3.a | 2a 上线后仍出现「提交时校验通过、成交后一侧失败、orphan sweep 无法覆盖」事故 | F7 RiskAlert 且 chase 被拒（:10141）且 flatten 落空（:10016 返回 Err，持仓仍在簿，下 tick 重复判）的事故工单 | 核对方法就绪（依赖 2a 合入） |
| R-2a→3.b | E-VA「only at SUBMISSION」边界被证明无法靠提交前校验收窄 | service.rs:9787-9789 doc-comment 原文（"but only at SUBMISSION"）；论证记录进 RFC 决议 | 待 RFC |
| R-2a→3.c | 新 venue 接入要求 pair 生命周期级原子性 | venue 接入评审记录（流程面，无日志锚点） | 待事件 |

## 三、失败模式 ①-⑤：观测点 → 判据 → 动作

| 场景 | 观测点（数据源 + 锚点） | 判据 | 动作 |
|------|------------------------|------|------|
| ① 一侧凭证缺失（executor inert，无 egress） | stderr「polymarket-extension: live mode but POLYMARKET_* credentials absent — venue bridge disabled」（live.rs:51-57，`Ok(None)`）；kalshi `credentials_from_env()` None → `Ok(None)`（executor.rs:176-179）；F6 retire 循环；F8 `ready=false` | 同策略同 internalKey 的「挂出→30s 退休→重试」循环 ≥2 轮且对应插件 ready=false | 即 R-1→2a.a 证据；先合 P1-a 面板缓解（标注「缓解，非根因」） |
| ② scope 拒腿（引擎静默丢第二腿） | 丢腿点无日志（`engine.rs:1313` `if !self.scope_admits_token(...) { continue; }` 静默）——观测点在下游：CreateArbDialog.vue:616 `venueGuardWarning`（绑定账户警告，事后警告）+ 订单日志 atomic 批仅 1 腿 + 随后 `flatten:` 行 | 绑定 venue 非空的账户发出 1 腿批，另一腿成交后走 `flatten:` 平仓（Taker） | 2a 的引擎 scope 过滤器 pair 化（任一腿拒 ⇒ 整批拒）；本行同时是 R-SLIP 分子来源 |
| ③ 一腿成交一腿超时退休 | F6 retire warn（单腿）→ F7 RiskAlert → `pair-chase:` 成交（可行臂）或 `flatten:` 成交（不可行臂，:10016） | 孤腿被 chase 或 alert+flatten；chase 窗口 2s（PAIR_CHASE_WINDOW_MS，service.rs:9857）后仍孤 ⇒ 重复 | 计入 R-SLIP；频率突增即 ②③ 义务触发 |
| ④ venue 侧强平（内核视野外） | reconcile 缺口成交（service.rs:9446-9451 `FilledGap` → `apply_delta_effects`）与外部成交（:9460 `apply_external_fills`）；reconcile.jsonl | 内核无风控事件形态地吸收一笔非自己发的成交 | 对账工单留痕（风控迟到成本定性）；不直接升格 |
| ⑤ 映射配置错误 | 同所对被拒（ledger.rs:170 `DuplicateVenue`，错误枚举 :309）；asset 冲突静默放行（ledger.rs:181-185 `_ => false`）；结算不收敛工单 | 两腿结算合计 ≠ $1 收敛，且该映射经 unified.confirm 建立 | 修复 asset 静默分支；修复合入后仍现工单 ⇒ R-⑥b 同款升格判定 |

## 四、锚点勘误（与 Issue #540 正文/任务点名不符的代码事实）

| # | Issue 正文锚点 | 实际（本 worktree 验证） |
|---|---------------|--------------------------|
| E1 | `check_with_equity`（:11680）、`permits_order`（:11655） | `place_gated` 内两调用实际在 service.rs:11724-11725 / :11701（另一处同链在 `place_pending` :7767-7768）；:11680 落在 `refuse_cross_venue_order`（:11646-11668，该锚点本身精确）之后。机制不变，行号漂移 ~45 行 |
| E2 | `engine.rs:1310` 丢腿 | 实际 `service.rs` 无关——engine.rs:1313 才是 `scope_admits_token` 静默 continue 行（fn 定义 :661-675，Issue 的 :656-663 相应漂移） |
| E3 | `resolve_place_timeouts`（:9691）、30s 常量（:14532） | fn 实际 :9730-9753（处理器调用点 :9463）；常量 `PLACE_TIMEOUT_VERDICT_MS = 30_000` 实际 :14586。另注意 :9691 附近的 `note_place_timeout`（:7842）语义相反——venue POST 超时时**保持 Pending + 预占**，不退休 |
| E4 | `registry` 在 service.rs:1396 | :1396 是 `extensions: ExtensionRegistry`（插件生命周期注册表）；market 插件 `MarketPluginRegistry` 构造在 ipc/server.rs:402-403 与 main.rs:2565-2566，`credentials_status` 挂接 ipc/server.rs:1436 |
| E5 | 「place_escalated 的 {id}:escalated 命名」用于 chase 归因 | `{id}:escalated` 确属 `place_escalated` 路径（:7936/:13822），但 orphan sweep 的 chase 腿虽也经 `place_escalated`（:10145）下发，键是独立前缀 `pair-chase:…`（:10097）。归因脚本必须分别匹配 `flatten:` / `pair-chase:` / `:escalated` 三种键 |
| E6 | host.rs:107-111 live 成交默认落 default 簿 | 精确为 :106-112（`account_id: m::default_account_id()` + E28 ABI 缺口注释），锚点成立 |

## 五、凭证轮换运维语义（RFC issue 541 裁决「追加边界二」的文档落点）

> 裁决原句（评论 6285852873）：「运维文档须写明：**凭证轮换后，env 改动即时生效于批校验，
> 持久格改动生效于下一会话；两通道都动 + 重启 = 确定性就绪。**」本节把这三句话钉成
> 可执行核对行。锚点均在生产树（main tip）验证；行号漂移按第四节惯例披露。

### 5.1 两通道语义表

| 通道 | 落点 | executor 读到的时点 | 批校验（2a-i 挂钩 B）读到的时点 |
|------|------|--------------------|--------------------------------|
| **真实 env**（`.env` 或导出） | 进程环境变量：launcher 每次启动自加载 `<cwd>/.env`（`ui/ui_kit_panel/src/bin/blitzkrieg.rs:318-338`，已导出的 env 永远赢文件） | **boot 一次**：`spawn_if_configured` 读入并固化 client（polymarket `live.rs:46-57`；kalshi `executor.rs:53-61/:173-180`，`Credentials` 被 `Arc<KalshiRest>` 固化到 executor 生命周期结束） | **每批现查**：`virtual_batch_fits` 的 `std::env::var_os`（`market/mod.rs:624`，service.rs:6993-7011）——env 改动对新批**即时生效**（但已固化的 executor client 不受影响） |
| **持久格**（`market.credentials.set`，面板「添加账户」凭证卡） | `data/runtime/config.json` 顶层 `credentials` 键（`ipc/runtime_config.rs:52-60/:257-296`，tmp 先 `chmod 600` 再 rename；值只下行、从不回显） | **下一会话**：boot 把格注入进程 env——只补 env 里没有的名字（`main.rs:3454-3485`，"next-session by construction"） | **每批重读格**（service.rs:6999 "loaded once per batch here"）——格改动对新批即时可见，但真实 env 仍然 outrank 格（"the cell loses to the environment"，`schema.rs:1335-1338`） |

**窗口错位（裁决指认的边界，2a-i 不能消除）**：批校验看到持久格已就绪 ⇒ 放行；
但本会话 executor 的 env 仍旧（boot 固化）⇒ place 仍失败。这不是缺陷，是
next_session 既有语义的边界。

### 5.2 运维核对行

| # | 运维判据 | 核对位置 / 验证命令 | 状态 |
|---|---------|--------------------|------|
| R-OPS① | **只动 env 轮换**（改 `.env` 后重启）：重启即确定性就绪——executor 新 client、批校验现查同源 | 重启后 `market.credentials.status` 的 `ready=true` + 首批 `credentials.notReady` 消失 | 绿（机制核对 2026-10-21） |
| R-OPS② | **只动持久格轮换**（面板凭证卡写入）：`status.ready` 即时翻转（status 每次现读 env∪格），但**本会话 executor 仍用旧凭证**——place 失败属预期，重启后注入生效 | 写入回执「重启后注入生效」（`AddAccountDialog.vue` 成功文案）；重启日志 stderr 汇报注入的变量名（`main.rs:3479-3484`，值不进日志） | 绿（机制核对 2026-10-21） |
| R-OPS③ | **轮换完整配方**：两通道都动 + 重启 = 确定性就绪。env 提供的名字格不覆盖（"only a name the process does not already carry is injected"，`main.rs:3470-3476`）——格补 env 缺口，二者拼出全量 `required_vars` | 重启后 `market.credentials.status`：每插件 `resolvedVars` == `requiredVars` 且 `ready=true`；需求表单一事实源 `plugin_credential_vars`（`market/mod.rs:541-548`） | 绿（机制核对 2026-10-21） |
| R-OPS④ | **批校验拒绝可见性**（边界三衔接）：整批拒绝行点名腿/token/venue/缺失变量名，不停留笼统失败 | `credentials.notReady: leg {key} (token {token}) venue {venue} … missing [{VARS}]`（service.rs:7013-7017）；缺失名单 `venue_credentials_missing` 按声明序（`market/mod.rs:616-630`） | 绿（2a-i 挂钩 B 已随 PR issue 547 + issue 549 落地） |

### 5.3 操作员速查

- **面板改凭证** → 提示「重启后注入生效」→ **重启内核**后新凭证才真正进 executor。
- **改 `.env`** → 重启（launcher 自动加载，无需 source）。
- **两处都改** → 重启一次即可；env 优先，格补缺。
- 判断当前会话凭证是否就绪：`market.credentials.status`（面板套利弹窗的 per-leg
  「凭证就绪/未就绪」徽标与缺变量名单是同一读数）。

## 复现命令（锚点复核）

```sh
cd '/Volumes/Hard Disk/bk-arb-batch/core/blitzkrieg_core/src'
grep -n 'format!("flatten:{id}")' service.rs          # F2  :12618
grep -n 'pair-chase:' service.rs                      # F3  :10097
grep -n '":escalated"' service.rs                     # F4  :7936/:13822
grep -n 'venue cash reconcile' ../market/host.rs      # F5  :480
grep -n 'PLACE_TIMEOUT_VERDICT_MS' service.rs         # F6  :14586
grep -n 'pair leg orphaned' service.rs                # F7  :9999/:10004
grep -n 'fn credentials_status' ../market/mod.rs      # F8  :555
grep -n 'venue_free: Option' service.rs               # ⑥  :1548
grep -n 'pub fn set_balance' service.rs               # ⑥  :2774
grep -n 'check_with_equity' service.rs                # ⑥  :7768/:11725
grep -n 'scope_admits_token' ../../blitzkrieg_core/src/engine.rs   # ②  :661/:1313
grep -n '_ => false' ../../market_api/src/unified/ledger.rs        # ⑤  :184
grep -n 'spawn_if_configured' ../../extensions/polymarket/src/live.rs   # R-OPS :46
grep -n 'credentials_from_env' ../../extensions/kalshi/src/executor.rs  # R-OPS :53/:177
grep -n 'fn venue_credentials_missing' ../market/mod.rs   # R-OPS :616
grep -n 'loaded once per batch here' service.rs        # R-OPS :6999
grep -n 'CREDENTIALS_SET_NOTE' ../ipc/schema.rs         # R-OPS :1383
grep -n 'fn persist_credentials_to' ../ipc/runtime_config.rs  # R-OPS :257
```
