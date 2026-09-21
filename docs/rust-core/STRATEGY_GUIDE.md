# 用户层策略编写指南（STRATEGY_GUIDE）

> **一句话**：你写策略"逻辑"，内核执行"交易"。
> 策略逻辑只接收行情，返回一个信号；其余（校验、风控、资金、签名、下单、对账、平仓）全部由内核负责。

## 1. 边界（硬性）

策略逻辑文件**绝不能**包含：

| 禁止 | 原因 |
|:---|:---|
| 私钥、API Key/Secret | 凭证只在核心里 |
| 签名代码 | 只有内核能签名 |
| 网络请求（HTTP/WS） | 策略不直接接触交易所 |
| 下单/撤单/查单 | 由内核的 OME 负责 |
| 文件系统读写 | 策略应为纯函数 |

允许：读取内核推送的行情、做数学/统计判断、返回 `Signal`。

自检：
```bash
grep -r "POLYMARKET_PRIVATE_KEY" user_layer/strategies/   # 应为空
grep -rE "reqwest|hyper|http" user_layer/strategies/      # 应为空
```

## 1.5 冻结的 C ABI v2（动态库）

用户层策略以共享库（`.dylib`/`.so`/`.dll`）交付时，走 **冻结的 C ABI v2**（crate `blitzkrieg-strategy-api`，`#[repr(C)]`，无 Rust 特有类型）。v2 是**全功能**契约：外挂策略与内建策略实现同一个 `EngineStrategy`，「外挂」只是加载方式不同。设计全文见 [ABI_V2_DESIGN.md](ABI_V2_DESIGN.md)。

```c
uint32_t bk_strategy_abi_version(void);           // 必须 == 2（协商先于读 vtable）
const BkStrategyVtable* bk_strategy_create(void); // 工厂（必须）
void bk_strategy_free_string(char* p);            // 释放本库产出的 JSON（同一分配器）

// 可选（按名字解析；未导出 = 不声明任何豁免，见 §3.5）：
char* bk_strategy_gate_exemptions(void* handle);  // {"timing":bool,"momentum":bool}

// 可选（按名字解析；未导出 = 明确「不可进化」，见 §3.6）：
char* bk_strategy_evolvable_knobs(void* handle);  // {"knobs":[{name,value,min,max}]}
```
```c
typedef struct { const char* price; const char* size; } BkLevel;
typedef struct {
    const char* symbol; const char* asset;
    const BkLevel* bids; size_t bid_count;
    const BkLevel* asks; size_t ask_count;
    const char *best_bid, *best_ask, *mid, *bid_depth, *ask_depth,
               *obi, *spread, *spread_pct;
    int64_t timestamp_ms;
} BkBookView;
typedef struct {
    const char* name; const char* version;
    uint32_t abi_version; uint32_t min_abi;
    BkHandle (*create)(void); void (*destroy)(BkHandle);
    void (*on_book)(BkHandle, const BkBookView*);
    void (*on_round)(BkHandle, const BkRound*);
    char* (*evaluate)(BkHandle, const BkRoundView*);
    char* (*confirmed_tokens)(BkHandle);
    char* (*take_breaks)(BkHandle);
    char* (*diagnostics)(BkHandle);
    int32_t (*on_config)(BkHandle, const char* json);
    int32_t (*on_hot_params)(BkHandle, const char* json);
    char* (*knobs)(BkHandle);
} BkStrategyVtable;
```

`evaluate` 返回由**本库分配**的 JSON 字符串（`bk_string_out`），结构为
`{"entries":[{"token","price","reason"}], "exits":[{"token","reason"}], "breaks":[{"token","broken_price"}]}`，
内核复制后用本库的 `bk_strategy_free_string` 归还——分配器不跨边界混用。

规则（**必须**）：
- 借入的字符串/数组/视图**仅在调用期间有效**；策略需要保留就自己复制。
- 价格/数量一律用**十进制字符串**（避免浮点漂移）。
- 入场**不带张数**（内核 `compute_shares` 定张数），出场**不带价格**（内核按盘口定价）。
- 策略拥有自己的 `handle`；内核不直接释放，只在卸载时调用 `destroy`。
- 策略**禁止** I/O、凭据、网络、OME/UDS——边界上只传只读盘口/回合视图与意图数据。
- 协商顺序固定：路径策略 → dlopen → `bk_strategy_abi_version()`（必须为 2，**无 v1 兼容层**）→ vtable 校验；v1 库在版本步即被拒绝。
- 变更结构体/vtable **必须**递增 `BK_ABI_VERSION`。

