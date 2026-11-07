# coin_flip_exit_lab — issue #962 Exit-First 实验台

⚠️ **LAB ONLY — 仅用于回放/研究，禁止进实盘默认配置。**
不要把它加进 `user_layer/configs/*`（默认配置、allowlist、生产 env 一概不加）。

## 哲学（issue #962）

入场方向由**固定随机种子抛硬币**决定，把「入场 alpha」剥离成零，单独度量
**出场机制的期望贡献**。同 seed 同语料两次回放逐字节一致（`trades_sha256` +
tradeLines 逐行对照），实验脚本用 JS 复算同一哈希交叉验证实际入场方向。

- 抛硬币：FNV-1a-64 + splitmix64 终混，纯函数 `coin_flip(seed, slot, condition_id)`
  → `h % 2` → UP/DOWN。无 `os.time`、无 `math.random` 状态、无调用顺序依赖。
  出场建议按 token 排序发射——Lua 5.4 字符串哈希种子随进程漂移，`pairs()` 顺序
  不钉死就会让平仓顺序（进而 `trades_sha256`）跨跑漂移。
- 轮次选择：窗口内**每个**入场窗被回放观察到的轮次市场（time_left ≥ 885 被
  见过，即转换器首个 top 行落在轮内 15 秒内）各抛一次、至多一次入场（taker
  风格：抬所选 token 自己的 best ask，manifest `entry_maker: false`，回放
  `--entry-maker-timeout-ms 1`）。
- 语料：`data/onchain/` 15m-act **单文件**（钱包 0x3725…08ad88 的活动语料，
  2026-08-05 → 2026-10-31，4,766 轮 / 9,780 轮次市场），先过两道语料闸再回放：
  `scripts/corpus/fix_resolutions.py --check`（8,034 结算行全 token 形式）+
  `scripts/corpus/relabel_official.py --apply-corpus`（CLOB 官方结算真值重写）。
  ⚠️ act 语料盘口是**合成盘口**（成交带重构，bid==ask，~60s 节奏，名义深度
  1.0）：入场必须 taker（1 股 sizing——深度 1.0 的盘 FOK 杀单更大单）；maker
  出场成交在本语料上偏乐观，其结论换生产 archive 复核后方可进生产。
- 写实结论档：每臂 `--fee-model official --slippage-ticks 1 --backtest-tick-ms
  50 --backtest-fast`——published taker 费率、每笔 taker 成交 1 tick 摩擦下限、
  实时节拍、deadline-jump 调度。0 成交=停下归因，禁读作无信号。

## 出场组合（被测对象）

1. `hard_stop_loss_pct` — 单笔最大亏损硬止损（net proceeds = 新鲜 best bid −
   `exit_fee_rate` taker 费模型；0 = 关）；
2. 时间加权紧缩止盈带 `take_profit_pct` → `take_profit_pct_tight`（与
   lua_momentum v0.4.0 同一 `tighten()` 线性算法）；
3. 移动止盈 `trail_arm_pct`（峰值净收益武装）+ `trail_giveback_pct`（回吐出场）；
4. `exit_time_left_sec` — 声明式截止线（默认 45s），保证所有仓位在窗口内了结；
5. 逻辑死亡离场（#961）：`bk_thesis_death(h)` 声明（Form A），委托
   `bk.thesis_death_default` stdlib 默认 —— 运行时当 tick 注入无条件全量
   taker 平仓 `thesis_death_exit`，替换本表一切出场。

对照组（固定呆板出场）= 同包同 seed 同语料，出场栈经 `--backtest-knob`
（#916 hot-param 反事实）全关（hard 0 / tp 0 / trail 0 / death 带 100,0 /
stale 0），只剩 `time_exit`。内核侧环境（holder 50% 容错止损 + 100% 回本
兜底）两臂一致，不构成处理差异。

## 论点死亡声明（#961 模板，新包必填）

```text
THESIS DEATH DECLARATION (#961)
  entry thesis : 无论点 —— 入场是 (seed, slot, condition) 上的抛硬币；持仓的
                 唯一收益主张是「随机入场后由出场机制收割」，不存在可被反转的
                 方向性前提。
  death cond 1 : 盘口瞬时反转 —— stdlib 默认：持仓 token 自身买价较入场回撤
                 ≥ thesis_death_reversal_pct（默认 25）。
  death cond 2 : 论点依赖字段消失/超时 —— stdlib 默认：持仓 token 无 book、
                 两侧无价、或报价年龄 > thesis_death_stale_ms（默认 30000；
                 0 关超时，book/价格消失仍判死）。
  death cond 3 : 轮次时间加权越界 —— stdlib 默认：反转带随轮次线性收到
                 thesis_death_reversal_pct_tight（0 = 平带，出厂缺省）。
  enforcement  : runtime-injected thesis_death_exit（不由本策略 exits 表执行）
```

抛硬币没有「对错」可言，所以本包 Form-A 谓词只在 stdlib 三条逆向条件上做委托
—— 再加任何策略自己的死亡条件，都等于把入场 alpha 从出场门里偷运回来。
