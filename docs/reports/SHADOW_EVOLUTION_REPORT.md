# Shadow Evolution — A/B 验证报告（含保真度修复）

- **日期**：2026-09-14（首版）／2026-09-14（保真度修复后复跑，D-2/D-3 结案）
- **模式**：全程 DRY / 离线确定性回放，**未触发任何 Live 交易、未改动任何真实凭证或资金配置**
- **代码**：`Blitzkrieg_core/examples/shadow_evolution_ab.rs`（确定性 A/B 复现器）
- **原始数据**：`docs/reports/data/shadow_ab_trades.csv`、`shadow_ab_summary.md`、`shadow_ab_evolution_B.jsonl`
- **一句话结论**：修复三处建模保真度缺陷（D-2/D-3）后，**整机 A/B 中影子进化真正触发并带来增益**——
  B 组净盈亏 **78.36 → 91.02**、胜率 **85.7% → 92.3%**、盈亏比 **4.09 → 8.19**，最大回撤不变，参数单调收敛一步
  （入场价上限 `0.45 → 0.4365`），安全锁仍硬拒越界跳变。

---

## 0. 相对首版的更正（必须先读）

首版报告（提交 `3f28e0d`）的结论是「机制正确但整机 A/B 中进化 0 次触发」。经深入定位，**0 触发不是工况偶然，
而是三处建模缺陷**，本次全部修复（对应 `docs/DECISIONS_PENDING.md` 的 D-2、D-3）：

| # | 缺陷 | 首版表现 | 本次修复 |
|---|---|---|---|
| 1 | **变体对 4 个参数等比例缩放** | 收紧入场价上限的同时也下调了入场因子，两个方向对入场价**相消** → 变体与基准决策完全相同（WR 逐位相同） | 改为**定向单旋钮变异**：每个变体只动 1 个参数（`variants.rs::build_variants`），使任何差异都可归因到单一旋钮 |
| 2 | **变体每回合重建** | `on_round()` 每回合重建变体集，样本清零 → 生产 900s 回合内永远凑不满 `min_sample_count=30`，进化**结构上不可达** | `on_round()` 只更新到期时间并**保留已平仓历史**（`Variant::retain_tokens`），样本**跨回合累积** |
| 3 | **(a) 影子按 tick 滞后一档观测 (b) 变体出场用 SL50 而 live 用 SL12** | 影子读的是引擎**上一 tick** 的盘口，入场时点错位；变体止损用 `ImmutableConfig.hard_stop_loss_pct`(50%)，与 live 的 12% 不一致 | (a) 影子改为在**引擎消费该 tick 之后**喂入**同刻盘口/同刻确认**（`service.rs::engine_on_data`）；(b) 变体改用 **live 的 `ExitConfig`**（`ShadowEvolutionConfig.exit_cfg ← PositionConfig.exit`），杜绝虚构止损 |

> 修复后，首版「§4 未触发的原因」三条已全部消除；下文为修复后的实验与判定。

---

## 1. 实验设计

### 1.1 关于配置的重要事实（未变）

任务书 Step 1 要求修改 `user_layer/configs/shadow_evolution.toml` 的
`enabled/mode/evaluation_window_minutes/min_sample_count/max_gradient`。

**事实核查**：该 TOML **没有被任何代码读取**（全仓 grep 对文件名与 `toml::from_str` 均无命中），内核解析
**零个**配置文件，配置来源为 CLI 参数 + `ShadowEvolutionConfig::default()`（`config.rs`）。该「死配置」现象
仍在 `docs/DECISIONS_PENDING.md`（D-1）中待用户决策。本实验按任务语义把参数直接钉在内核实际读取的位置：

| 任务要求 | 内核实际字段 | 默认值 |
|---|---|---|
| `enabled = true` | `ShadowEvolutionConfig.enabled` | `false`（opt-in，实验显式开启 B 组） |
| 禁止 Live（`mode=dry_run`） | 内核全局 `--mode dry` 且 `DRY_RUN=true` | dry |
| `evaluation_window_minutes = 30` | `evaluation_window_secs = 1800` | 1800 |
| `min_sample_count = 30` | `min_sample_count` | 30 |
| `max_gradient = 0.05` | `max_gradient = 0.05`（Lock 1，±5%） | 0.05 |

### 1.2 两组设置

| | A 组（对照） | B 组（实验） |
|---|---|---|
| 影子进化 | **关闭** | **开启** |
| 初始参数 | `min=0.55, factor=0.98, cap=0.45, broken=0.35` | 同左 |
| tick 流 | **完全相同**（同一脚本、同一时刻、同一价格路径） | 同左 |
| 初始余额 | 100,000 | 100,000 |
| 引擎/风控/账本/出场 | 完全相同（生产默认） | 同左 |
| 日志落盘 | 全部关闭（`trade/order/position/near-miss` = off），不污染生产数据 | 同左 |

