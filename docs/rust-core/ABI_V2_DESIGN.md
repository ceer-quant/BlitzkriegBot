# C ABI v2 — 全功能外挂策略接口设计（E7 / issue #38）

状态：已实现前的设计基线（feat/e7-abi-v2）
日期：2026-09-15
对应：issue #38「策略接口全功能化 + 外挂标准化」；里程碑 v0.2 第一项。

## 1. 设计裁决（一句话）

**内核侧 0 策略，只存在一套全功能策略契约。** 内核纯粹是执行器与风控中枢，
不实现任何交易策略；所有策略均通过 C ABI v2 动态加载（dlopen 一个 C ABI v2 cdylib 外壳）
或在测试中以通用适配器接入。

因此本批彻底实现**内核 0 策略**与**硬切换解耦**：
- 内核中完全移除树内硬编码交易策略实现（`spread_arb` / `trend_follow` / `mean_reversion` 全移至独立动态策略工作区 `user_layer/strategies`；**该工作区的 5 个策略已于 2026-09-23 整体删除**，`user_layer/strategies/` 只剩投放点职责，见 [CHANGELOG.md](../../CHANGELOG.md)）；
- 核心依赖共享逻辑库 `strategy_logic` 提供算法数学与测试参考实现（该库仍在，树内无调用者）；
- 生产环境所有策略全部为外挂 C ABI v2 cdylib，分发时不捆绑策略（**现已无任何自带策略可分**）；
- 移除 `[strategy] active = [...]` 等硬编码配置，启动时策略列表为空，由宿主动态加载并启用。

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
| 入场闸门自声明豁免（E2-b） | ✅ trait 默认无豁免 | ❌ | ✅ 可选符号 `bk_strategy_gate_exemptions`（§3.5） |
| 新鲜盘口门（E-parity） | ✅ `StrategyCtx::fresh_book` | ❌ | ✅ 可选符号 `bk_strategy_bind_eval_ctx`（§3.6） |
| 评估期真实计时 | ✅ `ctx.time_left_sec`/`now_ms` | ❌ 曾填 0 | ✅ `BkRound` 原生携带 |
| 诊断带评估上下文 | ✅ `diagnostics(&ctx)` | ❌ | ✅ 诊断调用前绑定同一 eval ctx（§3.6） |
| 配置生效视图 | ✅ `config_view_json` | ❌ | ✅ 可选符号 `bk_strategy_config_view`（§3.6） |

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

// E-parity（§3.6）：评估期借出的「新鲜盘口」上下文。
typedef struct {
  const char* token;      // 该行盘口所属 token
  bk_book_view_t book;    // 完整档位视图
  uint8_t fresh;          // 恒为 1（宿主只装订可定价的行）
} bk_token_book_t;

typedef struct {
  bk_round_view_t view;
  const bk_token_book_t* books; size_t book_count;
} bk_eval_ctx_t;
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

**此结构体自发布起冻结，永远不再追加字段**（按值拷贝，加字段即 `sizeof` 破坏式
变更，须升 v3）。v2 内的任何新能力都走 §3.5 的独立可选符号——首个实例是 E2-b 的
`bk_strategy_gate_exemptions`。

### 3.4 版本协商与拒绝矩阵（硬规则）

- `bk_strategy_abi_version() -> uint32` 对 v2 库为**必填**符号。
- loader 顺序：`policy_allows`（路径/凭证特征）→ dlopen → 读版本符号 →
  读 vtable。**先协商再解释 vtable**，所以 v1 库在版本步即被拒，不会被按 v2
  布局误读。
- 接受当且仅当：版本符号 `== 2` **且** `vtable.abi_version == 2`。其余一律
  `LoadOutcome::Failed`，信息明确（v1 库：要求用 strategy-api 0.2 重新构建）。
- v1 无生产消费者（feature 从未默认开启、无外部 dylib），故**不留 v1 兼容
  shim**，干净断裂（见 DECISIONS_PENDING D-15）。

### 3.5 演进规则：vtable 冻结，新能力走「可选符号」（E2-b 确立）

`BkStrategyVtable` 一旦发布即为**冻结布局**，原因是 loader 的内核侧用
`std::ptr::read(vt_ptr)` 把整个 vtable **按值**拷贝出来：给结构体末尾追加
字段会改变 `sizeof`，旧库在这一读上越界——那是一次破坏式变更，必须递增
`BK_ABI_VERSION=3` 并让所有库重编译。

