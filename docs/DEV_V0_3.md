# BlitzkriegBot v0.3.0 开发设计规范与实施蓝图（DEV_V0_3.md）

> **状态**：接口冻结稿（Interfaces Frozen）——冻结范围见 §14.0（两个契约文件 + 五个类型模块）
> **基线**：`feat/trading-safety-selfcheck` @ `bf4871a0`（0.2.1；0.2.0 已完成部分成交、结算到账、Maker 侧实盘验证）
> **目标版本**：v0.3.0
> **一句话**：内核是规则与裁决，扩展对接具体市场，策略只写赚钱逻辑；风控不可绕过，接口先冻结再实现。

**文档纪律**（本文所有断言都按此写）：

1. **只写代码里能验证的事**。本文出现的每个路径、符号、默认值都在基线 HEAD 上核对过；与代码不符的旧说法在 §A.1 逐条作废，不静默继承。
2. **不复述代码**。类型定义只写 v0.3 的**增量**与**变更**，既有类型以文件+行号引用。
3. **默认值只引用，不重写**。所有既有阈值以「引用既有常量」的形式出现，文中的数字是**读数**，不是新决策。

---

## 1. 版本目标

### 1.1 六个交付目标

| # | 目标 | 判据（可执行） |
|:--|:---|:---|
| G1 | **正式交付 BlitzkriegStrategy API 1.0**（Rust + Lua 双栈） | `ABI_V2_DESIGN.md` 冻结头落地；`bk_strategy_declare_modes` 可选符号可解析；**0.2 构建的 dylib 二进制不重编译仍可 `strategy.load`** |
| G2 | **策略建议 / 内核裁决分离** | 每条 intent 必产生 `Decision` 且落 `data/audit/intents.jsonl`；无裁决记录的入场在门禁中判定为缺陷 |
| G3 | **系统级风控不可绕过** | 账户级 5 项 + 全局级 4 项限额生效；新增项默认 **0 = 关闭**（不改变出厂行为）；`apply_physics` 绑定随单落审计 |
| G4 | **策略免止损** | `entries/exits` 载荷拒绝任何止损键；反向测试证明「带止损键的旧 payload 不产生任何止损动作」 |
| G5 | **多市场、多账户、三层声明** | `account.list`/`strategy.list` 可读三层声明与兼容性；`account_id` 贯穿订单/持仓/账本/行情；跨账户平仓被校验层拒绝 |
| G6 | **K 线兼容** | `KlineAggregator` 与离线基准逐字段一致；`on_kline` 回调、`KLINE_UPDATE` 事件、面板图表三处齐全 |

**不推迟**：六个目标全部落在 v0.3.0 内，不设 v0.3.1 补充分支。

### 1.2 明确不做的事（范围边界，详见 §17）

不改 `exit_policy` / `position` / `sim` / `ome` 的既有行为；不动 `scripts/lib/frozen-corpus.mjs` 的 sha256 与 `scripts/exit-economics-check.mjs` 的 `BASELINE`；不动任何既有风控默认值；不新增除 `mlua` 以外的依赖树；不引入第二套图表库。

### 1.3 与基线的关系：只加不改

0.3 的每一次改动都必须能回答「**它在 0.2 上的可观测行为差异是什么**」，答案只有两类：

- **加**：新增类型、新增可选符号、新增 IPC 方法/事件、新字段的**默认值等于旧行为**；
- **显式化**：把内核已经存在的行为（出场阶梯、止损、时间退出）从「隐式」变成「随单落审计的显式绑定」，**不新增触发路径**。

任何第三类改动（改变既有阈值、改变既有触发条件、改变既有撮合语义）一律拒绝合入。

---

## 2. BlitzkriegStrategy API 1.0 更名决议

### 2.1 决议

| 项 | 内容 |
|:---|:---|
| 正式名称 | **BlitzkriegStrategy API 1.0**（代码标识符 `blitzkrieg_strategy_api`，crate 名不变） |
| 旧称 | "C ABI v2" —— 进入**废弃期**（Deprecated），仅存在于历史文档与 git 历史 |
| 线协议版本 | `BK_ABI_VERSION` **仍为 2**，`BK_MIN_ABI_VERSION` 仍为 2（`user_layer/strategy_api/src/lib.rs:64,68`） |
| 符号保留 | 全部既有导出符号**零变化**：`bk_strategy_create` / `bk_strategy_abi_version` / `bk_strategy_free_string` + 4 个可选符号 |
| vtable | `BkStrategyVtable` 内存布局**逐字节冻结**（`sizeof` 不变，按值拷贝） |

**为什么改名不改版本号**：`BK_ABI_VERSION` 描述的是**内存布局**，而这次变的是**规范的名称与语义边界**（策略建议 vs 内核裁决）。把版本号推到 3 会让所有 0.2 已构建的 cdylib 立刻不可加载——包括 `scripts/exit-economics-check.mjs` 的测量夹具。语义变更走文档与可选符号，二进制契约保持。

### 2.2 废弃期操作规则

1. 文档层面：本文件、`docs/rust-core/ABI_V2_DESIGN.md`、`docs/rust-core/STRATEGY_GUIDE.md`、`docs/rust-core/EXTENSION_GUIDE.md`、`docs/rust-core/INTERFACES.md` 中对外统一写 API 1.0。
2. 代码层面：`user_layer/strategy_api/src/lib.rs` 顶部 doc 注释改写；`const` 名不动。
3. 运行时回执：`strategy.load` 成功回执追加 `; API 1.0 (line protocol 2)`，**不删旧文本**（有门禁断言回执内容，见 §16.4）。
4. 禁止：任何情况下不得出现 `BK_ABI_VERSION = 3` 的临时分支；不得引入 v1/v2 兼容层（既有决策 D-15 明确 v2 是干净切换）。

### 2.3 新增可选符号：`bk_strategy_declare_modes`

沿用本仓库**已经验证过三次**的可选符号模式（`bk_strategy_gate_exemptions` / `bk_strategy_evolvable_knobs` / `bk_strategy_bind_eval_ctx`）：内核**按名字解析**，缺失 = 未声明；vtable 不动，`sizeof` 不变，旧库零成本。**不要**给 vtable 追加字段——内核按值拷贝 vtable，追加字段会让旧库越界读。

```c
/* 可选导出符号。返回 UTF-8 JSON 字符串，由该库自己的 bk_strategy_free_string 释放。
 * 缺失 / NULL / 非 JSON / 形状非法 一律降级为「未声明」，绝不使加载失败。
 *
 * 载荷（顶层对象，键名小写 snake_case，与 MarketType 既有 wire 拼写同规则）：
 *   {"modes":[{"market_type":"prediction",
 *              "structure":"binary_outcome_wheel",     // 可选，省略 = 该类型下全部结构
 *              "capabilities":["websocket_feed","level2_snapshot"]}]}
 *
 * 空数组或 {"modes":[]} == 非法声明（见 §7.4），不是「未声明」。
 */
char* bk_strategy_declare_modes(void* handle);
```

| 项 | 取值 |
|:---|:---|
| 符号名常量 | `user_layer/strategy_api/src/lib.rs` 新增 `BK_DECLARE_MODES_SYMBOL: &[u8] = b"bk_strategy_declare_modes\0"` |
| 函数签名类型 | `pub type BkDeclareModesFn = unsafe extern "C" fn(handle: BkHandle) -> *mut c_char;` |
| 解析位置 | `core/blitzkrieg_core/src/strategy_engine/loader.rs`（与既有同名可选符号同一处解析） |
| 校验器 | 复用同一个 `ModeDecl` 校验器（插件与策略共用，见 §7.4） |
| 门禁 | `strategy:declaration-check`（§16.4） |

### 2.4 三个建议字段：封口，而非删除

**事实更正（必须写进文档，否则读者会去找不存在的代码）**：基线 HEAD 上 `suggested_stop_loss` / `suggested_take_profit` / `suggested_max_hold_sec` 这三个字符串**在仓库任何源码中都不存在**（全仓库检索命中仅 `docs/DEV_V0_3.md` 自身；此前提过它们的旧稿 `dev-docs/reports/DEV_V0_3.predraft.md` 与发布脚本 `scripts/publish-v0.3-issues.sh` 均已作废/删除）。它们从未进入 0.2 的实现。

因此 v0.3 的「移除」是一次**封口决议**，具体动作是三步：

1. **契约层明示禁止**：`ABI_V2_DESIGN.md` 与 `INTERFACES.md` 列出这三个键为**保留键（Reserved / Refused）**，并给出理由：止损归属内核（§5）。
2. **解析层显式拒绝**：intent 解析器遇到三个键之一 → 记录 `tracing::warn!`（`target: "strategy"`，含策略名、键名、token），并**丢弃该键**继续处理其余字段。**不因它拒绝整单**——一个字段拼错不该让一笔合法交易下不去；但必须留痕，让「谁还在写止损」可见。
3. **门禁层静态扫描**：`strategy:no-stop-loss-check`（§16.4）对 `user_layer/**` 的策略源码做 AST/文本扫描，命中 → 红。

### 2.5 完整 Rust 接口定义（v0.3 增量）

```rust
// user_layer/strategy_api/src/modes.rs —— 新建，类型定义先于实现落地
use crate::{MarketCapabilities, MarketStructure, MarketType};

/// 策略自声明的一个可工作模式。字段含义与插件侧 `MarketMode` 完全相同：
/// 两个类型是「谁在声明」的区分，不是两种数据形状。校验共用一个实现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyMode {
    /// 必填。缺失 = 该 mode 非法（不是「任意市场」）。
    pub market_type: MarketType,
    /// 可选。`None` = 适配该 market_type 下的全部结构。
    pub structure: Option<MarketStructure>,
    /// 可选。默认空 = 不要求任何能力位。
    pub required_capabilities: MarketCapabilities,
}
```

```rust
// user_layer/strategy_api/src/safe.rs —— trait 增量（只加带默认实现的方法）
pub trait SafeStrategy: Send + 'static {
    // ... 既有 15 个方法逐字不动 ...

    /// 本策略可工作的市场模式。空 Vec = 未声明 = 不参与兼容性校验
    /// （与「不导出可选符号」同义，这是 0.2 库的默认行为）。
    ///
    /// 非空的声明参与加载期握手：目标插件不能满足任一 mode 时，
    /// `strategy.load` 拒绝注册、`strategy.enable` 拒绝启用（§8.2）。
    fn declare_modes(&self) -> Vec<StrategyMode> {
        Vec::new()
    }
}
```

`export_strategy!` 宏（`safe.rs:408`）自动生成 `bk_strategy_declare_modes`：`declare_modes()` 为空 → 生成 `NULL`（等价于「符号存在但不声明」）；非空 → 序列化为 §2.3 的载荷。**宏生成的代码是策略作者的唯一入口**，手写 `#[no_mangle]` 不受支持。

### 2.6 完整 Lua 接口定义（v0.3 增量）

```lua
-- 策略包元数据（可选导出；缺失 = 不声明 = 不参与校验）
function bk_declare_modes()
    return {
        { market_type = "prediction",
          structure = "binary_outcome_wheel",
          capabilities = { "websocket_feed", "level2_snapshot" } },
    }
end
```

Lua 侧载荷与 Rust 侧**同一份 JSON 形状**（宿主把 Lua table 编码成 §2.3 的载荷再走同一个校验器）。键名合法值全集见 §7.2–§7.3；非法值 → 加载被拒，错误信息含策略名与非法键（可诊断性门禁断言）。

---

## 3. 核心理念一：策略建议，内核裁决

策略进程（Rust cdylib 或 Lua 状态机）是 **0 信任**组件：它看得见行情，看不见签名器、下单通道、账本与凭证。它唯一能做的事是**提交建议**；决定权在内核。

```
策略侧（0 信任）                      内核侧（唯一权威）
─────────────────                    ──────────────────────────────────────────
evaluate() -> Intents                process_intent(intent, ctx) -> Decision
  ├─ entries[]  {token,price,             Gate 1 合法性      OrderIntent::validate()
  │               reason,shares?}        Gate 2 系统风控    RiskGate + LossBreakers
  ├─ exits[]    {token,reason}           Gate 3 资金预扣    Ledger::reserve
  └─ breaks[]   {token,broken_price}     Gate 4 生存绑定    apply_physics
                                        ────────────────────────────────────────
                                        Approved / Modified / Rejected
                                        每条都写 data/audit/intents.jsonl
```

### 3.1 四层裁决关卡

| 关卡 | 职责 | **复用的既有实现**（不重写） | 拒绝码 |
|:--|:---|:---|:---|
| **Gate 1 合法性** | symbol/side/price/size 自洽；价格区间；tick 与最小名义；token 属于本轮；时钟可交易 | `market_api::OrderIntent::validate()`（`core/market_api/src/types.rs:227`）、`RiskConfig::{min_price,max_price}` | `INVALID_PARAMS` / `INVALID_SIZE` / `INVALID_TICK_SIZE` |
| **Gate 2 系统级风控** | 账户级 5 项 + 全局级 4 项 + 连亏熔断 + 冷静期 + 熔断开关 | `RiskGate::{check,check_with_equity}`、`LossBreakers`（`core/blitzkrieg_core/src/risk.rs:271`） | `RISK_REJECTED` / `KILL_SWITCH_ACTIVE` |
| **Gate 3 资金预扣** | 原子预扣；余额不足**硬拒绝**（不透支、不排队） | `Ledger::reserve`（`core/blitzkrieg_core/src/ledger.rs:45`） | `INSUFFICIENT_FUNDS` |
| **Gate 4 生存绑定** | 把内核既有的止损/时间退出/阶梯**显式绑定**到本单 | `exit_policy::{effective_stop_pct,get_profit_trail_pct,get_time_trail_pct}`、`ExitConfig` | 不拒绝（只绑定） |

**拒绝的层级语义**：平仓类 intent（`is_close_intent`，`risk.rs:20`）在 Gate 2 获得豁免——与既有内核行为逐字一致（"closing intents always pass"）。这条豁免**不是新加的**，是把既有规则写进裁决流程；改动它属于「改内核行为」，被 §1.3 禁止。

### 3.2 数据模型

```rust
// core/blitzkrieg_core/src/arbitration/mod.rs —— 新建
use rust_decimal::Decimal;
use serde::Serialize;

/// 关卡编号。审计里出现的就是这个枚举，索引号不再被当成稳定标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateId { Legality, Risk, Reservation, Physics }

/// 关卡结果。trace 让「哪一层改了这单」可复核，而不是只看最终 Decision。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateTrace {
    pub gate: GateId,
    pub outcome: GateOutcome,
    /// 该关卡的人类可读依据（进日志与审计，不进 UI 主视图）。
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateOutcome { Pass, Modify, Reject }

/// 内核对建议做的**唯一**两类修改。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Modification {
    /// 风控缩减仓位：只缩不放（approved <= suggested 恒成立）。
    SizeReduced { suggested: Decimal, approved: Decimal, limit: String },
    /// tick/步长对齐：对齐方向由内核决定并记录，策略无权指定。
    PriceClamped { suggested: Decimal, approved: Decimal, tick: Decimal },
}

/// 生存绑定（Gate 4 产物）。注意：这里只**记录**内核将执行的出场纪律，
/// 不新增任何触发路径——触发仍由既有 exit_policy / position 完成。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicsBinding {
    /// 硬止损价（由既有 effective_stop_pct 在入场时刻算出）。
    pub stop_price: Decimal,
    /// 时间退出：距回合结束的强制离场秒数（读 ExitConfig::force_exit_sec）。
    pub force_exit_sec: i64,
    /// 分批阶梯快照。未配置阶梯时是既有 exit policy 的**投影**（§4.3）。
    pub ladder: Vec<LadderStep>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LadderStep {
    /// 触发阈值（正数为盈利百分比，负数为亏损百分比）。
    pub at_pct: Decimal,
    /// 平掉的比例 [0,1]。
    pub close_ratio: Decimal,
    /// 平仓后止损移到（`None` = 不动）。
    pub move_stop_to: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    Approved { request_id: String, shares: Decimal, price: Decimal, physics: PhysicsBinding },
    Modified {
        request_id: String,
        modification: Modification,
        shares: Decimal,
        price: Decimal,
        physics: PhysicsBinding,
    },
    Rejected { reason: RejectReason, gate: GateId, detail: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectReason {
    /// Gate 1
    Malformed, OutOfPriceBand, BadTick, NotInRound,
    /// Gate 2
    AccountLimit, GlobalLimit, LossBreaker, Cooldown, KillSwitch, Capacity,
    /// Gate 3
    InsufficientFunds,
    /// 流程自身（不得静默吞掉）
    Internal,
}
```

`RejectReason` 的每一个变体都映射到一个既有 `CoreErrorCode`（`risk.rs` / `ledger.rs` 已在用），映射表写死在 `arbitration::reason_code()` 并单测——**UI 与门禁读的是既有 `coreCode`，不引入第二套错误词汇**。

### 3.3 `process_intent` 流水线

```rust
pub fn process_intent(
    intent: &StrategyIntent,      // 策略建议（entries/exits/breaks 归一后的单笔）
    ctx: &IntentCtx,              // 账户、账本句柄、风控句柄、回合信息、now_ms
) -> Decision;
```

规则（全部可测）：

1. **纯函数式外形**：`process_intent` 不自己下单、不自己动账本，只做判定 + 预留；返回 `Decision` 后由**既有** `Core::place()` 路径提交。这样「裁决」与「执行」的边界是一个函数调用，而不是散落的 if。
2. **无短路**：即使 Gate 1 拒绝也写一条审计（含 `gate` 与 `detail`）。审计写失败**不阻塞**交易（沿用 `jsonl` 的「append 失败被吞掉，绝不致命」规则，`core/blitzkrieg_core/src/jsonl.rs`），但 `tracing::warn!` 一次。
3. **不重复判定**：Gate 2 只调用 `RiskGate`，不复制它的判断；Gate 3 只调用 `Ledger::reserve`，不自己算余额。违反这条的 PR 拒绝合入（否则阈值会有两个真相）。
4. **平仓路径豁免**：`is_close_intent(internal_key)` 为真时 Gate 2 跳过（既有语义），Gate 4 跳过（平仓不需要生存绑定）。

### 3.4 审计留痕

```rust
// core/blitzkrieg_core/src/arbitration/audit.rs
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAuditRecord {
    pub ts_ms: i64,
    pub account_id: String,
    pub strategy: String,
    pub intent_id: String,
    /// 建议原样（已剔除保留键的版本，见 §2.4）
    pub intent: serde_json::Value,
    pub decision: Decision,
    pub gates: Vec<GateTrace>,
    pub latency_us: u64,
}
```

| 项 | 决议 |
|:---|:---|
| 落盘位置 | `data/audit/intents.jsonl`（与既有 `data/` 同级，遵循 `.gitignore`） |
| 写入方式 | 复用 `crate::jsonl::append`（一处实现、追加写、失败不致命） |
| 轮转 | **不做**。JSONL 追加 + 运维侧归档（`data-backup-*`）已覆盖；0.3 不引入轮转器（等出现真实体积问题再加） |
| 读取 | `intent.audit.tail { limit? }` 只读 IPC（§12.3），UI 裁决流用它 |
| 反向验收 | 把 `process_intent` 改成「Gate 1 通过后直接 return Approved」→ 审计里缺失 `gates[0].detail` → `intent-audit-check` 必须红 |

---

## 4. 核心理念二：系统级风控

风控是重力法则：策略可以判断错、可以死循环、可以返回垃圾，但不能因此活下来一笔本该被拦的单。

### 4.1 两条原则

1. **不可绕过**：策略没有 API 能触碰风控；插件没有 API 能触碰风控；Lua 沙箱里没有风控对象。风控的输入只有「账户状态 + 全局状态 + 本单」，输出只有 `Pass / Modify / Reject`。
2. **出厂即静默**：所有**新增**限额的默认值一律 **`0 = 关闭`**，与既有 `RiskConfig::{max_order_notional_pct, max_open_notional_usd}` 的既有约定逐字一致（`risk.rs:63` 注释原文：「0 = disabled (the shipped default: an unconfigured kernel behaves exactly as before)」）。这是本版本唯一允许的「新增风控」姿势——运维显式配置才生效。

### 4.2 限额矩阵

**账户级**（`AccountRiskLimits`，按 `AccountId` 各持一份）

