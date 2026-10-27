#![cfg(test)]

use super::*;
use blitzkrieg_market_api::DEFAULT_ACCOUNT_ID as DEFAULT_ACCT;
use rust_decimal_macros::dec;
// E2-b per-strategy gate opt-outs (#27) and D31 declared floors:
// timing/momentum exemptions, exemption accounting per account and
// per strategy, and the never-exemptible no-market precondition.
// ── E2-b per-strategy gate opt-out (#27) ────────────────────────────────

/// Book a trend-confirming run on `token`, then dip it: the setup the
/// builtin `spread_arb` needs to produce a candidate.
fn trend_then_dip(e: &mut Engine, token: &str, now: i64) {
    for i in 0..12 {
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + i * 1000,
        });
    }
    e.on_data(DataEvent::Book {
        token_id: token.into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
}

/// A single dip book on `token` at `at_ms`, with no trend run — the builtin
/// stays idle, so only a strategy that needs no trend confirmation can
/// signal here. Book it at the instant you evaluate: a stale book is not
/// priceable at all (`fresh_book` returns None).

/// Push a spot price that fell over the momentum window (against an UP bet).
fn spot_falls(e: &mut Engine, asset: &str, from: Decimal, to: Decimal, now: i64) {
    e.on_data(DataEvent::Spot {
        asset: asset.into(),
        price: from,
        now_ms: now,
    });
    e.on_data(DataEvent::Spot {
        asset: asset.into(),
        price: to,
        now_ms: now + 2_000,
    });
}

#[test]
fn declared_timing_exemption_lets_a_candidate_through_the_window_gate() {
    // Timing gate forced shut by an absurd min_round_age; a strategy that
    // declares the timing window unnecessary still enters. The exempting
    // strategy trades ETH so it does not compete for the builtin's BTC token
    // (one entry per token per cycle is NOT waivable).
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "window_fade",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("window_fade", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    dip_only(&mut e, "eth_up", now + 12_000);

    // The builtin is still gated; the declaring strategy is exempt.
    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].strategy, "window_fade");
    assert_eq!(orders[0].asset, "ETH");
    assert!(
        e.last_blocked().iter().any(|b| b.strategy == "spread_arb"),
        "builtin stays gated"
    );
    assert!(
        !e.last_blocked().iter().any(|b| b.strategy == "window_fade"),
        "exempted strategy must not appear as blocked"
    );
    // Auditable: the honoured exemption names the strategy, the gate and the
    // exact block that was waived.
    let ex = e.last_exemptions();
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert_eq!(ex[0].strategy, "window_fade");
    assert_eq!(ex[0].gate, "timing");
    assert!(ex[0].detail.contains("Round too young"), "{}", ex[0].detail);
    let line = ex[0].audit_line();
    assert!(
        line.contains("本单因策略 window_fade 豁免门禁 timing"),
        "{line}"
    );
}

#[test]
fn a_strategy_that_declares_nothing_is_still_gated() {
    // Identical setup, identical strategy body — only the declaration differs.
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::on_assets("plain", dec!(0.45), &["ETH"])),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("plain", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    dip_only(&mut e, "eth_up", now + 12_000);

    assert!(
        e.evaluate(now + 12_000).is_empty(),
        "undeclared strategy must stay gated"
    );
    assert!(
        e.last_exemptions().is_empty(),
        "nothing may be recorded as exempted"
    );
    assert!(
        e.last_blocked().iter().any(|b| b.strategy == "plain"),
        "{:?}",
        e.last_blocked()
    );
}

#[test]
fn round_markets_declared_expiry_beats_the_wall_clock_grid() {
    // The venue's declared round end (expires_at_ms) must drive
    // `time_left_sec` — not the host's wall-clock slot grid. Here the grid
    // would put the round end at 1_800_000 (800s left at 1_010_000), while
    // the venue declares 1_900_000 (890s left). Without the wiring the
    // D-31 floor flaked on wall-clock position (the strategy-gate-check
    // CI failure); with it the clock follows the venue declaration.
    let mut e = engine();
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market_of("BTC", 1_900_000)],
        now_ms: 1_000_000,
    });
    assert_eq!(
        e.scanner().round_state(1_010_000).time_left_sec,
        890,
        "the declared expiry, not the slot grid, is authoritative"
    );
}

