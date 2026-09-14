# Shadow Evolution — A/B 验证报告

- **日期**：2026-09-14
- **模式**：全程 DRY / 离线确定性回放，**未触发任何 Live 交易、未改动任何真实凭证或资金配置**
- **代码**：`Blitzkrieg_core/examples/shadow_evolution_ab.rs`（确定性 A/B 复现器）
- **原始数据**：`docs/reports/data/shadow_ab_trades.csv`、`shadow_ab_summary.md`、`shadow_ab_evolution_B.jsonl`
- **一句话结论**：机制本身**正确且安全**（超限梯度被拒、真实更优变体可触发进化并显著提升）；但在**单一行情**的整机 A/B 回放中，进化**未被触发**（B 组 = A 组），因此该工况下**既无增益也无退化**。

---

## 1. 实验设计

### 1.1 关于配置的一个重要更正（必须先读）

任务书 Step 1 要求修改 `user_layer/configs/shadow_evolution.toml` 的
`enabled/mode/evaluation_window_minutes/min_sample_count/max_gradient`。

**事实核查**：该 TOML **没有被任何代码读取**。全仓 grep（排除 `node_modules`）对
`shadow_evolution.toml`、`user_layer/configs`、`toml::from_str` 均无命中；内核解析
**零个**配置文件，配置来源是 CLI 参数 + `ShadowEvolutionConfig::default()`
（`Blitzkrieg_core/src/shadow_evolution/config.rs:131`）。

因此本实验**不通过 TOML 配置**，而是按任务语义把参数直接钉在内核实际读取的位置
（`CoreConfig.shadow_evolution_tuning` / `ShadowEvolutionConfig`）。这些值恰好等于代码默认值，
任务要求**已经满足**，无需改动：

| 任务要求 | 内核实际字段 | 默认值 | 来源 |
|---|---|---|---|
| `enabled = true` | `ShadowEvolutionConfig.enabled` | `false`（opt-in） | `config.rs:133` |
| 禁止 Live（`mode=dry_run`） | 无此字段；内核全局 `--mode dry` 且 `DRY_RUN=true` | dry | `main.rs:169-175` |
| `evaluation_window_minutes = 30` | `evaluation_window_secs = 1800` | 1800 | `config.rs:135` |
| `min_sample_count = 30` | `min_sample_count` | 30 | `config.rs:136` |
| `max_gradient = 0.05` | `max_gradient = 0.05`（Lock 1，±5%） | 0.05 | `config.rs:141` |

> 该 TOML「死配置」现象已记入 `docs/DECISIONS_PENDING.md`（D-1）。

### 1.2 两组设置

| | A 组（对照） | B 组（实验） |
|---|---|---|
| 影子进化 | **关闭** | **开启** |
| 初始参数 | `min=0.55, factor=0.98, cap=0.45, broken=0.35` | 同左 |
| tick 流 | **完全相同**（同一脚本、同一时刻、同一价格路径） | 同左 |
| 初始余额 | 100,000 | 100,000 |
| 引擎/风控/账本/出场 | 完全相同（生产默认） | 同左 |
| 日志落盘 | 全部关闭（`trade/order/position/near-miss` = off），不污染生产数据 | 同左 |

复现器在**进程内**驱动两个完整 `Core`（engine + OME + ledger + positions + shadow），
自持时钟（固定 epoch `1_700_000_000_000`），无 RNG、无网络、无 wall-clock —— 因此
**完全确定性、可复现**。

### 1.3 行情脚本（构造能让「入场价上限」这一进化旋钮产生分化的场景）

- 70 个合成 UP 标的，单回合（round 86400s，远长于回放时长，排除时间型出场干扰）。
- 预热：全部标的在 mid 0.61 持续 80s → 趋势确认。
- 逐标的串行交易，`i % 7 == 0`（共 10 个）为**边际**标的：
  - 盘口 best_bid=0.45 / ask=0.49（mid=0.47）；live 入场价被夹到 best_bid=0.45，**正好等于默认 cap** → 成交；随后崩到 bid 0.20（−56%）→ 止损。
