//! postonly_pair × `entry_maker` — the REAL #872 package through the
//! kernel's full entry pipeline, pinning the ticket's three contract teeth
//! at the OME level:
//!
//!   1. PAIR OF MAKERS, FEE 0 — the declared package's legs reach the OME
//!      as post-only Maker orders on the never-escalate clock
//!      (`maker_timeout_ms = -1`), quoted strictly UNDER the leg's own ask
//!      so they REST (the fill path a post-only buy has). The kernel's
//!      fee model prices a `Maker` fill at fee 0 (model.rs: "a maker fill
//!      pays no fee") — the pinning here is the routing: everything the
//!      package emits IS a maker leg.
//!   2. SINGLE LEG CANNOT ESCALATE — a naked remainder's completion re-quote
//!      is the same `FillPolicy::Maker` leg on the same never-escalate
//!      clock: no `:escalated` order exists for the declaring package even
//!      when the book moves under the resting order (the #464 honest
//!      mirror refuses, the rest never chases).
//!   3. NEAR-EXPIRY PULL CANCELS — inside the pull zone the package emits
//!      `breaks` for both legs, and the ingest cancel path (the same
//!      regime-change vocabulary trend_follow feeds) pulls the resting
//!      BUY orders — the ticket's "临近到期撤单" tooth, end to end.
//!
//! Fixture shape follows `pair_entry_maker_tests` (the same
//! `load_lua_package_inner` manifest-stamp construction).

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::{CryptoMarket, FillPolicy, OrderStatus, Side};
use crate::risk::RiskConfig;
use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal_macros::dec;

const SLOT: i64 = 1_888_889;
/// 60 s into the slot: `time_left_sec` 840 — inside the quoting window
/// (min_time_left_sec 60, pull_before_sec 20).
const NOW: i64 = SLOT * 900 * 1000 + 60_000;
const UP: &str = "po-up";
const DOWN: &str = "po-down";

fn package_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/postonly_pair")
}

fn pair_market() -> Vec<CryptoMarket> {
    vec![CryptoMarket {
        asset: "BTC".into(),
        condition_id: "po-cond".into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (SLOT + 1) * 900 * 1000,
        round_slot: SLOT,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }]
}

/// Core hosting the REAL postonly_pair, declarations stamped from its
/// manifest exactly as `load_lua_package_inner` stamps them. NO fee
/// schedule injected (the package's pair cost model reads no fee — the
/// maker fee is 0); overrides patch the tunables bag before the adapter
/// is built.
fn core_with_po(overrides: &[(&str, &str)]) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100_000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        min_round_age_sec: 0,
        min_shares: dec!(1),
        max_shares: dec!(10_000),
        max_positions_per_asset: Some(4),
        order_log_path: None,
        trade_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        positions: crate::position::PositionConfig {
            max_positions: 8,
            ..Default::default()
        },
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host_disabled(&mut eng, cfg.trend, cfg.spread_arb);
    let loaded = load_lua_package(&package_dir()).expect("postonly_pair package must load");
    assert!(
        loaded.entry_maker,
        "the fixture premise: the manifest declares entry_maker"
    );
    let mut tunables = loaded.tunables;
    for (name, value) in overrides {
        tunables.insert((*name).to_string(), (*value).to_string());
    }
    let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        .declare_trailing_exit(loaded.trailing_exit)
        .declare_entry_maker(loaded.entry_maker);
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", package_dir().display()),
    )
    .expect("postonly_pair registers");
    assert!(eng.set_strategy_enabled("postonly_pair", true));
    c.enable_engine(eng);
    c
}

/// The round + both leg books, fed fresh: ask 0.48 / 0.45 with depth 600.
fn feed_pair_books(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: pair_market(),
            now_ms: now,
        },
        now,
    );
    let mut book = |token: &str, ask: rust_decimal::Decimal, now: i64| {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(ask - dec!(0.01), dec!(600))],
                asks: vec![(ask, dec!(600))],
                now_ms: now,
            },
            now,
        );
    };
    book(UP, dec!(0.48), now);
    book(DOWN, dec!(0.45), now);
}

/// One leg's fresh book at `now`.
fn mirror_book(c: &mut Core, token: &str, ask: rust_decimal::Decimal, now: i64) {
    c.engine_on_data(
        DataEvent::Book {
            token_id: token.into(),
            bids: vec![(ask - dec!(0.01), dec!(600))],
            asks: vec![(ask, dec!(600))],
            now_ms: now,
        },
        now,
    );
}

/// Tooth 1: BOTH legs are never-escalating post-only Maker orders that
/// REST strictly under the ask — the fill path of a maker fee of 0.
#[test]
fn both_legs_rest_post_only_on_the_never_escalate_clock() {
    let mut c = core_with_po(&[]);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    let orders = c.list_orders();
    assert_eq!(orders.len(), 2, "exactly the two pair legs, no chase");
    let mut live = 0usize;
    for o in &orders {
        assert_eq!(o.mode, FillPolicy::Maker, "the leg is post-only Maker");
        assert!(o.mode.post_only(), "Maker is the post-only face");
        assert_eq!(o.maker_timeout_ms, -1, "the never-escalate clock");
        assert!(o.escalate_at_ms.is_none(), "no deadline is armed");
        assert!(
            !o.internal_key.ends_with(":escalated"),
            "no taker continuation for a maker leg"
        );
        if o.status == OrderStatus::Live {
            live += 1;
        }
    }
    assert_eq!(live, 2, "both quotes REST strictly under their asks");
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(up.price, dec!(0.47), "one tick under the UP ask");
    assert_eq!(down.price, dec!(0.44), "one tick under the DOWN ask");

    // The OME never arms the sweep: the maker fee stays 0 through the
    // legs' whole life — there is no taker side to pay.
    assert_eq!(c.ome().next_escalation_ms(), i64::MAX);
}

