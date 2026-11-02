//! postonly_pair (issue #872): both legs rest under the ask, or neither does.
//! Requotes wait out the 15s cadence and cancel through the book path. A
//! one-share unpaired inventory is not added to. The last 20s of a round
//! cancels instead of quoting.

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::{CryptoMarket, FillPolicy, OrderRole, OrderStatus, SignalDirection};
use crate::position::OpenParams;
use crate::risk::RiskConfig;
use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal_macros::dec;

const SLOT: i64 = 1_888_888;
const NOW: i64 = SLOT * 900 * 1000 + 60_000;
const UP: &str = "post-up";
const DOWN: &str = "post-down";

fn package_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/postonly_pair")
}

fn market(expires_at_ms: i64) -> Vec<CryptoMarket> {
    vec![CryptoMarket {
        asset: "BTC".into(),
        condition_id: "post-cond".into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms,
        round_slot: SLOT,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }]
}

fn core() -> Core {
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
    let loaded = load_lua_package(&package_dir()).expect("postonly_pair must load");
    assert!(loaded.entry_maker && loaded.atomic_batch);
    let sched = crate::exit_policy::fee_schedule();
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(true)
        .declare_entry_maker(true);
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", package_dir().display()),
    )
    .expect("registers");
    assert!(eng.set_strategy_enabled("postonly_pair", true));
    c.enable_engine(eng);
    c
}

fn feed(c: &mut Core, now: i64, up: Option<Decimal>, down: Option<Decimal>) {
    let expires = (SLOT + 1) * 900 * 1000;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: market(expires),
            now_ms: now,
        },
        now,
    );
    let mut book = |token: &str, ask: Decimal| {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(ask - dec!(0.02), dec!(10))],
                asks: vec![(ask, dec!(10))],
                now_ms: now,
            },
            now,
        );
    };
    if let Some(ask) = up {
        book(UP, ask);
    }
    if let Some(ask) = down {
        book(DOWN, ask);
    }
}

fn live_buys(c: &Core) -> Vec<crate::model::TrackedOrder> {
    c.list_orders()
        .into_iter()
        .filter(|o| o.side == crate::model::Side::Buy && o.status == OrderStatus::Live)
        .collect()
}

#[test]
fn both_legs_rest_one_tick_under_the_ask() {
    let mut c = core();
    feed(&mut c, NOW, Some(dec!(0.48)), Some(dec!(0.45)));
    assert_eq!(c.engine_evaluate(NOW + 1_000), 2);
    let orders = live_buys(&c);
    assert_eq!(orders.len(), 2);
    for o in &orders {
        assert_eq!(o.mode, FillPolicy::Maker);
        assert_eq!(o.maker_timeout_ms, -1);
        assert_eq!(o.size, dec!(1));
    }
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(up.price, dec!(0.47));
    assert_eq!(down.price, dec!(0.44));
    let key = strategy_acc_key("postonly_pair", c.accounts.active_id().as_str());
    assert_eq!(
        c.strategy_accounting.get(&key).map(|a| a.maker_quotes),
        Some(2)
    );
}

#[test]
fn one_sided_or_expensive_books_emit_nothing() {
    let mut c = core();
    feed(&mut c, NOW, Some(dec!(0.48)), None);
    assert_eq!(c.engine_evaluate(NOW + 1_000), 0);
    let mut c = core();
    feed(&mut c, NOW, Some(dec!(0.55)), Some(dec!(0.50)));
    assert_eq!(c.engine_evaluate(NOW + 1_000), 0);
}

#[test]
fn requote_cancels_after_the_cadence() {
    let mut c = core();
    feed(&mut c, NOW, Some(dec!(0.48)), Some(dec!(0.45)));
    assert_eq!(c.engine_evaluate(NOW + 1_000), 2);
    c.engine_evaluate(NOW + 6_000);
    assert_eq!(live_buys(&c).len(), 2, "inside 15s the resting bids stay");
    c.engine_evaluate(NOW + 20_000);
    feed(&mut c, NOW + 21_000, Some(dec!(0.48)), Some(dec!(0.45)));
    assert!(
        live_buys(&c).is_empty(),
        "the book path cancels the bids the strategy broke"
    );
    assert_eq!(c.engine_evaluate(NOW + 22_000), 2, "a fresh pair can rest");
}

#[test]
fn near_expiry_cancels_instead_of_quoting() {
    let mut c = core();
    let late = (SLOT + 1) * 900 * 1000 - 10_000;
    feed(&mut c, late, Some(dec!(0.48)), Some(dec!(0.45)));
    assert_eq!(c.engine_evaluate(late + 100), 0);
    assert!(live_buys(&c).is_empty());
}

#[test]
fn unpaired_inventory_is_not_grown() {
    let mut c = core();
    let expires = (SLOT + 1) * 900 * 1000;
    let id = c
        .positions
        .open(
            OpenParams {
                strategy: "postonly_pair".into(),
                asset: "BTC".into(),
                direction: SignalDirection::Up,
                token_id: UP.into(),
                condition_id: "post-cond".into(),
                account_id: crate::model::default_account_id(),
                venue: String::new(),
                partner_token_id: DOWN.into(),
                entry_price: dec!(0.47),
                expires_at_ms: expires,
                was_maker: true,
                target_exit_price: None,
                round_duration_sec: 900,
                round_slot: SLOT,
            },
            NOW,
        )
        .id;
    c.positions
        .apply_entry_fill(&id, dec!(1), dec!(0.47), dec!(0), OrderRole::Maker);
    feed(&mut c, NOW, Some(dec!(0.48)), Some(dec!(0.45)));
    c.engine_evaluate(NOW + 1_000);
    let buys = live_buys(&c);
    assert!(
        buys.iter().all(|o| o.token_id != UP),
        "the filled side is not quoted again"
    );
    assert!(buys.len() <= 1, "at most the completing leg");
}