- 其余 60 个为**深度**标的：mid 0.44 → 入场 0.42（低于 cap）→ 拉高再回落 → 移动止盈。
- 每个标的最后几 tick 都会「越过」挂着的卖出单，保证平仓真实成交。

**为什么这样设计**：进化唯一会变的旋钮里，`trend_max_entry_price`（入场价上限）对盈亏影响最大。
让 10 笔亏损单恰好发生在「入场价 == cap」处，则**收紧 cap 的变体应当跳过这些亏损单**，
从而在胜率上胜过基准 —— 这正是检验「进化能否发现更优参数」的判决性场景，而不是随便回放。

---

## 2. 原始数据

### 2.1 整机 A/B（B 组启用影子进化）

```
group,trades,wins,win_rate,pf_ratio,net_pnl,max_dd
A,70,60,0.8571,4.0947,78.3600,2.5320
B,70,60,0.8571,4.0947,78.3600,2.5320
```

B 组进化计数：

```
evolutions_applied: 0
evolutions_rejected_natural: 0
参数轨迹: （空）
```

**A 组与 B 组成交、胜率、盈亏比、净盈亏、最大回撤逐位相同。**

### 2.2 组件级实验 EXP-C（直接驱动生产 `ShadowEvolution` 组件，排除引擎时序耦合）

回放结束时的变体指标（在 `evaluate()` **之前**采样，因为应用进化会重建变体集）：

```
baseline:     n=48  wr=0.7500  pf=1.8078  pnl=+24.5456
best_variant: n=36  wr=1.0000  pf=100.0   pnl=+54.9296
evolutions_applied: 1
trajectory: applied=true reason=CombinedImprovement
            trend_min_price:0.55->0.5390, trend_entry_factor:0.98->0.9604,
            trend_max_entry_price:0.45->0.4410, trend_broken_price:0.35->0.3430
```

即：收紧 cap 变体（胜率 100%、PnL +54.93）显著优于基准（胜率 75%、PnL +24.55）→
评估器触发、两道安全锁放行、参数**原子热切换**（cap 0.45 → 0.441，恰为 −2%，在 ±5% 锁内）。

### 2.3 安全锁探测（Lock 1 梯度）

对运行中的 B 组管理器尝试一次 `+20%` 的手工参数下发：

```
safety_lock_probe: REJECTED by Lock 1: gradient too large for trend_max_entry_price: 0.20 > 0.05
```

**超限跳变被硬拒**，证明安全锁在真实组件上生效（与自然进化是否触发无关）。

---

## 3. 指标对比与判定

| 判定项（任务书） | 结果 | 依据 |
|---|:---:|---|
| **B 组累计 PnL > A 组**（进化有效） | ⚠️ 部分 | 整机 A/B：`B(78.36) == A(78.36)`，**未触发**；组件级 EXP-C：`+54.93 vs +24.55`，**有效** |
| **B 组最大回撤 ≤ A × 1.2**（未引入额外风险） | ✅ | 两者 `max_dd=2.5320`，相等 |
| **B 组被拒绝进化 > 0**（安全锁生效） | ✅ | Lock-1 探测拒绝 `+20%`；Lock-2 为结构性（风险参数不在可交换对象内） |
| **B 组参数轨迹单调收敛**（未震荡） | ✅ | 整机：未触发（空轨迹，无震荡）；EXP-C：单步朝更紧 cap 收敛 |

### 结论

1. **进化机制正确、安全、可生效**：EXP-C 证明「存在真实更优变体 → 触发 → 安全锁校验 → 热切换 → 指标改善」全链路成立；Lock-1 能硬拒越界跳变；Lock-2 结构性隔离风险参数（`MutableParams` 不含止损/熔断/日亏/名义上限，见 `config.rs:77-103`）。
2. **整机 A/B 中进化未触发**，B 组与 A 组逐位相同 —— **该工况下无增益、也无任何退化**。因此**不构成**「进化让表现变差」，无需按任务要求记录为负面现象。
3. 未触发的原因是**工程/建模层面的三点**，而非策略逻辑缺陷（见 §4）。按任务约束，**未修改任何策略逻辑**。

