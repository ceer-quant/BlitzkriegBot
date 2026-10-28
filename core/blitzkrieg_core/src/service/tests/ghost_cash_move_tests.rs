#![cfg(test)]

//! 幽灵成交探测器（issue 692，2026-10-28 11:31 事故）：场地现金异动而内核
//! 零认知时必须响亮告警。事故链：场地成交与现金变动（13.596906 →
//! 7.898029），内核全程零认知；事后证明记账本身是对的（reconcile 发现差额
//! → journal `ref=venue` realign 行如实记账），但整条链路静默 —— realign
//! 通道只写 journal 行，异动只能靠人工对账事后发现。
//!
//! 验收（issue 692 三条）：
//! 1. 场地现金变动且零成交/订单记录 → `RiskAlert` 恰一次；
//! 2. 同一异常窗口下一轮 reconcile 不重复告警（去重键 = 异常窗口）；
//! 3. 正常 realign（有对应成交记录的差额修正）零误报。

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

/// 单账户 Live fixture（与 tests_account_reset.rs 的 legacy 形态同一模具）：
/// realign 通道的 Live 分支才记账 + 探测，Dry 下直接落账不告警。
fn live_core_fixture(tag: &str) -> Core {
    let dir = reset_audit_dir(tag);
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            manual_ops_audit_path: Some(
                dir.join("manual-ops.jsonl").to_string_lossy().into_owned(),
            ),
            trade_log_path: log("trades.jsonl"),
            order_log_path: log("orders.jsonl"),
            position_log_path: log("positions.jsonl"),
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        crate::account::AccountLedgers::single_default(0),
    )
}

/// 把事件槽抽干，只留 RiskAlert 消息。
fn drain_alerts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    alerts
}

fn ghost_alerts(alerts: &[String]) -> Vec<&String> {
    alerts
        .iter()
        .filter(|m| m.contains("unattributed_cash_move"))
        .collect()
}

/// 验收 1：场地现金变动且内核零成交/订单记录 → `RiskAlert` 恰一次，消息
/// 可 grep（`unattributed_cash_move` 前缀 + venue + 金额），同一事实落审计
/// JSONL —— 与 `emit_late_fill` 同一三面孔（事件 / 审计日志 / tracing）。
#[test]
fn venue_cash_move_with_zero_kernel_records_alerts_exactly_once() {
    let mut c = live_core_fixture("ghost-692-alert");
    // 事故现场数字：13.596906 → 7.898029。
    c.set_balance(Decimal::new(13596906, 6));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 场地报它的真值（内核对这笔钱零成交、零订单记录）。
    c.realign_venue_cash("polymarket", Decimal::new(7898029, 6));

    let alerts = drain_alerts(&mut rx);
    let ghost = ghost_alerts(&alerts);
    assert_eq!(
        ghost.len(),
        1,
        "an unattributed venue cash move must alert exactly once: {alerts:?}"
    );
    let m = ghost[0];
    assert!(
        m.contains("unattributed_cash_move"),
        "greppable prefix: {m}"
    );
    assert!(m.contains("polymarket"), "names the venue: {m}");
    assert!(m.contains("13.596906"), "names the previous book: {m}");
    assert!(m.contains("7.898029"), "names the venue's report: {m}");
    assert!(m.contains("fills=0"), "states the zero-fill evidence: {m}");
    assert!(
        m.contains("orders=0"),
        "states the zero-order evidence: {m}"
    );

    // The audit log carries the same fact (structured, with the amounts).
    let path = c.audit_log_path().unwrap().to_string();
    let rows: Vec<serde_json::Value> = crate::jsonl::load(std::path::Path::new(&path), "audit");
    let events: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "unattributed_cash_move")
        .collect();
    assert_eq!(events.len(), 1, "the audit log holds exactly one record");
    assert_eq!(events[0]["venue"], "polymarket");
    assert_eq!(events[0]["previous"], "13.596906");
    assert_eq!(events[0]["reported"], "7.898029");
}

