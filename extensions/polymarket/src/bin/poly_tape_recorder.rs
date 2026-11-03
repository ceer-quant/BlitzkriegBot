//! `poly_tape_recorder` — issue #860：PolyBolt TWAP 与 market WebSocket 数据
//! 落盘录制器（独立 bin，不改任何生产路径）。
//!
//! 录什么（工单原文：记录 TWAP、现货、5m/15m book/trades、recv_ts/server_ts、
//! 断线与 seq）：
//!
//! * `poly_market` — Polymarket CLOB market WebSocket
//!   (`wss://ws-subscriptions-clob.polymarket.com/ws/market`) 的**原始帧逐条
//!   落盘**：`book`（全量盘口）、`price_change`、`last_trade_price`（成交）。
//!   订阅集 = 当前 5m + 15m 轮的 up/down token，由 Gamma 按
//!   `${asset}-updown-{5m,15m}-${slot}` slug 自动发现并随轮滚动（复用
//!   `gamma::slug_for`，与 discovery.rs 同一套 slug 纪律）。轮滚动导致的重连
//!   记为 planned，与链路故障（unplanned）分开记账。
//! * `spot` — Binance 现货 `@trade` 组合流（默认 btcusdt/ethusdt，可
//!   `--assets` 扩展），原始帧逐条落盘。
//! * `twap` — 每 `--twap-interval-ms`（默认 1s）每资产一行：现货最后价 +
//!   由现货 tape 推导的 60s/300s(5m)/900s(15m) 尾随 TWAP。**口径**：TWAP =
//!   最后价阶梯函数的时间加权均值（代理）；官方结算是 Chainlink BTC/USD
//!   TWAP 数据流，代理误差已由 issue #859 量化（~5%），此处落盘的正是
//!   可与官方结算离线对标的代理序列。
//! * `_round` — 轮发现：token ↔ 轮（asset/round_sec/slot/condition_id/
//!   expires_at_ms/seconds_delay）映射，是离线把 book/trades 归到 5m/15m 轮
//!   的钥匙。
//! * `_event` — 连接生命周期：connected / disconnected(planned|unplanned) /
//!   stall / gamma 故障转移，断线与重连事件按工单要求落行。
//! * `_report` — 会话结束时的汇总行（p50/p99、断线率、判定）。
//!
//! 每行统一形状：`{channel, symbol, recv_ts, server_ts, seq, payload}`。
//! `recv_ts` = 本机时钟（unix ms）；`server_ts` = 场地时间戳（poly: 帧内
//! `timestamp` 字段，spot: trade 的 `T`），缺失时为 null 且不进延迟统计。
//! `seq` = 按 channel 类别（poly_market / spot / 其余）各自连续递增的行号。
//!
//! 延迟口径（与工单一致）：`latency = server_ts − recv_ts`（毫秒，含场地
//! 时钟偏差 + 单向网络时延；正数 = 场地时钟超前）。p50/p99 为 nearest-rank。
//!
//! 断线率口径：`unplanned 断线时长 / 会话时长 × 100%`（planned 轮滚动重连
//! 单独计数、不进断线率）。**失败判定（工单阈值）**：断线率 > 1%/h 或任一
//! 来源 p99 延迟 > 1000ms → 汇总报告 FAIL 且退出码 2；正常 0；用法错误 1。
//!
//! 网络纪律（Mimosa）：仅 https/wss + 主机白名单 + 唯一 URL 构造器
//! [`build_url`] 先校验后拨号；公开市场数据，零凭据。
//!
//! 用法：
//! ```text
//! poly-tape-recorder [--assets BTC,ETH] [--rounds 300,900] [--out-dir DIR]
//!                    [--duration SEC] [--no-spot] [--backoff-ms MS]
//!                    [--twap-interval-ms MS] [--heartbeat-ms MS] [--self-test]
//! ```
//! `--duration 0`（默认）运行到 Ctrl-C。jsonl 落在 `--out-dir` 下
//! `tape-<UTC时间戳>-<pid>.jsonl`，结束额外写 `report-*.json`。
//!
//! 离线自测：`--self-test`（无需网络）或 `cargo test -p polymarket-extension`。

use blitzkrieg_market_api::net::now_ms;
use futures_util::{SinkExt, StreamExt};
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use polymarket_client_sdk_v2::gamma::types::request::MarketsRequest;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

// ── 常量：端点、看门狗、阈值 ────────────────────────────────────────────────

/// 主机白名单（Mimosa 纪律）：录制器只许拨这三个公开数据主机。
const ALLOWED_HOSTS: &[&str] = &[
    "ws-subscriptions-clob.polymarket.com", // Polymarket CLOB market WS
    "gamma-api.polymarket.com",             // 轮发现
    "stream.binance.com",                   // 现货 @trade
];

const POLY_WS_HOST: &str = "ws-subscriptions-clob.polymarket.com";
const POLY_WS_MARKET_PATH: &str = "/ws/market";
const GAMMA_HOST: &str = "gamma-api.polymarket.com";
const BINANCE_WS_HOST: &str = "stream.binance.com";

/// 连接与静默看门狗：无帧超过该时长视为 unplanned 断线（场地半开连接在
/// TCP 层不可见，只能靠数据静默兜底）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLY_STALL: Duration = Duration::from_secs(30);
const SPOT_STALL: Duration = Duration::from_secs(60);

/// CLOB WS 应用层心跳间隔（客户端 "PING" 文本帧；SDK 默认同拍 5s）。
const POLY_PING_INTERVAL: Duration = Duration::from_secs(5);

/// 工单失败阈值：断线 >1%/h、p99 >1s。
const MAX_DISCONNECT_RATE_PCT: f64 = 1.0;
const MAX_P99_LATENCY_MS: i64 = 1_000;

/// 轮滚动（planned）重连后的握手间隔，与故障退避区分开。
const PLANNED_RECONNECT_DELAY: Duration = Duration::from_millis(250);
/// Gamma 轮发现节奏，与 discovery.rs 的 5s 同拍。
const DISCOVERY_POLL: Duration = Duration::from_secs(5);
/// TWAP 推导窗的保留余量（缓冲裁剪边界）。
const TWAP_WINDOW_MAX_MS: i64 = 900_000;

// ── 网络纪律：唯一 URL 构造器 ───────────────────────────────────────────────

/// 唯一的 URL 构造点：scheme 只许 https/wss、host 必须在白名单内、path 必须
/// 以 `/` 开头（base 可为空串）。先校验，后返回；调用方拿到的字符串才允许
/// 去拨号。
fn build_url(scheme: &str, host: &str, path: &str) -> Result<String, String> {
    if scheme != "https" && scheme != "wss" {
        return Err(format!("scheme 不在 https/wss 内: {scheme:?}"));
    }
    if !ALLOWED_HOSTS.contains(&host) {
        return Err(format!("host 不在白名单内: {host:?}"));
    }
    if !path.is_empty() && !path.starts_with('/') {
        return Err(format!("path 必须以 / 开头: {path:?}"));
    }
    Ok(format!("{scheme}://{host}{path}"))
}

/// 环境变量覆盖的 WS base（如 `wss://ws-subscriptions-clob.polymarket.com`）
/// → (scheme, host)。同样只接受白名单主机，不允许 path（path 由构造器拼）。
fn parse_ws_base(raw: &str) -> Result<(String, String), String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| format!("WS base 缺 scheme: {raw:?}"))?;
    if scheme != "wss" {
        return Err(format!("WS base 必须是 wss: {raw:?}"));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("WS base 无 host: {raw:?}"));
    }
    // host 带端口时剥掉再对白名单（白名单主机都不带端口）。
    let bare = host.split(':').next().unwrap_or_default();
    if rest.contains('/') {
        return Err(format!("WS base 不允许带 path: {raw:?}"));
    }
    // 复用构造器做白名单与 scheme 校验（path 为空）。
    build_url(scheme, bare, "").map(|_| (scheme.to_string(), bare.to_string()))
}

// ── 行构造与落盘 ────────────────────────────────────────────────────────────

