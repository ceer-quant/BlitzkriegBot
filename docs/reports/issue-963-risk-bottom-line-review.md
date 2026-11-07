# issue #963 — 内核风控兜底审查（place_gated 门链 · 硬限制 · 出场优先级）

> 审查对象：`refs/remotes/ceer/main` @ `f3753e11`（#983 已合入，全部按 per-account 口径；文中不引用已删除的 `global.*` 路径）。
> 运营者哲学：即使入场信号完全是垃圾（随机乱发单），只要进内核，风控硬限制必须兜底；出场信号在 250ms 延迟下拥有最高优先级。
> 行号基于本 PR 的修复分支（`fix/issue-963-risk-bottom-line`），与 main 的差异见 §5 缺口表。

---

## 1. place_gated 门链清单（逐门：触发条件 / 拦截后行为 / 代码位置）

入口：`Core::place` / `place_outcome` / `place_escalated` 全部汇入 `place_inner` → `place_gated`
（`core/blitzkrieg_core/src/service/risk_admission/admission.rs:301`）。任何一门拒绝即短路返回
`Err`，`place_inner` 统一记入 `note_error`（admission.rs:59-62）——拒绝不会落在无痕迹路径。

| # | 门 | 触发条件 | 拦截后行为 | 代码位置 |
|---|-----|----------|------------|----------|
| 1 | 拒绝冷却 | 同 `internal_key` 上一次被场地拒绝、`now < next_ok_ms`（指数退避 ≤30s） | `RiskRejected`："intent … in rejection cooldown (…ms left)" | admission.rs:312-324 |
| 2 | 账户姿态 | `accounts.permits_order(account, is_close_intent)`：仅 `Active` 可开新仓；平仓除 `ReadOnly` 外全放行（冻结绝不困仓） | 账户姿态错误拒绝 | admission.rs:331-332（实现 `accounts/`） |
| 3 | 冻结门 | 账户在 `freeze_book` 冻结中 | 新单拒绝；平仓放行（#174 舱口，按账户） | admission.rs:337（`account_freeze_permits`，`risk_admission/freeze.rs`） |
| 4 | 跨账户平仓拒 | SELL+close-intent，但请求账户不持有该 token、而兄弟账户持有（会结进未付款的账本） | `InvalidParams`，点名两个账户；**不重路由** | admission.rs:83-111 / 调用点 340 |
| 5 | 跨 venue 绑定 | 账户绑定 venue V，token 属于 venue W 的 live 市场（未绑定账户、无标签 token 放行） | `InvalidParams`，点名两个 venue | admission.rs:124-146 / 调用点 350 |
| 6 | 时长族准入 | (venue, duration) 不在账户订阅集（空集=无声明=全放行）；**平仓豁免** | `AccountLimit` | admission.rs:226-260 / 调用点 357 |
| 7 | 风控基础门（equity） | `check_with_equity(req, 实时余额)`：kill switch（**平仓放行**，risk.rs:485-492）、价格带 (min,max]、size>0、BUY 名义帽 + #202 equity 相对帽（平仓豁免，risk.rs:533-549） | `RiskRejected` / `KillSwitchActive` | admission.rs:362-363 → risk.rs:483-551 |
| 8 | 系统性限额（仅 BUY） | `systemic_snapshot().any_armed()`（五项 per-account 界，#983）：`judge_entry` —— `max_position_size` 缩量、`max_single_loss_usd` 缩量/拒绝（§3） | `Reduce` 原地缩 `req.size`（INFO 日志）；`Reject` → `RiskRejected` | admission.rs:371-411 → risk/limits.rs:347-390 |
| 9 | 成对成本上限（仅 BUY） | 对手腿已持有：`pair_basis + price > $1.00`（一双redeem 恰 $1，超出=锁定亏损） | `RiskRejected` | admission.rs:417-440 |
| 10 | 审计门（entry_gates∧BUY） | 会计三方审计失败（#189；结构性失败或跨源连续 2 次，`audit_halt_entries`） | 新开仓拒绝直到审计恢复；**平仓不受困**（非 kill switch） | admission.rs:447-457 → service/accounts/audit.rs:166-285 |
| 11 | 连亏熔断 KI-10（entry_gates∧BUY） | 该策略连亏达阈（出厂 3 亏/300s，service/config.rs:946-947）；账户级 streak 走 `__account__:` 前缀键 | `RiskRejected`："breaker active until … for strategy …" | admission.rs:458-470 → risk.rs:229-345 |
| 12 | can_open（entry_gates∧BUY） | `max_positions` 容量 → **每日亏损熔断**（sticky，§4）→ 止损冷却 → 单资产数（"Already in"）→ 出场/亏损/资产冷却；pair-completion 豁免仅限 hold-to-settlement 的同条件对手腿/补腿 | `RiskRejected`，逐门原文 | admission.rs:471-513 → position/book.rs:343-432 |
| 13 | 去重/replace | 同 key 已有活单：裸腿 requote 先撤后下，字节级重复直接回原单；其余拒绝 | （非风控门，防重复敞口） | admission.rs:526-557 |

