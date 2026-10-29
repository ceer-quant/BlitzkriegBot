//! pair_discount_arb × `entry_maker` — the REAL maker-pairing package through
//! the kernel's full entry pipeline, proving the three teeth of the v1.2
//! doctrine at the OME level:
//!
//!   1. DECLARATION → ROUTING — the manifest's `entry_maker` makes the host
//!      build BOTH pair legs as `FillPolicy::Maker` submitted on the OME's
//!      never-escalate clock (`maker_timeout_ms = -1`): post-only at each
//!      leg's OWN ask, the join-not-chase price.
//!   2. NEVER ESCALATES — dry's honest #464 mirror: a post-only quote priced
//!      at the standing ask WOULD cross at submission, so the venue refuses
//!      the POST (`WouldCross`). A Maker leg's refusal is final: no
//!      `{id}:escalated` taker is ever placed, no escalation deadline is
//!      armed (`next_escalation_ms` stays `i64::MAX`), the cash reservation
//!      is released, and the pair never opens. The leg "rests" only in a
//!      book that lets it; the round's own expiry sweep cleans the rest.
//!   3. DEFAULT UNCHANGED — the SAME books through the same package WITHOUT
//!      the declaration keep today's `MakerThenTaker` routing: the refused
//!      POST chases at the book's real price through the WouldCross reaction
//!      (`escalation_guard` + `place_escalated`) and the pair opens as two
//!      taker fills — byte-identically to the pre-v1.2 economics the
//!      cross_venue twins already pin.
//!
//! Fixture shape follows `strategy_dispatch_tests::core_with_cross_venue_arb`:
//! the real package dir, declarations stamped from the loaded manifest the
//! way `load_lua_package_inner` stamps them, the kernel's ONE fee schedule
//! injected.

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::{CryptoMarket, FillPolicy, OrderStatus};
use crate::risk::RiskConfig;
use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal_macros::dec;

const SLOT: i64 = 1_888_888;
/// 60 s into slot 1_888_888: `time_left_sec` 840 — inside the package's
/// default entry window (the first 0.5 of a 900 s round, t >= 450).
const NOW: i64 = SLOT * 900 * 1000 + 60_000;
const UP: &str = "pair-up";
const DOWN: &str = "pair-down";

fn package_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/pair_discount_arb")
}

/// The one-condition round the package prices: 0.48 + 0.45 + fees ≈ 0.945 <
/// 0.995 (`max_pair_cost`) → the package fires both legs, each at its OWN ask.
fn pair_market() -> Vec<CryptoMarket> {
    vec![CryptoMarket {
        asset: "BTC".into(),
        condition_id: "pair-cond".into(),
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

/// Core hosting the REAL pair_discount_arb, declarations stamped from its
/// manifest exactly as `load_lua_package_inner` stamps them. `entry_maker`
/// overrides the manifest's declaration: `None` hosts it as shipped (the
/// premise asserts the manifest declares it), `Some(false)` hosts the same
/// package as a pre-v1.2 deployment saw it (the A/B contrast arm).
fn core_with_pair(entry_maker: Option<bool>) -> Core {
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
        // The pair's legs are DECLARED at the thin leg's depth; the kernel
        // band must never clamp below it.
        min_shares: dec!(1),
        max_shares: dec!(10_000),
        // Both legs are live positions on ONE asset (same condition id): the
        // #363 per-asset count must cover a same-market pair.
        max_positions_per_asset: Some(4),
        // cwd-relative logs are the historical three-copies pollution:
        // a test writes nothing to the process cwd.
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
    let loaded = load_lua_package(&package_dir()).expect("pair_discount_arb package must load");
    assert!(
        loaded.entry_maker,
        "the fixture premise: the manifest declares entry_maker"
    );
    // The kernel's ONE fee schedule, the same stamp the production loader
    // applies (`bk.fees()` nil would refuse everything).
    let sched = crate::exit_policy::fee_schedule();
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        .declare_trailing_exit(loaded.trailing_exit)
        .declare_entry_maker(entry_maker.unwrap_or(loaded.entry_maker));
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", package_dir().display()),
    )
    .expect("pair_discount_arb registers");
    assert!(eng.set_strategy_enabled("pair_discount_arb", true));
    c.enable_engine(eng);
    c
}

/// The round + both leg books: ask 0.48 / 0.45 with depth 600 each (equal
/// legs), fed fresh against the 900 s grid.
fn feed_pair_books(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: pair_market(),
            now_ms: now,
        },
        now,
    );
    let mut book = |token: &str, ask: Decimal, depth: Decimal| {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(ask - dec!(0.01), depth)],
                asks: vec![(ask, depth)],
                now_ms: now,
            },
            now,
        );
    };
    book(UP, dec!(0.48), dec!(600));
    book(DOWN, dec!(0.45), dec!(600));
}