两个可直接参考的真实外挂：
- `user_layer/strategies/dog_strategy.rs`（疯狗策略）：
  ```bash
  cd user_layer/strategies && cargo build --release   # → target/release/libdog_strategy.{dylib,so}
  ```
- `user_layer/parity_strategy/parity_strategy.rs`（对拍策略，与内树路径共用 `parity_logic`）。

## 2. 契约

内核只有**一个**全功能策略 trait `EngineStrategy`（内建与外挂共用）：

```rust
pub trait EngineStrategy: Send + Sync {
    fn name(&self) -> &str;
    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64);
    fn on_round(&mut self, slot: i64);

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal>;

    // 以下均为可选（有默认实现）：
    fn gate_exemptions(&self) -> GateExemptions;                  // 入场闸门豁免（E2-b，默认全保留，见 §3.5）
    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent>;   // 平仓意图
    fn take_breaks(&mut self) -> Vec<(String, Decimal)>;          // 趋势破位
    fn confirmed_tokens(&self) -> HashSet<String>;                // 自证
    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<Value>;   // 诊断
    fn evolvable_knobs(&self) -> Vec<KnobSpec>;                   // 自证可进化旋钮 + 取值域（E2-c，默认不声明=不可进化，见 §3.6）
    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>>;   // 造自己的影子孪生（E2-c，默认无）
    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>); // 挂/摘本策略的热参覆盖层（E2-c）
    fn on_config(&mut self, trend: &TrendConfig, arb: &SpreadArbConfig); // 配置
}
```

`set_hot_params(None)` 表示**摘除**覆盖层（进化关闭）——此时策略读不到任何热参句柄，
行为退回 `on_config` 下发的配置，因此「关闭进化 ⇒ 与改动前逐位一致」是可证明的。
`ParamRegistry` 按策略命名，策略只应读自己那一格（`handle_for(name)`）。

`StrategyCtx` 提供本轮 `markets`、`round_slot/time_left/now`，以及按 token 取**仍新鲜**的盘口 `fresh_book(token)`。
策略返回的全部是**候选/意图**：内核随后统一执行回合时序、现货动量、单 token 去重、定张数、风控/资金/签名/下单。
平仓意图经 `ExitReason::StrategySignal` 路由，即使 `auto_exits_enabled=false` 也会被处理（类似手动平仓），但仍受 kill switch / 风控 / 去重 / 持仓存在性约束。

## 3. 三种形态

### 3.1 声明式（TOML，推荐给非程序员）
`user_layer/strategies/trend_strategy.toml`：
```toml
[meta]
name = "trend_follow"
template = "spread_arb"
[params]
trend_min_price = 0.55
trend_entry_factor = 0.98
trend_max_entry_price = 0.45
[risk]
size_usd = 2.5
max_positions = 2
```
你只填参数，内核套用内建模板。TOML 天然无副作用，最安全。

### 3.2 Rust 动态库（v2，全功能）

#### 3.2.1 推荐上手：模板（E9-a，零 unsafe）

一条命令生成完整可构建的策略 crate（含业务骨架、cargo test 冒烟、README）：

```bash
node scripts/blitzkrieg-new-strategy.mjs my_dip_fade
# → user_layer/strategies/my_dip_fade/{src/lib.rs, Cargo.toml, README.md}
```

生成的 `src/lib.rs` 就是你写的**全部**代码——业务开发者只改三个点，全程无
`unsafe`（vtable、`#[no_mangle]` 导出、JSON 包装由 `export_strategy!` 宏从
`SafeStrategy` trait 自动生成）：

1. `on_book(update)` — 观察：缓存你以后要用来决策的盘口（mid/深度/obi/价差）。
   数值以**十进制字符串**给出，用 `dec()` 精确解析——模板里没有 f64 价格路径，
   与内核逐位一致；
2. `evaluate(ctx)` — 决策：产出 `Intents{entries, exits, breaks}`。entry 带
   LIMIT 价格（内核校验、定仓、提交），exit 不带价格（内核按实时盘口定价），
   一切照旧过内核风险门禁——策略永远拿不到签名器/socket/凭证；