#[test]
fn d31_declared_floor_stops_the_timing_exemption_waiving_the_time_left_gate() {
    // D-31: `timing: true` used to waive the time-left gate outright, so an
    // exempt strategy could enter with the round nearly over — where the exit
    // policy fires on the next tick and the trade is a guaranteed zero-hold.
    // A declared floor excludes exactly that window. The GLOBAL gate is the
    // ordinary 180s (that is what actually produces the block); the two
    // strategies differ ONLY in the floor they declare, which is the point.
    let mut late_cfg = cfg();
    late_cfg.scanner.min_round_age_sec = 0;
    late_cfg.scanner.min_time_left_sec = 180;
    let mut e = engine_from(late_cfg);
    // ETH: declares a 180s floor → must NOT be waived at 172s left.
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "floored",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: Some(180),
            },
        )),
        "test".into(),
    )
    .unwrap();
    // SOL: declares the SAME `timing` exemption but a lower floor, so its
    // opt-out still reaches into the last 100s. Keeping both in one cycle is
    // the point: the declared floor, and only it, separates them.
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "lowfloor",
            dec!(0.45),
            &["SOL"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: Some(100),
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("floored", true));
    assert!(e.set_strategy_enabled("lowfloor", true));
    // XRP: declares `timing` with NO floor. That must mean "use the kernel's
    // 180s", not "no floor at all" — i.e. it is blocked exactly like `floored`.
    // This is the backwards-compatibility guarantee a pre-D-31 library relies
    // on when it emits only the two booleans.
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "intestate",
            dec!(0.45),
            &["XRP"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("intestate", true));

    // Round expires at 1_800_000 → exactly 172s left at `at`.
    let at = 1_800_000 - 172_000;
    let now = at - 12_000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![
            market(1_800_000),
            market_of("ETH", 1_800_000),
            market_of("SOL", 1_800_000),
            market_of("XRP", 1_800_000),
        ],
        now_ms: now,
    });
    dip_only(&mut e, "eth_up", at);
    dip_only(&mut e, "sol_up", at);
    dip_only(&mut e, "xrp_up", at);

    let orders = e.evaluate(at);
    assert_eq!(
        e.scanner().round_state(at).time_left_sec,
        172,
        "fixture drift"
    );
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(
        orders[0].strategy, "lowfloor",
        "only the lower declared floor may still enter this late"
    );
    assert!(
        e.last_exemptions().iter().all(|x| x.strategy == "lowfloor"),
        "only the lower declared floor may be recorded as exempted: {:?}",
        e.last_exemptions()
    );
    assert!(
        e.last_blocked()
            .iter()
            .any(|b| b.strategy == "floored" && b.reason == BlockReason::Timing),
        "the floored strategy must be timing-blocked: {:?}",
        e.last_blocked()
    );
    assert!(
        e.last_blocked()
            .iter()
            .any(|b| b.strategy == "intestate" && b.reason == BlockReason::Timing),
        "an undeclared floor means the kernel's, not none: {:?}",
        e.last_blocked()
    );
    assert!(
        e.last_exemptions().iter().all(|x| x.strategy != "floored"),
        "nothing may be recorded as exempted for the floored strategy"
    );

    // The floor narrows the exemption; below it, nothing about the young-round
    // case changed. Re-run the same strategy against a "too young" block: its
    // `time_left_sec` is large, so the 180s floor never conflicts.
    let mut young_cfg = cfg();
    young_cfg.scanner.min_round_age_sec = 10_000;
    young_cfg.scanner.min_time_left_sec = 180;
    let mut e = engine_from(young_cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "floored",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: true,
                momentum: false,
                timing_min_time_left_sec: Some(180),
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("floored", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    dip_only(&mut e, "eth_up", now + 12_000);
    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 1, "too-young must stay waivable: {orders:?}");
    assert_eq!(orders[0].strategy, "floored");
    let ex = e.last_exemptions();
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert!(
        ex[0].detail.contains("Round too young"),
        "the honoured exemption is the young-round one: {}",
        ex[0].detail
    );
}

