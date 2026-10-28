#![cfg(test)]
//! ── issue 691: ghost-fill 告警 ──────────────────────────────────────────────
//!
//! `apply_fill_report` 的 resolve 不中分支曾把无法归属到任何订单的成交回报
//! 静默塞进 `pending_fills` 内存队列（上限 500、FIFO 淘汰）——不落盘、不告
//! 警。内核离线/断流期间场地发生的成交，重启后回报到达时全部走这条静默路径。
//!
//! 现在首次见到一个 trade key 会置 `FillOutcome.ghost_fill`，service 层照
//! `emit_late_fill` 的既有姿势落一行 `ghost_fill` 审计 + 恰一次 `RiskAlert`；
//! 同 id 重报不重复告警；后续解析成功后集合清除、停止告警；正常成交零误报。

use super::*;
use rust_decimal_macros::dec;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-ghost-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Live core（user-WS 成交回报的生产通道），审计与订单日志都落 `dir`。
fn ghost_core(dir: &std::path::Path) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        order_log_path: path("orders.jsonl"),
        trade_log_path: path("trades.jsonl"),
        audit_log_path: path("audit.jsonl"),
        ..Default::default()
    });
    c.set_balance(dec!(100));
    c
}

fn venue_fill(order_id: &str, trade: &str, size: Decimal, price: Decimal) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side: Side::Buy,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 10,
        tx_hash: None,
        maker: None,
        account_id: crate::model::default_account_id(),
    }
}

fn order(mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn audit_ghost_rows(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "ghost_fill")
        .collect()
}

/// 验收 1：无法归属的成交回报 → 审计落 `ghost_fill` 行 + `RiskAlert` 恰一次。
#[test]
fn unattributable_fill_lands_one_audit_row_and_exactly_one_risk_alert() {
    let dir = scratch("alert-once");
    let mut c = ghost_core(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 场地回报一个我们从未下过的订单的成交：进 pending 缓冲，且必须响亮。
    c.ingest_fill(venue_fill("0xunknown", "t-ghost-1", dec!(3), dec!(0.40)), 1)
        .unwrap();
    assert_eq!(c.ome().pending_unknown_count(), 1, "buffered for retry");

    let alerts: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
        Ok(Event::RiskAlert { message, .. }) => Some(message),
        Ok(_) => None,
        Err(_) => None,
    })
    .take(8)
    .collect();
    let ghost_alerts: Vec<_> = alerts.iter().filter(|m| m.contains("ghost_fill")).collect();
    assert_eq!(
        ghost_alerts.len(),
        1,
        "exactly ONE risk alert for the first ghost report: {alerts:?}"
    );
    assert!(
        ghost_alerts[0].contains("t-ghost-1"),
        "the alert names the trade id: {}",
        ghost_alerts[0]
    );

    let rows = audit_ghost_rows(&dir);
    assert_eq!(rows.len(), 1, "one ghost_fill audit row: {rows:?}");
    assert_eq!(rows[0]["tradeId"], "t-ghost-1");
    assert_eq!(rows[0]["tokenId"], "tok");
    assert_eq!(rows[0]["size"], "3");
    assert_eq!(rows[0]["price"], "0.40");
    let _ = std::fs::remove_dir_all(dir);
}

/// 验收 2：同一 trade id 二次回报不重复告警（缓冲照旧，去重集合拦截）。
#[test]
fn a_re_report_of_the_same_trade_id_does_not_alert_again() {
    let dir = scratch("re-report");
    let mut c = ghost_core(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    c.ingest_fill(venue_fill("0xunknown", "t-ghost-2", dec!(3), dec!(0.40)), 1)
        .unwrap();
    // 场地重报同一笔（WS 重放 / REST 扫描的日常）：绝不二次响铃。
    c.ingest_fill(venue_fill("0xunknown", "t-ghost-2", dec!(3), dec!(0.40)), 2)
        .unwrap();
    c.ingest_fill(venue_fill("0xunknown", "t-ghost-2", dec!(3), dec!(0.40)), 3)
        .unwrap();

    let alerts: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
        Ok(Event::RiskAlert { message, .. }) => Some(message),
        Ok(_) => None,
        Err(_) => None,
    })
    .take(8)
    .collect();
    assert_eq!(
        alerts.iter().filter(|m| m.contains("ghost_fill")).count(),
        1,
        "three reports of one trade id alert ONCE: {alerts:?}"
    );
    // 缓冲也只留一份（resolve 仍不中，同 key 各自入队——但审计只第一次）。
    let rows = audit_ghost_rows(&dir);
    assert_eq!(
        rows.len(),
        1,
        "one audit row despite three reports: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// 验收 3：后续解析成功（新到订单接住回报）后集合清除、停止告警。
#[test]
fn a_fill_that_later_resolves_stops_alerting() {
    let dir = scratch("late-resolve");
    let mut c = ghost_core(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    c.ingest_fill(venue_fill("0xunknown", "t-ghost-3", dec!(3), dec!(0.40)), 1)
        .unwrap();
    let first: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
        Ok(Event::RiskAlert { message, .. }) => Some(message),
        Ok(_) => None,
        Err(_) => None,
    })
    .collect();
    assert_eq!(
        first.iter().filter(|m| m.contains("ghost_fill")).count(),
        1,
        "first sight alerts: {first:?}"
    );

    // 新订单注册（token+side 兜底解析接住缓冲里的回报）。名义 0.40×10=4
    // 超出测试 core 的 per-order cap 3，故挂 0.30。
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.30), dec!(10), "k1"), 0, 2)
        .unwrap();
    c.tick(3).unwrap(); // drain_pending 把缓冲的 ghost 成交落到订单上

    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.filled_size, dec!(3), "the buffered ghost fill applied");
    // 解析成功已把该 key 清出告警集合：若 venue 再重报同一 trade，它已经
    // 归属（幂等表吸收），绝无新告警。
    c.ingest_fill(venue_fill(&id, "t-ghost-3", dec!(3), dec!(0.40)), 4)
        .unwrap();
    let later: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
        Ok(Event::RiskAlert { message, .. }) => Some(message),
        Ok(_) => None,
        Err(_) => None,
    })
    .collect();
    assert!(
        later.iter().all(|m| !m.contains("ghost_fill")),
        "no ghost alert after the fill resolved: {later:?}"
    );
    assert_eq!(audit_ghost_rows(&dir).len(), 1, "audit stays at one row");
    let _ = std::fs::remove_dir_all(dir);
}

/// 验收 4：正常成交（订单在册、正常归属）不触发 ghost 告警、不落审计行。
#[test]
fn normal_fills_stay_silent() {
    let dir = scratch("normal-silent");
    let mut c = ghost_core(&dir);
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    c.bind_venue(&id, "0xv1".into(), 2).unwrap();
    c.ingest_fill(venue_fill(&id, "t-ok-1", dec!(2), dec!(0.40)), 3)
        .unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::PartiallyFilled);
    assert_eq!(o.filled_size, dec!(2));

    let alerts: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
        Ok(Event::RiskAlert { message, .. }) => Some(message),
        Ok(_) => None,
        Err(_) => None,
    })
    .collect();
    assert!(
        alerts.iter().all(|m| !m.contains("ghost_fill")),
        "a normal fill must never ghost-alert: {alerts:?}"
    );
    assert!(
        audit_ghost_rows(&dir).is_empty(),
        "a normal fill must never write a ghost_fill audit row"
    );
    let _ = std::fs::remove_dir_all(dir);
}