3. `on_params` / `evolvable_knobs` — 调参：宿主配置与影子进化的热参数都走
   这里（返回 false 拒绝整包）；声明 knobs 即被影子进化机制纳入。

两个**可选**钩子对齐树内能力（缺失 = 未声明，不影响加载）：

- `on_eval_books(&[FreshBook])` — 在每次 evaluate/diagnostics 之前收到宿主装订
  的「轮次视图 + 仅可定价盘口」，`fresh` 标志与树内 `StrategyCtx::fresh_book`
  同一裁决（非空且未过期；过期/缺失不可区分）。用它做入场前的新鲜度门；
- 覆写 `config_view`（导出 `bk_strategy_config_view` 符号）— 返回任意 JSON 描述
  「当前生效配置」（热参叠加后的实际值），宿主经 `strategy_config_views()`
  展示。树内 `spread_arb_view` 的泛化，外挂与内建能力对等。

改完**一条命令全链验证**（真实内核在环，全部走沙盒 dry core）：

```bash
node scripts/strategy-devcheck.mjs my_dip_fade
# 生成→编译→strategy.load→断言注册即禁用→enable→engine.book 假行情→
# 断言 engine.stats 上 ordersPlaced≥1→断言可演化 knobs 注册→零 panic
```

这个门禁同时实测「从空目录到内核里跑起来 < 5 分钟」的承诺（本机 ~9s）。
手动进内核后再看效果：`bash scripts/tui-demo.sh` 打开面板，Plugins 页能看到你的策略行
（禁用态），`: strategy.load <dylib路径>` 加载、↑/↓ 选中回车启用。

模板的三条硬规则也是所有外挂策略的通用规则：**新策略永远默认禁用**（升级
不改变正在运行的会话在交易什么）；**仓位/价格/风控留在内核**（策略只出意图
数据）；**没有任何网络/凭证面**（策略 crate 不连接任何东西）。

需要比 `SafeStrategy` 更细的控制（直接布局内存、绕开包装的 JSON 薄层）时，
`user_layer/strategies/dog_strategy.rs`（`SafeStrategy` 模板 + 手写 ABI 两条路径
皆有实例）与 `user_layer/parity_strategy/parity_strategy.rs`（纯手写 raw ABI v2）
是完整范例——编译/加载/协商流程与模板完全一致，两条路径产出的 dylib 在内核侧
不可区分；存量手写库不需要任何改动即可继续加载（新可选符号缺失 = 未声明）。

加载器协商顺序（两条路径相同）：路径策略 → dlopen →
`bk_strategy_abi_version()==2`（v1 直接拒绝）→ vtable/必需钩子校验 →
`create()`。文件名含 `key/secret/private/credential/.env` 或非
`.so/.dylib/.dll` 一律在 dlopen 之前拒绝。

### 3.3 策略实现与分发（0 内核耦合）
内核自带 **0 个交易策略**（内核侧 0 策略，彻底解耦）。所有生产策略均通过 C ABI v2 共享库（`.dylib` / `.so` / `.dll`）在运行时加载：
- `user_layer/strategies/dog` → `libdog_strategy`
- `user_layer/strategies/spread_arb` → `libspread_arb_strategy`
- `user_layer/strategies/trend_follow` → `libtrend_follow_strategy`
- `user_layer/strategies/mean_reversion` → `libmean_reversion_strategy`

每个策略库均为独立 cdylib，通过 `user_layer/strategies` 独立工作区编译。算法逻辑与核心内核解耦，所有策略都通过 `strategy.load` / `load_strategy_dir` 挂载，并用 `strategy.list` / `strategy.enable` 进行查询与开关。

| 策略 | 默认 | 触发 | 定价 | 出场 |
|:---|:---|:---|:---|:---|
| `spread_arb`（抄底腿） | 初始未启用 | 已确认趋势里的回调 | 挂在 mid **之下**的被动买单（`entry < mid`） | 共享出场策略 |
| `trend_follow`（追涨腿，E4-a / #30） | 初始未启用 | 本 token 的 mid 在窗口内**上涨** ≥ `min_move_pct` | **抬价吃卖单**（`entry = best_ask > mid`），并受 `max_entry_price` 上限约束 | 共享出场策略 |
| `mean_reversion`（逆向/fade 腿，E4-b / #31） | 初始未启用 | 便宜侧（mid ≤ `max_price`=0.35 且 ≥ 0.05）从 120s 高点跌 ≥ `min_drop_pct`=10%、价差 ≤ `max_spread_pct`=8%，且**未处于单边下行**（600s 高点回落 < `trend_drop_pct`=30%，#176） | **低于 mid 的挂单**（`entry = round2(mid×0.98)`，向下夹到 best_bid，下限 0.05），token/分钟至多一次 | 共享出场策略 |