/// 统一行形状（纯函数，便于测试）：每行都带
/// `channel/symbol/recv_ts/server_ts/seq/payload`。
fn build_row(
    channel: &str,
    symbol: Option<&str>,
    recv_ts: i64,
    server_ts: Option<i64>,
    seq: u64,
    payload: Value,
) -> Value {
    json!({
        "channel": channel,
        "symbol": symbol,
        "recv_ts": recv_ts,
        "server_ts": server_ts,
        "seq": seq,
        "payload": payload,
    })
}

/// 落盘通道：单 jsonl 文件 + 每来源连续 seq。写线程逐行 flush（「立即落盘」
/// 的 P0 语义：录制器被 kill 最多丢正在写的那一行）。
#[derive(Clone)]
struct Tape {
    tx: std::sync::mpsc::Sender<String>,
    seq_poly: Arc<AtomicU64>,
    seq_spot: Arc<AtomicU64>,
    seq_other: Arc<AtomicU64>,
}

impl Tape {
    fn start(path: PathBuf) -> Result<(Self, std::thread::JoinHandle<()>), String> {
        let file = std::fs::File::create(&path)
            .map_err(|e| format!("创建 {} 失败: {e}", path.display()))?;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::Builder::new()
            .name("tape-writer".into())
            .spawn(move || {
                let mut w = std::io::BufWriter::new(file);
                for line in rx {
                    if writeln!(w, "{line}").is_err() {
                        break; // 磁盘没了：丢行好过静默吞线程
                    }
                    let _ = w.flush();
                }
                let _ = w.flush();
            })
            .map_err(|e| format!("起写线程失败: {e}"))?;
        Ok((
            Self {
                tx,
                seq_poly: Arc::new(AtomicU64::new(0)),
                seq_spot: Arc::new(AtomicU64::new(0)),
                seq_other: Arc::new(AtomicU64::new(0)),
            },
            handle,
        ))
    }

    fn next_seq(&self, channel: &str) -> u64 {
        let counter = match channel {
            "poly_market" => &self.seq_poly,
            "spot" => &self.seq_spot,
            _ => &self.seq_other,
        };
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 组行并落盘。`recv_ts` 由调用方在解析前打点（延迟口径用它）。
    fn emit(
        &self,
        channel: &str,
        symbol: Option<&str>,
        recv_ts: i64,
        server_ts: Option<i64>,
        payload: Value,
    ) {
        let row = build_row(
            channel,
            symbol,
            recv_ts,
            server_ts,
            self.next_seq(channel),
            payload,
        );
        let _ = self.tx.send(row.to_string());
    }

    /// 生命周期/发现/汇总行（无场地时间戳，recv 当场打点）。
    fn emit_event(&self, channel: &str, symbol: &str, payload: Value) {
        self.emit(channel, Some(symbol), now_ms(), None, payload);
    }
}

// ── 统计与断线记账 ──────────────────────────────────────────────────────────

/// 每来源（poly_market / spot）的会话统计。锁粒度=每来源一把，热路径只在
/// 每消息一次加锁，量级（几十条/秒）完全无压力。
#[derive(Default)]
struct SourceStat {
    msgs: u64,
    bytes: u64,
    latencies: Vec<i64>,
    first_msg_recv_ts: Option<i64>,
    last_msg_recv_ts: Option<i64>,
    connect_failures: u64,
    unplanned_disconnects: u64,
    planned_disconnects: u64,
    unplanned_downtime_ms: u64,
    planned_downtime_ms: u64,
    stalls: u64,
}

impl SourceStat {
    fn note_msg(&mut self, bytes: u64, recv: i64, server_ts: Option<i64>) {
        self.msgs += 1;
        self.bytes += bytes;
        if self.first_msg_recv_ts.is_none() {
            self.first_msg_recv_ts = Some(recv);
        }
        self.last_msg_recv_ts = Some(recv);
        if let Some(ts) = server_ts {
            self.latencies.push(ts - recv);
        }
    }
}

type StatHandle = Arc<Mutex<SourceStat>>;

fn lock(stat: &StatHandle) -> std::sync::MutexGuard<'_, SourceStat> {
    // 中毒锁 = 某处 panic 了，但计数仍是能拿到的最好证据：不放大失败。
    stat.lock().unwrap_or_else(|p| p.into_inner())
}

/// 断线原因分类。Planned = 录制器自己安排的重连（轮滚动），Unplanned =
/// 链路故障（EOF/错误/stall），Stop = 会话结束。
#[derive(Debug, Clone, PartialEq)]
enum BreakReason {
    Planned(String),
    Unplanned(String),
    Stop,
}

// ── 延迟分位数与失败判定 ────────────────────────────────────────────────────

/// nearest-rank 分位数（线性数组，p∈[0,100]）。空集 → None。
fn percentile(mut v: Vec<i64>, p: f64) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let idx = ((p / 100.0) * (v.len() as f64 - 1.0)).round() as usize;
    Some(v[idx.min(v.len() - 1)])
}

/// 工单失败判定：断线率 >1%/h 或 p99 >1000ms → 返回失败原因列表（空=PASS）。
fn verdict_for_source(name: &str, rate_pct: f64, p99_ms: Option<i64>) -> Vec<String> {
    let mut failures = Vec::new();
    if rate_pct > MAX_DISCONNECT_RATE_PCT {
        failures.push(format!(
            "{name} 断线率 {rate_pct:.3}% > {MAX_DISCONNECT_RATE_PCT}%/h"
        ));
    }
    if let Some(p99) = p99_ms.filter(|v| *v > MAX_P99_LATENCY_MS) {
        failures.push(format!("{name} p99 延迟 {p99}ms > {MAX_P99_LATENCY_MS}ms"));
    }
    failures
}

// ── TWAP（现货最后价阶梯的时间加权均值）────────────────────────────────────

/// 尾随窗口 TWAP：维护 `(t_ms, price)` 段阶梯（价格自 t 起生效直到下一段）。
/// 覆盖不满窗时如实返回 covered_ms（代理诚实口径，不脑补窗外价格）。
#[derive(Default)]
struct TwapTracker {
    samples: VecDeque<(i64, f64)>,
}

impl TwapTracker {
    fn push(&mut self, now_ms: i64, t_ms: i64, price: f64) {
        if !price.is_finite() {
            return;
        }
        // 乱序/重复：跳过（tape 本身保真，这里只做推导）。
        if self
            .samples
            .back()
            .is_some_and(|&(lt, lp)| t_ms < lt || (t_ms == lt && price == lp))
        {
            return;
        }
        self.samples.push_back((t_ms, price));
        // 裁剪：保留能跨住 `now - 15m - 余量` 的最老一段。
        let cutoff = now_ms - TWAP_WINDOW_MAX_MS - 30_000;
        while self.samples.len() >= 2 && self.samples[1].0 < cutoff {
            self.samples.pop_front();
        }
    }

    /// `window` 毫秒尾随 TWAP。None = 窗内无价格。
    fn twap(&self, now_ms: i64, window_ms: i64) -> Option<(f64, i64, usize, bool)> {
        let start = now_ms - window_ms;
        let v = &self.samples;
        if v.is_empty() {
            return None;
        }
        // 跳过整段落在窗外之前的段（保留跨 start 的那一段）。
        let mut idx = 0;
        while idx + 1 < v.len() && v[idx + 1].0 <= start {
            idx += 1;
        }
        let first_t = v[idx].0.max(start);
        if first_t >= now_ms {
            return None; // 全是未来时间戳（时钟漂移），不脑补
        }
        let mut sum = 0.0;
        let mut i = idx;
        while i < v.len() {
            let (t, p) = v[i];
            let seg_start = t.max(start);
            let seg_end = if i + 1 < v.len() {
                v[i + 1].0.min(now_ms)
            } else {
                now_ms
            };
            if seg_end > seg_start {
                sum += p * (seg_end - seg_start) as f64;
            }
            i += 1;
        }
        let covered = now_ms - first_t;
        if covered <= 0 {
            return None;
        }
        let full = v[idx].0 <= start;
        Some((sum / covered as f64, covered, v.len() - idx, full))
    }

    fn last_price(&self) -> Option<f64> {
        self.samples.back().map(|&(_, p)| p)
    }

