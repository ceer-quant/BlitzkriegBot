# 挖掘候选池入档（2026-11-05）—— issues #914/#915 数据面

**坐标声明（2026-11-02 运营者双轨裁决「挖掘宽、采用严」）**：本报告全部读数在 **mine 档**取得——成交 = identity（0 tick 滑点、0 延迟、touch 即成交、fill prob 100%，逐臂 out.json 的 `fillModel` 字段自证）、费 = legacy_quadratic（0.125/p(1-p) 平方，`feeSchedule` 字段自证）、tick 网格 50ms。**挖掘正读数不构成采用主张**；采用门（verify 1 tick / l2-delay 150+150ms / official 费 / #854 预注册 kill 线）全部原样，采用判决只属于 #854 门与运营者。

**入档方式**：本报告配套 `dig-candidate-pool-2026-11-05.json`（同日入档，`scripts/dig-matrix-pool.mjs --write` 产出）：63 臂挖掘池全量 JSON，字段语义对齐 `cost-adjusted-edge.mjs` digArm（n / evMicro / wrWilsonLb / pf），无任何 GO/NO-GO 字段。原始产物仍在仓库外 scratch：`/Volumes/Hard Disk/bk-tmp/dig-rescan/`（arms-summary.json、run/\<臂\>/out.json）。

语料窗口（两套）：

- **主**：replay-862 四资产 pm 切片 `corpus-{BTC,ETH,SOL,XRP}-pm.jsonl`（2026-10-27 14:00–16:00Z，各 8 轮 900s）——下称 **pm 窗**。
- **参考**：twap-859 全量（2026-10-27 13:55–16:05Z，含 Kalshi 行，仅作参考通道不作判据）——下称 **tw 窗**。

## 一、#914 四条子结论（如实入档）

### 1. maker_pair mode3（bid 报价）：唯一被两条独立证据链交叉互证的候选

挖掘读数：mp-m3 四资产 pm 窗全正 —— BTC +0.11（n=10，PF 1.0373）、ETH +0.08（n=8，PF 1.0377）、SOL +0.14（n=8，PF 1.0718）、XRP +0.16（n=8，PF 1.0829）；c0.985/c0.99 两档逐位相同（#912 已证同 sha）。tw 窗同向：BTC +0.18（n=12）、SOL/XRP/ETH 与 pm 同值。
source = 臂 `mp-m3-c0.985-{BTC,ETH,SOL,XRP}`、`mp-m3-c0.99-*`、`tw-mp-m3-c0.985/99-*`，pm/tw 两窗。
交叉互证链：#912 六臂冻结语料网格 PF 1.0675 同向（来源 #912 报告，与 11-02 冻结窗口独立）。m1/m2 只有 +0.02 微正且样本 2 笔（臂 `mp-m1-c0.985-ETH` 等，噪声级），不与 m3 并列。

**运营者提示：mode3 的下界与终判见下文第三条与 #914 评论。**

### 2. lum-both：identity 口径全场唯一大额正臂 → l2-delay+official 口径反向坍塌

- identity 读数：`lum-both-base-BTC` 净 +12.40、14 closed（7W/7L）、WR 50.0%、PF 1.370、maxDD 7.6 —— **pm 窗全场唯一大额正臂**。source = 臂 `lum-both-base-BTC`，pm 窗。
- 多资产复扫（11-03，挖掘坐标先用 geography 排除单窗伪影）：pm 窗 SOL +17.06（13 closed）、XRP +7.00（13）、ETH −7.10（12）如实报负，四资产合计 +29.36 / 52 closed；tw 通道 3/4 同向合计 +28.36。source = `/Volumes/Hard Disk/bk-tmp/lumboth-rescan/` results.jsonl，pm/tw 两窗。**正号跨资产 3/4、跨语料同向，但 ETH 是一致负腿。**
- **反向坍塌（11-04 冲刺轮，l2-delay+official 写实口径）**：3 个独立日历窗 × 4 资产共 12 臂，10 臂负，合计 **−27.99**，与 identity 口径 +28.36 相差 −56.35；W1/W2 双窗 BTC/SOL/XRP 全灭（W2 PF 0.39–0.73，XRP 单臂 −8.09），#914 的 +12.40/+17.06/+7.00 不再复现。source = `/Volumes/Hard Disk/bk-tmp/dash-1104/run/lumb-events.*` 各臂 out.json + 评论记录（2026-11-04），窗口 = 11-04 重切 W1/W2/W3 独立日历窗。
- 归义：identity 正读数的幅度主要由入场端乐观成交吸收（l2-delay 把入场滑点/延迟与官方结算费计入后即翻负）。两条读数**均已如实入档**，从中把「identity 通道有跨资产正号倾向」与「写实口径不成立」分开记录，不做二选一判决——判决归 #854 门与运营者。