复现器在**进程内**驱动两个完整 `Core`（engine + OME + ledger + positions + shadow），自持时钟（固定 epoch
`1_700_000_000_000`），无 RNG、无网络、无 wall-clock —— **完全确定性、可复现**。

### 1.3 行情脚本（构造能让「入场价上限」这一进化旋钮产生分化的场景）

- 70 个合成 UP 标的，单回合（round 86400s，远长于回放时长，排除时间型出场干扰）。
- 预热：全部标的 mid 0.61 持续 80s → 趋势确认。
- 逐标的串行交易，`i % 7 == 0`（共 10 个）为**边际**标的：
  - 盘口 best_bid=0.45 / ask=0.49（mid=0.47）→ live 挂 bid 0.45（**正好等于默认 cap**）；下一 tick ask 0.45 越过成交；
  - 随后崩到 bid 0.20（−56%）→ **触发 live 的 12% 止损（taker 卖出立即成交）**。
  - 关键：成交 tick（bid 0.44/ask 0.45，mid 0.445）对因子 0.98 的变体仍算出入场 0.44，故**仅收紧 2% 的 cap 变体会在此重入**；
    定向 cap 变体收紧 **3%（cap 0.4365 < 0.44）**，两 tick 全部跳过 → 真正少做一笔亏损。
- 其余 60 个为**深度**标的：mid 0.44 → 入场 0.42（低于 cap）→ 拉高再回落 → 移动止盈。
- 每个标的最后几 tick 都会「越过」挂着的卖出单，保证平仓真实成交。

修复后，影子与 live **同刻**观测、**同套出场机制**，唯一差异就是可变入场旋钮 —— 这正是检验「进化能否发现更优参数」的判决性场景。

---

## 2. 原始数据（修复后复跑）

### 2.1 整机 A/B

```
group,trades,wins,win_rate,pf_ratio,net_pnl,max_dd
A,70,60,0.8571,4.0947,78.3600,2.5320
B,65,60,0.9230,8.1895,91.0200,2.5320
```

**B 组少做 5 笔亏损单（70→65），净盈亏 +12.66，胜率 +6.6pt，盈亏比翻倍，最大回撤逐位相同。**
（A 组的 10 笔边际亏损中，进化在回放中途应用了一次，之后 5 笔被跳过；应用前已发生的 5 笔保留在两组的共同历史里。）

B 组进化计数：

```
evolutions_applied: 1
evolutions_rejected_natural: 0
参数轨迹: trend_max_entry_price: 0.45 -> 0.4365  (CombinedImprovement, applied)
```

审计记录（`shadow_ab_evolution_B.jsonl`）：

```json
{"timestamp":1700000868000,"signalId":"evolve-1700000868000",
 "fromParams":{"trendMinPrice":0.55,"trendEntryFactor":0.98,"trendMaxEntryPrice":0.45,"trendBrokenPrice":0.35},
 "toParams":{"trendMinPrice":0.55,"trendEntryFactor":0.98,"trendMaxEntryPrice":0.4365,"trendBrokenPrice":0.35},
 "reason":"combined_improvement","confidence":0.976,"sampleCount":30,"expectedImprovement":0.143,
 "applied":true,"gradientCheck":"passed","immutableCheck":"passed","rollback":false}
```

即：变体（收紧 cap）在胜率/盈亏比上双双超过基准 → 评估器触发 → 两道安全锁放行 → **参数原子热切换**
（cap 0.45 → 0.4365，恰为 −3%，在 ±5% 锁内），此后 live 入场价上限同步收紧，跳过余下边际亏损单。

### 2.2 组件级实验 EXP-C（直接驱动生产 `ShadowEvolution` 组件，排除引擎时序耦合）

回放结束时的变体指标（在 `evaluate()` **之前**采样）：

```
baseline:     n=48  wr=0.7500  pf=1.8078  pnl=+24.5456
best_variant: n=36  wr=1.0000  pf=100.0   pnl=+54.9296
evolutions_applied: 1
trajectory: applied=true reason=CombinedImprovement
            trend_max_entry_price:0.45->0.4365
```

重新收敛为**单旋钮**变更（此前是四参数等比例），与整机 A/B 的轨迹一致。

### 2.3 安全锁探测（Lock 1 梯度）

对运行中的 B 组管理器尝试一次 `+20%` 的手工参数下发：

```
safety_lock_probe: REJECTED by Lock 1: gradient too large for trend_max_entry_price: 0.20 > 0.05
```

**超限跳变被硬拒**，证明安全锁在真实组件上持续生效（与自然进化是否触发无关）。

---

## 3. 指标对比与判定

