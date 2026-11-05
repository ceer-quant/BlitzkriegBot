#![cfg(test)]

//! Pending-close exit reasons (issue #190). The table is a ledger of IN-FLIGHT
//! closes, not a history: an entry must not outlive the position it belongs to,
//! the table must never grow without bound, and a later position on the same
//! token must never inherit a dead reason.

use super::settlement_service_tests::{
    open_and_anchor, scratch, settling_core, winning_resolution,
};
use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use crate::position::OpenParams;
use rust_decimal_macros::dec;

/// The fixture's config, exposed so a test can vary exactly one field.
fn cfg() -> CoreConfig {
    CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        ..Default::default()
    }
}

/// Dry core, seed 1000, automated exits OFF: the fixture drives every close
/// explicitly so the path under test is the only one that runs.
fn core() -> Core {
    let mut c = Core::new(cfg());
    c.set_balance(dec!(1000));
    c
}

fn order(side: Side, price: Decimal, size: Decimal, token: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side,
        mode: FillPolicy::Taker,
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

/// Open a 10-share position on `token` at 0.40 off a resting ask.
fn open_position(c: &mut Core, token: &str, at_ms: i64) {
    c.book_snapshot(token, vec![], vec![(dec!(0.40), dec!(500))], at_ms - 1);
    let (_, status) = c
        .place(
            order(Side::Buy, dec!(0.40), dec!(10), token, "entry"),
            0,
            at_ms,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
}

/// Close the position on `token` the way a venue-side manual sell would: a
/// SELL fill the OME never issued, folded in by the reconcile sweep. This
/// path does NOT go through `on_position_closed`.
fn venue_close(c: &mut Core, token: &str, at_ms: i64) {
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: format!("manual-{token}"),
            trade_id: format!("t-{token}"),
            token_id: token.into(),
            side: Side::Sell,
            size: dec!(10),
            price: dec!(0.60),
            ts_ms: at_ms,
            tx_hash: None,
            maker: Some(false),
        }],
        now_ms: at_ms + 100,
    };
    c.reconcile(snap).unwrap();
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .all(|p| p.token_id != token),
        "the venue close must leave no open position on {token}"
    );
}

/// A close retires the reason that was pending for it, whichever path lands
/// the close — including the reconciled venue close, which does not funnel
/// through `on_position_closed`.
#[test]
fn a_close_retires_the_pending_exit_reason() {
    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    // The exit policy decided to leave (a stop); the reason stays pending
    // until the closing sell fills.
    c.note_exit_reason("tok", ExitReason::StopLoss);
    assert_eq!(c.exit_reason_ledger.exit_reasons.len(), 1);
    assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(1));
    assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));

    venue_close(&mut c, "tok", 1_100);

    assert!(
        c.exit_reason_ledger.exit_reasons.is_empty(),
        "a closed position must not leave its exit reason behind: {:?}",
        c.exit_reason_ledger.exit_reasons
    );
    assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(0));
    assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));
}

/// The other cleanup site, and the one every internal close funnels through:
/// a position that settles locally never had its exit fill, so
/// `take_exit_reason` never ran for it and the retirement inside
/// `on_position_closed` is the only thing that removes the entry.
#[test]
fn a_local_settlement_retires_the_pending_exit_reason() {
    let dir = scratch("settle-reason");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    // A stop was decided, its exit never filled, and the market resolved
    // under it: the reason must die with the position it belonged to.
    c.note_exit_reason("tok", ExitReason::StopLoss);
    assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(1));

    c.on_market_resolution(winning_resolution(), 2_001);

    assert_eq!(
        c.positions().closed_positions()[0].exit_reason,
        ExitReason::Settlement,
        "the settlement, not the dead stop, is what closed this position"
    );
    assert!(
        c.exit_reason_ledger.exit_reasons.is_empty(),
        "a settled position must not leave its exit reason behind: {:?}",
        c.exit_reason_ledger.exit_reasons
    );
    assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(0));
    assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The mis-attribution this issue is about: a reason recorded for an exit