#[test]
fn d31_floor_is_read_through_the_abi_json_and_clamped_at_zero() {
    // The floor crosses the ABI as an optional JSON key, so it must survive a
    // round trip, degrade to the kernel default when absent, and clamp a
    // negative declaration (which would restore the unbounded waiver).
    let declared = serde_json::json!({
        "timing": true,
        "momentum": false,
        "timing_min_time_left_sec": 180,
    });
    let g = GateExemptions::from_json(&declared);
    assert!(g.timing && !g.momentum);
    assert_eq!(g.timing_min_time_left_sec, Some(180));
    assert_eq!(g.timing_floor_sec(0), 180);
    assert_eq!(g.to_json(), declared, "the declaration round-trips");

    // A pre-D-31 library emits only the two booleans: the missing key must
    // mean "use the kernel default", never "no floor".
    let legacy = GateExemptions::from_json(&serde_json::json!({
        "timing": true,
        "momentum": false,
    }));
    assert_eq!(legacy.timing_min_time_left_sec, None);
    assert_eq!(legacy.timing_floor_sec(180), 180);
    assert_eq!(
        legacy.to_json(),
        serde_json::json!({ "timing": true, "momentum": false }),
        "an undeclared floor must not be written back"
    );

    // Malformed / negative declarations degrade inward, never outward.
    let bad = GateExemptions::from_json(&serde_json::json!({
        "timing": true,
        "timing_min_time_left_sec": "180",
    }));
    assert_eq!(
        bad.timing_min_time_left_sec, None,
        "a string is not a floor"
    );
    assert_eq!(
        GateExemptions::from_json(&serde_json::json!({
            "timing": true,
            "timing_min_time_left_sec": -60,
        }))
        .timing_floor_sec(0),
        0,
        "a negative floor clamps to zero"
    );
}

#[test]
fn declared_momentum_exemption_lets_a_mean_reversion_entry_through() {
    let mut e = engine();
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "fader",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: false,
                momentum: true,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("fader", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    dip_only(&mut e, "eth_up", now + 12_000);
    // Both spots fall over the momentum window: exactly the case a
    // mean-reversion entry wants (and the builtin filter rejects).
    spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now + 1000);
    spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now + 1000);

    let orders = e.evaluate(now + 12_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].strategy, "fader");
    assert_eq!(orders[0].asset, "ETH");
    let ex = e.last_exemptions();
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert_eq!(ex[0].gate, "momentum");
    assert!(ex[0].detail.contains("spot ETH"), "{}", ex[0].detail);
    assert!(ex[0].detail.contains("vs up"), "{}", ex[0].detail);
    // The builtin, declaring nothing, was blocked by the very same filter.
    assert!(
        e.last_blocked()
            .iter()
            .any(|b| b.strategy == "spread_arb" && b.reason == BlockReason::Momentum)
    );
}

#[test]
fn an_exemption_is_scoped_to_the_declaring_strategy_only() {
    // Two strategies on different assets in the SAME cycle: only the one that
    // declared the momentum exemption gets through.
    let mut e = engine();
    assert!(e.set_strategy_enabled("spread_arb", false));
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "fader",
            dec!(0.45),
            &["BTC"],
            GateExemptions {
                timing: false,
                momentum: true,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("fader", true));
    // A second, undeclared strategy restricted to ETH.
    e.register_user_strategy(
        Box::new(DipBuyer::on_assets("gated", dec!(0.45), &["ETH"])),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("gated", true));

    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    dip_only(&mut e, "up", now + 2_000);
    dip_only(&mut e, "eth_up", now + 2_000);
    spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now);
    spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now);

    let orders = e.evaluate(now + 2_000);
    assert_eq!(
        orders.len(),
        1,
        "only the declaring strategy may pass: {orders:?}"
    );
    assert_eq!(orders[0].strategy, "fader");
    assert_eq!(orders[0].asset, "BTC");
    assert_eq!(e.last_exemptions().len(), 1);
    assert!(
        e.last_blocked().iter().any(|b| b.strategy == "gated"
            && b.asset == "ETH"
            && b.reason == BlockReason::Momentum)
    );
}

