#![cfg(test)]

//! 场地证据窗游标在账本容量边界上的行为（issue 723）：journal 是有界窗口
//! （`JOURNAL_CAP` 行，满容后 pop_front 再 push_back），行数封顶后不再增长
//! —— 以「行数」为键的游标从此每次 skip 都落在空窗口，新记账的现金事实
//! 永久进不了 booked，场地的每一笔合法修正都被误判 unattributed（告警风
//! 暴 + 窗口证据模型失效）。
//!
//! 修复语义：游标 = 「上次消费过的最后一行的全局序号」（进程内单调累计
//! 行计数器 − 窗口长度 + 下标），淘汰不再冻结它。四条验收：
//! 1. 满容后再记账的事实照常进 booked，相符报数静默；
//! 2. 跨容量边界多次淘汰仍正确，真实未知差额只对未知残差告警；
//! 3. 游标之后的行被淘汰（缺口）→ 恰一次 cash_evidence_gap 告警、本窗不
//!    判 residual、下一窗恢复正常；
//! 4. 账本重建（reset / 删户）后游标不跨代复用旧行号。

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

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

fn alerts_with<'a>(alerts: &'a [String], tag: &str) -> Vec<&'a String> {
    alerts.iter().filter(|m| m.contains(tag)).collect()
}

/// 容量边界现场（issue 723）：真值进场 → 灌到恰好满容（`JOURNAL_CAP` 行）
/// → 相符报数收窗。返回后：游标停在全局序号 `JOURNAL_CAP`（行数语义在此
/// 刻与序号语义重合 —— 分歧只出现在下一次 append 之后）、balance =
/// 1_099.99、窗口已对账干净、事件槽清空。
fn capacity_fixture(tag: &str) -> (Core, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    let mut c = live_core_fixture(tag);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.set_balance(Decimal::from(1_000)); // Realign 行，不算事实
    c.realign_venue_cash("polymarket", Decimal::from(1_000)); // 真值进场，静默
    let _ = drain_alerts(&mut rx);
    for _ in 0..(crate::ledger::JOURNAL_CAP - 1) {
        c.accounts
            .active_ledger_mut()
            .credit_cashflow(dec!(0.01), "rebate");
    }
    // 满容时刻的相符报数：booked = 99.99，residual = 0 → 静默。
    c.realign_venue_cash("polymarket", Decimal::new(109999, 2));
    assert!(
        alerts_with(&drain_alerts(&mut rx), "unattributed_cash_move").is_empty(),
        "fixture precondition: the report at the cap must be explained"
    );
    (c, rx)
}

/// 验收 1（issue 723 反例）：账本满容后游标已停在行数 `JOURNAL_CAP`，此后
/// 每一次按行数 skip 都得到空窗口 —— 新记账的现金事实永久进不了 booked，
/// 相符报数被误判 unattributed。修复后：游标是全局序号，满容后的新事实照
/// 常进窗。
#[test]
fn facts_appended_after_the_window_is_full_still_reach_booked() {
    let (mut c, mut rx) = capacity_fixture("journal-cursor-cap");
    // 满容后再记一笔已归属事实（cashflow 形态）：balance 1099.99 → 1100.99。
    c.accounts
        .active_ledger_mut()
        .credit_cashflow(dec!(1), "rebate");
    // 相符报数：booked 必须含这笔 1.00 → residual = 0 → 静默。
    c.realign_venue_cash("polymarket", Decimal::new(110099, 2));
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts_with(&alerts, "unattributed_cash_move").is_empty(),
        "a fact booked after the window filled must be credited to booked: {alerts:?}"
    );
}

/// 验收 2（issue 723）：跨容量边界多次淘汰仍正确（每次 append 都在淘汰最
/// 老行，窗口事实照常进 booked）；叠加真实未知差额时，只对未知残差告警
/// —— 窗口内已记账的 2.00 事实被对账吸收，residual 恰好等于未知部分。
#[test]
fn multiple_evictions_stay_explained_and_only_the_unknown_residual_alerts() {
    let (mut c, mut rx) = capacity_fixture("journal-cursor-evict");
    // 跨边界 5 笔事实（每笔都在触发淘汰）：balance 1099.99 → 1102.49。
    for _ in 0..5 {
        c.accounts
            .active_ledger_mut()
            .credit_cashflow(dec!(0.50), "rebate");
    }
    c.realign_venue_cash("polymarket", Decimal::new(110249, 2));
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts_with(&alerts, "unattributed_cash_move").is_empty(),
        "facts booked across repeated evictions must still explain the report: {alerts:?}"
    );
    // 真实未知差额：账本再进 2.00（balance → 1104.49），场地报数里既含这
    // 2.00 又额外少了 7.00（报 1097.49）→ residual = −7.00 恰为未知部分。
    c.accounts
        .active_ledger_mut()
        .credit_cashflow(dec!(2), "rebate");
    c.realign_venue_cash("polymarket", Decimal::new(109749, 2));
    let alerts = drain_alerts(&mut rx);
    let ghost = alerts_with(&alerts, "unattributed_cash_move");
    assert_eq!(
        ghost.len(),
        1,
        "the unknown part must alert exactly once: {alerts:?}"
    );
    assert!(
        ghost[0].contains("residual=-7"),
        "only the unknown residual alerts (the booked 2.00 is absorbed): {}",
        ghost[0]
    );
}

