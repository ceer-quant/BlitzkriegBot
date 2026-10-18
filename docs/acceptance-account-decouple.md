# 多账户解耦 — 验收矩阵（acceptance-account-decouple）

分支 `feat/account-venue-decouple`（基线 `029028f`）· 验收 officer：Agent-验收
· 牙齿文件：`core/blitzkrieg_core/tests/account_decouple_check.rs`（7 个 #[test]，
覆盖任务书的 6 颗牙）· 收口前牙齿独立编译、独立跑（`cargo test -p
blitzkrieg-core --test account_decouple_check`，从 worktree 根执行）。

## 红基线快照（唯一一次终裁前重跑在收口时做）

时间点 **2026-10-19T01:33:24+0800**，worktree `/Volumes/Hard Disk/bk-decouple`，
命令 `cargo test -p blitzkrieg-core --test account_decouple_check`：

```
test dual_venue_data_plane_keeps_both_venues_and_rolls_per_venue ... ok
test an_infeasible_chase_alerts_and_flattens_the_naked_leg ... ok
test assignment_rebuilds_at_boot_from_the_file ... ok
test removing_a_held_scope_demotes_it_to_exits_only ... ok
test switching_the_active_account_leaves_the_other_scope_trading ... FAILED
test assignments_are_independent_and_coexist ... FAILED
test a_feasible_chase_reoffers_the_missing_leg_and_keeps_the_pair_whole ... FAILED
test result: FAILED. 4 passed; 3 failed; 0 ignored; 0 measured
```

`cargo fmt --all -- --check`：本验收新增文件零 diff（worktree 存量
`src/service.rs` 有 3 处他人 fmt diff，不属于本验收域，未触碰）。

## 牙齿清单（测试名 → 判据 → 红基线终态）

| # | 测试名 | 钉的验收项 | 判据 | 红基线 |
|---|--------|-----------|------|--------|
| 1 | `switching_the_active_account_leaves_the_other_scope_trading` | 反向 ① | paper 指派跑腿后 `accounts_mut().switch(&default)`，仍 placed=4、paper 2 持仓（cost 516）、default 2 持仓 | **红**：`count_positions("paper") 0 ≠ 2`（:374）|
| 2 | `assignments_are_independent_and_coexist` | 正向 ② + 反向 ③ | 未指派 paper 零单；指派后 placed2=2、paper 2 持仓、default 仍 2 持仓 | **红**：`count_positions("paper") 0 ≠ 2`（:434）|
| 3 | `assignment_rebuilds_at_boot_from_the_file` | 正向 ②（重启半） | accounts.toml `strategies = ["cross_venue_arb"]` ⇒ 两次全新 boot 后 scope/行为一致：paper 交易、default 永不动 | **绿** |
| 4 | `removing_a_held_scope_demotes_it_to_exits_only` | 反向 ①（销毁半）+ exits-only 合同 | 移除 held scope ⇒ 返回 kept==["cross_venue_arb"]；重报价到回落阈值 ⇒ release 出场流继续、账户归零；再移除 ⇒ DROP；白送组合再现 ⇒ 0 entry | **绿**（fixture 修正后）|
| 5 | `dual_venue_data_plane_keeps_both_venues_and_rolls_per_venue` | 反向 ②（内核数据面半） | 同 slot 双所 RoundMarkets 合流、`venue_of_token` 各归其主、单所翻轮只换自己 venue 的行 | **绿** |
| 6 | `a_feasible_chase_reoffers_the_missing_leg_and_keeps_the_pair_whole` | 补课：Chase 追腿可行臂 | 裸腿 + 对手腿 0.42 可行 ⇒ tick(+12s) 后 UP 腿活着、KX_DOWN 有追回持仓 | **EXPECTED-RED**：sweep 现状 alert+flatten、永不追 |
| 7 | `an_infeasible_chase_alerts_and_flattens_the_naked_leg` | 补课：Chase 追腿不可行臂 | 对手腿 0.60 不可行 ⇒ RiskAlert 计数上升且点名 default、裸腿被 flatten | **绿** |

### 关键红基线输出尾（逐字）

