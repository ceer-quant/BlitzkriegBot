#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use crate::service::{Core, CoreConfig};
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
// Candidate dispatch, settlement cooperation and the fast path:
// all_hold precondition, per-strategy position views, pending-token
// release, builtin dip/trend/chase legs and freshness budgets.

fn core() -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(5),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        ..Default::default()
    });
    // Keep the exit engine out of the way for signal tests.
    let cc = c.config_mut();
    cc.auto_exits_enabled = false;
    c
}

/// The fast path's all_hold precondition counts only ENABLED strategies:
/// a disabled non-holder contributes nothing during a replay (no
/// evaluation, no orders, no positions — its hook output is dropped), so
/// it must not veto the maintenance-gating skip. A registry of nine
/// libraries with one enabled holder is exactly the deployment shape the
/// fast path exists for.
#[test]
fn all_strategies_hold_to_settlement_ignores_disabled_strategies() {
    let mut e = Engine::new(cfg());
    crate::strategies::test_support::host_disabled(&mut e, cfg().trend, cfg().spread_arb);
    // Registered but every one disabled: no replay position can exist —
    // the enabled-holder set is empty, so the precondition holds vacuously.
    assert!(e.all_strategies_hold_to_settlement());
    // Enabling the (non-holder) dip buyer vetoes the fast path.
    assert!(e.set_strategy_enabled("spread_arb", true));
    assert!(!e.all_strategies_hold_to_settlement());
}

/// The host-pushed position view is sliced PER STRATEGY inside `evaluate`:
/// each strategy's `StrategyCtx::held_positions` carries only rows whose
/// `strategy` names it — a package can never read another package's book
/// through the view (the `bk.holdings()` isolation contract).
#[test]
fn evaluate_slices_the_position_view_by_strategy_name() {
    struct HoldingsProbe {
        name: &'static str,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl EngineStrategy for HoldingsProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
            let tokens: Vec<String> = ctx
                .held_positions()
                .iter()
                .map(|p| p.token_id.clone())
                .collect();
            self.seen.lock().unwrap().push(tokens.join(","));
            Vec::new()
        }
    }

    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(HoldingsProbe {
            name: "probe_a",
            seen: Arc::clone(&seen_a),
        }),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(
        Box::new(HoldingsProbe {
            name: "probe_b",
            seen: Arc::clone(&seen_b),
        }),
        "test".into(),
    )
    .unwrap();
    e.set_strategy_enabled("probe_a", true);
    e.set_strategy_enabled("probe_b", true);
    e.set_positions_view(vec![
        HeldPosition {
            strategy: "probe_a".into(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            condition_id: "cond".into(),
            token_id: "tok_a".into(),
            direction: "up".into(),
            shares: "1.00".into(),
            entry_price: "0.42".into(),
            round_slot: 1,
        },
        HeldPosition {
            strategy: "probe_b".into(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            condition_id: "cond".into(),
            token_id: "tok_b".into(),
            direction: "down".into(),
            shares: "2.00".into(),
            entry_price: "0.44".into(),
            round_slot: 1,
        },
    ]);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: 1_000,
    });
    e.evaluate(1_000);
    assert_eq!(
        *seen_a.lock().unwrap(),
        vec!["tok_a".to_string()],
        "probe_a sees only its own position"
    );
    assert_eq!(
        *seen_b.lock().unwrap(),
        vec!["tok_b".to_string()],
        "probe_b sees only its own position"
    );
}

/// `release_pending_token` re-opens a token the kernel had suppressed:
/// a partially-filled leg whose entry order ended may signal again THIS
/// round (the gap-fill loop). Verified through the public surface: after
/// `note_order_placed`, a standing candidate on the token does not count
/// as standing; after the release, it does.
#[test]
fn release_pending_token_reopens_the_gate_after_a_terminal_order() {
    let mut e = Engine::new(cfg());
    e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_gap");
    e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_resting");
    // Both tokens suppressed: a standing set that references them is
    // entirely suppressed → `has_standing_candidates` is false.
    e.last_candidates = vec![test_signal("tok_gap"), test_signal("tok_resting")];
    assert!(
        !e.has_standing_candidates(),
        "every candidate's token has a live entry order"
    );
    // The terminal-order release: only the named token re-opens.
    e.release_pending_token("tok_gap");
    assert!(
        e.has_standing_candidates(),
        "the released token's candidate is standing again"
    );
}

