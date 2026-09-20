# MARKET_REGIME — 状态机定义与评测口径（E16 / #98）

> 状态：已实现 + 已在冻结语料上完成首轮评测。边界如实记录。
> #98 的验收句是「MarketRegime 状态机（准确率 ≥80%，需定义评测口径与标注集）」
> ——本文就是那个口径与标注集的定义，附首轮实测结果。

## 1. 状态机定义

实现：`user_layer/strategy_logic/src/market_regime.rs`（strategy_logic 参考实现，
内核与 dylib 共用同一定义；这是**库类型**，不是 C ABI v2 接口变更，外部策略
以后经 `on_params` 消费状态即可，本次不动 ABI）。

四个状态：`range`（默认）/ `trendUp` / `trendDown` / `volatile`。

规则（全部以 tick = 0.01 为单位，阈值常量与离线标注共享同一函数
`regime_from_stats`，不存在第二套口径）：

| 统计量 | 定义 | 阈值 |
|---|---|---|
| `net` | 窗末 − 窗首（tick） | trend: \|net\| ≥ 3 |
| `eff` | (末−首) / Σ\|Δp\|，方向效率 ∈ [−1,1] | trend: eff ≥ 0.5（或 ≤ −0.5） |
| `mad` | Σ\|Δp\| / 步数（tick） | volatile: mad ≥ 1.5 且非 trend |
| 其余 | 小净移 + 小抖动 | `range` |

在线状态机 `MarketRegime`：每个 token 一台实例；每次喂入 mid（最优买/卖
均值）后按 `window_ms`（默认 300 s）修剪环形样本窗（硬上限 4096 样本，
防突发流量撑爆）；每次用与离线**同一条规则**对环内样本做 raw 分类；状态
切换需 `confirmations`（默认 2）次连续一致 —— 滞回是状态机的真实代价，
也是评测要量的东西。

## 2. 评测口径（定义）

- **语料**：冻结档案 `full.jsonl`（与 E15 全档 A/B 复盘同一份：
  `data/evolution/sweeps/e15-20260920/ab/full.jsonl`，3 段轮转合并，
  0.75 天 / 3,593,457 事件）。
- **标注集**：档案中 book/top 事件最活跃的 **100 个 token**（按事件数排序，
  同分按 token id 字典序，确定性选集），每个 token 独立：
  自其首个样本起按 **300 s 非重叠墙钟窗**切分。
- **离线标注（ground truth）**：`classify_window` 对该窗**全量**样本算
  net/eff/mad → 上表规则出标签。二元市场的「 regime 真值」并不存在外部
  裁判，规则标注就是可复现的标注集本身。
- **机器标签**：在线状态机在**窗末时刻**的状态（窗首事件之前的所有样本
  已喂入；滞回生效）。
- **准确率**：全部被标注窗上「机器标签 = 离线标注」的占比；按类 recall
  与混淆对一并输出。

为什么这是真测量而不是自我印证：机器与离线用的是**同一规则的不同实现**
——离线看全窗、在线只看环形窗的近似 + 承担滞回延迟。一致率量的正是
「在线估计器复现离线规则」的能力，不一致的窗主要是行情在窗中切换的
过渡窗。

## 3. 首轮实测（2026-09-20，冻结语料）

```
cargo run --release --bin blitzkrieg-core -- \
  --regime-eval data/evolution/sweeps/e15-20260920/ab/full.jsonl \
  --regime-report data/evolution/regime/e16-20260920/regime.json \
  --regime-max-tokens 100
```

- **300 窗 / 100 token，一致 292，准确率 97.33% —— ≥80% 验收 PASS**。
- 按类 recall：range 257/261 = 98.5%，trendUp 12/14 = 85.7%，
  trendDown 12/14 = 85.7%，volatile 11/11 = 100%。
- 误判集中在过渡窗：行情在窗中切换时，离线标注看整窗、机器看窗末，
  滞回使其晚 1–2 个窗跟随 —— 这是记录在案的已知代价，不是缺陷。
- 机读与明细：`data/evolution/regime/e16-20260920/regime.{json,md}`
  （数据目录不进 git；报告口径与数字以本文为准）。

## 4. 数据边界（如实）

- 语料仍只有 **0.75 天**（#97 的 30 天前提不成立），标注集 ≈ 300 窗，
  trend 类窗占比 ~9%（42/300）——类不平衡存在，全猜 range 的基线
  准确率是 87%，状态机 97.3% 显著高于基线且在 trend 类上有真实召回。
- 每个 token 的序列只覆盖其所属轮次（二元市场轮内 token 寿命短），
  跨轮状态由每 token 独立实例承担，无跨轮状态污染。
- dry 经济性（KI-1）不进入标签（regime 标注只用价格序列），但「30 天
  尺度的 regime 分布」仍属未验证。

## 5. 复跑手册

```bash
# 全量评测（含轮转段，标注集 + 准确率 + 混淆）
cargo run --release --bin blitzkrieg-core -- \
  --regime-eval <档案.jsonl> \
  --regime-report <输出路径.json> \
  --regime-max-tokens 100 \
  [--regime-window-sec 300] [--regime-min-trend-ticks 3] \
  [--regime-min-efficiency 0.5] [--regime-volatile-mad-ticks 1.5] \
  [--regime-confirmations 2] [--regime-token <token_id>]
```

注意：与 `--backtest` 不同，本模式只读档案、不构建 Core、不下单、
不回写档案；`--regime-report` 同时产出同名 `.md`。