之后：BUY 预留名义（admission.rs:566-569，E28 请求自己的账本）→ OME submit（失败回滚预留，admission.rs:591-597）→ `place_after_submit`。

**策略信号的更早一道**：策略建议在仲裁管线（`arbitration/pipeline.rs`）先走同一套裁决
（RiskGate + 账户 breaker + 系统性限额 + 物理/组合层），此处是提交路径的最终执法点——两层同一词汇，无绕过。

---

## 2. 出场路径与 250ms 下的优先级

### 2.1 提交顺序（同 pass 内出场先于入场）

维护循环（`ipc/server/session.rs:604-623`）每个 beat：**先** `c.tick(now)`（其内部
`run_exit_checks`，`service/risk_admission/exits.rs:483`，提交出场 SELL；随后才是
`run_expiry_sweep`/`run_escalation_sweep`，exits.rs:513/529），**后** `c.engine_evaluate(now)`
（提交入场）。同一维护节拍内出场单先于入场单到达 OME。

### 2.2 策略平仓意图的同期提交（#963 修复 ②）

`engine_evaluate` 从策略收出 `StrategyExitIntent`（evaluate.rs:158-176 收取）。修复前：
intent 只 extend 进 `strategy_exits`，等**下一个**维护节拍的 `run_exit_checks` 才提交——同周期的
入场在本周期就已 place，出场信号永远输掉同期竞争（延迟一个维护间隔）。修复后（#963）：
live 内核（`!replay_sched()`）在引擎借用结束、入场循环之前调用
`submit_strategy_exits_now`（exits.rs:1249，evaluate.rs:196-203）——**同周期出场先提交、先成交**；
replay 保持原延迟路径（#390 调度形状输出锁定），merge 意图仍走 tick 路径（是结算工作，不是流动性竞争）。

### 2.3 taker 出场与 #351 250ms 延迟

止损/时间/强平/残量出场一律 `FillPolicy::Taker`（出场判定端 `use_maker=false`，
exit_policy.rs:1407/1419；`reason_is_profit_side` 只放 TP/trailing 进 maker 咨询，
exits.rs:49-51；#939 PnL 守卫再把水下单压回 taker）。taker 腿在 250ms 延迟下按
#351 顺序推进（`drain_inflight_takers`，admission.rs:984-1074）：**先 placement 的腿先 walk**——
出场单提交在先 ⇒ 匹配在先；匹配用的是 `execute_at_ms` 时刻的镜像盘（严格无未来数据）。

### 2.4 maker 出场的 deadline（#963 修复 ①，原缺口）

TP 类 maker-first 出场（`maker_first_exit_enabled` 出厂 true）修复前以纯 `FillPolicy::Maker`
提交（exits.rs `mode` 三元），而 `place_after_submit` 只为 `MakerThenTaker` 武装升级时钟
（admission.rs:870-878）——**出场 maker 永不升级**；入场却有 `entry_maker_timeout_ms=5000`
（service/config.rs:1023）。#935 的文档写着 "escalating on the OME's maker_timeout_ms clock,
like every maker"，代码从未兑现。修复：出场 maker 以 `MakerThenTaker` 提交、挂
`ExitConfig::exit_maker_timeout_ms`（出厂 **250ms**，0=沿用入场默认，负=纯 maker 不升级，
exit_policy.rs:170-178/317；提交点 exits.rs:1522-1536）。

### 2.5 入场抢占出场流动性的路径排查

