use super::*;

#[test]
fn default_config_has_no_limits_and_places_the_entry() {
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        1,
        "no caps configured must not change behaviour"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 0);

    // Fill the resting maker entry so exposure is live, then read the ledger.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["enabled"], true);
    // The kernel has no `builtin` class any more (PR-B): the test adapter is
    // hosted by the test itself, and a production session would report the
    // library's `dylib:<path>` provenance here instead.
    assert_eq!(s["source"], "test");
    assert_eq!(s["ordersPlaced"], 1);
    assert_eq!(s["openPositions"], 1);
    assert_eq!(dec_of(&s["openNotionalUsd"]), dec!(3.90));
}

#[test]
fn strategy_position_cap_rejects_the_entry_and_is_counted() {
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "position cap 0 must block the entry"
    );
    assert!(
        c.ome().live_orders().is_empty(),
        "no order may reach the OME"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["limitRejected"], 1);
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(s["openPositions"], 0);
}

#[test]
fn rejection_causes_report_position_cap_bucket() {
    // E9-c: the limit rejection lands in a machine-parseable bucket next
    // to the live counters, so an operator can see WHY without logs.
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 0);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.positionCap"], 1,
        "position-cap rejection must be bucketed: {s}"
    );
    assert_eq!(
        s["ordersRejected"], 0,
        "cap rejections are not place rejections"
    );
    // A strategy that never placed has NO causes object at all.
    let clean_core = core_with_engine(HashMap::new());
    let clean_stats = clean_core.strategy_stats();
    let clean = strategy_entry(&clean_stats, "spread_arb");
    assert!(clean["rejectionCauses"].is_null());
}

#[test]
fn rejection_causes_report_per_order_cap_risk_bucket() {
    // E9-c: a Risk::check per-order-cap rejection inside place() is
    // bucketed as risk.perOrderCap (previously the reason was dropped).
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    c.risk_config_mut().max_order_notional = dec!(1);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "1 USD per-order cap must reject the 4.30 entry"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["ordersRejected"], 1);
    assert_eq!(
        s["rejectionCauses"]["risk.perOrderCap"], 1,
        "must count and bucket the risk reason: {s}"
    );
}

#[test]
fn strategy_notional_cap_blocks_below_and_passes_above_the_entry() {
    // Entry notional is 0.43 * 10 = 4.30: a 1.00 cap rejects, a 10.00 cap passes.
    let mut tight = HashMap::new();
    tight.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(1)),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(tight);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "notional cap 1.00 must block a 4.30 entry"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);

    let mut loose = HashMap::new();
    loose.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(10)),
            ..Default::default()
        },
    );
    let mut c2 = core_with_engine(loose);
    feed_entry_setup(&mut c2, now);
    assert_eq!(
        c2.engine_evaluate(now + 12_000),
        1,
        "cap above the entry notional must not block"
    );
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

/// E16/#98: the portfolio-level open-notional cap bounds the TOTAL open
/// commitment across all strategies (0 = off = shipped behaviour).
#[test]
fn portfolio_notional_cap_bounds_total_open_exposure() {
    // Entry notional is 0.43 * 10 = 4.30: a 2.00 portfolio cap rejects it
    // even though no per-strategy limit is configured at all.
    let mut c = core_with_engine(HashMap::new());
    c.risk_config_mut().max_open_notional_usd = dec!(2);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "a 4.30 entry breaches a 2.00 portfolio cap"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.portfolioNotionalCap"], 1,
        "the portfolio cap must be bucketed so tooling can answer why: {s}"
    );

    // 0 = off: the same entry goes through untouched.
    let mut c2 = core_with_engine(HashMap::new());
    feed_entry_setup(&mut c2, now);
    assert_eq!(c2.engine_evaluate(now + 12_000), 1);
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

// ── E-VA: strategy-level virtual accounts ───────────────────────────────
//
// Test 1 (strategy isolation): A's big entry must NOT consume B's
// headroom — each governed scope reserves against its OWN pot.
// Test 2 (atomic batch): a pair batch whose sum exceeds the pot places
// ZERO legs; the legs cancel together through the existing StrategyLimit
// rejection accounting.
// Test 3 (precise release): a partial fill converts exactly the filled
// shares' share of the hold; the rest stays outstanding.
// Test 4 (terminal release): a cancelled order's hold leaves the pot.
// Test 5 (bypass guard): undeclared strategy + armed pot = the historical
// path, byte-identically (no pot, no counters).