因此 v2 内新增能力的**唯一合法形式**，是一个按名字解析的**独立可选符号**：

```c
// E2-b：策略自证它不需要哪些共享入场质量闸门（见 STRATEGY_GUIDE §3.5）
char* bk_strategy_gate_exemptions(void* handle); // {"timing":bool,"momentum":bool}

// E2-c：策略自证它的可进化旋钮与取值域（见 STRATEGY_GUIDE §3.6）
char* bk_strategy_evolvable_knobs(void* handle);
// {"knobs":[{"name":"trendMaxEntryPrice","value":"0.43","min":"0.05","max":"0.90"}]}

// E-parity（§3.6）：评估期借出「轮次视图 + 可定价盘口」，语义=树内 fresh_book
void bk_strategy_bind_eval_ctx(void* handle, const bk_eval_ctx_t* ctx); // ctx=NULL=解除装订

// E-parity（§3.6）：策略自证「当前生效配置」（树内 config_view_json 的泛化）
char* bk_strategy_config_view(void* handle);
```

符号常量在 `blitzkrieg-strategy-api`：
`BK_GATE_EXEMPTIONS_SYMBOL = b"bk_strategy_gate_exemptions\0"`、
`BK_EVOLVABLE_KNOBS_SYMBOL = b"bk_strategy_evolvable_knobs\0"`、
`BK_BIND_EVAL_CTX_SYMBOL = b"bk_strategy_bind_eval_ctx\0"`、
`BK_CONFIG_VIEW_SYMBOL = b"bk_strategy_config_view\0"`。

loader 用 `lib.get::<T>(BK_..._SYMBOL).ok()` 解析：**符号缺失 = 该项未声明**，
旧库行为与「使用 trait 默认实现」的树内策略逐位一致，`BK_ABI_VERSION` /
`BK_MIN_ABI_VERSION` 维持 2。对 `bk_strategy_evolvable_knobs` 而言「缺失」的语义
格外明确：**缺失即该策略不可进化**——内核不为它建参数单元，对其
`shadow_evolution.apply`/`rollback` 一律拒绝，加载回执写明
`not evolvable (no knobs declared)`。约定：

- 可选符号复用 `bk_string_out` / `bk_strategy_free_string` 的 JSON 出参与同库
  释放规则（除非另有说明）。
- 解析失败 / JSON 非法 / 取值类型不符，一律**降级为「未声明」**，绝不 panic、
  绝不按「更宽松」解释。
- 只有当某个能力无法用「符号缺失即无操作」表达（例如必填输入的布局变化）时，
  才允许升级 v3；单纯的输出型新能力永远走可选符号。

### 3.6 E-parity：评估上下文装订与配置生效视图（issue #38 E7 收尾）

E7 的承诺是「外挂只是换一种加载方式，而不是换一套能力」。落到代码上还剩三项
能力差异，全部以 v2 可选符号补齐，`BK_ABI_VERSION` / `BK_MIN_ABI_VERSION` 仍为 2：

**① 新鲜盘口门** — 树内策略经 `StrategyCtx::fresh_book(token)` 拿到的盘口带
宿主的新鲜度裁决（非空且未超 `max_orderbook_stale_ms` 预算才算可定价；过期与
缺失不可区分，一律 `None`）。外挂策略通过导出
`void bk_strategy_bind_eval_ctx(void* handle, const bk_eval_ctx_t* ctx)` 获得同一
裁决：宿主在**每次** `evaluate` 与 diagnostics 调用**之前**，装订一个借用的
`bk_eval_ctx_t`（轮次视图 + **仅可定价**的盘口行，行内 `fresh` 恒为 1），调用
结束后装订 `NULL` 关闭借用窗口——上下文只在当次回调内有效，策略不得保存指针。
某 token 没有对应行 = 「此刻不可定价」，与树内 `fresh_book(..) == None` 逐位同义。
安全模板（`safe.rs`）把它接成 `on_eval_books(&[FreshBook])` 钩子，作者无需碰 ABI。

**措辞必须严格**：`fresh=1` 是**入场条件，不是标记**。装订进来的每一行盘口
都**一定是新鲜的**（宿主只装订可定价行），策略看到 `fresh=1` 之外不会有别的
取值；「这一行过期了吗」对外挂策略**不构成问题**——过期与缺失同样表现为
「没有这一行」。树内策略同样无法区分过期与缺失（`fresh_book` 返回 `None`
不带原因），因此两侧语义逐位一致，不存在信息差。