/// Teeth 1 + 2: the declared package's legs reach the OME as post-only Maker
/// orders on the never-escalate clock; the crossing POST is refused (#464)
/// and NOTHING chases — no escalated taker, no armed deadline, no position.
#[test]
fn entry_maker_legs_go_post_only_on_the_never_escalate_clock_and_never_chase() {
    let mut c = core_with_pair(None);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    let orders = c.list_orders();
    assert_eq!(orders.len(), 2, "exactly the two declared legs, no chase");
    for o in &orders {
        assert_eq!(
            o.mode,
            FillPolicy::Maker,
            "the declared package's leg is post-only Maker, got {:?}",
            o.mode
        );
        assert!(
            o.mode.post_only(),
            "Maker is the post-only face of the policy"
        );
        assert_eq!(
            o.maker_timeout_ms, -1,
            "the never-escalate clock the doctrine submits with"
        );
        assert!(
            o.escalate_at_ms.is_none(),
            "no escalation deadline is armed on a pure-maker leg"
        );
        assert!(
            !o.internal_key.ends_with(":escalated"),
            "no taker continuation exists for a refused maker leg"
        );
        assert_eq!(
            o.status,
            OrderStatus::Rejected,
            "#464: a post-only quote AT the standing ask would cross, so the \
             venue refuses the POST — the honest mirror, never a hidden taker \
             fill: {o:?}"
        );
    }
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(
        up.price,
        dec!(0.48),
        "each leg prices at its OWN ask (join)"
    );
    assert_eq!(down.price, dec!(0.45), "join, not chase");

    // And the OME agrees: no live order carries a deadline at all.
    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "the never-escalate clock never arms the sweep"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "a refused pair opens nothing — the taker-pair economics are gone"
    );

    // Drive the clock far past any MakerThenTaker window (5 s default): a
    // chasing deployment would have crossed by now. The maker posture keeps
    // holding: no sweep fires, no order appears, the round's own expiry is
    // the only end these legs know.
    let later = NOW + 30_000;
    c.tick(later).expect("the tick sweeps nothing");
    c.engine_evaluate(later);
    assert_eq!(
        c.list_orders().len(),
        2,
        "still exactly the two refused legs"
    );
    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "still nothing armed"
    );
    assert!(c.positions().open_positions().is_empty());
}

/// Tooth 3: the SAME books through the same package WITHOUT the declaration
/// keep the `MakerThenTaker` entry — the refused POST chases at the book's
/// real price and the pair opens as two taker fills (the pre-v1.2 routing
/// the replay baseline reproduces), on the configured 5 s window.
#[test]
fn without_the_declaration_the_maker_then_taker_chase_is_unchanged() {
    let mut c = core_with_pair(Some(false));
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    let orders = c.list_orders();
    let legs: Vec<_> = orders
        .iter()
        .filter(|o| !o.internal_key.ends_with(":escalated"))
        .collect();
    let chases: Vec<_> = orders
        .iter()
        .filter(|o| o.internal_key.ends_with(":escalated"))
        .collect();
    assert_eq!(legs.len(), 2, "the two declared legs");
    assert_eq!(chases.len(), 2, "each refused POST escalates a taker");
    for o in &legs {
        assert_eq!(o.mode, FillPolicy::MakerThenTaker, "the legacy routing");
        assert_eq!(
            o.maker_timeout_ms, 5000,
            "the configured entry_maker_timeout_ms, byte-identically"
        );
        assert_eq!(o.status, OrderStatus::Rejected, "#464 refuses the POST");
    }
    for o in &chases {
        assert_eq!(o.mode, FillPolicy::Taker, "the WouldCross reaction chases");
        assert_eq!(o.status, OrderStatus::Filled, "the chase crosses and fills");
    }
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "the pair opens exactly as the pre-v1.2 deployment's did"
    );
    let up = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == UP)
        .expect("the up leg held");
    let down = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == DOWN)
        .expect("the down leg held");
    assert_eq!(up.entry_price, dec!(0.48));
    assert_eq!(down.entry_price, dec!(0.45));
}
