use super::*;

/// The sparse-cadence contract: a quiet tick inside the fallback window
/// advances the scheduled-cycle counter but does NOT call the strategy;
/// a fresh engine input wakes the evaluation early; the fallback beat
/// bounds anything the wake/boundary model does not cover.
#[test]
fn fast_mode_quiet_tick_skips_the_engine_but_keeps_the_counter() {
    let (mut c, count) = core_with_probe(true);
    let t0 = 1_000_000i64;
    c.tick(t0).unwrap();
    c.engine_evaluate(t0);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Quiet: no data, no timing boundary, inside the 2 s fallback window.
    for i in 1..20 {
        let t = t0 + i * 50;
        c.tick(t).unwrap();
        c.engine_evaluate(t);
    }
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a quiet tick must not call the strategy"
    );
    assert_eq!(
        c.stats.evaluations, 20,
        "the scheduled-cycle counter advances even on quiet ticks"
    );

    // An engine input wakes the evaluation before the fallback beat.
    let tw = t0 + 1000;
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(Decimal::new(55, 2), Decimal::from(100))],
            asks: vec![(Decimal::new(57, 2), Decimal::from(100))],
            now_ms: tw,
        },
        tw,
    );
    c.tick(tw).unwrap();
    c.engine_evaluate(tw);
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a book event wakes the evaluation"
    );

    // The fallback beat evaluates even with nothing new (last eval was
    // the wake at t0+1000, so the 2 s bound fires at t0+3000).
    let tf = t0 + 3000;
    c.tick(tf).unwrap();
    c.engine_evaluate(tf);
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the fallback beat evaluates"
    );
}

/// Fast OFF (the default): every scheduled cycle evaluates the strategy —
/// the historical behaviour the fast path must not disturb.
#[test]
fn default_mode_evaluates_every_scheduled_cycle() {
    let (mut c, count) = core_with_probe(false);
    let t0 = 1_000_000i64;
    for i in 0..5 {
        let t = t0 + i * 50;
        c.tick(t).unwrap();
        c.engine_evaluate(t);
    }
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "with the fast path off, every scheduled cycle evaluates"
    );
}