### 3. mp-m3 PF=inf 是定价构造的产物，不是样本外强度

dig 池对 m3 的定价是 `pnl = 1.02 − (ask_up + ask_down)`（两腿各降 1 tick、零费）：只要两腿 ask 和 ≤ 1.02 就必不亏，因此冻结语料 dig Pool 的 PF=inf、maxDD=0、26W/0L 是**定价构造**（`pnl > 0 ⇒ 无亏损轮 ⇒ PF 无分母`），与「跨窗样本外强度」无关。可读信号只有 EV 跨窗正负与缩水：pm 基线 +0.007656（n=32）→ 10-28 窗 +0.008947（n=19）→ 10-30 窗 +0.010000（仅 BTC n=8），仍 >0 且未缩水，但每窗 8–19 轮，**统计上是弱证据，只够支持「值得进采用档去证伪」，不够支持任何资金主张**。且同一 m3 在 11-04 l2-delay+official 口径下多窗合计 −0.7268（BTC 臂裸腿 3 笔贡献 −1.24，显示配对腿成交不可保证的尾部已兑现）。
source = `dig-pool-corpora.json`、`/Volumes/Hard Disk/bk-tmp/wider-windows/`（mp-m3-windows.json / new-windows.json）、`/Volumes/Hard Disk/bk-tmp/dash-1104/run/mp3-events.*`，各注其窗。

### 4. 负边际定谳：挖掘档也救不回的臂

- **lua_momentum down 系全灭**：down-base 四资产 pm 窗 −2.0 ~ −13.6（BTC −13.6、ETH −13.4、SOL −8.5、XRP −2.0，PF 0.47–0.89），down-tp5n −8.03（BTC，PF 0.235，WR 75% 但 2 笔结算大亏吞掉 6 笔赢利）；tw 窗同构全负。identity = 0 成本 0 延迟 + touch 成交的乐观上界都救不回来 → **信号本身无 edge，与 #911 的 0ms 定谳一致（执行模型不是缺口）**。source = 臂 `lum-down-base-*`、`lum-down-tp5n-*`、`tw-lum-down-*`，pm/tw 两窗。
- **内核梯子惰性独立复现**：`lum-down-tp5kernel-BTC`（内核 exit-take-profit 5%）净利与 down-base **逐位相同**（−13.6）→ 梯子对 holds_to_settlement 策略不触发，#911「诚实 TP 通道只有策略信号级」在独立矩阵再证。source = 臂 `lum-down-tp5kernel-BTC` vs `lum-down-base-BTC`，pm 窗。
- **mad_dog 在 pm 窗饿死**：切片 spotEvents=0，block 四臂零成交（pf=na）、pass 小负（BTC −0.76、XRP −0.49）；tw 窗 pass 同负。source = 臂 `md-block-*`、`md-pass-*`、`tw-md-pass-*`，pm/tw 两窗。

## 二、#915 建设性语义：出场合同扩展的候选依据（数据面如实入档）

**判决原文（引用 #915 报告净裁决）**：「止盈挂单化=可行且不劣（赢家打平+rebate 微优+对信号失明路径纯增益），值得进内核 exit 合同扩展的候选票；止损挂单化=对二项崩腿无效（0/2），不应替代 taker 止损，二者是互补不是互替。」

