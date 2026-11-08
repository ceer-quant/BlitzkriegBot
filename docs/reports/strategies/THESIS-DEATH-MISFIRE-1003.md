# thesis_death 误杀 A/B 实测报告（issue #1003）

- 分支：`fix/issue-1003-thesis-death`（基于 main `4d4e6c86`，内核 0.4.1+g4d4e6c86d6f6）
- 复现：
  - 修复前：`node scripts/ab-thesis-death-1003.mjs --packages single_leg_pair,spread_arb --corpus <15m-act 语料> --out /tmp/bk-1003-before`
  - 修复后：同命令 `--out /tmp/bk-1003-after`（工作树已是修复后形态，death 臂 = 出厂包原样）
- 语料：`data/onchain/3725d52f3c252e8374999cc8617292ea2608ad88_1785888000_1790812799-15m-act-4bf06519abf5.jsonl`
  （manifest sha256 `2787348f07c3f265…`，事件 356,749 / 成交 254,840 / verdicts 38,603；
  `scripts/corpus/fix_resolutions.py --check` 8,034 ok / 0 bad）
- 资金面：seed-balance 1000 / max-order-notional 12 / round 900s / tick 50ms（与 exit-economics
  门、ab-exit-964 同一 spawn 形状）
- 费率档：内核默认（#995 的 official 曲线）。device 支持 `--fee-model`，未传 = 内核出厂。
- 判定装置：`scripts/ab-thesis-death-1003.mjs`（#1003 专用；death 臂 = 出厂包，
  nodeath 臂 = 仅关死亡判据的 staged 副本并重签 manifest sha256——旋钮语法关不掉
  book-gone 判死，所以 nodeath 臂必须是包副本；唯一差异 = 判据开关）。判定线 =
  **death 臂净利 ≥ nodeath 臂净利**（#961 验收锚「净利不劣于基线」的反向用法）；
  误杀 = death 臂 `ThesisDeathExit` 平仓的 token 在 nodeath 臂净利 > 0。
  独立复算先例：workbuddy #1333 W5（`verify/thesis-death-misfire-1333.mjs`，2026-11-07）。

## 修复前（声明形态 B 活跃，2026-11-08 实测）

| 包 | death 净利 | nodeath 净利 | Δ(death−nodeath) | 分母/误杀/误杀率 | unmatched | 判定 |
|---|---|---|---|---|---|---|
| spread_arb | −568.971 | −495.867 | **−73.103** | 764 / 309 / **40.45%** | 4 | **FAIL（death 更差）** |
| single_leg_pair | −6.735 | −6.665 | **−0.070** | 238 / 121 / **50.84%** | 54 | **FAIL（death 更差）** |

两包 death 臂平仓里 `ThesisDeathExit` 全部由 stdlib 默认判据（单腿自身 bid 回撤 ≥ 25%
带 + stale 30s）触发。spread_arb 的 Δ −$73.10 比 issue 原报（legacy 费档 −$62.64）更差，
方向一致；single_leg_pair 在 legacy 费档曾测得 +0.663（death 略优），official 费档下
反转为 −0.070。workbuddy W5 独立复算逐位一致（−568.970531 / −495.8674 /
−6.735246 / −6.664974；误杀率 40.45% / 50.84%）。

费档隔离对照（W5，#998 切 official 费后的费率归因）：same-corpus legacy 费重放给出
spread_arb Δ −$62.64 —— 两轮数字之差 100% 来自费率档，判据行为本身未变。

## 修复后（#1003 按包关闭：撤销两包声明，2026-11-08 实测）

| 包 | death 净利 | nodeath 净利 | Δ(death−nodeath) | death 臂 ThesisDeathExit 平仓 | 双跑确定性 | 判定 |
|---|---|---|---|---|---|---|
| spread_arb | −495.867 | −495.867 | **0** | **0** | match（报告 sha256 逐字节一致） | **PASS** |
| single_leg_pair | −6.665 | −6.665 | **0** | **0** | match | **PASS** |

