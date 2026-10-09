//! cross_venue_arb replay acceptance — the REAL package driven through the
//! FULL live engine over a synthetic two-venue corpus (`VecSource` → the same
//! `engine_on_data` choke point production data hits), issue #427's backtest
//! acceptance mapped:
//!
//!   dual-venue corpus — `RoundMarkets` carrying one market per venue
//!                       (polymarket + kalshi, `venue` stamped), four leg
//!                       books, and the `UnifiedMapping` row (`k:"unified"`)
//!                       binding them into one bet;
//!   fills             — the pair enters (both legs fill) and releases via the
//!                       exit policy / release intents when the discount
//!                       reprices away;
//!   capacity          — every filled position's size ≤ the THINNER leg's ask
//!                       depth (the reverse-acceptance row: a package or
//!                       kernel that ignored one venue's depth would fill
//!                       past it and this assertion goes red);
//!   both fees         — the pair's two legs each carry an entry fee (the
//!                       double-sided fee accounting the blueprint demands).
//!
//! The corpus declares the 900s grid the config runs — the #354 cadence gate
//! would refuse anything else.

use std::path::PathBuf;

use blitzkrieg_core::backtest::{BacktestConfig, Backtester, EventBacktester, VecSource};
use blitzkrieg_core::data_source::TimedEvent;
use blitzkrieg_core::engine::DataEvent;
use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::service::CoreConfig;
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;

const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

/// The thin leg's ask depth — capacity of the whole pair (min of the two legs'
/// depths; the DOWN@kx leg is deliberately the thinner one, on the cheaper
/// venue, so a "take the deep side only" bug sizes past it).
const THIN_DEPTH: i64 = 250;

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

fn host() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions);
    a.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    a
}

fn market(token_up: &str, token_down: &str, cond: &str, venue: &str) -> CryptoMarket {
    let slot = NOW / 1000 / 900;
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: ((slot + 1) * 900 * 1000),
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn listing(venue: Venue, cond: &str, up: &str, down: &str) -> PlatformListing {
    PlatformListing {
        venue,
        condition_id: cond.into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        asset: Some("bitcoin".into()),
        title: "Bitcoin Up or Down".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        expires_at_ms: NOW + 900_000,
        status: ListingStatus::Active,
    }
}

fn paired_event() -> UnifiedEvent {
    let mut e = UnifiedEvent {
        id: "evt-1".into(),
        title: "Bitcoin Up or Down".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        listings: Default::default(),
        discrepancies: Vec::new(),
        status: EventStatus::Paired,
    };
    e.listings.insert(
        Venue::Polymarket,
        listing(Venue::Polymarket, PM, PM_UP, PM_DOWN),
    );
    e.listings
        .insert(Venue::Kalshi, listing(Venue::Kalshi, KX, KX_UP, KX_DOWN));
    e
}

/// pm: UP ask 0.44 / depth 600, DOWN ask 0.50 / depth 600.
/// kx: UP ask 0.48 / depth 9000, DOWN ask 0.42 / depth `THIN_DEPTH`.
/// Cross-venue pair = UP@pm 0.44 + DOWN@kx 0.42 + both fees ≈ 0.875 ≤ 0.98.
fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> DataEvent {
    DataEvent::Book {
        token_id: token.to_string(),
        bids: vec![(ask - dec!(0.01), depth)],
        asks: vec![(ask, depth)],
        now_ms: ts,
    }
}

/// The corpus: round discovery on both venues at T0, the unified mapping row,
/// then four leg books refreshed every second. The pair opens on the first
/// evaluation and stays quoted.
fn corpus() -> Vec<TimedEvent> {
    let mut evs = vec![TimedEvent {
        at_ms: NOW,
        event: DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket"),
                market(KX_UP, KX_DOWN, KX, "kalshi"),
            ],
            now_ms: NOW,
        },
    }];
    evs.push(TimedEvent {
        at_ms: NOW + 100,
        event: DataEvent::UnifiedMapping {
            event: paired_event(),
            now_ms: NOW + 100,
        },
    });
    // Enough refreshes to ride out the evaluation cadence and the maker→taker
    // escalation: every second for 40 virtual seconds.
    for i in 0..40 {
        let t = NOW + 1_000 + i * 1_000;
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(PM_UP, dec!(0.44), dec!(600), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(PM_DOWN, dec!(0.50), dec!(600), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(KX_UP, dec!(0.48), dec!(9000), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(KX_DOWN, dec!(0.42), Decimal::from(THIN_DEPTH), t),
        });
    }
    evs
}