/// A dip strategy that declares `atomic_batch` and emits N equal-share
/// legs per cycle (one per configured asset) — the pair-batch shape the
/// virtual-account path exists for.
struct AtomicDip {
    name: String,
    assets: Vec<String>,
}
impl crate::strategies::EngineStrategy for AtomicDip {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    // Pair semantics: legs hold to settlement (the EngineStrategy doc's
    // complete-set rationale). Also the release posture the cancel test
    // exercises — a cancelled leg's token re-arms for a re-attempt.
    fn holds_to_settlement(&self) -> bool {
        true
    }
    fn atomic_batch(&self) -> bool {
        true
    }
    fn find_candidates(
        &mut self,
        ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.contains(&market.asset) {
                continue;
            }
            for (token_id, direction) in [
                (&market.up_token_id, crate::model::SignalDirection::Up),
                (&market.down_token_id, crate::model::SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else {
                    continue;
                };
                if book.mid_price <= dec!(0.45) {
                    out.push(crate::signal::TradeSignal {
                        strategy: self.name.clone(),
                        account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: "atomic dip".into(),
                        shares: Some(dec!(10)),
                        pair: None,
                    });
                }
            }
        }
        out
    }
}

/// A core with one enabled `AtomicDip` on the given assets (the kernel
/// ships no strategies; the test hosts its own, PR-B posture).
fn core_with_atomic_dip(virtual_cap: Option<Decimal>, assets: &[&str]) -> Core {
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "pair_dip".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
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
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(AtomicDip {
            name: "pair_dip".to_string(),
            assets: assets.iter().map(|a| (*a).to_string()).collect(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("pair_dip", true));
    c.enable_engine(eng);
    c
}

#[test]
fn virtual_quota_is_per_strategy_a_cannot_starve_b() {
    // Test 1: two strategies share one account. A holds a big RESERVATION
    // (a placed, unfilled entry); B's entry must still place — B's quota
    // is its own pot, and the REAL ledger was never touched by A's hold
    // (the pot is a risk-layer book, not cash).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    // A (pair_dip) places its 10-share leg at 0.44: 4.40 USD held.
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );

    // B (a plain dip, NOT declared atomic) places against the SAME
    // account while A's hold is outstanding: B's entry is judged on the
    // real ledger (untouched) — it places.
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "plain_b".into(),
            buy_below: dec!(0.45),
            assets: vec!["ETH".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("plain_b", true));
    // The engine is replaced wholesale — re-push the round/book state the
    // new engine needs, then evaluate B's candidate in the same cycle.
    c.enable_engine(eng);
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "eth_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 3_000,
        },
        now + 3_000,
    );
    let placed = c.engine_evaluate(now + 4_000);
    assert_eq!(
        placed, 1,
        "B's entry must place: A's virtual hold is A's own"
    );
    let stats = c.strategy_stats();
    let b = strategy_entry(&stats, "plain_b");
    assert_eq!(b["ordersPlaced"], 1, "B must not be starved: {b}");
    // A's hold is untouched by B's placement.
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );
}

#[test]
fn atomic_batch_over_cap_cancels_the_whole_batch() {
    // Test 2: two legs at 0.44 x 10 shares = 4.40 each, batch 8.80. A pot
    // of 8.00 covers leg 1 alone but not the SUM: zero legs may place.
    let mut c = core_with_atomic_dip(Some(dec!(8)), &["BTC", "ETH"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap batch must place ZERO legs (no naked first leg)"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg may reach the OME");
    // The refusal rides the EXISTING StrategyLimit accounting.
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "pair_dip");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        Decimal::ZERO,
        "a cancelled batch holds nothing"
    );

    // A pot of 20.00 fits the whole batch: both legs place.
    let mut c2 = core_with_atomic_dip(Some(dec!(20)), &["BTC", "ETH"]);
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c2.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(c2.engine_evaluate(now + 2_000), 2);
    assert_eq!(
        c2.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(8.80),
        "both legs' holds outstanding after placement"
    );
}

#[test]
fn virtual_release_is_fill_proportional() {
    // Test 3: hold 0.44 x 10 = 4.40; fill 1.2 shares → exactly 1.2 x 0.44
    // = 0.528 released, 3.872 still outstanding.
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));

    // Fill PART of the 10 resting shares via a crossing book. The identity
    // fill model takes min(remaining, crossing depth) — the ask depth of
    // 1.2 fills exactly 1.2 shares, the rest keeps resting.
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.42), dec!(1))],
        vec![(dec!(0.42), dec!(1.2))],
        now + 3_000,
    );
    let used_after = c.virtual_used(acct, "pair_dip");
    let released = dec!(4.40) - used_after;
    assert_eq!(
        used_after,
        dec!(3.872),
        "release = 1.2 x 0.44 exactly: {released}"
    );
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .any(|p| p.shares == dec!(1.2)),
        "the 1.2 filled shares are position occupancy"
    );
}