    fn snapshot(&self, now_ms: i64) -> Value {
        fn fmt(t: Option<(f64, i64, usize, bool)>) -> Value {
            match t {
                None => Value::Null,
                Some((v, covered, n, full)) => json!({
                    "value": v, "covered_ms": covered, "n": n, "full_window": full,
                }),
            }
        }
        json!({
            "spot_last": self.last_price(),
            "twap_60s": fmt(self.twap(now_ms, 60_000)),
            "twap_300s": fmt(self.twap(now_ms, 300_000)),
            "twap_900s": fmt(self.twap(now_ms, 900_000)),
        })
    }
}

type Trackers = Arc<Mutex<HashMap<String, TwapTracker>>>;

// ── 帧解析（纯函数）────────────────────────────────────────────────────────

/// Polymarket 帧的场地时间戳：对象取 `timestamp`，数组取首个带该字段的元素；
/// 字符串/数字都收，秒级（<1e12）归一到毫秒。缺 → None（不进延迟统计）。
fn poly_server_ts(v: &Value) -> Option<i64> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    for it in items {
        // 元素缺 timestamp / 解析失败 → 继续看下一个元素，不让一帧的脏字段
        // 丢掉整帧的时间戳机会。
        let ts = match it.get("timestamp") {
            Some(Value::String(s)) => s.parse::<i64>().ok(),
            Some(Value::Number(n)) => n.as_i64(),
            _ => None,
        };
        let Some(ts) = ts else { continue };
        return Some(if ts < 1_000_000_000_000 {
            ts * 1000
        } else {
            ts
        });
    }
    None
}

/// Polymarket 行的 symbol：单对象取 `event_type`（book/price_change/
/// last_trade_price/…），数组帧（venue 会合帧）→ None，靠 payload 自带。
fn poly_symbol(v: &Value) -> Option<&str> {
    v.get("event_type").and_then(|e| e.as_str())
}

/// 市场频道订阅帧：**最小形态** `{"assets_ids":[...],"type":"market"}`。
/// 实测（2026-11-03 裸探针）：SDK 形态多出的 `operation`/`markets`/
/// `initial_dump` 字段会被场地静默忽略订阅（连接保活但零数据帧），官方
/// 文档的最小帧立即开始推 book/price_change/last_trade_price。
fn market_sub_frame(asset_ids: &[String]) -> String {
    // 场地解析器对键序敏感：`assets_ids` 必须是首个键。不能走
    // serde_json::Map（键序受 preserve_order 特性支配，本仓库恰好开着
    // insertion-order，写 "type" 在前就真发 "type" 在前）——实测 2026-11-03
    // A/B：同 token 同链路，assets_ids 在前=1383 帧，type 在前=0 帧。
    // 手工拼帧，键序钉死；token 逐个过 json! 转义。
    let ids: Vec<String> = asset_ids.iter().map(|id| json!(id).to_string()).collect();
    format!(r#"{{"assets_ids":[{}],"type":"market"}}"#, ids.join(","))
}

/// Binance 现货帧解析：兼容组合流包裹（`{stream, data}`）与单流裸帧。
/// 只收 `e=trade`。返回 (symbol, server_ts, 价格(可缺), 原始 data 对象)。
fn parse_spot_frame(text: &str) -> Option<(String, i64, Option<f64>, Value)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let data = if v.get("stream").is_some() {
        v.get("data")?.clone()
    } else {
        v.clone()
    };
    if data.get("e").and_then(|e| e.as_str()) != Some("trade") {
        return None;
    }
    let symbol = data.get("s")?.as_str()?.to_string();
    let ts = data
        .get("T")
        .and_then(Value::as_i64)
        .or_else(|| data.get("E").and_then(Value::as_i64))?;
    let price = data
        .get("p")
        .and_then(Value::as_str)
        .and_then(|p| p.parse::<f64>().ok());
    Some((symbol, ts, price, data))
}

/// `BTCUSDT` → `BTC`（本录制器只构造 usdt@trade 流，反推安全）。
fn symbol_to_asset(symbol: &str) -> String {
    symbol.trim_end_matches("USDT").to_ascii_uppercase()
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Args {
    assets: Vec<String>,
    rounds: Vec<i64>,
    out_dir: PathBuf,
    duration_secs: u64,
    self_test: bool,
    no_spot: bool,
    backoff_ms: u64,
    twap_interval_ms: u64,
    heartbeat_ms: u64,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        assets: vec!["BTC".to_string()],
        rounds: vec![300, 900],
        out_dir: PathBuf::from("./poly-tape-860"),
        duration_secs: 0,
        self_test: false,
        no_spot: false,
        backoff_ms: 2_000,
        twap_interval_ms: 1_000,
        heartbeat_ms: 60_000,
    };
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{flag} 缺参数值"))
        };
        match flag {
            "--assets" => {
                let v = next()?;
                a.assets = v
                    .split(',')
                    .map(|s| s.trim().to_ascii_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if a.assets.is_empty() {
                    return Err("--assets 不能为空".into());
                }
            }
            "--rounds" => {
                let v = next()?;
                a.rounds = v
                    .split(',')
                    .map(|s| s.trim().parse::<i64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("--rounds 解析失败: {e}"))?;
                if a.rounds.is_empty() {
                    return Err("--rounds 不能为空".into());
                }
            }
            "--out-dir" => a.out_dir = PathBuf::from(next()?),
            "--duration" => {
                a.duration_secs = next()?
                    .trim()
                    .parse()
                    .map_err(|e| format!("--duration: {e}"))?
            }
            "--no-spot" => a.no_spot = true,
            "--backoff-ms" => {
                a.backoff_ms = next()?
                    .trim()
                    .parse()
                    .map_err(|e| format!("--backoff-ms: {e}"))?
            }
            "--twap-interval-ms" => {
                a.twap_interval_ms = next()?
                    .trim()
                    .parse()
                    .map_err(|e| format!("--twap-interval-ms: {e}"))?
            }
            "--heartbeat-ms" => {
                a.heartbeat_ms = next()?
                    .trim()
                    .parse()
                    .map_err(|e| format!("--heartbeat-ms: {e}"))?
            }
            "--self-test" => a.self_test = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other:?}（--help 看用法）")),
        }
        i += 1;
    }
    Ok(a)
}

fn print_help() {
    eprintln!(
        "poly-tape-recorder — issue #860 TWAP/WS 落盘录制器\n\
         用法: poly-tape-recorder [--assets BTC,ETH] [--rounds 300,900] [--out-dir DIR]\n\
         \x20        [--duration SEC] [--no-spot] [--backoff-ms MS]\n\
         \x20        [--twap-interval-ms MS] [--heartbeat-ms MS] [--self-test]\n\
         退出码: 0=PASS, 2=FAIL(断线率>1%/h 或 p99>1s), 1=用法/运行错误"
    );
}

// ── 主流程 ──────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_args(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("poly-tape-recorder: {e}");
            print_help();
            std::process::exit(1);
        }
    };
    if parsed.self_test {
        std::process::exit(run_self_test());
    }
    let code = tokio_main(parsed);
    std::process::exit(code);
}