/// The service-facing release is scoped to the STRATEGY that placed the
/// order: a hold-to-settlement package gets its token re-opened (the
/// gap-fill loop), a mid-round-exiting strategy does not (the historic
/// one-entry-per-token-per-round discipline — the exit-economics windows
/// degraded the moment spread_arb could re-enter after a terminal order).
#[test]
fn the_scoped_release_opens_holder_tokens_but_not_plain_strategies() {
    struct PlainHolder {
        name: String,
        holds: bool,
    }
    impl EngineStrategy for PlainHolder {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _: &str, _: &crate::model::OrderbookSnapshot, _: i64) {}
        fn on_round(&mut self, _: i64, _: i64, _: i64) {}
        fn gate_exemptions(&self) -> GateExemptions {
            GateExemptions::none()
        }
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
        fn holds_to_settlement(&self) -> bool {
            self.holds
        }
    }
    let mut e = Engine::new(cfg());
    e.register_user_strategy(
        Box::new(PlainHolder {
            name: "plain".into(),
            holds: false,
        }),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(
        Box::new(PlainHolder {
            name: "pair_like".into(),
            holds: true,
        }),
        "test".into(),
    )
    .unwrap();
    e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_plain");
    e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_pair");
    assert!(
        !e.release_pending_token_for_strategy(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID,
            "plain",
            "tok_plain"
        ),
        "no release for a non-holder: the suppression stands"
    );
    assert!(
        e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "tok_plain".into()
        )),
        "the plain strategy's token stays latched"
    );
    assert!(
        e.release_pending_token_for_strategy(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID,
            "pair_like",
            "tok_pair"
        ),
        "a hold-to-settlement package's token is released"
    );
    assert!(
        !e.pending_tokens.contains(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "tok_pair".into()
        )),
        "the holder's token re-opens"
    );
}

#[test]
fn engine_places_a_trend_confirmed_dip_buy() {
    let mut e = engine();
    let mut c = core();
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });

    // Confirm the UP trend: several books with mid >= 0.5 within the window.
    for i in 0..12 {
        let t = now + i * 1000;
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: t,
        });
    }
    // Dip: mid 0.44 (bid 0.43) → entry 0.43 ≤ 0.45 → signal.
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
    // Spot calm.
    e.on_data(DataEvent::Spot {
        asset: "BTC".into(),
        price: dec!(60000),
        now_ms: now + 12_000,
    });

    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 1, "expected one spread_arb entry");
    // Placing through the core succeeds (funds seeded) and fills on the book.
    let req = orders.into_iter().next().unwrap();
    let (id, _) = c.place(req, 5000, now + 12_000).unwrap();
    assert!(!id.is_empty());
}

#[test]
fn engine_skips_without_confirmed_trend() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // Only one dip book; no confirm window yet.
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now,
    });
    assert!(e.evaluate(now).is_empty());
}

#[test]
fn engine_blocks_entry_when_spot_moves_against() {
    let mut e = engine();
    let _c = core();
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    for i in 0..12 {
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
    // Spot falls hard over the momentum window → UP entry blocked.
    e.on_data(DataEvent::Spot {
        asset: "BTC".into(),
        price: dec!(60000),
        now_ms: now + 1000,
    });
    e.on_data(DataEvent::Spot {
        asset: "BTC".into(),
        price: dec!(59000),
        now_ms: now + 12_000,
    });
    assert!(
        e.evaluate(now + 12_000).is_empty(),
        "spot moving against should block the entry"
    );
}

#[test]
fn trend_break_is_reported_for_bid_cancellation() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    for i in 0..12 {
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    let broken = e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.30), dec!(100))],
        asks: vec![(dec!(0.32), dec!(100))],
        now_ms: now + 12_000,
    });
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].0, "up");
}