- 牙 1：`assertion left == right failed: A(paper)'s legs filled in the background
  after switching to B — left: 0, right: 2`（account_decouple_check.rs:374）
- 牙 2：`assertion left == right failed: same tokens, paper's own fills under
  paper's account — left: 0, right: 2`（account_decouple_check.rs:434）
- 牙 6：`CONTRACT: a feasible chase must NOT flatten the naked leg
  (EXPECTED-RED-WAITING: today the sweep alerts + flattens and never chases)`
  （account_decouple_check.rs:717）

## 根因锚点（给收口 agent 的落点，红→绿的判定面）

1. **牙 1/2 的红是真实的解耦缺口，不是夹具问题。** `Core::project_fill_delta`
   （`src/service.rs` ~8672）把持仓账本按 **token_id 单键** 去重（注释原话
   "One position per token, so token_id identifies it"），没有 account 维度。
   探针证据（已删除的临时探针，结论录在此处）：paper 的两条腿各自以
   `acct=paper` 下单并以 paper 自己的账本结算成交（`dry_5:escalated acct=paper
   Filled 600`），但成交投影落进了 **default 的** token 键持仓
   （pm-up 1200 股 cost 528 = 两账户各 600 股合仓；kx-down 504 同理），
   于是 `count_positions("paper") == 0`。收口判据：持仓账本带 account 维度
   （或同 token 多账户各记一份），牙 1/2 转绿且牙 4 的机制不受影响。
2. **牙 6 是合同缺口（任务书明示 EXPECTED-RED-WAITING）。**
   `run_pair_orphan_sweep`（`src/service.rs` ~9535）对 governed scope 的裸腿
   只做 `Event::RiskAlert` + `flatten`；Chase 追腿（可行→追单过对手价、双腿
   合拢；不可行→alert+flatten）未接线。牙 7 证明不可行臂的语义已在；牙 6 等
   可行臂落地后转绿。
3. 夹具口径（复现与判读时对齐）：`execution_policy_path: None`（默认路径是
   cwd 相对拼写，worktree 根下会加载 `max_positions_per_asset = 1`，把双腿
   第二条拒掉）；`exit_grace_sec: 0`（3 秒默认宽限会 hold 掉紧贴建仓的出场
   tick）；出场走**策略 release 阀**而非政策阶梯——cross_venue_arb 声明
   hold-to-settlement，Dry 下 `run_exit_checks` 的 settlement_holders 会整体
   跳过其持仓的政策阶梯（含止损），所以牙 4 的出场触发是重报价到
   `spread_close_pct` 回落阈值（pair cost ≥ 0.995）→ lua release intent →
   `run_exit_checks` 策略信号路径 → FOK 吃自家书。
4. 报价档口径：`leg_book` 挂 bid=ask-0.01 / ask，600 深度；身份成交模型
   （零延迟、100% 概率）下 FOK 全吃。MakerThenTaker 入场腿在提交瞬间即
   crossing → WouldCross 拒 → escalation 立刻以对手价 taker 全量追成
   （`place_escalated`，internal_key `{id}:escalated`）——牙 1/2 的 "placed=4
   → 4 次成交" 都走这条路径，这是既有内核语义，非夹具偏置。

## 十项验收矩阵（正向 3 + 反向 3 + 补课 4）

每项：验收操作 → 门禁落点 → 红基线证据 → 终裁状态（收口时统一改判）。

### 正向 ①：策略列表全量显示（UI 侧）

- 验收操作：切账户 A→B，策略列表不因 B 未启用某策略而少行（全量 = 注册面，
  不是 active 账户的启用面）。
- 门禁落点：UI agent 的 `npm run check:strategy-visibility`（webui
  package.json 既有脚本）+ `npm run check`（vue-tsc）。
- 红基线证据：UI agent 域，本文件不采证。
- 终裁状态：**待收口重跑**（UI 门禁）。

### 正向 ②：账户 A 的成交在切到 B 后继续后台更新（内核半 + 重启半 + UI 半）

