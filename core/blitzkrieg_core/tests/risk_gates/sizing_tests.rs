use crate::support::*;

// ── #202: one order is bounded as a share of the account ────────────────────

/// A dry core on a chosen account size with the equity-relative per-order cap
/// armed (`pct`) or off (`0`). The absolute cap is deliberately wide so only the
/// relative one can bite.
fn account_core(balance: Decimal, pct: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            max_order_notional_pct: pct,
            ..Default::default()
        },
        dry_seed_balance: balance,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions: PositionConfig::default(),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

#[test]
fn the_relative_cap_bounds_any_order_but_never_the_way_out() {
    // The live account: 4.8 USDC, 20% per order = 0.96 USD.
    let mut c = account_core(dec!(4.8), dec!(20));
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );

    // The measured live ticket — 10 shares at 0.40 = 4.00 USD, 83% of this
    // account — is refused, and the refusal names the cap that bit. The old
    // 6.00 absolute bound waved exactly this order through.
    let err = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(10), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect_err("an over-cap entry must be rejected, not truncated");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("equity cap"), "{}", err.message);
    assert!(
        c.positions().open_positions().is_empty(),
        "a refused entry leaves no position and no half-filled remnant"
    );

    // The largest whole ticket inside 0.96 is 2 shares (0.80): it fills, so the
    // escape hatch below has a real position to reduce.
    let (_, status) = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(2), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect("the in-cap entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 1);

    // The way out: 2 shares at a 0.59 bid is 1.18 USD — over the cap — and the
    // close must pass anyway, because a cap that traps a position is #174
    // reopened through another door.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.59), dec!(1000))],
        vec![(dec!(0.60), dec!(1000))],
        NOW + 5_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 5_000).unwrap(),
        1,
        "the closing sell must be submitted"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the close must have completed in full"
    );
}

#[test]
fn the_panel_states_one_orders_worst_case_and_the_cap_that_bounds_it() {
    // Armed: 20% of a 4.8 book = 0.96 USD, and that is the binding bound.
    let c = account_core(dec!(4.8), dec!(20));
    let stats = c.engine_stats_at(NOW);
    let sizing = &stats["sizing"];
    assert_eq!(sizing["equityUsd"].as_f64(), Some(4.8));
    assert_eq!(sizing["shareBandUsd"].as_f64(), Some(10.0));
    assert_eq!(sizing["maxOrderNotionalPct"].as_f64(), Some(20.0));
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(0.96));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(0.96));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!(pct <= 20.0 + 1e-9, "worst case must fit the cap: {pct}%");
    assert_eq!(sizing["closesExemptFromEquityCap"].as_bool(), Some(true));

    // Unconfigured (the shipped default): the same account has NO bound as a
    // share of itself — 10 shares × 1.00 = 10.00 USD, 208% of the book. That is
    // the hole #202 is about, and the panel now states it instead of leaving it
    // to be re-derived from the flags.
    let c = account_core(dec!(4.8), Decimal::ZERO);
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!((pct - 208.333_333).abs() < 0.001, "{pct}%");
    assert_eq!(sizing["equityCapUsd"], serde_json::Value::Null);

    // The same knobs on a 480 USD account: the PERCENTAGE is the same, the money
    // is not — which is why an absolute bound cannot say this on every account.
    let c = account_core(dec!(480), dec!(20));
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(96.0));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
}
