# C ABI v2 — 全功能外挂策略接口设计（E7 / issue #38）

状态：已实现前的设计基线（feat/e7-abi-v2）
日期：2026-09-15
对应：issue #38「策略接口全功能化 + 外挂标准化」；里程碑 v0.2 第一项。

## 1. 设计裁决（一句话）

**只存在一套全功能策略契约。** 树内策略与外挂 dylib 实现的是同一个
`EngineStrategy`（10 个能力点）；外挂只是**加载方式不同**（dlopen 一个 C ABI
v2 外壳），不是一套残废接口。

因此本批**删除** v1 的二等公民路径：

- 删除 `strategy_engine::Strategy`（on_tick 单点、best-only 的缩减契约）；
- 删除 `strategies/user_adapter.rs`（`on_book` 空实现、`Sell` 被丢弃的适配器）；
- 删除 core 内的「独立 strategy_engine 诊断注册表」回退路径；策略一律注册进
  真正驱动交易的 `engine::Engine`，启动即 DISABLED。

能力差因此在结构上不可能复发：树内外都走同一个 trait、同一套宿主门禁。

## 2. 能力对照（v1 → v2）

| 能力 | 树内（保留） | dylib v1 | dylib v2（本批） |
| --- | --- | --- | --- |
| 生命周期 on_round | ✅ | ✅ | ✅ |
| 每次盘口回调 on_book | ✅ | ❌ 空 | ✅ |
| 全档位深度 + OBI/spread | ✅ | ❌ 只 best | ✅ `BkBookView` |
| 入场候选 | find_candidates | ✅ Buy | ✅ evaluate.entries |
| **出场意图** | 内核统一出场 | ❌ Sell 丢弃 | ✅ evaluate.exits → 内核平仓 |
| take_breaks | ✅ | ❌ | ✅ evaluate.breaks |
| confirmed_tokens | ✅ | ❌ | ✅ JSON 数组 |
| diagnostics | ✅ | ❌ | ✅ JSON 值数组 |
| 影子进化热参 | ✅ | ❌ | ✅ on_hot_params(JSON) |
| 自证可进化旋钮 | （E2-c） | ❌ | ✅ knobs() JSON Schema 片段 |
| 配置变更 on_config | ✅ | ❌ | ✅ on_config(JSON) |

## 3. ABI v2 二进制契约（crate `blitzkrieg-strategy-api`，版本 2）

保持 v1 原则：`#[repr(C)]`、零 Rust 专有类型跨边界（无 `String`/`Vec`/trait
对象/dyn）、decimal 一律以 NUL 结尾 UTF-8 字符串过界（无浮点漂移）。

### 3.1 输入结构（宿主 → 策略，借用，仅当次调用有效）

```c
typedef struct { const char* price; const char* size; } bk_level_t;

typedef struct {
  const char* symbol;          // token id
  const char* asset;           // e.g. "BTC"
  const bk_level_t* bids; size_t bid_count;   // 价格降序
  const bk_level_t* asks; size_t ask_count;   // 价格升序
  const char *best_bid, *best_ask, *mid;
  const char *bid_depth, *ask_depth;          // 档位量累加
  const char *obi;                            // (bd-ad)/(bd+ad)
  const char *spread, *spread_pct;
  int64_t timestamp_ms;
} bk_book_view_t;

typedef struct { int64_t slot; int64_t time_left_sec; int64_t now_ms; } bk_round_t;

typedef struct {
  const char* asset; const char* condition_id; const char* question_id;
  const char* up_token; const char* down_token;
  int64_t expires_at_ms; int64_t slot; uint8_t neg_risk;
} bk_market_t;

typedef struct {
  bk_round_t round;
  const bk_market_t* markets; size_t market_count;
} bk_round_view_t;
```

档位的排序、深度、OBI、spread 全部由**宿主**用 `OrderbookSnapshot::from_levels`
同一套算法预算好——策略看到的派生量与内核、与树内策略完全一致。

### 3.2 输出（策略 → 宿主）：堆上 JSON C 字符串