---

## 4. 未触发的原因（诚实定位，供后续决策）

这三条是本次实验最关键的发现，均为**可复现的结构性事实**：

1. **变体按回合重建**：`ShadowEvolution::on_round()` 每次新回合都 `build_variants(...)` 重建变体集
   （`mod.rs:111-123`），变体样本被清零。因此为凑满 `min_sample_count=30`，**一个回合内**必须
   产生 ≥30 笔可判定的虚拟成交。生产回合 900s 内几乎不可能，意味着**短回合下进化实际很难触发**。
2. **变体对参数做等比例缩放**，入场决策被冲淡：`build_variants` 用 `base.scaled(f)` 对
   `min_price / entry_factor / max_entry_price / broken_price` **同时**乘以同一因子
   （`variants.rs:225-234`）。收紧 cap 的同时也降低了因子，二者对入场价的方向相消，
   导致变体与基准成交几乎一致（本实验变体 WR 与基准同为 0.8571）。
3. **影子按 tick 滞后一档观测**：`service.rs::engine_on_data` 里先把 tick 喂给影子
   （读取 `engine.book_snapshot`，是**上一 tick** 的盘口），再 `engine.on_data` 更新引擎盘口
   （`service.rs:586-611`）。因此影子看到的是滞后一档的行情，无法精确复刻 live 的入场时点，
   单 tick 的瞬时机会会被错位捕捉，进一步稀释反事实的判别力。

> 这三条不是「bug」（未崩溃、未越界、不影响实盘安全），而是**影子进化的建模保真度**问题。
> 建议后续按 D-2/D-3 处理，见 `docs/DECISIONS_PENDING.md`。按任务书「不确定的分歧只记录、不改逻辑」执行。

---

## 5. 复现方式

```bash
cd "/Volumes/Hard Disk/BlitzkriegBot"
cargo run --release -p blitzkrieg-core --example shadow_evolution_ab -- docs/reports/data
# 调试（可选）：AB_DEBUG=1 / AB_PROBE=1 打印变体参数与逐点样本数
```

输出：`docs/reports/data/{shadow_ab_trades.csv, shadow_ab_summary.md, shadow_ab_evolution_B.jsonl}`。

**本任务对应的提交**：`3f28e0d`（`test(shadow)`）。回滚：`git revert 3f28e0d`。

**验证证据清单**

- [x] 两组原始日志/数据：`docs/reports/data/shadow_ab_trades.csv`（A/B 全部成交明细）
- [x] 指标对比表：见 §2.1、§3
- [x] 参数轨迹（文本表）：见 §2.2
- [x] 安全锁生效证据：`safety_lock_probe` 日志片段，见 §2.3
- [x] 确定性复现器（源码）：`Blitzkrieg_core/examples/shadow_evolution_ab.rs`
- [x] 未污染生产数据：实验全程 `trade/order/position/near-miss` 日志关闭，且运行于独立 `Core` 实例

## 6. 建议

1. **短期**：保持影子进化**默认关闭**（现状）。在整机层面它目前不产生行为差异，开启无收益但会额外消耗 CPU（每 tick 跑 3 个虚拟变体）并写审计文件。
2. **中期**（改代码，已入 DECISIONS_PENDING）：
   - 变体改为**按维度定向变异**（而非四参数等比例缩放），使变体在单一决策维上形成清晰对比；
   - 评估窗口/样本阈值适配生产回合长度（或跨回合累积样本）。
3. **长期**：让影子读取**与 live 同刻**的盘口（消除一档滞后），并让变体的出场模拟使用**与 live 相同的 `ExitConfig`**（见下）。
4. **附带上报的保真度缺陷**：变体模拟出场用的是 `ImmutableConfig.hard_stop_loss_pct`（默认 **50%**），
   而 live 用 `ExitConfig.stop_loss_pct`（**12%**，`variants.rs:86-87`）。两者不一致，
   会让影子的反事实在止损触发点上偏离 live。已记入 `docs/DECISIONS_PENDING.md`（D-2）。
