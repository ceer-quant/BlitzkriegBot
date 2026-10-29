#![cfg(test)]

//! 幽灵成交探测器（issue 692，2026-10-28 11:31 事故；#702 决策修订）：场地
//! 现金异动而内核解释不了时必须响亮告警。事故链：场地成交与现金变动
//! （13.596906 → 7.898029），内核全程零认知；事后证明记账本身是对的
//! （reconcile 发现差额 → journal `ref=venue` realign 行如实记账），但整条
//! 链路静默 —— realign 通道只写 journal 行，异动只能靠人工对账事后发现。
//!
//! #702 判据（窗口化证据，替代 #700 的终身订单计数）：本次差额能否被
//! 「上次场地真值以来台账记下的现金事实」解释。终身计数被证明过宽 ——
//! 一张零成交订单或很早以前的成交会永久豁免之后的所有未知变动。
//!
//! 验收（issue 702 五条）：
//! 1. 零记录账户的未知现金减少 → 告警；
//! 2. 历史成交已对账完成后，新的无归属现金变化 → 告警；
//! 3. 同窗口真实成交能解释的修正 → 不误报；
//! 4. 同一未解释残差重复报告只告警一次，变化后重新告警；
//! 5. 拒单/零成交取消单（从不进台账）不构成免警凭据（验收 1 的账本即此形态）。

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

/// 一笔真实成交的生产路径（挂单 → live → 确认成交）：订单行 + 台账 BuyFill
/// 行各折一次。
fn place_and_fill(c: &mut Core, price: Decimal, size: Decimal, ts: i64) {
    c.book_snapshot("tok", vec![], vec![(price, dec!(100))], 1);
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price,
        size,
        internal_key: "k1".into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    };
    let (id, _) = c.place(req, ts, ts + 1).unwrap();
    c.bind_venue(&id, "0xv1".into(), ts + 2).unwrap();
    c.ingest_fill(
        Fill {
            order_id: id,
            trade_id: Some("t1".into()),
            token_id: "tok".into(),
            side: Side::Buy,
            price,
            size,
            status: FillStatus::Confirmed,
            ts_ms: ts + 3,
            tx_hash: None,
            maker: None,
            account_id: crate::model::default_account_id(),
        },
        ts + 3,
    )
    .unwrap();
}

/// 验收 1：场地现金变动且内核窗口内零账务事实 → `RiskAlert` 恰一次，消息
/// 可 grep（`unattributed_cash_move` 前缀 + venue + 金额 + 残差），同一事实
/// 落审计 JSONL —— 与 `emit_late_fill` 同一三面孔（事件 / 审计日志 /
/// tracing）。账本从未下过单（拒单/零成交取消单也不进台账），没有任何
/// 免警凭据 —— issue 702「仅有拒单 → 未知现金减少：告警」的形态。
#[test]
fn venue_cash_move_with_zero_kernel_records_alerts_exactly_once() {
    let mut c = live_core_fixture("ghost-692-alert");
    // 事故现场数字：13.596906 → 7.898029。
    c.set_balance(Decimal::new(13596906, 6));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 场地报它的真值（内核对这笔钱零账务事实）。首报是 boot adoption：
    // 报数本身成为账本真值，静默 —— 窗口从此刻起算。
    c.realign_venue_cash("polymarket", Decimal::new(13596906, 6));
    assert!(
        ghost_alerts(&drain_alerts(&mut rx)).is_empty(),
        "the boot adoption seeds the book and never alerts"
    );

    // 第二轮报数：venue 自己动了 5.698877，内核窗口内零事实 → 响亮告警。
    c.realign_venue_cash("polymarket", Decimal::new(7898029, 6));

    let alerts = drain_alerts(&mut rx);
    let ghost = ghost_alerts(&alerts);
    assert_eq!(
        ghost.len(),
        1,
        "an unexplained venue cash move must alert exactly once: {alerts:?}"
    );
    let m = ghost[0];
    assert!(
        m.contains("unattributed_cash_move"),
        "greppable prefix: {m}"
    );
    assert!(m.contains("polymarket"), "names the venue: {m}");
    assert!(m.contains("13.596906"), "names the previous book: {m}");
    assert!(m.contains("7.898029"), "names the venue's report: {m}");
    assert!(
        m.contains("residual=-5.698877"),
        "states the unexplained residual (venue SHORT): {m}"
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

/// 验收 3：窗口内台账事实能解释的修正零误报 —— 成交花了 2.00，场地报数
/// 相应少 2.00（Maker 零费）：残差 = −2 −(−2) = 0 → 静默。紧接的 fee 级
/// 零头抖动（±0.01 以内）同样在 ε 容忍内。#702 验收「解释不足的残差不得
/// 被全额免警」由 an_unexplained_move_after_history_was_reconciled 钉死。
#[test]
fn a_correction_explained_by_window_facts_never_alerts() {
    let mut c = live_core_fixture("ghost-702-normal");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 真值进场：场地报 100，窗口起点就此定格。
    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx);

    // 窗口内一笔真实成交：买 0.40 × 5 = 2.00（Maker 零费，台账 −2.00）。
    place_and_fill(&mut c, dec!(0.40), dec!(5), 10);
    drain_alerts(&mut rx);

    // 场地报 98 = 100 − 2.00：每一分都被窗口内的 BuyFill 解释 → 静默。
    c.realign_venue_cash("polymarket", Decimal::from(98));

    // fee 级零头（0.01 容忍内）依旧静默。
    c.realign_venue_cash("polymarket", Decimal::new(9801, 2));

    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "a correction fully explained by this window's booked facts must never alert: {alerts:?}"
    );
}

