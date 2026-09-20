# STRATEGY_EVOLUTION — 数据驱动的策略进化方法论（E15 / #97）

> 状态：方法论 + 首轮实证报告。数据边界如实记录，不夸大结论。
> 本文档回答 #97 的五条验收，每一条都给出「做了什么 / 结果是什么 / 差距在哪」。

## 0. 一句话结论

walk-forward 参数扫与 A/B 对比已经**产品化**（可重复、可门禁），首轮实证在
现有冻结语料上完成；但现有语料只覆盖 **0.75 天**，#97 的「30 天影子数据」
前提**不成立**——因此本轮结果只能证明方法有效，不能宣称是 30 天尺度的验证。

## 1. 30 天影子数据导出（验收 1）——工具完成，前提未达

工具：`scripts/shadow-export.mjs`。对全部影子数据源（冻结事件档案、
near-miss、成交/订单/持仓台账）做一次流式扫描，产出 `manifest.json` +
`manifest.md`：每个文件的字节数、事件数、首末时间戳、内容 sha256，以及
按 UTC 日的直方图和**总覆盖天数**。覆盖不足时 manifest 直接写明
「30 天前提 NOT MET」。

首轮实测（2026-09-20 运行）：

| 来源 | 事件数 | 覆盖（UTC） |
|:---|---:|:---|
| event-archive ×3 | 3,561,976 | 2026-09-19T06:29Z → 2026-09-20T00:27Z |
| shadow-near-miss | 336 | —（无 `at` 时间戳） |
| trade/order/position log | 249 | — |

**结论：覆盖 0.7487 天 ≈ 17.5 小时，30 天前提不成立。**
事件档案按 256 MiB 轮转、总量有上限，早期语料已被挤出；2026-09-17..19 调参
时的 9.7M 事件语料如今只剩 3.56M。要把前提补上，需要：调大
`--event-archive-max-mb`（或外接归档），让档案自然积累 30 天，然后重跑本
文档全流程。`--copy` 可将语料物化为独立导出目录（本次未复制，manifest 为准）。

另一个如实记录的前提缺口：影子数据来自 **dry 模式**，而 dry 的成交基准受
KI-1 / #15 影响（挂单永不成交 ⇒ 入场全 taker），#97 自己已标注该依赖。dry
经济性在 KI-1 修复前是失真的，本报告所有金额类指标都在此前提下阅读。

## 2. Walk-forward 参数扫（验收 2）——已产品化

工具：`scripts/walk-forward-sweep.mjs`。

- 输入：冻结档案（glob）+ 可重复的 `--param key=v1,v2`（笛卡尔网格）。
  参数键→CLI 旗标映射表是唯一耦合面（`trend_entry_factor` →
  `--spread-arb-entry-factor` 等），全部走标准 config 链
  （`CoreConfig → engine_config`，回测与实盘同一映射），没有任何专用旁路。
- 折段：档案单趟流式切成等**事件数**折段（`--folds N`）。
- 协议：rolling walk-forward——折 i 为选择窗（按 `--select` 指标选最优
  候选），折 i+1 为该选择的验证窗；窗口不相交，选择只有在没见过的数据上
  再赢才算泛化。
- 每个候选 × 每折都完整回放，报告里是**完整矩阵**，任何选择规则都能离线
  重算，不需要再扫一遍。
- 可断点续扫（已有报告的 run 跳过），产出 `walk-forward.json` + `.md`。

内核侧新增（同 PR）：spread_arb 的入场旋钮全部 CLI 化
（`--spread-arb-entry-factor / --spread-arb-min-obi / --spread-arb-max-spread-pct
/ --spread-arb-dip-max-pct / --spread-arb-bounce-min-pct /
--spread-arb-bounce-window-sec`），默认 `None` = 出厂默认，逐字段映射测试钉住
（`spread_arb_entry_overrides_flow_through_engine_config`）。dylib 收到的
`spreadArb` 配置包同步补齐这五个键（`foreign.rs on_config` +
`spread_arb_strategy.rs on_params`）。

## 3. 数据驱动的策略重定义（验收 3）——证据链在案

本节不是拍脑袋：现行 `trend_entry_factor = 0.88` 就是上一轮数据驱动调参的
产出（#142/#143，2026-09-17..19 dry 语料 9.7M 事件、冻结回放、两半 holdout
验证：胜率 74.7%/71.8%、盈亏比 1.38，对比 0.98 基线 45%/1.18），调参注释就
写在 `signal.rs` 的 Default 里。四个入场过滤器（OBI / 点差 / 深回落 / 反转
确认）以数据机制为依据设计，但**默认全关**——先让默认行为逐字节等于改动前，
再由本工具链用数据决定是否开启。