| 字段 | 语义 | 默认 | 说明 |
|:---|:---|:---|:---|
| `max_single_loss` | 单笔最大容忍亏损（USD） | `0` = 关闭 | 由 Gate 2 在**入场**时按 `stop_price` 反算本单最大亏损；超限 → `SizeReduced`（缩到刚好等于上限），缩不下 → `Reject` |
| `max_daily_drawdown` | 日回撤上限（USD） | `0` = 关闭 | **与既有 `PositionConfig::max_daily_loss_usd` 是同一个预算**：配置了它就写入既有字段，**不新增第二个日亏计数器** |
| `max_position_size` | 单账户单标的最大持仓量 | `0` = 关闭 | 与既有 `RiskContext::max_position_size()` 同源（`risk_context.rs:12`），按账户覆盖 |
| `max_consecutive_losses` | 连亏熔断笔数 | `0` = 关闭 | 直接喂既有 `LossBreakers::new(max_consecutive_losses, cooldown_sec)` |
| `cooldown_minutes` | 熔断后冷静期（分钟） | `0` = 关闭 | 配置时换算为秒传给既有 `LossBreakers`；**冷却语义不改**（既有实现是 per-strategy，账户级用 `strategy = "__account__"` 键共用同一个 `LossBreakers` 实例） |

**全局级**（`GlobalRiskLimits`，进程一份）

| 字段 | 语义 | 默认 |
|:---|:---|:---|
| `max_total_position` | 全账户并发持仓总数上限 | `0` = 关闭（既有 `max_positions` 仍是 per-account 的那一个，两者都生效时取更严） |
| `max_total_exposure` | 全账户总名义敞口（USD） | `0` = 关闭 |
| `max_correlation` | 同相关性组的名义上限（USD） | `0` = 关闭 |
| `global_kill_switch_loss` | 全账户日亏总额熔断（USD） | `0` = 关闭 |

**`max_correlation` 的诚实说明**：0.3 **不引入**相关性矩阵模型。它的定义是「**同 `asset` 视为同组**」——即对每个 asset 分别求名义敞口，取最大值与上限比较。这保留了「同一资产上过度集中」这一唯一在单市场单账户下可观测的风险，且零统计假设。真正的跨资产相关性矩阵是独立课题，不在 0.3（§17）。

### 4.3 `apply_physics`：把既有纪律显式化

**关键约束（再次强调）**：`apply_physics` **不新增触发路径**。它做三件事：

1. **绑定止损价**：调用既有 `exit_policy::effective_stop_pct(stop_loss_pct, time_left_sec, &cfg)` 算出本单的硬止损价，写进 `PhysicsBinding::stop_price`。这个价**就是既有出场阶梯已经在用的那个价**——绑定只是把结果抄进审计与 UI。
2. **绑定时间退出**：读既有 `ExitConfig::force_exit_sec`（`exit_policy.rs:106`，出厂 120s）与 `min_time_left_sec`（180s），写进绑定。**不修改它们**（§19 明令禁止）。
3. **阶梯快照**：`ladder` 的来源见下。

#### 分批平仓阶梯（Dynamic Survival Ladder）

| 来源 | 形状 | 默认 |
|:---|:---|:---|
| **投影模式（默认）** | 从既有 `ExitConfig` 的 `{stop_loss_pct, take_profit_pct, trailing_min_high_pct, min_trail_pct}` 生成**两段投影**：一段是止损（`at_pct = -stop_loss_pct`，`close_ratio = 1.0`），一段是既有固定止盈（`at_pct = +take_profit_pct`，`close_ratio = 1.0`） | 开启，且**与 0.2 行为逐字等价**（因为它描述的就是既有阶梯） |
| **显式配置模式（opt-in）** | 运维在 `user_layer/configs/*.toml` 写 `[[risk.ladder]]` 段（`at_pct / close_ratio / move_stop_to`），此时阶梯**接管**：内核按段执行分批平仓 | 关闭（无配置段即无行为） |

> 显式阶梯是**唯一**在 0.3 里新增的平仓触发能力，且它默认关闭、开关在运维手里、每一次触发都落 `INTENT_DECISION` 与既有 `POSITION_CLOSED` 事件。投影模式的 `close_ratio = 1.0` 与既有「一次平完」语义相同——默认开启不等于默认改变行为。

### 4.4 风控读数的来源与呈现

`risk.limits`（新只读 IPC，§12.4）返回**生效值与来源**：

```json
{ "version": "1.1",
  "account": { "id": "default", "limits": {
    "maxSingleLossUsd":    { "value": "0",  "source": "default" },
    "maxDailyDrawdownUsd": { "value": "35", "source": "toml" },
    "maxPositionSize":     { "value": "0",  "source": "default" },
    "maxConsecutiveLosses":{ "value": "0",  "source": "default" },
    "cooldownMinutes":     { "value": "0",  "source": "default" } } },
  "global": { "limits": {
    "maxTotalPosition":        { "value": "0", "source": "default" },
    "maxTotalExposureUsd":     { "value": "0", "source": "default" },
    "maxCorrelationUsd":       { "value": "0", "source": "default" },
    "globalKillSwitchLossUsd": { "value": "0", "source": "default" } } },
  "exit": { "stopLossPct":   { "value": "12",  "source": "default" },
            "takeProfitPct": { "value": "100", "source": "default" },
            "forceExitSec":  { "value": "120", "source": "default" } } }
```

`source ∈ {default, toml, env, flag}` —— 与内核启动时既有的「每个高于默认的设置都记录来源」日志同一套词（`user_layer/configs/default.toml` 顶部规则）。**这就是「五件套」里的 UI 呈现**：面板显示的不是「已配置」，而是「这个值从哪来」。出厂状态下它必须显示 12/100/120 与一串 0。

**热加载边界**：新增的 9 个限额**不进** `risk.setLimits` 白名单（既有白名单只允许改开仓限额，见 `INTERFACES.md` §2.5），改它们需要重启。理由：日回撤/熔断类阈值改变的是「已经发生的事如何解释」，热改会造成同一日两个预算口径。

---

## 5. 核心理念三：策略不再写止损

### 5.1 职责分离

| 关注点 | 策略（Alpha） | 内核（Physics） |
|:---|:---|:---|
| 入场择时 | 发现信号，给出 `{token, price, reason, shares?}` | 校验、定价、定寸、预扣、签名、提交 |
| 仓位定额 | 可给 `size_ratio`/`shares` **建议** | 按账户净值与风险上限**裁决**绝对股数（只缩不放） |
| 止损 | **禁止**（不算、不监视、不下单） | 绑定 → 监视 → 触发（既有 `exit_policy` + `position`） |
| 止盈兑现 | 可发 `exits[]` 建议 | 与阶梯合并后**裁决**出场时机与价格 |
| 距回合结束 | 可关注时效 | 硬性时间退出（既有 `force_exit_sec`） |
| 持仓到期 | 可声明 `holds_to_settlement`（既有能力） | 到期按赎回价值结算 |

### 5.2 策略作者看到的唯一一句话

`docs/rust-core/STRATEGY_GUIDE.md` **顶部**新增章节「**止损不归你管**」，内容按此写（不是软性建议）：

> **止损不归你管。**
>
> 策略只写赚钱逻辑，生存交给内核。你不需要、也不能在策略里做这些事：
> 计算止损价、监视止损价、在 `exits` 里表达"止损"、用 `breaks` 冒充止损。
>
> 原因：内核的出场纪律是**与账户余额、回合剩余时间、盘口新鲜度联动**的；策略只能看到其中的一小部分。策略写的止损一定会与内核的止损不一致，而两个止损并存意味着两个真相——其中一个必然在错误的时间开火。
>
> 你要做的：`entries` 写你**为什么进场**（`reason` 是给人和审计看的），`exits` 写你**为什么离场**（信号消失、机会成本、结构破坏）。**何时**以及**多大比例**离场，内核决定。
>
> 自检：`strategy:no-stop-loss-check` 会把写进策略的止损逻辑钉出来。把你的止损逻辑写进策略，门禁会红。

### 5.3 三个保留键的处置（与 §2.4 一致）

| 键 | 解析器行为 | 门禁行为 | 日志 |
|:---|:---|:---|:---|
| `suggested_stop_loss` | 丢弃 + `warn!` | 源码扫描命中 → 红 | `target: "strategy"`, 含 strategy/token/key |
| `suggested_take_profit` | 丢弃 + `warn!` | 同上 | 同上 |
| `suggested_max_hold_sec` | 丢弃 + `warn!` | 同上 | 同上 |

**例外面**：`holds_to_settlement()` 是既有的**声明**能力（不是止损建议），继续保留。它与止损无关——它声明的是「这批仓位按赎回结算，别用阶梯卖掉」。

---

## 6. 核心理念四：Lua 作为策略语言

### 6.1 决策：`mlua` + Lua 5.4，**vendored**

| 项 | 决议 | 理由 |
|:---|:---|:---|
| 绑定 | `mlua`（`features = ["lua54", "vendored"]`） | `vendored` 编译自带 Lua，运维不需要在机器上装 Lua；版本钉死，不会因系统 Lua 变化而行为漂移 |
| 依赖树 | **这是 0.3 唯一的依赖树新增** | `Cargo.lock` 必须同步（CI 用 `--locked`，漂移即红）；根 workspace 加成员 `user_layer/lua_runtime`，两个 nested workspace（`user_layer/strategies`、`user_layer/parity_strategy`）**不**引入它 |
| 定位 | 与 Rust dylib **平权** | 同一个 `SafeStrategy` 语义位、同一个裁决流水线、同一套门禁 |

### 6.2 沙箱边界（完整清单）

| 类别 | 内容 | 处置 |
|:---|:---|:---|
| **标准库** | `math` `string` `table` | **允许**（`Lua::new_with(StdLib::MATH \| StdLib::STRING \| StdLib::TABLE, ...)`） |
| | `os` `io` `debug` `package` | **禁止**（不装载） |
| | `coroutine` | **禁止**——不是因为它危险，而是因为指令钩子是**每线程**的：协程可以成为钩子的逃逸路径。加它意味着沙箱要额外证明「每个协程都装了钩子」，这是 0.3 不需要承担的成本 |
| **全局函数** | `require` `dofile` `loadfile` `load` `loadstring` `collectgarbage` `string.dump` | **显式置 `nil`**（`load`/`string.dump` 能绕过「不装载 package」的意图；`collectgarbage` 能干扰内存账） |
| | `pairs` `ipairs` `type` `tostring` `tonumber` `setmetatable` `getmetatable` `pcall` `error` `select` `next` `rawget` `rawset` `rawequal` `rawlen` | **允许**（语言基础；`pcall` 允许，但见 §6.3 的「投毒」规则） |
| **宿主注入** | `bk.*` 命名空间 | **唯一**的数据入口（§6.5） |
| **FFI** | 任何形式 | **不存在**：`vendored` + 不装载 `package`，Lua 连 C 模块加载器都没有 |

**唯一的全局写权限**：策略可以写自己的 globals（那是它的状态）。宿主不把 `_G` 暴露成共享表，每个策略一个独立 `Lua` 状态机（`Lua` 不是 `Send`，因此每个状态机固定在一个线程上——这是设计，不是限制）。

### 6.3 资源配额

| 配额 | 值 | 实现 |
|:---|:---|:---|
| 内存 | **16 MB / 状态机** | `lua.set_memory_limit(16 * 1024 * 1024)`。超限 → Lua 侧 `memory allocation error`，宿主捕获为确定性异常，状态机标记 `poisoned` |
| 指令数 | **1,000,000 / 单次回调** | `lua.set_hook(HookTriggers::new().every_nth_instruction(10_000), hook)`；hook 里累加计数，> 1_000_000 → `error` |
| 单次回调墙钟 | 由 soak 门禁实测记录（§16.5），**不预设数字** | 指令预算是主判据；若实测显示 1e6 指令在本机耗时超过 50ms，则下调预算并把实测值写入 `docs/perf/V0_3.md` |

**投毒规则（防 `pcall` 吞掉超限错误）**：hook 触发超限时，**先**把状态机标记为 `poisoned`，**再** `error`。此后宿主拒绝任何对该状态机的调用（含 `on_book` / `evaluate` / `on_kline`），并上报一次 `RISK_ALERT`（code `INTERNAL`，detail 含策略名与原因）。这样即使策略用 `pcall` 包住整个 `evaluate`，"超限后继续跑" 也不存在——`pcall` 只能吞掉错误，吞不掉投毒。

### 6.4 策略分发格式

```
user_layer/strategies_lua/
└── lua_momentum/
    ├── strategy.lua       # 入口：定义 bk_* 全局函数
    ├── manifest.json      # 元数据 + 源码指纹
    └── README.md          # 算法说明与参数示例
```

`manifest.json`：

```json
{
  "name": "lua_momentum",
  "version": "0.1.0",
  "api": "1.0",
  "entry": "strategy.lua",
  "sha256": "3f1c…",
  "author": "…",
  "description": "…",
  "tunables": { "threshold": { "type": "decimal", "default": "0.04" } }
}
```

| 规则 | 内容 |
|:---|:---|
| 指纹校验 | `manifest.json::sha256` 是对 `strategy.lua` 的 sha256。加载时**必须**校验；不匹配 → 拒绝加载（对齐既有 #207 纪律：不能追溯到本 checkout 源码的策略，不予测量/加载） |
| 命名冲突 | 目录名 ≠ `manifest.name` → 拒绝加载（拒绝两套身份） |
| 与 dylib 扫描器隔离 | Lua 包放在 `--lua-strategy-dir`（默认 `user_layer/strategies_lua`）。**不要**放进 `user_layer/strategies`——那是 `*.dylib/*.so` 的扫描点，也是 `exit-economics-check.mjs` 的 `STRATEGY_DIR`，混进去会让「测量夹具」这一语义变模糊 |
| `README.md` | 必需（缺失 → 加载告警，不拒绝；文档缺失不该阻挡交易，但要可见） |

### 6.5 `bk.*` 宿主 API（完整）

```lua
-- 只读，全部返回副本（策略改不动宿主数据）
bk.now_ms()                    -- 整数
bk.round()                     -- { slot=, time_left_sec=, now_ms= }
bk.markets()                   -- { {asset=,condition_id=,up_token=,down_token=,expires_at_ms=,neg_risk=}, ... }
bk.book(token)                 -- { symbol=, best_bid=, best_ask=, mid=, obi=, spread=, bid_depth=, ask_depth=, ts_ms=, fresh= } | nil
bk.params()                    -- 热参数字典（字符串->字符串，与 Rust ParamBag 同一来源）
bk.kline(symbol, interval)     -- { open=,high=,low=,close=,volume=,is_closed= } | nil   (§10)
bk.account()                   -- { id=,name=,market_type=,balance=,available=,reserved= }  -- 只读，无凭证

-- 建议出口（不是下单 API：返回值交给内核裁决）
function bk_on_book(update) end                        -- 可选
function bk_on_kline(kline) end                        -- 可选 (§10.4)
function bk_on_round(round) end                        -- 可选
function bk_evaluate()                                 -- 必需
    return { entries = { {token=,price=,reason=,shares?} },
             exits   = { {token=,reason=} },
             breaks  = { {token=,broken_price=} } }
end
```

**`bk.params()` 与 `bk.account()` 是最小暴露面**：前者是策略自己的热参数，后者是它被授权操作的账户的**公开展示字段**（无凭证、无私有键）。`bk.account()` 存在是因为策略可能需要按余额调整建议规模——但它拿到的是 `balance`，不是「如何下单」。

### 6.6 加载与生命周期

| 阶段 | Rust | Lua |
|:---|:---|:---|
| 发现 | `--strategy-dir` 扫描 dylib | `--lua-strategy-dir` 扫描 `manifest.json` |
| 校验 | `bk_strategy_abi_version() == 2` + 可选符号 | `manifest.sha256` 匹配 + 必需函数存在 |
| 注册 | `strategy.load` 回执含 API 1.0 | `strategy.load` 回执含 `(lua)` 标记 |
| 启用 | `strategy.enable` | 同左（同一个开关，同一条状态持久化 `data/strategy-state.json`） |
| 隔离 | `catch_unwind` 兜策略 panic（既有） | 投毒 + 错误捕获兜 Lua 异常（新） |
| 影子进化 | 既有 `bk_strategy_evolvable_knobs` | `manifest.tunables` + 热参（复用同一 `ParamRegistry`） |

**影子进化对 Lua 的边界**：Lua 策略参与热参数与影子孪生，但**不参与自动改写源码**。0.3 不做「机器改写策略源码」。

---

## 7. 市场声明三层模型

### 7.1 三层

```
Layer 1  MarketType          这个市场「是什么」（必填，唯一）
Layer 2  MarketStructure     它「怎么撮合」（可选，None = 不限）
Layer 3  MarketCapabilities  它「能提供什么」（位图，要求是超集）
```

### 7.2 Layer 1：`MarketType`（7 值，含 v0.3 新增 3 值）

既有 4 值（`core/market_api/src/types.rs:173`，wire 为 `snake_case`）：`Prediction` `Spot` `Futures` `Options`。
v0.3 **新增 3 值**：`Margin`（杠杆现货/借币） `Earn`（理财/生息，只读为主） `Bot`（第三方机器人托管市场）。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketType {
    Prediction, Spot, Margin, Futures, Options, Earn, Bot,
}
```

**序列化注意**：枚举**加变体是 wire 兼容的**（旧消费者收到未知字符串会报错——这是期望行为，不是缺陷：旧面板不认识 `earn` 就该明确失败，而不是把它当 `prediction` 画出来）。UI 侧的 `MarketType` 映射（`ui/webapp/webui/src/api/client.ts:379`）必须同步补 3 个 label，且 `market-plugin-check.mjs` 断言每个变体都有 label。

### 7.3 Layer 2 / Layer 3

```rust
// core/market_api/src/modes.rs —— 新建
/// 它怎么撮合。`None`（在 mode 里）= 适配该 market_type 下全部结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStructure {
    /// 连续双向盘口撮合。现货/合约的主力形态。
    CentralLimitOrderBook,
    /// 无盘口，按到达时间连续成交（部分 DEX 路由、场外撮合）。
    ContinuousAuction,
    /// 定时集合竞价（开盘/收盘/轮次切换）。
    CallAuction,
    /// 恒定函数做市（AMM/CFMM）。
    AutomatedMarketMaker,
    /// 二元结果轮盘：一轮一结，两个 token 互补（Polymarket 预测市场）。
    BinaryOutcomeWheel,
    /// 询价成交（期权、大宗）。
    RequestForQuote,
}
```

```rust
/// 它提供什么。位图，手写（不引入 bitflags 依赖——库里 2.13 只是传递依赖，
/// 为 10 个常量加一个直接依赖不值得；`u64` 的位运算 stdlib 就够）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketCapabilities(pub u64);

impl MarketCapabilities {
    pub const NONE: Self = Self(0);
    pub const WEBSOCKET_FEED: Self       = Self(1 << 0);
    pub const LEVEL2_SNAPSHOT: Self      = Self(1 << 1);
    pub const KLINE_STREAM: Self         = Self(1 << 2);
    pub const TRADE_STREAM: Self         = Self(1 << 3);
    pub const LEVERAGE: Self             = Self(1 << 4);
    pub const SHORT_SELLING: Self        = Self(1 << 5);
    pub const BATCH_ORDERS: Self         = Self(1 << 6);
    pub const POST_ONLY: Self            = Self(1 << 7);
    pub const CANCEL_ON_DISCONNECT: Self = Self(1 << 8);
    pub const MAKER_REBATE: Self         = Self(1 << 9);

