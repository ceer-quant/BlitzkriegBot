//! PR0 (#840) — the deterministic dry fixture: the behavior golden for the
//! contract-freeze waves.
//!
//! PR1..PR3 migrate DTOs, readers and the strategy ABI between crates. The
//! proof that a migration changed NO behavior is this fixture: the SAME
//! synthetic two-venue corpus through the FULL live engine (the
//! `engine_on_data` choke point production data hits) must produce the
//! byte-identical report every run — pinned here by
//! `BacktestReport::trades_sha256` (#351's reproducibility digest), the order
//! ledger and the open-position identity.
//!
//! The corpus is the #427 acceptance shape (dual-venue round + unified
//! mapping row + four leg books) — the proven-filling corpus of
//! `cross_venue_replay_check.rs`, trimmed to its deterministic core. It runs
//! `Mode::Dry` always (the replay forces it), with fixed virtual clocks
//! (`NOW`-anchored) so nothing depends on wall time. If this file goes red
//! after a contract wave, the wave changed engine behavior, not just types.

use blitzkrieg_core::backtest::{BacktestConfig, Backtester, EventBacktester, VecSource};
use blitzkrieg_core::data_source::TimedEvent;
use blitzkrieg_core::engine::DataEvent;
use blitzkrieg_core::model::{CryptoMarket, Mode};
use blitzkrieg_core::service::CoreConfig;
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::path::PathBuf;

const NOW: i64 = 1_700_000_000_000;

const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

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

fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> DataEvent {
    DataEvent::Book {
        token_id: token.to_string(),
        bids: vec![(ask - dec!(0.01), depth)],
        asks: vec![(ask, depth)],
        now_ms: ts,
    }
}

/// Round discovery on both venues at T0, the unified mapping row, then four
/// leg books refreshed every second for 40 virtual seconds. The pair enters
/// on the first evaluation and stays quoted (UP@pm 0.44 + DOWN@kx 0.42 +
/// both fees ≈ 0.875 ≤ 0.98).
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
            event: leg_book(KX_DOWN, dec!(0.42), dec!(250), t),
        });
    }
    evs
}

fn replay(events: Vec<TimedEvent>) -> blitzkrieg_core::backtest::BacktestReport {
    let core = CoreConfig {
        mode: Mode::Dry,
        risk: blitzkrieg_core::risk::RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100000),
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
            max_positions_per_asset: 4,
            ..Default::default()
        },
        // `None` = load no execution_policy.toml: the struct literal above is
        // the whole truth, independent of any cwd config file.
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
    use blitzkrieg_core::account::{Account, AccountStatus};
    use blitzkrieg_market_api::types::MarketType;
    b.core_mut()
        .accounts_mut()
        .insert(Account {
            id: blitzkrieg_market_api::AccountId::from("kalshi"),
            name: "kalshi".into(),
            market_type: MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: Default::default(),
            venue: "kalshi".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: NOW,
        })
        .expect("the kalshi account mints (unique id)");
    b.core_mut()
        .accounts_mut()
        .get_mut(&blitzkrieg_market_api::AccountId::from("kalshi"))
        .expect("kalshi just inserted")
        .set_balance(dec!(100_000));
    b.core_mut()
        .arb_config_set(&blitzkrieg_core::account::CrossVenueArbConfig {
            leg_a_venue: "polymarket".into(),
            leg_a_account: "default".into(),
            leg_b_venue: "kalshi".into(),
            leg_b_account: "kalshi".into(),
            pairs: vec!["BTC".into()],
        })
        .expect("the leg configuration lands (memory-only core)");
    b.run().expect("replay runs")
}

/// The determinism half: two independent replays of the same corpus must
/// agree BYTE-FOR-BYTE on the serialized report (#351's contract), which also
/// makes the pinned constants in the next test meaningful.
#[test]
fn two_runs_produce_byte_identical_reports() {
    let a = replay(corpus());
    let b = replay(corpus());
    let ja = serde_json::to_string(&a).expect("report serializes");
    let jb = serde_json::to_string(&b).expect("report serializes");
    assert_eq!(ja, jb, "same corpus ⇒ byte-identical report JSON");
}

/// The freeze half: the fixture's outcome is pinned. `trades_sha256` is the
/// #351 digest over the closed-trade list; together with the order ledger and
/// the open-position identity it IS the engine's observable behavior on this
/// corpus. A contract wave that changes any of these changed behavior.
#[test]
fn fixture_outcome_is_frozen() {
    let r = replay(corpus());
    assert!(
        r.errors.is_empty(),
        "no replay errors expected: {:?}",
        r.errors
    );
    println!(
        "ACTUAL trades_sha256={} orders={:?} fills={} open_positions={} open_notional={} end_at_ms={} start_at_ms={}",
        r.trades_sha256,
        r.orders,
        r.fills,
        r.open_positions,
        r.open_notional_usd,
        r.end_at_ms,
        r.start_at_ms
    );
    // ── pinned digest ──
    assert_eq!(
        r.trades_sha256, PINNED_TRADES_SHA256,
        "trades_sha256 drifted — engine behavior changed"
    );
    // ── pinned order ledger ──
    assert_eq!(r.orders.orders, PINNED_ORDERS_TOTAL);
    assert_eq!(r.orders.filled, PINNED_ORDERS_FILLED);
    assert_eq!(r.orders.cancelled, PINNED_ORDERS_CANCELLED);
    assert_eq!(r.orders.rejected, PINNED_ORDERS_REJECTED);
    assert_eq!(r.fills, PINNED_FILLS);
    // ── pinned end state ──
    assert_eq!(r.open_positions, PINNED_OPEN_POSITIONS);
    assert_eq!(r.open_notional_usd, PINNED_OPEN_NOTIONAL_USD);
    assert_eq!(r.start_at_ms, NOW);
    assert_eq!(r.end_at_ms, PINNED_END_AT_MS);
    // The replay is ALWAYS dry and the window is purely virtual.
    assert!(r.forced_dry, "replay forces Mode::Dry");
}

// ── the pinned constants (derived from the d21e940 engine; re-derive ONLY by
//    rerunning with --nocapture and re-reviewing the diff) ────────────────────

/// SHA256 over the closed-trade list, close order (#351). The fixture's pair
/// is still OPEN at window end (the release corpus is out of scope here), so
/// the closed-trade list is empty and the digest is the empty-string SHA —
/// pinning THAT is the point: a migration that silently closes (or re-opens)
/// a position shows up here.
const PINNED_TRADES_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const PINNED_ORDERS_TOTAL: u64 = 4;
const PINNED_ORDERS_FILLED: u64 = 2;
const PINNED_ORDERS_CANCELLED: u64 = 0;
const PINNED_ORDERS_REJECTED: u64 = 2;
const PINNED_FILLS: u64 = 2;
const PINNED_OPEN_POSITIONS: usize = 2;
const PINNED_OPEN_NOTIONAL_USD: Decimal = dec!(215.0000);
const PINNED_END_AT_MS: i64 = 1_700_000_050_000;
