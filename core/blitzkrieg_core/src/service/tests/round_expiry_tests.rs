#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

/// Regression: a filled entry must expire at the END of its round slot, not
/// the start. The old formula (slot * duration) put expiry in the past and
/// force-exited every position on arrival (hold=0s, "never trades").
#[test]
fn filled_entry_expires_at_round_end_not_start() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 300,
        ..Default::default()
    };
    let duration = cfg.round_duration_sec;
    let mut c = Core::new(cfg);
    // A round slot that started 60s ago; its end is 240s from now.
    let now_ms = 1_000_000_000i64;
    let slot = now_ms / 1000 / duration;
    let req = OrderRequest {
        token_id: "tok".into(),
        condition_id: "c".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(10),
        internal_key: "k".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot,
        account_id: crate::model::default_account_id(),
    };
    // Ask 0.40 crosses the buy limit with enough depth: an honest FOK
    // fills at the walked price, opening the position under test.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], now_ms);
    c.place(req, 0, now_ms).unwrap();
    let pos = &c.positions().open_positions()[0];
    let expected_end = (slot + 1) * duration * 1000;
    assert_eq!(
        pos.expires_at_ms, expected_end,
        "expiry must be the round END"
    );
    assert!(pos.expires_at_ms > now_ms, "expiry must be in the future");
    // time_left must be comfortably positive (not an instant force-exit).
    let time_left = (pos.expires_at_ms - now_ms) / 1000;
    assert!(
        time_left > 0 && time_left <= duration,
        "time_left={time_left}"
    );
}

/// Regression (kalshi discovery repair): `round_view` must emit ONE price
/// card per asset. The panel keys the cards by asset, so a round with
/// several markets for the same asset used to render duplicate cards
/// with colliding Vue keys (the "BTC ×4" panel bug).
#[test]
fn round_view_prices_dedupe_by_asset() {
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let end = (slot + 1) * 900 * 1000;
    let market = |asset: &str, q: &str, up: Decimal| crate::model::CryptoMarket {
        asset: asset.into(),
        condition_id: format!("c-{asset}-{q}"),
        question_id: q.into(),
        up_token_id: format!("{q}-up"),
        down_token_id: format!("{q}-down"),
        up_price: up,
        down_price: Decimal::ONE - up,
        expires_at_ms: end,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            // Three markets for BTC (a strike ladder) + one for ETH.
            markets: vec![
                market("BTC", "q1", dec!(0.60)),
                market("BTC", "q2", dec!(0.55)),
                market("BTC", "q3", dec!(0.50)),
                market("ETH", "e1", dec!(0.70)),
            ],
            now_ms: now,
        },
        now,
    );
    let view = c.round_view().expect("round view");
    assert_eq!(view.markets, 4, "the market count itself stays honest");
    let assets: Vec<&str> = view
        .market_prices
        .iter()
        .map(|p| p.asset.as_str())
        .collect();
    assert_eq!(
        assets,
        vec!["BTC", "ETH"],
        "one price card per asset, first market wins"
    );
    let btc = &view.market_prices[0];
    assert_eq!(btc.up, dec!(0.60), "first BTC market's price is kept");
}