    /// 超集判定：`self` 是否满足 `required`。位运算一处实现。
    pub fn satisfies(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

/// 插件自声明的一个市场模式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketMode {
    pub market_type: MarketType,
    pub structure: Option<MarketStructure>,
    pub capabilities: MarketCapabilities,
}
```

**申报名称 ↔ 位常量**对应表（wire 用小写 snake_case，与 `MarketType` 同规则）：`websocket_feed` `level2_snapshot` `kline_stream` `trade_stream` `leverage` `short_selling` `batch_orders` `post_only` `cancel_on_disconnect` `maker_rebate`。表在 `market_api::modes::capability_by_name()` 一处，解析与文档共用。

**本文档不代插件声明能力位图**：Polymarket 插件实际支持哪些能力，由 `extensions/polymarket` 自己的 `declare_modes()` 给出，`market.list` 原样呈现。文档只钉**申报格式**与**握手规则**。

### 7.4 校验器（一个实现，两个调用方）

```rust
// core/market_api/src/modes.rs
pub fn parse_modes(payload: &str) -> Result<Vec<MarketMode>, ModeError>;
/// 策略侧复用：StrategyMode 与 MarketMode 字段相同，解码后同一套校验。
pub fn parse_strategy_modes(payload: &str) -> Result<Vec<StrategyMode>, ModeError>;

pub enum ModeError {
    NotJson(String),
    /// mode 数组为空——「声明了但什么都没声明」是配置错误，不是「不声明」。
    Empty,
    /// market_type 缺失或非法。这是唯一必填字段。
    MissingMarketType { index: usize },
    UnknownMarketType { index: usize, got: String },
    UnknownStructure { index: usize, got: String },
    UnknownCapability { index: usize, got: String },
}
```

**「空」与「未声明」的区别必须写死**：

| 输入 | 语义 |
|:---|:---|
| 符号缺失 / 返回 NULL | 未声明 → 不参与校验 |
| `{"modes":[]}` | **非法** → 加载拒绝，错误信息含 `Empty` |
| `{"modes":[{...}]}` 且某个 mode 缺 `market_type` | **非法** → 加载拒绝 |

理由：把「空数组」当「未声明」会让一个写错的声明静默变成「不声明」，故障不可见。**声明了就必须是合法的声明**。

### 7.5 兼容性规则（写死）

```
策略声明的某个 mode M 与插件声明的某个 mode P 相容，当且仅当：
  (1) M.market_type == P.market_type                      必须强匹配
  (2) M.structure == None 或 M.structure == P.structure    策略不要求则不限
  (3) P.capabilities.satisfies(M.required_capabilities)    插件能力必须是超集

策略「可装载」当且仅当存在至少一个相容对。
```

注意 (2) 的方向：**策略可以宽松，插件必须具体**。插件声明 `structure: None` 表示「我这个插件适配该类型下所有结构」——这是插件的自我否定，允许，但**此时策略要求具体结构时不相容**（插件说不清自己是不是 CLOB，就不能承诺给一个要求 CLOB 的策略）。这条要在 `ModeError` 文档与 `EXTENSION_GUIDE.md` 里写成第一句话，因为它是唯一容易搞反的地方。

---

## 8. 策略多模式声明

### 8.1 声明在哪

| 策略形态 | 声明方式 |
|:---|:---|
| Rust cdylib | `export_strategy!` 生成的 `bk_strategy_declare_modes`（源：`SafeStrategy::declare_modes`） |
| Lua 包 | `strategy.lua` 的 `bk_declare_modes()`（可选） |
| 未声明 | 不参与兼容性校验（0.2 全部策略的默认状态） |

### 8.2 握手时机与失败后果

| 时机 | 检查 | 失败后果 |
|:---|:---|:---|
| 启动扫描 | 只在日志记录「声明了什么」，**不做拒绝** | 无（列表出厂为空，不改变状态） |
| `strategy.load { path }` | 相容性判定 | **拒绝注册**，回执 `Rejected { path, reason }`，`reason` 形如 `incompatible modes: strategy wants [futures/clob] but active plugin 'polymarket' offers [prediction/binary_outcome_wheel]` |
| `strategy.enable { name }` | 相容性判定 | **拒绝启用**，返回 `{ name, enabled: false, found: true, reason }` |
| 启动时 `--enable-strategy <name>` | 相容性判定 | 该策略被跳过并记录 ERROR 日志；若这次启动因此 live 模式下**一个策略都没有**，沿用既有规则（`--allow-zero-strategies` 之外的场景 exit 1）。这是既有纪律的复用，不是新规则 |

**错误信息必须说出来两边**（策略要什么、插件给什么、差在哪一位）。这是可诊断性门禁的断言对象：反向测试里比对的是**错误信息的字段**，不是「返回了 false」。

### 8.3 呈现

`strategy.list` 每行增加 `modes`（声明原样）与 `compatible`（布尔）+ `incompatibleReason`（可空）。`market.list` 每行增加 `structure` 与 `capabilities`（可读名数组，同时给位图原值便于比对）。

---

## 9. 多市场多账户设计

### 9.1 `Account` 是一等公民

```rust
// core/blitzkrieg_core/src/account/mod.rs —— 新建
use crate::ledger::Ledger;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 账户标识。字符串 newtype，不用整数：账户来自配置与人手，编号化只会
/// 让日志里出现 "account 3" 这种没人能读的东西。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

/// 0.2 数据（无 account_id 的行）落到这里，保证历史 JSONL 继续可读。
pub const DEFAULT_ACCOUNT_ID: &str = "default";
pub fn default_account_id() -> AccountId { AccountId(DEFAULT_ACCOUNT_ID.to_string()) }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// 可下单（仍受风控与门禁约束）。
    Active,
    /// 只读：可看、可对账，任何下单请求在 Gate 2 被拒（`AccountLimit`）。
    ReadOnly,
    /// 冻结：禁止新开仓，**平仓豁免**（与熔断开关同一姿势——不能让冻结
    /// 变成把仓位困在原地的理由）。
    Frozen,
    /// 挂起：与 `Frozen` 相同但附原因字符串，用于人工介入。
    Suspended { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub market_type: MarketType,
    pub status: AccountStatus,
    /// 该账户的账本（资金与预扣的唯一真相）。
    #[serde(skip)]
    pub ledger: Ledger,
    /// 凭证的**环境键名**，绝不是凭证值。见 §9.3。
    pub credential_keys: CredentialKeys,
    pub updated_at_ms: i64,
}

/// 只有键名。任何序列化路径都不得输出值——`credential_keys` 本身是可安全
/// 序列化的，因为它没有任何地方存过值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialKeys {
    pub api_key_env: Option<String>,
    pub secret_env: Option<String>,
    pub passphrase_env: Option<String>,
    pub private_key_env: Option<String>,
    /// 启动时是否成功读到（布尔，不回值）。
    pub loaded: bool,
}
```

### 9.2 `account_id` 的贯穿（**只加字段，默认值等于旧行为**）

| 结构 | 文件 | 增量 | 兼容手段 |
|:---|:---|:---|:---|
| `OrderRequest` | `core/blitzkrieg_core/src/model.rs:140` | `account_id: AccountId` | `#[serde(default = "default_account_id")]` |
| `OrderIntent` | `core/market_api/src/types.rs:213` | `account_id: AccountId` | 同上 |
| `Position` / `OpenPosition` | `core/blitzkrieg_core/src/position.rs` | `account_id: AccountId` | 同上 |
| `Fill` / `MarketFill` | `model.rs` / `market_api/src/types.rs:349` | `account_id: AccountId` | 同上 |
| `LedgerEntry` | `core/blitzkrieg_core/src/ledger.rs` | `account_id: AccountId` | 同上 |
| 行情快照（回合上下文类结构） | `marketdata.rs` / strategy_api | `account_id: AccountId` | 同上 |
| `PositionClosed` 事件 | `core/blitzkrieg_core/src/ipc/schema.rs:620` | `account_id` 字段 | 加字段，既有字段与拼写不动 |
| 三个持久化 DB | `order_db.rs` / `position_db.rs` / `trade_db.rs` | 行级 `account_id` | 读旧行 → `default`；**既有键语义不变** |

**为什么用 `serde(default)` 而不是数据迁移**：既有 `jsonl` 的加载规则是「不可解析的行跳过，绝不致命」（`jsonl.rs` 头部注释）。给 `account_id` 一个默认值让 0.2 的数据**不需要迁移就能读**，而迁移脚本是「可能删掉用户数据」的那类程序。0.3 不写迁移脚本。

**写入侧**：新记录**一律写显式 `account_id`**（包括 `"default"`）。读旧写新 = 老数据可读、新数据可追溯。

### 9.3 账本隔离

```rust
/// 账本按账户隔离：一个 AccountId 一个 Ledger 实例。
/// 复用既有 `Ledger`（reserve/release/settle_* 全部不动），
/// 只把「唯一账本」变成「按账户查表」。
pub struct AccountLedgers {
    map: HashMap<AccountId, Ledger>,
    active: AccountId,
}

impl AccountLedgers {
    /// 账户不存在时**显式报错**，不隐式建一个空账本。
    /// 隐式建账本是「A 账户的钱被 B 账户花掉」这类事故的温床。
    pub fn get_mut(&mut self, id: &AccountId) -> CoreResult<&mut Ledger>;
    pub fn active(&self) -> (&AccountId, &Ledger);
    pub fn switch(&mut self, id: &AccountId) -> CoreResult<()>;
}
```

`Ledger` 自身零改动（方法签名不动，只加 `account_id` 到 `LedgerEntry`）。**总账恒等式**（`balance == seed + Σ netPnl − Σ 开仓成本 + Σ 平仓收入`，`account-drift-check.mjs` 的口径）变为**按账户各持一份**：`account-parity.mjs` 扩展为「每个账户各自 bit-identical」，而不是「总计相等」——总计相等会掩盖 A 补 B 的错误。

### 9.4 凭证

| 规则 | 内容 |
|:---|:---|
| 加载者 | **只有内核进程**。从内核自身环境变量读取，键名由账户配置给出 |
| 策略 | 无凭证（`bk.account()` 只给 `id/name/market_type/balance/available/reserved`） |
| 插件 | 只拿自己那个账户的凭证，且**经内核注入的客户端**使用，不拿到原始字节 |
| IPC | 任何方法的响应都不得包含凭证值。`account.list` 只回 `credentialsLoaded: bool` |
| 日志 | 永不打印值（既有 `net.rs` 对代理 URL 的「报名字不报值」规则，同一姿势） |
| 落盘 | 账户配置里只存**环境键名**；凭证值不落 `data/`、不落 `user_layer/configs/` |
| 反向验收 | 植入哨兵凭证值（如 `BK_TEST_SECRET=…sentinel…`），把**所有** IPC 响应 JSON 串起来 grep 哨兵 → 命中即红。这是 `account-credential-check.mjs` 的断言 |

### 9.5 账户配置来源

```toml
# user_layer/configs/accounts.toml （新建；文件缺失 = 只有一个 default 账户）
[[account]]
id = "default"
name = "Poly Main"
market_type = "prediction"
status = "active"
api_key_env = "POLY_API_KEY"
secret_env = "POLY_SECRET"
passphrase_env = "POLY_PASSPHRASE"

[[account]]
id = "paper"
name = "Paper Trading"
market_type = "prediction"
status = "read_only"      # 只读：能看不能下单
```

| 项 | 决议 |
|:---|:---|
| 默认 | 文件不存在 → 单一 `default` 账户（`status = active`，凭证键名沿用既有单账户环境变量）→ **0.2 部署零改动** |
| 优先级 | CLI > env > toml > 默认（沿用既有 `default.toml` 顶部规则） |
| 新增/删除账户 | **不做运行期增删**（账户是部署事实，不是运行时状态）。改账户 = 改配置 + 重启 |
| 活跃账户 | `account.switch` 是**会话级**（按 UDS 连接），不是进程级全局：两个客户端可以看两个账户。切换只影响**该连接后续请求的默认 account_id**，不影响内核调度 |

### 9.6 环境与插件的关系（诚实边界）

**0.3 的插件实例与账户是 1:1**：一个 `MarketPlugin` 驱动一个账户。两个账户意味着两个插件实例（同一 `market_type`）。这是「多账户」在 0.3 的真实边界，写在这里以免被读成「一个插件并发服务多账户」（那是 0.4+ 的课题，见 §17）。

---

## 10. K 线交易兼容

### 10.1 位置与定位

盘口 Tick 仍是内核的主驱动（性能路径不动）。K 线是**旁路派生视图**：由既有行情流聚合，供传统周期策略与面板使用。它不参与撮合、不参与风控。

### 10.2 数据结构

```rust
// core/blitzkrieg_core/src/kline/mod.rs —— 新建
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KlineInterval { Sec1, Sec5, Sec15, Min1, Min5, Min15, Hour1, Hour4, Day1 }

impl KlineInterval {
    pub fn secs(self) -> i64 {
        match self {
            Self::Sec1 => 1, Self::Sec5 => 5, Self::Sec15 => 15,
            Self::Min1 => 60, Self::Min5 => 300, Self::Min15 => 900,
            Self::Hour1 => 3600, Self::Hour4 => 14400, Self::Day1 => 86400,
        }
    }
    /// 桶起点：UTC 对齐的向下取整。`Day1` 是 **UTC 日**，不是本地日——
    /// 显式声明，因为「日线按哪一天切」是 K 线最常见的静默分歧。
    pub fn bucket_open_ms(self, ts_ms: i64) -> i64 {
        let s = self.secs() * 1000;
        ts_ms.div_euclid(s) * s
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kline {
    pub symbol: String,
    pub interval: KlineInterval,
    pub open_time_ms: i64,
    /// `open_time_ms + interval - 1`。恒等于该值，由构造保证，不由调用方提供。
    pub close_time_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub trade_count: u64,
    /// true 恰好一次：第一笔越过 `close_time_ms` 的输入到达时。
    pub is_closed: bool,
}
```

**decimal 走既有约定**：全部经既有 decimal 序列化（字符串）过界，K 线不是例外。**不允许 f64 出现在 K 线任何位置**——K 线的 low/high 会被用来判断「是否触及止损」，f64 漂移在这里是钱的问题。

### 10.3 聚合器

```rust
/// 从 Trade/盘口更新聚合 K 线。
///
/// 设计取舍（写在类型上，不写在注释里）：
/// - **一分桶只留一个当前 bar**：`HashMap<(symbol, interval), Kline>` +
///   一个 `last_closed`。不做历史环形缓冲——历史归 `kline.history`
///   （从既有归档/DB 读），不占常驻内存。
/// - **乱序一律丢弃**：`ts_ms < bar.open_time_ms` 的输入不参与聚合。
///   丢弃计数在 `stats()` 里可见（静默丢弃是缺陷，可见丢弃是纪律）。
/// - **绝不生成倒挂 K 线**：`close_time_ms` 只由 `bucket_open_ms` 决定，
///   因此「上一根 bar 的 close_time 大于这一根」在构造上不可能发生。
pub struct KlineAggregator {
    intervals: Vec<KlineInterval>,
    bars: HashMap<(String, KlineInterval), Kline>,
    last_closed: HashMap<(String, KlineInterval), Kline>,
    dropped_out_of_order: u64,
    no_data_bars: u64,
}

impl KlineAggregator {
    pub fn new(intervals: Vec<KlineInterval>) -> Self;
    /// 吃一笔成交。返回「刚刚闭合」的 bar（若有）——
    /// 回调与事件都由它驱动，不由时钟驱动。
    pub fn on_trade(&mut self, symbol: &str, price: Decimal, size: Decimal, ts_ms: i64)
        -> Vec<Kline>;
    /// 当前未闭合 bar 的快照（`is_closed = false`），供订阅推送。
    pub fn current(&self, symbol: &str, interval: KlineInterval) -> Option<&Kline>;
    pub fn stats(&self) -> AggregatorStats;
}
```

| 规则 | 内容 |
|:---|:---|
| 触发 | **由数据驱动**，不由计时器驱动。一个 symbol 长时间无成交时，其 bar 不会自己闭合（会有 `no_data_bars` 计数可见） |
| 输入源 | 既有成交/盘口路径。**原生 K 线流（`KLINE_STREAM` 能力）在 0.3 只做能力位与通道留位，不做实现**——本地聚合是唯一实现，行为可复现 |
| 内存上界 | `symbols × intervals × sizeof(Kline)`；以 8 symbol × 9 interval 计，远小于既有 E14 内存基线的零头。门禁断言上界公式，不靠人工观察 |
| 精度 | 全程 `Decimal`，无浮点 |
| 反向验收 | ① 10,000 笔随机 Tick 的流式结果 == 离线基准逐字段；② **注入时间戳倒挂输入 → 断言 `close_time_ms` 单调不减且 `dropped_out_of_order > 0`**（改坏成「照单全收」→ 门禁红） |

### 10.4 策略回调

```rust
// SafeStrategy 增量（又一个带默认实现的方法，vtable 不动）
fn on_kline(&mut self, _kline: &Kline) {}
```

`on_kline` **只收到已闭合的 bar**（`is_closed = true`）。未闭合 bar 通过 `bk.kline(symbol, interval)`（Lua）按需读——理由：把未闭合 bar 推进回调会让每个策略都要自己判断 `is_closed`，而漏判的那个策略会在重复信号里下单。

### 10.5 IPC 与 UI

见 §12.2 / §13.2。
---

## 11. 技术架构与目录结构

### 11.1 模块图

```
                       ┌──────────────────────────────────────────────┐
                       │            blitzkrieg-core（内核）            │
                       │  唯一有签名器、下单通道、账本、风控的进程        │
                       └──────────────────────────────────────────────┘
   UDS JSON-RPC 1.1        ▲                    ▲                ▲
   （Node / TUI / WebUI）   │                    │                │
   ─────────────────────────┘         ┌──────────┘                │
                                      │                           │
                       ┌──────────────┴───────────┐   ┌───────────┴──────────────┐
                       │  strategy_engine/        │   │  market/ registry        │
                       │  ├─ loader.rs (dylib)    │   │  ├─ MarketPlugin trait   │
                       │  ├─ lua 桥（经 trait）    │   │  └─ declare_modes 握手    │
                       │  └─ arbitration/（裁决）   │   └───────────┬──────────────┘
                       └──────────────┬───────────┘               │
                                      │                            │
       ┌──────────────────────────────┴──────────────┐   ┌─────────┴────────────┐
       │  user_layer/lua_runtime（mlua 5.4 沙箱）       │   │  extensions/*         │
       │  └─ 独立 workspace 成员，不在 libblitzkrieg_core│   │  ├─ polymarket        │
       └──────────────────────────────────────────────┘   │  └─ binance_spot(配置) │
                                                          └──────────────────────┘
```

**关键的依赖方向**（不许出现反向边）：

```
market_api  ◄──  core  ──►  build_info
    ▲             ▲
    │             └── strategy_api ──►（无内部依赖）
    │
extensions/*      lua_runtime ──► strategy_api（只做类型对齐，不依赖 core）
```

`market_api` 的 crate 头注释写明它「deliberately has **no internal dependencies**」——`MarketStructure`/`MarketCapabilities` 放这里正是为了保持这一点：`strategy_api` 需要它们做模式声明，而 `strategy_api` 不能依赖 `core`（否则外挂策略会把内核拖进依赖图）。

**因此新增的 `modes.rs` 归属 `market_api`，`strategy_api` 通过 `market_api` 的类型做声明**——这是 0.3 唯一新增的一条 crate 间依赖边，方向由 crate 头注释的既定纪律决定。

### 11.2 v0.3 目录增量（`[NEW]` / `[MOD]` 标注）