凡是「变长/结构化」输出（evaluate/confirmed/breaks/diagnostics/knobs），策略
返回一个 `*mut c_char`，内容为 UTF-8 JSON。内存由**产出方同一个 cdylib**
释放：策略库导出 `bk_strategy_free_string`，loader 从**该 lib** 解析符号并回
调，因此跨编译器版本也不会串分配器。API crate 提供成对辅助：

- `bk_string_out(String) -> *mut c_char`（`CString::into_raw`）
- `bk_strategy_free_string(*mut c_char)`（`CString::from_raw` + drop）

serde 不进 ABI：边界上只有 `const char*`。

evaluate 的 JSON 约定：

```json
{
  "entries": [ { "token": "0xUP", "price": "0.43", "reason": "dip" } ],
  "exits":   [ { "token": "0xUP", "reason": "tp" } ],
  "breaks":  [ { "token": "0xUP", "broken_price": "0.30" } ]
}
```

- entries **不带 size**：定仓是宿主职责（与树内 `TradeSignal` 一致，内核
  `compute_shares` 统一算），策略无权定仓。
- exits **不带价格**：平仓价由内核以当前盘口定价（策略信号出场默认 Taker、
  取当前 best bid），策略只表达「平哪个 token、为什么」。
- token 必须属于本轮 `markets` 的 up/down；宿主据此回填 asset / condition_id /
  direction，无法回填的条目直接丢弃（信任边界在宿主侧校验）。

### 3.3 vtable

```c
typedef struct bk_strategy_vtable {
  const char* name; const char* version;
  uint32_t abi_version; uint32_t min_abi;
  void* (*create)(void);
  void  (*destroy)(void*);
  void  (*on_book)(void*, const bk_book_view_t*);
  void  (*on_round)(void*, const bk_round_t*);
  char* (*evaluate)(void*, const bk_round_view_t*);   // 必填
  char* (*confirmed_tokens)(void*);
  char* (*take_breaks)(void*);
  char* (*diagnostics)(void*);
  int   (*on_config)(void*, const char* json);        // 0 ok / 非0 拒绝
  int   (*on_hot_params)(void*, const char* json);
  char* (*knobs)(void*);                              // 自证可进化旋钮
} bk_strategy_vtable_t;
```

必填：`create/destroy/on_book/on_round/evaluate` 与工厂/版本符号。其余可空，
空 = `EngineStrategy` 的默认实现（不破坏「全功能接口」，单个策略可不用某个钩子，
树内策略同理）。

### 3.4 版本协商与拒绝矩阵（硬规则）

- `bk_strategy_abi_version() -> uint32` 对 v2 库为**必填**符号。
- loader 顺序：`policy_allows`（路径/凭证特征）→ dlopen → 读版本符号 →
  读 vtable。**先协商再解释 vtable**，所以 v1 库在版本步即被拒，不会被按 v2
  布局误读。
- 接受当且仅当：版本符号 `== 2` **且** `vtable.abi_version == 2`。其余一律
  `LoadOutcome::Failed`，信息明确（v1 库：要求用 strategy-api 0.2 重新构建）。
- v1 无生产消费者（feature 从未默认开启、无外部 dylib），故**不留 v1 兼容
  shim**，干净断裂（见 DECISIONS_PENDING D-15）。

## 4. 出场意图如何接进内核（不失控）

策略只表达 intent；以下全部仍由内核独占，树内外一视同仁：

1. `Engine::evaluate` 调每个策略的 find_candidates；新增 trait 方法
   `take_exit_intents() -> Vec<StrategyExitIntent{token_id, reason}>`（默认空），
   evaluate 与 on_data 后统一 drain 进引擎队列。
2. `service.engine_evaluate` 取 `engine.drain_strategy_exits()` 入 service 队列。
3. `service.run_exit_checks`（现 1661）重构为：估值 → 汇总两类平仓请求
   （策略自动出场策略的 `ExitRequest` **＋** 策略信号出场）→ **复用同一个提交
   循环**（live-sell 去重、`sell_shares`、`place` 风险/账本/签名、记录出场原因）。