#[tokio::main]
async fn tokio_main(args: Args) -> i32 {
    let started_at_ms = now_ms();
    let start = std::time::Instant::now();

    // WS base：环境变量可换（仍过白名单），默认官方端点。
    let ws_base = match std::env::var("POLYMARKET_WS_URL") {
        Ok(raw) => match parse_ws_base(raw.trim()) {
            Ok((_s, host)) => host,
            Err(e) => {
                eprintln!("poly-tape-recorder: POLYMARKET_WS_URL 拒绝: {e}");
                return 1;
            }
        },
        Err(_) => POLY_WS_HOST.to_string(),
    };
    let poly_ws_url = match build_url("wss", &ws_base, POLY_WS_MARKET_PATH) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("poly-tape-recorder: {e}");
            return 1;
        }
    };
    let gamma_base = match build_url("https", GAMMA_HOST, "") {
        Ok(u) => u,
        Err(e) => {
            eprintln!("poly-tape-recorder: {e}");
            return 1;
        }
    };
    let spot_streams: Vec<String> = args
        .assets
        .iter()
        .map(|a| format!("{}usdt@trade", a.to_lowercase()))
        .collect();
    let spot_ws_url = match build_url(
        "wss",
        BINANCE_WS_HOST,
        &format!("/ws/{}", spot_streams.join("/")),
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("poly-tape-recorder: {e}");
            return 1;
        }
    };

    // 输出目录与 tape 文件。测试临时目录纪律：默认参数化，落调用方给的目录。
    if let Err(e) = std::fs::create_dir_all(&args.out_dir) {
        eprintln!(
            "poly-tape-recorder: 建目录 {} 失败: {e}",
            args.out_dir.display()
        );
        return 1;
    }
    let stamp = chrono::DateTime::from_timestamp_millis(started_at_ms)
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%SZ");
    let tape_path = args
        .out_dir
        .join(format!("tape-{stamp}-{}.jsonl", std::process::id()));
    let (tape, writer) = match Tape::start(tape_path.clone()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("poly-tape-recorder: {e}");
            return 1;
        }
    };

    // 共享态。
    let (stop_tx, stop_rx) = watch::channel(false);
    let (tokens_tx, tokens_rx) = watch::channel(Vec::<String>::new());
    let poly_stat: StatHandle = Arc::new(Mutex::new(SourceStat::default()));
    let spot_stat: StatHandle = Arc::new(Mutex::new(SourceStat::default()));
    let trackers: Trackers = Arc::new(Mutex::new(HashMap::new()));
    let twap_rows = Arc::new(AtomicU64::new(0));
    let backoff = Duration::from_millis(args.backoff_ms);
    let twap_interval = Duration::from_millis(args.twap_interval_ms.max(100));
    let heartbeat = Duration::from_millis(args.heartbeat_ms);

    tape.emit_event(
        "_event",
        "session",
        json!({
            "kind": "session_start",
            "issue": 860,
            "assets": args.assets,
            "rounds": args.rounds,
            "poly_ws_url": poly_ws_url,
            "spot_ws_url": spot_ws_url,
            "out_file": tape_path.display().to_string(),
            "pid": std::process::id(),
        }),
    );

    // Gamma 客户端（轮发现；失败只记事件不致命——轮会在下个 tick 重试）。
    let gamma = match GammaClient::new(&gamma_base) {
        Ok(c) => Some(c),
        Err(e) => {
            tape.emit_event(
                "_event",
                "discovery",
                json!({"kind": "gamma_client_init_failed", "detail": e.to_string()}),
            );
            None
        }
    };

    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // 轮发现：slug → Gamma → token 集（watch 广播）+ `_round` 行。
    if let Some(gamma) = gamma {
        tasks.push(tokio::spawn(discovery_loop(
            gamma,
            args.assets.clone(),
            args.rounds.clone(),
            tokens_tx,
            tape.clone(),
            stop_rx.clone(),
        )));
    }

    // Polymarket market WS 录制循环。
    tasks.push(tokio::spawn(poly_market_loop(
        poly_ws_url.clone(),
        tape.clone(),
        tokens_rx.clone(),
        stop_rx.clone(),
        Arc::clone(&poly_stat),
        backoff,
    )));

    // Binance 现货录制循环（可关）。
    if !args.no_spot {
        tasks.push(tokio::spawn(spot_loop(
            spot_ws_url.clone(),
            tape.clone(),
            stop_rx.clone(),
            Arc::clone(&spot_stat),
            Arc::clone(&trackers),
            backoff,
        )));
        // TWAP 推导行。
        tasks.push(tokio::spawn(twap_loop(
            Arc::clone(&trackers),
            tape.clone(),
            stop_rx.clone(),
            Arc::clone(&twap_rows),
            twap_interval,
        )));
    }

    // 心跳（仅 stderr，不进 tape）。
    tasks.push(tokio::spawn(heartbeat_loop(
        heartbeat,
        stop_rx.clone(),
        Arc::clone(&poly_stat),
        Arc::clone(&spot_stat),
        Arc::clone(&twap_rows),
        tokens_rx.clone(),
        start,
    )));

    // 停止条件：--duration 到点或 Ctrl-C。duration=0（运行到 Ctrl-C）时
    // 该分支被前置条件禁用；留一个超长兜底时长防误评估。
    let duration = if args.duration_secs > 0 {
        Duration::from_secs(args.duration_secs)
    } else {
        Duration::from_secs(86_400 * 365)
    };
    tokio::select! {
        _ = tokio::time::sleep(duration), if args.duration_secs > 0 => {
            eprintln!("[poly-tape] --duration {}s 到点，收盘中", args.duration_secs);
        }
        r = tokio::signal::ctrl_c() => {
            if let Err(e) = r {
                eprintln!("[poly-tape] ctrl_c 监听失败: {e}");
            }
        }
    }
    let _ = stop_tx.send(true);

    // 收任务（上限 15s，超时强杀；强杀的来源统计以已计数为准）。
    for h in tasks {
        if tokio::time::timeout(Duration::from_secs(15), h)
            .await
            .is_err()
        {
            eprintln!("[poly-tape] 任务 15s 未退出，强杀");
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let tokens_final = tokens_rx.borrow().clone();

    // 汇总报告（工单：会话结束时输出 p50/p99、断线次数与断线率、判定）。
    let report = build_report(
        started_at_ms,
        now_ms(),
        duration_ms,
        &tape_path,
        &args.assets,
        &args.rounds,
        lock(&poly_stat),
        lock(&spot_stat),
        args.no_spot,
        twap_rows.load(Ordering::Relaxed),
        &tokens_final,
    );
    let exit_code = if report["verdict"]["pass"].as_bool().unwrap_or(false) {
        0
    } else {
        2
    };
    tape.emit("_report", Some("session"), now_ms(), None, report.clone());

    // 关写线程（drop 最后一个 sender → 写线程排空后退出并 flush）。
    drop(tape);
    let _ = writer.join();

    let report_path = args
        .out_dir
        .join(format!("report-{stamp}-{}.json", std::process::id()));
    let rendered = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into());
    if let Err(e) = std::fs::write(&report_path, rendered.clone() + "\n") {
        eprintln!("[poly-tape] 写 {} 失败: {e}", report_path.display());
    }
    eprintln!("{rendered}");
    eprintln!(
        "[poly-tape] tape={} report={} exit={exit_code}",
        tape_path.display(),
        report_path.display()
    );
    exit_code
}

// ── 轮发现 ──────────────────────────────────────────────────────────────────