```
BlitzkriegBot/
├── core/
│   ├── blitzkrieg_core/src/
│   │   ├── arbitration/            [NEW] 裁决流水线（§3）
│   │   │   ├── mod.rs              #    process_intent / Decision / GateTrace
│   │   │   └── audit.rs            #    IntentAuditRecord + jsonl 落盘
│   │   ├── account/                [NEW] 多账户（§9）
│   │   │   └── mod.rs              #    AccountId/Account/AccountStatus/AccountLedgers
│   │   ├── kline/                  [NEW] K 线（§10）
│   │   │   ├── mod.rs              #    Kline / KlineInterval
│   │   │   └── aggregator.rs       #    KlineAggregator
│   │   ├── risk.rs                 [MOD] 只加 AccountRiskLimits/GlobalRiskLimits 与
│   │   │                           #     apply_physics 的只读接线；既有 check 不改
│   │   ├── strategy_engine/        [MOD] loader.rs 增 declare_modes 解析
│   │   ├── position.rs             [MOD] OpenPosition/ClosedPosition 加 account_id
│   │   ├── ledger.rs               [MOD] LedgerEntry 加 account_id
│   │   ├── model.rs                [MOD] OrderRequest/Fill 加 account_id
│   │   └── ipc/schema.rs           [MOD] 新增方法/事件（§12）
│   ├── market_api/src/
│   │   ├── modes.rs                [NEW] MarketType/MarketStructure/MarketCapabilities/
│   │   │                           #      MarketMode/ModeError/parse_modes
│   │   └── types.rs                [MOD] MarketType 加 3 变体；OrderIntent 加 account_id
│   └── market_api/                 #      （无其它改动）
├── extensions/
│   └── polymarket/                 [MOD] 实现 declare_modes()
├── user_layer/
│   ├── strategy_api/src/
│   │   ├── modes.rs                [NEW] StrategyMode
│   │   ├── lib.rs                  [MOD] BK_DECLARE_MODES_SYMBOL；文档头改 API 1.0
│   │   └── safe.rs                 [MOD] trait 加 declare_modes/on_kline；export 宏扩两符号
│   ├── lua_runtime/                [NEW] 独立 workspace 成员：mlua 沙箱宿主
│   │   ├── Cargo.toml              #     mlua = { features = ["lua54","vendored"] }
│   │   ├── src/sandbox.rs          #     状态机 + 配额 + 投毒
│   │   ├── src/bk_api.rs           #     bk.* 宿主函数
│   │   └── src/lua_strategy.rs     #     LuaStrategy: SafeStrategy 实现
│   ├── strategies_lua/             [NEW] Lua 策略投放点（§6.4）
│   │   └── lua_momentum/           #     官方 Lua 示例
│   └── strategies/spread_arb/      [MOD] 加 declare_modes()；**不改逻辑**
├── ui/
│   ├── ui_kit/src/gateway/         [MOD] 新 IPC 方法透传
│   └── webapp/webui/src/
│       ├── components/kline/       [NEW] KlineChart.vue（复用既有 charts/ 约定）
│       ├── components/AccountSwitcher.vue  [NEW]
│       ├── pages/Decisions.vue     [NEW] 裁决流
│       └── pages/{Overview,Strategies,SettingsPage}.vue [MOD]
├── scripts/
│   ├── strategy-no-stop-loss-check.mjs   [NEW] §16.4
│   ├── strategy-declaration-check.mjs    [NEW]
│   ├── risk-systemic-check.mjs           [NEW]
│   ├── plugin-modes-check.mjs            [NEW]
│   ├── lua-sandbox-check.mjs             [NEW]
│   ├── kline-aggregation-check.mjs       [NEW]
│   ├── intent-audit-check.mjs            [NEW] §16.4（裁决留痕）
│   ├── account-credential-check.mjs      [NEW] §9.4（凭据不泄漏）
│   └── account-cross-isolation-check.mjs [NEW] §16.4（跨账户隔离）
└── docs/
    ├── DEV_V0_3.md                 [NEW] 本文
    └── rust-core/{INTERFACES,STRATEGY_GUIDE,EXTENSION_GUIDE,ARCHITECTURE,ABI_V2_DESIGN}.md [MOD]
```

### 11.3 关于「门禁脚本」的两条纪律

1. **门禁与产品同构**：每个门禁只测一件事，命名 `领域-对象-check.mjs`（既有 40+ 脚本全此形），零依赖裸 Node（`scripts/README.md` 首行纪律），需要内核时用 `scripts/lib/child-guard.mjs` 的 `spawn`（否则中断的门禁会留下 `PPID=1` 的孤儿内核，2026-09-19 有 33 小时孤儿的实例）。
2. **门禁必须有反向验收**：`--teeth` 或等价的自证失败能力。0.3 新增的 9 个门禁全部要求 `--teeth`，且 **`--teeth` 接进 CI**（不是留给人工跑——`exit-economics` 的教训是「没人跑的检查等于关掉的检查」）。

---

## 12. IPC 契约

**总协议不变**：UDS、一行一个 JSON、JSON-RPC 2.0、每消息带 `version: "1.1"`。0.3 的所有 IPC 变更是**加项**：新增方法、新增事件变体、既有响应加字段。**版本号保持 1.1**（与 `INTERFACES.md` §4 既有惯例一致：加项不改版本号）。

### 12.1 账户管理（新增）

| method | params | result |
|:---|:---|:---|
| `account.list` | `{}` | `{ "version": "1.1", "active": "default", "accounts": [AccountView...] }` |
| `account.switch` | `{ "accountId": "paper" }` | `{ "active": "paper" }`（**会话级**：只改这条连接后续请求的默认 account_id，见 §9.5） |
| `account.status` | `{ "accountId", "status": "frozen", "reason"? }` | `{ "id", "status" }`（**只能收紧**：`Active → ReadOnly/Frozen/Suspended` 是允许的；反向解冻必须走 CLI/配置 + 重启。运行期不能「解锁」，否则风控的收紧动作可以被一条指令撤销） |

```jsonc
// AccountView
{ "id": "default",
  "name": "Poly Main",
  "marketType": "prediction",
  "status": "active",
  "balance": "1500.50",
  "available": "1200.10",
  "reserved": "300.40",
  "credentialsLoaded": true,          // 布尔，绝不回值（§9.4）
  "openPositions": 2,
  "dayRealizedUsd": "-12.30",
  "updatedAtMs": 1758888000000 }
```

### 12.2 K 线（新增）

| method | params | result |
|:---|:---|:---|
| `kline.history` | `{ "symbol", "interval", "limit"? }` | `{ "symbol", "interval", "klines": [KlineView...] }`（按 `openTimeMs` 升序，`limit` 默认 200 上限 1000） |
| `kline.subscribe` | `{ "symbols": ["BTC-UP"], "intervals": ["min1", "min15"] }` | `{ "subscribed": 2 }` |
| `kline.unsubscribe` | `{ "symbols": [...], "intervals": [...] }` | `{ "subscribed": 0 }` |

**事件**（`core.event`，`kind` 用既有 `SCREAMING_SNAKE_CASE` 规则，与 `ready`/`order_update`/`position_closed` 同一形状）：

```jsonc
{ "jsonrpc": "2.0",
  "method": "core.event",
  "params": {
    "kind": "KLINE_UPDATE",
    "kline": { "symbol": "BTC-UP", "interval": "min1",
               "openTimeMs": 1758887940000, "closeTimeMs": 1758887999999,
               "open": "0.4520", "high": "0.6210", "low": "0.4180", "close": "0.5820",
               "volume": "125000", "tradeCount": 431, "isClosed": true } } }
```

**两处必须钉住的细节**：

1. `is_closed` 在事件里**真实存在且可区分**——未闭合 bar 每 tick 推一次但**限流**（同 `(symbol, interval)` 最快 1 次/秒），闭合 bar 立即推一次且**必定推**。理由：面板要画「正在长的那根」，但每秒 50 次重绘是浪费；而闭合那一次不能被限流吃掉，否则 K 线会永远缺一根。
2. **订阅是会话级的**（按连接），连接断开自动退订——订阅泄漏会让一个关闭的面板继续把行情推给一个不存在的 socket。

### 12.3 裁决流（新增）

| method | params | result |
|:---|:---|:---|
| `intent.audit.tail` | `{ "limit"?: 50, "accountId"?, "strategy"?, "decision"?: "rejected" }` | `{ "records": [IntentAuditRecordView...], "total"? }` |

返回最新的 N 条（读尾部，不读全文件）。`decision` 过滤值 `approved` / `modified` / `rejected`。UI 的裁决流用它。

**事件**（可选推送，用于面板实时反映裁决）：

```jsonc
{ "kind": "INTENT_DECISION",
  "accountId": "default", "strategy": "lua_momentum", "intentId": "…",
  "status": "MODIFIED",                      // APPROVED / MODIFIED / REJECTED
  "gate": "RISK",
  "detail": "size reduced 25.0 -> 6.0 by max_order_notional",
  "tsMs": 1758888000123 }
```

`INTENT_DECISION` 是**节流推送**：同一策略同类 Reject 连续发生时按 `(strategy, gate, reason)` 折叠（每秒最多一条 + 计数）。理由：被风控拒绝的 intent 可以在毫秒级产生几千条（既有 `refusal-attribution` 记的 34,480 次拒绝），逐条推送会把面板与 socket 打爆。**折叠的是推送，不是审计**——审计永远逐条落盘。

### 12.4 风控读数（新增）

`risk.limits`（见 §4.4 完整响应示例）。只读、无副作用、**不取 Core 锁**（对齐既有 `system.version` 的姿势：数据锁未就绪时也必须能回答，因为运维在故障时刻最需要看这个）。

### 12.5 模式声明与兼容性（新增/扩展）

| method | 变更 |
|:---|:---|
| `market.list` | 每行**加** `structure`（可空字符串）与 `capabilities`（可读名数组）+ `capabilitiesBits`（u64 原值）。既有字段零变化 |
| `strategy.list` | 每行**加** `modes`（声明原样，未声明为空数组）、`compatible`（bool）、`incompatibleReason`（可空） |
| `strategy.load` | 回执**加** `; API 1.0 (line protocol 2)`（§2.2）；失败时 `reason` 含相容性诊断（§8.2） |

### 12.6 既有方法的行为扩展（不改拼写，不改语义）

| method | 变更 |
|:---|:---|
| `positions.list` | 每行加 `accountId` |
| `orders.list` | 每行加 `accountId` |
| `orders.place` | params **加可选** `accountId`（省略 = 会话默认账户）；响应不变 |
| `ledger.balance` | params **加可选** `accountId`（省略 = 会话默认）；响应不变 |
| `engine.stats` | 加 `accounts`（每账户一行统计）与 `kline`（聚合器 stats：bars/dropped/closed） |
| `core.ready` | 加 `apiVersion: "1.0"` 与 `accountId`（会话默认） |

**兼容性断言（门禁对象）**：`core-parity.mjs` 的 22 条断言必须**全部继续绿**，且新增一条「旧形状请求（不带 accountId）与显式 `accountId: "default"` 的结果逐字段相同」。

---

## 13. UI 设计

### 13.1 现状与约束

| 项 | 事实（基线核对） |
|:---|:---|
| 面板 | 两个：**TUI**（`ui/ui_kit_panel`，ratatui + crossterm，tab 驾驶舱）与 **WebUI**（`ui/webapp/webui`，Vue 3 + Vite + Pinia） |
| WebUI 页面 | `Overview` / `HftPage` / `Strategies` / `EvolutionPage` / `BacktestPage` / `Plugins` / `SettingsPage` |
| 图表 | 自己写的 ECharts 组件（`components/charts/`：`BookDepth.vue` / `EquityCurve.vue` / `RejectionChart.vue`） |
| **不存在的东西** | `SpotPanel` / `FuturesPanel`（**没有这个组件**）、Lightweight Charts（**未安装**，也没必要装） |

**因此 0.3 的 K 线图是**：在 `components/charts/` 下加 `KlineChart.vue`（ECharts `candlestick` series），并挂到 **`HftPage`**（主要交易面）与 `Overview`（摘要）。**不安装新图表库**、**不新建 Spot/Futures 面板**——那两个名字来自对代码的误读，不在本版本范围（§A.1）。

### 13.2 K 线图（`KlineChart.vue`）

```
┌─ BTC-UP  ── [min1] [min5] [min15] ────────── 持仓标记 ☑ 裁决标记 ☑ ─┐
│ 0.65 ┤                          ╱╲                                │
│      │        ╱╲    ╱╲        ╱   ╲      ▲ 内核止损线 (0.5180)      │
│ 0.52 ┤   ╱╲  ╱  ╲  ╱  ╲  ╱╲ ╱      ╲╱                            │
│      │  ╱  ╲╱    ╲╱    ╲╱  ╲╱    ● 入场 ● 内核裁决点                │
│ 0.40 ┤╱                                                          │
│      └──────────────────────────────────────────────────────────  │
│       12:30   12:45   13:00   13:15   13:30   13:45               │
└───────────────────────────────────────────────────────────────────┘
```

| 元素 | 数据来源 | 语义（必须与代码一致） |
|:---|:---|:---|
| K 线主体 | `kline.history` + `KLINE_UPDATE` | `isClosed = false` 的最后一根画半透明（"正在长"） |
| **▲ 内核止损线** | `risk.limits.exit.stopLossPct` + 入场价 | 一条水平线。**只画内核的那一条**——不画策略意图的线（策略没有） |
| **● 入场点** | `PositionClosed` / `positions.list` 的 `entryPrice` + `OPENED` 时刻 | 时间对齐到 bar 的 `openTimeMs`（不对齐会画出「未来的成交」） |
| **● 内核裁决点** | `INTENT_DECISION` / `intent.audit.tail` | 只有 `APPROVED`/`MODIFIED` 画点；`REJECTED` 走下面的拒绝条 |
| 拒绝条（可选开启） | `intent.audit.tail?decision=rejected` | 在 bar 底部画短竖线（黄），表示「这一分钟有建议被拦」 |

**「物理止损线」的诚实呈现**：这条线是**入场时刻算出的止损价**，不是一个永远钉在那里的价格（既有 `effective_stop_pct` 随剩余时间变化）。所以 UI 上它是**该持仓的当前止损位投影**，并且当它变化时旧值画成虚线、新值画成实线，视觉上留下"止损被上移过"的痕迹。**不要把 `LadderStep` 的 `move_stop_to` 画成一条不会动的横线**——那会让人以为止损是静态的。

### 13.3 账户切换器（`AccountSwitcher.vue`）

位置：WebUI 顶部导航右侧（与既有 `LoginView` 的状态区并列）。TUI 对应：命令栏 `account <id>` 动词（新增 gateway 命令）+ Status tab 显示当前账户。

```
┌──────────────────────────────┐
│ ● default  Poly Main         │   ← 颜色 = 状态（绿 active / 灰 read_only
│   prediction · active        │      琥珀 frozen / 红 suspended）
│   avail  1200.10  USDC       │
│   positions 2   day −12.30   │
└──────────────────────────────┘
```

| 交互 | 行为 |
|:---|:---|
| 切换 | 调 `account.switch`；成功后**整页数据重取**（持仓/挂单/K 线/裁决流全部按新账户重查，不合并显示） |
| 状态非 `active` | 顶部横幅（复用既有 `safety-banners.check.mjs` 的横幅机制）：`frozen` = 黄，"新开仓已停止，平仓仍可执行"；`suspended` = 红 + reason |
| 只读账户 | 下单/平仓控件置灰，并给出**具体原因**（不是笼统的「无权限」） |

**`tui-parity.check.mjs` 必须同步更新**：新增 TUI tab 或 WebUI 页面都要按既有规则在两处登记，否则 parity 门禁红（这是它的设计意图——新增面必须两边都有家）。

### 13.4 裁决流页（`Decisions.vue`）

新增 WebUI 页面，与 TUI 对应（新增 tab 或并入既有 Positions tab 的子视图——由 `tui-parity` 决定，不在这里预设）。

```
┌─ 裁决流 ── 过滤: [全部] [已通过] [已修改] [已拒绝]   账户: default ───┐
│ 时间      策略         结果      关卡    详情                       │
│ 13:04:22  lua_momentum MODIFIED  RISK    size 25.0 → 6.0 (名义上限)  │
│ 13:04:19  spread_arb   REJECTED  RISK    连亏 3 笔，冷静期至 13:19    │
│ 13:04:11  spread_arb   APPROVED  PHYSICS stop 0.5180, force-exit 120s│
└─────────────────────────────────────────────────────────────────────┘
```

一行的信息量按此固定：**时间 / 策略 / 结果 / 关卡 / 详情**。`详情` 直接取 `GateTrace.detail`（内核写的那句人话），UI 不自己编文案——否则同一次拒绝在日志、面板、审计里会有三种说法（既有 `refusal-attribution` 的教训就是这个：面板把两类不相交的拒绝合成一个数字，读起来完全错）。

### 13.5 三层声明与兼容性呈现

`Plugins.vue` 每行加：`structure` 徽标（如 `CLOB` / `BinaryOutcomeWheel`）+ `capabilities` 徽标组（最多 4 个，其余显示 `+N`，悬浮列全）。
`Strategies.vue` 每行加：声明模式徽标 + 不兼容时的红徽标（悬浮显示 `incompatibleReason` 原文）。**不兼容的策略行必须显示原因**，否则用户看到的是「策略加载失败」而不知道是市场不匹配——这是 §8.2 「说出来两边」在 UI 侧的对应要求。
---

## 14. 任务拆分（Epic E24–E30）

### 14.0 Wave 0：接口冻结节（**先于一切 Epic**）

`INTERFACES.md` 的冻结不是在 Epic 里做的——**Epic 开工时它必须已经冻好**。因此先落一个小的、只含「类型与签名」的 PR，它是全部并行开发的地基：

| Wave 0 交付 | 内容 | 为什么必须在 Wave 0 |
|:---|:---|:---|
| `core/market_api/src/modes.rs` | `MarketStructure` / `MarketCapabilities` / `MarketMode` / `ModeError` 类型（无解析逻辑） | 被 E24/E27/E29 同时引用 → 定义了就不会三方打架 |
| `user_layer/strategy_api/src/modes.rs` | `StrategyMode` 类型 | 同上 |
| `strategy_api/src/lib.rs` | `BK_DECLARE_MODES_SYMBOL` + `BkDeclareModesFn` 常量与类型 | 符号名一旦定死，E24/E27 不必各写一个 |
| `strategy_api/src/safe.rs` | `declare_modes()` / `on_kline()` **两个带默认实现的签名**（方法体 `Vec::new()` / `{}`） | 两个 Epic 要加的两个 trait 方法一次落位，避免二次动 trait |
| `core/blitzkrieg_core/src/kline/mod.rs` | `Kline` / `KlineInterval`（含 `secs()`/`bucket_open_ms()`） | E29 的聚合器与 E30 的 `bk.kline` 共用 |
| `core/blitzkrieg_core/src/account/mod.rs` | `AccountId` / `DEFAULT_ACCOUNT_ID` / `AccountStatus` / `CredentialKeys` | E25 的审计记录、E28 的账户、E27 的 `market.list` 都要它 |
| `market_api/src/types.rs` | `MarketType` 加 `Margin`/`Earn`/`Bot` 三变体 | 枚举加变体会影响 match 穷尽性检查 → 后加会让三个分支同时红 |
| 三个**热点文件**的缝 | `service.rs` 一行委托调用、`schema.rs` 的类型声明、`server.rs` 两个 arm（见 §14.2） | 让 7 个 Epic 不必改同一批文件 |
| `user_layer/lua_runtime/` | **空壳 crate** + `mlua` 依赖 + `Cargo.lock` 同步 | **lock 只变一次**：否则 E30 的 `Cargo.lock` hunk 会让所有并行分支 rebase 冲突 |
| `INTERFACES.md` | 本文 §12 的契约表落进文档 | 冻结的判据是「文档已写、类型已编译」，不是「口头约定」 |

> Wave 0 的 `lua_runtime` 空壳会让 `cargo build --workspace` 多编译一次 mlua（约 5–10 秒）。**这是故意付出的成本**：用一次构建时间，换掉七条分支上的 lock 文件冲突。

**Wave 0 的验收**：`cargo build --release --workspace --locked` 绿、`cargo test --workspace --locked` 绿、既有全部门禁绿（证明「只加类型」确实零行为变化）。**任一既有门禁变红 → Wave 0 不合并**。

### 14.1 七个 Epic

每个 Epic 的格式：**编号 / 目标 / 关键任务 / 验收标准（含反向验收）/ 领地白名单 / 领地声明**。

---

#### Epic E24：BlitzkriegStrategy API 1.0 契约与免止损封口

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e24-api10` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e24" -b feat/v0.3-e24-api10 feat/trading-safety-selfcheck` |
| **目标** | G1（契约面）+ G4（免止损的契约与门禁面） |

**关键任务**

1. `strategy_api` 文档头改名 API 1.0；`export_strategy!` 宏生成 `bk_strategy_declare_modes`（空 `Vec` → `NULL`）。
2. `loader.rs`：解析该符号 + 起止日志；`declare_modes` 载荷走 Wave 0 的校验器（非法 → 拒载，错误含 `index` 与 `got`）。
3. **保留键封口**：intent 解析遇 `suggested_stop_loss` / `suggested_take_profit` / `suggested_max_hold_sec` → `warn!` + 丢弃键（§2.4）。
4. `strategy.load` 成功回执追加 `; API 1.0 (line protocol 2)`（保留既有文本）。
5. 官方 Rust 示例：`user_layer/examples/momentum_alpha/`（新增**独立嵌套 workspace**，附 `Cargo.lock`；根 `Cargo.toml` 的 `exclude` 增一行）。
6. 文档：`INTERFACES.md`（保留键 + API 1.0 命名）、`ABI_V2_DESIGN.md`（v0.3 增补节）、`STRATEGY_GUIDE.md`（§5.2 章节 + 示例索引）。
7. 门禁：`strategy:no-stop-loss-check`、`strategy:declaration-check`（§16.4）。