- **maker-TP 真波动依据 = 赢家打平 + rebate 微优**。V3 对照八笔中 6 笔赢家的 taker-TP 触发条件是 `bid − fee ≥ 1.05×entry`，即 bid 必然**先**穿过 1.05×entry——这部分是**真实 bid 路径波动**（不是构造），若当时挂的是 post-only 卖单则 6/6 全部成交。逐笔对照：maker = 0.05×entry×10 股零费 + rebate ≈ $0.29–0.30/笔；V3 实得 $0.225–0.341（10 股）；赢家侧二者在 ±2¢/笔内打平，rebate（0.20×费等效 ≈ $0.02/笔）后 maker 微优。taker 略占优的部分 = 信号触发后 bid 继续爬 1–3 tick，恰好抵掉 taker 费。source = `/Volumes/Hard Disk/bk-tmp/momentum-maker-tp/` out-v3-tp5n-btc.json（tradesSha256 bf3bf6e9…f8917 与 #911 参考报告逐位一致）+ corpus-BTC-pm.jsonl pm 窗语料 bid 路径（4738 行 books 逐行重建）。
- **挂单止损 0/2 对崩腿无效**。两笔 Settlement 输家：hft-2（e=0.54）DOWN 腿 bid 从 0.97 崩落，挂单买单被穿价成交（逆选入场）@0.54，之后路径单调崩至 0.02，过 −12% 位（0.475）后**零反弹**，内核保护止损三次「SUPPRESSED (no executable bid)」，结算 $0；hft-6（e=0.51）0.49→0.001 单调崩，过 0.449 零反弹，结算 $0。**挂单止损反弹成交率 0/2**。机械原因（引原文）：「post-only 卖单只能挂在 bid 上方、只在价格朝它回走时成交——二项市场的崩腿不给这个机会。taker 止损的 taker 费就是保险费，而本窗的崩溃是『连保险窗口都没有』（bid=0.01 时想卖都卖不掉）」。source = 同上语料 bid 路径，pm 窗。
- **建设性落点**：Lua 合同没有挂单卖出通道（出场 = 信号制 exits），字面 maker-TP 需要内核 exit 合同扩展（加 price/post_only 字段，触及 #840 契约冻结面）——数据面支持「进采用档的候选票」，**本票不动内核**。开放项 = 「入场后先冲高再崩」路径（6 赢家 bid 回落幅度未检验）。

## 三、诚实边界

单窗口小样本（pm 窗每资产仅 8–14 closed）；合成执行 identity/touch 是乐观上界；结算前撤单机制未建模；PF/WR 在二元市场有结构性语义（完整 pair 必一赢一输）；**全部数字不得离开挖掘坐标引用**。l2-delay 侧读数（−27.99 / −0.7268）已一并入档以锚定下界，但其口径下面同样无判决权。

## 四、产物清单

| 产物 | 位置 | 说明 |
|---|---|---|
| 63 臂挖掘池 JSON | `docs/reports/dig-candidate-pool-2026-11-05.json` | 本票入档，sourceSha `2487420d…:rows-fingerprint-over-63-shards` |
| 归档脚本 | `scripts/dig-matrix-pool.mjs` | `--input`/`--write`，字段对齐 digArm，缺数据 fail-closed |
| 逐臂原始产物 | 仓库外 `/Volumes/Hard Disk/bk-tmp/dig-rescan/` | arms-summary.json、run/\<臂\>/out.json、matrix-exits.txt |
| lum-both 复扫 | 仓库外 `/Volumes/Hard Disk/bk-tmp/lumboth-rescan/` | 11-03 多资产 12 臂 |
| mp-m3 多窗 | 仓库外 `/Volumes/Hard Disk/bk-tmp/wider-windows/` | ①–④ 产物 sha 见 #914 评论 |
| l2-delay 冲刺轮 | 仓库外 `/Volumes/Hard Disk/bk-tmp/dash-1104/` | 11-04 写实口径 12+8 臂 |