`spread_arb`/`trend_follow` 是**结构互逆**的一对（一个买被低估的一侧、一个买正在被买上去的一侧），
因此互为对冲；`mean_reversion` 是第三种形态——买**正在被砸 down** 的便宜侧，
赌盘口回弹被 trailing 抓住。它是**唯一声明门禁豁免的策略**：反向入场天然与现货动量闸门冲突
（实测 39–50% 的 fade 候选会被拦），因此宣告 `momentum` 豁免（E2-b 通道），timing 豁免不声明。
新策略默认**禁用**，这样一次升级不会改变已在运行的会话实际交易什么；要启用有三种等价方式：

```bash
# 1) 开机即启（可重复；--disable-strategy 优先于 --enable-strategy）
blitzkrieg-core --engine --enable-strategy mean_reversion ...
# 2) 运行期开关（无需重启）
{ "method": "strategy.enable", "params": { "name": "mean_reversion", "enabled": true } }
# 3) 回测/回放同样吃这两个开关（复用同一份 CoreConfig）
blitzkrieg-core --backtest <archive.jsonl> --engine --enable-strategy mean_reversion
```

`trend_follow` 的六个旋钮（`momentum_window_sec` / `min_move_pct` / `min_confirm_price` /
`break_price` / `max_entry_price` / `max_spread_pct`）都可被影子进化（§3.6）。
它**不声明任何门禁豁免**：顺势入场天然通过现货动量闸门，需要豁免的是 E4-b 的逆向腿。
默认值中 `min_move_pct=3.0` 与 `max_spread_pct=3.0` 由 `data/archive/` 实测分布定标
（详情见 `user_layer/strategies/trend_follow/` 的配置说明与源码）。
留出段回放表现与通道验证见 [TREND_FOLLOW_HOLDOUT_REPORT.md](../reports/TREND_FOLLOW_HOLDOUT_REPORT.md)。

`mean_reversion` 的八个旋钮（`lookback_sec` / `min_drop_pct` / `max_price` / `entry_factor` /
`max_spread_pct` / `cooldown_sec` / `trend_window_sec` / `trend_drop_pct`）同样可被影子进化；
断点 = mid 回升到 `max_price` 之上时冷却重置（镜像趋势腿的 move-death）。`min_drop_pct=10` 与
`max_spread_pct=8` 由同一 18 h 语料的 round-2 切片定标，其余为结构性选择；完整实证与方法论见
#31 设计评论与 [MEAN_REVERSION_HOLDOUT_REPORT.md](../reports/MEAN_REVERSION_HOLDOUT_REPORT.md)。

后两个旋钮是 #176 的**趋势闸门**：把入场已经用的同一个量（mid 相对自身历史高点的回落）放到
**更长窗口**上读一遍，`trend_window_sec=600`/`trend_drop_pct=30` 表示“600s 高点回落 ≥ 30% 即判定
为单边下行，不再抄这把刀”。它拦的是**下跌的年龄**而不是深度，因此单边行情里不再连续接刀
（冻结语料上单边切片成交 37 → 8、整段净额 -$22.80 → +$0.82、最大回撤 $31.00 → $7.16）；
`trend_window_sec=0` 即关闭闸门、精确回到 #176 之前的行为。形状与阈值的对照回测（含扫参表）见
`docs/reports/data/mean-reversion-gate/` 与 `scripts/mean-reversion-gate-evidence.mjs`。

加载进自驱动引擎的用户策略**默认禁用**：`strategy.list` 会显示它，但必须 `strategy.enable` 之后才会参与下单。

### 3.4 多策略并发（P-1.1）
自驱动引擎遍历**所有已启用**的策略产生候选单，然后统一过共享闸门：
- 回合时序（`--min-round-age` / `--min-time-left`）与现货动量闸门默认对所有策略一视同仁；
  仅当策略**显式声明**时，这两个入场质量闸门才可只对它自己放宽（E2-b，见 §3.5）；