#[test]
fn records_near_miss_when_timing_gate_blocks_a_valid_dip() {
    // A valid trend-confirmed dip that the timing gate rejects must be
    // counted (this is the near-miss telemetry the shadow log lacks).
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000; // force the timing gate to fail
    let mut e = engine_from(cfg);
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    for i in 0..12 {
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });

    let orders = e.evaluate(now + 12_000);
    assert!(orders.is_empty(), "timing gate must block the order");
    assert_eq!(
        e.last_blocked().len(),
        1,
        "the blocked dip should be recorded"
    );
    assert_eq!(e.last_blocked()[0].reason, BlockReason::Timing);
    e.tally_blocked();
    assert_eq!(e.blocked_timing_count(), 1);
    assert_eq!(e.blocked_momentum_count(), 0);
}

#[test]
fn a_fresh_engine_registers_nothing_and_only_the_host_can_add_strategies() {
    // PR-B hard switch: the kernel ships ZERO strategies, so a bare
    // `Engine::new` must come up empty. This is the assertion that keeps a
    // privileged in-tree code path from creeping back in.
    let bare = Engine::new(cfg());
    assert!(
        bare.supported_strategies().is_empty(),
        "the kernel must not register any strategy: {:?}",
        bare.supported_strategies()
    );
    assert!(bare.enabled_strategies().is_empty());
    assert_eq!(bare.strategy_source("spread_arb"), None);

    // The test host then registers three adapters in a defined order: it is
    // the tie-break when two strategies want the same token in the same
    // cycle (one entry per token per cycle), and `spread_arb` is first.
    let mut e = engine();
    assert_eq!(
        e.supported_strategies(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string()
        ]
    );
    assert_eq!(
        e.enabled_strategies(),
        vec!["spread_arb".to_string()],
        "the host enables the first adapter explicitly; the rest stay off"
    );
    assert_eq!(e.strategy_source("trend_follow"), Some("test"));
    assert!(
        !e.set_strategy_enabled("nope", true),
        "unknown name must not toggle"
    );
    assert!(e.set_strategy_enabled("trend_follow", true));
    assert_eq!(
        e.enabled_strategies(),
        vec!["spread_arb".to_string(), "trend_follow".to_string()]
    );
}

#[test]
fn the_chase_leg_declares_no_gate_exemption_but_is_evolvable() {
    // E4-a / #30: entering WITH the move is exactly what the shared spot
    // momentum gate wants, so the chase leg opts out of nothing. (E4-b, the
    // counter-trend leg, is the one that needs `momentum: true`.)
    let e = engine();
    let ex = e
        .strategy_gate_exemptions("trend_follow")
        .expect("registered");
    assert_eq!(ex, GateExemptions::none());
    assert!(!ex.any());
    // The only declaration under the default registry is the E4-b fade leg.
    let declared = e.declared_gate_exemptions();
    assert_eq!(declared.len(), 1, "{declared:?}");
    assert_eq!(declared[0].0, "mean_reversion");
    // It IS evolvable, with a coherent declaration and a twin that builds —
    // a declaration without a factory would be inert.
    let s = e
        .strategy_refs()
        .into_iter()
        .find(|s| s.name() == "trend_follow")
        .expect("registered");
    let knobs = s.evolvable_knobs();
    assert_eq!(knobs.len(), 6);
    assert!(knobs.iter().all(|k| k.is_coherent()));
    assert!(s.shadow_factory().is_some());
}

/// A rising 1-cent-tick book: `bid` climbs `steps` cents from 0.50, the offer
/// sits one tick above. Prices live on the real cent grid (mid `x.xx5`), which
/// matters: a half-cent-offset book would let `round2` collapse the ask onto
/// the mid and the chase leg refuses to "lift" an offer that is not above mid.
fn rising_books(e: &mut Engine, token: &str, steps: i64, start_ms: i64) {
    for i in 0..=steps {
        let bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(bid, dec!(100))],
            asks: vec![(bid + dec!(0.01), dec!(100))],
            now_ms: start_ms + i * 1000,
        });
    }
}