4. 新增 `ExitReason::StrategySignal`（序列化为 `"strategy_signal"`）。策略信号
   出场按「显式决策」对待，**不**被 `auto_exits_enabled=false` 拦截（与手动
   flatten 同类），但仍受 master kill switch / RiskGate / 去重 / 持仓存在性约束；
   它无法绕过任何一条内核规则，也拿不到订单管理器或签名器。

策略对象跨边界仍然只有：只读盘口/轮次/配置/热参。**没有**凭证、venue client、
OME、UDS、网络句柄。

## 5. 热参数与「策略自证旋钮」

- `set_hot_params(Arc<ArcSwap<MutableParams>>)` 到达时，foreign 外壳缓存句柄；
  每次 evaluate 读取当前值，序列化为 camelCase JSON（今天即那 4 个 trend_*
  字段），与上次下发不同则回调 `on_hot_params`。next-tick 生效、无重启，语义
  与树内 builtin 对齐。
- `knobs()` 返回该策略**自证**的可进化字段（JSON Schema 片段）。按策略的参数
  命名空间/独立评估属 E2-c（#28）；本批只把**过界通道与自证机制**端到端打通，
  策略从同一份 MutableParams JSON 里读自己认识的字段。E2-c 扩的是内核参数
  模型，不再改 ABI。

## 6. 单一注册路径

- `Engine::register_user_strategy(Box<dyn EngineStrategy>, source)` 取代旧的
  缩减 trait 版本；启动 DISABLED、重名拒绝、来源标记不变。
- `service.load_strategy_lib(path)`：dlopen → 协商 → 包成 `ForeignStrategy`
  （impl `EngineStrategy`）→ 注册进 engine（DISABLED）。engine 未挂载时返回
  `Failed`（生产始终挂载）。
- IPC 方法名 `strategy.load` / `strategy.enable` 不变，Node 侧无感。

## 7. 对拍验收（issue 验收第一条）

为保证「同一份逻辑、两种加载、逐信号一致」不是两份手写拷贝互相迁就：

- 新增零业务依赖的共享 crate `user_layer/parity_logic`（rlib，仅依赖
  rust_decimal/serde_json），定义**唯一一份**确定性策略算法，运行在中立的
  `ParityBook/ParityMarket` 表示上（入场用 best ask + OBI；出场用 best bid/
  spread；confirmed 看档位数；diagnostics 输出深度/OBI/spreadPct；热参改
  buyBelow）。
- 树内路径：集成测试里一个实现 `EngineStrategy` 的薄壳，把 OrderbookSnapshot
  映射成 ParityBook，调同一算法。
- 外挂路径：`user_layer/parity_strategy` cdylib，把 `BkBookView` 映射成
  ParityBook，调**同一个** parity_logic。
- 集成测试 `foreign_parity.rs`：两台 Engine 注入**同一组**确定性 DataEvent
  回放，逐周期比对：入场 `OrderRequest`（token/price/strategy/direction/
  internal_key/size）、出场意图（token/reason）、confirmed 集合、diagnostics
  规范化 JSON。任何一项不一致即失败。
- `tests/dynamic_strategy.rs` 重写为 v2：加载 → 全深度 on_book → evaluate
  entries/exits → confirmed/diagnostics → 热参 → v1 协商拒绝。

## 8. 构建与 CI（验收第四条）

- `strategy-loading` 改为**默认 feature**（`default = ["polymarket",
  "strategy-loading"]`），release 内核正式具备 dylib 能力。
- CI rust-check 在 `cargo test` 前先真实构建两个独立 cdylib：
  `(cd user_layer/strategies && cargo build --release --locked)`、
  `(cd user_layer/parity_strategy && cargo build --release --locked)`；
  工作区测试以默认 feature 运行（含加载/驱动/对拍）。
- 集成测试默认在 dylib 缺失时 skip；CI 置 `BK_REQUIRE_DYLIB=1` 时缺失即
  **硬失败**，杜绝「写了但 CI 没真跑」。

## 9. 不做什么（边界）

- 不改任何入场/出场的既有业务参数与门禁阈值；不启用 Live；不碰凭证。
- 不实现 E2-c 的按策略参数模型、不实现 E3/E4 新策略（它们将直接实现本契约，
  且必须能外挂——修正 #30/#31 原「仅原生」前提）。
- 不引入 v1 shim。