- **每个 token 每个评估周期至多一单**（按注册顺序，先到先得），避免多策略抢同一 token；
- 仓位/名义金额仍受全局风控与 `--max-positions`（全局总容量）约束；
- 可选的 **per-strategy 限额与定寸**（`--strategy-limit`，可重复；`-` 或空段 = 该维度继承全局值）：
  - 旧格式（P-1.1，逐位兼容）：`<name>:<max_open_positions>:<max_notional_usd>`
  - 扩展格式（E2-a）：`<name>:<max_open_positions>:<max_notional_usd>:<size_usd>:<min_shares>:<max_shares>`
  - 生产由环境变量 **`HFT_STRATEGY_LIMITS`**（逗号分隔）透传到内核。
- **定寸与配额**：每个策略可自带目标名义额与张数区间（`size_usd`/`min_shares`/`max_shares`），
  未配置的维度沿用全局 `--size-usd`/`--min-shares`/`--max-shares`。**全局值既是兜底也是硬上限**：
  策略的覆盖只会被夹到全局风控区间内（名义额不超过全局预算、张数不超过全局 `max_shares`、
  不低于全局 `min_shares`），任何配置都无法突破全局风控。因此三策略共用同一全局预算时
  各自按自己的区间定寸，不再互相饿死。
- 超限的入场**在下单层之前**被拒：配额超限计入 `engine.stats.strategyLimitRejected` 与该策略的
  `limitRejected`（每次评估尝试计一次，语义同 `placeRejected`）；触到**全局**容量/风控的则计入
  `placeRejected` 与该策略的 `ordersRejected`（两者可区分是「策略配额」还是「全局闸门」）。
- `engine.stats.strategies[]` 给出每策略的会话账本与生效配置：`openPositions`/`openNotionalUsd`（实况敞口）、
  `maxOpenPositions`/`maxOpenNotionalUsd`（配置的配额，null = 未配置）、`sizingSource`
  （`"global"`|`"strategy"`）与 `effectiveSizeUsd`/`effectiveMinShares`/`effectiveMaxShares`（夹取后的生效定寸）、
  `ordersPlaced`/`ordersRejected`/`limitRejected`（入场上报）、`closedTrades`/`wins`/`losses`/`feesUsd`/`netPnlUsd`
  （已实现盈亏）。默认无任何限额配置 → 行为与单策略时代逐位一致。

### 3.5 入场闸门豁免（E2-b / #27）

两个**入场质量闸门**默认对所有策略一视同仁，而它们恰好会挡掉一类正当策略的入场点：

| 闸门 | 默认参数 | 误伤的形态 |
|:---|:---|:---|
| 回合时序窗口 `timing` | `min_round_age_sec=30`、`min_time_left_sec=180` | 开回合瞬间均值回归 / 抢前 30 秒 |
| 现货动量 `momentum` | 30 s 窗口、0.03% 容差 | 逆向 / 均值回归（现货越跌越买 Up） |

策略可以**显式声明**自己不需要这两个闸门中的某一个只作用于**它自己的候选单**。默认是全保留，
因此不声明的策略（含内建 `spread_arb`）行为逐位不变。

- **Rust（内建/内树）**：覆写 trait 方法
  ```rust
  fn gate_exemptions(&self) -> GateExemptions {
      // 只豁免时序窗口，且只豁免到「剩余 ≥180s」为止
      GateExemptions { timing: true, momentum: false, timing_min_time_left_sec: Some(180) }
  }
  ```
- **外挂 C ABI v2**：额外导出一个**可选符号**（不导出 = 不声明任何豁免），返回由本库
  `bk_string_out` 分配、内核用 `bk_strategy_free_string` 归还的 JSON：
  ```c
  char* bk_strategy_gate_exemptions(void* handle);
  // {"timing":true,"momentum":false,"timing_min_time_left_sec":180}
  ```
  `dog_strategy` 已导出该符号（`timing:true` + 下限 180）作为可运行范例。**注意**：它是独立可选符号而**不是**
  vtable 的新字段——内核按值拷贝 `BkStrategyVtable`，追加字段会改变 `sizeof` 并迫使
  `BK_ABI_VERSION=3`；按名字解析的可选符号缺省即「未声明」，因此 `BK_ABI_VERSION` 维持 2，
  旧库无需重编译。JSON 里非布尔值/未知键一律按 `false`（降级为「未声明」）处理。

