//! 离线自测（--self-test / cargo test 共用同一组用例，原样迁出）。

use serde_json::{Value, json};

use crate::frames::{
    market_sub_frame, parse_spot_frame, poly_server_ts, poly_symbol, symbol_to_asset,
};
use crate::hosts::{POLY_WS_HOST, POLY_WS_MARKET_PATH, build_url, parse_ws_base};
use crate::parse_args;
use crate::tape::build_row;
use crate::twap::TwapTracker;
use crate::verdict::{percentile, verdict_for_source};

/// 一个自测用例 =（名称，离线断言函数）。
pub(crate) type SelfTestCase = (&'static str, fn() -> Result<(), String>);

pub(crate) fn self_test_cases() -> Vec<SelfTestCase> {
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

pub(crate) fn run_self_test() -> i32 {
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