/// that never filled must not be applied to the NEXT position on the same
/// token. The internal exit-fill path is the one that reads the table, so
/// the assertion is on the recorded close reason.
#[test]
fn a_later_position_never_inherits_a_dead_exit_reason() {
    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    c.note_exit_reason("tok", ExitReason::StopLoss); // an exit that never filled
    venue_close(&mut c, "tok", 1_100); // the position dies first

    // The next round reuses the token (past the exit cooldown): a new
    // position, with no reason of its own.
    open_position(&mut c, "tok", 200_000);
    c.book_snapshot("tok", vec![(dec!(0.55), dec!(500))], vec![], 200_100);
    let (_, status) = c
        .place(
            order(Side::Sell, dec!(0.55), dec!(10), "tok", "close-2"),
            0,
            200_100,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);

    let closed = c.positions().closed_positions();
    assert_eq!(closed.len(), 2);
    assert_eq!(
        closed[1].exit_reason,
        ExitReason::Manual,
        "the second close inherited the first position's dead stop reason"
    );
}

/// The table is bounded, and the bound evicts the entries whose position is
/// already gone before it touches a live one.
#[test]
fn the_table_is_bounded_and_evicts_dead_entries_first() {
    let mut c = core();
    open_position(&mut c, "live", 1_000);
    c.note_exit_reason("live", ExitReason::TakeProfit);
    for i in 0..EXIT_REASON_TABLE_MAX {
        c.note_exit_reason(&format!("dead-{i}"), ExitReason::StopLoss);
    }
    assert_eq!(
        c.exit_reason_ledger.exit_reasons.len(),
        EXIT_REASON_TABLE_MAX,
        "the table must never exceed its bound"
    );
    assert_eq!(
        c.exit_reason_ledger.exit_reasons.get("live"),
        Some(&ExitReason::TakeProfit),
        "a live position's reason must survive while dead entries remain"
    );
    assert_eq!(c.exit_reasons_view()["evictedTotal"], serde_json::json!(1));
    assert_eq!(
        c.exit_reason_ledger.exit_reason_order.len(),
        c.exit_reason_ledger.exit_reasons.len()
    );
}

/// …and when EVERY entry still has an open position, the oldest is the one
/// dropped (the table cannot be allowed to exceed the bound either way).
#[test]
fn the_oldest_entry_is_evicted_when_all_of_them_are_live() {
    let mut c = core();
    for i in 0..=EXIT_REASON_TABLE_MAX {
        let token = format!("tok-{i}");
        c.positions.open(
            OpenParams {
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: crate::model::SignalDirection::Up,
                token_id: token.clone(),
                condition_id: "cond".into(),
                entry_price: dec!(0.40),
                expires_at_ms: 9_999,
                was_maker: false,
                target_exit_price: None,
                account_id: crate::model::default_account_id(),
                venue: String::new(),
                partner_token_id: String::new(),
                round_duration_sec: 0,
                round_slot: 0,
            },
            1_000,
        );
        c.note_exit_reason(&token, ExitReason::TakeProfit);
    }
    assert_eq!(
        c.exit_reason_ledger.exit_reasons.len(),
        EXIT_REASON_TABLE_MAX
    );
    assert!(
        !c.exit_reason_ledger.exit_reasons.contains_key("tok-0"),
        "the oldest entry is the one evicted"
    );
    assert!(
        c.exit_reason_ledger
            .exit_reasons
            .contains_key(&format!("tok-{EXIT_REASON_TABLE_MAX}"))
    );
    assert_eq!(
        c.exit_reason_ledger.exit_reason_order.len(),
        c.exit_reason_ledger.exit_reasons.len()
    );
}

/// The round rollover is the backstop: an entry whose position is gone by
/// then can no longer belong to anything live, so it is swept — which is
/// what keeps `orphaned` at 0 for the panel.
#[test]
fn a_round_rollover_sweeps_reasons_whose_position_is_gone() {
    let mut c = core();
    open_position(&mut c, "live", 1_000);
    c.note_exit_reason("live", ExitReason::TakeProfit);
    // An exit submitted for a position that closed in between: the entry is
    // real, its position is not.
    c.note_exit_reason("ghost", ExitReason::StopLoss);
    assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(1));

    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![],
            now_ms: 5_000,
        },
        5_000,
    );

    assert!(
        !c.exit_reason_ledger.exit_reasons.contains_key("ghost"),
        "a round rollover must sweep a reason whose position is gone"
    );
    assert_eq!(
        c.exit_reason_ledger.exit_reasons.get("live"),
        Some(&ExitReason::TakeProfit),
        "the live position's reason is not swept"
    );
    assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));
    assert_eq!(c.exit_reasons_view()["sweptTotal"], serde_json::json!(1));
}