/// 验收 2：历史合法成交已对账完成后，新的无归属现金变化 → 告警。
/// 这正是 #702 推翻的旧语义：成交在窗口 1 里发生并已被真值 98 对账收编；
/// 窗口 2 里场地又动了 3.00 而内核零事实 —— 旧终身计数模型会因「有交易史」
/// 永久静默，窗口模型正确告警。
#[test]
fn an_unexplained_move_after_history_was_reconciled_alerts() {
    let mut c = live_core_fixture("ghost-702-after-history");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 窗口 1：真值 100 → 成交 2.00 → 场地报 98（对账成立，静默）。
    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx);
    place_and_fill(&mut c, dec!(0.40), dec!(5), 10);
    drain_alerts(&mut rx);
    c.realign_venue_cash("polymarket", Decimal::from(98));
    assert!(
        ghost_alerts(&drain_alerts(&mut rx)).is_empty(),
        "window 1 closes clean: the fill explains the correction"
    );

    // 窗口 2：场地又动了 3.00（98 → 95），内核零窗口事实 → 告警。
    // 「曾经成交过」不再构成免警凭据（issue 702 的核心修订）。
    c.realign_venue_cash("polymarket", Decimal::from(95));
    let alerts = drain_alerts(&mut rx);
    let ghost = ghost_alerts(&alerts);
    assert_eq!(
        ghost.len(),
        1,
        "history that is already reconciled must not mute a NEW unexplained move: {alerts:?}"
    );
    assert!(
        ghost[0].contains("residual=-3"),
        "names this window's own residual: {}",
        ghost[0]
    );
}

