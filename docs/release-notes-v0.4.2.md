# ⚡ BlitzkriegBot v0.4.2 — 巨量更新发布

> **0.4.1 → 0.4.2：239 个提交 · 111+ 个 PR · 510 个文件变更 · +64,150 / −18,512 行**
> 这是从「能跑」到「交易操作系统」的一次代际跨越。

---

## 🏛️ 一、微内核化与契约冻结 —— 0.4.2 epic 收官

**内核不再是巨石。** 契约冻结 commit `e7f8bbd2` 落地：

- 全新契约 crate `blitzkrieg-bk-contract`（leaf，依赖白名单 serde/serde_json/rust_decimal）承载全部 market contract——market_api 全 facade，孤儿副本 −755 行
- **22 族 wire golden 锁定 118 项 JSON-RPC 形状**，此后任何契约 diff 需要 `[CONTRACT-BREAKING]` 标题 + 下游全适配——AI 多 agent 协作时代的护栏
- core 停链 concrete plugins，组合根外移 launcher（bk_contract 换面 361 处）
- 统一 capability model：strategy_api / lua_manifest / core 同常量同枚举；依赖方向守卫 + lane-planned CI
- **UI 零内核依赖**：ui_kit schema 收敛契约（wire 形状迁 bk_contract，presentation 留守），core→UI dev-dep seam 拆除

## 💰 二、现金真相 —— 未知是一等状态

- **双账本绝对独立**（#941）：live/dry journal 拆双文件，dry→live 零继承（A/B/E 测试钉死）
- **pending receivable 裁决落地**（#991/#1002）：unverifiable-expiry 平仓的估计现金不再混入可支用 cash——单 flag `BK_UNVERIFIED_EXPIRY_POLICY=cash|receivable` 双完整模型，复用 settlement receivable 池，reconcile 时按场地真值 redemption + 双行校正；48h 冻结上限 + 琥珀横幅；day budget/连损断路器基数只算可支用现金
- venue-price 回填三态 `source=reconciled`：标签不再跑在现金前面；`positions.unverified_exposure` IPC（golden 族 23）
- 官方费率模型 `0.07·p(1−p)` 入账 + `fee_source` 双行校正模式

## 🎯 三、策略出场栈一代目

- `thesis_death_exit` 落内核 + Lua 标准库 `bk.thesis_death_default`——**禁止扛单**：逻辑死亡当 tick 无条件平仓
- 出场 wire 加 `price` 字段：策略报价 maker-TP（profit-side 门 + PnL 守门）
- 出场升级钟 250ms maker→taker + 同期出场优先于入场
- lua_momentum 0.4.0 时间加权止盈带（二项定价收敛 {0,1}，带子随轮钟线性收紧）
- **实证文化**：出场栈交付即见效——lua_momentum 0.4.0 时间加权止盈带（移动止盈止损）正是把策略做活的那一环；coin_flip 出场对照实验（#962）16 点网格全负、真数据 A/B 双判决（#964）23 窗 × 3 臂 × 双跑逐字节——框架如实给出负结果，证伪的是「掷硬币当出场」这一具体实验，不是出场哲学本身

## ⚙️ 四、config 单源 .env

- risk/exit 归一化 `BK_RISK_*`/`BK_EXIT_*`，TOML 层断链（#967/#968）
- 三个工厂 toml（accounts/execution_policy/update）退役
- 重启不再全面冻结；风控状态持久化（#861）

## 📊 五、回测坐标系制度

- mine 档钉 0 tick 滑点 + 报告头标注坐标系；`--backtest-knob` 按 tunable 声明类型解析
- l2-delay 命名预设（真实 L2+150ms 延迟回放）
- **A/B 出场对照实验框架**（ab-exit-964.mjs）：三臂预声明 + 双跑 sha256 确定性 + arm-starved 结构化归因 + vacuous 空跑不构成证据（#1008）——报告数字全部语料实算，硬编码清零

## 🚦 六、门禁代际升级

- **E24 声明式止损三层制**（操作员裁决）：none/declared/lab_only——止损必须显式声明（manifest `stop_loss_policy` + knob 白名单 + 裁决引用），硬编码止损数学任何层全红；标识符改名不再能翻转门禁（#1006/#1007）
- 文件体积棘轮扩 ui/ 与 webui；72h dry-run 限额+扣费证据门（#869）
- corpus verify 门去随机化（确定性等步长采样）

## 👥 七、多账户与新策略包

- account.strategies.set 解锁未注册策略 + 策略包热 reload RPC（#978）
- 冻结按账户可见：红色横幅 + 解锁卡 + 倒计时暂停（#943）
- 新包：maker_pair 双腿齐挂或全弃、postonly_pair pairing LOOP、whale_follow taker 包、smart_money_reverse 证据门反向包、pair_discount_arb v1.3
- polysim 写实：确认窗延迟成交 + maker/taker 角色判定 + 官方 taker 费率 + 平台流动性激励 REWARD 归账

## 🎨 八、品牌与体验

- 启动横幅品牌定名：**OME=Song / backtest=Stone / UI=PonyYiDan**（#971）
- webUI 移动端 + i18n 适配 + 溢出修复（#936/#973/#977）
- webUI/TUI parity wave 1；TradingOS 重定位文档

---

## 数字一览

| 指标 | 0.4.1 → 0.4.2 |
|---|---|
| 提交 | **239** |
| PR | **111+** |
| 文件变更 | 510（+64,150 / −18,512） |
| 内核 lib 测试 | 1315 → **1321** 全绿 |
| wire golden | 22 族 118 项（+ 新族 23 unverified exposure） |
| 策略包 | LEGACY 14 包收敛 + 5 新包 |
| 工作区 | 22 crates，依赖方向全程守卫 |

## 升级说明

- 默认行为兼容：`BK_UNVERIFIED_EXPIRY_POLICY` 默认 `cash`（现行为逐字节不变），拨 `receivable` 需重启
- 契约冻结生效：下游适配面全部 additive（`#[serde(default)]`）
- 升级后热参需重设（BK_RISK_*/BK_EXIT_* 已进 .env 单源）

**OME=Song · backtest=Stone · UI=PonyYiDan**