/// 每 5s：对每个 (asset, round) 用 `slug_for` 造 slug 查 Gamma，取首个活跃且
/// 未过期的市场，抽 up/down token。token 集变化 → watch 广播（poly 循环
/// planned 重连）；单轮签名变化 → `_round` 行（离线归轮的钥匙）。Gamma 故障
/// 只在状态翻转时落 `_event` 行，不打爆 tape。
async fn discovery_loop(
    gamma: polymarket_client_sdk_v2::gamma::Client,
    assets: Vec<String>,
    rounds: Vec<i64>,
    tokens_tx: watch::Sender<Vec<String>>,
    tape: Tape,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut last_sig: HashMap<String, String> = HashMap::new();
    let mut gamma_ok = true;
    let mut tick = tokio::time::interval(DISCOVERY_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
        }
        let now = now_ms();
        let mut new_set: Vec<String> = Vec::new();
        for asset in &assets {
            for &round in &rounds {
                let Some(slug) = polymarket_extension::gamma::slug_for(asset, round, now / 1000)
                else {
                    continue;
                };
                let req = MarketsRequest::builder().slug(vec![slug.clone()]).build();
                let found = match tokio::time::timeout(CONNECT_TIMEOUT, gamma.markets(&req)).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        if gamma_ok {
                            tape.emit_event(
                                "_event",
                                "discovery",
                                json!({"kind": "gamma_error", "detail": e.to_string(), "slug": slug}),
                            );
                            gamma_ok = false;
                        }
                        continue;
                    }
                    Err(_) => {
                        if gamma_ok {
                            tape.emit_event(
                                "_event",
                                "discovery",
                                json!({"kind": "gamma_timeout", "slug": slug}),
                            );
                            gamma_ok = false;
                        }
                        continue;
                    }
                };
                if !gamma_ok {
                    tape.emit_event("_event", "discovery", json!({"kind": "gamma_recovered"}));
                    gamma_ok = true;
                }
                let Some(m) = found.into_iter().next() else {
                    continue;
                };
                if m.closed.unwrap_or(false) || !m.active.unwrap_or(false) {
                    continue;
                }
                let Some(condition_id) = m.condition_id.map(|c| c.to_string()) else {
                    continue;
                };
                let outcomes = m.outcomes.clone().unwrap_or_default();
                let tokens = m.clob_token_ids.clone().unwrap_or_default();
                if outcomes.len() < 2 || tokens.len() < 2 {
                    continue;
                }
                let up_idx = outcomes
                    .iter()
                    .position(|o| matches!(o.to_lowercase().as_str(), "up" | "yes"));
                let down_idx = outcomes
                    .iter()
                    .position(|o| matches!(o.to_lowercase().as_str(), "down" | "no"));
                let (Some(ui), Some(di)) = (up_idx, down_idx) else {
                    continue;
                };
                let end_ms = m.end_date.map(|d| d.timestamp_millis()).unwrap_or(0);
                if end_ms <= now {
                    continue;
                }
                // token 直接以 SDK 解出的 U256 十进制字符串进订阅集
                // （与 discovery.rs 喂核的形态一致）。
                let (up_tok, down_tok) = (tokens[ui].to_string(), tokens[di].to_string());
                let key = format!("{asset}/{round}");
                let sig = format!("{condition_id}|{up_tok}|{down_tok}");
                if last_sig.get(&key).map(String::as_str) != Some(sig.as_str()) {
                    tape.emit_event(
                        "_round",
                        asset,
                        json!({
                            "kind": "round_discovered",
                            "asset": asset,
                            "round_sec": round,
                            "slot": end_ms / 1000 / round,
                            "slug": slug,
                            "condition_id": condition_id,
                            "up_token_id": up_tok,
                            "down_token_id": down_tok,
                            "expires_at_ms": end_ms,
                            "seconds_delay": m.seconds_delay,
                            "question": m.question.clone().unwrap_or_default(),
                        }),
                    );
                    last_sig.insert(key, sig);
                }
                new_set.push(up_tok);
                new_set.push(down_tok);
            }
        }
        new_set.sort();
        new_set.dedup();
        // 只在真变化且非空时广播：空集（Gamma 抖动）保持旧订阅，不打断录制。
        if !new_set.is_empty() && *tokens_tx.borrow() != new_set {
            let _ = tokens_tx.send(new_set);
        }
    }
}

// ── Polymarket market WS 循环 ───────────────────────────────────────────────

#[allow(unused_assignments)] // 读循环每次 break 前必先赋值，初值 None 永不读取
async fn poly_market_loop(
    url: String,
    tape: Tape,
    mut tokens_rx: watch::Receiver<Vec<String>>,
    mut stop_rx: watch::Receiver<bool>,
    stat: StatHandle,
    backoff: Duration,
) {
    // 等首个非空 token 集（轮发现通常 ≤10s 内给出）。
    let mut current: Vec<String> = {
        loop {
            tokio::select! {
                r = tokens_rx.changed() => match r {
                    Ok(()) => {
                        let t = tokens_rx.borrow().clone();
                        if !t.is_empty() {
                            break t;
                        }
                    }
                    Err(_) => return,
                },
                _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
            }
        }
    };

    let mut downtime_since: Option<std::time::Instant> = None;
    let mut last_break: Option<BreakReason> = None;
    let mut conn_ok = false;
    loop {
        // 连接（限时）。失败：只记次数+状态翻转事件，downtime 由
        // downtime_since（断线时刻起）持续覆盖，直到重连成功才结算。
        let connected = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok((ws, _))) => {
                if let Some(t) = downtime_since.take() {
                    let dt = t.elapsed().as_millis() as u64;
                    match last_break {
                        Some(BreakReason::Unplanned(_)) => {
                            lock(&stat).unplanned_downtime_ms += dt;
                        }
                        Some(BreakReason::Planned(_)) => {
                            lock(&stat).planned_downtime_ms += dt;
                        }
                        Some(BreakReason::Stop) | None => {}
                    }
                }
                if !conn_ok {
                    tape.emit_event(
                        "_event",
                        "poly_market",
                        json!({"kind": "connected", "tokens": current.len()}),
                    );
                    conn_ok = true;
                }
                Some(ws)
            }
            Ok(Err(e)) => {
                lock(&stat).connect_failures += 1;
                if conn_ok {
                    tape.emit_event(
                        "_event",
                        "poly_market",
                        json!({"kind": "connect_failed", "detail": e.to_string()}),
                    );
                    conn_ok = false;
                }
                None
            }
            Err(_) => {
                lock(&stat).connect_failures += 1;
                if conn_ok {
                    tape.emit_event(
                        "_event",
                        "poly_market",
                        json!({"kind": "connect_timeout", "timeout_ms": CONNECT_TIMEOUT.as_millis()}),
                    );
                    conn_ok = false;
                }
                None
            }
        };

        let Some(ws) = connected else {
            // 连接失败不算一次 unplanned_disconnect（那是一条已建立的链路
            // 断掉）；次数与状态翻转已记，退避后重试。
            if sleep_or_stop(backoff, &mut stop_rx).await {
                break;
            }
            continue;
        };
        // 已建立连接：订阅 + 读循环，直到断线/滚动/停止。
        let mut break_reason: Option<BreakReason> = None;
        let (mut write, mut read) = ws.split();
        let sub = market_sub_frame(&current);
        let sub_ok = write.send(Message::text(sub)).await.is_ok();
        if !sub_ok {
            break_reason = Some(BreakReason::Unplanned("subscribe 发送失败".into()));
        } else {
            // 应用层心跳：CLOB WS 要求客户端周期发 "PING" 文本帧、场地回
            // "PONG"（SDK 默认 5s 发一次、15s 无回包判死）。不发 PING 的
            // 连接会被场地静默——冒烟实测 30s 零帧。
            let mut ping = tokio::time::interval(POLY_PING_INTERVAL);
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ping.tick().await; // 首拍立即返回，先吃掉
            let mut last_frame = std::time::Instant::now();
            loop {
                let residual = POLY_STALL
                    .checked_sub(last_frame.elapsed())
                    .unwrap_or(Duration::ZERO);
                tokio::select! {
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() { break_reason = Some(BreakReason::Stop); break; }
                    }
                    r = tokens_rx.changed() => {
                        match r {
                            Ok(()) => {
                                let new = tokens_rx.borrow().clone();
                                if !new.is_empty() && new != current {
                                    break_reason = Some(BreakReason::Planned("token 集滚动".into()));
                                    break;
                                }
                            }
                            Err(_) => { break_reason = Some(BreakReason::Stop); break; }
                        }
                    }
                    next = read.next() => {
                        last_frame = std::time::Instant::now();
                        match next {
                            Some(Ok(Message::Text(txt))) => {
                                // 应用层心跳回包（以及场地罕见的服务端 PING）：
                                // 只当活性证据，不进 tape、不进统计。
                                if txt == "PONG" || txt == "PING" {
                                    continue;
                                }
                                let recv = now_ms();
                                let bytes = txt.len() as u64;
                                let v: Value = serde_json::from_str(&txt)
                                    .unwrap_or_else(|_| Value::String(txt.to_string()));
                                let server_ts = poly_server_ts(&v);
                                let symbol = poly_symbol(&v).map(str::to_string);
                                lock(&stat).note_msg(bytes, recv, server_ts);
                                tape.emit(
                                    "poly_market",
                                    symbol.as_deref(),
                                    recv,
                                    server_ts,
                                    v,
                                );
                            }
                            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                            Some(Ok(Message::Close(f))) => {
                                break_reason = Some(BreakReason::Unplanned(format!("close 帧: {f:?}")));
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                break_reason = Some(BreakReason::Unplanned(format!("读错误: {e}")));
                                break;
                            }
                            None => {
                                break_reason = Some(BreakReason::Unplanned("流 EOF".into()));
                                break;
                            }
                        }
                    }
                    // 应用层心跳：到点即发 "PING" 文本帧。
                    _ = ping.tick() => {
                        if write.send(Message::text("PING")).await.is_err() {
                            break_reason = Some(BreakReason::Unplanned("PING 发送失败".into()));
                            break;
                        }
                    }
                    _ = tokio::time::sleep(residual) => {
                        lock(&stat).stalls += 1;
                        break_reason = Some(BreakReason::Unplanned(format!(
                            "静默看门狗 {}ms 无帧",
                            POLY_STALL.as_millis()
                        )));
                        break;
                    }
                }
            }
        }
        drop(write);
        drop(read);

        // 断线处理：分类记账 + 事件行 + 退避。
        let reason = break_reason.unwrap_or_else(|| BreakReason::Unplanned("未知退出".into()));
        match reason {
            BreakReason::Stop => break,
            BreakReason::Planned(d) => {
                lock(&stat).planned_disconnects += 1;
                tape.emit_event(
                    "_event",
                    "poly_market",
                    json!({"kind": "planned_rollover_reconnect", "detail": d.clone()}),
                );
                downtime_since = Some(std::time::Instant::now());
                last_break = Some(BreakReason::Planned(d));
                current = tokens_rx.borrow().clone();
                if sleep_or_stop(PLANNED_RECONNECT_DELAY, &mut stop_rx).await {
                    break;
                }
            }
            BreakReason::Unplanned(d) => {
                let msgs = lock(&stat).msgs;
                lock(&stat).unplanned_disconnects += 1;
                tape.emit_event(
                    "_event",
                    "poly_market",
                    json!({"kind": "unplanned_disconnect", "detail": d, "msgs_before": msgs}),
                );
                downtime_since = Some(std::time::Instant::now());
                last_break = Some(BreakReason::Unplanned(d));
                if sleep_or_stop(backoff, &mut stop_rx).await {
                    break;
                }
            }
        }
        if *stop_rx.borrow() {
            break;
        }
    }
}

