# RFC-645: 策略身份泛化——名字分流与 `on_config` 具体类型的退役路线

> Refs #645（微内核审计 MD-4 / 策略面 D-3，P1）。审计报告：bk-session/audit-strategy-ui-face.md §D-3。
> 评审态。**决议前禁止产出触碰 `EngineStrategy` trait 签名 / `EngineConfig` 字段 / ARB_STRATEGY 分流点的实现 PR**（照 #541 门禁先例）。本文是决策文档，只给裁决项，不给实现。

## 一、问题

两处具体策略知识泄漏进内核，违反铁律一「core 零市场/策略代码」的精神：

1. **名字字符串特权路径**。`pub(crate) const ARB_STRATEGY: &str = "cross_venue_arb"`（service.rs:16683），三个消费点：
   - service.rs:8229 —— `strategy_stats()` 里名字相等分支：只有 `cross_venue_arb` 的行携带 `arbNetPnlUsd` / `arbWins` / `arbLosses` / `arbClosedTrades` / `arbActivePairs`（后者经 service.rs:8235 读 `pair_book.active_pairs_count(ARB_STRATEGY)`，pair.rs:518）；
   - service.rs:8260 —— `strategy_stats_for_account()` 的 `arb_totals` 分支：全局行携带的套利合计字段被抢救下来随每个账户切片转发。
   后果：第二个跨所策略永远无法获得同级统计通路；统计归属判定由「名字碰巧相等」而非「策略声明了 pair 能力」驱动。

2. **trait 面携带具体配置类型**。`EngineConfig { trend: TrendConfig, spread_arb: SpreadArbConfig }`（engine.rs:49-52，类型定义在 user_layer/strategy_logic/src/signal.rs:124/:307）经 `Engine::set_config`（engine.rs:928-933，对每个在册策略派发）与注册期「出生投喂」（engine.rs:2660-2662）进入 trait 方法 `fn on_config(&mut self, &TrendConfig, &SpreadArbConfig)`（strategies/mod.rs:493）。任何 `EngineStrategy` 实现者被迫看见两个与己无关的内建策略配置类型——包括用户 dylib 作者经 foreign.rs:996 的 marshal 层。内核 `EngineStrategy` trait 本身其余方法（intents-only、wants_trailing_exit/atomic_batch/entry_pair 声明式）是干净的；`on_config` 是唯一的类型级泄漏。

**与 #654 互为因果**：#654 把 on_kline/entry_pair/trailing_exit/atomic_batch 四项能力拉平成声明式可选符号，但统计面仍按名字倾斜（本条第 1 点）、配置面仍是类型化广播（本条第 2 点）——能力面拉平了，身份面没拉平。第三策略拿了四项能力，仍拿不到 `arb*` 统计字段、仍被迫编译期看见 TrendConfig。

**对照事实（本 RFC 的支点）**：C ABI 的 `on_config` **已经**是不透明 JSON——`on_config: Option<unsafe extern "C" fn(handle, json: *const c_char) -> i32>`（user_layer/strategy_api/src/lib.rs:487），且 foreign.rs:997-1027 的 marshal 层正是把 `&TrendConfig, &SpreadArbConfig` 现场拼成 JSON 才过墙。**trait 面反而比它要面向的 ABI 面更不抽象**。审计原文（§D-3）：「trait 面向 C ABI 的既有形态看齐即可」。

锚点行号以本 worktree（ceer/main @ c401d2b）为准。

## 二、目标 / 非目标

**目标**：
- G1 内核对策略集合无知：统计分流（`arb*` 字段 + pair 计数）改由策略**声明**驱动——「我消费 pair_book 统计」是一条声明能力，谁声明谁获得，内核不再点名。
- G2 `EngineStrategy::on_config` 泛化为不透明 JSON（`&serde_json::Value` 或等价借用形态），`EngineConfig` 不再携带具体策略配置类型；`TrendConfig` / `SpreadArbConfig` 的序列化形态不变。

**非目标**：
- 不改任何策略行为——trend_follow / spread_arb / cross_venue_arb 的逐位语义不动（见 §五验收口径）。
- 不改 wire——IPC schema、`arb.config.get/set`、`strategy.*` 方法面一个字节不动。
- 不动 #654 刚落的能力面——四个可选符号、vtable 15 槽、`BK_ABI_VERSION=2` 全部冻结。
- 不动 Lua manifest 键集（atomic_batch/trailing_exit/pair 保留键封印只增不改的既有裁决维持）。

## 三、方案