#### 3.5.1 `timing` 豁免的剩余时间下限（D-31）

`timing` 豁免最初会**整段**waive 时序窗口，包括「离到期太近」（`TooCloseToExpiry`）。后果是：
一个豁免策略可以在轮次只剩几十秒时入场，而**出场策略会在下一个 tick 按设计强平**
（`force_exit_sec` 默认 120）。这笔单子定价没错，但**轮次已经没有生命去够到目标价**，
必然变成一笔零持仓出场——它不是策略判断失误，是把噪声写进了 dry 账本，拉低胜率却不携带信息。

因此策略可以为自己的 `timing` 豁免**声明一个 `time_left_sec` 下限**：

- `timing_min_time_left_sec = Some(n)`：该策略的候选单在 `time_left_sec >= n` 时才兑现豁免。
  **它替换**（而不是叠加）内核的 `min_time_left_sec`。
- `timing_min_time_left_sec = None`（含**所有 D-31 之前的库**）：沿用内核的
  `scanner.min_time_left_sec`。缺键、`null`、字符串、负数都降级到此值；负数还会被 clamp 到 0。
- 只影响「**离到期太近**」这一支：「轮次太年轻」（`TooYoung`）时 `time_left_sec` 很大，
  下限天然不冲突，仍然照旧豁免。相对 D-31 之前的行为，它**只能收窄、不可能放开**。
- 该字段同样走**已存在的可选 JSON 符号**（不是新的 vtable 字段），因此 `BK_ABI_VERSION` 仍为 2；
  旧库不写这个键即可，内核自动用更严的默认值。

> **统计口径说明（重要）**：本项改变的是**入场时机**，因此 `engine.stats` 里的
> `gateExemptedTiming`（豁免兑现次数）、`blocked.timing`（时序拦截次数）以及由它们衍生的
> 胜率 / 零持仓比例，在 D-31 前后**不可直接比较**：同一批「本该在收尾窗口入场」的候选单，
> 之前会计入成交、现在会计入 `blocked.timing`。跨这一天的 dry 账本做同比时，请以
> `engine.stats` 里 `byStrategy` 的分策略数字为准，并把「收尾窗口被挡掉多少」单列出来看，
> 而不是把两段的胜率直接相减。历史 `trades.jsonl` 里的零持仓记录是**改前**的既成事实，
> 不会被回填或剔除。

**可豁免的范围被刻意收窄，安全边界永远不可豁免：**
- 时序闸门里只有「窗口」（`TooYoung` / `TooCloseToExpiry`）可豁免；**「本轮没有市场」
  （`NoMarkets`）是结构性前提，永不豁免**。
- 单 token 单周期一单、挂单去重（`pending_tokens`）、定张数、per-strategy 配额、
  全局容量与风控区间**都不在豁免范围**。
- `ImmutableConfig`、`RiskGate`、kill switch、单日亏损上限、资金预扣等安全边界位于
  `Core::place`/`PositionManager`，与入场闸门路径完全无关，豁免**物理上**够不到它们
  （回归测试 `a_gate_exemption_never_bypasses_a_safety_boundary` /
  `a_gate_exemption_does_not_lift_the_daily_loss_cap` 钉住）。

**豁免必须显性且可审计**（不允许静默挖洞）：
- 每次被兑现的豁免产生一条中文审计日志（`tracing` target `strategy`）：
  `本单因策略 dog_strategy 豁免门禁 timing（Round too young (12s < 10000s)，token=up）`；
- `strategy.load` 的回执会在**启用前**写明声明了哪些闸门：
  `… registered … (disabled; declares gate exemptions: timing)`；
- `engine.stats.strategies[]` 增加 `gateExemptions`（声明的闸门）、`blockedTiming`/
  `blockedMomentum`（被挡次数）、`gateExemptedTiming`/`gateExemptedMomentum`（兑现次数）；
- `engine.stats.blocked` 增加 `byStrategy`（把每次拦截归属到具体策略，键 `timing`/`momentum`）
  与 `declaredExemptions`（当前在生效的全部豁免声明 `[{strategy, gates}]`），原有的
  `timing`/`momentum` 全局总数保留不变。