/// 睡 backoff，期间被叫停则返回 true（该退出了）。
async fn sleep_or_stop(d: Duration, stop_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stop_rx.changed() => *stop_rx.borrow(),
    }
}

// ── Binance 现货循环 ────────────────────────────────────────────────────────

#[allow(unused_assignments)] // 读循环每次 break 前必先赋值，初值 None 永不读取
async fn spot_loop(
    url: String,
    tape: Tape,
    mut stop_rx: watch::Receiver<bool>,
    stat: StatHandle,
    trackers: Trackers,
    backoff: Duration,
) {
    let mut downtime_since: Option<std::time::Instant> = None;
    let mut last_break: Option<BreakReason> = None;
    let mut conn_ok = false;
    loop {
        let connected = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok((ws, _))) => {
                if let Some(t) = downtime_since.take() {
                    let dt = t.elapsed().as_millis() as u64;
                    if matches!(last_break, Some(BreakReason::Unplanned(_))) {
                        lock(&stat).unplanned_downtime_ms += dt;
                    }
                }
                if !conn_ok {
                    tape.emit_event("_event", "spot", json!({"kind": "connected"}));
                    conn_ok = true;
                }
                Some(ws)
            }
            Ok(Err(e)) => {
                lock(&stat).connect_failures += 1;
                if conn_ok {
                    tape.emit_event(
                        "_event",
                        "spot",
                        json!({"kind": "connect_failed", "detail": e.to_string()}),
                    );
                    conn_ok = false;
                }
                None
            }
            Err(_) => {
                lock(&stat).connect_failures += 1;
                if conn_ok {
                    tape.emit_event(
                            "_event",
                            "spot",
                            json!({"kind": "connect_timeout", "timeout_ms": CONNECT_TIMEOUT.as_millis()}),
                        );
                    conn_ok = false;
                }
                None
            }
        };

        if let Some(ws) = connected {
            let (_write, mut read) = ws.split();
            let mut break_reason: Option<BreakReason> = None;
            let mut last_frame = std::time::Instant::now();
            loop {
                let residual = SPOT_STALL
                    .checked_sub(last_frame.elapsed())
                    .unwrap_or(Duration::ZERO);
                tokio::select! {
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() { break_reason = Some(BreakReason::Stop); break; }
                    }
                    next = read.next() => {
                        last_frame = std::time::Instant::now();
                        match next {
                            Some(Ok(Message::Text(txt))) => {
                                let recv = now_ms();
                                let bytes = txt.len() as u64;
                                if let Some((symbol, server_ts, price, raw)) =
                                    parse_spot_frame(&txt)
                                {
                                    lock(&stat).note_msg(bytes, recv, Some(server_ts));
                                    if let Some(p) = price {
                                        let asset = symbol_to_asset(&symbol);
                                        trackers.lock()
                                            .unwrap_or_else(|p| p.into_inner())
                                            .entry(asset)
                                            .or_default()
                                            .push(recv, server_ts, p);
                                    }
                                    tape.emit("spot", Some(&symbol), recv, Some(server_ts), raw);
                                }
                                // 非 trade 帧（如订阅确认）不落 tape 也不计数：
                                // tape 语义=现货成交。
                            }
                            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                            Some(Ok(Message::Close(f))) => {
                                break_reason = Some(BreakReason::Unplanned(format!("close 帧: {f:?}")));
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                break_reason = Some(BreakReason::Unplanned(format!("读错误: {e}")));
                                break;
                            }
                            None => {
                                break_reason = Some(BreakReason::Unplanned("流 EOF".into()));
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(residual) => {
                        lock(&stat).stalls += 1;
                        break_reason = Some(BreakReason::Unplanned(format!(
                            "静默看门狗 {}ms 无帧",
                            SPOT_STALL.as_millis()
                        )));
                        break;
                    }
                }
            }
            drop(read);

            match break_reason.unwrap_or_else(|| BreakReason::Unplanned("未知退出".into())) {
                BreakReason::Stop => break,
                BreakReason::Unplanned(d) => {
                    let msgs = lock(&stat).msgs;
                    lock(&stat).unplanned_disconnects += 1;
                    tape.emit_event(
                        "_event",
                        "spot",
                        json!({"kind": "unplanned_disconnect", "detail": d, "msgs_before": msgs}),
                    );
                    downtime_since = Some(std::time::Instant::now());
                    last_break = Some(BreakReason::Unplanned(d));
                    if sleep_or_stop(backoff, &mut stop_rx).await {
                        break;
                    }
                }
                // 现货流无 planned 重连：静态订阅集，断线全是 unplanned。
                BreakReason::Planned(_) => break,
            }
            if *stop_rx.borrow() {
                break;
            }
            continue;
        }

        // 首连或重连失败：退避重试（downtime 已由 downtime_since 覆盖）。
        if sleep_or_stop(backoff, &mut stop_rx).await {
            break;
        }
        if *stop_rx.borrow() {
            break;
        }
    }
}

// ── TWAP 推导行 ─────────────────────────────────────────────────────────────

async fn twap_loop(
    trackers: Trackers,
    tape: Tape,
    mut stop_rx: watch::Receiver<bool>,
    rows: Arc<AtomicU64>,
    interval: Duration,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
        }
        let now = now_ms();
        let map = trackers.lock().unwrap_or_else(|p| p.into_inner());
        for (asset, tr) in map.iter() {
            if tr.last_price().is_none() {
                continue;
            }
            rows.fetch_add(1, Ordering::Relaxed);
            tape.emit("twap", Some(asset), now, None, tr.snapshot(now));
        }
    }
}

// ── 心跳（stderr）───────────────────────────────────────────────────────────

async fn heartbeat_loop(
    period: Duration,
    mut stop_rx: watch::Receiver<bool>,
    poly: StatHandle,
    spot: StatHandle,
    twap_rows: Arc<AtomicU64>,
    tokens_rx: watch::Receiver<Vec<String>>,
    start: std::time::Instant,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = stop_rx.changed() => { return; }
        }
        eprintln!(
            "[poly-tape] t={}s poly_msgs={} spot_msgs={} twap_rows={} tokens={}",
            start.elapsed().as_secs(),
            lock(&poly).msgs,
            lock(&spot).msgs,
            twap_rows.load(Ordering::Relaxed),
            tokens_rx.borrow().len(),
        );
    }
}