/// 验收 4：同一未解释残差重复报告（reconcile 周期重放同一条腿）只告警
/// 一次；残差变化（叠加新异动）重新告警；账本恢复可解释（窗口事实把
/// 残差吃回 ε 内）后清槽，再来的新异动又告警 —— 不靠会话生命周期重新
/// 武装（旧模型用「从未交易过的整个会话」当窗口，#702 明令废弃）。
#[test]
fn the_same_residual_dedups_but_a_new_one_re_alerts() {
    let mut c = live_core_fixture("ghost-702-dedup");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // 真值 100 进场。
    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx);

    // 第一次未知异动（−30）：告警一次。
    c.realign_venue_cash("polymarket", Decimal::from(70));
    assert_eq!(ghost_alerts(&drain_alerts(&mut rx)).len(), 1);

    // 同一条腿重放（场地继续报 70）：同一残差，静默不刷屏。
    c.realign_venue_cash("polymarket", Decimal::from(70));
    c.fold_venue_free("polymarket", Decimal::from(70));
    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "the SAME unexplained residual must not re-alert: {alerts:?}"
    );

    // 残差变化（又动了 10，70 → 60）：新事实，重新告警。
    c.realign_venue_cash("polymarket", Decimal::from(60));
    assert_eq!(
        ghost_alerts(&drain_alerts(&mut rx)).len(),
        1,
        "a CHANGED residual is a new anomaly and re-alerts"
    );

    // 方向反转也是新残差（60 → 90，无中生有 +30）：告警一次；重放静默。
    c.realign_venue_cash("polymarket", Decimal::from(90));
    assert_eq!(
        ghost_alerts(&drain_alerts(&mut rx)).len(),
        1,
        "a reversed-direction anomaly is its own new residual"
    );
    c.realign_venue_cash("polymarket", Decimal::from(90));
    assert!(
        ghost_alerts(&drain_alerts(&mut rx)).is_empty(),
        "the reversed residual replays silent, same as any other"
    );

    // 恢复清槽：窗口内台账进账 +30（cashflow 台账事实），场地按事实报
    // 120 —— 残差 = (120−90) − 30 = 0 → 解释成立，槽位清空。
    c.accounts
        .active_ledger_mut()
        .credit_cashflow(Decimal::from(30), "rebate");
    c.realign_venue_cash("polymarket", Decimal::from(120));
    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "a residual the window now explains clears the alert slot: {alerts:?}"
    );

    // 恢复后的新异动：120 → 90（场地又少 30，窗口零新事实）→ 重新告警
    // —— 不靠会话生命周期重新武装（旧终身模型做不到这一点）。
    c.realign_venue_cash("polymarket", Decimal::from(90));
    assert_eq!(
        ghost_alerts(&drain_alerts(&mut rx)).len(),
        1,
        "after recovery a fresh unexplained move re-alerts"
    );
}

/// 验收 5（跨 venue 隔离，#701 承接）：A（polymarket）窗口内有成交事实，
/// B（poly）零事实但发生异动 —— A 的事实解释不了 B 的账：B 恰一次告警；
/// A 自己账本上能解释的修正照旧静默。#700 曾用全局 applied 计数让 A 的
/// 成交替 B 挡刀。
#[test]
fn a_sibling_venues_facts_do_not_explain_this_venues_residual() {
    // 双账户：poly-acct 绑 polymarket，klin-acct 绑 poly。
    let dir = reset_audit_dir("ghost-701-cross");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let acct = |id: &str, name: &str, venue: &str| crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: name.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys::default(),
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        updated_at_ms: 0,
    };
    let mut c = Core::with_account_book(
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
        crate::account::AccountLedgers::new(
            vec![
                acct("poly-acct", "polymarket 主账户", "polymarket"),
                acct("klin-acct", "poly 副账户", "poly"),
            ],
            crate::account::AccountId::from("poly-acct"),
        ),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // A 账本真值进场。
    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx); // 首报=种子，静默

    // A（polymarket / poly-acct）窗口内一笔真实成交：台账 −2.00。
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
        account_id: crate::account::AccountId::from("poly-acct"),
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
            account_id: crate::account::AccountId::from("poly-acct"),
        },
        3,
    )
    .unwrap();
    drain_alerts(&mut rx);

    // A 的账本对得上：场地报 98 = 100 − 2.00 → 静默。
    c.realign_venue_cash("polymarket", Decimal::from(98));
    assert!(
        ghost_alerts(&drain_alerts(&mut rx)).is_empty(),
        "A's own explained correction stays silent"
    );

    // B（poly / klin-acct）真值进场后异动：A 的台账事实解释不了 B 的账。
    c.realign_venue_cash("poly", Decimal::from(50));
    drain_alerts(&mut rx); // 首报=种子，静默
    c.realign_venue_cash("poly", Decimal::from(30));
    let alerts = drain_alerts(&mut rx);
    assert_eq!(
        ghost_alerts(&alerts).len(),
        1,
        "B's unexplained move must alert even though venue A has window facts: {alerts:?}"
    );
}