#[test]
fn the_no_market_precondition_is_never_exemptible() {
    // Declaring every gate must not conjure a market: with no round markets
    // there is no priceable token, so nothing is exempted.
    let mut e = engine();
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "all_in",
            dec!(0.99),
            &[],
            GateExemptions::all(),
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("all_in", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now,
    });
    assert!(
        e.evaluate(now).is_empty(),
        "no market → no candidates at all"
    );
    assert!(
        e.last_exemptions().is_empty(),
        "a structural precondition is not exemptible"
    );
}

#[test]
fn blocked_candidates_carry_their_owning_strategy() {
    // Per-strategy attribution of blocked.timing / blocked.momentum (#27).
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    assert!(e.evaluate(now + 12_000).is_empty());
    assert_eq!(e.last_blocked().len(), 1);
    assert_eq!(e.last_blocked()[0].strategy, "spread_arb");

    e.tally_blocked();
    assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
    assert_eq!(e.gate_tally("spread_arb").exempted_timing, 0);
    assert_eq!(e.gate_tally("nobody"), StrategyGateTally::default());
}

/// 账户跟随（策略计数的账户切片）：same strategy on two accounts, each
/// scope's gate verdicts land under ITS OWN (strategy, account) key —
/// account A's page must never print account B's blocked counts, and the
/// global per-name tally stays the exact sum (the replay baseline reads
/// it byte-for-byte unchanged).
#[test]
fn blocked_gate_counts_are_attributable_per_account() {
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    let now = 1_000_000i64;
    // The paper scope is trading-OFF until the host mirrors the account
    // book (the engine fixture never does).
    e.set_account_trading("paper", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // Two candidates for the SAME token, DIFFERENT accounts: the timing
    // gate blocks both (one entry per (account, token), so both ride the
    // batch), each stamped with its own scope.
    let orders = e.submit_candidate_orders_for_test(
        vec![
            scoped_signal(DEFAULT_ACCT, "up"),
            scoped_signal("paper", "up"),
        ],
        now + 12_000,
    );
    assert!(orders.is_empty(), "both scopes are gated: {orders:?}");
    assert_eq!(e.last_blocked().len(), 2);

    e.tally_blocked();
    let d = e.gate_tally_for_account("probe", DEFAULT_ACCT);
    let p = e.gate_tally_for_account("probe", "paper");
    assert_eq!(
        d.blocked_timing, 1,
        "default account blocked_timing mismatch"
    );
    assert_eq!(p.blocked_timing, 1, "paper account blocked_timing mismatch");
    assert_eq!(d.blocked_momentum, 0);
    assert_eq!(p.exempted_timing, 0);
    // A scope that never saw a gate verdict reads zeros.
    assert_eq!(
        e.gate_tally_for_account("probe", "nobody"),
        StrategyGateTally::default()
    );
    // The per-name fold is unchanged: the sum of the slices.
    assert_eq!(e.gate_tally("probe").blocked_timing, 2);
}

/// Same slicing for the honoured-exemption trail: each scope's exemption
/// counts under its own (strategy, account) key, global unchanged.
#[test]
fn honoured_exemptions_are_counted_per_account() {
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "window_fade",
            dec!(0.45),
            &["BTC"],
            GateExemptions {
                timing: true,
                momentum: true,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("window_fade", true));
    let now = 1_000_000i64;
    e.set_account_trading("paper", true);
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    // Both scopes' candidates go through; both are EXEMPTED (the declared
    // opt-out), each stamped with its own account. The candidates carry
    // the strategy's own name — the exemption lookup reads the declared
    // opt-out per strategy.
    let mut a = scoped_signal(DEFAULT_ACCT, "up");
    let mut p = scoped_signal("paper", "up");
    a.strategy = "window_fade".into();
    p.strategy = "window_fade".into();
    let orders = e.submit_candidate_orders_for_test(vec![a, p], now + 12_000);
    assert_eq!(
        orders.len(),
        2,
        "both scopes ride the exemption: {orders:?}"
    );
    assert_eq!(e.last_exemptions().len(), 2);

    let drained = e.take_exemptions();
    assert_eq!(drained.len(), 2);
    assert!(
        drained
            .iter()
            .all(|r| r.strategy == "window_fade" && r.gate == "timing")
    );
    let d = e.gate_tally_for_account("window_fade", DEFAULT_ACCT);
    let p = e.gate_tally_for_account("window_fade", "paper");
    assert_eq!(
        d.exempted_timing, 1,
        "unexpected exempted_timing for default account"
    );
    assert_eq!(
        p.exempted_timing, 1,
        "unexpected exempted_timing for paper account"
    );
    assert_eq!(d.blocked_timing, 0);
    assert_eq!(p.blocked_momentum, 0);
    // The per-name fold keeps its pre-fanout value semantics: the sum.
    assert_eq!(e.gate_tally("window_fade").exempted_timing, 2);
}

#[test]
fn honoured_exemptions_are_counted_per_strategy_and_drained_once() {
    let mut cfg = cfg();
    cfg.scanner.min_round_age_sec = 10_000;
    let mut e = engine_from(cfg);
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "window_fade",
            dec!(0.45),
            &["ETH"],
            GateExemptions {
                timing: true,
                momentum: true,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    assert!(e.set_strategy_enabled("window_fade", true));
    let now = 1_000_000i64;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
        now_ms: now,
    });
    trend_then_dip(&mut e, "up", now);
    dip_only(&mut e, "eth_up", now + 12_000);
    assert_eq!(e.evaluate(now + 12_000).len(), 1);
    e.tally_blocked();

    let drained = e.take_exemptions();
    assert_eq!(drained.len(), 1);
    assert!(e.take_exemptions().is_empty(), "drained once");
    let t = e.gate_tally("window_fade");
    assert_eq!(t.exempted_timing, 1);
    assert_eq!(t.exempted_momentum, 0);
    // Counting an exemption never inflates the blocked counters: the builtin
    // was blocked by the very gate the other strategy declared unnecessary.
    assert_eq!(e.blocked_timing_count(), 1, "only the builtin was blocked");
    assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
    assert_eq!(e.gate_tally("window_fade").blocked_timing, 0);
}