// ── 汇总报告与判定 ──────────────────────────────────────────────────────────

fn source_report(stat: &SourceStat, duration_ms: u64) -> Value {
    let rate = if duration_ms > 0 {
        stat.unplanned_downtime_ms as f64 / duration_ms as f64 * 100.0
    } else {
        0.0
    };
    json!({
        "msgs": stat.msgs,
        "bytes": stat.bytes,
        "first_msg_recv_ts": stat.first_msg_recv_ts,
        "last_msg_recv_ts": stat.last_msg_recv_ts,
        "latency": {
            "n": stat.latencies.len(),
            "p50_ms": percentile(stat.latencies.clone(), 50.0),
            "p99_ms": percentile(stat.latencies.clone(), 99.0),
            "max_ms": stat.latencies.iter().max().copied(),
            "口径": "server_ts − recv_ts，毫秒，含场地时钟偏差（工单口径）",
        },
        "unplanned_disconnects": stat.unplanned_disconnects,
        "planned_disconnects": stat.planned_disconnects,
        "connect_failures": stat.connect_failures,
        "unplanned_downtime_ms": stat.unplanned_downtime_ms,
        "planned_downtime_ms": stat.planned_downtime_ms,
        "stalls": stat.stalls,
        "disconnect_rate_pct": (rate * 1000.0).round() / 1000.0,
        "口径_断线率": "unplanned 断线时长/会话时长×100（轮滚动 planned 重连不计入）",
    })
}

#[allow(clippy::too_many_arguments)]
fn build_report(
    started_at_ms: i64,
    ended_at_ms: i64,
    duration_ms: u64,
    tape_path: &std::path::Path,
    assets: &[String],
    rounds: &[i64],
    poly: std::sync::MutexGuard<'_, SourceStat>,
    spot: std::sync::MutexGuard<'_, SourceStat>,
    no_spot: bool,
    twap_rows: u64,
    tokens_final: &[String],
) -> Value {
    let poly_rep = source_report(&poly, duration_ms);
    let poly_p99 = poly_rep["latency"]["p99_ms"].as_i64();
    let poly_rate = poly_rep["disconnect_rate_pct"].as_f64().unwrap_or(0.0);
    let mut failures = verdict_for_source("poly_market", poly_rate, poly_p99);
    if !no_spot {
        let spot_rep = source_report(&spot, duration_ms);
        let spot_p99 = spot_rep["latency"]["p99_ms"].as_i64();
        let spot_rate = spot_rep["disconnect_rate_pct"].as_f64().unwrap_or(0.0);
        failures.extend(verdict_for_source("spot", spot_rate, spot_p99));
    }
    json!({
        "tool": "poly-tape-recorder",
        "issue": 860,
        "session": {
            "started_at_ms": started_at_ms,
            "ended_at_ms": ended_at_ms,
            "duration_ms": duration_ms,
            "duration_min": (duration_ms as f64 / 60_000.0 * 100.0).round() / 100.0,
            "assets": assets,
            "rounds": rounds,
            "tape": tape_path.display().to_string(),
            "pid": std::process::id(),
        },
        "sources": {
            "poly_market": poly_rep,
            "spot": if no_spot { Value::Null } else { source_report(&spot, duration_ms) },
        },
        "twap_rows": twap_rows,
        "tokens_final": tokens_final,
        "verdict": {
            "pass": failures.is_empty(),
            "failures": failures,
            "阈值": {"disconnect_rate_pct_per_h": MAX_DISCONNECT_RATE_PCT, "p99_latency_ms": MAX_P99_LATENCY_MS},
            "退出码映射": {"PASS": 0, "FAIL": 2},
        },
    })
}

// ── 离线自测（--self-test / cargo test 共用同一组用例）──────────────────────