- 同 token 顶补 BUY 被 can_open "Already in BTC" 拒（book.rs:399-402）；pair-completion 豁免只对
  **同条件另一 token**（UP/DOWN 是两本盘口），不存在同盘口入场挤占出场深度的树内路径。
- 现金维度：出场是 SELL，不预留现金；入场预留不挤占出场。
- 同周期竞争：出场先提交（§2.1/2.2）；in-flight 队列 FIFO（admission.rs:989-991 `std::mem::take`），
  出场腿先匹配。
- 残量场景：出场 SELL 未成交而入场腿成交扩大持仓 → E31-b 残量兜底补平（exits.rs:1290-1352）。

**结论**：修复后不存在树内的"入场抢占出场"路径；250ms 延迟下的出场优先级由
提交顺序（先出场）+ 专属升级时钟（250ms deadline）+ FIFO 匹配三重保证。

---

## 3. 硬限制一：单笔最大亏损（阈值来源 · 实际生效点）

| 项 | 值 |
|----|-----|
| 阈值来源 | env `BK_RISK_MAX_SINGLE_LOSS_USD`（唯一生效源，#967；0=关闭，risk/limits.rs:196） |
| 运行时读数 | `risk.limits` / `risk.setSystemic`（热改 live 档，trading_reset.rs:196-…） |
| 数学 | `loss_per_share(entry, bound_stop)`（risk/limits.rs:282）；`single_loss_verdict` 整股地板（limits.rs:291-310）：`Within` / `Reduce{approved}` / `Refuse` |
| 生效点 1 | 提交门 `place_gated` BUY 系统性块（admission.rs:371-411），`judge_entry` 缩量/拒绝 |
| 生效点 2 | 仲裁管线 Gate 2（arbitration/pipeline.rs:285-320）——策略意图的更早一道 |
| 生效点 3 | 升级守卫 `escalation_guard`（exits.rs:561-627）——追单（升级腿）同判；**#963：close-intent 腿豁免系统性块**（帽约束新敞口，永不困出场，#174 原则） |
| bound_stop | `effective_stop_pct`（exit_policy.rs:387-406）：动态紧缩坡道，300s 起、10% 地板。**#963：提交门 sizing 用坡道最宽端（base，出厂 12%）**——修复前传 `time_left=0` 取最紧地板 10%，而 fresh 仓的实际止损就是 12% 基宽，单笔帽被低估 1.2×（12/10） |
| 网格分辨率 | **#963：`loss_per_share` 按 venue 0.01 tick 取可成交最劣**（limits.rs:270-289）：止损以"挂盘最优 bid（≤止损位的 0.01 tick）"成交，止损位落在 tick 之间时每股多亏一 tick（0.30 仓、12% 止损位 0.264 实际成交 0.26 = 13.33%）。帽按 tick 精确最劣 sizing 后，不变式 `approved × loss_per_share ≤ cap` 在网格上成立 |

## 4. 硬限制二/三：时间加权紧缩 · 每日熔断

**时间加权紧缩**（阈值来源：`ExitConfig`，出厂 `stop_loss_pct=12%`、`dynamic_stop_enabled=true`、
`stop_tighten_start_sec=300`、`stop_min_pct=10%`，exit_policy.rs:271-279）：

- 实际生效点 A：出场梯判定 `decide_exit_verdict` 用 `effective_stop_pct(base, time_left)` 逐 tick 收紧
  （exit_policy.rs:1403、1424-1434）；梯层overlay同样读取（position/exits.rs:414）。
- 实际生效点 B：PHYSICS 投影（入场 sizing 的纪律约束）与 `holder_effective_stop_pct`
  （holder 视图平坦在 50% 容纳宽，exit_policy.rs:400-405）。
- 单笔帽按"坡道最宽端" sizing（§3），故坡道收紧只会更安全，帽与执行永不脱节。

**每日熔断**（阈值来源：`PositionConfig::max_daily_loss_usd`（0=关）与
`max_daily_loss_equity_pct`（出厂 20%）取更紧；基数=该账户当日开盘 equity，daily.rs:104-121）：

- 记账：平仓净额入**该账户**当日桶（position/book.rs:82-96，E28 per-account）；
  `daily_loss_marks_unrealized=true`（出厂）时未实现亏损并入判定输入（daily.rs:98-114，#464）。