| 维度 | A. 声明驱动（**推荐**） | B. 内核配置注册表 | C. 维持现状 + 文档记档 |
|---|---|---|---|
| 统计分流 | manifest/声明加「我消费 pair_book 统计」键，内核按声明分发 | 内核内建一张 `strategy_name → 统计特权` 注册表 | 不改，INTERFACES.md 记档「跨所套利是内核特权的策略」 |
| on_config | 签名改 `&serde_json::Value`（C ABI lib.rs:487 对齐） | 不动 trait，配置类型登记进注册表 | 不动 |
| 铁律一 | **恢复**：内核零策略知识 | 名字集中进一张表，知识仍在内核 | 显式放弃 |
| 第三策略获得同级通路 | 声明即可 | 仍须改内核表（「集中」而非「消除」） | 永不 |
| 迁移成本 | 中（trait 签名变更 + 3 处分支 + marshal 层简化） | 低-中 | 零 |
| 行为风险 | 低（见 §五；marshal 层已存在，A 只是删掉墙上多余的一层） | 低但留下永久第二真相信源 | 零，但 #645 只能关闭为 wontfix |

**推荐 A**，理由一句话：**两个泄漏点的抽象目标形态都已经存在于代码里**——C ABI 的 `on_config(json)`（lib.rs:487）证明不透明配置是可行终态，且 foreign.rs 的 marshal 层恰好就是 A 要内联的转换（迁移 = 把 JSON 拼装从 ForeignStrategy 下沉进内核配置出口， ForeignStrategy 反而变薄）；E2-b 的 `strategy_gate_exemptions`（service.rs:8160-8163 旁的 `declared` 读取）证明「按声明读能力、不点名」在统计面已有同构先例。B 只是把散落的知识集中成一张表，内核仍点名、第三策略仍需改内核——不满足 G1；C 等于裁定「跨所套利被刻意特权化」，那是审计 §D-3 反例条款里需要与 RFC #541 平行的裁决才能走的方向，不应由「不改」默认达成。

**A 的两个设计点（供裁决钉死）**：

- **A-1 统计声明键**。声明面挂在哪？候选：(a) Rust trait 方法（如 `consumes_pair_stats(&self) -> bool`，对齐 wants_trailing_exit/atomic_batch 的三态声明先例，mod.rs:400/:415）；(b) Lua manifest 键 + cdylib 可选符号，与 #654 同型。推荐 (a)：消费 pair_book 统计的前提是策略真的产出 pair（声明 atomic_batch + 经 submit_pair 产出 PairBook 条目），Rust trait 内建策略与 ForeignStrategy 都在同一 trait 面上作答，不需要新 ABI 符号——**本 RFC 不碰 vtable**；Lua 侧 cross_venue_arb 的归属经 loader 适配器回答，同样不新增 manifest 键（若决议要求 Lua 包也可声明，走 #654 同型可选符号另开小票）。
- **A-2 `on_config` 的 JSON 契约**。`EngineConfig` 瘦身为 `scanner` + 中性运行时旋钮，`trend`/`spread_arb` 字段删除；`set_config` 派发点（engine.rs:931）与出生投喂点（engine.rs:2662）改为把各策略**自己的**配置投影序列化为 JSON 再递 trait。`TrendConfig`/`SpreadArbConfig` 序列化形态 = foreign.rs:998-1026 现有 camelCase JSON 逐字段不变（`confirmSec`/`minPrice`/... 含 decimal-STRING 拼写）——这正是 dylib 侧今天的既成契约，A 使 trait 面向它看齐，零 wire 变化。内建策略在 trait 实现内反序列化回自己的类型，反序列化失败 = 配置拒绝路径（对齐 C ABI 的 non-zero reject 语义，advisory 不炸策略，foreign.rs:1027-1030 现状）。

## 四、迁移序

