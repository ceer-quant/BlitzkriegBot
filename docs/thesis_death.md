# 论点死亡（thesis_death_exit）— 定义模板与运行时契约

Issue #961（Exit-First 基石，运营者哲学 2026-11-06）：入场方向近似抛硬币，正期望由
出场机制决定。每个 Lua 策略包必须声明「我的入场论点在什么条件下已死亡」；命中后，
**运行时**（不是策略自己的 exits 表）在**同一 tick** 注入无条件全量 taker 平仓，
`exit_reason = thesis_death_exit`，禁止扛单。

本文是「论点死亡」的**定义模板**：新策略包接入时必须按下表逐项填写，缺省视为不合规
（`scripts/thesis-death-check.mjs` 在 CI 默认检查）。存量包经 #961 收敛后要么已声明、
要么在 gate 的豁免表里带一句书面理由（不再是裸 grandfather），但任何新包（或一次改变
入场论点的改版）都必须声明。

---

## 0. 按包关闭（#1003，实测撤销声明的正规出口）

#1003（P1，2026-11-08）实测证明：一个已声明的包，其论点形状可能被 stdlib 默认判据
**误杀**——默认判据按**单腿自身 bid 相对入场价的回撤**判死（§3 条件 2），这只对
「论点 = 该腿自身方向路径」的包成立。对论点是 **spread**、**赔率带**或**对冲对形态**
的包，带内逆向波动是入场定价本来要扛的方差，强制同 tick taker 平仓把会自然恢复的
仓位 realize 成损失。

按包关闭的正规程序（不是裸 grandfather，也不是全局删钩子——「每个新 Lua 包必须有
thesis_death」仍是硬约束）：

1. **先测后关**：用 `scripts/ab-thesis-death-1003.mjs`（death 臂 = 出厂包，
   nodeath 臂 = 仅关死亡判据的 staged 副本，唯一差异 = 判据开关）在同语料同资金
   下出数。关闭线 = **death 臂净利 < nodeath 臂净利**（净利不劣于基线是 #961 的
   验收锚，被证伪即关闭）。
2. **写书面理由**：`scripts/thesis-death-check.mjs` 的 `EXEMPT` 表加一项
   （误杀率、两臂净利差、为什么默认判据与该包论点矛盾），包内 `strategy.lua`
   的声明块改成撤销注释，manifest `description` 同步（版本 +1，sha256 重签）。
3. **撤干净 tunables**：包 manifest 里不再需要的 `thesis_death_*` tunables 一并
   删除（`--backtest-knob` 拒绝未声明旋钮，残留 = 幻旋钮）。
4. **改入场论点须重新声明**：豁免只对「当前论点形状」成立。

已关闭的包：`spread_arb`（#1003：death 臂净利 −568.971 vs nodeath −495.867，
official 费档 Δ −$73.10；legacy 费档 Δ −$62.64 同向；误杀 309/764 = 40.45%）、
`single_leg_pair`（#1003：误杀 121/238 = 50.84%；official 费档下 death 臂不再
优于 nodeath，Δ −$0.07）。证据：`docs/reports/strategies/THESIS-DEATH-MISFIRE-1003.md`。

---

## 1. 声明形态（二选一，谓词优先）

### 形态 A：声明谓词（推荐，可表达策略自己的论点）

```lua
-- 论点死亡声明：命中返回 true，运行时当 tick 无条件全量 taker 平仓。
function bk_thesis_death(h)
    -- h 是一行 bk.holdings() 行：
    -- { strategy=, condition_id=, token_id=, direction=, shares=,
    --   entry_price=, round_slot= }（decimal 均为字符串）
    if <你的论点已死> then
        return true
    end
    -- 复用标准库默认（盘口反转带 + 字段消失/超时），叠加在自述条件之后：
    if bk.thesis_death_default(h) then
        return true
    end
    return false
end
```

### 形态 B：布尔开关（最小声明，直接用标准库默认）

```lua
BK_THESIS_DEATH_ENABLE = true
```

