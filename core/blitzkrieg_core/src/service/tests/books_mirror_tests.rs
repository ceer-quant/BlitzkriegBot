#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

// ── #496: TP maker-first 退出单必须挂买一上方一档（真 post-only 单）────

/// Reverse acceptance: a TP maker-first exit priced AT the best bid is a
/// crossing post-only SELL — the venue refuses it every time (`WouldCross`),
/// so live's TP exit can never rest: the storm #163 only backs off. The fix
/// prices the resting SELL one tick ABOVE the bid (clamped inside the band
/// so it never crosses the ask either), which is what a post-only order
/// requires to be accepted at all. Taker exits (stop/force/time) keep
/// `marketable_exit_price` and are untouched.
#[test]
fn tp_maker_exit_rests_one_tick_above_the_bid_not_at_the_touch() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let end = (slot + 1) * 900 * 1000;

    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "c".into(),
                question_id: "q".into(),
                up_token_id: "tok".into(),
                down_token_id: "d".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: end,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: true,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        },
        now,
    );
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "c".into(),
        side: crate::model::Side::Buy,
        mode: crate::model::FillPolicy::Taker,
        price: dec!(0.45),
        size: dec!(10),
        internal_key: "k".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot,
        account_id: crate::model::default_account_id(),
    };
    c.place(req, 0, now).unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);

    // Profitable book with a TWO-tick spread: bid 0.95 / ask 0.97. TP
    // fires (+137%), and a resting post-only SELL has room one tick above
    // the bid (0.96 < 0.97 ask). The pre-fix order priced AT the bid
    // (0.95) — live refuses that POST every time.
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.95), dec!(100))],
            asks: vec![(dec!(0.97), dec!(100))],
            now_ms: now + 1000,
        },
        now + 1000,
    );
    c.tick(now + 1200).unwrap();

    // The TP exit must be a LIVE maker SELL resting one tick above the
    // bid — not filled at the touch (the old optimistic dry fill), not
    // sitting at the bid where live would refuse it.
    assert_eq!(c.positions().open_positions().len(), 1, "not closed yet");
    let sells: Vec<_> = c
        .ome()
        .all()
        .into_iter()
        .filter(|o| o.side == crate::model::Side::Sell && o.status.is_live())
        .collect();
    assert_eq!(sells.len(), 1, "one live exit SELL: {:?}", sells);
    assert_eq!(
        sells[0].mode,
        crate::model::FillPolicy::Maker,
        "TP exits are maker-first"
    );
    assert_eq!(
        sells[0].price,
        dec!(0.96),
        "one tick ABOVE the bid — the only price a post-only SELL can rest at"
    );
}

/// Regression: a Rust-native feed tick (engine_on_data) must mirror the book
/// into Core.books so the exit path can value open positions. Previously
/// Core.books stayed empty under --feed-ws, so currentPrice never moved and
/// no TP/trailing/SL exit could fire (positions looked frozen at entry).
#[test]
fn engine_feed_populates_core_books_and_enables_exits() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let end = (slot + 1) * 900 * 1000;

    // Feed a round + a book the way the Rust feed layer does.
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "c".into(),
                question_id: "q".into(),
                up_token_id: "tok".into(),
                down_token_id: "d".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: end,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: true,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        },
        now,
    );
    // Open a position directly, then feed a profitable book via engine_on_data
    // and tick: the exit path must now see the higher price and exit.
    // A FOK taker fills against the resting ASK (0.45), so the limit must
    // reach the touch.
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "c".into(),
        side: crate::model::Side::Buy,
        mode: crate::model::FillPolicy::Taker,
        price: dec!(0.45),
        size: dec!(10),
        internal_key: "k".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot,
        account_id: crate::model::default_account_id(),
    };
    c.place(req, 0, now).unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);
    let entry = c.positions().open_positions()[0].entry_price;

    // Feed a much higher book through the ENGINE path (the bug's path).
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.95), dec!(100))],
            asks: vec![(dec!(0.97), dec!(100))],
            now_ms: now + 1000,
        },
        now + 1000,
    );
    c.tick(now + 1200).unwrap();

    // #496: the maker-first TP exit now RESTS one tick above the bid
    // (0.96) instead of filling optimistically at the touch — the shape a
    // live post-only order requires. The bid rising into the resting
    // limit is the honest maker fill that closes the position.
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.96), dec!(100))],
            asks: vec![(dec!(0.98), dec!(100))],
            now_ms: now + 2000,
        },
        now + 2000,
    );
    c.tick(now + 2200).unwrap();

    // Position closed with a profit, and the recorded exit reflects the
    // higher price (not frozen at entry).
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "exit engine must close on profit"
    );
    let closed = &c.positions().closed_positions()[0];
    assert!(
        closed.exit_price > entry,
        "exit must be above entry (was frozen before fix)"
    );
    assert!(closed.net_pnl_usd > Decimal::ZERO);
}

