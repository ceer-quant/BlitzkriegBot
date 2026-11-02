//! maker_pair — both post-only legs, or neither.
//!
//! The package quotes one tick under each ask, declares `entry_maker` and
//! `atomic_batch`, and sizes both legs to the same share count (default 1).
//! A book that cannot price both quotes, or whose quotes do not clear
//! `max_pair_cost`, emits nothing. There is no at-ask mode and no second
//! evaluate that adds a lone leg.

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::{CryptoMarket, FillPolicy, OrderStatus};
use crate::risk::RiskConfig;
use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal_macros::dec;

const SLOT: i64 = 1_888_888;
const NOW: i64 = SLOT * 900 * 1000 + 60_000;
const UP: &str = "maker-up";
const DOWN: &str = "maker-down";

fn package_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/maker_pair")
}

fn market() -> Vec<CryptoMarket> {
    vec![CryptoMarket {
        asset: "BTC".into(),
        condition_id: "maker-cond".into(),
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
    let loaded = load_lua_package(&package_dir()).expect("maker_pair package must load");
    assert!(loaded.entry_maker, "maker_pair declares entry_maker");
    assert!(loaded.atomic_batch, "maker_pair declares atomic_batch");
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
    .expect("maker_pair registers");
    assert!(eng.set_strategy_enabled("maker_pair", true));
    c.enable_engine(eng);
    c
}

fn feed(c: &mut Core, now: i64, up: Option<Decimal>, down: Option<Decimal>) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: market(),
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

#[test]
fn both_legs_rest_under_the_ask_at_one_share_each() {
    let mut c = core();
    feed(&mut c, NOW + 1_000, Some(dec!(0.48)), Some(dec!(0.45)));
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs or neither");
    let orders = c.list_orders();
    assert_eq!(orders.len(), 2);
    for o in &orders {
        assert_eq!(o.mode, FillPolicy::Maker);
        assert_eq!(o.maker_timeout_ms, -1);
        assert!(o.escalate_at_ms.is_none());
        assert_eq!(o.status, OrderStatus::Live);
        assert_eq!(o.size, dec!(1), "default max_pair_shares is 1");
    }
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(up.price, dec!(0.47));
    assert_eq!(down.price, dec!(0.44));
    assert_eq!(up.size, down.size);
}

#[test]
fn a_one_sided_book_emits_nothing() {
    let mut c = core();
    feed(&mut c, NOW + 1_000, Some(dec!(0.48)), None);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 0);
    assert!(c.list_orders().is_empty());
}

#[test]
fn quotes_that_do_not_clear_max_pair_cost_emit_nothing() {
    let mut c = core();
    // 0.54 + 0.49 = 1.03, above the 0.985 quote cap.
    feed(&mut c, NOW + 1_000, Some(dec!(0.55)), Some(dec!(0.50)));
    assert_eq!(c.engine_evaluate(NOW + 2_000), 0);
    assert!(c.list_orders().is_empty());
}