/// A dip probe whose `buy_below` threshold is 0: ANY fresh book (even a
/// mid of 0.00) arms an entry priced at that mid. A book whose sides are
/// both empty prices the signal at 0.00 — the exact intent shape that
/// PANICKED the kernel at the policy budget-shrink division
/// (`budget_usd / req.price`) before arbitration Gate 1 could refuse it.
/// The guard refuses the intent fail-closed: counted, bucketed, logged,
/// and the process survives (0 orders placed, no panic).
#[test]
fn zero_price_entry_is_refused_fail_closed_not_a_panic() {
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        assets: vec!["BTC".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "zero_dip".to_string(),
            buy_below: Decimal::ZERO,
            assets: vec!["BTC".to_string()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("zero_dip", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    // An ask-only book on the BTC up token: fresh (timestamp is now) and
    // NON-empty (so fresh_book serves it), but with no bid the F6 rule
    // prices the mid at 0.00 — the one-sided shape a top-only feed
    // degrades into, and the exact intent shape that PANICKED the kernel
    // at the policy budget-shrink division before this guard existed.
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![],
            asks: vec![(dec!(0.60), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    // The evaluate itself must not panic — this is the regression the
    // test exists for (pre-guard this panicked inside the policy arm).
    let placed = c.engine_evaluate(now + 1_100);
    assert_eq!(placed, 0, "a 0.00-price entry must be refused, not placed");
    assert!(
        c.list_orders().is_empty(),
        "no order may reach the book: {:#?}",
        c.list_orders()
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["illegalPriceRejected"], 1,
        "the guard counts the refusal: {stats}"
    );
    assert_eq!(
        stats["placeRejected"], 1,
        "and it counts as a place rejection"
    );
    // The per-strategy accounting carries the cause bucket.
    let rows = c.strategy_stats();
    let row = rows
        .iter()
        .find(|r| r["name"] == "zero_dip")
        .expect("the strategy's accounting row");
    assert_eq!(
        row["rejectionCauses"]["illegal.price"], 1,
        "bucketed under illegal.price: {row}"
    );
}

// ── E4-a: startup strategy selection ────────────────────────────────────

#[test]
fn three_strategies_size_independently_in_one_cycle() {
    // Global band is [10,10] at a 2.5u budget; two strategies tighten it and
    // one inherits it. All three trade the same cycle without interfering.
    let limits = limits_of(&[
        (
            "small",
            StrategyLimit {
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(2)),
                max_shares: Some(dec!(2)),
                ..Default::default()
            },
        ),
        (
            "mid",
            StrategyLimit {
                size_usd: Some(dec!(2)),
                min_shares: Some(dec!(4)),
                max_shares: Some(dec!(4)),
                ..Default::default()
            },
        ),
    ]);
    let mut c = core_with_dips(limits, &DIPS, 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        3,
        "one entry per strategy: {:#?}",
        c.list_orders()
    );

    let sizes: Vec<(String, Decimal)> = c
        .list_orders()
        .iter()
        .map(|o| (o.strategy.clone(), o.size))
        .collect();
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "small").map(|(_, v)| *v),
        Some(dec!(2)),
        "1u/0.44 → 2 shares inside [2,2]: {sizes:?}"
    );
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "mid").map(|(_, v)| *v),
        Some(dec!(4)),
        "2u/0.44 ≈ 5 → clamped to the strategy's 4-share ceiling: {sizes:?}"
    );
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "unset").map(|(_, v)| *v),
        Some(dec!(10)),
        "no override must fall back to the global lot: {sizes:?}"
    );

    let stats = c.strategy_stats();
    let small = strategy_entry(&stats, "small");
    assert_eq!(small["sizingSource"], "strategy");
    assert_eq!(dec_of(&small["effectiveSizeUsd"]), dec!(1));
    assert_eq!(dec_of(&small["effectiveMinShares"]), dec!(2));
    assert_eq!(dec_of(&small["effectiveMaxShares"]), dec!(2));
    let unset = strategy_entry(&stats, "unset");
    assert_eq!(unset["sizingSource"], "global");
    assert_eq!(dec_of(&unset["effectiveSizeUsd"]), dec!(2.5));
    assert_eq!(dec_of(&unset["effectiveMaxShares"]), dec!(10));
}

// ── #202: the engine sizes from the balance the ledger hands it ─────────

/// The acceptance behind #202's first half: the SAME 20% on two account
/// sizes produces lots 100× apart, and the engine reads the balance the
/// ledger actually holds rather than a copy taken at configuration time.
#[test]
fn equity_sizing_scales_one_order_with_the_account() {
    let now = 1_000_000i64;
    let mut small = equity_core(dec!(4.8), dec!(20));
    feed_btc_dip(&mut small, now);
    assert_eq!(small.engine_evaluate(now + 1_000), 1);
    // 4.8 × 20% = 0.96 → 0.96/0.44 = 2.18 → 2 shares. The pre-#202 path
    // sent 10 shares (4.40 USD, 92% of this account) whatever the price.
    assert_eq!(small.list_orders()[0].size, dec!(2));
    assert_eq!(
        small.engine.as_ref().unwrap().equity_usd(),
        small.ledger().balance(),
        "the engine must have been handed the ledger's own balance"
    );

    let mut big = equity_core(dec!(480), dec!(20));
    feed_btc_dip(&mut big, now);
    assert_eq!(big.engine_evaluate(now + 1_000), 1);
    // 96/0.44 = 218.18 → 218 shares: the same statement, 100× the account.
    assert_eq!(big.list_orders()[0].size, dec!(218));
    // The panel states both the percentage and what it is worth here, so the
    // scaling is visible without re-deriving it from the flags.
    let small_stats = small.strategy_stats();
    assert_eq!(
        dec_of(&strategy_entry(&small_stats, "dip")["effectiveSizePct"]),
        dec!(20)
    );
    assert_eq!(
        dec_of(&strategy_entry(&small_stats, "dip")["entryBudgetUsd"]),
        dec!(0.96)
    );
    let big_stats = big.strategy_stats();
    assert_eq!(
        dec_of(&strategy_entry(&big_stats, "dip")["entryBudgetUsd"]),
        dec!(96)
    );
}