#[test]
fn cancelled_order_releases_its_virtual_hold() {
    // Test 4: a placed entry holds 4.40; cancelling it must leave the
    // pot empty (today's real-ledger release has no virtual twin — that
    // is the hole this closes).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));
    let id = c
        .ome()
        .live_orders()
        .first()
        .expect("the entry rests")
        .order_id
        .clone();
    c.cancel(&id, now + 3_000).unwrap();
    assert_eq!(
        c.virtual_used(acct, "pair_dip"),
        Decimal::ZERO,
        "the cancelled order's hold must release"
    );
    // The freed pot funds the next batch (the pre-check re-passes).
    let mut limits = HashMap::new();
    limits.insert(
        "pair_dip".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(100)),
            accounts: vec![acct.to_string()],
        },
    );
    let _ = limits; // cap unchanged; the re-place below proves the release
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.43), dec!(100))],
        vec![(dec!(0.45), dec!(100))],
        now + 4_000,
    );
    assert_eq!(
        c.engine_evaluate(now + 5_000),
        1,
        "the freed pot funds the next batch"
    );
}

// ── cross_venue_arb × E-VA: the REAL package through the kernel ─────────

/// The REAL cross_venue_arb Lua package, hosted the way `install_engine`
/// hosts one: package loaded from its worktree dir, `holds_to_settlement`
/// and `atomic_batch` stamped from the manifest, the kernel's ONE fee
/// schedule injected (the shipped legacy_quadratic — `bk.fees()` nil
/// would refuse everything). Registration rides
/// `register_user_strategy` (the `load_lua_package_inner` route minus the
/// engine plumbing), DISABLED — the test enables it explicitly, the same
/// opt-in an operator makes.
fn core_with_cross_venue_arb(virtual_cap: Option<Decimal>) -> Core {
    let package_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb");
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        min_round_age_sec: 0,
        // The pair's legs are DECLARED at the strategy's capacity (the
        // thin leg's depth); the kernel band must never clamp below it.
        min_shares: dec!(1),
        max_shares: dec!(10000),
        // The pair's two legs are TWO live positions on one asset with
        // DIFFERENT condition ids (one per venue) — the #363 per-asset
        // count must cover them, exactly as an operator running this
        // package would configure it (`--max-positions-per-asset 2`).
        max_positions_per_asset: Some(4),
        // cwd-relative logs are the historical three-copies pollution:
        // a test writes nothing to the process cwd.
        order_log_path: None,
        trade_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        positions: crate::position::PositionConfig {
            // Same shape as the per-asset count above, on the position
            // manager itself: two legs = two open positions.
            max_positions: 8,
            ..Default::default()
        },
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host_disabled(&mut eng, cfg.trend, cfg.spread_arb);
    let loaded = crate::strategy_engine::lua_loader::load_lua_package(&package_dir)
        .expect("cross_venue_arb package must load");
    assert!(
        loaded.atomic_batch,
        "the fixture premise: the manifest declares atomic_batch"
    );
    let sched = crate::exit_policy::fee_schedule();
    let adapter =
        crate::strategy_engine::lua_loader::LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
            .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
            .declare_atomic_batch(loaded.atomic_batch)
            // RFC-645 A-1: this fixture hosts the REAL arbiter package — stamp
            // the statistics declaration the production loader stamps (the
            // acceptance predicate still requires atomic_batch + the live
            // PairBook entry the fixture's evaluate produces).
            .declare_pair_book_stats(true);
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    eng.register_user_strategy(Box::new(adapter), format!("lua:{}", package_dir.display()))
        .expect("cross_venue_arb registers");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` →
    // the adapter's params bag); unconfigured ⇒ the strategy pairs
    // nothing (fail-closed). The fixture wires both legs onto the
    // DEFAULT account — the pre-v2 single-account batch shape these
    // dispatch tests judge (pot accounting, orphan sweep, naked-leg
    // degradation ride the same (account, strategy) scope as before).
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");
    c
}

/// The two-venue round the package prices: pm quotes the cheaper UP ask,
/// kx the cheaper DOWN ask. `expires_at_ms` rides the 900 s grid (the
/// #354 cadence the config runs).
fn cross_venue_markets(now: i64) -> Vec<CryptoMarket> {
    let slot = now / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    };
    vec![
        market("polymarket", "pm-cond", "pm-up", "pm-down"),
        market("kalshi", "kx-cond", "kx-up", "kx-down"),
    ]
}

/// The confirmed mapping binding the two listings into one bet (the human
/// gate: `unified_confirm`'s own ledger route — the exact path production
/// clicks).
fn confirm_cross_venue_mapping(c: &mut Core, now: i64) {
    let listing =
        |venue: blitzkrieg_market_api::unified::Venue, cond: &str, up: &str, down: &str| {
            blitzkrieg_market_api::unified::PlatformListing {
                venue,
                condition_id: cond.into(),
                up_token_id: up.into(),
                down_token_id: down.into(),
                asset: Some("bitcoin".into()),
                title: "Bitcoin Up or Down".into(),
                settlement_rules: "Binance 1m candle".into(),
                settlement_source: blitzkrieg_market_api::unified::SettlementSource::Chainlink,
                expires_at_ms: now + 900_000,
                status: blitzkrieg_market_api::unified::ListingStatus::Active,
            }
        };
    c.unified_confirm(
        "Bitcoin Up or Down",
        "uid:test",
        vec![
            listing(
                blitzkrieg_market_api::unified::Venue::Polymarket,
                "pm-cond",
                "pm-up",
                "pm-down",
            ),
            listing(
                blitzkrieg_market_api::unified::Venue::Kalshi,
                "kx-cond",
                "kx-up",
                "kx-down",
            ),
        ],
        now,
    )
    .expect("the two-listing mapping confirms");
}

/// Feed the pair books: pm up ask 0.44 / depth 600, kx down ask 0.42 /
/// depth 250 (the thin leg), the other legs mid-priced. Cross-venue pair
/// cost ≈ 0.875 + fees ≤ 0.98 → the package fires both legs.
fn feed_cross_venue_books(c: &mut Core, now: i64) {
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
    book("pm-up", dec!(0.44), dec!(600));
    book("pm-down", dec!(0.50), dec!(600));
    book("kx-up", dec!(0.48), dec!(9000));
    book("kx-down", dec!(0.42), dec!(250));
}

#[test]
fn cross_venue_arb_over_pot_refuses_the_whole_pair_through_the_real_package() {
    // Two legs at 0.44 / 0.42 × 250 declared shares ≈ 215 notional. A pot
    // of 200 covers neither pair whole... it refuses the WHOLE batch:
    // zero legs reach the OME, and the refusal rides the existing
    // StrategyLimit accounting (`limit.virtualCap`, one per leg).
    let mut c = core_with_cross_venue_arb(Some(dec!(200)));
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap pot must place ZERO legs through the real package"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg reaches the OME");
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "cross_venue_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "cross_venue_arb"),
        Decimal::ZERO,
        "a refused pair holds nothing"
    );

    // The same books under a pot that fits: BOTH legs place — the
    // acceptance twin proving the refusal above was the cap, not the
    // package. The package quotes at its own asks, so each leg's
    // MakerThenTaker POST crosses and the leg lands inside its own place
    // call: the pot's holds convert to position occupancy within the
    // cycle, and the committed 215 reads off the POSITION book (the pot's
    // own `batch_fits` arithmetic: used + committed_elsewhere).
    let mut c2 = core_with_cross_venue_arb(Some(dec!(1000)));
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c2, now);
    feed_cross_venue_books(&mut c2, now + 1_000);
    assert_eq!(c2.engine_evaluate(now + 2_000), 2, "both legs place");
    let committed: Decimal = c2
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.cost_usd)
        .sum();
    assert_eq!(
        committed,
        dec!(215),
        "250 × (0.44 + 0.42) committed as ONE ticket"
    );
    assert_eq!(
        c2.positions().open_positions().len(),
        2,
        "the pair is whole: one position per venue leg"
    );
}

/// 套利合计字段（前端消费契约，PR #577）：arbNetPnlUsd / arbWins /
/// arbLosses / arbClosedTrades / arbActivePairs —— 只有 cross_venue_arb
/// 的行携带；无论账户切片选哪个账户，五个字段都读套利单元的全局合计
/// （双腿平仓各记在 (cross_venue_arb, 各腿账户) 键下，全局 fold 是和）。
/// 非套利行不带 arb* 键 —— 前端按行内有无字段 feature-detect。
#[test]
fn arb_totals_ride_every_account_slice_of_the_arb_row_only() {
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    // A≠B 的腿配置（跨所配对的声明闸要求两腿盖章到不同账户）：套利
    // 单元 = 实例主账户 default + 腿账户 arb-b。arb-b 是内存账簿的新
    // 账户（set 校验要求账户已配置）。
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("arb-b"),
            name: "arb-b".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        })
        .expect("arb-b does not exist yet");
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("arb-b"))
        .expect("arb-b just inserted")
        .set_balance(dec!(1000));
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    })
    .expect("the A≠B leg configuration lands");
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(c.engine_evaluate(now + 2_000), 2, "both legs place");
    // 声明受理：跨所配对真的进了配对簿（在册读数的前提）。
    assert_eq!(
        c.pair_book_all().len(),
        1,
        "the A≠B pair declaration lands in the book"
    );

    // 两腿平仓：生产 close 路径（close → on_position_closed），净额一
    // 正一负，wins/losses 各记一笔；合计字段读全局 fold = 两腿之和
    // （含各腿真实手续费 —— 合计字段是 fold 的别名，不是手抄数）。
    let legs: Vec<crate::position::OpenPosition> = c.positions().open_positions().to_vec();
    assert_eq!(legs.len(), 2, "fixture premise: the pair is open");
    let mut net_sum = Decimal::ZERO;
    for held in &legs {
        // 价差驱动平仓价：正腿 +0.40/股，负腿 −0.30/股。
        let delta = if held.entry_price < dec!(0.43) {
            dec!(0.40)
        } else {
            dec!(-0.30)
        };
        let closed = c
            .positions
            .close(
                &held.id,
                held.entry_price + delta,
                crate::model::ExitReason::TakeProfit,
                false,
                1,
            )
            .expect("the leg closes through the production close path");
        net_sum += closed.net_pnl_usd;
        c.on_position_closed(&closed, 1);
    }
    assert!(net_sum.is_sign_positive(), "the fixture books a net profit");

    // ── 全局行：arb* = 该行 fold 值（双腿各账户键之和）──────────────
    let global = c.strategy_stats();
    let g = strategy_entry(&global, "cross_venue_arb");
    assert_eq!(
        dec_of(&g["arbNetPnlUsd"]),
        net_sum,
        "the arb row's total is both legs' sum: {g}"
    );
    assert_eq!(g["arbWins"], serde_json::json!(1));
    assert_eq!(g["arbLosses"], serde_json::json!(1));
    assert_eq!(g["arbClosedTrades"], serde_json::json!(2));
    assert_eq!(
        g["arbActivePairs"],
        serde_json::json!(1),
        "the pair sits in the book until the round rolls: {g}"
    );
    assert_eq!(
        dec_of(&g["netPnlUsd"]),
        net_sum,
        "the plain counter and the arb total agree on the global row"
    );

    // ── 账户切片行：arb* 仍读全局合计；平计数是该账户的腿切片 ──
    // （一正一负 ⇒ 任一切片都只看得见自己那腿的胜负，但 arb* 都是 1/1。）
    for account in [blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "arb-b"] {
        let sliced = c.strategy_stats_for_account(account);
        let s = strategy_entry(&sliced, "cross_venue_arb");
        assert_eq!(
            dec_of(&s["arbNetPnlUsd"]),
            net_sum,
            "every account's page shows the arb unit's combined take ({account}): {s}"
        );
        assert_eq!(s["arbWins"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbLosses"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbClosedTrades"], serde_json::json!(2), "{account}");
        assert_eq!(s["arbActivePairs"], serde_json::json!(1), "{account}");
        // 切片的平计数是本账户腿的（两腿账户不同 ⇒ 各自 1 笔）。
        assert_eq!(
            s["closedTrades"],
            serde_json::json!(1),
            "the slice's plain counters stay per-account ({account}): {s}"
        );
    }

    // ── 非套利行不带 arb* 键 ──
    let g_plain = strategy_entry(&global, "spread_arb");
    assert!(
        g_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals: {g_plain}"
    );
    let s_plain_slice = c.strategy_stats_for_account("arb-b");
    let s_plain = strategy_entry(&s_plain_slice, "spread_arb");
    assert!(
        s_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals on a slice either: {s_plain}"
    );
}

#[test]
fn pair_orphan_sweep_flattens_a_naked_leg_after_the_grace_window() {
    // The compensation watch: leg ONE lands, leg TWO's order dies before
    // it can (the starved ledger below is one real way — the #457 expiry
    // sweep, a manual withdraw and a venue reject all share the same
    // terminal shape), and after the 5 s landing grace the LIVE tick sweep
    // alerts and flattens the naked leg.
    //
    // Device: the ledger runs dry between the legs. pm-up places and
    // FILLS inside its own place call (the package prices at its own ask,
    // so the MakerThenTaker POST crosses and escalates); kx-down's 105
    // reserve then exceeds what the book has left and its place dies —
    // a leg that never rested, never filled, never will.
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    {
        // Drain the default book to 110: pm-up (0.44 × 250 = 110) just
        // fits; kx-down (0.42 × 250 = 105) does not.
        c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(c.engine_evaluate(now + 2_000), 1, "only leg one lands");

    let held: Vec<String> = c
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.token_id.clone())
        .collect();
    assert_eq!(
        held,
        vec!["pm-up".to_string()],
        "pm-up filled, kx-down never placed"
    );

    // Inside the grace window the sweep holds its hand (the two legs of
    // one batch land serially inside one evaluate cycle).
    c.tick(now + 4_000).expect("grace-window tick");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the grace window protects the just-filled leg"
    );

    // Past the window, the LIVE tick sweep flattens the naked leg. The
    // alert names the SAME-market partner token (`partner_of` stamps the
    // condition's other outcome — the dry settlement ladder's key), not
    // the cross-venue leg that died.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().any(|m| m.contains("pair leg orphaned")
            && m.contains("cross_venue_arb")
            && m.contains("partner_missing=pm-down")),
        "the orphan alert must name the strategy and the missing leg: {alerts:?}"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the naked leg is flattened: remaining = {:?}",
        c.positions()
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn pair_orphan_sweep_leaves_ungoverned_strategies_alone() {
    // The zero-config guard: the SAME naked-leg shape (one fill, the
    // partner leg dead) under a strategy the pot book does NOT govern
    // must see ZERO alerts and ZERO flatten orders — orphan compensation
    // is a governed-atomic-scope posture, never a global behaviour.
    let mut c = core_with_cross_venue_arb(None);
    {
        // Same starved-ledger device as the governed test: pm-up lands,
        // kx-down's place dies on the reserve.
        c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        1,
        "leg one lands the same way"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "leg one fills the same way"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().all(|m| !m.contains("pair leg orphaned")),
        "an ungoverned scope is invisible to the orphan sweep: {alerts:?}"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the naked leg of an ungoverned strategy stays in the book"
    );
    assert!(
        c.ome()
            .live_orders()
            .into_iter()
            .all(|o| o.side != Side::Sell),
        "no flatten SELL was spawned"
    );
}

#[test]
fn undeclared_strategy_keeps_the_per_order_path_byte_identically() {
    // Test 5 (bypass guard): a strategy that did NOT declare
    // `atomic_batch` but IS armed in virtual_limits (and one that is
    // neither) must take the historical path: no pot, no reservation,
    // no new counters — the same entry the default core places.
    // Reference run: NO virtual layer at all.
    let mut reference = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut reference, now);
    let ref_placed = reference.engine_evaluate(now + 2_000);
    let ref_stats = reference.strategy_stats();
    let ref_row = strategy_entry(&ref_stats, "plain").clone();

    // Armed pot, undeclared strategy: same outcome, empty pot.
    let mut limits = HashMap::new();
    limits.insert(
        "plain".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(1)),
            accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
        },
    );
    let mut armed = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    armed.set_virtual_limits_for_test(limits);
    feed_three_asset_dip(&mut armed, now);
    assert_eq!(
        armed.engine_evaluate(now + 2_000),
        ref_placed,
        "an undeclared strategy ignores the pot entirely"
    );
    let armed_stats = armed.strategy_stats();
    let armed_row = strategy_entry(&armed_stats, "plain");
    assert_eq!(
        armed_row["ordersPlaced"], ref_row["ordersPlaced"],
        "byte-identical placement outcome"
    );
    assert!(
        armed_row["rejectionCauses"].is_null()
            || armed_row["rejectionCauses"] == ref_row["rejectionCauses"],
        "no new rejection bucket may appear: {armed_row}"
    );
    assert_eq!(
        armed.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "plain"),
        Decimal::ZERO,
        "the pot must stay empty — reservations are the declared path only"
    );
}