两种都未声明 = 该包没有论点死亡条件（#961 收敛后：存量包要么已声明、要么带理由豁免；
新包 CI 红）。

## 2. 「论点死亡」定义模板（新策略必填）

把下面四行填进包内注释/README，`scripts/thesis-death-check.mjs` 按第一行锚定扫描：

```text
THESIS DEATH DECLARATION (#961)
  entry thesis : <一句话：我为什么入场>
  death cond 1 : <逆向条件 1 —— 盘口瞬时反转：什么信号说明盘口已与论点背离>
  death cond 2 : <逆向条件 2 —— 论点依赖字段消失/超时：依赖什么字段，没了/多久算死>
  death cond 3 : <逆向条件 3 —— 轮次时间加权越界：随轮次衰减的界是什么>
  enforcement  : runtime-injected thesis_death_exit（不由本策略 exits 表执行）
```

- 三条逆向条件是**下限**：至少覆盖（1）盘口瞬时反转（bid/ask 与持仓方向背离超阈值）、
  （2）论点依赖字段消失/超时、（3）轮次时间加权越界。形态 B 的标准库默认实现恰好
  一一对应这三条（见 §3），直接引用即可；形态 A 允许用更贴近论点的条件**替换或收紧**
  某条，但必须在模板里写清替换后的定义。
- 阈值必须落在包的 manifest `tunables` 里（热参数可调），不允许写死在算式里。

## 3. 标准库默认实现（`bk.thesis_death_default(h)` → boolean）

宿主在 `bk.*` 面注册的默认谓词（lua_runtime），三条逆向条件的缺省实现：

| 条件 | 键 | 缺省 | 判定 |
|---|---|---|---|
| 1. 字段消失/超时 | `thesis_death_stale_ms` | `30000` | 持仓 token 无 book、两侧无价、或报价年龄 > 阈值 → 死（`0` 关闭超时，book/价格消失仍判死） |
| 2. 盘口瞬时反转 | `thesis_death_reversal_pct` | `25` | 持仓 token 自身买价较入场回撤 ≥ 界 → 死（多头/空头同式：持任何腿，其自身 bid 跌穿界即逆论点） |
| 3. 时间加权越界 | `thesis_death_reversal_pct_tight` | `0`（关） | 界从 base 随轮次线性收到 tight（与 lua_momentum `tighten()` 同一算法）；回撤 ≥ 收紧后界 → 死 |

失败方向：入场价损坏/为零 → 反转类条件判**活**（绝不在坏数据上强平）；
热参数解析失败 → 回落到缺省（拼写错误只会放松到出厂形状，不会关闭检查）。

## 4. 运行时强制语义（lua_runtime / 内核，#961）

1. 每个评估周期，运行时对**每个持仓**调用一次谓词（形态 A）或默认谓词（形态 B）；
2. 命中 → 运行时**替换**该 token 本周期策略自报的一切 exit 行，注入
   `{ token=, reason="thesis_death_exit", use_maker=false }` —— 全量 taker，
   内核按新鲜 best bid 定价、走常规风险闸；
3. `bk_evaluate` 报错不豁免扫描（坏的策略更需要出路）；单个持仓的谓词报错只跳过该行；
4. 内核侧：`thesis_death_exit` 标签 → `ExitReason::ThesisDeathExit`（台账标签，
   非内核风控）；该标签**永不**转 maker（`reason_is_profit_side` 拒之门外）——
   论点死亡是出场，不是挂单。
5. 入场信号族、内核 exit ladder、place_gated、live 配置一概不碰（#961 禁止项）。

## 5. 验收锚（可证伪）

- 单测：逆向条件 → 同 tick 平仓事件，`exit_reason=thesis_death_exit`，持仓归零无残留
  （lua_runtime `lua_strategy::tests`、内核 `exit_reason_table_tests`）；
- 回放：同段行情接入前/后对比，命中后不再出现加仓/续持 tick；净利不劣于基线
  （报告：`docs/reports/strategies/THESIS-DEATH-REPLAY-961.md`）；
- CI：`scripts/thesis-death-check.mjs`（--teeth 先证明能红，再跑真实判定）。