/// Issue #205 requirement 4: the orderbook-staleness budget in force is
/// readable from `engine.stats`, so a panel or a harness never has to parse
/// the boot log. Built through `install_engine` — the production
/// `CoreConfig -> EngineConfig` mapping — so this covers the whole chain and
/// reports the value the engine *enforces*, not just the field that was set.
#[test]
fn engine_stats_reports_the_orderbook_freshness_budget() {
    let view = |budget_ms: i64| -> serde_json::Value {
        let mut c = Core::new(CoreConfig {
            max_orderbook_stale_ms: budget_ms,
            ..cfg()
        });
        let engine_cfg = c.config().engine_config();
        c.enable_engine(crate::engine::Engine::new(engine_cfg));
        c.engine_stats_at(1_000)["orderbookFreshness"].clone()
    };

    let widened = view(25_000);
    assert_eq!(widened["maxStaleMs"], serde_json::json!(25_000));
    assert_eq!(widened["checkEnabled"], serde_json::json!(true));
    // The shipped default is the historic hardcoded value: #205 made the knob
    // configurable without moving it.
    let shipped = view(crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS);
    assert_eq!(shipped["maxStaleMs"], serde_json::json!(8_000));
    assert_eq!(shipped["checkEnabled"], serde_json::json!(true));
    // 0 = the operator switched the gate off. Reported as such rather than as
    // a "0ms budget", which would read as "refuse everything".
    let off = view(0);
    assert_eq!(off["maxStaleMs"], serde_json::json!(0));
    assert_eq!(off["checkEnabled"], serde_json::json!(false));
}

/// Strategy-signal sells price off the fresh best bid. The mid of a 0.42/0.60
/// book is 0.51; a limit up there finds no bid and the FOK dies. A bid-priced
/// sell (slipped two ticks to 0.40) lifts the 0.42 bid.
#[test]
fn a_strategy_exit_sells_the_best_bid_not_the_mid() {
    let book = crate::sim::Book {
        bids: vec![(dec!(0.42), dec!(500))],
        asks: vec![(dec!(0.60), dec!(500))],
        received_ms: 2_000,
    };
    let bid = super::risk_admission::strategy_signal_sell_bid(Some(&book), 2_000, 60);
    assert_eq!(bid, Some(dec!(0.42)));
    // The mid is the number this function must not return.
    assert_ne!(bid, book.mid());

    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    c.book_snapshot(
        "tok",
        vec![(dec!(0.42), dec!(500))],
        vec![(dec!(0.60), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: false,
        });
    c.tick(2_000).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "the bid was marketable; the position must close"
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::TakeProfit);
    assert_eq!(
        closed.exit_price,
        dec!(0.42),
        "filled at the resting bid, not the 0.51 mid"
    );
}

/// No bid, or a bid older than the pricing window: the exit is withheld.
/// Selling `current_price` (the entry mark, 0.40) into either book would be
/// a trade nobody is bidding right now.
#[test]
fn a_strategy_exit_without_a_fresh_bid_does_not_sell() {
    let dark = crate::sim::Book {
        bids: vec![],
        asks: vec![(dec!(0.60), dec!(500))],
        received_ms: 2_000,
    };
    assert_eq!(
        super::risk_admission::strategy_signal_sell_bid(Some(&dark), 2_000, 60),
        None,
        "an ask-only book has no buyer"
    );
    let stale = crate::sim::Book {
        bids: vec![(dec!(0.42), dec!(500))],
        asks: vec![(dec!(0.60), dec!(500))],
        received_ms: 1_000,
    };
    assert_eq!(
        super::risk_admission::strategy_signal_sell_bid(Some(&stale), 71_000, 60),
        None,
        "a 70s-old bid is not a quote"
    );

    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    // Ask only, stamped fresh. The entry mark (0.40) is still on the position.
    c.book_snapshot("tok", vec![], vec![(dec!(0.60), dec!(500))], 2_000);
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: false,
        });
    c.tick(2_000).unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);
    assert!(
        c.ome().live_orders().is_empty(),
        "no sell is rested off a mark"
    );

    // A real bid, but received a minute ago. Same refusal.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.42), dec!(500))],
        vec![(dec!(0.60), dec!(500))],
        1_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: false,
        });
    c.tick(71_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "a stale bid does not close the position"
    );
}