**验收标准**

- [ ] 0.2 构建的 `spread_arb_strategy.dylib` **不重编译**直接 `strategy.load` 成功（证明线协议未破坏）。
- [ ] `strategy:declaration-check` 覆盖：合法 3 例 / 非法 5 例（非 JSON、空数组、缺 `market_type`、未知 structure、未知 capability），全部给出含 `index` 的错误。
- [ ] **反向验收 A**：源码注入 `local stop = price * 0.965` 并放进 `exits` 的 reason → `strategy:no-stop-loss-check --teeth` 必须红。
- [ ] **反向验收 B**：向 loader 喂含 `suggested_stop_loss` 的 intent → 断言 `warn!` 出现一次**且**该 intent 的入场结果与「不带该键」逐字段相同（证明它不产生任何止损作用）。

**领地白名单**

```
user_layer/strategy_api/**
user_layer/examples/**
core/blitzkrieg_core/src/strategy_engine/loader.rs
Cargo.toml                                 # 仅 exclude 数组 +1 行
scripts/strategy-no-stop-loss-check.mjs
scripts/strategy-declaration-check.mjs
docs/rust-core/INTERFACES.md
docs/rust-core/ABI_V2_DESIGN.md
docs/rust-core/STRATEGY_GUIDE.md
```

**领地声明**：`strategy_api/**` 全权独有。`loader.rs` 与 E27 **同文件** → 本 Epic 先合并，E27 rebase（§15.2 串行对 S1）。白名单外**零改动**（PR 必须附 `git diff --name-only` 输出）。

---

#### Epic E25：策略建议 / 内核裁决分离流水线

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e25-arbitration` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e25" -b feat/v0.3-e25-arbitration feat/trading-safety-selfcheck` |
| **目标** | G2 |

**关键任务**

1. `arbitration/mod.rs`：`GateId` / `GateTrace` / `GateOutcome` / `Modification` / `PhysicsBinding` / `LadderStep` / `Decision` / `RejectReason` + `reason_code()` 映射（单测覆盖全部变体，一一对应既有 `CoreErrorCode`）。
2. `arbitration/pipeline.rs`：`process_intent` 四关卡；Gate 1 调 `OrderIntent::validate()`，Gate 2 调 `RiskGate::check_with_equity`，Gate 3 调 `Ledger::reserve`，Gate 4 产出 `PhysicsBinding`（读既有 `ExitConfig`，投影模式）。
3. `arbitration/audit.rs`：`IntentAuditRecord` + `data/audit/intents.jsonl` 落盘（复用 `jsonl::append`）。
4. 接线：`service.rs` 的**既有** intent 消费点前插一行委托（热点文件，见 §14.2）。
5. CLI：`--no-intent-audit`（关审计，供「字节等价」验证用，见 §15.3）。
6. IPC：`intent.audit.tail`、`INTENT_DECISION` 事件（**节流推送**，§12.3）。
7. UI：`Decisions.vue` + TUI 侧对应面（由 `tui-parity` 决定形态）。
8. 门禁：`intent-audit-check.mjs`。
9. 文档：`ARCHITECTURE.md` 裁决章节。

**验收标准**

- [ ] 每条进出场建议都有一条审计记录，`gates` 长度 ≥ 1，且最后一条 `outcome` 与 `Decision` 自洽（`Approved` → 无 `Reject`；`Rejected` → 恰有一条 `Reject`）。
- [ ] 极端拒绝风暴（4000 条/秒 Reject）下：审计**逐条**落盘不丢，`INTENT_DECISION` 推送被折叠到 ≤ 1 条/秒（§12.3）。
- [ ] **反向验收 A**：把 `process_intent` 改成 Gate 1 后直接 `return Approved{...}` → `intent-audit-check --teeth` 红（缺 `gates[1].detail`）。
- [ ] **反向验收 B**：把 Gate 1 的 `validate()` 换成 `Ok(())`（跳过合法性）→ 用「精度非法 intent」喂入 → 断言该单**到达 OME**，门禁红。反向证明这条门禁真的守住「绝不进入 OME」。
- [ ] **零行为变化证明**：`node scripts/exit-economics-check.mjs`（开审计）与 `--no-intent-audit` 两种情形**都**通过既有 `BASELINE`（§16.5）。

**领地白名单**

```
core/blitzkrieg_core/src/arbitration/**
core/blitzkrieg_core/src/cli.rs                    # 仅 +--no-intent-audit
core/blitzkrieg_core/src/service.rs                # 热点：仅委托调用一行 + 审计句柄
core/blitzkrieg_core/src/ipc/schema.rs             # 热点：仅 intent.audit.tail + INTENT_DECISION
core/blitzkrieg_core/src/ipc/server.rs             # 热点：仅对应两个 arm
scripts/intent-audit-check.mjs
ui/webapp/webui/src/pages/Decisions.vue
ui/webapp/webui/src/components/DecisionTable.vue
ui/webapp/webui/src/api/client.ts                  # 仅新增类型与方法
ui/webapp/webui/src/App.vue                        # 仅导航项
ui/ui_kit_panel/src/**                             # 仅新增裁决面
docs/rust-core/ARCHITECTURE.md
```

**领地声明**：三个热点文件（`service.rs` / `schema.rs` / `server.rs`）在本 wave 内以**最小 hunk** 触碰，且必须在 PR 描述里单列「热点文件改动清单」（每个文件的行数与用途）。E25 是这三个文件在 Wave 1 的**监护人**（§14.2）。

---

#### Epic E26：系统级风控与动态生存防线

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e26-systemic-risk` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e26" -b feat/v0.3-e26-systemic-risk feat/trading-safety-selfcheck` |
| **目标** | G3 |

**关键任务**

1. `risk/limits.rs`：`AccountRiskLimits` / `GlobalRiskLimits`（全部默认 `0` = 关闭），从既有配置链（CLI > env > toml > 默认）解析；满足上限时生成 `Modification::SizeReduced`。
2. `risk/physics.rs`：`apply_physics` —— 绑定 `stop_price`（调既有 `effective_stop_pct`）、`force_exit_sec`、投影/显式阶梯（§4.3）。
3. `risk.rs` 的既有 `RiskGate::check_with_equity` **只在末尾追加**新增限额的判定（默认关闭 → 恒不触发）；**既有判定条件一行不改**。
4. 账户级接线：`max_daily_drawdown` → 既有 `PositionConfig::max_daily_loss_usd`；`max_consecutive_losses`/`cooldown_minutes` → 既有 `LossBreakers::new`（**不新增日亏计数器**）。
5. IPC `risk.limits`（§12.4，含 `source` 字段）。
6. UI：`SettingsPage.vue` 风控卡片（生效值 + 来源）、持仓视图显示当前止损位与阶梯进度。
7. 门禁：`risk-systemic-check.mjs`。
8. 文档：`ARCHITECTURE.md` 风控章节 + `dev-docs/PERF_BASELINE.md` 记录一次公开的旁证（无需改基线文件）。

**验收标准**

- [ ] 出厂状态（不配置任何限额）下：`risk.limits` 返回全部 `0` + 既有 `exit` 三值为 `12 / 100 / 120`。
- [ ] 连亏触阈 → 后续入场被拒且 `RejectReason::LossBreaker`，冷静期结束后自动恢复；平仓类 intent 全期间**不被拦**。
- [ ] `max_single_loss` 生效时：超限单被 `SizeReduced` 到「刚好等于上限」的股数，且 `approved <= suggested` 恒成立（单测遍历随机 1000 组）。
- [ ] **反向验收 A**：把 Gate 2 的限额判定改成恒 `Pass` → `risk-systemic-check --teeth` 红。
- [ ] **反向验收 B**：把 `apply_physics` 的 `stop_price` 从「既有 `effective_stop_pct` 结果」改成硬编码 `0.99` → 「单边击穿止损线」场景断言失败，门禁红。
- [ ] **反向验收 C**：把 `force_exit_sec` 绑定的读取源从 `ExitConfig` 改成常量 `120`（写死）→ 用 `--exit-*` 覆盖后再跑，断言「绑定值 != 配置值」→ 门禁红（证明绑定真的是读配置而不是抄数值）。

**领地白名单**

```
core/blitzkrieg_core/src/risk.rs                   # 仅末尾追加 + 新增模块声明
core/blitzkrieg_core/src/risk/**
core/blitzkrieg_core/src/risk_context.rs
core/blitzkrieg_core/src/config.rs                 # 仅 +risk 段解析
core/blitzkrieg_core/src/ipc/schema.rs             # 热点：仅 risk.limits
core/blitzkrieg_core/src/ipc/server.rs             # 热点：仅对应 arm
user_layer/configs/*.toml                          # 仅新增注释块与示例段（不设默认值）
scripts/risk-systemic-check.mjs
ui/webapp/webui/src/pages/SettingsPage.vue         # 仅风控卡片
docs/rust-core/ARCHITECTURE.md
```

**领地声明**：`risk.rs` 的既有函数体**零改动**是硬约束——修改既有判定会被 §19 直接拒绝合入。与 E25 在 `schema.rs`/`server.rs` 上同 wave 冲突 → 由监护人落地（§15.2）。

---

#### Epic E27：市场三层声明与插件兼容性握手

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e27-market-modes` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e27" -b feat/v0.3-e27-market-modes feat/trading-safety-selfcheck` |
| **目标** | G5 的声明面 |

**关键任务**

1. `market_api/src/modes.rs` 补实现：`parse_modes` / `parse_strategy_modes` / `capability_by_name()` / `satisfies` 使用点（类型来自 Wave 0）。
2. `market_api/src/types.rs`：`PluginInfo` 加 `structure: Option<MarketStructure>` 与 `capabilities: MarketCapabilities`。
3. `market/MarketPlugin` trait 加 `declare_modes()`（**带默认实现**返回空 `Vec`，既有第三方插件不破）。
4. `extensions/polymarket`：实现 `declare_modes()`，如实申报自己支持的能力位（**不代它猜**）。
5. 兼容性握手：`strategy.load` / `strategy.enable` / `--enable-strategy` 三处（§8.2）。
6. IPC：`market.list` / `strategy.list` 字段扩展（热点）。
7. UI：`Plugins.vue` / `Strategies.vue` 徽标与不兼容原因。
8. 门禁：`plugin-modes-check.mjs`。
9. 文档：`EXTENSION_GUIDE.md` 多模式章节（含 §7.5 的「方向性」第一句）。

**验收标准**

- [ ] `market.list` 每行的 `capabilitiesBits` 与 `capabilities`（名字数组）互推一致（门禁双向断言）。
- [ ] 未声明模式的 0.2 策略**行为零变化**（`compatible: true`，可正常 load/enable）。
- [ ] **反向验收 A**：把 `satisfies` 改成 `self.0 != required.0` → `plugin-modes-check --teeth` 红。
- [ ] **反向验收 B**：给只声明 `prediction` 的插件加载声明 `futures` 的策略 → `strategy.load` 拒绝，且 `reason` 同时含 `futures`、`prediction` 与插件名（断言是**三个子串都在**，不是「返回了错误」）。
- [ ] **反向验收 C**：插件声明 `structure: None` + 策略要求 `structure: CLOB` → 判定为**不相容**（§7.5 方向性），门禁断言这条。

**领地白名单**

```
core/market_api/src/modes.rs
core/market_api/src/types.rs                       # MarketType 变体已在 Wave 0；本 Epic 仅 PluginInfo
core/market_api/src/plugin.rs
core/blitzkrieg_core/src/market/**
core/blitzkrieg_core/src/strategy_engine/loader.rs # 串行对 S1：E24 先合并
extensions/polymarket/**
core/blitzkrieg_core/src/ipc/schema.rs             # 热点：market.list / strategy.list
core/blitzkrieg_core/src/ipc/server.rs             # 热点：对应 arm
scripts/plugin-modes-check.mjs
ui/webapp/webui/src/pages/{Plugins,Strategies}.vue
docs/rust-core/EXTENSION_GUIDE.md
```

**领地声明**：与 E24 共享 `loader.rs`（串行对 S1）；与 E25/E26 共享 IPC 两文件（同 wave 热点）。`extensions/polymarket/**` 独有。

---

#### Epic E28：多市场多账户一等公民

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e28-accounts` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e28" -b feat/v0.3-e28-accounts feat/trading-safety-selfcheck` |
| **目标** | G5 的账户面 |

**关键任务**

1. `account/mod.rs` 补实现：`Account` / `AccountLedgers`（§9.3），`get_mut` 缺账户显式报错。
2. `account/config.rs`：读 `user_layer/configs/accounts.toml`；文件缺失 → 单一 `default` 账户（0.2 部署零改动）。
3. `account_id` 贯穿全部结构（§9.2 表格逐行落地），一律 `#[serde(default = "default_account_id")]`。
4. `AccountLedgers` 接入 `service.rs`（热点）：`ledger` 单例 → 按账户查表；**既有 `Ledger` 方法体零改动**。
5. 三个持久化 DB 加行级 `account_id`（读旧行 → `default`，不写迁移脚本）。
6. IPC：`account.list` / `account.switch` / `account.status`（**只能收紧**）。
7. UI：`AccountSwitcher.vue` + 全页数据按账户重取；TUI 命令栏 `account <id>`。
8. 门禁：`account-cross-isolation-check.mjs`、`account-credential-check.mjs`，并**扩展** `account-parity.mjs`（每账户各自 bit-identical）。
9. 文档：`ARCHITECTURE.md` 多账户章节 + `INTERFACES.md` 账户方法。

**验收标准**

- [ ] A 账户的交易、亏损、回撤**完全不影响** B 账户的 `balance`/`available`/持仓/日 PnL（逐字段断言）。
- [ ] `account-parity.mjs` 在两个账户上**各自** bit-identical（不是总计相等）。
- [ ] 无 `account_id` 的 0.2 数据行（orders/positions/trades 三库）可正常读取，且读回后**重新写出的行带 `"default"`**（读旧写新）。
- [ ] `account.status` 只能收紧：尝试 `frozen → active` 必须被拒（`INVALID_PARAMS`）。
- [ ] **反向验收 A**：A 账户发起平仓 B 账户的持仓 → Gate 1/2 拒绝，且审计记录里 `accountId = A`、`detail` 含 B 的 positionId。
- [ ] **反向验收 B**：注入哨兵凭证值 → 把所有 IPC 响应 JSON 串起来 grep 哨兵 → 命中即红（`account-credential-check`）。
- [ ] **反向验收 C**：把 `get_mut` 的「缺账户报错」改成「隐式建空账本」→ 断言 A 账户订单在 B 账户余额充足时**成功** → 门禁红（证明这条护栏真的在防串账）。

**领地白名单**

```
core/blitzkrieg_core/src/account/**
core/blitzkrieg_core/src/ledger.rs                 # 仅 LedgerEntry +account_id
core/blitzkrieg_core/src/model.rs                  # 仅 +account_id 字段
core/blitzkrieg_core/src/position.rs               # 仅 +account_id 字段
core/blitzkrieg_core/src/{order_db,position_db,trade_db}.rs   # 仅行字段
core/blitzkrieg_core/src/service.rs                # 热点：账本查表 + 账户句柄
core/blitzkrieg_core/src/ipc/schema.rs             # 热点：account.*
core/blitzkrieg_core/src/ipc/server.rs             # 热点：对应 arm
user_layer/configs/accounts.toml
scripts/account-{cross-isolation,credential}-check.mjs
scripts/account-parity.mjs                         # 扩展为按账户
ui/webapp/webui/src/components/AccountSwitcher.vue
docs/rust-core/{ARCHITECTURE,INTERFACES}.md
```

**领地声明**：`service.rs` 在 Wave 2 由本 Epic 监护；`ledger.rs`/`position.rs`/`model.rs` 只加字段不改逻辑（PR 描述需说明「本 PR 未修改任何既有方法体」）。

---

#### Epic E29：K 线数据结构、聚合器与呈现

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e29-kline` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e29" -b feat/v0.3-e29-kline feat/trading-safety-selfcheck` |
| **目标** | G6 |

**关键任务**

1. `kline/aggregator.rs`：`KlineAggregator`（§10.3）+ `AggregatorStats`。
2. 接线：既有成交/盘口回调 → `on_trade`（热点文件，只加一处喂入 + 闭合事件的派发）。
3. `on_kline` 回调派发（trait 方法已在 Wave 0 冻结）；只推 `is_closed = true`。
4. IPC：`kline.history` / `kline.subscribe` / `kline.unsubscribe` + `KLINE_UPDATE` 事件（含限流规则）。
5. UI：`components/charts/KlineChart.vue`（ECharts candlestick）+ `HftPage.vue` / `Overview.vue` 挂载 + 止损线/入场点/裁决点标记（§13.2）。
6. 门禁：`kline-aggregation-check.mjs`。
7. 文档：`INTERFACES.md` K 线方法 + `STRATEGY_GUIDE.md` `on_kline` 用法。

**验收标准**

- [ ] 10,000 笔随机成交的流式结果与离线基准**逐字段相等**（含 `tradeCount`/`volume`）。
- [ ] 每个 interval 的 `open_time_ms` 对齐 `bucket_open_ms`；`close_time_ms == open_time_ms + interval - 1` 恒成立（穷尽 9 个 interval）。
- [ ] 订阅断开自动退订（`kline.unsubscribe` 不需要也能清干净）——重建连接后不会双推。
- [ ] `is_closed = false` 的推流 ≤ 1 条/秒/（symbol, interval）；`is_closed = true` **一 bar 一条，一条不少**。
- [ ] **反向验收 A**：注入倒挂时间戳 → 断言 `close_time_ms` 单调不减 且 `dropped_out_of_order > 0`。
- [ ] **反向验收 B**：把 `bucket_open_ms` 的 `div_euclid` 改成 `+ 1`（向上取整）→ 离线基准对拍红。
- [ ] **反向验收 C**：把 `close_time_ms` 改成「由调用方传入」→ 门禁断言「构造上不可能倒挂」的用例红。

**领地白名单**

```
core/blitzkrieg_core/src/kline/**
core/blitzkrieg_core/src/engine.rs                 # 热点：仅喂入一行 + 派发
core/blitzkrieg_core/src/ipc/schema.rs             # 热点：kline.*
core/blitzkrieg_core/src/ipc/server.rs             # 热点：对应 arm
ui/webapp/webui/src/components/charts/KlineChart.vue
ui/webapp/webui/src/pages/{HftPage,Overview}.vue
ui/webapp/webui/src/stores/panel.ts                # 仅 kline 状态
scripts/kline-aggregation-check.mjs
docs/rust-core/{INTERFACES,STRATEGY_GUIDE}.md
```

**领地声明**：`Kline`/`KlineInterval` 类型已在 Wave 0，本 Epic **不再改类型**（改了就会与 E30 打架）。`engine.rs` 在 Wave 2 由本 Epic 监护。

---

#### Epic E30：Lua 5.4 沙箱与双栈示例

| 项 | 内容 |
|:---|:---|
| **分支** | `feat/v0.3-e30-lua` |
| **worktree** | `git worktree add "/Volumes/Hard Disk/bk-wt-e30" -b feat/v0.3-e30-lua feat/trading-safety-selfcheck` |
| **目标** | G1（Lua 面）+ G4 的 Lua 侧 |

**关键任务**

1. `lua_runtime/src/sandbox.rs`：`Lua::new_with(StdLib::MATH|STRING|TABLE)`（不装载 `os`/`io`/`debug`/`package`/`coroutine`）、显式置 `nil` 的高危全局（§6.2）、`set_memory_limit(16MB)`、`set_hook` 指令预算（1e6/回调）+ **投毒**。
2. `lua_runtime/src/bk_api.rs`：`bk.*` 全部只读函数（§6.5）。
3. `lua_runtime/src/lua_strategy.rs`：`LuaStrategy: SafeStrategy`（`name`/`on_book`/`on_round`/`on_kline`/`evaluate`/`take_breaks`/`declare_modes`）。
4. 发现与加载：`--lua-strategy-dir`（默认 `user_layer/strategies_lua`）+ `manifest.json::sha256` 校验 + 名字冲突拒绝。
5. 官方 Lua 示例：`user_layer/strategies_lua/lua_momentum/`（`strategy.lua` + `manifest.json` + `README.md`）。
6. 门禁：`lua-sandbox-check.mjs`。
7. 文档：`STRATEGY_GUIDE.md` Lua 章节（分发格式、`bk.*`、两个配额、投毒语义）。