#[test]
fn the_chase_leg_trades_a_breakout_the_dip_buyer_would_not() {
    let mut e = engine();
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // A RISING up-token: bid 0.50 → 0.62 (mid 0.505 → 0.625, +24%).
    rising_books(&mut e, "up", 12, now);
    let at = now + 12_000;
    // Disabled → nothing, however good the setup looks.
    assert!(
        e.evaluate(at).is_empty(),
        "the chase leg must not trade while disabled"
    );
    assert!(e.set_strategy_enabled("trend_follow", true));
    let orders = e.evaluate(at);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].strategy, "trend_follow");
    assert_eq!(orders[0].token_id, "up");
    assert_eq!(orders[0].direction, "up");
    // It LIFTS the offer: the entry is above the mid, the inverse of the dip
    // buyer's below-mid resting bid.
    let mid = dec!(0.625);
    assert_eq!(orders[0].price, dec!(0.63), "the lifted offer");
    assert!(
        orders[0].price > mid,
        "{} must be above the mid {mid}",
        orders[0].price
    );
    assert!(
        orders[0].internal_key.starts_with("trend_follow:BTC:up:"),
        "{}",
        orders[0].internal_key
    );
    // And it cleared the spot gate without an exemption: with no spot buffer
    // at all the gate is a pass, so a breach here would mean a declaration.
    assert!(
        e.last_exemptions().is_empty(),
        "the chase leg waived nothing"
    );
}

#[test]
fn the_chase_leg_is_blocked_by_the_shared_spot_gate_when_spot_falls() {
    // The gate rejects a bet whose direction fights the spot move. A chase
    // entry is WITH the token's move, but when spot opposes it the candidate
    // must be stopped (it declares no exemption) — proving the new strategy
    // is subject to the same gates as the incumbent.
    let mut e = engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: 1_000_000,
    });
    assert!(e.set_strategy_enabled("trend_follow", true));
    // Spot falls: the momentum filter will refuse an UP bet.
    for i in 0..10 {
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000) - Decimal::from(i) * dec!(10),
            now_ms: 1_000_000 + i * 1000,
        });
    }
    rising_books(&mut e, "up", 12, 1_000_000);
    let orders = e.evaluate(1_000_000 + 12_000);
    assert!(orders.is_empty(), "spot moved against the bet: {orders:?}");
    assert!(
        e.last_blocked()
            .iter()
            .any(|b| b.strategy == "trend_follow" && b.reason == BlockReason::Momentum),
        "{:?}",
        e.last_blocked()
    );
}