/// #935 fix 1: the strategy's own reason tag is what the ledger stamps when
/// the close fills — a take_profit close reads as `take_profit`, not the
/// blanket `strategy_signal`. Unknown tags keep the historic stamp.
#[test]
fn a_strategy_exit_carrying_a_known_reason_is_stamped_with_it() {
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("take_profit"),
        ExitReason::TakeProfit
    );
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("take_profit_price"),
        ExitReason::TakeProfit
    );
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("trailing_stop"),
        ExitReason::TrailingStop
    );
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("time_exit"),
        ExitReason::TimeExit
    );
    // Unknown tags fall back — no invented categories.
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("momentum_flip"),
        ExitReason::StrategySignal
    );
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag(""),
        ExitReason::StrategySignal
    );
    assert_eq!(
        super::risk_admission::exit_reason_from_strategy_tag("merge"),
        ExitReason::StrategySignal,
        "merge never routes through the sell path; a stray tag must not resemble one"
    );

    // End-to-end: the intent's tag lands on the closed row.
    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    c.book_snapshot(
        "tok",
        vec![(dec!(0.42), dec!(500))],
        vec![(dec!(0.60), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: false,
        });
    c.tick(2_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::TakeProfit);
}

/// #935 fix 2: the maker advisory converts ONLY when reason is profit-side
/// AND the kernel gate is on. Gate off (the shipped default) keeps the taker
/// sweep for every declaration; a `use_maker` stop never converts either way.
#[test]
fn the_maker_advisory_converts_only_profit_side_exits_behind_the_gate() {
    assert!(super::risk_admission::reason_is_profit_side(
        ExitReason::TakeProfit
    ));
    assert!(super::risk_admission::reason_is_profit_side(
        ExitReason::TrailingStop
    ));
    assert!(!super::risk_admission::reason_is_profit_side(
        ExitReason::StopLoss
    ));
    assert!(!super::risk_admission::reason_is_profit_side(
        ExitReason::TimeExit
    ));
    assert!(!super::risk_admission::reason_is_profit_side(
        ExitReason::StrategySignal
    ));

    let mut c = core();
    open_position(&mut c, "tok", 1_000);
    c.book_snapshot(
        "tok",
        vec![(dec!(0.42), dec!(500))],
        vec![(dec!(0.60), dec!(500))],
        2_000,
    );
    // Advisory on, gate OFF (the shipped default): the order goes out taker.
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: true,
        });
    c.tick(2_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    assert!(
        c.ome
            .live_orders()
            .iter()
            .all(|o| o.side == Side::Sell && o.mode != FillPolicy::Maker),
        "the default gate-off posture stays the taker sweep"
    );
}

/// #935 fix 2 end-to-end: gate ON + a declared advisory + a profit-side
/// reason → the close rests as a MAKER instead of sweeping the touch.
#[test]
fn gate_on_profit_side_advisory_rests_a_maker_close() {
    let mut c = core();
    let mut exit = c.config().positions.exit.clone();
    exit.strategy_exit_maker_first = true;
    let mut cfg2 = cfg();
    let mut pos_exit = cfg2.positions.clone();
    pos_exit.exit = exit;
    cfg2.positions = pos_exit;
    c = Core::new(cfg2);
    c.set_balance(dec!(1000));
    open_position(&mut c, "tok", 1_000);
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: true,
        });
    c.tick(2_000).unwrap();
    // A one-tick spread (0.40 bid / 0.44 ask) leaves an inside rest at 0.41:
    // the maker close rests and the position stays open until it fills or
    // times out — not swept at the 0.40 touch.
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the maker close is resting, not swept"
    );
    let rested: Vec<_> = c
        .ome()
        .live_orders()
        .into_iter()
        .filter(|o| o.side == Side::Sell)
        .collect();
    let rested = rested.first().expect("the maker close must be live");
    assert_eq!(rested.mode, FillPolicy::Maker);
    assert_eq!(rested.price, dec!(0.41));
}
