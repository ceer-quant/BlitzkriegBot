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

## 1.5 冻结的 C ABI（动态库）

用户层策略以共享库（`.dylib`/`.so`/`.dll`）交付时，走 **冻结的 C ABI v1**（crate `blitzkrieg-strategy-api`，`#[repr(C)]`，无 Rust 特有类型）：

```c
uint32_t bk_strategy_abi_version(void);          // 版本协商（可选但推荐）
const BkStrategyVtable* bk_strategy_create(void);// 工厂（必须）
```
```c
typedef struct { const char* symbol; const char* asset; const char* best_bid;
                 const char* best_ask; const char* mid; int64_t timestamp_ms; } BkTick;
typedef struct { int side; /*0 None,1 Buy,2 Sell*/ const char* symbol;
                 const char* price; const char* size; } BkSignal;
typedef struct { const char* name; const char* version; uint32_t abi_version;
                 BkHandle (*create)(void); void (*destroy)(BkHandle);
                 BkSignal (*on_tick)(BkHandle, const BkTick*);
                 void (*on_round)(BkHandle, int64_t); } BkStrategyVtable;
```

规则（**必须**）：
- 字符串为借用的 NUL 结尾指针，**仅在调用期间有效**；内核立即复制。
- 价格/数量用**十进制字符串**（避免浮点漂移）。
- 策略拥有自己的 `handle`；内核不释放，只在卸载时调用 `destroy`。
- 策略**禁止** I/O、凭据、venue 调用——边界上只传 tick 与 signal。
- 变更结构体/vtable **必须**递增 `BK_ABI_VERSION`；内核加载时校验，不匹配即拒绝。

最小实现见 `user_layer/strategies/dog_strategy.rs`（含 `bk_strategy_create` 与 `bk_strategy_abi_version`），其 crate 见 `user_layer/strategies/Cargo.toml`：
```bash
cd user_layer/strategies && cargo build --release   # → libdog_strategy.{dylib,so}
```

## 2. 契约

```rust
pub struct MarketTick {
    pub symbol: String,      // 代币/合约标识
    pub asset: String,       // BTC/ETH/...
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: f64,
    pub timestamp_ms: i64,
}

pub enum Signal {
    Buy  { symbol: String, price: f64, size: f64 },
    Sell { symbol: String, price: f64 },
    Hold,
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal>;
    fn on_round(&mut self, slot: i64) {}   // 回合切换时清状态（可选）
}
```

内核会在调用 `on_tick` 之外执行 **信号校验闸**（`validate_signal`）：价格必须在 (0,1]、symbol 必须与 tick 一致、size 必须为正。非法信号被丢弃，不会进入风控/下单。

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

### 3.2 Rust 源文件（动态库）
`user_layer/strategies/dog_strategy.rs`（见该文件）：
```rust
pub struct DogStrategy { pub buy_price: f64, pub take_profit: f64, pub stop_loss: f64 }
impl DogStrategy {
    pub fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal> {
        if tick.mid <= self.buy_price {
            Some(Signal::Buy { symbol: tick.symbol.clone(), price: self.buy_price, size: 10.0 })
        } else { None }
    }
}
```
编译为动态库后由内核加载：
```bash
rustc --crate-type=dylib user_layer/strategies/dog_strategy.rs \
      -o user_layer/strategies/libdog_strategy.dylib
```
加载（需内核以 `--features strategy-loading` 编译）：
```
/crypto-hft ... → IPC: { "method": "strategy.load", "params": { "path": "user_layer/strategies/libdog_strategy.dylib" } }
```
加载器会先做策略检查：文件名含 `key/secret/private/credential` 或非 `.so/.dylib/.dll` 一律拒绝。

### 3.3 内建（内核自带）
`spread_arb` 是内建策略（`strategies::SpreadArbBuiltin`，宿主化实现），可用 `strategy.list` / `strategy.enable` 查询与开关。
加载进自驱动引擎的用户策略**默认禁用**：`strategy.list` 会显示它，但必须 `strategy.enable` 之后才会参与下单。

### 3.4 多策略并发（P-1.1）
自驱动引擎遍历**所有已启用**的策略产生候选单，然后统一过共享闸门：
- 回合时序（`--min-round-age` / `--min-time-left`）与现货动量闸门对所有策略一视同仁；
- **每个 token 每个评估周期至多一单**（按注册顺序，先到先得），避免多策略抢同一 token；
- 仓位/名义金额仍受全局风控与 `--max-positions` 约束；
- 可选的 **per-strategy 限额**（`--strategy-limit <name>:<max_open_positions>:<max_notional_usd>`，
  可重复；`-` 或空段表示该维度不限）。生产由环境变量 **`HFT_STRATEGY_LIMITS`**（逗号分隔）透传到内核。
  超限的入场**在下单层之前**被拒，计入
  `engine.stats.strategyLimitRejected` 与该策略的 `limitRejected`（每次评估尝试计一次，语义同 `placeRejected`）。
- `engine.stats.strategies[]` 给出每策略的会话账本：`openPositions`/`openNotionalUsd`（实况敞口）、
  `ordersPlaced`/`ordersRejected`/`limitRejected`（入场上报）、`closedTrades`/`wins`/`losses`/`feesUsd`/`netPnlUsd`
  （已实现盈亏）。默认无任何限额配置 → 行为与单策略时代一致。

## 4. 生命周期与开关

| method | 说明 |
|:---|:---|
| `strategy.list` | 列出内核支持的策略及启用状态 |
| `strategy.enable` | `{ name, enabled }` 开关某策略 |
| `strategy.load` | 加载用户层动态库（feature 开启时） |

回合切换时内核调用 `on_round(slot)`，策略应在此清空本回合状态。

## 5. 常见问题

- **为什么我的策略能编译但不下单？** 信号只表达意图，是否成交取决于内核的风控/资金/入场闸门（例如 `min-time-left`、入场上限、资金预扣）。
- **策略能自己平仓吗？** 不能。平仓由内核的持仓/出场策略负责；策略只发 `Buy`/`Sell`/`Hold`。
- **能访问盘口深度吗？** `MarketTick` 目前给最优买卖价与 mid；更深的盘口将随行情层扩展逐步加入契约（保持向后兼容）。

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