- 触发：`daily_loss_tripped_for` sticky（当日不再解冻，daily.rs:127-139）；生效点 =
  `can_open` 的 "Daily loss limit for account …" 拒绝（book.rs:358-370）——**只冻新开仓，持仓照常出场**。
- 可观测：`roll_daily_budget` 每日 UTC 翻页广播（trading_reset.rs:1291-1345）；
  触发即发 `RiskAlert "DAILY LOSS LIMIT REACHED for account …"` 恰一次（trading_reset.rs:1369-1391）。
  同日 re-anchor 只紧不松（`reconcile_same_day_base`，daily.rs:248-…，P1 #235）。

---

## 5. 缺口表（审查结论 → 修复）

| # | 缺口（main @ f3753e11） | 证据 | 修复（本 PR） |
|---|--------------------------|------|----------------|
| ① | **出场无 maker→taker 升级超时**：maker-first TP 出场以纯 `FillPolicy::Maker` 提交且永不升级（入场 5000ms 有钟，出场没有） | admission.rs:870-878 只为 MakerThenTaker 武装；exits.rs 出场提交 `place(order, 0, …)`+mode=Maker | `ExitConfig::exit_maker_timeout_ms`（出厂 250ms，三值语义同 `escalation_delay_ms`）；出场 maker→`MakerThenTaker` 提交挂专属钟（exits.rs:1522-1536） |
| ② | **策略平仓意图输掉同期竞争**：intent 在 `engine_evaluate` 收取却等下一节拍提交，同周期入场先 place | evaluate.rs:158-176（收取）vs exits.rs:483（下一节拍消费） | live 内核同周期先提交出场再放入场（`submit_strategy_exits_now`，exits.rs:1249；evaluate.rs:196-203）；replay/merge 保持原路径 |
| ③ | **单笔帽按最紧地板 sizing**：提交门/升级守卫传 `time_left=0` → 10% 地板，而实际止损从 12% 基宽起跑，帽被低估 1.2× | admission.rs:384-392、exits.rs:586-597 传 0；坡道实现 exit_policy.rs:387-406 | sizing 改用坡道最宽端（`i64::MAX` → base），只收紧不放宽；管线（pipeline.rs:287 用当前 time_left，即当下最宽）本就正确，未动 |
| ④ | **帽的网格漏洞**：`loss_per_share` 连续值 sizing，止损位落 tick 之间时网格成交多亏一 tick（0.30 仓 12% 止损实亏 13.33%） | limits.rs:272-275 旧实现 | `loss_per_share` 按 0.01 tick 取可成交最劣（limits.rs:270-289），不变式在网格上成立；只收紧 |
| ⑤ | **升级守卫不认 close**：`escalation_guard` 对任何 side 跑 `judge_entry`，且升级腿 key `{id}:escalated` 不是 close-intent——armed 帽（`max_position_size<1`/单损 `Refuse`）或 kill switch 会困住升级中的出场 | exits.rs:575-603、admission.rs 升级 key | `escalation_guard` 系统性块 close-aware 豁免（exits.rs:575-583）；出场腿升级 key 改 `exit-escalated:{id}` 且 `is_close_intent` 识别（risk.rs:18-31；exits.rs:640-… / 736-…）——所有 #174 舱口（kill switch/族门/守卫豁免）随行 |

不放宽任何硬限制：③④⑤全部只收紧或只扩豁免面（豁免只对 close-intent）；①②给出场加 deadline/提前提交，均为收紧方向。不改策略层与 Lua 标准库（#961/#962 另单）、不改 UI。

---

## 6. 可证伪验收对照

### 验收 1：故障注入（随机/乱序信号 + 250ms 延迟），每笔实际亏损 ≤ 单笔上限；熔断可观测

测试 `service::tests::risk_bottom_line_963_tests::random_entry_fault_injection_single_loss_cap_holds_and_daily_breaker_trips`
（`core/blitzkrieg_core/src/service/tests/risk_bottom_line_963_tests.rs`）：