/// A budget that cannot buy one whole share emits NO order and says so in
/// the stats, rather than turning into a 0-size rejection on every tick.
#[test]
fn an_unaffordable_equity_budget_skips_the_signal_visibly() {
    let now = 1_000_000i64;
    let mut c = equity_core(dec!(4.8), dec!(1)); // 0.048 USD per entry
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 0, "no ticket, no order");
    assert!(c.list_orders().is_empty());
    assert_eq!(
        c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
        Some(1),
        "the skip must be counted, not silent"
    );
}

/// Unconfigured (the shipped default) is untouched: the service pushes a
/// balance every cycle now, and this is the test that proves that push alone
/// changes nothing about an absolute-budget deployment's orders.
#[test]
fn an_unconfigured_core_keeps_its_historical_lot() {
    let now = 1_000_000i64;
    let mut c = equity_core(dec!(4.8), Decimal::ZERO);
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    // The historical path: 2.5/0.44 ≈ 5.68 → 6 shares, inside [1, 1000].
    assert_eq!(c.list_orders()[0].size, dec!(6));
    assert_eq!(
        c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
        Some(0)
    );
}

#[test]
fn per_strategy_quota_is_independent_across_strategies() {
    // `capped` may hold zero positions and targets two assets; `other` has no
    // cap and targets a third. In one cycle the cap rejects both of `capped`'s
    // candidates while `other` trades untouched — the quotas are per strategy,
    // and one strategy being full never starves another.
    let limits = limits_of(&[(
        "capped",
        StrategyLimit {
            max_open_positions: Some(0),
            size_usd: Some(dec!(1)),
            min_shares: Some(dec!(2)),
            max_shares: Some(dec!(2)),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(
        limits,
        &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
        5,
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "only `other` may place: {:#?}",
        c.list_orders()
    );
    assert_eq!(
        c.engine_stats()["strategyLimitRejected"],
        2,
        "both capped candidates were rejected"
    );
    assert_eq!(c.list_orders()[0].strategy, "other");

    let stats = c.strategy_stats();
    let capped = strategy_entry(&stats, "capped");
    assert_eq!(capped["ordersPlaced"], 0);
    assert_eq!(
        capped["limitRejected"], 2,
        "counted against the capped strategy only"
    );
    assert_eq!(capped["maxOpenPositions"], 0);
    assert_eq!(
        dec_of(&capped["effectiveMaxShares"]),
        dec!(2),
        "its own lot is 2 shares"
    );
    let other = strategy_entry(&stats, "other");
    assert_eq!(other["ordersPlaced"], 1, "the sibling still trades");
    assert_eq!(other["limitRejected"], 0);
    assert_eq!(
        other["maxOpenPositions"],
        serde_json::Value::Null,
        "unset = uncapped"
    );
    assert_eq!(
        dec_of(&other["effectiveSizeUsd"]),
        dec!(2.5),
        "and inherits the global sizing"
    );
}

#[test]
fn a_strategy_cap_counts_only_its_own_open_positions() {
    // `capped` allows one position and gets exactly one: it takes BTC, fills
    // it, and is then rejected on ETH — while `other`'s SOL position is
    // invisible to `capped`'s quota (and vice versa).
    let limits = limits_of(&[(
        "capped",
        StrategyLimit {
            max_open_positions: Some(1),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(
        limits,
        &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
        5,
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        3,
        "two capped candidates + one sibling"
    );

    // Fill BTC (capped) and SOL (other): two live positions, one per strategy.
    fill(&mut c, "BTC", now + 2_000);
    fill(&mut c, "SOL", now + 2_000);
    let open = c.positions().open_positions();
    assert_eq!(open.len(), 2, "{open:#?}");
    assert_eq!(open.iter().filter(|p| p.strategy == "capped").count(), 1);
    assert_eq!(open.iter().filter(|p| p.strategy == "other").count(), 1);
}

#[test]
fn the_global_position_ceiling_still_binds_across_strategies() {
    // Per-strategy quotas do not lift the global capacity gate. Open two
    // positions with two strategies, then let a THIRD strategy find a dip:
    // its entry is rejected by the shared global ceiling, not by its own cap.
    let mut c = core_with_dips(
        HashMap::new(),
        &[("a", &["BTC"]), ("b", &["ETH"]), ("c", &["SOL"])],
        2,
    );
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );

    feed_dip_on(&mut c, now + 1_000, &["BTC"]);
    assert_eq!(c.engine_evaluate(now + 1_000), 1, "a takes BTC");
    fill(&mut c, "BTC", now + 2_000);
    feed_dip_on(&mut c, now + 3_000, &["ETH"]);
    assert_eq!(c.engine_evaluate(now + 3_000), 1, "b takes ETH");
    fill(&mut c, "ETH", now + 4_000);
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "global ceiling reached"
    );

    feed_dip_on(&mut c, now + 5_000, &["SOL"]);
    assert_eq!(
        c.engine_evaluate(now + 5_000),
        0,
        "the third strategy must be gated out"
    );
    assert_eq!(
        c.engine_stats()["placeRejected"],
        1,
        "rejected by the global capacity gate"
    );
    assert_eq!(
        c.engine_stats()["strategyLimitRejected"],
        0,
        "and NOT by a per-strategy quota"
    );
    assert_eq!(c.positions().open_positions().len(), 2);

    let stats = c.strategy_stats();
    // No strategy configured a quota, so all three report uncapped yet the
    // global gate still held the line.
    for name in ["a", "b", "c"] {
        assert_eq!(
            strategy_entry(&stats, name)["maxOpenPositions"],
            serde_json::Value::Null
        );
    }
    assert_eq!(strategy_entry(&stats, "c")["ordersRejected"], 1);
}

#[test]
fn a_greedy_strategy_override_is_clamped_to_the_global_risk_band() {
    // sizeUsd 100 / maxShares 999 / minShares 0 all lose to the globals; the
    // report still marks the strategy as sizing-scoped.
    let limits = limits_of(&[(
        "small",
        StrategyLimit {
            size_usd: Some(dec!(100)),
            min_shares: Some(dec!(0)),
            max_shares: Some(dec!(999)),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(limits, &[("small", &["BTC"])], 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    assert_eq!(c.list_orders()[0].size, dec!(10), "global ceiling must win");

    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "small");
    assert_eq!(s["sizingSource"], "strategy");
    assert_eq!(
        dec_of(&s["effectiveSizeUsd"]),
        dec!(2.5),
        "notional clamped to the global budget"
    );
    assert_eq!(
        dec_of(&s["effectiveMaxShares"]),
        dec!(10),
        "share ceiling clamped to the global"
    );
    assert_eq!(
        dec_of(&s["effectiveMinShares"]),
        dec!(10),
        "floor raised to the global min"
    );
}

// ── E2-b: per-strategy gate opt-out (#27) ───────────────────────────────

/// Shut for every strategy that did not declare the timing window unnecessary.
const WINDOW_SHUT: i64 = 10_000;

#[test]
fn a_declared_timing_exemption_places_an_order_the_gate_would_have_blocked() {
    let mut c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("gated", &["ETH"])],
        5,
        WINDOW_SHUT,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: false,
            timing_min_time_left_sec: None,
        },
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "only the declaring strategy may trade: {:#?}",
        c.list_orders()
    );
    assert_eq!(c.list_orders()[0].strategy, "fader");
    assert_eq!(
        c.engine_stats()["blocked"]["timing"],
        1,
        "the sibling stayed gated"
    );

    let stats = c.strategy_stats();
    let fader = strategy_entry(&stats, "fader");
    assert_eq!(fader["gateExemptions"], serde_json::json!(["timing"]));
    assert_eq!(
        fader["gateExemptedTiming"], 1,
        "the honoured exemption is counted"
    );
    assert_eq!(fader["blockedTiming"], 0);
    let gated = strategy_entry(&stats, "gated");
    assert_eq!(gated["gateExemptions"], serde_json::json!([]));
    assert_eq!(gated["blockedTiming"], 1);
    assert_eq!(gated["gateExemptedTiming"], 0);
    assert_eq!(gated["ordersPlaced"], 0);
}

#[test]
fn blocked_gate_counts_are_attributable_to_a_specific_strategy() {
    let mut c = core_with_declared_dips(
        HashMap::new(),
        &[("a", &["BTC"]), ("b", &["ETH"])],
        5,
        WINDOW_SHUT,
        |_| crate::strategies::GateExemptions::none(),
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 0, "both are gated");

    let blocked = c.engine_stats()["blocked"].clone();
    assert_eq!(blocked["timing"], 2, "global total");
    assert_eq!(blocked["byStrategy"]["a"]["timing"], 1, "{blocked}");
    assert_eq!(blocked["byStrategy"]["b"]["timing"], 1, "{blocked}");
    assert_eq!(blocked["byStrategy"]["a"]["momentum"], 0, "{blocked}");
    // Only the hosted test adapter declares an exemption (momentum) here;
    // the test dip strategies declare nothing beyond what their builder gave
    // them. (PR-B: there is no builtin set — every hosted strategy declares
    // its own gates through the same seam.)
    assert_eq!(
        blocked["declaredExemptions"],
        serde_json::json!([{ "strategy": "mean_reversion", "gates": ["momentum"] }])
    );
}

/// 账户跟随（策略计数的账户切片）：同一策略名在两个账户各跑一个 scope，
/// 各自的 gate 判定只落在自己的 (strategy, account) 键下 —— 账户 A 的
/// 策略页绝不能看见账户 B 被挡了几次，而全局 fold（回放基线）不动。
#[test]
fn gate_counts_follow_the_account_in_the_strategy_page() {
    // One strategy NAME, two scopes: default dips BTC, paper dips ETH.
    // WINDOW_SHUT shuts the timing gate for every scope.
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            round_duration_sec: 900,
            min_round_age_sec: WINDOW_SHUT,
            auto_exits_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            positions: crate::position::PositionConfig {
                max_positions: 5,
                exit: crate::exit_policy::ExitConfig {
                    min_time_left_sec: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        book,
    );
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "acc".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    eng.register_scoped_strategy(
        Box::new(TargetDip {
            name: "acc".into(),
            buy_below: dec!(0.45),
            assets: vec!["ETH".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: "paper".into(),
        }),
        "test".into(),
        "paper",
    )
    .expect("paper scope registers");
    assert!(eng.set_strategy_enabled("acc", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "both scopes are timing-gated"
    );

    let global_row = strategy_entry(&c.strategy_stats(), "acc").clone();
    assert_eq!(
        global_row["blockedTiming"], 2,
        "the per-name fold is the sum of both slices: {global_row}"
    );

    let for_default = c.strategy_stats_for_account("default");
    let d = strategy_entry(&for_default, "acc");
    assert_eq!(
        d["blockedTiming"], 1,
        "default sees only its own block: {d}"
    );
    assert_eq!(d["blockedMomentum"], 0);
    assert_eq!(d["gateExemptedTiming"], 0);
    let for_paper = c.strategy_stats_for_account("paper");
    let p = strategy_entry(&for_paper, "acc");
    assert_eq!(p["blockedTiming"], 1, "paper sees only its own block: {p}");
    assert_eq!(p["blockedMomentum"], 0);
    assert_eq!(p["gateExemptedTiming"], 0);
}

#[test]
fn declared_exemptions_are_reported_in_engine_stats() {
    let c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("plain", &["ETH"])],
        5,
        0,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: name == "fader",
            timing_min_time_left_sec: None,
        },
    );
    let declared = c.engine_stats()["blocked"]["declaredExemptions"].clone();
    assert_eq!(
        declared,
        serde_json::json!([
            { "strategy": "mean_reversion", "gates": ["momentum"] },
            { "strategy": "fader", "gates": ["timing", "momentum"] }
        ]),
        "hosted and user declarations are both listed"
    );
}

#[test]
fn a_declared_timing_floor_is_reported_beside_the_gates() {
    // D-31: the floor is the difference between "this opt-out reaches into the
    // closing window" and "it stops at the global window", so it has to be
    // visible from the stats snapshot alone. `fader` declares a floor;
    // `plain` does not, and a strategy that declares nothing must NOT grow a
    // key it never asked for.
    let c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("plain", &["ETH"])],
        5,
        0,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: name == "fader",
            timing_min_time_left_sec: (name == "fader").then_some(180),
        },
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["blocked"]["declaredExemptions"],
        serde_json::json!([
            { "strategy": "mean_reversion", "gates": ["momentum"] },
            { "strategy": "fader", "gates": ["timing", "momentum"], "timingFloorSec": 180 }
        ]),
        "a declared floor is named; an undeclared one adds nothing"
    );

    let by_name = |n: &str| -> serde_json::Value {
        stats["strategies"]
            .as_array()
            .expect("strategies array")
            .iter()
            .find(|s| s["name"] == n)
            .unwrap_or_else(|| panic!("no stats row for {n}"))["gateExemptionTimingFloorSec"]
            .clone()
    };
    assert_eq!(by_name("fader"), serde_json::json!(180));
    // `null`, not absent and not 0: "this strategy left the floor to the kernel"
    // must be distinguishable from "it declared a floor of zero".
    assert_eq!(by_name("plain"), serde_json::Value::Null);
}

#[test]
fn a_gate_exemption_never_bypasses_a_safety_boundary() {
    // The strongest form of the acceptance criterion: a strategy that declares
    // BOTH entry gates unnecessary still cannot get past the global risk gate,
    // the kill switch or the global capacity ceiling — those live in
    // `Core::place`, far below the entry gates.
    // The risk gate snapshots `config.risk` at construction, so the per-order
    // notional ceiling is set here: 1 USD vs the 0.44 x 10 = 4.40 order.
    let mut c = core_with_declared_dips_and_risk(
        HashMap::new(),
        &[("all_in", &["BTC"])],
        5,
        WINDOW_SHUT,
        RiskConfig {
            max_order_notional: dec!(1),
            ..Default::default()
        },
        |_| crate::strategies::GateExemptions::all(),
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);

    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "risk gate must still reject"
    );
    assert_eq!(c.engine_stats()["placeRejected"], 1);
    assert!(c.list_orders().is_empty());
    assert!(
        c.strategy_stats().iter().any(|s| {
            s["name"] == "all_in"
                && s["gateExemptions"] == serde_json::json!(["timing", "momentum"])
        }),
        "the declaration is still reported even though the order was refused"
    );

    // Kill switch: identical exemption, hard stop wins.
    c.kill("test".into());
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "kill switch is not exemptible"
    );
    assert!(c.positions().open_positions().is_empty());

    // Global capacity ceiling: also outside the entry gates.
    let mut c2 = core_with_declared_dips(
        HashMap::new(),
        &[("all_in", &["BTC"])],
        0,
        WINDOW_SHUT,
        |_| crate::strategies::GateExemptions::all(),
    );
    feed_three_asset_dip(&mut c2, now);
    assert_eq!(
        c2.engine_evaluate(now + 1_000),
        0,
        "global capacity is not exemptible"
    );
    assert_eq!(c2.engine_stats()["placeRejected"], 1);

    // And the exemption cannot conjure a market (structural precondition).
    let mut c3 = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
        crate::strategies::GateExemptions::all()
    });
    assert_eq!(
        c3.engine_evaluate(now),
        0,
        "no round markets → nothing to exempt"
    );
}

