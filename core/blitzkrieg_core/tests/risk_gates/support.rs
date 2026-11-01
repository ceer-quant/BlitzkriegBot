//! Shared fixtures for the risk-gate acceptance tests.

pub use blitzkrieg_core::model::{
    CoreErrorCode, ExitReason, FillPolicy, Mode, OrderRequest, OrderRole, OrderStatus,
    OrderbookSnapshot, Side, SignalDirection,
};
pub use blitzkrieg_core::position::{
    DailyLossFile, DailyLossState, EquityBasis, OpenParams, PositionConfig, PositionManager,
    StopSuppressionCause, utc_day_index,
};
pub use blitzkrieg_core::risk::RiskConfig;
pub use blitzkrieg_core::schema::Event;
pub use blitzkrieg_core::service::{Core, CoreConfig};
pub use rust_decimal::Decimal;
pub use rust_decimal_macros::dec;

/// A fixed instant (2001-09-09, 01:46:40 UTC) whose UTC day index is 11_574 —
/// far from a boundary, so the tests can pick the day they mean.
pub(crate) const NOW: i64 = 1_000_000_000_000;
pub(crate) const DAY_MS: i64 = 86_400_000;

pub(crate) fn dry_core(positions: PositionConfig) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        ..Default::default()
    });
    c.set_balance(dec!(100));
    c
}

/// The round slot the kernel derives from the clock: a position opened in slot N
/// expires at the END of it, so this keeps the test's expiry in the future (a
/// hard-coded slot puts it in 1970 and ForceExit preempts every other rule).
pub(crate) fn slot_at(now: i64) -> i64 {
    now / 1000 / 900
}

pub(crate) fn order(
    tok: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    key: &str,
    now: i64,
) -> OrderRequest {
    OrderRequest {
        token_id: tok.into(),
        condition_id: "cond".into(),
        side,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot_at(now),
        account_id: blitzkrieg_core::model::default_account_id(),
    }
}

/// Open one LONG through the ordinary taker path (dry fills are immediate).
pub(crate) fn open_long(
    c: &mut Core,
    tok: &str,
    bid: Decimal,
    ask: Decimal,
    size: Decimal,
    now: i64,
) {
    c.book_snapshot(tok, vec![(bid, dec!(1000))], vec![(ask, dec!(1000))], now);
    let (_, status) = c
        .place(
            order(tok, Side::Buy, ask, size, &format!("entry:{tok}"), now),
            0,
            now,
        )
        .expect("the entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "one position must have opened"
    );
}

pub(crate) fn drain_alerts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            out.push(message);
        }
    }
    out
}

/// Open one LONG directly in the position manager (no Core), so a test can
/// hand it any book it likes — including none at all.
pub(crate) fn position(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    now: i64,
) -> String {
    let pos = pm.open(
        OpenParams {
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: tok.into(),
            condition_id: "cond".into(),
            entry_price: entry,
            expires_at_ms: now + 900_000,
            was_maker: false,
            target_exit_price: None,
            account_id: blitzkrieg_core::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            round_duration_sec: 0,
            round_slot: 0,
        },
        now,
    );
    pm.apply_entry_fill(&pos.id, shares, entry, Decimal::ZERO, OrderRole::Taker);
    pos.id
}