#[test]
fn declared_exemptions_are_listed_for_audit() {
    let mut e = engine();
    e.register_user_strategy(
        Box::new(DipBuyer::exempting(
            "fader",
            dec!(0.45),
            &[],
            GateExemptions {
                timing: false,
                momentum: true,
                timing_min_time_left_sec: None,
            },
        )),
        "test".into(),
    )
    .unwrap();
    e.register_user_strategy(Box::new(DipBuyer::new("plain", dec!(0.45))), "test".into())
        .unwrap();
    let declared = e.declared_gate_exemptions();
    assert_eq!(
        declared.len(),
        2,
        "silent strategies are omitted: {declared:?}"
    );
    assert_eq!(
        declared[0].0, "mean_reversion",
        "the builtin declares first (hosted first)"
    );
    assert_eq!(declared[0].1.gates(), vec!["momentum"]);
    assert_eq!(declared[1].0, "fader");
    assert_eq!(declared[1].1.gates(), vec!["momentum"]);
    assert_eq!(
        e.strategy_gate_exemptions("plain"),
        Some(GateExemptions::none())
    );
    assert_eq!(e.strategy_gate_exemptions("nobody"), None);
}

#[test]
fn the_builtin_declares_no_exemptions() {
    let e = engine();
    assert_eq!(
        e.strategy_gate_exemptions("spread_arb"),
        Some(GateExemptions::none())
    );
}

#[test]
fn disabling_spread_arb_stops_its_entries() {
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
    e.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.43), dec!(100))],
        asks: vec![(dec!(0.45), dec!(100))],
        now_ms: now + 12_000,
    });
    assert_eq!(e.evaluate(now + 12_000).len(), 1);
    assert!(e.set_strategy_enabled("spread_arb", false));
    // Same round, no order noted as pending: the empty result must come from
    // the disable, not from suppression.
    assert!(
        e.evaluate(now + 12_001).is_empty(),
        "disabled strategy must not emit"
    );
    assert!(e.set_strategy_enabled("spread_arb", true));
    assert_eq!(
        e.evaluate(now + 12_002).len(),
        1,
        "re-enabling restores entries"
    );
}
