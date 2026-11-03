//! pair_discount_arb × `entry_maker` — the REAL maker-pairing package through
//! the kernel's full entry pipeline, pinning the doctrine's teeth at the OME
//! level under the v1.3 quote policy:
//!
//!   1. DECLARATION → ROUTING — the manifest's `entry_maker` makes the host
//!      build BOTH pair legs as `FillPolicy::Maker` submitted on the OME's
//!      never-escalate clock (`maker_timeout_ms = -1`). v1.3 quotes strictly
//!      UNDER the leg's own ask (`entry_quote_mode` 1 = ask − 1 tick), so a
//!      post-only BUY can actually REST — v1.2's at-ask quote was the ONE
//!      price a post-only buy can never rest at (#464 `WouldCross`), which
//!      is why it filled 0/N.
//!   2. THE #464 HONEST MIRROR (declared mode-0 baseline) — with
//!      `entry_quote_mode = 0` (v1.2's at-ask quote, kept as a replay arm)
//!      the POST is refused (`WouldCross`) and nothing chases: no
//!      `{id}:escalated` taker is ever placed, no escalation deadline is
//!      armed, the cash reservation is released, and the pair never opens.
//!      A Maker leg's refusal is final; the round's own expiry sweep cleans
//!      the rest.
//!   3. NEVER ESCALATES — a Maker leg never arms the sweep
//!      (`next_escalation_ms` stays `i64::MAX`), on either quote arm.
//!   4. DEFAULT UNCHANGED — the SAME books through the same package WITHOUT
//!      the declaration keep today's `MakerThenTaker` routing: the
//!      under-ask POST now RESTS (no WouldCross, no immediate chase), the
//!      configured 5 s clock fires, the sweep cancels the resting maker and
//!      the chase crosses at the book's real price — the pair opens as two
//!      taker fills, byte-identically to the pre-v1.2 economics the
//!      cross_venue twins already pin.
//!
//! Fixture shape follows `strategy_dispatch_tests::core_with_cross_venue_arb`:
//! the real package dir, declarations stamped from the loaded manifest the
//! way `load_lua_package_inner` stamps them, the kernel's ONE fee schedule
//! injected.

use super::*;
use crate::engine::{DataEvent, Engine};
use crate::model::{CryptoMarket, FillPolicy, OrderRequest, OrderStatus, Side};
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
/// 0.995 (`max_pair_cost`) → the package fires both legs, each quoted by the
/// v1.3 policy off its OWN ask.
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
/// `overrides` patches the tunables bag before the adapter is built — the
/// `entry_quote_mode` replay arms (mode 0 = the v1.2 at-ask baseline).
fn core_with_pair_overrides(entry_maker: Option<bool>, overrides: &[(&str, &str)]) -> Core {
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
    let mut tunables = loaded.tunables;
    for (name, value) in overrides {
        tunables.insert((*name).to_string(), (*value).to_string());
    }
    let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
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

fn core_with_pair(entry_maker: Option<bool>) -> Core {
    core_with_pair_overrides(entry_maker, &[])
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

/// One leg's fresh book at `now` (the same shape `feed_pair_books` builds):
/// a 0.01-wide spread with `depth` on each side, ask at `ask`. The ingest
/// path settles resting makers itself, so a crossing feed fills on arrival.
fn mirror_book(c: &mut Core, token: &str, ask: Decimal, now: i64) {
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

/// Teeth 1 + 3: the declared package's legs reach the OME as post-only Maker
/// orders on the never-escalate clock, quoted strictly UNDER the leg's own
/// ask (v1.3 `entry_quote_mode` 1) — so they REST, which is the fill path
/// v1.2 never had, and no deadline is ever armed, no chase ever exists, no
/// position opens while the book stands still.
#[test]
fn declared_legs_quote_under_the_ask_and_rest_on_the_never_escalate_clock() {
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
            "no taker continuation exists for a maker leg"
        );
        assert_eq!(
            o.status,
            OrderStatus::Live,
            "v1.3: a post-only quote strictly UNDER the standing ask rests — \
             the rest IS the fill path v1.2 never had: {o:?}"
        );
    }
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(
        up.price,
        dec!(0.47),
        "each leg prices one tick UNDER its OWN ask (join under, the 0.01 grid)"
    );
    assert_eq!(down.price, dec!(0.44), "join under, never chase");

    // And the OME agrees: no live order carries a deadline at all.
    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "the never-escalate clock never arms the sweep"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "a resting pair opens nothing until the book crosses it"
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
        "still exactly the two resting legs"
    );
    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "still nothing armed"
    );
    assert!(c.positions().open_positions().is_empty());
}

