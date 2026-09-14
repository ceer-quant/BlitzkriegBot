# 影子进化（Shadow Evolution）

> 模块：`Blitzkrieg_core/src/shadow_evolution/`
> 默认状态：**关闭（opt-in）**——用户显式开启后才生效
> 一句话：**让策略在实盘运行中，通过影子引擎的反事实推演，自动发现更优参数，并毫秒级无停机热切到新参数。**

## 1. 机制

```
主策略（真实资金，参数 A）        影子变体（虚拟资金，参数 B/C/D）
        │                                    │
        └──────────── 同一 Tick 数据流 ───────┘
                          ↓
                  Evolution Evaluator（全部条件满足才触发）
                          ↓  EvolveSignal
                    Guard（锁1 渐变 ≤5% / 锁2 风控不可变）
                          ↓  通过
              ArcSwap<MutableParams> 原子交换
                          ↓
                下一个 Tick 自动使用新参数（不重启/不断连/不动持仓）
```

- **变体**运行与主策略**完全相同**的入场判据（spread_arb 纪律）与**同一份** `exit_policy`，
  只有可变参数不同——因此是真正的反事实比较（同数据、同机器、不同旋钮）。
- 变体只用**虚拟账本**，零真实成本，绝不影响真实持仓/订单/资金。
- 每个变体的 tick 处理包在 `catch_unwind` 内：变体 panic 会被**隔离标记**，不影响主策略。

## 2. 触发条件（必须全部满足）

| 条件 | 阈值（默认） |
|:---|:---|
| 变体样本量 | ≥ 30 |
| 基准样本量 | ≥ 30 |
| 胜率差 | 变体 > 基准 + 5pp |
| 盈亏比差 | 变体 > 基准 × 1.10 |
| 观察时间 | 变体存在 ≥ 5 分钟 |
| 冷却期 | 距上次应用 ≥ 10 分钟 |

## 3. 安全锁（硬编码，不可绕过）

### 锁一：参数只能渐变（≤ ±5%/步）
`guard::validate_gradient` 逐字段校验相对变化。需要更大变化时，必须**多轮进化逐步逼近**
（`guard::clamped_step` 提供该步步逼近）。零值/负值参数一律拒绝。

### 锁二：底层风控不可进化
风控参数（硬止损、连亏熔断、日亏上限、单笔名义上限）**不在** `MutableParams` 中，而是独立的
`ImmutableConfig`——这是**结构性隔离**，不是一个可绕过的检查：`ArcSwap` 里根本没有这些字段。
`guard::validate_immutable` 再做一次纵深断言。

- 可进化：`trend_min_price`、`trend_entry_factor`、`trend_max_entry_price`、`trend_broken_price`
- 不可进化：`hard_stop_loss_pct`、`max_consecutive_losses`、`max_daily_loss_usd`、`max_order_notional`

## 4. 热切换

`HotSwap` 封装 `ArcSwap<MutableParams>`：读侧是**无锁加载**（纳秒级），写侧是**原子发布**。
引擎持有同一 `Arc` 的克隆，`effective_spread_arb()` 每个 evaluate 都叠加当前热参数——
所以切换在**下一个 tick** 生效，不重启进程、不断开 WebSocket、不影响在途订单与持仓。

## 5. 配置

`user_layer/configs/shadow_evolution.toml`（默认 `enabled = false`）：

```toml
[shadow_evolution]
enabled = false
evaluation_window_minutes = 30
min_sample_count = 30
min_win_rate_improvement = 0.05
min_profit_factor_improvement = 0.10
min_observation_minutes = 5
cooldown_minutes = 10
max_gradient = 0.05
variant_count = 3
audit_log_path = "data/evolution/evolution.jsonl"
```

内核启动开关：`--shadow-evolution`（默认关）；测试/运维可覆盖阈值：
`--se-min-samples N`、`--se-cooldown-secs N`、`--se-min-obs-secs N`。

## 6. IPC

请求：

| 方法 | 说明 |
|:---|:---|
| `shadow_evolution.enable` | 开启（构建变体集） |
| `shadow_evolution.disable` | 关闭（清空变体；不影响已应用参数） |
| `shadow_evolution.status` | 状态 + 当前参数 + 变体明细 + 计数 |
| `shadow_evolution.history` | `{ limit }` → 审计记录（含 applied/rejected） |
| `shadow_evolution.rollback` | 回滚到上次进化前参数（无历史则报错） |

事件（`core.event`）：

| 事件 | 说明 |
|:---|:---|
| `EVOLUTION_SIGNAL` | 产生进化提议（应用前） |
| `EVOLUTION_APPLIED` | 参数已原子切换成功 |
| `EVOLUTION_REJECTED` | 提议被安全锁拒绝（含 reason） |

面板/命令：`/crypto-hft shadow-evolution [enable|disable|status|history|rollback]`

## 7. 审计

每次（尝试）进化写入 `data/evolution/evolution.jsonl`：

```json
{
  "timestamp": 1789146497000,
  "signalId": "evolve-1789146497000",
  "fromParams": { "trendMaxEntryPrice": 0.45 },
  "toParams":   { "trendMaxEntryPrice": 0.46 },
  "reason": "combined_improvement",
  "confidence": 0.82,
  "sampleCount": 42,
  "expectedImprovement": 0.08,
  "applied": true,
  "gradientCheck": "passed",
  "immutableCheck": "passed",
  "rollback": false
}
```
被拒绝时 `applied=false` 且带 `rejection` 原因。内存中保留最近 500 条以便 IPC 查询。

## 8. 验收对照

| 验收项 | 实现 | 验证 |
|:---|:---|:---|
| 默认关闭，显式开启 | `enabled=false`；`is_enabled()` | 单测 `disabled_by_default_is_fully_inert` |
| ≥2 个变异策略 | `build_variants(count≥2)` | 单测 `enable_builds_at_least_two_variants_and_hot_swaps` |
| 虚拟资金不影响真实账本 | 变体内置 `VirtualLedger`，不触 OME/ledger | 变体单测 + 隔离设计 |
| 满足条件发出 EvolveSignal | `evaluator::evaluate` | 单测 `emits_signal_when_a_variant_clearly_wins` |
| ArcSwap 原子交换、无停机 | `HotSwap` | 单测 `store_is_visible_to_shared_handle` |
| 下一 tick 用新参数 | engine `effective_spread_arb()` | 集成测试 `hot_swap_reaches_the_engine_and_rolls_back` |
| 变化 >±5% 被拒 | `validate_gradient` | 单测 `gradient_over_limit_is_rejected` |
| 风控不可被改 | `ImmutableConfig` + `validate_immutable` | 单测 `immutable_risk_cannot_be_weakened` |
| 违反锁被记录/告警 | `audit.record_rejection` | 审计单测 |
| 支持手动回滚 | `rollback` | 集成测试 + IPC `shadow_evolution.rollback` |
| 每次进化完整审计 | `AuditLog` JSONL | 审计单测 `records_and_returns_recent` |
| 审计可 IPC 查询 | `shadow_evolution.history` | IPC 实测 |
| 影子崩溃不影响主策略 | `catch_unwind` + `crashed` 标记 | 单测 `shadow_does_not_touch_real_state_and_survives_variant_panic` |

## 9. 设计哲学

> 策略是耗材，引擎是载体，风控是底线。
> 影子进化让策略自己学会适应市场，但永远不能学会突破底线。
> 参数可以变，物理定律不可变。
> 进化是渐变的，不是突变的——因为突变意味着不可预测的风险。