/// Boot adoption 门只对「从未报过数的场地」生效：账本中途被外部清零
/// （previous==0 但真值簿已有记录）是真实异动，必须照常探测。
#[test]
fn a_zero_book_mid_session_is_a_move_not_a_reseed() {
    let mut c = live_core_fixture("ghost-702-zero-mid");
    c.set_balance(Decimal::from(100));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx); // boot adoption 种子，静默

    // 场地报 0（资金全没了），内核窗口内零事实 → 告警（不是 boot adoption）。
    c.realign_venue_cash("polymarket", Decimal::from(0));
    let alerts = drain_alerts(&mut rx);
    assert_eq!(
        ghost_alerts(&alerts).len(),
        1,
        "a mid-session collapse to zero is a move the kernel must see: {alerts:?}"
    );
    assert!(
        ghost_alerts(&alerts)[0].contains("residual=-100"),
        "names the full unexplained loss: {}",
        ghost_alerts(&alerts)[0]
    );
}

/// #701 验收 1：跨 venue 证据隔离。A（polymarket）有一笔合法成交（落进
/// A 自己的窗口），B（poly）零事实但发生现金异动 —— A 的台账事实永远
/// 解释不了 B 的账本（两本账、两个游标）。B 恰一次告警；A 自己的异动
/// 被窗口内成交事实解释，走正常 realign 静默。
#[test]
fn a_sibling_venues_fills_do_not_mute_this_venues_ghost_alert() {
    // 双账户：poly-acct 绑 polymarket，klin-acct 绑 poly。
    let dir = reset_audit_dir("ghost-701-cross");
    let log = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
    let acct = |id: &str, name: &str, venue: &str| crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: name.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys::default(),
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        updated_at_ms: 0,
    };
    let mut c = Core::with_account_book(
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
        crate::account::AccountLedgers::new(
            vec![
                acct("poly-acct", "polymarket 主账户", "polymarket"),
                acct("klin-acct", "poly 副账户", "poly"),
            ],
            crate::account::AccountId::from("poly-acct"),
        ),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // A 账本先注资（live 账本默认 0，reserve 门要先有钱才放行下单）。
    c.realign_venue_cash("polymarket", Decimal::from(100));
    drain_alerts(&mut rx); // 首报=种子，静默

    // A（polymarket / poly-acct）一笔真实成交：订单行 + applied 表各折一次。
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
        account_id: crate::account::AccountId::from("poly-acct"),
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
            account_id: crate::account::AccountId::from("poly-acct"),
        },
        3,
    )
    .unwrap();
    drain_alerts(&mut rx);

    // B（poly / klin-acct）零记录但现金异动：修复前被 A 的全局计数静默。
    // 场地报 B 的账前 B 账本要先有非零初值（previous==0 是 boot adoption 静默）。
    c.realign_venue_cash("poly", Decimal::from(50));
    drain_alerts(&mut rx); // 首报=种子，静默
    c.realign_venue_cash("poly", Decimal::from(30));
    let alerts = drain_alerts(&mut rx);
    assert_eq!(
        ghost_alerts(&alerts).len(),
        1,
        "B's unattributed move must alert even though venue A has fills: {alerts:?}"
    );

    // A 自己账本上的异动（窗口内成交事实解释得了）：正常 realign 静默 ——
    // 归属不串门。场地报 98 = 100 − 2.00 成交，residual = (−2) − (−2) = 0。
    c.realign_venue_cash("polymarket", Decimal::from(98));
    let alerts = drain_alerts(&mut rx);
    assert!(
        ghost_alerts(&alerts).is_empty(),
        "a realign explained by A's own window facts stays a normal correction: {alerts:?}"
    );
}
