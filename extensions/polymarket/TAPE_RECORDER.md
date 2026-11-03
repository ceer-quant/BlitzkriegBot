# poly-tape-recorder — issue #860 TWAP/WS 落盘录制器

独立录制器 bin（不进生产路径）：连续录制 Polymarket CLOB market WebSocket
（5m/15m updown 轮的 book/price_change/last_trade_price）、Binance 现货成交
（BTC/ETH-USDT trade）与现货 TWAP 代理序列，逐行落盘 jsonl，并输出
p50/p99 延迟与断线率报告。是 #854 实盘准入门的数据面前置。

## 构建 / 运行

```bash
cargo build --release -p polymarket-extension --bin poly_tape_recorder --locked
./target/release/poly_tape_recorder \
  --assets BTC --rounds 300,900 \
  --out-dir /path/to/tape-out \
  --duration 2100            # 秒；不传则一直跑到 Ctrl-C
```

**必须用 release build**：poly market 频道稳态 600–900 帧/s，debug build 的
serde 解析吞吐跟不上，会被场地以
`close 1013 "slow consumer: send buffer full"` 踢线（实测 debug 90s 内
2 次 unplanned 断线；release 同链路 0 次）。

### 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--assets` | `BTC` | 现货与轮发现标的（逗号分隔） |
| `--rounds` | `300,900` | 轮长秒（300=5m、900=15m），决定 updown slug 轮转 |
| `--out-dir` | `std::env::temp_dir()/poly-tape-860` | jsonl 与报告输出目录 |
| `--duration` | 不限 | 运行秒数，到点收盘中并出报告 |
| `--no-spot` | 关 | 不连 Binance 现货（只录 poly + TWAP 不成立，慎用） |
| `--backoff-ms` | `2000` | unplanned 断线重连退避 |
| `--twap-interval-ms` | `5000` | TWAP 采样输出间隔 |
| `--heartbeat-ms` | `30000` | 进度心跳行间隔 |
| `--self-test` | — | 离线自检（13 组用例，不联网），退出码同主程序 |

网络纪律：仅 https/wss + 主机白名单（`ws-subscriptions-clob.polymarket.com`、
`gamma-api.polymarket.com`、`stream.binance.com`），所有 URL 经唯一构造器
先校验后拨号；零凭据。

## 行形（jsonl，每行一个 JSON 对象，逐行 flush）

```json
{"channel":"poly_market","symbol":"book","recv_ts":1793673427656,"server_ts":1793673427487,"seq":42,"payload":{...}}
```

- `channel`：`poly_market`（CLOB market WS 帧）／`spot`（现货 trade）／
  `twap`（TWAP 代理采样）／`_round`（轮发现）／`_event`（连接生命周期）／
  `_report`（收盘中报告副本）。
- `seq`：按 channel 类别（poly／spot／other）各自连续递增，断线不清零，
  可用于对账丢行。
- `recv_ts`：本机收到帧的毫秒时间戳；`server_ts`：帧内场地时间戳
  （poly 取顶层 `timestamp`（ms 字符串，秒级归一），spot 取 `T`/`E`）；
  缺失时为 `null`，该帧不进延迟统计。

## 报告口径（`report-*.json`）

- **延迟** = `server_ts − recv_ts`（毫秒，工单口径，含场地时钟偏差，
  可为负）。分位数 p50/p99/max 按源（poly_market／spot）分别统计。
- **断线率** = unplanned 断线累计时长 ÷ 会话时长 × 100。轮滚动导致的
  planned 重连（token 集变化）单独计数，不计入断线率。
- **失败判定**：断线率 > 1%/h **或** p99 延迟 > 1000ms ⇒ `verdict.pass=false`，
  退出码 2；否则退出码 0；用法/运行错误退出码 1。

## 踩坑实录（改协议形态前必读）

1. **应用层心跳**：CLOB market WS 需要客户端每 5s 发文本帧 `"PING"`，
   场地回 `"PONG"`（非 WS 协议层 ping/pong）。不发则静默连接约 30s 后
   零帧，需自建静默看门狗（30s 无帧视为 unplanned 断线）。
2. **订阅帧最小形态 + 键序敏感**：只接受
   `{"assets_ids":[...],"type":"market"}`（注意字段名是 `assets_ids`）。
   SDK 形态多余的 `operation`/`markets`/`initial_dump` 字段会被静默拒订阅
   （连接保活、PONG 正常、但零数据帧）；且解析器对键序敏感——
   `assets_ids` 必须是首个键。实测 A/B（2026-11-03，同 token 同链路）：
   `assets_ids` 在前 = 1383 帧，`type` 在前 = 0 帧。
3. **数组合帧**：venue 会把多个事件合成 JSON 数组推，`server_ts` 取数组内
   首个带 `timestamp` 的元素。

## 产物

- `tape-<UTC时间戳>-<pid>.jsonl`：上述行形混流。
- `report-<UTC时间戳>-<pid>.json`：会话元数据 + 分源统计 + verdict。

录制产物（jsonl/报告）不进仓库，落盘目录自定。