/// Tooth 2: the UP leg fills (the ask descends ONTO the quote); the naked
/// remainder's completion re-quote for DOWN is still a post-only Maker on
/// the never-escalate clock — a single leg cannot escalate, and the book
/// moving under the resting leg never produces a taker.
#[test]
fn the_naked_legs_completion_requote_stays_a_maker_and_never_esclates() {
    let mut c = core_with_po(&[]);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    // The UP leg fills maker-style: the ask descends onto 0.47.
    mirror_book(&mut c, UP, dec!(0.47), NOW + 3_000);
    let held = c.positions().open_positions();
    assert_eq!(held.len(), 1, "only the filled UP leg is held");
    assert_eq!(held[0].token_id, UP);

    // The next cycle re-qualifies the naked DOWN leg: still a maker order
    // (the top-up rule bids it), replaced into the book. Both books are
    // refreshed at the requote instant — the freshness gate prices off
    // live books only.
    mirror_book(&mut c, UP, dec!(0.47), NOW + 20_000);
    mirror_book(&mut c, DOWN, dec!(0.45), NOW + 20_000);
    let placed = c.engine_evaluate(NOW + 20_000);
    assert!(placed >= 1, "the completion leg re-places");
    let orders = c.list_orders();
    let down = orders
        .iter()
        .find(|o| o.token_id == DOWN && o.status == OrderStatus::Live)
        .expect("a live DOWN order restates the completion");
    assert_eq!(down.mode, FillPolicy::Maker, "the completion is a maker");
    assert_eq!(down.maker_timeout_ms, -1, "never-escalate on the re-quote");
    assert!(down.escalate_at_ms.is_none(), "no deadline on the re-quote");
    assert!(
        !down.internal_key.ends_with(":escalated"),
        "a single leg has no taker continuation"
    );
    assert_eq!(c.ome().next_escalation_ms(), i64::MAX);

    // A rising DOWN book (the quote would now cross) hits the #464 mirror:
    // the REPLACEMENT post is refused and nothing chases — the naked leg's
    // tail stays pure maker until the pull or expiry.
    mirror_book(&mut c, DOWN, dec!(0.44), NOW + 40_000);
    let placed = c.engine_evaluate(NOW + 40_000);
    let _ = placed; // 0 or 1 — the refusal path is what must NOT chase
    for o in c.list_orders().iter().filter(|o| o.token_id == DOWN) {
        assert_eq!(
            o.mode,
            FillPolicy::Maker,
            "every DOWN order the package ever places is post-only Maker"
        );
    }
    assert!(
        !c.list_orders()
            .iter()
            .any(|o| o.token_id == DOWN && o.mode == FillPolicy::Taker),
        "no taker order exists for the naked leg"
    );
}

/// Tooth 3: inside the pull zone (time_left <= pull_before_sec = 20) the
/// package emits `breaks` for both legs and the ingest path cancels the
/// resting BUY orders — the near-expiry pull, end to end.
#[test]
fn the_pull_zone_cancels_both_resting_bids() {
    let mut c = core_with_po(&[]);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");
    assert_eq!(c.list_orders().len(), 2, "two riders in the book");

    //Inside the quoting window the legs only re-place (throttled). Walk the
    // clock into the pull zone: t = T-20 (the zone boundary, inclusive).
    // The package latches per condition per round, so a NEW round slot puts
    // a fresh pair in; instead ride the SAME round's clock down: feed a
    // book tick whose engine cycle sweeps time_left to 20.
    let near_end = SLOT * 900 * 1000 + 880_000; // time_left 20s
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: pair_market(),
            now_ms: near_end,
        },
        near_end,
    );
    feed_pair_books(&mut c, near_end);
    mirror_book(&mut c, UP, dec!(0.48), near_end);
    mirror_book(&mut c, DOWN, dec!(0.45), near_end);
    // The evaluate queueing the breaks; a BOOK event after it drains the
    // queued breaks into the ingest cancel path.
    c.engine_evaluate(near_end);
    c.engine_on_data(
        DataEvent::Book {
            token_id: UP.into(),
            bids: vec![(dec!(0.47), dec!(600))],
            asks: vec![(dec!(0.48), dec!(600))],
            now_ms: near_end,
        },
        near_end,
    );

    // The ingest cancel path runs on the pulls: the resting BUYs are gone.
    let riders: Vec<_> = c
        .list_orders()
        .into_iter()
        .filter(|o| o.side == Side::Buy && o.status == OrderStatus::Live)
        .collect();
    assert!(
        riders.is_empty(),
        "the pull zone cancels every resting BUY (bids pulled, no riders)"
    );

    // Re-evaluating inside the zone emits nothing new (the pulled latch).
    let placed = c.engine_evaluate(near_end + 1_000);
    assert_eq!(placed, 0, "nothing re-quotes inside the pulled zone");
}