| 步 | 内容 | 触碰文件 | 测试 |
|---|---|---|---|
| M1 | A-1 声明面落地：trait 加统计消费声明（默认 false），cross_venue_arb 的宿主适配声明 true；service.rs 三处分支改读声明（名字分支**保留**，命中声明与命中名字等价） | strategies/mod.rs（trait + 默认实现）、engine.rs（声明查询透出）、service.rs:8229/8235/8260、pair.rs 不动 | 既有 `arb_totals_ride_every_account_slice_of_the_arb_row_only`（service.rs:24431）、strategy_stats 相关断言（service.rs:24378/:24510/:24534）必须逐位绿；新增：声明 true 的第二策略获得同字段、声明 false 不携带 |
| M2 | A-2 配置 JSON 化：trait 签名改 `&serde_json::Value`；EngineConfig 字段删除；内建策略（test_support.rs:178、twin 路径）与 ForeignStrategy 适配；marshal 层下沉，ForeignStrategy::on_config 变直通 | engine.rs:49-52/:928-933/:2660-2662、strategies/mod.rs:493、strategies/foreign.rs:996-1030、strategies/test_support.rs:178、strategy_engine/lua_loader.rs（Lua 适配器 on_config 为 no-op，mod.rs:493 默认实现覆盖，无行为变化） | 派发时机回归（见 §五）；`spread_arb_entry_overrides_flow_through_engine_config`（service.rs:17813）；shadow evolution 双胞胎行为等价套件全绿 |
| M3 | 删名字分支 + deprecation 撤除：M1 期名字分支带 `tracing::warn` deprecation 日志（对「名字碰巧相等但未声明」的策略提示改声明），M3 删 `ARB_STRATEGY` 常量（service.rs:16683）与残余名字比较 | service.rs:16683 及 8229/8235/8260 残余 | 全量回归 + `cross_venue_arb_strategy_check.rs` / `pair_book_check.rs` / `account_decouple_check.rs`（后者的 `"cross_venue_arb"` 字面量是测试自建实例命名，不依赖内核常量，不需改） |

M1 与 M2 可独立开 PR、独立回滚；M3 必须在 M1 稳定一个发布周期后。步骤间无循环依赖。

## 五、风险与反例

1. **热更新时序**。`Engine::set_config`（engine.rs:928-933）对每个在册策略先 `on_config` 再写 `self.cfg`，注册期出生投喂（engine.rs:2660-2662）保证后来者拿到当下配置——两个时机 M2 必须原样保持，只换载荷形态。风险：JSON 序列化在派发点新增一次分配/转换，若有人把派发点从「同步于 set_config」改成「下个 evaluate 周期懒推」，会出现配置切换与统计读数的一拍错位。钉死：**M2 的验收包含派发时机不变断言**（set_config 返回后立即查询策略可见配置必须已是新值，测试以 ProbeCount 型 stub 断言调用序，engine.rs:3237 同型）。
2. **行为等价验收口径**。「两策略行为逐位不变」的判定：(a) 配置投影 JSON 与 foreign.rs:998-1026 现有拼写逐字段相等（golden 断言，漂移即红）；(b) trend_follow / spread_arb 在相同 K 线/盘口序列下的 TradeSignal 序列 diff 为空（复用 parity 套件 foreign_parity.rs:185 的 InTreeParity 手法）；(c) shadow evolution 双胞胎（test_support.rs:33-56 的 twin 结构）计数器零漂移；(d) `arb*` 统计字段值在 M1 前后逐位相等（既有 service.rs:24431 测试承担）。
3. **声明冒领**：任意策略声明「消费 pair_book 统计」即可获得 `arb*` 字段。这是特性不是漏洞——字段内容按该策略自己的 accounting 键与 PairBook owner 过滤（pair.rs:518 已按 owner_strategy 过滤）计算，冒领者拿到的是自己键下的空/自身数据，不越权读他人。若操作员希望收紧为「须声明 atomic_batch + 产出过 pair 才生效」，在 M1 加一条受理校验即可（对齐 #568 决议钉 7 的受理权威位先例），实现成本一条 if，**留决议裁决**。
4. **撤回条件（照审计 §D-3 反例条款原文）**：若 pair_book/arb_totals 统计被裁定为「内核内建能力而非策略可声明能力」——即跨所套利被**刻意**特权化——则 A-1 撤回、本 RFC 降级为仅 A-2（on_config JSON 化）或整体关闭。该裁定需要与 RFC #541 平行的操作者裁决，不能由实现期默认达成。

## 请求裁决项

1. 选方案：A / B / C？（推荐 A，理由见 §三。）
2. A-1 声明面：Rust trait 方法（推荐，不碰 vtable）还是 #654 同型可选符号？
3. A-1 冒领收紧：是否加「须声明 atomic_batch 且 PairBook 有本策略条目」的受理校验？
4. A-2 JSON 契约：确认以 foreign.rs:998-1026 现有 camelCase 拼写为 golden 基线（wire 不变）？
5. M3 删常量的等待期：一个发布周期是否足够？

## 门禁条款（决议前）

- 禁止产出触碰 `EngineStrategy` trait 签名 / `EngineConfig` 字段 / service.rs ARB_STRATEGY 分流点 / foreign.rs marshal 层的实现 PR。
- 文档类 PR（INTERFACES.md 记档、SDK 指南标注）不受限，但不得预设方案 A 已被采纳。