/// 一个自测用例 =（名称，离线断言函数）。
type SelfTestCase = (&'static str, fn() -> Result<(), String>);

fn self_test_cases() -> Vec<SelfTestCase> {
    vec![
        ("url 构造器白名单", test_url_allowlist),
        ("ws base 解析", test_parse_ws_base),
        ("行形状", test_row_shape),
        ("poly 时间戳提取", test_poly_server_ts),
        ("poly symbol 提取", test_poly_symbol),
        ("订阅帧", test_sub_frame),
        ("现货帧解析", test_spot_frame),
        ("TWAP 计算", test_twap),
        ("TWAP 裁剪", test_twap_trim),
        ("分位数", test_percentile),
        ("失败判定", test_verdict),
        ("参数解析", test_args),
        ("slug 互操作", test_slug),
    ]
}

fn run_self_test() -> i32 {
    let mut failed = 0;
    for (name, f) in self_test_cases() {
        match f() {
            Ok(()) => println!("ok  - {name}"),
            Err(e) => {
                println!("FAIL- {name}: {e}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        println!("self-test: 全部通过");
        0
    } else {
        println!("self-test: {failed} 项失败");
        1
    }
}

fn test_url_allowlist() -> Result<(), String> {
    let good = build_url("wss", POLY_WS_HOST, POLY_WS_MARKET_PATH)?;
    if good != "wss://ws-subscriptions-clob.polymarket.com/ws/market" {
        return Err(good);
    }
    for bad in [
        build_url("http", POLY_WS_HOST, "/x"),
        build_url("wss", "127.0.0.1", "/x"),
        build_url("wss", "gamma-api.polymarket.com.evil.io", "/x"),
        build_url("wss", "localhost", "/x"),
        build_url("wss", POLY_WS_HOST, "no-slash"),
    ] {
        if bad.is_ok() {
            return Err(format!("应当拒绝: {bad:?}"));
        }
    }
    Ok(())
}

fn test_parse_ws_base() -> Result<(), String> {
    let (s, h) = parse_ws_base("wss://ws-subscriptions-clob.polymarket.com")?;
    if s != "wss" || h != POLY_WS_HOST {
        return Err(format!("{s}/{h}"));
    }
    for bad in [
        "https://ws-subscriptions-clob.polymarket.com",
        "wss://127.0.0.1",
        "wss://ws-subscriptions-clob.polymarket.com/ws/market",
        "ws-subscriptions-clob.polymarket.com",
    ] {
        if parse_ws_base(bad).is_ok() {
            return Err(format!("应当拒绝: {bad}"));
        }
    }
    Ok(())
}

fn test_row_shape() -> Result<(), String> {
    let row = build_row(
        "spot",
        Some("BTCUSDT"),
        111,
        Some(100),
        7,
        json!({"p": "1"}),
    );
    for k in [
        "channel",
        "symbol",
        "recv_ts",
        "server_ts",
        "seq",
        "payload",
    ] {
        if row.get(k).is_none() {
            return Err(format!("缺键 {k}"));
        }
    }
    if row["seq"].as_u64() != Some(7) || row["server_ts"].as_i64() != Some(100) {
        return Err(format!("值错: {row}"));
    }
    Ok(())
}

fn test_poly_server_ts() -> Result<(), String> {
    let obj = json!({"event_type": "book", "timestamp": "1700000000000"});
    if poly_server_ts(&obj) != Some(1_700_000_000_000) {
        return Err("字符串 ms".into());
    }
    let arr = json!([{"x": 1}, {"event_type": "price_change", "timestamp": 1_700_000_000_001u64}]);
    if poly_server_ts(&arr) != Some(1_700_000_000_001) {
        return Err("数组帧取首元素".into());
    }
    let secs = json!({"timestamp": "1700000000"});
    if poly_server_ts(&secs) != Some(1_700_000_000_000) {
        return Err("秒级归一".into());
    }
    if poly_server_ts(&json!({"a": 1})).is_some() {
        return Err("缺失应 None".into());
    }
    Ok(())
}

fn test_poly_symbol() -> Result<(), String> {
    if poly_symbol(&json!({"event_type": "book"})) != Some("book") {
        return Err("对象帧".into());
    }
    if poly_symbol(&json!([{"event_type": "book"}])).is_some() {
        return Err("数组帧应 None".into());
    }
    Ok(())
}

fn test_sub_frame() -> Result<(), String> {
    let f_str = market_sub_frame(&["123".into(), "456".into()]);
    let f: Value = serde_json::from_str(&f_str).map_err(|e| e.to_string())?;
    // 实测：场地解析器键序敏感 + 多余字段静默拒订阅——assets_ids 必须是
    // 首个键，且帧里不许有 operation/initial_dump/markets。
    if !f_str.starts_with("{\"assets_ids\":[") || !f_str.ends_with("\"type\":\"market\"}") {
        return Err(format!("订阅帧键序/形态错: {f_str}"));
    }
    for bad in ["operation", "initial_dump", "markets"] {
        if f.get(bad).is_some() {
            return Err(format!("订阅帧不应含 {bad}: {f}"));
        }
    }
    let ids: Vec<String> =
        serde_json::from_value(f["assets_ids"].clone()).map_err(|e| e.to_string())?;
    if ids != vec!["123".to_string(), "456".to_string()] {
        return Err(format!("assets_ids: {ids:?}"));
    }
    Ok(())
}

fn test_spot_frame() -> Result<(), String> {
    let combined = r#"{"stream":"btcusdt@trade","data":{"e":"trade","s":"BTCUSDT","T":1700000000123,"p":"60000.5","q":"0.01"}}"#;
    let (s, ts, p, _) = parse_spot_frame(combined).ok_or("组合流应解析")?;
    if s != "BTCUSDT" || ts != 1_700_000_000_123 || p != Some(60_000.5) {
        return Err(format!("{s} {ts} {p:?}"));
    }
    let bare = r#"{"e":"trade","s":"ETHUSDT","T":1700000000124,"p":"3000","q":"1"}"#;
    let (s, _, p, _) = parse_spot_frame(bare).ok_or("裸帧应解析")?;
    if s != "ETHUSDT" || p != Some(3000.0) {
        return Err(format!("{s} {p:?}"));
    }
    if parse_spot_frame(r#"{"e":"24hrTicker","s":"BTCUSDT"}"#).is_some() {
        return Err("非 trade 应丢弃".into());
    }
    if parse_spot_frame("not json").is_some() {
        return Err("坏 json 应丢弃".into());
    }
    if symbol_to_asset("BTCUSDT") != "BTC" {
        return Err("symbol→asset".into());
    }
    Ok(())
}

fn test_twap() -> Result<(), String> {
    // 阶梯：(0,10) 起 30s，(30,20) 起 30s → 窗 [0,60] TWAP=15，满窗。
    let mut tr = TwapTracker::default();
    tr.push(60_000, 0, 10.0);
    tr.push(60_000, 30_000, 20.0);
    let (v, covered, n, full) = tr.twap(60_000, 60_000).ok_or("应有值")?;
    if (v - 15.0).abs() > 1e-9 || covered != 60_000 || n != 2 || !full {
        return Err(format!("{v} {covered} {n} {full}"));
    }
    // 部分覆盖：只有 (30,20) → covered=30s，TWAP=20，full=false。
    let mut tr = TwapTracker::default();
    tr.push(60_000, 30_000, 20.0);
    let (v, covered, _, full) = tr.twap(60_000, 60_000).ok_or("应有值")?;
    if (v - 20.0).abs() > 1e-9 || covered != 30_000 || full {
        return Err(format!("{v} {covered} {full}"));
    }
    // 单价满窗。
    let mut tr = TwapTracker::default();
    tr.push(60_000, -100, 42.0);
    let (v, covered, _, full) = tr.twap(60_000, 60_000).ok_or("应有值")?;
    if (v - 42.0).abs() > 1e-9 || covered != 60_000 || !full {
        return Err(format!("{v} {covered} {full}"));
    }
    // 空窗。
    if TwapTracker::default().twap(60_000, 60_000).is_some() {
        return Err("空应 None".into());
    }
    Ok(())
}

fn test_twap_trim() -> Result<(), String> {
    let mut tr = TwapTracker::default();
    // 远超 15m 的老样本应被裁掉，但跨窗界的那段要留住。
    tr.push(900_000, 0, 1.0);
    tr.push(900_000, 100_000, 2.0);
    tr.push(900_000, 900_000, 3.0);
    if tr.samples.len() > 3 {
        return Err("不应膨胀".into());
    }
    // now=900_000, 窗 900_000：段 (100_000,2) 跨不住 start=0？start=0，
    // (0,1) 跨 [0,100_000)，(100_000,2) 跨到 900_000。TWAP=(1*100k+2*800k)/900k。
    let (v, covered, _, full) = tr.twap(900_000, 900_000).ok_or("应有值")?;
    let expect = (1.0 * 100_000.0 + 2.0 * 800_000.0) / 900_000.0;
    if (v - expect).abs() > 1e-9 || covered != 900_000 || !full {
        return Err(format!("{v} vs {expect} {covered} {full}"));
    }
    // 继续灌入推进 now，裁剪生效且窗口计算仍对。
    for t in [1_200_000i64, 1_500_000] {
        tr.push(t, t, 4.0);
    }
    let (v, covered, _, _) = tr.twap(1_500_000, 60_000).ok_or("应有值")?;
    if (v - 4.0).abs() > 1e-9 || covered != 60_000 {
        return Err(format!("{v} {covered}"));
    }
    if tr.samples.len() > 4 {
        return Err(format!("裁剪失效: {}", tr.samples.len()));
    }
    Ok(())
}

fn test_percentile() -> Result<(), String> {
    if percentile(vec![], 50.0).is_some() {
        return Err("空应 None".into());
    }
    let v = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    if percentile(v.clone(), 50.0) != Some(6) {
        return Err(format!("p50: {:?}", percentile(v.clone(), 50.0)));
    }
    if percentile(v.clone(), 99.0) != Some(10) {
        return Err(format!("p99: {:?}", percentile(v.clone(), 99.0)));
    }
    if percentile(v, 0.0) != Some(1) {
        return Err("p0".into());
    }
    Ok(())
}

fn test_verdict() -> Result<(), String> {
    if !verdict_for_source("x", 0.9, Some(999)).is_empty() {
        return Err("阈值内应 PASS".into());
    }
    let f = verdict_for_source("x", 1.001, Some(1_001));
    if f.len() != 2 {
        return Err(format!("双超限应两条: {f:?}"));
    }
    // 恰好压线（=阈值）不算超限：断线率与 p99 都是严格大于才 FAIL。
    if !verdict_for_source("x", 1.0, Some(1_000)).is_empty() {
        return Err("压线应 PASS".into());
    }
    if verdict_for_source("x", 0.0, Some(1_001)).len() != 1 {
        return Err("仅 p99 超限应一条".into());
    }
    Ok(())
}

fn test_args() -> Result<(), String> {
    let a = parse_args(&[
        "--assets".into(),
        "btc,eth".into(),
        "--rounds".into(),
        "300".into(),
        "--duration".into(),
        "60".into(),
        "--out-dir".into(),
        "/x/y".into(),
        "--no-spot".into(),
    ])?;
    if a.assets != vec!["BTC", "ETH"]
        || a.rounds != vec![300]
        || !a.no_spot
        || a.duration_secs != 60
    {
        return Err(format!("{a:?}"));
    }
    if parse_args(&["--bogus".into()]).is_ok() {
        return Err("未知参数应报错".into());
    }
    if parse_args(&["--assets".into()]).is_ok() {
        return Err("缺值应报错".into());
    }
    Ok(())
}

fn test_slug() -> Result<(), String> {
    // 与 discovery.rs 同一套 slug 纪律（纯本地，无网络）。
    let slug =
        polymarket_extension::gamma::slug_for("BTC", 300, 1_777_000_000).ok_or("5m 应可构造")?;
    if slug != format!("btc-updown-5m-{}", 1_777_000_000 - 1_777_000_000 % 300) {
        return Err(slug);
    }
    if polymarket_extension::gamma::duration_label(300) != Some("5m") {
        return Err("duration_label".into());
    }
    Ok(())
}

// ── cargo test 入口（与 --self-test 共用用例）───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_test_cases_all_pass() {
        for (name, f) in self_test_cases() {
            assert!(f().is_ok(), "self-test 用例失败: {name}");
        }
    }
}