/// 验收 3（issue 723 缺口路径）：两次报数之间灌进超过一整个窗口的行 ——
/// 游标之后的行未消费就被淘汰，证据窗不完整。修复后：本窗恰一次
/// cash_evidence_gap 告警（RiskAlert + 审计 JSONL 同一事实）、不判
/// residual（不误报 unattributed）、真值照常推进、游标重锚到最新行，下一
/// 窗恢复正常。
#[test]
fn an_evicted_window_alerts_the_gap_once_and_recovers_next_window() {
    let (mut c, mut rx) = capacity_fixture("journal-cursor-gap");
    // 灌 `JOURNAL_CAP + 1` 行：游标行本身被淘汰，且游标之后还有一行未消
    // 费就被逐出 —— 缺口成立。balance 1099.99 → 1200.00。
    for _ in 0..(crate::ledger::JOURNAL_CAP + 1) {
        c.accounts
            .active_ledger_mut()
            .credit_cashflow(dec!(0.01), "rebate");
    }
    // 相符报数：修复前 booked 只剩被截剩的窗口（残差 100.01 误判
    // unattributed）；修复后走缺口路径。
    c.realign_venue_cash("polymarket", Decimal::new(120000, 2));
    let alerts = drain_alerts(&mut rx);
    let gaps = alerts_with(&alerts, "cash_evidence_gap");
    assert_eq!(
        gaps.len(),
        1,
        "the eviction gap alerts exactly once: {alerts:?}"
    );
    assert!(
        alerts_with(&alerts, "unattributed_cash_move").is_empty(),
        "an incomplete window must not fake a residual verdict: {alerts:?}"
    );
    let path = c.audit_log_path().unwrap().to_string();
    let rows: Vec<serde_json::Value> = crate::jsonl::load(std::path::Path::new(&path), "audit");
    let events: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "cash_evidence_gap")
        .collect();
    assert_eq!(
        events.len(),
        1,
        "the audit log holds exactly one gap record"
    );
    assert_eq!(events[0]["venue"], "polymarket");
    // 下一窗恢复正常：1.00 事实 + 相符报数 → 全静默。
    c.accounts
        .active_ledger_mut()
        .credit_cashflow(dec!(1), "rebate");
    c.realign_venue_cash("polymarket", Decimal::new(120100, 2));
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts.is_empty(),
        "the next window must be normal again: {alerts:?}"
    );
}

/// 验收 4（issue 723）：账本重建后游标不得跨代复用旧行号。重置把 journal
/// 重写为单行 Reset —— 一代新证据；上一世序号的游标必须随之死亡（否则
/// 新账本的行永远落在游标之前，booked 恒 0）。
#[test]
fn a_reset_ledger_kills_the_venues_cursor_generation() {
    let dir = reset_audit_dir("journal-cursor-reset");
    let mut c = Core::new(CoreConfig {
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        dry_seed_balance: Decimal::from(1_000),
        // Default() points the record logs at the checkout's shared data/
        // dir — the reset self-check counts THEIR lines. Memory-only.
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    });
    // 模拟上一代留下的游标（探测通道只在 Live 写入，这里直接注入旧序号）。
    c.venue_fact_cursor.insert("polymarket".to_string(), 42);
    c.reset_trading(&reset_params("RESET"), "test", 0).unwrap();
    assert!(
        !c.venue_fact_cursor.contains_key("polymarket"),
        "a rebuilt ledger must not inherit the previous generation's cursor"
    );
}

/// 验收 4 的删户半边（issue 723）：`account.remove` 退役账本时，绑定在该
/// 账户上的场地游标一并清掉 —— 同名重建是新账本、新序号空间；未绑定到
/// 该账户的场地游标不动（隔离不被殃及）。
#[test]
fn an_account_remove_kills_only_its_own_venues_cursors() {
    let dir = reset_audit_dir("journal-cursor-remove");
    // issue 751: the persist half writes .env and refuses while a legacy
    // toml sits beside it — the fixture keeps the toml path ONLY as the
    // refusal probe (never written).
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,kalshi-acct\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_KALSHI_ACCT_NAME=\"k\"\nACCOUNT_KALSHI_ACCT_VENUE=\"kalshi\"\n",
    )
    .expect("fixture");
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
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            // Default() points the record logs at the checkout's shared
            // data/ dir — keep every fixture write inside the scratch dir.
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        },
        crate::account::AccountLedgers::new(
            vec![
                acct("default", "main", ""),
                acct("kalshi-acct", "k", "kalshi"),
            ],
            crate::account::AccountId::from("default"),
        ),
    );
    c.venue_fact_cursor.insert("kalshi".to_string(), 42);
    c.venue_fact_cursor.insert("polymarket".to_string(), 7);
    c.remove_account_persisting(
        &crate::account::AccountId::from("kalshi-acct"),
        "test",
        "unit",
    )
    .unwrap();
    assert!(
        !c.venue_fact_cursor.contains_key("kalshi"),
        "the removed account's venue cursor dies with the ledger"
    );
    assert!(
        c.venue_fact_cursor.contains_key("polymarket"),
        "a sibling venue's cursor is untouched"
    );
}