#[test]
fn the_two_builtins_do_not_starve_each_other() {
    // Both strategies enabled, both presented with the setups they want on
    // DIFFERENT tokens: each gets its own entry in the same cycle, each
    // tagged with its own name, and neither is dropped.
    let mut e = engine();
    assert!(e.set_strategy_enabled("trend_follow", true));
    let now = 1_000_000i64;
    let mut m = market(1_800_000);
    m.down_token_id = "down".into();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![m.clone()],
        now_ms: now,
    });

    // "up" climbs (chase), "down" holds then dips (dip buy).
    for i in 0..=12 {
        let up_bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(up_bid, dec!(100))],
            asks: vec![(up_bid + dec!(0.01), dec!(100))],
            now_ms: now + i * 1000,
        });
        e.on_data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.61), dec!(100))],
            asks: vec![(dec!(0.63), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    // Dip on "down": mid 0.44 → the dip buyer's resting bid is 0.43, inside its
    // 0.45 ceiling, while the chase leg wants the OTHER token and is unaffected.
    e.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
    let orders = e.evaluate(now + 12_000);
    assert_eq!(
        orders.len(),
        2,
        "one entry per strategy per token: {orders:?}"
    );
    assert!(
        orders
            .iter()
            .any(|o| o.strategy == "trend_follow" && o.token_id == "up"),
        "{orders:?}"
    );
    assert!(
        orders
            .iter()
            .any(|o| o.strategy == "spread_arb" && o.token_id == "down"),
        "{orders:?}"
    );
    // Neither strategy was blocked out of existence.
    assert!(e.last_blocked().is_empty(), "{:?}", e.last_blocked());
}

#[test]
fn a_shared_token_is_settled_by_registration_order_not_by_starvation() {
    // When both want the SAME token in the SAME cycle the kernel emits one
    // entry (one entry per token per cycle) and the registered-first strategy
    // wins. This is the documented, deterministic tie-break — not a race.
    //
    // The default engine config cannot produce this collision at all: the dip
    // buyer's ceiling is 0.45 while the chase leg only looks above 0.55. So the
    // collision is constructed deliberately by raising the dip buyer's ceiling,
    // which is exactly the configuration where the tie-break has to be decided.
    let mut c = cfg();
    c.spread_arb.trend_max_entry_price = dec!(0.70);
    let mut e = engine_from(c);
    assert!(e.set_strategy_enabled("trend_follow", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // A book both want: "up" rising on the cent grid (the chase leg lifts the
    // 0.63 offer) while the mid 0.625 is still inside the dip buyer's raised
    // band (it would rest 0.61, below the mid).
    rising_books(&mut e, "up", 12, now);
    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 1, "one entry per token per cycle: {orders:?}");
    assert_eq!(
        orders[0].strategy, "spread_arb",
        "the incumbent holds the tie-break"
    );
    assert!(
        orders[0].price < dec!(0.625),
        "and it rests below the mid: {}",
        orders[0].price
    );
}

/// Reports one break per call and counts the calls, so a test can tell "the
/// hook ran" from "the host acted on what it returned" (#302).
struct BreakReporter {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl EngineStrategy for BreakReporter {
    fn name(&self) -> &str {
        "break_reporter"
    }
    fn on_book(&mut self, _token_id: &str, _snap: &OrderbookSnapshot, _now_ms: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vec![("up".to_string(), dec!(0.5))]
    }
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        Vec::new()
    }
}

#[test]
fn a_disabled_strategy_contributes_no_breaks() {
    let mut e = engine();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    e.register_user_strategy(
        Box::new(BreakReporter {
            calls: std::sync::Arc::clone(&calls),
        }),
        "test".into(),
    )
    .unwrap();

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // Several arms end in `drain_breaks`; count from here rather than
    // assuming how many ran before this event.
    let drained = calls.load(std::sync::atomic::Ordering::Relaxed);

    // Registered but off. The hook still runs — its library may be queueing
    // breaks internally and this call is what empties that queue — and
    // nothing it returns reaches the host.
    let broken = e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.55), dec!(100))],
        asks: vec![(dec!(0.57), dec!(100))],
        now_ms: now + 1_000,
    });
    assert!(
        broken.is_empty(),
        "a disabled strategy must not break another's bids, got {broken:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        drained + 1,
        "the hook is drained even while disabled, so its queue cannot grow"
    );

    // Enabled, the same break reaches the host.
    assert!(e.set_strategy_enabled("break_reporter", true));
    let broken = e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.55), dec!(100))],
        asks: vec![(dec!(0.57), dec!(100))],
        now_ms: now + 2_000,
    });
    assert_eq!(broken, vec![("up".to_string(), dec!(0.5))]);
}

#[test]
fn user_strategy_registers_disabled_and_is_tagged_once_enabled() {
    let mut e = engine();
    let name = e
        .register_user_strategy(
            Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
            "test".into(),
        )
        .unwrap();
    assert_eq!(name, "dip_buyer");
    assert_eq!(
        e.supported_strategies(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string(),
            "dip_buyer".to_string(),
        ]
    );
    assert_eq!(
        e.enabled_strategies(),
        vec!["spread_arb".to_string()],
        "starts disabled"
    );
    assert_eq!(e.strategy_source("dip_buyer"), Some("test"));
    // Duplicate names are rejected outright.
    assert!(
        e.register_user_strategy(
            Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
            "test".into()
        )
        .is_err()
    );

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.55), dec!(100))],
        asks: vec![(dec!(0.57), dec!(100))],
        now_ms: now + 1_000,
    });
    // Disabled → no order even though the mid satisfies the strategy.
    assert!(e.evaluate(now + 1_000).is_empty());

    assert!(e.set_strategy_enabled("dip_buyer", true));
    let orders = e.evaluate(now + 2_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].strategy, "dip_buyer");
    assert_eq!(orders[0].asset, "BTC");
    assert!(
        orders[0].internal_key.starts_with("dip_buyer:BTC:"),
        "{}",
        orders[0].internal_key
    );
}