本轮 walk-forward 的候选即围绕这两类旋钮（入场折价 × 深回落过滤器），见 §4。

## 4. A/B 对照（验收 4）——工具化，判定标准「≥2 项指标更优」

工具：`scripts/strategy-ab-compare.mjs`。输入两份 `--backtest-report`，
对胜率 / 盈亏比 / 盈利因子 / 净PnL / 最大回撤 / 费用逐项判定
（更优/更差/持平），**候选在 ≥ `--min-better`（默认 2）项上严格更优才算
pass**，退出码 0/1/2 可直接接门禁。两份报告必须来自同一档案（工具回显
`events` 供核对）。

## 5. 首轮实证（2026-09-20，冻结语料 3.56M 事件 / 0.75 天）

扫描：5 折 rolling，候选 = {factor 0.98, 0.88} × {dip_max 0, 4}，选择指标 =
净PnL。完整矩阵与折段选择见
`data/evolution/sweeps/e15-20260920/walk-forward.{json,md}`
（文件内含每折每候选的 closed/WR/payoff/PF/net）。要点：
**出厂配置（factor 0.88、过滤全关）在全部 4 个选择窗胜出且全部验证窗方向
一致**；dip_max=4 过滤过狠（把好入场也滤掉）；折 3（17:21→20:46Z）对所有
候选都是亏损折——语料本身有一段全员亏损行情，属行情属性而非参数缺陷。
汇总口径：0.88/dip0 累计 +$75.06 / 148 平仓 vs 0.98/dip0 −$0.23 / 106。

全档 A/B 复盘（同语料 3,593,457 事件、同时间跨度，`ab/` 目录）：

| 指标 | baseline 0.98 | shipped 0.88 | 判定 |
|---|---|---|---|
| 胜率 % | 34.58 | 47.70 | 更优 |
| 盈亏比 | 1.319 | 1.364 | 更优 |
| 盈利因子 | 0.697 | 1.244 | 更优 |
| 净PnL USD | −25.69 | +18.20 | 更优 |
| 最大回撤 USD | 32.62 | 8.88 | 更优 |
| 费用 USD | 18.59 | 24.20 | 更差 |

判定：**PASS——候选在 5 项指标上更优（≥2 要求）**，
`ab/ab-compare.{json,md}` 为机读证据。唯一更差项是费用（成交更多，
费用自然更高，属「次数换胜率」的预期代价）。

读数纪律不变：本轮**不据此升级任何参数**。0.75 天语料 + dry 经济性失真
（KI-1）两个前提缺口之下，这套数字只值得作为下一轮 30 天扫描的候选输入，
不值得作为上线动作。这正是流程要的形态：数据先说话，人再拍板（或勾选
自动进化由 E13 提案工作流兜底）。

## 6. 复跑手册

```bash
# 1) 覆盖清单（30 天前提核查）
node scripts/shadow-export.mjs

# 2) walk-forward 扫描（30 天档案就绪后重跑）
node scripts/walk-forward-sweep.mjs \
  --archive 'data/archive/*.jsonl' --folds 5 \
  --param trend_entry_factor=0.98,0.88 --param entry_dip_max_pct=0,4 \
  --out data/evolution/sweeps/<日期>

# 3) 全档 A/B 复盘：两份报告必须来自同一语料（合并或单档均可）
cargo run --release --bin blitzkrieg-core -- --backtest <档案.jsonl> \
  --backtest-report <旧配置报告.json> [旧配置的 CLI 旋钮]
cargo run --release --bin blitzkrieg-core -- --backtest <档案.jsonl> \
  --backtest-report <新配置报告.json> [新配置的 CLI 旋钮]

# 4) A/B 判定（≥2 项更优；退出码可直接接门禁）
node scripts/strategy-ab-compare.mjs \
  --baseline <旧报告.json> --candidate <新报告.json> --out <目录>

# 5) DryRun 周报（E16，按策略独立账本）
node scripts/dryrun-report.mjs --days 7 --out data/evolution/dryrun/<日期>
```

## 7. 与 E13 提案工作流的衔接

E15 的产物（扫描报告、A/B 判定）是 E13 提案的**证据输入**：由数据支撑的
变异才有资格成为 `shadow_evolution` 提案，经人工决策或自动进化采纳——
这条链路已在 #95 / E13 交付（提案状态机、72h 深度周期、promotions.jsonl
回滚锚点）。E15 不自动改参数；E13 提供带审计的采纳通道。