**② 评估期真实计时** — `on_round`/evaluate 的 `BkRound` 本就携带
`time_left_sec` / `now_ms`；内核曾向 foreign 策略填 0（已修复）：引擎现在用
`scanner.round_state(now_ms).time_left_sec` 计算真实剩余秒数，树内与外挂看到
完全相同的时钟。

**③ 配置生效视图** — 树内实现可覆写 `config_view_json`（in-force 配置的
只读视图）。外挂策略通过导出 `char* bk_strategy_config_view(void* handle)`
返回任意 JSON（热参叠加后的实际生效值）。缺失符号 / NULL / 非法 UTF-8 = 未声明，
`engine.strategy_config_views()` 不列该策略；首个真实载体是 parity 库，其
in-tree 孪生与 dylib 的视图在 `foreign_parity` 中被要求完全相等。

诊断同理：foreign 库的 diagnostics 钩子被调用前，宿主装订与 evaluate 完全相同的
评估上下文——一个诊断因此可以像树内 `diagnostics(&ctx)` 一样报告新鲜度裁决后
的值。

**生命周期与 panic 安全**：`bk_eval_ctx_t` 及其全部指针**只在单次回调内有效**
（借用的宿主存储，回调返回即可能失效），外挂策略**不得保存任何引用/指针**——
持有即 use-after-free，这是 ABI 借用规则的硬性条款。宿主侧以
`call_with_bound_ctx` 保证「装订 → 调用 → 解绑（bind NULL）」在**所有**路径上
成对发生：钩子 panic 时先解绑、释放行存储，再恢复 panic 传播（内核的 panic
语义与装订机制出现之前完全一致）。这是兜底层：按标准 `"C"` unwind 约定编译的
库在自己的帧内即中止，该防护覆盖的是宿主侧 marshal panic 与 `extern "C-unwind"`
类库。

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

E2-c（[#28](https://github.com/ceer-quant/BlitzkriegBot/issues/28)）把这一节从「共用一份
全局 JSON」改成了**按策略隔离**，且**没有改 ABI**：新能力全部落在可选符号上。

- **内核侧**：`EngineStrategy::set_hot_params(Option<Arc<ParamRegistry>>)`。`ParamRegistry`
  按策略名持有 `Arc<ArcSwap<StrategyParams>>`；`StrategyParams` 是**该策略自己的**
  `BTreeMap<knob, Decimal>`，不是四个 `trend_*` 字段的全局结构。
  `None` = **摘除**覆盖层（进化关闭），策略从此读不到任何热参句柄，行为退回 `on_config`
  下发的配置——这是「关闭进化 ⇒ 与改动前逐位一致」的实现方式。
- **foreign 外壳**：`set_hot_params` 后用 `registry.handle_for(&self.name)` 只解析
  **自己那一格**；声明了 0 个旋钮的库拿不到单元（明确「不可进化」）。
  `push_hot_params_if_changed` 在热路径上读当前值、序列化为 JSON、与上次下发**去重**后
  调 `on_hot_params`。next-tick 生效、无重启，语义与树内 builtin 一致。
- **旋钮自证**：可选符号 `bk_strategy_evolvable_knobs` →
  `{"knobs":[{name,value,min,max}]}`（十进制字符串）。loader 经
  `KnobDeclaration::parse` 解析，**非法 JSON / 缺字段 / 符号缺失 = 未声明**，永不 panic。
  `ForeignStrategy::shadow_factory()` 在 `knobs.is_empty()` 时返回 `None`，
  这正是「不声明 = 不可进化」的落点。
- **孪生**：`shadow_factory()` 的 foreign 实现调用库的 `create()` **再建一个独立实例**
  （同一 handle 表之外的第二实例），把反事实参数经 `apply_params_direct` 直接推给它——
  于是影子变体跑的是**该库自己的**入场/出场逻辑，与树内路径语义等价。
  `LoadedLibrary` 用 `Arc` 持有，孪生也持有同一 `Arc`，因此只要有孪生存活，库就不会被卸载。

按策略的参数命名空间 / 独立评估 / 独立审计 / 独立回滚均由内核完成；**ABI 仍为 v2**，
`BK_ABI_VERSION` / `BK_MIN_ABI_VERSION` 不变，已发布的 v2 库无需重编译。

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
- `core/blitzkrieg_core/tests/dynamic_strategy.rs` 重写为 v2：加载 → 全深度 on_book
  → evaluate entries/exits → confirmed/diagnostics → 热参 → v1 协商拒绝。

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
