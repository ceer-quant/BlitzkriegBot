//! 帧解析纯函数（poly/spot 场地帧 → (symbol, ts, price)，原样迁出）。

use serde_json::{Value, json};

/// Polymarket 帧的场地时间戳：对象取 `timestamp`，数组取首个带该字段的元素；
/// 字符串/数字都收，秒级（<1e12）归一到毫秒。缺 → None（不进延迟统计）。
pub(crate) fn poly_server_ts(v: &Value) -> Option<i64> {
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
pub(crate) fn poly_symbol(v: &Value) -> Option<&str> {
    v.get("event_type").and_then(|e| e.as_str())
}

/// 市场频道订阅帧：**最小形态** `{"assets_ids":[...],"type":"market"}`。
/// 实测（2026-11-03 裸探针）：SDK 形态多出的 `operation`/`markets`/
/// `initial_dump` 字段会被场地静默忽略订阅（连接保活但零数据帧），官方
/// 文档的最小帧立即开始推 book/price_change/last_trade_price。
pub(crate) fn market_sub_frame(asset_ids: &[String]) -> String {
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
pub(crate) fn parse_spot_frame(text: &str) -> Option<(String, i64, Option<f64>, Value)> {
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
pub(crate) fn symbol_to_asset(symbol: &str) -> String {
    symbol.trim_end_matches("USDT").to_ascii_uppercase()
}
