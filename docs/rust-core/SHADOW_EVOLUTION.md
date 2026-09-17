# 影子进化（Shadow Evolution）

> 模块：`core/blitzkrieg_core/src/shadow_evolution/`
> 默认状态：**关闭（opt-in）**——用户显式开启后才生效
> 一句话：**让每个策略各自在实盘运行中，通过影子孪生的反事实推演发现更优参数，
> 并毫秒级无停机热切到新参数；彼此互不串扰。**

自 E2-c（[#28](https://github.com/ceer-quant/BlitzkriegBot/issues/28)）起，影子进化是
**按策略（per-strategy）**的：每个策略有自己的可进化旋钮、自己的影子变体、自己的
参数单元、自己的审计文件、自己的应用与回滚。**参数、评估、审计、回滚四个维度全部按策略隔离。**

## 1. 机制

```
策略 A（真实资金，参数 a）      策略 B（真实资金，参数 b）
        │                              │
        ├── 影子孪生 A'（a+Δ / a-Δ）    ├── 影子孪生 B'（b+Δ / b-Δ）
        │                              │
        └──────── 同一 Tick 数据流 ─────┘
                          ↓
              按策略评估（该策略自己的「假设成交」逻辑）
                          ↓  每策略独立的 EvolveSignal
               Guard（锁1 渐变 ≤5% / 锁2 风控不可变 / 取值域）
                          ↓  通过
              ArcSwap<StrategyParams> 按策略原子发布
                          ↓
           下一 Tick 该策略自动使用新参数（不重启/不断连/不动持仓/不影响别的策略）
```

- **影子是策略自己造的孪生**：由策略通过 `ShadowFactory::make(params)` 提供，
  与主策略**同一份入场与出场逻辑**，只有旋钮值不同——因此是真正的反事实比较
  （同数据、同机器、同代码、不同旋钮）。内核侧 `EngineStrategyShadow` + `TwinReplay`
  负责按回放流驱动它，内核**不再硬编码任何策略的入场逻辑**（E2-c 的核心改动）。
- 孪生只用**虚拟账本**，零真实成本，绝不影响真实持仓/订单/资金。
- 每个孪生的 tick 处理包在 `catch_unwind` 内：孪生 panic 会被**隔离标记**，不影响主策略，
  也不影响其他策略的进化。

## 2. 触发条件（必须全部满足，逐策略判定）

| 条件 | 阈值（默认） |
|:---|:---|
| 变体样本量 | ≥ 30 |
| 基准样本量 | ≥ 30 |
| 胜率差 | 变体 > 基准 + 5pp |
| 盈亏比差 | 变体 > 基准 × 1.10 |
| 观察时间 | 变体存在 ≥ 5 分钟 |
| 冷却期 | 该策略距上次应用 ≥ 10 分钟 |

**判定是逐策略独立的**：策略 A 满足条件并应用，不会改变策略 B 的变体集、计数器、
基准或冷却期。`spread_arb`（内建，关闭态）与外部 dylib 共存时，各自有独立单元。

## 3. 安全锁（硬编码，不可绕过）

### 锁 0：取值域（最先检查）
每个旋钮在声明时就带 `{value, min, max}`（`KnobSpec`）。`validate_declared` 与
`validate_domain` 在渐变锁**之前**执行：越界值直接拒绝，连「渐变地爬出域外」都不允许。
域是**外层的硬边界**，渐变只是域内的步长纪律。

### 锁一：参数只能渐变（≤ ±5%/步）
`guard::validate_gradient` 逐字段校验相对变化。需要更大变化时，必须**多轮进化逐步逼近**
（`guard::clamped_step` 提供该步步逼近）。零值/负值参数一律拒绝。
越界（超出声明域）与越步（超过 max_gradient）都会留下审计记录。

### 锁二：底层风控不可进化
风控参数（硬止损、连亏熔断、日亏上限、单笔名义上限）**不在**任何 `StrategyParams` 中，
而是独立的 `ImmutableConfig`——这是**结构性隔离**，不是一个可绕过的检查：`ArcSwap`
里根本没有这些字段。`guard::validate_immutable` 再做一次纵深断言。

- 可进化：由**策略自己声明**（内建走 `EngineStrategy::evolvable_knobs()`，
  外挂走可选符号 `bk_strategy_evolvable_knobs`）。例：`spread_arb` 声明 4 个 `trend_*` 旋钮；
  `dog_strategy` 声明 `trendMaxEntryPrice`（0.05–0.90）。
- **不声明 = 明确「不可进化」**，不是「暂时没参数」：该策略拿不到参数单元，也不会出现在
  `status.strategies[]` 里，回滚它会被明确拒绝并说明原因。
- 不可进化：`hard_stop_loss_pct`、`max_consecutive_losses`、`max_daily_loss_usd`、`max_order_notional`

## 4. 热切换（可摘除的覆盖层）

`ParamRegistry` 为每个策略持有 `Arc<ArcSwap<StrategyParams>>`：读侧是**无锁加载**（纳秒级），
写侧是**按策略原子发布**。引擎持有注册表句柄，`EngineStrategy::set_hot_params(Option<Arc<ParamRegistry>>)`：

- `Some(registry)` → **挂上**覆盖层；策略每次 evaluate 读**自己那一个**单元。
- `None` → **摘除**覆盖层。这不是「忽略参数」，而是**物理上没有句柄可读**——
  于是「关闭进化 ⇒ 与改动前逐位一致」是**可证明的**，而不是约定俗成的。
  观测方式：`Core::has_hot_params() == false`。

所以关闭进化时，策略只能读到宿主通过 `on_config` 下发的配置；`data/evolution/` 下
不会有任何新文件。

E2-c 修复：`strategy.load` 载入的新库会**立即**拿到参数单元（`rewire_hot_params`）。
在此之前，进化开启后才载入的库要等到重启才能进化——这是一个真实缺陷，已关闭。
加载回执会同时说明「不可进化（未声明旋钮）」或列出声明的旋钮名。

## 5. 配置

`user_layer/configs/shadow_evolution.toml`（默认 `enabled = false`）。
**该文件已被内核读取**（KI-11 / `MIGRATION_LOG` §59）；优先级
**CLI > `BK_*` env > 本文件 > 代码默认**，启动时会打印每个非默认值的来源。
文件里的窗口/观察/冷却用**分钟**，内核内部用秒，换算在加载处做一次。

```toml
[shadow_evolution]
enabled = false
evaluation_window_minutes = 30
min_sample_count = 30
min_win_rate_improvement = 0.05
min_profit_factor_improvement = 0.10
min_observation_minutes = 5
cooldown_minutes = 10
max_gradient = 0.05                   # +/-5% per step (Lock 1) — 只能收紧，不能放宽
variant_count = 3                     # shadow variants (>=2)
audit_dir = "data/evolution"          # per-strategy: data/evolution/<strategy>.jsonl
```

`audit_dir` 是**目录**而非文件：文件名由策略名派生
（`audit_path_for(strategy)`，非字母数字/`_`/`-`/`.` 的字符替换为 `_`），
于是审计天然按策略分文件，不需要任何配置就能做到隔离。

内核启动开关：`--shadow-evolution`（默认关）；测试/运维可覆盖阈值：
`--se-min-samples N`、`--se-cooldown-secs N`、`--se-min-obs-secs N`。
`max_gradient` 超过内置上限 `0.05`（Lock 1）会被**拒绝并告警**，不是 clamp——
放宽安全锁的请求必须让操作员看见。

## 6. IPC

请求（**`strategy` 是必需参数**——参数、审计、回滚都是按策略的）：

| 方法 | 参数 | 说明 |
|:---|:---|:---|
| `shadow_evolution.enable` | `{}` | 开启（为每个**已声明**旋钮的策略建单元与孪生） |
| `shadow_evolution.disable` | `{}` | 关闭（摘除覆盖层；不影响已应用的参数值本身） |
| `shadow_evolution.status` | `{}` | 聚合值 + `strategies[]` 每策略明细（含自己那组旋钮与域） |
| `shadow_evolution.history` | `{ limit?, strategy? }` | 审计记录；给了 `strategy` 就只看该策略那本账 |
| `shadow_evolution.apply` | `{ strategy, params }` | **手动**应用一组参数（走同一套锁） |
| `shadow_evolution.rollback` | `{ strategy }` | 回滚该策略到上次进化前参数（无历史则报错） |

`status.strategies[]` 每项：`{ strategy, status, params, knobs[{name,value,min,max}],
evolutionsApplied, evolutionsRejected, secondsSinceLastEvolution }`。
`params` / `status` 为 `null` 表示该策略**未声明可进化旋钮**。

事件（`core.event`）：

| 事件 | 说明 |
|:---|:---|
| `EVOLUTION_SIGNAL` | 某策略产生进化提议（应用前） |
| `EVOLUTION_APPLIED` | 该策略参数已原子切换成功 |
| `EVOLUTION_REJECTED` | 提议被安全锁拒绝（含 reason） |

面板/命令：`/crypto-hft shadow-evolution [enable|disable|status|history [strategy]|apply <strategy> <knob=value>…|rollback <strategy>]`

## 7. 审计（按策略分文件）

每次（尝试）进化写入 **`data/evolution/<strategy>.jsonl`**：

```json
{
  "timestamp": 1789146497000,
  "strategy": "dog_strategy",
  "signalId": "evolve-1789146497000",
  "fromParams": { "trendMaxEntryPrice": "0.43" },
  "toParams":   { "trendMaxEntryPrice": "0.4429" },
  "reason": "combined_improvement",
  "confidence": 0.82,
  "sampleCount": 42,
  "expectedImprovement": 0.08,
  "applied": true,
  "gradientCheck": "passed",
  "immutableCheck": "passed",
  "manual": false,
  "rollback": false
}
```

- `strategy` 字段在每条记录里，所以**单文件自描述**：拿到 `dog_strategy.jsonl`
  就能证明其中没有别的策略的记录。
- `manual: true` = 运维手动 `apply`；`rollback: true` = 回滚记录。
- 被拒绝时 `applied=false` 且带 `rejection` 原因。
- 内存中每策略保留最近 500 条以便 IPC 查询（`recent(strategy, limit)`）。

> **迁移说明（D-17）**：E2-c 之前只有一个全局文件 `data/evolution/evolution.jsonl`，
> 因生产从未开启进化（`enabled=false`）而始终是 0 字节。它**被保留不删**，
> 新代码只读写 `data/evolution/<strategy>.jsonl`。旧文件不再被任何代码路径追加。

## 8. 验收对照

| 验收项 | 实现 | 验证 |
|:---|:---|:---|
| 默认关闭，关闭时完全惰性 | `enabled=false`；无注册、无文件 | 单测 `disabled_by_default_is_inert`、`observation_alone_never_moves_parameters` |
| 参数模型按策略命名 | `MutableParams` = `BTreeMap<strategy, StrategyParams>` | `knobs.rs` 单测 + 集成 `apply_and_rollback_move_exactly_one_strategy` |
| 每策略自证旋钮与取值域 | `evolvable_knobs()` / 可选符号 | 单测 `enable_scaffolds_every_strategy_and_publishes_its_own_cell`；门禁 `core:strategy-evolve` 断言 `declares evolvable knobs: trendMaxEntryPrice` |
| 未声明 = 不可进化 | 无符号 → 无单元 | 单测 `a_strategy_that_declares_nothing_gets_no_unit`；集成 `an_undeclared_strategy_is_reported_not_evolvable` |
| 两策略并行互不串扰 | 每策略独立单元与计数器 | 单测 `two_strategies_evolve_in_parallel_without_cross_talk`；集成 `two_strategies_evolve_in_parallel_without_cross_talk_through_the_core`（真 `Core`+`Engine`，含内建 `spread_arb` 全程不动） |
| ≥2 个变异策略 | `ShadowFactory::make` 生成 ±Δ 孪生 | 单测 `build_produces_a_baseline_plus_directed_single_knob_variants`、`the_sweep_visits_every_knob_and_both_directions` |
| 虚拟资金不影响真实账本 | 孪生走内核侧 `TwinReplay`，从不触 OME/ledger/positions | 结构性隔离（无句柄）：单测 `metrics_are_windowed_and_identical_for_baseline_and_variant` 证明孪生只在自己的回放账本里累计 |
| 满足条件发出 EvolveSignal | `evaluator::evaluate` 逐策略 | 单测 `emits_a_strategy_tagged_signal_when_a_variant_clearly_wins`、`the_counterfactual_is_a_real_decision_difference` |
| 不满足条件不发（含冷却） | 六条全满足才触发 | 单测 `no_signal_when_baseline_has_too_few_samples`、`a_losing_variant_never_qualifies`、`cooldown_suppresses_signals_per_strategy`、`an_empty_set_never_signals` |
| 取值域宽度为 0 不算可进化 | `KnobSpec::is_coherent` / `contains` | 单测 `a_zero_width_domain_is_not_mutable`、`no_declaration_means_no_variants_at_all`、`variants_are_deterministic` |
| ArcSwap 按策略原子交换、无停机 | `ParamRegistry` | 单测 `enable_scaffolds_every_strategy_and_publishes_its_own_cell` |
| 下一 tick 用新参数 | engine 的 hot 覆盖 | 集成 `two_strategies_evolve_in_parallel_without_cross_talk_through_the_core` |
| 变化 >±5% 被拒 | `validate_gradient` | 单测 `guard_rejects_gradient_beyond_lock`、`gradient_over_limit_is_rejected`；门禁拒绝 +35% |
| 越出取值域被拒 | `validate_domain` | 集成（0.99 越界被拒）；门禁拒绝 0.99 |
| 风控不可被改 | `ImmutableConfig` + `validate_immutable` | 单测 `immutable_risk_cannot_be_weakened` |
| 违反锁被记录/告警 | `audit::record_rejection` | 单测 `a_disabled_audit_writes_no_files`、`each_strategy_gets_its_own_file_and_history` |
| 回滚按策略独立 | `rollback(strategy, …)` | 单测 `rollback_is_per_strategy`；集成 `apply_and_rollback_move_exactly_one_strategy`；门禁回滚 A 不动 B |
| 每次进化完整审计（分文件） | `data/evolution/<strategy>.jsonl` | 单测 `each_strategy_gets_its_own_file_and_history`；集成断言 `alpha.jsonl` 无 beta/spread_arb 记录且 beta/spread_arb 文件不存在；门禁断言 `spread_arb.jsonl` 从未产生 |
| 手动 apply / 回滚有标记 | `manual` / `rollback` 字段 | 单测 `manual_and_rollback_records_are_flagged`、`a_manual_apply_is_audited_and_domain_checked`；集成 `apply_and_rollback_move_exactly_one_strategy` |
| 关掉进化行为逐位一致 | 覆盖层**摘除**（`None`） | 集成 `evolution_off_is_byte_for_byte_the_previous_behaviour` |
| 孪生重放的是同一份出场策略 | `ExitConfig` 取自 `config.positions.exit` | 单测 `the_exit_policy_replayed_is_the_configured_one` |
| 孪生崩溃不影响主策略 | `catch_unwind` + `crashed` 标记（`mod.rs` 逐孪生 tick）| **机制在位但无专门回归测试**（旧版那条测试随 `hot_swap.rs` 一并删除）——见 §10 已知缺口 |

本地门禁：`npm run core:strategy-evolve`（真 release 二进制 + 真 dog cdylib + 私有 socket +
临时工作目录，断言声明可见、逐策略隔离、域/步长拒绝、分文件审计、回滚、关闭态不变）。

## 10. 已知缺口（登记不隐藏）

1. **孪生崩溃隔离无回归测试**：`catch_unwind` + `crashed` 隔离逻辑在
   `mod.rs`（逐孪生 tick）与 `strategies/shadow_twin.rs::catch` 里，但 E2-c 重写时
   旧测试随 `hot_swap.rs` 一起删除了，暂无专门断言「孪生 panic 后主策略照常交易、
   该孪生被永久跳过」。机制没有被移除，只是缺一条测试。
2. **`data/evolution/evolution.jsonl`（旧的全局文件，0 字节）保留不删**（D-17）。
   新代码不读也不写它；没有代码路径会再向它追加。是否物理删除需用户裁决。
3. 生产**仍未开启**影子进化（`enabled=false`）。E2-c 的验收在测试与门禁里完成，
   不是在生产里开跑；「影子进化转正」属 v0.2。

## 11. 设计哲学

> 策略是耗材，引擎是载体，风控是底线。
> 影子进化让策略自己学会适应市场，但永远不能学会突破底线。
> 参数可以变，物理定律不可变。
> 进化是渐变的，不是突变的——因为突变意味着不可预测的风险。
> 每个策略只对自己的旋钮负责：进化不是集体投票，而是各自修行。