/// KI-1 regression: a resting maker order must fill when the MARKET path
/// (`engine_on_data`) delivers a crossing book — not only when a
/// `books.snapshot` RPC arrives. Before the fix the feed path only mirrored
/// the book, so every dry entry sat until its maker→taker escalation and
/// dry's fill/fee economy was systematically taker-side.
#[test]
fn engine_feed_cross_fills_resting_maker() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let end = (slot + 1) * 900 * 1000;

    // Register the round the way the feed does, then seed the book.
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "c".into(),
                question_id: "q".into(),
                up_token_id: "tok".into(),
                down_token_id: "d".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: end,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: true,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        },
        now,
    );

    // Rest a maker BUY at 0.43 via the normal entry path.
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "c".into(),
        side: crate::model::Side::Buy,
        mode: crate::model::FillPolicy::Maker,
        price: dec!(0.43),
        size: dec!(10),
        internal_key: "k".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot,
        account_id: crate::model::default_account_id(),
    };
    let id = c.place(req, 0, now).unwrap().0;
    assert_eq!(
        c.ome.get(&id).map(|o| o.status),
        Some(crate::model::OrderStatus::Live),
        "maker rests while the book is above its limit"
    );

    // A feed book whose ask crosses the resting bid: the MAKER path must
    // fill it at the resting limit (maker role — no taker fee).
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.41), dec!(100))],
            asks: vec![(dec!(0.42), dec!(100))],
            now_ms: now + 500,
        },
        now + 500,
    );
    let order = c.ome.get(&id).expect("order present");
    assert_eq!(order.status, crate::model::OrderStatus::Filled);
    assert_eq!(order.filled_size, dec!(10));
    assert_eq!(
        order.avg_fill_price,
        Some(dec!(0.43)),
        "fills at the resting limit, not the crossing ask"
    );
    // Maker fills are free (E17): the ledger holds only the notional.
    let bal = c.ledger().balance();
    assert!(
        bal < dec!(1000) && bal > dec!(957),
        "reserved notional 4.30 spent, no taker fee charged (got {bal:?})"
    );
}

/// The complementary invariant: in the default fill model a NON-crossing
/// feed book must NOT fill the resting maker (no phantom fills).
#[test]
fn engine_feed_does_not_fill_maker_without_cross() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    let now = 1_000_000_000i64;
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        },
        now,
    );
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "c".into(),
        side: crate::model::Side::Buy,
        mode: crate::model::FillPolicy::Maker,
        price: dec!(0.43),
        size: dec!(10),
        internal_key: "k".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: now / 1000 / 900,
        account_id: crate::model::default_account_id(),
    };
    let id = c.place(req, 0, now).unwrap().0;
    // Ask 0.44 > limit 0.43: still resting after many feed ticks.
    for i in 1..5 {
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.44), dec!(100))],
                now_ms: now + i * 100,
            },
            now + i * 100,
        );
    }
    assert_eq!(
        c.ome.get(&id).map(|o| o.status),
        Some(crate::model::OrderStatus::Live),
        "non-crossing feed must not fill"
    );
}