- 验收（真机 + 真实 dog cdylib）：`node scripts/strategy-gate-check.mjs`
  （npm 脚本 `core:strategy-gate`）——时序窗口对所有人关闭时，dog_strategy 仍入场、内建仍被挡、
  豁免被计数且 momentum 恒为 0。

> 运维侧「谁可以批准某策略豁免」的授权层（与策略自声明正交的二次授信）**明确不在 E2-b 范围**，
> 记为 [DECISIONS_PENDING D-16](../DECISIONS_PENDING.md)。

### 3.6 可进化旋钮的自证（E2-c / #28）

影子进化（[SHADOW_EVOLUTION.md](SHADOW_EVOLUTION.md)）现在**按策略**运行：每个策略声明自己
**愿意让进化去调**的旋钮及取值域，内核据此为它建一个独立的参数单元、独立评估、独立审计、独立回滚。
内核**不再硬编码任何策略的入场逻辑**——反事实比较用的影子孪生由策略**自己**造。

声明三件套（都是策略自己的责任）：

1. **可进化旋钮**：名称 + 当前值 + `[min,max]` 取值域（十进制字符串）。
2. **影子孪生工厂** `ShadowFactory`：给出这组参数时，返回一个与主策略**同代码、同出场策略**、
   仅旋钮不同的 `EngineStrategy` 实例（用 `ArcSwap` 里那一份值构造，因此比较是真正的反事实）。
3. **在自己的 evaluate 里读当前值**：从 `set_hot_params` 给到的注册表读**本策略**那一格；
   覆盖层被摘除时（进化关闭）读不到任何句柄，行为退回 `on_config` 配置。

- **Rust（内建/内树）**：覆写 trait 方法
  ```rust
  fn evolvable_knobs(&self) -> Vec<KnobSpec> {
      vec![KnobSpec::new("trendMaxEntryPrice", dec!(0.43), dec!(0.05), dec!(0.90))]
  }
  fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
      Some(Box::new(MyFactory))   // make(&StrategyParams) -> Option<Box<dyn EngineStrategy>>
  }
  ```
  内建 `spread_arb`（4 个 `trend_*` 旋钮）与 `trend_follow`（6 个入场旋钮）都已实现，作为可运行范例。
- **外挂 C ABI v2**：额外导出一个**可选符号**（不导出 = 明确「不可进化」）：
  ```c
  char* bk_strategy_evolvable_knobs(void* handle);
  // {"knobs":[{"name":"trendMaxEntryPrice","value":"0.43","min":"0.05","max":"0.90"}]}
  ```
  `dog_strategy` 与 `parity_strategy` 都已导出该符号。与 §3.5 同理，它是**独立可选符号**
  而非 vtable 新字段（内核按值拷贝 vtable），`BK_ABI_VERSION` 维持 2、旧库无需重编译；
  JSON 非法/缺字段一律降级为「未声明」（`KnobDeclaration::parse` 永不 panic）。

**「不声明」的语义是明确且可观测的**，不是「暂时没有参数」：
该策略拿不到参数单元、不出现在 `shadow_evolution.status.strategies[]` 里、
对其 `apply`/`rollback` 会被明确拒绝并说明原因。`strategy.load` 的回执也会写清：
`… (disabled; not evolvable (no knobs declared))` 或 `… ; declares evolvable knobs: trendMaxEntryPrice`。

**三层锁对声明同样生效**（域 → 步长 → 风控不可变），声明逃不掉：
- 取值域是**外层硬边界**，越界直接拒绝——想「渐变地爬出域外」也不行（域检查在步长检查之前）；
- 单步变化仍受 `max_gradient`（默认 ±5%）限制，需要更大变化只能多轮逼近；
- 风控参数（硬止损/连亏熔断/日亏上限/单笔名义上限）**不在任何** `StrategyParams` 里，
  结构上就读不到、写不进。

审计按策略分文件：`data/evolution/<strategy>.jsonl`；`apply`/`rollback` 走 IPC
且**都要求 `strategy` 参数**。门禁：`node scripts/strategy-evolution-check.mjs`。

## 4. 生命周期与开关

| method | 说明 |
|:---|:---|
| `strategy.list` | 列出内核支持的策略及启用状态 |
| `strategy.enable` | `{ name, enabled }` 开关某策略 |
| `strategy.load` | 加载用户层动态库（feature 开启时） |