/// Issue #205: the freshness budget is the entry gate, so the operator's value
/// has to be the one that decides. One book, one decision time, several
/// configs: refused at the compiled default, accepted *unchanged* once the
/// budget is widened past the book's age. The book is deliberately older than
/// any plausible default and well inside the knob's ceiling, so this is the
/// exact case an operator reaching for the flag cares about.
#[test]
fn the_configured_staleness_budget_decides_whether_a_book_can_trade() {
    let now = 1_000_000i64;
    let book_ms = now - 30_000; // 30s old at decision time

    let entries = |budget_ms: i64| -> Vec<crate::model::OrderRequest> {
        let mut e = engine_from(EngineConfig {
            max_orderbook_stale_ms: budget_ms,
            ..cfg()
        });
        e.register_user_strategy(
            Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("dip_buyer", true));
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: book_ms,
        });
        e.evaluate(now)
    };

    // Default budget → the 30s-old book is stale, and a stale book is not a
    // priceable book: the strategy never sees it, so nothing is placed.
    let default_budget = entries(DEFAULT_MAX_ORDERBOOK_STALE_MS);
    assert!(
        default_budget.is_empty(),
        "a {}-ms-old book must not trade under the {DEFAULT_MAX_ORDERBOOK_STALE_MS}ms default: {default_budget:?}",
        now - book_ms
    );
    // Widened budget → the very same book is fresh enough and trades.
    let wide = entries(60_000);
    assert_eq!(
        wide.len(),
        1,
        "a 60s budget must accept the same book: {wide:?}"
    );
    assert_eq!(wide[0].strategy, "dip_buyer");
    assert_eq!(wide[0].asset, "BTC");
    // `0` is the operator's explicit "check OFF": accepted however old.
    assert_eq!(
        entries(0).len(),
        1,
        "0 disables the freshness check, so the book is accepted"
    );
    // The age is the deciding variable, and it sits strictly between the two
    // budgets — otherwise this test would pass for the wrong reason.
    assert!(now - book_ms > DEFAULT_MAX_ORDERBOOK_STALE_MS);
    assert!(now - book_ms < 60_000);
}

/// The boundary of the freshness rule, at the one place that enforces it for
/// entries: an age equal to the budget is still fresh, one millisecond past it
/// is not, and `0` disables the age test without accepting an EMPTY book
/// (there would be nothing to price off).
#[test]
fn the_freshness_budget_boundary_and_its_off_switch() {
    let now = 1_000_000i64;
    let mut books = HashMap::new();
    let mut b = LocalBook::new();
    b.apply_snapshot(
        &[(dec!(0.55), dec!(100))],
        &[(dec!(0.57), dec!(100))],
        now - 8_000,
    );
    books.insert("up".to_string(), b);

    assert!(
        fresh_book(&books, "up", now, 8_000).is_some(),
        "an age exactly at the budget is still fresh"
    );
    assert!(
        fresh_book(&books, "up", now, 7_999).is_none(),
        "one millisecond past the budget is stale"
    );
    assert!(
        fresh_book(&books, "up", now, 0).is_some(),
        "0 disables the age test"
    );
    assert!(
        fresh_book(&books, "up", now, -1).is_some(),
        "any non-positive value is the OFF switch"
    );

    // OFF is not "accept anything": an empty book is still unpricable.
    let mut empty = LocalBook::new();
    empty.apply_snapshot(&[], &[], now);
    books.insert("flat".to_string(), empty);
    assert!(fresh_book(&books, "flat", now, 0).is_none());
    assert!(fresh_book(&books, "flat", now, 8_000).is_none());
    // An unknown token has no book at all.
    assert!(fresh_book(&books, "nope", now, 0).is_none());
}

#[test]
fn two_strategies_run_side_by_side_with_separate_tags() {
    let mut e = engine();
    e.register_user_strategy(
        Box::new(DipBuyer::new("dip_buyer", dec!(0.45))),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("dip_buyer", true));

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // Confirm the UP trend over 12 samples (spread_arb needs it).
    for i in 0..12 {
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    // UP dips → spread_arb candidate (and dip_buyer's too, on the same token).
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
    // DOWN is cheap → only dip_buyer signals there (no trend confirmation).
    e.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.41), dec!(100))],
        asks: vec![(dec!(0.43), dec!(100))],
        now_ms: now + 12_000,
    });

    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 2, "one entry per token per cycle: {orders:?}");
    assert!(
        orders
            .iter()
            .any(|o| o.token_id == "up" && o.strategy == "spread_arb"),
        "{orders:?}"
    );
    assert!(
        orders
            .iter()
            .any(|o| o.token_id == "down" && o.strategy == "dip_buyer"),
        "{orders:?}"
    );
}