- 确定性 xorshift 伪随机（固定种子，逐字节可重放）：随机价格（0.25/0.50/0.75，止损位恰落 0.01 网格——断言是**精确**帽而非容差帽）、随机股数、随机 key、四个策略名轮转（乱序/重复/无状态）。
- `fill_model = { taker_latency_ms: 250, taker_rtt_ms: 250 }`（#351 拨盘）；`BK_RISK` 等价内存写入
  `max_single_loss_usd=$0.50`（`Bound::new(0.5, Env)`，per-account，#983 口径）；盘口每周期一格下跌（锯齿），从不跳过止损位（跳空是市场结构性失效，内核以 suppressed-stop/held-exit 报告，不可定价帽——见 §7 残余）。
- 断言：11 笔平仓**每笔 `(entry−exit)×shares ≤ 0.50`**（逐笔，实测最大 0.45）；净额 ≤ 帽+该笔自身记录的手续费；KI-10 连亏熔断 25 次拦截（可观测）；日熔断触发且 `RiskAlert "DAILY LOSS LIMIT REACHED"` 上事件流、sticky 闩锁、随后入场以 `"Daily loss limit"` 按名拒绝。

### 验收 2：门链清单

§1——每门给出触发条件/拦截后行为/代码位置，非结论式。

### 验收 3：出场优先级（同 tick 出场+入场竞争，出场先行，附时序证据）

测试 `strategy_exit_intent_beats_the_same_cycle_entry`：策略在同一 `find_candidates` 周期同时发
出场意图与入场候选。断言（订单 id 由内核按提交序铸造 + 事件流）：

- 出场 SELL `dry_2`（`exit-strategy:tok:take_profit`，Filled）**先于**入场 `dry_3`（`race:BTC:up:0`）；
- 二者 `submitted_at_ms` 相同（同一维护时刻），sell id < buy id；
- 事件流上 SELL 的首条 `OrderUpdate` 先于 BUY 的首条；
- 该周期内 tok 持仓已按 `TakeProfit` 平仓（出场意图当周期消费，不再隔节拍）。
- 对照证据：入场腿以 MakerThenTaker 提交被 post-only 拒后**自身升级**为 taker（`dry_3:escalated` 成交）——升级机制双向可用。

### 验收 4：缺口落成可执行修复

§5 五项缺口全部在本 PR 落地为代码 + 测试（修复①②③④⑤），另附负向测试：

- `exit_maker_close_escalates_at_the_exit_clock`（250ms 整点升级：2249ms 未到不升、2250ms 升级成交）；
- `exit_maker_close_with_negative_timeout_never_escalates`（负值=纯 maker 关断位）；
- `entry_caps_never_trap_the_exit_escalation`（armed `max_position_size=0.5` 拒绝一切入场的同时，出场升级照常成交——帽永不困出场，负向）；
- `the_escalated_close_key_keeps_close_intent`（升级 key 的 close-intent 语义单元测试）。

---

## 7. 已知残余（记录，不在本 PR 范围）

1. **跳空穿越止损**：若盘口一次跨越多个 tick（真实闪崩），止损成交价由市场决定，任何 sizing 都不能
   事先封顶。内核的既有对策是 wick guard（单 tick 跌幅 >8% 抑制并报告，
   `max_bid_wick_pct`，exit_policy.rs:285）+ held-exit 审计事件（#225/#267/#268）——"该跑的没跑"可观测。
   本审查将其定性为市场结构残余，非 sizing 缺口。
2. **replay 内核的策略平仓意图仍走节拍延迟**（#963 修复 ②刻意保留 #390 调度形状以维持回放输出
   逐字节不变）。live 已同期提交；若操作者要求 replay 也同期，需要单独的输出变更决策（新 issue）。
3. `escalation_guard` 的 pair-cost 块仍只对 BUY（出场 SELL 本就不参与成对成本），无需豁免。

## 8. 测试与门禁

- `cargo test --lib`（blitzkrieg_core）：**1304 passed / 0 failed**（新增 6 测）
- `cargo test --tests`（workspace 全部目标）：**2237 passed / 0 failed**
- `cargo clippy --lib --tests`：0 警告
- `cargo fmt --check`：干净
- 更新的既有行为钉：#936 系 maker-close 模式钉 `Maker → MakerThenTaker`（exit_reason_table_tests 6 处、books_mirror_tests 1 处）——rest-first 语义不变，只是有了 deadline。