回合切换时内核调用 `on_round(slot)`，策略应在此清空本回合状态。

## 5. 常见问题

- **为什么我的策略能编译但不下单？** 信号只表达意图，是否成交取决于内核的风控/资金/入场闸门（例如 `min-time-left`、入场上限、资金预扣）。入场不带张数、出场不带价格——都由内核按盘口与配置决定。
- **策略能自己平仓吗？** 能表达**出场意图**（v2 `exits` / `take_exit_intents`，记录为 `ExitReason::strategy_signal`），但平仓本身仍由内核执行（按盘口定价、live 卖单去重、风控/账本/签名）。即使关闭了自动出场（`auto_exits_enabled=false`），策略出场也会被处理，语义同手动平仓；没有持仓的 token 会被丢弃。
- **能访问盘口深度吗？** v2 可以：`BkBookView` 携带**全档位** `bids/asks`（price/size 字符串）以及 `bid_depth/ask_depth/obi/spread/spread_pct`，每次盘口回调都送达。`confirmed_tokens`/`diagnostics` 可据此自证看到的深度。
- **v1 的策略库还能加载吗？** 不能，v2 是干净断点（无兼容层；v1 从未默认启用、无外部消费者，见 DECISIONS_PENDING D-15）。请用 strategy-api v2（`bk_strategy_abi_version()==2`）重新编译。

## 6. 回测你的策略（P-1.2 / P-1.3）

回测不是一个另写的模拟器：**内核自带全链路回放**（`--backtest`）在虚拟时钟上驱动**同一个 `Core`**，
把归档的行情按原时间戳喂回同一入口 `engine_on_data` —— 策略、风控、账本、下单、出场、费用
与 live 是同一条代码路径。

```bash
# 1) 运行时归档行情（Dry/Live 均可；归档只镜像行情事件，不读写任何凭证）
target/release/blitzkrieg-core --socket <path> --mode dry --engine --feed-ws \
  --event-archive data/archives/events.jsonl

# 2) 用同一策略回放这段时间（永远 Dry：不起 feed、不写 trade/order/position 日志）
target/release/blitzkrieg-core --backtest data/archives/events.jsonl --engine \
  --backtest-report data/archives/report.json \
  --assets BTC,ETH,SOL,XRP --min-round-age 30 --min-time-left 180 --round-sec 900 \
  [--slippage-ticks 1] [--latency-ms 250] [--fill-prob-bps 5000]
```

- **回放参数必须与采集时一致**（`--assets`/`--round-sec`/`--min-*`/`--max-*`/策略开关），
  否则重放的不是同一个决策环境。
- 想比较**不同参数**：换参数重放同一归档，对比报告里的净 PnL / 胜率 / 每策略分账。
- 报告（`--backtest-report` JSON）：feed 计数、blocked、订单终态计数、逐笔明细、净/毛 PnL、
  费用、每策略账本、源统计（坏行/乱序）、`forcedDry: true`。报告**逐字节可复现**（同参数两次回放
  完全相同），可以直接用 `diff` 做参数回归。
- 摩擦三旋钮默认全关（恒等 = 与 live 等价）；逐步打开可回答「滑点/延迟/成交率吃掉多少收益」。
- **评估节拍**：回放的 `tick + engine_evaluate` 跑在自己的 `--backtest-tick-ms` 定时表上（默认 50 ms，
  与 live 的 `ipc::server` interval 同频），**与事件密度无关**——出场（TP/SL/追踪/强平）因此与 live 同等灵敏。
  真实 feed 是亚毫秒级突发：若把维护周期挂到"到下一事件的间隙"上，13 分钟只会跑 803 个周期（应 15 610），
  回测出场会比 live 迟钝。该缺陷已修复（`MIGRATION_LOG §35`），并有回归测试钉住两种密度下的周期数。
- 一致性验收：`node scripts/backtest-check.mjs`（**21/21**）——同一次采集的 live 与回放**逐位相等**
  （含成交：净盈亏 5.12208717）；真实 feed 归档（1 025 963 事件 / 13 分钟）重放 **19/19**。
- 已知口径（D-11）：dry 行情路径的 maker 挂单除下单瞬间外不会成交，入场最终升级为 taker（付 taker 费）；
  归档+回放如实复现，因此 dry 回测的入场成本是**保守**的。