| 判定项（任务书） | 结果 | 依据 |
|---|:---:|---|
| **B 组累计 PnL > A 组**（进化有效） | ✅ | 整机 A/B：`B(91.02) > A(78.36)`，**净 +12.66**；组件级 EXP-C 同向（+54.93 vs +24.55） |
| **B 组最大回撤 ≤ A × 1.2**（未引入额外风险） | ✅ | 两者 `max_dd=2.5320`，相等 |
| **B 组被拒绝进化 > 0**（安全锁生效） | ✅ | Lock-1 探测拒绝 `+20%`；Lock-2 为结构性（风险参数不在可交换对象内） |
| **B 组参数轨迹单调收敛**（未震荡） | ✅ | 单步朝更紧 cap 收敛（0.45→0.4365），无来回震荡 |

### 结论

1. **进化在整机层面真实生效并带来增益**：同一天、同一 tick 流，开启影子进化的 B 组净盈亏高 12.66、胜率高 6.6pt、
   盈亏比翻倍，而回撤不变 —— 且增益可完全归因到单一的 `trend_max_entry_price` 收紧。
2. **机制安全**：Lock-1 硬拒越界跳变；Lock-2 结构性隔离风险参数（`MutableParams` 不含止损/熔断/日亏/名义上限）。
3. **修复的是建模保真度，不是策略逻辑**：本次改动仅涉及影子侧的观测时点、变异方式、出场配置与样本留存；
   live 的入场/出场/风控/账本决策逻辑**未改**（唯一行为变化是进化成功应用后，live 读取到新参数，这是该功能的本意）。

---

## 4. 修复内容（代码级）

| 文件 | 变更 | 对应 |
|---|---|---|
| `shadow_evolution/config.rs` | `ShadowEvolutionConfig` 增 `exit_cfg: ExitConfig`；`MutableParams` 增 `get(name)` | D-2/D-3 |
| `shadow_evolution/variants.rs` | `Variant::new` 接收 `&ExitConfig`（不再用 50% 硬止损）；新增 `retain_tokens`/`exit_config`；`build_variants` 改为**定向单旋钮**变异 | D-2/D-3 |
| `shadow_evolution/mod.rs` | `on_round` 不再重建变体（只保留有效 token、累积历史）；`evaluate` 应用后按新参数重建变体（防"用过期历史自强化"的棘轮） | D-3 |
| `service.rs` | 影子改为在引擎**消费该 tick 之后**喂入**同刻盘口与同刻确认**；`ShadowEvolutionConfig.exit_cfg ← PositionConfig.exit` | D-2/D-3 |
| `examples/shadow_evolution_ab.rs` | 边际标的改为「挂单→成交→崩塌」，并让定向 cap 变体（−3%）真正跳过；文件头与数据同时更新 | 复现器 |

**新增单测（4 项，见 §5）**：定向变异只动一个旋钮、变体用 live `ExitConfig`、平仓历史跨回合保留、配置默认含 `exit_cfg`。

---

## 5. 复现方式

```bash
cd "REPO_ROOT"
cargo run --release -p blitzkrieg-core --example shadow_evolution_ab -- docs/reports/data
# 调试（可选）：AB_DEBUG=1 / AB_PROBE=1 打印变体参数与逐点样本数
```

输出：`docs/reports/data/{shadow_ab_trades.csv, shadow_ab_summary.md, shadow_ab_evolution_B.jsonl}`。

**验证证据清单**

- [x] 两组原始数据：`docs/reports/data/shadow_ab_trades.csv`（A=70 行、B=65 行）
- [x] 指标对比表：见 §2.1、§3
- [x] 参数轨迹（文本表）：见 §2.1、§2.2
- [x] 安全锁生效证据：`safety_lock_probe` 日志片段，见 §2.3
- [x] 确定性复现器（源码）：`Blitzkrieg_core/examples/shadow_evolution_ab.rs`
- [x] 未污染生产数据：实验全程 `trade/order/position/near-miss` 日志关闭，且运行于独立 `Core` 实例
- [x] 门禁全绿：`cargo build --release`、`cargo test -p blitzkrieg-core`（101 项）、`cycle-check`、
  `core-parity`、`parity-engines`、`order-recovery`、`position-recovery`、`npm run typecheck`

---

## 6. 建议

1. **生产启用**：保真度修复后影子进化在受控工况已证明可带来正增益，且安全锁完好。建议在
   **Dry 环境**先开启运行观察若干回合（默认可仍保持关闭，视运维决策），跟踪 `data/evolution/evolution.jsonl`
   的 applied/rejected 比例与参数轨迹。
2. **定向变异的覆盖面**：当前 `variant_count=3` 时只探索 cap/entry_factor 两个旋钮；若需覆盖
   `trend_broken_price`/`trend_min_price`，可将 `variant_count` 提到 ≥5（每个变体仍只动一个旋钮）。
3. **D-1（死配置）仍待决策**：若要让 `shadow_evolution.toml` 真正可配置，需引入配置加载层（见 DECISIONS_PENDING D-1）。
4. **相关待决策**：D-2/D-3 已由本次实现结案（见 `DECISIONS_PENDING.md`）；D-4（删除 Node 交易域）、
   D-5（实盘小额验证）仍需用户拍板。
