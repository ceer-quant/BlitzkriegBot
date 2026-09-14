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
`spread_arb` 已作为 `builtins::SpreadArbStrategy` 注册在策略引擎中，可用 `strategy.list` / `strategy.enable` 查询与开关。

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