**验收标准**

- [ ] Rust 与 Lua 两个示例策略同时加载、同时启用、各自产生 intent 并全部走裁决流水线（同一份审计文件里可见两个策略名）。
- [ ] Lua 示例在 DryRun 下连续运行 24 小时无崩溃、无状态机投毒、内存曲线平稳（用既有 `soak-health` 框架跑，记录到 `docs/perf/V0_3.md`）。
- [ ] `strategy.load` 回执含 `(lua)` 标记；`strategy.list` 里 Lua 与 dylib 同形（同名字段集合）。
- [ ] **反向验收 A**：`require('io')` / `os.time()` / `debug.getinfo` → 全部确定性异常（不是 crash），宿主继续跑完测试。
- [ ] **反向验收 B**：`while true do end` → 指令预算触发 → 状态机投毒 → 后续调用被拒，宿主稳定。
- [ ] **反向验收 C**：`pcall(function() while true do end end)` → **仍然**熔断（投毒生效），断言「投毒后 `evaluate` 返回拒绝而非继续执行」。
- [ ] **反向验收 D**：分配 17MB → 内存上限触发，确定性异常。
- [ ] **反向验收 E**：把 `poisoned` 标志置位逻辑删掉 → `lua-sandbox-check --teeth` 红。
- [ ] **反向验收 F**：篡改 `manifest.json::sha256` → 拒载且错误含期望/实际两个值。

**领地白名单**

```
user_layer/lua_runtime/**
user_layer/strategies_lua/**
core/blitzkrieg_core/src/strategy_engine/**        # 仅 lua 发现/加载（串行对 S1：E24/E27 之后）
core/blitzkrieg_core/src/cli.rs                    # 仅 +--lua-strategy-dir
scripts/lua-sandbox-check.mjs
docs/rust-core/STRATEGY_GUIDE.md
```

**领地声明**：`Cargo.lock` 与 `mlua` 依赖在 Wave 0 已落地 → 本 Epic **不得**引入任何新依赖（引了就要再改 lock，与全员冲突）。`bk.kline` 依赖 E29 的聚合器 → 本 PR 标注「依赖 E29 合并」，合并顺序在其后。

---

### 14.2 热点文件与监护人制度

| 热点文件 | 为什么是热点 | 规则 |
|:---|:---|:---|
| `core/blitzkrieg_core/src/service.rs` | Core 结构、下单路径、账本持有者 | 每 wave 一个监护人；他人不得直接改；需要接线时在 PR 里附**独立 hunk 片段**（`git diff` 输出），由监护人 rebase 后落地 |
| `core/blitzkrieg_core/src/ipc/schema.rs` | 所有方法/事件的类型声明 | 同上。每个 Epic 只加自己的类型，**不改既有类型的字段拼写** |
| `core/blitzkrieg_core/src/ipc/server.rs` | 方法分发 | 同上。每个 Epic 只加自己的 arm |
| `core/blitzkrieg_core/src/strategy_engine/loader.rs` | 策略加载与可选符号解析 | **串行对 S1**：E24 → E27 → E30 |
| `docs/rust-core/INTERFACES.md` | 契约单一真相 | 按 Wave 顺序追加「版本变更记录」行；同一 wave 内由该 wave 的第一个合并者代理 |

**监护人指派**：

| Wave | `service.rs` | `schema.rs` + `server.rs` | 串行对 |
|:---|:---|:---|:---|
| Wave 1 | E25 | E25 | S1: E24 → E27 |
| Wave 2 | E28 | E28 | — |
| Wave 3 | E30（唯一活跃） | E30 | S1 收尾：E30 |

**监护人不是审批者**：监护人的职责只有两件——把别人的 hunk 落地、保证「最小改动」这条不被稀释（一个热点 PR 里出现无关重构，退回）。

### 14.3 五件套对照表（每个 Epic 必须齐全）

| Epic | API | 文档 | 示例 | 门禁 | UI 呈现 |
|:---|:---|:---|:---|:---|:---|
| E24 | `declare_modes` / 保留键 | `INTERFACES` + `ABI_V2_DESIGN` + `STRATEGY_GUIDE` | Rust `momentum_alpha` | `strategy:no-stop-loss-check` + `strategy:declaration-check` | `Strategies.vue` 声明徽标 |
| E25 | `process_intent` / `Decision` | `ARCHITECTURE` | 由 E24/E30 的示例覆盖 | `intent-audit-check` | `Decisions.vue` + TUI |
| E26 | `AccountRiskLimits` / `apply_physics` | `ARCHITECTURE` | 场景式单测（风暴/击穿） | `risk:systemic-check` | `SettingsPage` 卡片 + 持仓止损位 |
| E27 | `MarketMode` / 握手 | `EXTENSION_GUIDE` | Polymarket 插件自身 | `plugin:modes-check` | `Plugins/Strategies` 徽标 |
| E28 | `Account` / `account.*` | `ARCHITECTURE` + `INTERFACES` | `accounts.toml` 双账户示例 | `account-cross-isolation` + `account-credential` + `account-parity` 扩展 | `AccountSwitcher.vue` |
| E29 | `on_kline` / `kline.*` | `INTERFACES` + `STRATEGY_GUIDE` | Lua 示例消费 K 线 | `kline:aggregation-check` | `KlineChart.vue` |
| E30 | Lua 全部 | `STRATEGY_GUIDE` Lua 章节 | Lua `lua_momentum` | `lua:sandbox-check` | 策略列表显示 `(lua)` |

**示例策略两套**（用户要求的「至少 2 个」）：

| 语言 | 路径 | 声明 | 免止损 | 用途 |
|:---|:---|:---|:---|:---|
| Rust | `user_layer/examples/momentum_alpha/` | `prediction / binary_outcome_wheel` + 2 能力位 | 仅 `entries` + 理由式 `exits` | 证明外挂最小可行策略 |
| Lua | `user_layer/strategies_lua/lua_momentum/` | 同上（走 `bk_declare_modes()`） | 同上 | 证明 Lua 与 Rust 平权 |

两个示例的**完整源码**见附录 A.2。

---

## 15. 执行顺序

### 15.1 关键路径

```
Wave 0  接口冻结节（类型 + 缝 + lock + INTERFACES.md）
   │    ← 阻塞全部：不合并则任何 Epic 不许开分支
   ├────────────────────┬────────────────────┐
   ▼                    ▼                    ▼
Wave 1  E24 API 1.0     E25 裁决流水线        E27 三层声明
   │    （strategy_api）  （arbitration）      （market_api/插件）
   │         │                 │                 │
   │         └────────┬────────┴────────┬────────┘
   │                  ▼                 ▼
Wave 2            E26 系统风控        E28 多账户
   │                                   │
   │                  ┌────────────────┘
   │                  ▼
Wave 3            E29 K 线 ──► E30 Lua 沙箱（E29 后合并）
   │
   ▼
收口   集成验证 + 全门禁矩阵 + RC 清单 + CHANGELOG
```

**关键路径长度**：Wave 0 → E24 → E25 → E26 → 收口。E24 与 E25 在 Wave 1 并行，但 **E25 的 `PhysicsBinding` 依赖 E26 的值**（E25 用「读 `ExitConfig` 的投影」占位，E26 换成真实限额驱动）——所以 E25 必须先于 E26 落地，否则 E26 无处接线。

### 15.2 串行对与合并顺序（**只开 PR 不合并**）

Agent 只开 PR。合并由集成 owner 按此顺序执行，每一步合并后**重跑受影响的门禁**：

| 顺序 | 内容 | 前置 | 为什么这个顺序 |
|:---|:---|:---|:---|
| 1 | **Wave 0 冻结节** | — | 类型与 lock 的唯一变更点 |
| 2 | E24 | 1 | `strategy_api` 与 `loader.rs` 是 S1 的第一位；`INTERFACES.md` 由它开版本记录 |
| 3 | E25 | 2 | 裁决流水线要在 `service.rs` 落缝；Wave 1 监护人 |
| 4 | E27 | 2, 3 | S1 第二位（rebase on E24）；IPC 字段扩展在 E25 之后避免同 wave 二次改 arm |
| 5 | E28 | 3, 4 | Wave 2 监护人；`account_id` 覆盖 E25 的审计字段与 E27 的 `market.list` |
| 6 | E26 | 3, 5 | 风控限额落在 E25 的 Gate 2 上；`risk.limits` 在 E28 的账户句柄之后 |
| 7 | E29 | 4, 5 | K 线喂入点在 E27/E28 之后的 `engine.rs` |
| 8 | E30 | 6, 7 | S1 第三位（rebase on E24/E27）；`bk.kline` 依赖 E29 |
| 9 | 收口 | 全部 | 全门禁矩阵 + RC 清单 |

**串行对汇总**：

| 编号 | 文件 | 顺序 | 理由 |
|:---|:---|:---|:---|
| S1 | `strategy_engine/loader.rs` | E24 → E27 → E30 | 同一函数里加三组可选符号解析；一次只让一个人改 |
| S2 | `core/market_api/src/types.rs` | Wave 0（`MarketType`）→ E27（`PluginInfo`）→ E28（`OrderIntent.account_id`） | 三个改动落在同一个文件的不同结构上，后两位必须 rebase |
| S3 | `ui/webapp/webui/src/{App.vue,api/client.ts}` | E25 → E27 → E28 → E29 | 导航与客户端的单点 |

### 15.3 「零行为变化」的验证姿势（贯穿全部 wave）

每个 wave 合并后跑一遍三条命令，**任何一条红就回退**：

```bash
# 1) 经济结论不变：与 0.2 记录的 BASELINE 比对（不许改 BASELINE）
node scripts/exit-economics-check.mjs

# 2) 账本语义不变：22 条断言
node scripts/core-parity.mjs && node scripts/account-parity.mjs

# 3) 全链不变：确认→下单→成交→持仓→重估→出场
node scripts/cycle-check.mjs
```

另外增加一条**本版本专属**的证明：用 `--no-intent-audit` 跑一遍同一 corpus，与开审计的跑法比经济结论——两者都必须通过 `BASELINE`。这证明「审计是旁路，不在交易路径上」。

### 15.4 worktree 与构建环境

| 项 | 决议 |
|:---|:---|
| worktree 位置 | `/Volumes/Hard Disk/bk-wt-e<NN>`（与仓库同卷，避免 `/tmp` 上的 APFS/权限差异；仓库已有 `/private/tmp/bk-socket-wt` 的前例，说明 worktree 是既有实践） |
| `CARGO_TARGET_DIR` | 每 worktree 用 `<worktree>/target`（即默认）。**不要**共享一个 target 目录：并发 cargo 会互相等待锁，把并行变成串行 |
| 构建范围 | 日常只 `cargo test -p blitzkrieg-core --locked`（快）；**PR 前必须** `cargo build --release --workspace --locked` + `cargo clippy --workspace --all-targets --locked -- -D warnings`（CI 就是这么卡的：`-D warnings`） |
| 提交署名 | 按 `dev-docs/GITHUB_GOVERNANCE.md`：逐次 `git -c user.name=ceer_quant -c user.email=ceer_quant@users.noreply.github.com commit`，**不落地全局 config** |
| PR 模板必填 | ① 领地声明（白名单 + `git diff --name-only` 全文）② 热点文件改动清单（文件/行数/用途）③ 反向验收证据（`--teeth` 原始输出）④ 未完成项与依赖（如「依赖 E29 合并」） |
| 禁止 | Agent 之间不得 `git push` 到别人分支、不得 rebase 他人分支、不得合并自己或他人的 PR |

---

## 16. 验收标准

### 16.1 RC 清单（Release Candidate Checklist）

**构建与测试**