#[test]
fn a_gate_exemption_does_not_lift_the_daily_loss_cap() {
    // Realise a loss through the ordinary exit path, then confirm the
    // exempting strategy is refused exactly like any other.
    let mut c = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
        crate::strategies::GateExemptions::all()
    });
    c.config_mut().auto_exits_enabled = true;
    c.config_mut().positions.max_daily_loss_usd = dec!(0.0001);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "the exemption still lets the entry in"
    );
    fill(&mut c, "BTC", now + 2_000);
    assert_eq!(c.positions().open_positions().len(), 1);

    // Crash the book and tick at the force-exit horizon → a realized loss.
    let token = "btc_up";
    c.book_snapshot(
        token,
        vec![(dec!(0.05), dec!(100))],
        vec![(dec!(0.06), dec!(100))],
        1_800_000 - 101_000,
    );
    c.tick(1_800_000 - 100_000).unwrap();
    assert!(
        c.positions().daily_pnl_for("default") < Decimal::ZERO,
        "expected a realized loss"
    );

    // A fresh dip on the NEXT round: the exempted strategy is refused by the
    // daily-loss cap, proving the exemption stops at the entry gates.
    let now2 = 1_800_000i64;
    feed_three_asset_dip(&mut c, now2);
    let before = c.list_orders().len();
    assert_eq!(
        c.engine_evaluate(now2 + 1_000),
        0,
        "daily loss cap is not exemptible"
    );
    assert_eq!(c.list_orders().len(), before, "no new order was tracked");
}