/// 验收 2：同一异常窗口（账户仍零成交/零订单记录）下的下一轮 reconcile /
/// fold 不重复告警 —— 去重键是异常窗口本身，不是单次记账。窗口关闭
/// （内核获得自己的成交证据）后，账本带交易史，此后的修正按验收 3 的
/// 「正常 realign」静默；全新会话自然重新武装（槽位随 Core 重建）。
#[test]
fn the_same_anomaly_window_does_not_re_alert_but_rearms_after_evidence() {
    let mut c = live_core_fixture("ghost-692-dedup");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 首次异动：响亮告警一次。
    c.realign_venue_cash("polymarket", Decimal::from(70));
    assert_eq!(ghost_alerts(&drain_alerts(&mut rx)).len(), 1);

    // 场地继续报同一窗口内的数（未修正的差额随 reconcile 周期反复出现）：
    // 一轮轮 fold 不刷屏 —— 静默正是本探测器与告警风暴的分界线。
    c.realign_venue_cash("polymarket", Decimal::from(60));
    c.realign_venue_cash("polymarket", Decimal::from(50));
    c.fold_venue_free("polymarket", Decimal::from(50));
    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "the same anomaly window must not re-alert: {alerts:?}"
    );

    // 窗口关闭：内核经生产成交路径获得自己的订单 + 成交记录（OME 幂等表
    // 折进一笔 fill，订单行 filled_size > 0）。挂单 → live → 确认成交。
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(5),
        internal_key: "k1".into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    };
    let (id, _) = c.place(req, 0, 1).unwrap();
    c.bind_venue(&id, "0xv1".into(), 2).unwrap();
    c.ingest_fill(
        Fill {
            order_id: id,
            trade_id: Some("t1".into()),
            token_id: "tok".into(),
            side: Side::Buy,
            price: dec!(0.40),
            size: dec!(5),
            status: FillStatus::Confirmed,
            ts_ms: 3,
            tx_hash: None,
            maker: None,
            account_id: crate::model::default_account_id(),
        },
        3,
    )
    .unwrap();
    drain_alerts(&mut rx);

    // 有交易史后的差额修正：走「正常 realign 静默」分支（验收 3 的语义），
    // 既不重复旧窗口的告警，也不误报。全新会话（新 Core）自带空告警槽 ——
    // 重启后的第一笔零记录异动自然重新武装（验收 1 的形态）。
    c.realign_venue_cash("polymarket", Decimal::from(1));
    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "with trade records on the book the realign is a normal correction: {alerts:?}"
    );
}

/// 验收 3：正常 realign（有对应成交记录的差额修正）零误报 —— fee 级零头、
/// 结算时点差、人工平仓残差这类有交易史账本的校正必须保持静默（改前语义）。
#[test]
fn a_normal_realign_with_trade_records_never_alerts() {
    let mut c = live_core_fixture("ghost-692-normal");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 有对应成交记录：一笔真实成交（订单行 + OME 幂等表各折一次）。
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(5),
        internal_key: "k1".into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    };
    let (id, _) = c.place(req, 0, 1).unwrap();
    c.bind_venue(&id, "0xv1".into(), 2).unwrap();
    c.ingest_fill(
        Fill {
            order_id: id,
            trade_id: Some("t1".into()),
            token_id: "tok".into(),
            side: Side::Buy,
            price: dec!(0.40),
            size: dec!(5),
            status: FillStatus::Confirmed,
            ts_ms: 3,
            tx_hash: None,
            maker: None,
            account_id: crate::model::default_account_id(),
        },
        3,
    )
    .unwrap();
    drain_alerts(&mut rx);

    // 此后场地报来的差额修正（如 fee 零头 / 手续费回填）全是正常 realign。
    c.realign_venue_cash("polymarket", Decimal::from(97));
    c.realign_venue_cash("polymarket", Decimal::from(98));
    c.fold_venue_free("polymarket", Decimal::from(98));

    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "a realign over a book with trade records must never alert (zero false positives): {alerts:?}"
    );

    // 反向半边：成交证据出现前若曾误警过，窗口关闭时槽位必须被摘除 ——
    // 这里用「先无记录告警 → 获得成交 → 再异动不再算同窗口」已被验收 2
    // 覆盖；本测试钉死的是「有记录 → 永远静默」这半边。
}