- 验收操作（内核）：牙 1（切换后台成交）+ 牙 2（共存）+ 牙 3（boot 恢复）。
- 验收操作（UI）：切到 B 后 A 的持仓/成交面板仍推进（feed 活性）。
- 门禁落点：`cargo test -p blitzkrieg-core --test account_decouple_check`（牙
  1/2/3）+ UI `npm run check:feed`（feed-liveness.check.mjs）+ `npm run
  check:account`（account-filter.check.mjs）。
- 红基线证据：牙 1/2 **红**（见上，根因锚点 1：持仓账本无 account 维度）；
  牙 3 绿。
- 终裁状态：**待收口重跑**。

### 正向 ③：切到绑 Kalshi 的账户 B 立即显示 Kalshi 盘口

- 验收操作（内核数据面）：牙 5（双所数据面合流 + 每所独立翻轮 +
  `venue_of_token` 归属）。
- 验收操作（UI/IPC 线）：B 绑 kalshi ⇒ 选中后盘口行即时呈现 kalshi venue 的
  报价（wire 的 per-row venue 字段由 ENGINE_ROUND / ENGINE_BOOKS 组装层从
  `venue_of` 富化——内核牙 5 钉数据源本身，wire 富化与 UI 呈现归 ② 号 agent
  与 UI agent）。
- 门禁落点：牙 5 + UI `npm run check:venues`（venue-presentation.check.mjs）
  + `npm run check:pairing`（account-venue.check.mjs）。
- 红基线证据：牙 5 **绿**（029028f 内核数据面已具备 #498 跨所合流与按所翻
  轮）；UI 侧不采证。
- 终裁状态：**待收口重跑**。

### 反向 ①：切换账户不得暂停/销毁其他账户的策略

- 验收操作：牙 1（切换只挪 active 指针，双方 scope 照常下单成交）+ 牙 4
  （移除 held scope 走 DEMOTE：exit 流不断、绝不带仓硬拆）。
- 门禁落点：牙 1 + 牙 4。
- 红基线证据：牙 4 绿（exits-only→DROP→零残留链条完整）；牙 1 红于持仓投影
  合仓（根因锚点 1），属「A 的成交不可见」，与「被暂停/销毁」是同一缺口的
  两个侧面——切换本身确实没动 scope（placed=4 证明），但 A 的仓位事实丢了。
- 终裁状态：**待收口重跑**。

### 反向 ②：行情页永远只显示一个交易所

- 验收操作（内核数据面半）：牙 5——同一 slot 双所行情必须同时活在盘口视图，
  单所翻轮不得波及对手所。
- 验收操作（UI 呈现半）：行情页同时渲染两个 venue 的行（含 per-row venue 标
  识）。
- 门禁落点：牙 5 + UI `npm run check:venues`。
- 红基线证据：牙 5 **绿**；UI 呈现半不采证。
- 终裁状态：**待收口重跑**。

### 反向 ③：不因选中 B 就看不到未在 B 启用的策略

- 验收操作（内核半）：牙 2——paper 未指派时零单但 scope 面照常注册；指派与
  default 共存互不压制（同 token 双账户各自成交，候选互不吞）。
- 验收操作（UI 半）：正向 ① 的同一 UI 门禁（可见性按注册面，不按 active 账
  户启用面过滤）。
- 门禁落点：牙 2 + UI `npm run check:strategy-visibility`。
- 红基线证据：牙 2 **红**（根因锚点 1）；UI 半不采证。
- 终裁状态：**待收口重跑**。

### 补课 A：Chase 追腿（可行臂 / 不可行臂）

- 验收操作：牙 6（可行：追回过对手价、双腿合拢、裸腿不拆）+ 牙 7（不可行：
  alert 点名账户 + flatten 裸腿）。
- 门禁落点：牙 6 + 牙 7（同一文件相邻用例，同一 naked_pair_fixture）。
- 红基线证据：牙 7 绿（不可行臂语义已在 `run_pair_orphan_sweep`）；牙 6
  **EXPECTED-RED**（根因锚点 2）。
- 终裁状态：**待收口重跑**（牙 6 允许在 Chase 落地后转绿；牙 7 保持绿）。

### 补课 B：跨账户双腿设计文档指针