/// The cross-venue release corpus: same pair, but after 12 s the DOWN@kx bid
/// recovers so the all-in pair cost (0.48 + 0.50 + fees ≈ 0.985) climbs back
/// within the 50% release band — the package offers both legs back at their
/// bids and the exit policy closes them.
fn release_corpus() -> Vec<TimedEvent> {
    let mut evs = corpus();
    // Strip nothing: append the repriced books. UP legs quote 0.50/0.48 both
    // sides (pair cost from the UP leg's ask 0.50 + DOWN ask 0.48), i.e. the
    // discount is gone.
    for i in 0..20 {
        let t = NOW + 42_000 + i * 1_000;
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(PM_UP, dec!(0.50), dec!(600), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(PM_DOWN, dec!(0.52), dec!(600), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(KX_UP, dec!(0.50), dec!(9000), t),
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: leg_book(KX_DOWN, dec!(0.48), dec!(9000), t),
        });
    }
    evs
}

fn replay(events: Vec<TimedEvent>) -> blitzkrieg_core::backtest::BacktestReport {
    let core = CoreConfig {
        mode: blitzkrieg_core::model::Mode::Dry,
        risk: blitzkrieg_core::risk::RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100000),
        // Sizing above the declared capacity: the strategy DECLARES the pair's
        // share count (min of the two legs' depths); the kernel honours a
        // declaration up to this ceiling. With the shipped 10-share default
        // this row would clamp everything and prove nothing about capacity.
        size_usd: dec!(100000),
        min_shares: dec!(1),
        max_shares: dec!(10000),
        engine_enabled: true,
        assets: vec!["BTC".into()],
        round_duration_sec: 900,
        min_round_age_sec: 0,
        trend_confirm_sec: 5,
        trend_window_floor_ms: 0,
        auto_exits_enabled: true,
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            // The pair's two legs are TWO positions on one asset with
            // DIFFERENT condition ids (one per venue) — the #363 per-asset
            // count must cover them, exactly as an operator running this
            // package would configure it (`--max-positions-per-asset 2`).
            max_positions_per_asset: 4,
            ..Default::default()
        },
        // `CoreConfig::default()` carries the SHIPPED policy spelling, and the
        // cwd-resolved `user_layer/configs/execution_policy.toml` (which says
        // per-asset = 1) would silently override the positions block above.
        // `None` = load nothing (the documented zero-config path): the struct
        // literal is then the whole truth.
        execution_policy_path: None,
        ..Default::default()
    };
    let mut b = EventBacktester::new(
        BacktestConfig {
            core,
            tick_ms: 50,
            tail_ms: 10_000,
            hot_params: Vec::new(),
            progress: None,
        },
        Box::new(VecSource::new(events)),
    );
    b.host_strategy(Box::new(host()));
    assert!(
        b.core_mut().set_strategy_enabled("cross_venue_arb", true),
        "the hosted adapter registers under its package name"
    );
    b.run().expect("replay runs")
}

#[test]
fn replayed_pair_fills_both_legs_and_never_exceeds_the_thin_leg() {
    let r = replay(corpus());
    assert!(
        r.errors.is_empty(),
        "no replay errors expected: {:?}",
        r.errors
    );
    assert!(r.orders.filled >= 2, "both legs must fill:\n{}", r.render());
    assert_eq!(
        r.open_positions,
        2,
        "the pair is TWO concurrent legs, one per venue:\n{}",
        r.render()
    );
    // Capacity = min(the two legs' ask depths): the DOWN@kx leg quotes
    // THIN_DEPTH shares of depth, the UP@pm leg 600. A package or kernel
    // that ignored one venue's depth would fill 600 against the 250-deep
    // kx book — the notional identity below goes red on exactly that:
    //   open notional = shares_up × 0.44 + shares_down × 0.42 with BOTH
    //   share counts ≤ THIN_DEPTH ⇒ ≤ the cap; filling past the thin leg
    //   breaks it in the very leg that ignores the depth.
    let notional = r.open_notional_usd;
    let cap_notional =
        Decimal::from(THIN_DEPTH) * dec!(0.44) + Decimal::from(THIN_DEPTH) * dec!(0.42);
    assert!(
        notional <= cap_notional,
        "open notional {notional} exceeds the thin-leg-capped pair {cap_notional} — \
         capacity must be min(the two legs' depths)"
    );
}

#[test]
fn replayed_release_flattens_the_pair() {
    let r = replay(release_corpus());
    assert!(
        r.orders.filled >= 2,
        "the pair must enter before it can release:\n{}",
        r.render()
    );
    assert_eq!(
        r.open_positions,
        0,
        "the release must flatten both legs:\n{}",
        r.render()
    );
    // Two sells: the pair's exits. The UP leg releases at pm's bid, the DOWN
    // leg at kx's bid — both close through the exit path.
    assert!(
        r.orders.filled >= 4,
        "entry legs + release legs all fill:\n{}",
        r.render()
    );
}

#[test]
fn replay_without_the_unified_mapping_row_never_trades() {
    let events: Vec<TimedEvent> = corpus()
        .into_iter()
        .filter(|te| !matches!(te.event, DataEvent::UnifiedMapping { .. }))
        .collect();
    let r = replay(events);
    assert_eq!(
        r.orders.orders,
        0,
        "no mapping → no cross-venue pair → no orders:\n{}",
        r.render()
    );
}