/// Tooth 2: the declared mode-0 baseline (v1.2's at-ask quote, kept as a
/// replay arm) still hits the #464 honest mirror — the POST is refused,
/// the refusal is final for a Maker leg, and nothing hidden chases.
#[test]
fn the_at_ask_baseline_still_refuses_the_post_and_never_chases() {
    let mut c = core_with_pair_overrides(None, &[("entry_quote_mode", "0")]);
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
        assert_eq!(
            o.maker_timeout_ms, -1,
            "the never-escalate clock the doctrine submits with"
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
        "the v1.2 baseline prices AT its own ask — the unpostable price"
    );
    assert_eq!(down.price, dec!(0.45), "join, not chase (the v1.2 form)");

    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "the never-escalate clock never arms the sweep, even on the refused arm"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "a refused pair opens nothing — the taker-pair economics are gone"
    );
}

/// Tooth 4: the SAME books through the same package WITHOUT the declaration
/// keep the `MakerThenTaker` entry — the under-ask POST now RESTS (no
/// WouldCross, no immediate chase), the configured 5 s clock fires, the
/// sweep cancels the resting maker and the chase crosses at the book's real
/// price: the pair opens as two taker fills, byte-identically to the
/// pre-v1.2 economics the replay baseline reproduces.
#[test]
fn without_the_declaration_the_maker_then_taker_rests_then_chases_after_its_window() {
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
    assert_eq!(
        chases.len(),
        0,
        "the under-ask POST does not cross at submission — no immediate chase"
    );
    for o in &legs {
        assert_eq!(o.mode, FillPolicy::MakerThenTaker, "the legacy routing");
        assert_eq!(
            o.maker_timeout_ms, 5000,
            "the configured entry_maker_timeout_ms, byte-identically"
        );
        assert_eq!(
            o.status,
            OrderStatus::Live,
            "v1.3: the under-ask POST rests instead of being refused"
        );
    }
    assert_ne!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "MakerThenTaker arms the 5 s clock the maker posture never does"
    );

    // Drive past the maker window (submitted at NOW+2_000, due NOW+7_000):
    // the sweep cancels the resting makers and chases at the book's price.
    // No engine_evaluate after the tick — the tick is sweeps only, the same
    // maintenance pass the host's loop runs beside its evaluate cycle.
    c.tick(NOW + 8_000)
        .expect("the tick runs the escalation sweep");

    let orders = c.list_orders();
    let legs: Vec<_> = orders
        .iter()
        .filter(|o| !o.internal_key.ends_with(":escalated"))
        .collect();
    let chases: Vec<_> = orders
        .iter()
        .filter(|o| o.internal_key.ends_with(":escalated"))
        .collect();
    assert_eq!(legs.len(), 2, "the two maker legs, escalated away");
    assert_eq!(chases.len(), 2, "each expired maker escalates a taker");
    for o in &legs {
        assert_eq!(
            o.status,
            OrderStatus::Cancelled,
            "the sweep cancels the resting maker to make room for the chase"
        );
    }
    for o in &chases {
        assert_eq!(o.mode, FillPolicy::Taker, "the clock's chase is a taker");
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

// ── issue #848 · the naked-leg tail's gap-fill retry ────────────────────────

/// The UP leg has filled; the DOWN leg's order still rests (NOT terminal) and
/// the naked tail's book has moved, so the gap-fill loop's requote carries the
/// SAME internal key (`strategy:asset:direction:slot` — kernel-derived, the
/// strategy layer cannot rekey) at a fresh price. Today the retry is refused
/// twice: the engine's per-round repeat suppression drops the re-signal, and
/// the OME's internal-key dedup refuses the resubmit. The fix: the naked
/// token (the side the strategy does NOT hold — the open position is the
/// ground truth of the first leg's fill) is released for re-signalling
/// every cycle, and the place path RETIRES the resting leg order through
/// the standard cancel and lets the requote replace it — at most ONE live
/// order under the key, the dedup's purpose intact, the leg still pure
/// maker on the never-escalate clock (#742), still gated by the post-only
/// mirror (#464).
#[test]
fn the_naked_legs_gap_fill_requote_replaces_the_resting_order() {
    let mut c = core_with_pair(None);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    // The UP leg fills the honest maker way: the ask descends ONTO the
    // resting 0.47 quote (the almach path), a later tick than the POST.
    mirror_book(&mut c, UP, dec!(0.47), NOW + 3_000);
    let orders = c.list_orders();
    let up = orders.iter().find(|o| o.token_id == UP).unwrap();
    let down = orders.iter().find(|o| o.token_id == DOWN).unwrap();
    assert_eq!(up.status, OrderStatus::Filled, "the first leg filled");
    assert_eq!(
        down.status,
        OrderStatus::Live,
        "the counterpart leg still rests — the not-terminal state the dedup used to hide behind"
    );
    assert_eq!(down.price, dec!(0.44));
    let held = c.positions().open_positions();
    assert_eq!(held.len(), 1, "only the filled UP leg is held");
    assert_eq!(held[0].token_id, UP);
    assert_eq!(held[0].direction, crate::signal::SignalDirection::Up);

    // The DOWN book ticks one level up: the fresh restable quote (0.45)
    // differs from the resting 0.44 — the naked-tail requote shape. The
    // gap-fill loop re-emits the shortfall leg every cycle; the released
    // token lets it reach the place path this time.
    mirror_book(&mut c, UP, dec!(0.47), NOW + 4_000);
    mirror_book(&mut c, DOWN, dec!(0.46), NOW + 4_000);
    let placed = c.engine_evaluate(NOW + 4_000);
    assert_eq!(placed, 1, "the gap-fill requote places (was: refused)");

    let orders = c.list_orders();
    let mut down_orders: Vec<_> = orders.iter().filter(|o| o.token_id == DOWN).collect();
    down_orders.sort_by_key(|o| o.submitted_at_ms);
    assert_eq!(
        down_orders.len(),
        2,
        "the requote replaced the resting order: old retired, new resting"
    );
    assert_eq!(down_orders[0].price, dec!(0.44));
    assert_eq!(
        down_orders[0].status,
        OrderStatus::Cancelled,
        "the resting order was retired through the standard cancel"
    );
    assert_eq!(down_orders[1].price, dec!(0.45), "the fresh-book quote");
    assert_eq!(down_orders[1].status, OrderStatus::Live);
    assert_eq!(
        down_orders[0].internal_key, down_orders[1].internal_key,
        "same internal key — the replace, not a rekey; fills keep addressing the leg"
    );
    assert_eq!(
        down_orders[1].mode,
        FillPolicy::Maker,
        "#742: the requote stays a pure-maker leg — the exemption never upgrades"
    );
    assert_eq!(
        down_orders[1].maker_timeout_ms, -1,
        "the never-escalate clock the doctrine submits with"
    );
    assert_eq!(
        c.ome().next_escalation_ms(),
        i64::MAX,
        "no escalation deadline anywhere — the requote is not a taker path"
    );
    // The dedup's purpose holds: exactly ONE live order under the key.
    let live_downs = orders
        .iter()
        .filter(|o| o.token_id == DOWN && o.status.is_live())
        .count();
    assert_eq!(
        live_downs, 1,
        "never two live orders under one internal key"
    );
}

/// The exemption's boundary (issue #848 test ②): a same-key resubmit with NO
/// filled sibling — a plain duplicate entry, or a fresh-pair re-emit while
/// both legs rest — is still refused by the OME's internal-key dedup
/// byte-identically, and the resting order is untouched. The requote path
/// must not leak into a global dedup bypass.
#[test]
fn a_same_key_resubmit_without_a_filled_sibling_still_refuses() {
    let mut c = core_with_pair(None);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2, "both legs place");

    let resting = c
        .list_orders()
        .into_iter()
        .find(|o| o.token_id == UP)
        .expect("the UP leg rests");
    let dup = OrderRequest {
        token_id: resting.token_id.clone(),
        condition_id: resting.condition_id.clone(),
        side: Side::Buy,
        mode: resting.mode,
        price: resting.price,
        size: resting.size,
        internal_key: resting.internal_key.clone(),
        strategy: resting.strategy.clone(),
        asset: resting.asset.clone(),
        direction: resting.direction.clone(),
        round_slot: resting.round_slot,
        account_id: resting.account_id.clone(),
    };
    let err = c
        .place(dup, -1, NOW + 3_000)
        .expect_err("a duplicate key with no filled sibling still refuses");
    assert!(
        err.message.contains("duplicate internal key"),
        "the dedup refusal, byte-identically: {err:?}"
    );
    let orders = c.list_orders();
    assert_eq!(orders.len(), 2, "nothing was cancelled, nothing was placed");
    assert!(orders.iter().all(|o| o.status == OrderStatus::Live));
}