- 验收操作：核对本文件「根因锚点」1（持仓账本 account 维度缺口）+ 引擎侧
  per-scope 持仓切片语义（`HeldPosition` 的 strategy+account 双键切片，
  `src/strategies/mod.rs` ~168-191；`Engine::evaluate` 的 held_view 按
  scope 切片）。
- 门禁落点：本文件（docs/acceptance-account-decouple.md 的根因锚点节）+ 牙
  1/2 的失败消息文本本身（把根因写进了 assert 消息）。
- 红基线证据：见根因锚点 1 的探针证据链。
- 终裁状态：**已交付**（设计事实录毕；收口后如实现选型变化需回写此节）。

### 补课 C：TUI 'A'（账户切换热键）

- 验收操作：TUI 侧切账户后策略列表/盘口跟随（会话级 switch 不动内核 scope）。
- 门禁落点：UI agent 域的 account 相关门禁（`npm run check:account` +
  `npm run check:switch-purity`，后者在 package.json check:all 链中）。
- 红基线证据：UI agent 域，本文件不采证。
- 终裁状态：**待收口重跑**（UI 门禁）。

### 补课 D：实时预估利润

- 验收操作：多账户下各账户实时预估利润不串账（估值按持仓自身账户的账本与
  持仓走；内核侧 `strategy_stats_for_account` 已按 (strategy, account) 分行）。
- 门禁落点：本验收以牙 1/2 的持仓归属为前置事实（持仓不串账是利润不串账的
  前提），利润呈现归 UI agent（`npm run check:balance`，balance-accounting
  .check.mjs）。
- 红基线证据：牙 1/2 红（持仓合仓即利润合仓的直接前因）；UI 侧不采证。
- 终裁状态：**待收口重跑**。

## 披露与取舍（验收 officer 如实披露）

1. `counterpart_pair_basis`（`src/service.rs` ~6703，pair-cost cap 的对手腿
   基准）按 condition+asset+对手方向过滤持仓，**不按 account 过滤**——
   跨账户污染：A 账户持仓会把 B 账户的加仓/追腿顶过 $1 上限。多账户解耦
   收口时应判：这是要修的缺口还是可接受的保守（本验收未钉牙，留给收口
   决策，因为修法影响 Chase 的 guard 路径）。
2. 持仓账本 token 单键（根因锚点 1）同时是本任务最大的红与最大的收口项；
   牙 4 的 exits-only 链条在其修复后必须保持绿（出场按持仓自身账户花销，
   E28 §9.2 已在 place_gated/ExitJob 落实）。
3. 牙 1/2 的 fill 路径全部经 escalation（提交瞬间 crossing → WouldCross →
   taker 追成），与 cross_arb_reverse_check 模板同口径；模板只测单账户，
   故「双 scope 同 token」的合仓行为在模板里不可见——本验收的红是双账户
   场景首次显式钉出该行为。
4. 牙 5 钉的是 blitzkrieg_core 侧数据源（scanner/round_state + books 视图 +
   `venue_of_token`）；IPC wire 的 per-row venue 富化与 UI 呈现不在本验收的
   文件域，由 ② 号 agent 与 UI agent 的门禁覆盖（见矩阵正向 ③ / 反向 ②）。
5. 本验收未跑回放类门禁（exit-economics 4 窗 / backtest）与全 workspace
   clippy，按任务书豁免；`cargo fmt` 的 service.rs 存量 diff 属他人域。

## 收口操作清单

1. 协调者收口时重跑：`cargo test -p blitzkrieg-core --test
   account_decouple_check`（从 worktree 根），对照本文件的红基线快照逐牙
   改判终裁状态。
2. 牙 1/2 转绿需持仓账本 account 维度落地；牙 6 转绿需 Chase 可行臂接线；
   牙 3/4/5/7 应保持绿——任何转红即为回归，按根因锚点定位。
3. UI 门禁（check / check:account / check:venues / check:feed / check:pairing
   / check:strategy-visibility / check:switch-purity / check:balance）由 UI
   agent 的验收文档采证，本矩阵只引用脚本名。