- [ ] `cargo build --release --workspace --locked` 零 warning。
- [ ] `cargo build --release --locked -p blitzkrieg-ui-kit -p blitzkrieg-core -p blitzkrieg-ui-panel` 绿（CI 的第三组构建目标）。
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` 绿。
- [ ] `cargo test --workspace --locked` 绿（含 `--features account-precision` 的既有矩阵）。
- [ ] `(cd user_layer/parity_strategy && cargo build --release --locked)` 与 `(cd user_layer/strategies && cargo build --release --locked)` 绿（两个嵌套 workspace 的 lock 未漂移）。
- [ ] `(cd user_layer/examples && cargo build --release --locked)` 绿（E24 新增的第三个嵌套 workspace）。
- [ ] `node scripts/version-guard.mjs` 绿（0.3.0 全成员继承一致，含 `src-tauri` 双版本）。

**既有门禁全绿**（一个都不能少）

- [ ] `cycle-check` / `core-parity` / `account-parity` / `core-adopt-check` / `shutdown-cleanliness-check` / `parent-monitor-check` / `child-guard-check`
- [ ] `readonly-egress-check` / `core-args-check` / `crash-recovery-check` / `order-recovery-check` / `position-recovery-check`
- [ ] `capacity-check` / `equity-drawdown-check` / `risk-sizing-check` / `observability-check`
- [ ] `ui-eventbus-check` / `ui-kit-gateway-check` / `ui-plugin-check` / `webapp-check` / `tui-demo-check` / `tui-parity`
- [ ] `strategy-devcheck` / `scale-plugins-check` / `feed-scale-check` / `market-plugin-check` / `trade-log-flag-check`
- [ ] `exit-economics-check`（含 `--self-test` 与 `--teeth`）/ `data-backup-check` / `soak-health-check` / `soak-monitor-check`
- [ ] `binary-size-check` / `e14-memory-baseline` / `gateway-*` / `unified-launcher-check` / `core-provenance-check`
- [ ] WebUI 侧 18 个 `check:*`（`npm run check:all`）全绿，其中 `check:parity` 已按 §13.3 更新

**新增门禁全绿**（§16.4 的 9 个）

### 16.2 功能验收

| # | 项 | 判据 |
|:---|:---|:---|
| F1 | API 1.0 兼容 | 0.2 构建的 cdylib 不重编译可加载（`exit-economics` 用的 `spread_arb` 即是这一证明） |
| F2 | 声明与握手 | 4 类时机（扫描/load/enable/CLI 启用）行为与 §8.2 逐条一致 |
| F3 | 裁决留痕 | 每条 intent 一条审计；`gates` 与 `Decision` 自洽 |
| F4 | 系统风控 | 9 项限额各自可触发、可观察（`risk.limits` 的 `source` 正确） |
| F5 | 免止损 | 三保留键丢弃 + warn；门禁静态扫描零命中 |
| F6 | 双栈 | Rust 与 Lua 策略同时启用、同时出 intent |
| F7 | 多账户 | 两账户并行运行，资金/持仓/回撤独立 |
| F8 | K 线 | 9 interval × 全部 symbol 的聚合正确；策略收到闭合 bar；面板画出止损线与裁决点 |

### 16.3 安全验收

| # | 项 | 判据 |
|:---|:---|:---|
| S1 | 策略无凭证 | 全 IPC 响应 + 策略可见数据里 grep 哨兵 → 零命中 |
| S2 | Lua 无逃逸 | `os`/`io`/`debug`/`package`/`require`/`dofile`/`loadfile`/`load`/`collectgarbage`/FFI 全部不可用（逐项断言，不是抽样） |
| S3 | 资源封顶 | 16MB 内存 + 1e6 指令，各自有确定性异常与投毒 |
| S4 | 策略不触碰风控 | 检索策略可见符号面，无任何风控写入口（`risk.setLimits` 只对 IPC 开放，策略无 UDS 客户端） |
| S5 | `--readonly` 结构性保持 | `readonly-egress-check` 绿 + 新增断言「多账户下任一账户都不能在 readonly 模式下下单」 |
| S6 | 账户隔离不被绕过 | 跨账户平仓/查询/结算三类请求全部拒绝 |
| S7 | 审计不可被策略伪造 | 审计记录的 `accountId`/`strategy` 由内核填，不由 intent 载荷取信 |

### 16.4 门禁验收（9 个新增：6 必选 + 3 支撑）

命名对照：契约名 `域:对象-check` ↔ 脚本 `scripts/<域>-<对象>-check.mjs`（`:` 在文件名里不合法，也是既有仓库的命名习惯）。

| # | 契约名 | 脚本 | 断言 | `--teeth` 反向验收（必须红） |
|:--|:---|:---|:---|:---|
| 1 | `strategy:no-stop-loss-check` | `strategy-no-stop-loss-check.mjs` | `user_layer/**` 策略源码零止损计算/监视；intent 载荷零保留键 | 注入 `stop = price * 0.965` → 红 |
| 2 | `strategy:declaration-check` | `strategy-declaration-check.mjs` | 合法 3 例通过；非法 5 例拒绝且错误含 `index`+`got` | 把校验器的 `Empty` 分支改成「当作未声明」→ 红 |
| 3 | `risk:systemic-check` | `risk-systemic-check.mjs` | 风暴/击穿/连亏/冷静期四场景全部按 §16.2 F4 拦截 | Gate 2 恒 `Pass` → 红 |
| 4 | `plugin:modes-check` | `plugin-modes-check.mjs` | 全部内置插件有合法声明；位图与名字双向一致 | `satisfies` 改 `!=` → 红 |
| 5 | `lua:sandbox-check` | `lua-sandbox-check.mjs` | 禁用清单逐项 + 两配额 + 投毒 + 指纹 | 删投毒置位 → 红 |
| 6 | `kline:aggregation-check` | `kline-aggregation-check.mjs` | 10k tick 对拍 + 单调性 + 丢弃计数 | `bucket_open_ms` 向上取整 → 红 |
| 7 | `intent:audit-check`（支撑） | `intent-audit-check.mjs` | 每条 intent 一条记录且自洽 | Gate 1 后短路 → 红 |
| 8 | `account:credential-check`（支撑） | `account-credential-check.mjs` | 哨兵零命中 | 把 `credential_keys` 改成带值 → 红 |
| 9 | `account:isolation-check`（支撑） | `account-cross-isolation-check.mjs` | 三向隔离断言 | 隐式建空账本 → 红 |

**全部 9 个门禁的 `--teeth` 必须接进 CI**（`core-gates` job）。理由见 §11.3：不被 CI 调用的检查等于关掉的检查，这个仓库已经付过一次学费（#280）。

### 16.5 性能验收

| # | 项 | 判据 |
|:---|:---|:---|
| P1 | 经济结论不变 | `exit-economics-check` 在有/无审计两种情形下均通过**既有** `BASELINE`；**不重录 BASELINE** |
| P2 | 裁决开销 | `process_intent` 的 p50/p99 实测写入 `docs/perf/V0_3.md`；判据是「P1 通过」而非人为阈值 |
| P3 | Lua 回调 | 1e6 指令预算下的 p99 单次回调耗时记录；**新增阈值：≤ 5ms**（相对 50ms tick 留 10x 余量）。超出 → 下调指令预算并把实测写入文档 |
| P4 | K 线聚合 | 10k 成交的聚合吞吐与内存上界（`symbols × intervals × sizeof(Kline)`）记录；不设新阈值 |
| P5 | 二进制体积 | `binary-size-check.mjs --verbose` 绿（既有 50MB 目标），并记录 mlua vendored 带来的增量 |
| P6 | 常驻内存 | 既有 `e14-memory-baseline.mjs` 门禁绿（阈值不动）；记录 K 线 + 账户账本带来的增量 |

> P3 是 0.3 **唯一新增的性能阈值**，理由是它同时是安全边界（沙箱不得阻塞撮合主循环）。其余一律复用既有阈值，避免「为了过验收而发明一个宽松的线」。

### 16.6 反向验收总表（改坏实现 → 测试必须红）

**这是本版本最重要的一张表**：它规定了「改坏什么，哪个门禁必须红」。每个 Epic 的 PR 必须附带自己那几行的原始输出。

| Epic | 改坏方式 | 必须红的门禁 | 期望的失败信息（子串） |
|:---|:---|:---|:---|
| E24 | 策略源码写止损 | `strategy:no-stop-loss-check` | `stop-loss logic found` |
| E24 | 声明返回空数组 | `strategy:declaration-check` | `Empty` |
| E25 | 短路 Gate 1 | `intent:audit-check` | `gates[1] missing` |
| E25 | 跳过合法性校验 | `intent:audit-check` | `reached OME` |
| E26 | Gate 2 恒通过 | `risk:systemic-check` | `limit not enforced` |
| E26 | 止损价硬编码 | `risk:systemic-check` | `stop_price mismatch` |
| E26 | `force_exit_sec` 写死 | `risk:systemic-check` | `binding not from config` |
| E27 | `satisfies` 判等取代超集 | `plugin:modes-check` | `superset violated` |
| E27 | `structure: None` 误判相容 | `plugin:modes-check` | `unspecified structure accepted` |
| E28 | 跨账户平仓放行 | `account:isolation-check` | `cross-account close accepted` |
| E28 | 隐式建账本 | `account:isolation-check` | `implicit ledger` |
| E28 | 凭证进响应 | `account:credential-check` | `sentinel leaked` |
| E29 | 向上取整分桶 | `kline:aggregation-check` | `bucket alignment` |
| E29 | 倒挂输入照收 | `kline:aggregation-check` | `monotonicity violated` |
| E30 | 删投毒 | `lua:sandbox-check` | `poison flag not set` |
| E30 | 放宽内存上限 | `lua:sandbox-check` | `memory limit` |
| E30 | 指纹不校验 | `lua:sandbox-check` | `sha256 mismatch not raised` |
---

## 17. 明确不做的事（Out of Scope）

| # | 不做 | 为什么 | 什么时候再说 |
|:--|:---|:---|:---|
| O1 | 改 `exit_policy` / `position` / `sim` / `ome` 的既有行为 | 0.2 已实盘验证；0.3 只加接口 | 永不作为「顺手改」；单独提版本 |
| O2 | 动 `scripts/lib/frozen-corpus.mjs` 的 sha256 与 `scripts/exit-economics-check.mjs` 的 `BASELINE` | 这是「经济结论」的唯一锚，改它等于把结论改成想要的样子 | 只在有**新测量**时重录，且不与 0.3 同 PR |
| O3 | 改任何既有风控默认值（含 `12 / 100 / 15 / 8 / 120 / 180`） | 默认值是 0.2 的实盘结论 | 有 A/B 回放数据时 |
| O4 | 数据迁移脚本 | 迁移是「可能删用户数据」的程序；`serde(default)` 已让旧数据可读 | 真有格式断裂时 |
| O5 | 新增 `mlua` 以外的依赖 | 每一次依赖新增都要过 lock 与供应链 | 独立评估 |
| O6 | 第二套图表库（如 Lightweight Charts） | 面板已有 ECharts 组件族，K 线是 `candlestick` series | 现有库画不动时 |
| O7 | `SpotPanel` / `FuturesPanel` 两个面板 | **基线里不存在这两个组件**（§A.1）。K 线挂到既有 `HftPage`/`Overview` | 真有按市场分面板的需求时 |
| O8 | 一个插件实例并发服务多账户 | 0.3 是插件 : 账户 = 1:1（§9.6） | 0.4+ |
| O9 | 真正的跨资产相关性矩阵 | `max_correlation` 在 0.3 = 「同 asset 同组」（§4.2），零统计假设 | 有足够样本与协方差估计时 |
| O10 | 原生 K 线流接入（交易所 WS K 线） | `KLINE_STREAM` 能力位与通道留位，本地聚合是唯一实现 | 有 venue 明确提供且可测时 |
| O11 | 运行期增删账户 | 账户是部署事实 | 有热配置需求时 |
| O12 | 解密/解冻账户（`frozen → active`） | 风控的收紧动作不该被一条指令撤销（§12.1） | 永不；走配置 + 重启 |
| O13 | 自动改写策略源码（机器进化源码） | 影子进化只改热参数 | 独立课题 |
| O14 | K 线历史持久化到 DB | 0.3 从既有归档/DB 读；常驻只留当前 bar | 有查询压力时 |
| O15 | v1/v2 兼容层 | 既有决策 D-15：v2 是干净切换 | 永不 |
| O16 | 运行期热改新增的 9 个限额 | 日回撤/熔断类阈值热改会造成同日双口径（§4.4） | 有明确需求与口径方案时 |

---

## 18. 风险与对策

| # | 风险 | 触发信号 | 对策 |
|:--|:---|:---|:---|
| R1 | **`mlua` vendored 引入的构建风险**：新 C 代码进构建、`--locked` 漂移、交叉编译变复杂 | CI `rust-check` 在 Wave 0 就红 | Wave 0 先落空壳 + lock（§14.0），把风险前置到最早、最小的一步 |
| R2 | **七个 Epic 并行改同一批热点文件**，rebase 地狱 | 同一文件出现 ≥3 个分支的 hunk | 监护人制度 + 串行对 S1/S2/S3 + Wave 0 预先落缝（§14.2） |
| R3 | **「只加不改」被慢慢侵蚀**：某个 Epic 顺手改了既有逻辑 | 既有门禁变红，或 PR 出现白名单外改动 | 每 wave 三条命令（§15.3）+ PR 必附 `git diff --name-only`；热点 PR 出现无关重构即退回 |
| R4 | **门禁自己会腐化**（写了个永远绿的检查） | `--teeth` 跑不出红 | 9 个门禁**全部**要求 `--teeth` 且接进 CI（§16.4） |
| R5 | **审计成为性能瓶颈**：每 intent 一次 JSONL 写 | P3 或 `exit-economics` 变慢 | 审计用既有 `jsonl::append`（追加写）；`--no-intent-audit` 提供旁路证明（§15.3） |
| R6 | **`INTENT_DECISION` 推送风暴**（毫秒级几千条拒绝） | 面板卡死、socket 背压 | 按 `(strategy, gate, reason)` 折叠 + ≤1 条/秒（§12.3）；折叠只针对推送 |
| R7 | **Lua 沙箱逃逸**（`load`/`string.dump`/协程绕过钩子） | 沙箱门禁有漏项 | 禁用清单**逐项**断言（不是抽样）；不装载 `coroutine`（§6.2）；投毒防 `pcall`（§6.3） |
| R8 | **`account_id` 泄漏到日志/面板** | 日志里出现凭证或跨账户数据 | 哨兵 grep 门禁（§9.4）；`credential_keys` 只存键名 |
| R9 | **倒挂 K 线 / 重复闭合**：策略收到重复信号重复下单 | 同一 bar 出现两次 `is_closed = true` | 「构造上不可能倒挂」（`close_time_ms` 只由 `bucket_open_ms` 决定）+ 反向验收 A/C（§16.6） |
| R10 | **声明校验把「空数组」当「未声明」** → 写错的声明静默生效 | 用户以为声明生效其实没有 | §7.4 写死 `Empty` = 非法；反向验收（§16.6 E24 行） |
| R11 | **兼容性判定的方向搞反**（插件 `structure: None` 被当万能） | 策略在不匹配市场上加载成功 | §7.5 方向性第一句 + 反向验收 C（§16.6 E27 行） |
| R12 | **冻结漂移**：Epic 开工后又改接口 | 两个 Epic 的类型定义不一致 | Wave 0 冻结 + 改动接口需重跑 Wave 0 验收；文档与类型由同一 PR 落地 |
| R13 | **`serde(default)` 变成「静默错误吸收」**：拼错的 `accountId` 被吃成 default | 本该报错的请求被当成 default 账户执行 | 只在**缺字段**时用 default；**字段存在但非法**（空串、未知账户）必须报错（§9.3 的 `get_mut` 显式报错） |
| R14 | **24h soak 只跑示例策略**，没覆盖真实负载 | 集成期才发现问题 | soak 用「Rust + Lua 同时启用 + 面板连接」的组合（E30 验收），并在 Wave 3 收口重跑 |
| R15 | **合并顺序被打破**（先合 E30 后合 E29） | 编译不过或行为不可解释 | §15.2 顺序表作为合并清单；集成 owner 按表执行 |

---

## 19. 禁止事项（硬红线）

1. **禁止修改既有风控判定条件**。`risk.rs` 的 `check` / `check_with_equity` 只能在其后**追加**新增限额判定；既有 `if` 一行不改。
2. **禁止修改既有默认阈值**：`stop_loss_pct = 12` / `take_profit_pct = 100` / `trailing_min_high_pct = 15` / `min_trail_pct = 8` / `force_exit_sec = 120` / `min_time_left_sec = 180` / `max_order_notional` 等一律不动。
3. **禁止修改 `scripts/lib/frozen-corpus.mjs` 的 sha256 常量与 `scripts/exit-economics-check.mjs` 的 `BASELINE`**。
4. **禁止给 `BkStrategyVtable` 追加字段**（`sizeof` 变化会让旧库越界读）。新能力一律走**可选符号**。
5. **禁止 `BK_ABI_VERSION` 改动**（3 会让 0.2 cdylib 全部不可加载）。
6. **禁止引入 v1/v2 兼容层**。
7. **禁止策略侧出现任何止损逻辑**：不算止损价、不监视、不在 `exits` 的 `reason` 里冒充止损、不用 `breaks` 冒充止损。
8. **禁止凭证进入任何 IPC 响应、日志、策略可见数据、`data/` 落盘**。
9. **禁止运行期「解冻」账户**（`frozen/suspended → active`）。
10. **禁止隐式创建账户/账本**（缺账户必须显式报错）。
11. **禁止 `rm -rf` / 通配符删除**：本版本涉及 `data/`（审计、账户配置）与 `user_layer/`（策略包），任何批量清理必须走回收站机制并先备份。**这一条对 Agent 无例外**。
12. **禁止 Agent 合并 PR**：只开 PR；合并由集成 owner 按 §15.2 顺序执行。
13. **禁止 Agent 修改他人分支**（不 push、不 rebase、不 force-push）。
14. **禁止在 `user_layer/strategies` 放 Lua 包**（那是 dylib 扫描点与测量夹具目录，§6.4）。
15. **禁止把 `Kline` 里的价格改成 `f64`**（会被用来判断止损触及，浮点漂移在这里是钱的问题）。
16. **禁止门禁只在本地跑**：新增门禁必须接进 `.github/workflows/ci.yml`（`core-gates` job），否则视为未交付。
17. **禁止 `git config` 落地**：提交用逐次 `-c` 指定署名（`dev-docs/GITHUB_GOVERNANCE.md`）。
18. **禁止在工作目录外操作个人文件**：全部改动静止于仓库路径与 `/Volumes/Hard Disk/bk-wt-e*` worktree。

---

## 20. 版本记录

### 20.1 0.2 → 0.3 衔接总表（新增 / 变更 / 废弃）

| 模块 / 概念 | v0.2 现状（基线事实） | v0.3 变更 | 性质 |
|:---|:---|:---|:---|
| **接口命名** | "C ABI v2" | BlitzkriegStrategy API 1.0；`BK_ABI_VERSION` 仍为 2 | **变更**（语义命名，二进制不变） |
| **可选符号** | 4 个（gate_exemptions / evolvable_knobs / bind_eval_ctx / config_view / settlement_holds） | 新增 `bk_strategy_declare_modes` | **新增** |
| **建议字段** | 三个建议止损字段在本仓库**从未实现** | 列入保留键：解析丢弃 + warn + 门禁静态扫描 | **新增**（封口，非删除） |
| **trait 方法** | `SafeStrategy` 15 个 | 新增 `declare_modes()` / `on_kline()`（均有默认实现） | **新增** |
| **裁决流水线** | 无（下单直接经 `Core::place` → `RiskGate` → `Ledger`） | `process_intent` 四关卡 + `Decision` + 审计 | **新增**（把既有链路显式化） |
| **风控限额** | `RiskConfig` 4 项 + `PositionConfig` 日亏 + `LossBreakers` | 账户级 5 项 + 全局级 4 项（默认 0 = 关闭） | **新增**（默认零行为变化） |
| **生存绑定** | 出场纪律由 `exit_policy` 隐式执行 | `apply_physics` 把既有纪律随单落审计与 UI | **变更**（显式化，不新增触发） |
| **分批阶梯** | 一次平完（`close_ratio = 1.0` 语义） | 投影模式（等价）+ 显式配置模式（opt-in，默认关） | **新增** |
| **策略语言** | 仅 Rust cdylib | Rust cdylib + Lua 5.4 沙箱（平权） | **新增** |
| **策略发现** | `--strategy-dir` 扫 dylib | 加 `--lua-strategy-dir`（默认 `user_layer/strategies_lua`） | **新增** |
| **市场声明** | `MarketType` 4 值；`PluginInfo` 无 structure/capabilities | 三层模型（`MarketType` 7 值 + `MarketStructure` 6 值 + `MarketCapabilities` 10 位）+ 握手 | **新增**（`MarketType` 是**变更**：加 3 变体） |
| **账户模型** | 单账本（`Ledger` 单例） | `AccountId` 一等公民；`AccountLedgers` 按账户隔离 | **新增**（`default` 账户 = 旧行为） |
| **数据结构** | 无 `account_id` | `OrderRequest`/`OrderIntent`/`Position`/`Fill`/`LedgerEntry`/事件加字段 | **新增**（`serde(default)` 兼容旧数据） |
| **K 线** | 无 | `Kline`/`KlineInterval`/`KlineAggregator`/`on_kline`/`kline.*`/`KLINE_UPDATE` | **新增** |
| **IPC** | 1.1，无账户/裁决/K 线 | 加 4 组方法 + 2 个事件；协议版本**不变** | **新增** |
| **UI** | Overview/Hft/Strategies/Evolution/Backtest/Plugins/Settings | 加 `KlineChart`/`AccountSwitcher`/`Decisions`；改 3 个既有页 | **新增 + 变更** |
| **门禁** | 40+ 个 | 新增 9 个（6 必选 + 3 支撑），全部带 `--teeth` 并接 CI | **新增** |
| **依赖** | 无 mlua | 加 `mlua`（lua54 + vendored）+ 新 workspace 成员 | **新增**（本版本唯一依赖新增） |
| **持久化** | order/position/trade 三库无 account 维度 | 行级 `account_id`（读旧 → `default`，读旧写新） | **新增**（不写迁移脚本） |
| **废弃** | — | "C ABI v2" 名称进入废弃期（仅名称，无代码删除） | **废弃** |

### 20.2 变更分类的读法

- **新增**：0.2 的行为**逐字保持**，只是多了可选择使用的东西。判据是「不配置 = 不变化」。
- **变更**：0.2 的行为**可观测地变了**。本版本只有两类允许的变更：① 命名（API 1.0）；② 把隐式纪律变显式（`apply_physics`）。**第三类变更一律拒绝**。
- **废弃**：只废弃**名称**。0.3 不删任何导出符号、不删任何 IPC 方法、不删任何数据字段。

### 20.3 CHANGELOG 条目（`CHANGELOG.md` 追加，格式沿用 Keep a Changelog）

```markdown
## [0.3.0] - 2026-XX-XX

### Added
- **BlitzkriegStrategy API 1.0**（原 C ABI v2 更名，`BK_ABI_VERSION` 仍为 2）：
  新增可选符号 `bk_strategy_declare_modes`；`SafeStrategy` 增 `declare_modes()` /
  `on_kline()`（均有默认实现，0.2 库零改动可加载）。
- **策略建议 / 内核裁决分离**：`process_intent` 四层裁决流水线 + `Decision`
  （Approved/Modified/Rejected）+ 审计 `data/audit/intents.jsonl` +
  `intent.audit.tail` / `INTENT_DECISION`。
- **系统级风控**：账户级 5 项 + 全局级 4 项限额（**默认全部 0 = 关闭**），
  `apply_physics` 把既有出场纪律随单绑定并落审计；`risk.limits` 返回生效值与来源。
- **Lua 5.4 沙箱**（mlua + vendored）：16MB / 1e6 指令配额、投毒语义、`bk.*` 只读 API、
  `strategy.lua + manifest.json + README.md` 分发格式、`--lua-strategy-dir`。
- **市场三层声明**：`MarketType`（+Margin/Earn/Bot）、`MarketStructure`、
  `MarketCapabilities`；插件与策略双向声明 + 加载期兼容性握手。
- **多账户一等公民**：`AccountId` 贯穿订单/持仓/账本/成交/事件；`AccountLedgers` 隔离；
  `account.list` / `account.switch` / `account.status`（只能收紧）。
- **K 线兼容**：`Kline` / `KlineInterval` / `KlineAggregator` / `on_kline` /
  `kline.history|subscribe|unsubscribe` / `KLINE_UPDATE`；面板 `KlineChart.vue`。
- 9 个新门禁，全部带 `--teeth` 反向验收并接入 CI。

### Changed
- 对外命名统一为 BlitzkriegStrategy API 1.0（旧称进入废弃期，符号全部保留）。
- `apply_physics` 使既有出场纪律（止损价 / 时间退出 / 阶梯）**显式可见**：
  写入裁决审计与面板；触发路径未变。

### Deprecated
- "C ABI v2" 名称（仅名称；无代码删除）。

### Not changed（明示）
- `exit_policy` / `position` / `sim` / `ome` 既有行为；所有既有风控默认阈值；
  `frozen-corpus.mjs` 的 sha256；`exit-economics-check.mjs` 的 `BASELINE`。
```

### 20.4 版本号与文件落位

| 项 | 值 |
|:---|:---|
| 版本 | `0.3.0`（写 `Cargo.toml` 的 `[workspace.package].version`，全成员继承；`version-guard.mjs` 守卫） |
| 本文档 | `docs/DEV_V0_3.md`（本文件） |
| 契约冻结物 | `docs/rust-core/INTERFACES.md`（IPC 1.1 增量）、`docs/rust-core/ABI_V2_DESIGN.md`（API 1.0 增补） |
| 指导文档 | `docs/rust-core/STRATEGY_GUIDE.md`（Lua + 免止损）、`docs/rust-core/EXTENSION_GUIDE.md`（三层声明）、`docs/rust-core/ARCHITECTURE.md`（多账户 + 风控） |
| 性能记录 | `docs/perf/V0_3.md`（P2/P3/P4 的实测值——**记录**，不是新阈值） |

---

## 21. 设计哲学

1. **永远不相信策略代码**。策略可能判断错、可能死循环、可能返回垃圾、可能被机器改坏。内核的职责不是祈祷策略写对，而是让**策略写错时系统仍然活着**。这不是不信任作者，而是承认「人会犯错」这条物理事实。

2. **重力法则高于商业逻辑**。市场不会因为你的信号漂亮就放过你。止损、资金隔离、账户边界是重力，不是策略可以商量的选项。给策略一个「建议止损」的字段，就是给重力开了一个讨价还价的窗口。

3. **接口冷酷，实现极简**。每加一个字段都要过一道审问：**这件事内核能不能自己算出来？** 能算的就不该由策略传。`suggested_stop_loss` 这类字段之所以不该存在，不是因为危险，而是因为它把「内核已经知道的事」变成了「策略可以改的事」。

4. **一个真相**。阈值只有一个定义处、错误码只有一套词汇、账本只有一个权威、止损只有一个归属。出现第二个真相的地方，就是未来事故的现场——所以 Gate 2 调用 `RiskGate` 而不是复制它的判断，所以面板读 `GateTrace.detail` 而不是自己编文案。

5. **默认即安全，配置才收紧**。所有新增限额默认 `0 = 关闭`，出厂行为一字不变；要更强的保护必须有人明确地写下数字。这样「0.3 比 0.2 更安全」永远成立，而「0.3 比 0.2 更激进的配置」永远是某个人显式做的决定。

6. **可复现胜过聪明**。f64 换成 Decimal、UTC 日而不是本地日、`close_time_ms` 由构造保证而不是由调用方提供、`--locked` 的 lockfile 而不是「差不多能编」——每一条都是把「聪明」换成「可复现」。在钱的系统里，可复现的笨比聪明的漂移值钱。

7. **门禁必须会红**。一个不会失败的检查是装饰品。`--teeth` 不是额外工作，是门禁的**定义**：先证明它能在坏实现上红，再让它守卫好实现。

8. **删掉的东西比加上的东西更能说明设计**。0.3 里最重的一笔改动不是新增了什么，而是决定策略**不再拥有**止损。让一个组件少管一件事，比让它多管一件事需要更多勇气，也更值得。

9. **不推迟，也不夹带**。范围写在 §1.1 的六条里，就六条；不塞进「顺手改的」第七、第八条。夹带的范围会带来夹带的风险，而风险从不写在 PR 描述里。

---

## 附录 A

### A.1 事实更正清单（旧稿的说法 → 代码事实）

本表逐条列出「草稿里写过、但代码里不是这样」的说法，防止它被继续引用。**下文列出的每一行都是本文档对旧稿的作废**。

| # | 旧稿说法 | 代码事实 | 本文档的处置 |
|:--|:---|:---|:---|
| A.1.1 | `suggested_stop_loss` / `suggested_take_profit` / `suggested_max_hold_sec` 已被实现，需「移除」 | 三个字符串在仓库源码中**不存在**（只有旧稿与 issue 脚本提过） | 改为**封口决议**（§2.4）：保留键 + 解析丢弃 + warn + 静态扫描门禁 |
| A.1.2 | 扩展声明字段 `MarketStructure` / `MarketCapability`（单数） | 无任何 `MarketStructure` / `Capability` 定义；`PluginInfo` 只有 `market_type` | 新建 `MarketStructure` / `MarketCapabilities`（§7.3），落 `market_api/src/modes.rs` |
| A.1.3 | 需「引入 bitflags」 | 无 crate 直接依赖 `bitflags`（仅是传递依赖） | 手写 `u64` 位图 + `satisfies`，**不新增依赖**（§7.3） |
| A.1.4 | `StrategyMode` 的 serde 用 `snake_case`（含 `market_type` / `required_capabilities` 等键） | `MarketType` 的既有 wire 是 `snake_case`；但 workspace 默认无 `serde(rename_all)` 统一约定，plugin/IPC 侧多用 `camelCase` | 明确：**枚举变体 wire = `snake_case`**（与既有 `MarketType`/`Side`/`OrderType` 一致）；**结构体字段 wire = `camelCase`**（与 `PluginInfo`/IPC 一致）。两处都写进类型上的 `#[serde(...)]`，不靠默认（§7.3） |
| A.1.5 | 插件 `info()` 里「结构体字面量 `market_type:` 会直接失效」，需 adapter 层 | `PluginInfo.market_type` 是既有字段，0.3 只**追加**新字段，既有构造点零改动 | 撤销 adapter 方案：只加字段，不加层（§11.2 的 `[MOD]` 行） |
| A.1.6 | `strategy_api/src/modes.rs` 新增 166 行 | 该文件不存在；`strategy_api` 是 `pub mod safe;` + 顶层常量 | 新建但**只放 `StrategyMode`**（共享类型在 `market_api`，避免 strategy_api 依赖 core）——§2.5 |
| A.1.7 | `MarketSnapshot` | 基线中不存在该类型（grep 零命中） | 不说「给 MarketSnapshot 加 account_id」，改说「回合上下文类结构加 `account_id`」（§9.2 第 6 行） |
| A.1.8 | 需「引入 `bitflags` 用于 capability 位图」/ 依赖需 `--locked` 同步 | 同 A.1.3；lock 同步的要求成立且必须遵守（CI 用 `--locked`） | 保留 lock 纪律（§14.0），撤销 bitflags |
| A.1.9 | 前端图表「基于 Lightweight Charts」 | 面板为 ECharts（`echarts ^6.1.0` 已是依赖）；**也没有** `SpotPanel` / `FuturesPanel` | 用 ECharts `candlestick`；挂 `HftPage` / `Overview`（§13.1–13.2） |
| A.1.10 | K 线事件写 `kline.update` | 既有事件 kind 是 `SCREAMING_SNAKE_CASE`（`READY`/`ORDER_UPDATE`/`POSITION_CLOSED`/`RISK_ALERT`…） | 事件 kind 用 `KLINE_UPDATE`；**方法名**保留小写点分（`kline.history`），与既有 `orders.place` 同形（§12.2） |
| A.1.11 | 「所有 4 个 CI job 全绿」 | 基线 CI 有 **8 个** job（`rust-check` / `panel-check` / `core-gates` / `exit-economics` / `release-bundle` / `secret-scan` / `ops-gates` / `notify`） | 验收写「8 个 job 全绿」，新门禁接进 `core-gates`（§16.4） |
| A.1.12 | 八个 issue 由脚本发布的编号就是交付编号 | 旧发布脚本 `scripts/publish-v0.3-issues.sh` 按其旧草稿的错误文本计划发布 8 个 issue（含上述 A.1.1/2/3/9），**该脚本已于 2026-09-25 删除** | 本文档为唯一设计真相；issue 按本文档直接创建（不再经脚本），**编号 E24–E30 保留**（§14.1） |
| A.1.13 | 「多账户 = 一个插件服务多账户」的读法 | 0.3 实现是插件 : 账户 = 1:1 | 显式写明边界（§9.6） |
| A.1.14 | `apply_physics` 需「实现 FOK 市价强制平仓」等**新触发路径** | 既有平仓走 `positions.exit` / `exit_policy` 触发链；新增触发路径违反 §1.3 | `apply_physics` 改为**只绑定与记录**（§4.3），不新增触发 |
| A.1.15 | K 线的 9 个 interval 用「Sec1/Sec5/…/Day1」枚举字面量 | 枚举名可用，但既有 wire 惯例是小写 | 枚举变体 `Sec1` 等，wire `sec1` 等（§10.2 的 `snake_case`） |

### A.2 两个示例策略（完整可用源码）

#### A.2.1 Rust 示例：`user_layer/examples/momentum_alpha/`

```
user_layer/examples/
├── Cargo.toml          # [workspace] members = ["momentum_alpha"]  ← 根 Cargo.toml 的 exclude 需 +1 行
└── momentum_alpha/
    ├── Cargo.toml
    └── src/lib.rs
```

`user_layer/examples/momentum_alpha/Cargo.toml`：

```toml
[package]
name = "momentum-alpha-strategy"
version = "0.1.0"
edition = "2024"
description = "Reference external strategy for BlitzkriegStrategy API 1.0 (Rust)."

[lib]
name = "momentum_alpha_strategy"
path = "src/lib.rs"
crate-type = ["cdylib"]

[dependencies]
# 与真实第三人称作者一样：只依赖冻结的接口 crate，不依赖内核。
blitzkrieg-strategy-api = { path = "../../strategy_api" }
rust_decimal = "1"
serde_json = "1"
```

`src/lib.rs`（**可编译、无止损、带声明**）：

```rust
//! momentum_alpha —— BlitzkriegStrategy API 1.0 参考策略（Rust 原生）。
//!
//! 职责边界（读 STRATEGY_GUIDE 的「止损不归你管」章节）：
//!   * 本策略只判断**何时进场**与**何时离场**，并给出理由字符串；
//!   * 不计算止损、不监视止损、不表达止损——生存由内核绑定并执行；
//!   * 仓位只给"建议比例"，内核裁决绝对股数。

use blitzkrieg_strategy_api::{
    dec, export_strategy, BookUpdate, Entry, Exit, FreshBook, Intents, RoundContext,
    SafeStrategy, StrategyMode,
};
use blitzkrieg_strategy_api::MarketType; // 由 strategy_api 重导出（见 §11.2 依赖方向）
use blitzkrieg_strategy_api::MarketStructure;
use rust_decimal::Decimal;

#[derive(Default)]
struct MomentumAlpha {
    /// 已确认上一轮出现过的动量标的（回合内记忆）。
    seen: Vec<String>,
    /// 本次回调里观察到的中间价（用于比较相邻两笔）。
    last_mid: Option<Decimal>,
}

impl SafeStrategy for MomentumAlpha {
    fn name(&self) -> &str {
        "momentum_alpha"
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    // ── 模式声明（API 1.0 新增；不声明则不参与兼容性校验）────────────────────
    fn declare_modes(&self) -> Vec<StrategyMode> {
        vec![StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            // 要求的每一项，插件都必须具备（超集判定，§7.5）
            required_capabilities: blitzkrieg_strategy_api::MarketCapabilities::WEBSOCKET_FEED
                | blitzkrieg_strategy_api::MarketCapabilities::LEVEL2_SNAPSHOT,
        }]
    }

    fn on_book(&mut self, update: &BookUpdate) {
        // 只记中间价；价格一律走 dec()（精确 decimal），绝不经 f64。
        if let Some(mid) = dec(&update.mid) {
            self.last_mid = Some(mid);
        }
    }

    fn on_eval_books(&mut self, _books: &[FreshBook]) {
        // 本策略只信 evaluate 时传入的 ctx；这里不做事。
    }

    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut out = Intents::none();
        let Some(mid) = self.last_mid else { return out };

        for m in &ctx.markets {
            // 回合剩余时间太短就放弃入场（把"该不该在这里交易"留给内核，
            // 但"还值不值得开新仓"是策略自己的判断）。
            if ctx.round.time_left_sec < 240 {
                continue;
            }
            // 动量条件：中间价在 [0.35, 0.62] 区间的"上半段"表明向上动量。
            let want = mid >= Decimal::new(35, 2) && mid <= Decimal::new(62, 2);
            if !want || self.seen.contains(&m.up_token) {
                continue;
            }

            self.seen.push(m.up_token.clone());
            out.entries.push(Entry {
                token: m.up_token.clone(),
                // 限价：以中间价下沿报价，具体能否成交由内核按盘口裁决。
                price: format!("{mid}"),
                reason: "momentum_alpha: mid in momentum band, round has time left".into(),
                // 不给绝对股数 → 内核按账户净值与风控上限定寸。
                shares: None,
            });
        }
        out
    }

    fn take_breaks(&mut self) -> Vec<blitzkrieg_strategy_api::Break> {
        Vec::new()
    }

    /// 离场建议：**只表达"理由消失"**，不表达止损、不表达价格。
    /// 内核会把它与自身的出场纪律（止损/时间退出/阶梯）合并裁决。
    fn confirmed_tokens(&self) -> Vec<String> {
        self.seen.clone()
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "seen": self.seen.len(),
            "last_mid": self.last_mid.map(|d| d.to_string()),
        })]
    }
}

// 生成全部 ABI 表面：vtable、bk_strategy_create / abi_version / free_string /
// declare_modes（本策略声明非空 → 宏会导出真实载荷）。
export_strategy!(MomentumAlpha);
```

> **编译前置**：`strategy_api` 的 `lib.rs` 需要 `pub use blitzkrieg_market_api::{MarketType, MarketStructure, MarketCapabilities};`（§11.2 记录的唯一新增依赖边）。若该重导出在编码时尚未存在，示例用 `blitzkrieg_market_api` 直接引用即可——**两处一致即可，不必两处都有**。

**该示例的免止损自证**：全文无 `stop` / `stoploss` / `止损` 字样（`strategy:no-stop-loss-check` 的扫描对象）。`exits` 只通过 `confirmed_tokens()` / `take_breaks()` 表达"理由消失"。

#### A.2.2 Lua 示例：`user_layer/strategies_lua/lua_momentum/`

`strategy.lua`：

```lua
-- lua_momentum —— BlitzkriegStrategy API 1.0 参考策略（Lua 5.4 沙箱）。
--
-- 职责边界：只判断进场/离场理由；不计算、不监视、不表达止损。
-- 只使用沙箱允许的标准库（math / string / table）与 bk.* 只读接口。

local S = {
    band_lo     = 0.35,   -- 动量带宽下沿
    band_hi     = 0.62,   -- 动量带宽上沿
    size_ratio  = 0.20,   -- 建议仓位比例；内核裁决绝对股数
    min_left    = 240,    -- 剩余时间少于该秒数则不开新仓
    last_mid    = nil,
    seen        = {},
    seen_n      = 0,
    kline_state = nil,    -- on_kline 写入的最近一根闭合 bar 的收盘价
}

-- 模式声明（可选；缺省 = 不参与兼容性校验）
function bk_declare_modes()
    return {
        { market_type  = "prediction",
          structure    = "binary_outcome_wheel",
          capabilities = { "websocket_feed", "level2_snapshot" } },
    }
end

-- 热参数（影子进化 / 宿主配置注入；值为字符串，与 Rust ParamBag 同源）
local function refresh_params()
    local p = bk.params()
    if not p then return end
    if p.band_lo    then S.band_lo    = tonumber(p.band_lo)    or S.band_lo    end
    if p.band_hi    then S.band_hi    = tonumber(p.band_hi)    or S.band_hi    end
    if p.size_ratio then S.size_ratio = tonumber(p.size_ratio) or S.size_ratio end
    if p.min_left   then S.min_left   = tonumber(p.min_left)   or S.min_left   end
end

function bk_on_book(update)
    -- update.mid 是十进制字符串；用 tonumber 只为比较，落单时仍回传字符串。
    if update and update.mid then S.last_mid = tonumber(update.mid) end
end

-- on_kline 只收到 is_closed == true 的 bar（宿主契约，§10.4）
function bk_on_kline(kline)
    if kline and kline.is_closed then
        S.kline_state = tonumber(kline.close)
    end
end

function bk_evaluate()
    refresh_params()

    local round = bk.round()
    local entries, exits = {}, {}
    if not round then return { entries = entries, exits = exits } end
    if round.time_left_sec < S.min_left then
        return { entries = entries, exits = exits }
    end

    for _, m in ipairs(bk.markets() or {}) do
        local book = bk.book(m.up_token)
        -- 只信宿主给出 fresh = true 的盘口（与 Rust 侧 on_eval_books 同一门）
        if book and book.fresh and book.mid then
            local mid = tonumber(book.mid)
            if mid and mid >= S.band_lo and mid <= S.band_hi
               and not S.seen[m.up_token] then
                S.seen[m.up_token] = true
                S.seen_n = S.seen_n + 1
                table.insert(entries, {
                    token       = m.up_token,
                    -- 十进制字符串，不做算术拼接（避免浮点漂移）
                    price       = book.mid,
                    size_ratio  = string.format("%.2f", S.size_ratio),
                    reason      = "lua_momentum: mid in momentum band, round has time left",
                    -- 无止损字段：内核统一绑定（免止损，§5）
                })
            end
        end
    end

    return { entries = entries, exits = exits }
end
```

`manifest.json`：

```json
{
  "name": "lua_momentum",
  "version": "1.0.0",
  "api": "1.0",
  "entry": "strategy.lua",
  "sha256": "PLACEHOLDER_REPLACE_WITH_SHA256_OF_strategy.lua",
  "author": "ceer-quant",
  "description": "Lua 5.4 reference strategy: mid-price momentum band entry on binary outcome wheels.",
  "tunables": {
    "band_lo":    { "type": "decimal", "default": "0.35" },
    "band_hi":    { "type": "decimal", "default": "0.62" },
    "size_ratio": { "type": "decimal", "default": "0.20" },
    "min_left":   { "type": "decimal", "default": "240" }
  }
}
```

> `sha256` 必须**实际计算**后写回（`shasum -a 256 strategy.lua`）。占位符留在仓库里 = 加载必被拒（这正是 E30 反向验收 F 要证明的路径）。

`README.md`（必需，供作者可读）：

```markdown
# lua_momentum（Lua 5.4 参考策略）

## 它做什么
在二元结果轮盘市场（`prediction` / `binary_outcome_wheel`）上，当某侧 token 的中间价落入
`[band_lo, band_hi]` 且本回合剩余时间 ≥ `min_left` 秒时，提交一笔买入建议。

## 它不做什么
- 不计算或表达止损。止损由内核绑定（见 `docs/rust-core/STRATEGY_GUIDE.md`「止损不归你管」）。
- 不给绝对股数，只给 `size_ratio` 建议；内核按账户净值与风控上限裁决。
- 不使用任何沙箱禁用能力（`os`/`io`/`debug`/`package`/`require`/`load` 等）。

## 参数
| 名 | 默认 | 含义 |
|:---|:---|:---|
| `band_lo` | `0.35` | 动量带宽下沿（中间价） |
| `band_hi` | `0.62` | 动量带宽上沿（中间价） |
| `size_ratio` | `0.20` | 建议仓位比例（内核裁决绝对股数） |
| `min_left` | `240` | 剩余时间低于该秒数则不开新仓 |

## 运行
```bash
blitzkrieg run --lua-strategy-dir user_layer/strategies_lua --enable-strategy lua_momentum
```
```

### A.3 相邻文档的落地清单（每个 Epic 的文档义务）

| 文档 | 落地内容 | 责任 Epic |
|:---|:---|:---|
| `docs/rust-core/INTERFACES.md` | §12 的增量 + 「版本变更记录」追加 0.3 行（账户/裁决/K 线/模式） | E24 开设，E25/E27/E28/E29 各追加自己那行 |
| `docs/rust-core/ABI_V2_DESIGN.md` | API 1.0 命名；保留键；`bk_strategy_declare_modes` 规格；v0.3 增补节 | E24 |
| `docs/rust-core/STRATEGY_GUIDE.md` | 「止损不归你管」章节（§5.2）+ Rust/Lua 示例索引 + Lua 章节（分发格式/`bk.*`/配额/投毒）+ `on_kline` 用法 | E24（章节）+ E30（Lua） |
| `docs/rust-core/EXTENSION_GUIDE.md` | 三层声明 + 兼容性方向性（§7.5 第一句）+ `declare_modes()` 例 | E27 |
| `docs/rust-core/ARCHITECTURE.md` | 裁决流水线章节（含「不重复判定」纪律）+ 多账户章节 + 系统风控章节 | E25 / E28 / E26 |
| `docs/perf/V0_3.md` | P2/P3/P4 实测值（记录，非阈值）+ mlua 体积增量 | E25 / E26 / E30 / 收口 |
| `CHANGELOG.md` | §20.3 的条目 | 收口 |
| `scripts/README.md` | 9 个新门禁进「Acceptance gates」表（含 `--teeth` 说明） | 各 Epic；收口复核 |

### A.4 门禁接线（CI 落地片段）

`.github/workflows/ci.yml` 的 `core-gates` job 追加（**照既有写法，先 `--teeth` 自证再正跑**）：

```yaml
      # v0.3.0 新增门禁：先证明它会红，再让它守卫。
      - name: v0.3 gates — reverse acceptance (--teeth)
        run: |
          node scripts/strategy-no-stop-loss-check.mjs --teeth
          node scripts/strategy-declaration-check.mjs --teeth
          node scripts/risk-systemic-check.mjs --teeth
          node scripts/plugin-modes-check.mjs --teeth
          node scripts/lua-sandbox-check.mjs --teeth
          node scripts/kline-aggregation-check.mjs --teeth
          node scripts/intent-audit-check.mjs --teeth
          node scripts/account-credential-check.mjs --teeth
          node scripts/account-cross-isolation-check.mjs --teeth
      - name: v0.3 gates — the real verdicts
        run: |
          node scripts/strategy-no-stop-loss-check.mjs
          node scripts/strategy-declaration-check.mjs
          node scripts/risk-systemic-check.mjs
          node scripts/plugin-modes-check.mjs
          node scripts/lua-sandbox-check.mjs
          node scripts/kline-aggregation-check.mjs
          node scripts/intent-audit-check.mjs
          node scripts/account-credential-check.mjs
          node scripts/account-cross-isolation-check.mjs
```

**每条新门禁的实现骨架**（统一形状，零依赖裸 Node）：

```
1. 解析 argv：识别 --teeth / --self-test
2. --self-test：用夹具验证判定逻辑（无需二进制）—— 照 exit-economics 的既有模式
3. --teeth：把"坏实现"作为输入喂给判定逻辑，**期望判定为失败**；全绿则本地通过
4. 正常模式：真实内核（child-guard.mjs spawn）+ UDS JSON-RPC，产出判定
5. 退出码：0 通过 / 1 判定失败 / 2 环境缺失（如二进制未构建，与既有脚本同约定）
```

---

## 附录 B：本文档与其他文档的关系

| 文档 | 关系 |
|:---|:---|
| `docs/rust-core/INTERFACES.md` | 本文 §12 是它的 0.3 增量来源；两者冲突时**以 INTERFACES.md 为准**（它是契约单一真相） |
| `docs/rust-core/ABI_V2_DESIGN.md` | 本文 §2 是它的 0.3 增补；线协议细节以它为准 |
| `dev-docs/DECISIONS_PENDING.md` | 既有决策（D-15 v2 干净切换、D-20 `panic = "unwind"`、D-31 timing floor）在本版本继续有效；本文不覆盖它们 |
| `dev-docs/GITHUB_GOVERNANCE.md` | 提交署名、PR、分支规范以它为准（本文 §15.4 只是引用） |
| `dev-docs/DEVELOPMENT.md` | 本地门禁矩阵以它为准 |
| `docs/VERSIONING.md` | 版本单事实来源与 `version-guard.mjs` 以它为准 |
| `scripts/README.md` | 门禁清单以它为准（本文 §16.4 的新门禁需同步进去） |

**冲突解决顺序**：契约文件（`INTERFACES.md` / `ABI_V2_DESIGN.md`）> 本文 > 其他文档。本文与契约冲突时，说明**契约先改**，本文随后跟进——不允许反过来（先改文档再改契约）。