/// And the requote predicate is direction-bound (issue #848 boundary):
/// once BOTH legs have filled, the pair is complete — a same-direction
/// duplicate on the filled leg's key is stacking, not completion. The
/// fixture's per-asset cap is deliberately 4 (#363: both legs of a
/// same-market pair are one asset), so the cap does not refuse and the
/// FIRST duplicate legitimately rests — that is the existing gates'
/// business, unchanged here. The boundary this pins: the SECOND identical
/// duplicate still meets the OME's internal-key dedup — NOT the
/// completion no-op, and the resting order is not retired. Without the
/// predicate's missing-leg half, the opposite-direction sibling position
/// alone would make the replace path answer the duplicate Ok and ride
/// over a complete pair.
#[test]
fn a_same_direction_duplicate_after_the_fill_is_not_a_requote() {
    let mut c = core_with_pair(None);
    feed_pair_books(&mut c, NOW + 1_000);
    assert_eq!(c.engine_evaluate(NOW + 2_000), 2);
    mirror_book(&mut c, UP, dec!(0.47), NOW + 3_000);
    mirror_book(&mut c, DOWN, dec!(0.44), NOW + 3_100);
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "both legs filled — the pair is complete"
    );

    let filled = c
        .list_orders()
        .into_iter()
        .find(|o| o.token_id == UP)
        .expect("the filled UP leg");
    let dup = OrderRequest {
        token_id: filled.token_id.clone(),
        condition_id: filled.condition_id.clone(),
        side: Side::Buy,
        mode: filled.mode,
        // Under the 0.47 ask: the post-only mirror lets it rest, so the
        // boundary is probed against a LIVE same-key order, the state the
        // replace path acts on.
        price: dec!(0.46),
        size: filled.size,
        internal_key: filled.internal_key.clone(),
        strategy: filled.strategy.clone(),
        asset: filled.asset.clone(),
        direction: filled.direction.clone(),
        round_slot: filled.round_slot,
        account_id: filled.account_id.clone(),
    };
    let (stack_id, status) = c
        .place(dup.clone(), -1, NOW + 4_000)
        .expect("the fixture's cap admits the stack — the existing gates' business");
    assert_eq!(status, OrderStatus::Live, "the duplicate rests");

    let err = c
        .place(dup, -1, NOW + 4_100)
        .expect_err("a same-direction duplicate on a COMPLETE pair is not a requote");
    assert!(
        err.message.contains("duplicate internal key"),
        "the dedup refusal, byte-identically — not the completion no-op: {err:?}"
    );
    assert_eq!(
        c.list_orders()
            .iter()
            .find(|o| o.order_id == stack_id)
            .expect("the resting stack order")
            .status,
        OrderStatus::Live,
        "the resting stack order was not retired"
    );
}