- 修复后两包不再声明任何论点死亡（`scripts/thesis-death-check.mjs` EXEMPT 表 + 包内撤销
  注释），nodeath 臂 = 与 death 臂逐字节相同的出厂包副本（「arms identical by
  construction」），Δ ≡ 0 且 0 笔强制平仓——**被误杀的出场面已回到 nodeath 基线
  （−495.867 / −6.665，与修复前 nodeath 臂逐位一致）**。
- 单测面：`core/blitzkrieg_core/tests/legacy_thesis_convergence_check.rs`
  `exempted_packages_declare_nothing_and_force_no_exit`——豁免包在 33.3% 逆向书上也
  不触发任何强制平仓。
- 残留（预期内）：death 臂 report JSON 里 spread_arb 仍有 `taker_insufficient_depth`
  类 CoreError 行——那是内核 taker 深度拒单的正常回放噪音，两臂同现，不进判定。

## UNJUDGEABLE 包（同语料，不得发明判定）

| 包 | 判定 | 依据 |
|---|---|---|
| flash_arb | UNJUDGEABLE_ZERO_ORDERS | 两臂均 0 intent、0 平仓 |
| hot_side_momentum | UNJUDGEABLE_ZERO_ORDERS | 两臂均 0 intent、0 平仓 |
| mad_dog | UNJUDGEABLE_ZERO_ORDERS | 两臂均 0 intent、0 平仓 |
| whale_follow | UNJUDGEABLE_NO_DEATH_CLOSES | 两臂有 intent 但 death 臂 0 笔 ThesisDeathExit，分母为 0 |

这 4 包不在本 issue 处置范围内（0 分母不能证明或证伪判据），#961 契约原样保留。

## 为什么是按包关闭而不是调参

1. 判据错位：stdlib 默认条件 2（§3）按**单腿自身 bid** 对入场价回撤判死。spread_arb 的
   论点是 **spread**（宽赔率带入场，25% 单腿方差在定价内）；single_leg_pair 的论点是
   **赔率带 [0.55,0.88]**，包章约明确「止损不归你管」——裸腿生存权在内核（结算 /
   force-exit）。对这两个形状，判死 = 把会自然恢复的回撤 realize 成损失。
2. 调参已测过：#961 回放报告（`THESIS-DEATH-REPLAY-961.md`）的带参对照臂证明放松带宽
   收不回基线（lua_momentum trend-1000Z 放松到 20% 仍 −22.79 vs 基线 −17.49）；
   且 stale_ms=0 关不掉 book-gone 判死——旋钮不是关闭开关。
3. 误杀率 40.45% / 50.84%：death 臂的强制平仓里近半数 token 在 nodeath 臂是净利的。
   「净利不劣于基线」验收锚被证伪，按 issue #1003 处置 = **按包关闭**（非全局删钩子，
   「每个新 Lua 包必须有 thesis_death」硬约束不动；lua_momentum 行为不变）。

## 出场面（关闭后）

- spread_arb：manifest `trailing_exit: true`（#606）+ 内核 trailing/stop 阶梯 + 结算；
- single_leg_pair：结算 / 内核 force-exit；
- 两包均不引入策略侧止损（撤销注释里写明：改入场论点须按 docs/thesis_death.md §2
  模板重新声明）。

## 证据链

| 产物 | 路径 |
|---|---|
| 判定装置（带 --self-test，8 PASS） | `scripts/ab-thesis-death-1003.mjs` |
| 修复前原始 JSON/MD | `/tmp/bk-1003-before/ab-thesis-death-1003.{json,md}` |
| 修复后原始 JSON/MD | `/tmp/bk-1003-after/ab-thesis-death-1003.{json,md}` |
| 独立复算（workbuddy W5） | `/tmp/bk-misfire-1333/misfire-summary.json` |
| 门禁豁免表（书面理由） | `scripts/thesis-death-check.mjs` EXEMPT |
| 收敛测试 | `core/blitzkrieg_core/tests/legacy_thesis_convergence_check.rs` |
| 制度文档 | `docs/thesis_death.md` §0 |
