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
//! `build_url` 先校验后拨号；公开市场数据，零凭据。
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

mod frames;
mod hosts;
mod report;
mod selftest;
mod tape;
mod twap;
mod verdict;

use blitzkrieg_market_api::net::now_ms;
use futures_util::{SinkExt, StreamExt};
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use polymarket_client_sdk_v2::gamma::types::request::MarketsRequest;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use crate::frames::{
    market_sub_frame, parse_spot_frame, poly_server_ts, poly_symbol, symbol_to_asset,
};
use crate::hosts::{
    BINANCE_WS_HOST, GAMMA_HOST, POLY_WS_HOST, POLY_WS_MARKET_PATH, build_url, parse_ws_base,
};
use crate::report::build_report;
use crate::selftest::run_self_test;
use crate::tape::{SourceStat, StatHandle, Tape, lock};
use crate::twap::Trackers;
use crate::verdict::BreakReason;

// ── 常量：端点、看门狗、阈值 ────────────────────────────────────────────────

/// 连接与静默看门狗：无帧超过该时长视为 unplanned 断线（场地半开连接在
/// TCP 层不可见，只能靠数据静默兜底）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLY_STALL: Duration = Duration::from_secs(30);
const SPOT_STALL: Duration = Duration::from_secs(60);

/// CLOB WS 应用层心跳间隔（客户端 "PING" 文本帧；SDK 默认同拍 5s）。
const POLY_PING_INTERVAL: Duration = Duration::from_secs(5);

/// 轮滚动（planned）重连后的握手间隔，与故障退避区分开。
const PLANNED_RECONNECT_DELAY: Duration = Duration::from_millis(250);
/// Gamma 轮发现节奏，与 discovery.rs 的 5s 同拍。
const DISCOVERY_POLL: Duration = Duration::from_secs(5);

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
