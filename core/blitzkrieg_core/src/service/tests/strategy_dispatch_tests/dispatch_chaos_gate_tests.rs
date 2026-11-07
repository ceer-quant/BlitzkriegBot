#![cfg(test)]
//! Issue #965 acceptance: fault injection (乱发单) under the env-armed posture —
//! one `.env` arming BOTH layers the way a boot does (policy via `POLICY_RULES`,
//! systemic via `from_lookup` over `BK_RISK_*`), caps held, refusals attributed.

use super::*;

/// Issue #965 acceptance (故障注入 / 乱发单): a strategy gone haywire under
/// the env-armed posture. ONE scratch `.env` carries the whole posture — the
/// policy backstop (`POLICY_RULES`, the only way a boot accepts it since #967
/// retired the toml layer) and the systemic caps (`BK_RISK_*`). The test arms
/// them exactly the way production boots: the policy through
/// `env_file_path` → `load_single_source` (the toml path is MISSING — the
/// fresh-deployment shape #965 bootstraps a template for), the systemic
/// matrix through `from_lookup`, `from_env`'s pure seam.
///
/// The haywire assertions: concurrent positions never exceed the policy cap,
/// every entry is shrunk to the single-loss cap's approved size, the refused
/// intents land in the `policy.skip` attribution bucket (never a silent
/// pass), and no order ever reaches the OME beyond the caps.
#[test]
fn chaos_entries_under_env_armed_policy_and_systemic_caps_hold_the_line() {
    let dir = std::env::temp_dir().join(format!("bk-chaos-965-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    // The whole posture in one file, spelled exactly as docs/.env.example
    // documents it.
    std::fs::write(
        &env_path,
        "POLICY_RULES=[{\"name\":\"max_two\",\"priority\":100,\"enabled\":true,\
         \"when\":\"open_positions >= 2\",\"then\":{\"action\":\"skip\"}}]\n\
         BK_RISK_MAX_POSITION_SIZE=100\n\
         BK_RISK_MAX_SINGLE_LOSS_USD=0.15\n",
    )
    .unwrap();

    // The boot chain, exactly as the launcher wires it: the systemic matrix
    // resolves from the same file's BK_RISK_* keys.
    let on_file: HashMap<String, String> = std::fs::read_to_string(&env_path)
        .unwrap()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    let systemic =
        crate::risk::limits::SystemicRiskLimits::from_lookup(|k| on_file.get(k).cloned());
    assert!(
        systemic.any_armed(),
        "the same .env must arm the systemic layer (one file, both layers)"
    );

    let mut c = Core::new(CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        // Missing on purpose: the fresh-deployment shape issue #965
        // bootstraps a template for. The env-only loader never reads it.
        execution_policy_path: Some(
            dir.join("user_layer/configs/execution_policy.toml")
                .to_string_lossy()
                .into_owned(),
        ),
        risk: RiskConfig {
            max_order_notional: dec!(100),
            systemic,
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        min_shares: dec!(1),
        max_shares: dec!(100),
        assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        positions: crate::position::PositionConfig {
            max_positions: 10,
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
            name: "dip".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("dip", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for asset in ["BTC", "ETH", "SOL"] {
        let token = format!("{}_up", asset.to_lowercase());
        c.engine_on_data(
            DataEvent::Book {
                token_id: token,
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 1_000,
            },
            now + 1_000,
        );
    }

    // Cycle 1 — three haywire intents at a 0.44 mid:
    // - the single-loss cap (0.15 at the 12% bound stop → 0.44 × 0.12 =
    //   0.0528 per share) approves exactly 2 shares per entry
    //   (floor(0.15 / 0.0528) = 2): every placed order is shrunk to 2;
    // - the policy backstop allows the first two intents and skips the
    //   third (cycle-honest count: 0 open + 2 placed ≥ 2).
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 2,
        "the backstop allows exactly 2 of the 3 haywire intents"
    );
    for order in c.ome().live_orders() {
        assert_eq!(
            order.size,
            dec!(2),
            "the single-loss cap shrinks every entry to 2 shares, never the budget-sized 100+"
        );
    }

    // Fill both resting entries with crossing books: two LIVE positions.
    for asset in ["btc", "eth"] {
        c.book_snapshot(
            &format!("{asset}_up"),
            vec![(dec!(0.38), dec!(100))],
            vec![(dec!(0.39), dec!(100))],
            now + 3_000,
        );
    }
    let open = c.positions().open_positions();
    assert_eq!(open.len(), 2, "both entries filled: the chaos is live");
    // The approved size is deterministic (floor(0.15 / (0.44 × 0.12)) = 2), so
    // the assertion needs no per-position value in its message — CodeQL's
    // cleartext-logging rule rightly keeps financial data out of logs, even
    // test ones.
    assert!(
        open.iter().all(|p| p.shares <= dec!(2)),
        "no position may exceed the single-loss-approved size (2 shares at 0.44 / 12%)"
    );

    // Cycle 2 — the strategy keeps firing (乱发单): every intent is now
    // judged against 2 open positions ≥ 2 → SKIP. Nothing new may reach the
    // OME, and the refusals land in the policy.skip bucket.
    for asset in ["BTC", "ETH", "SOL"] {
        let token = format!("{}_up", asset.to_lowercase());
        c.engine_on_data(
            DataEvent::Book {
                token_id: token,
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 4_000,
            },
            now + 4_000,
        );
    }
    let placed2 = c.engine_evaluate(now + 5_000);
    assert_eq!(
        placed2, 0,
        "an armed policy backstop must refuse every entry once the cap is reached"
    );
    assert!(
        c.ome()
            .live_orders()
            .iter()
            .all(|o| o.side != crate::model::Side::Buy || o.size <= dec!(2)),
        "no oversized order anywhere"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "dip");
    assert_eq!(
        s["rejectionCauses"]["policy.skip"],
        serde_json::json!(2),
        "every refusal attributed: cycle 1's third intent (cycle-placed backstop) + \
         cycle 2's SOL intent (the armed btc/eth tokens hold their positions and do \
         not re-emit — the one-per-asset gate keeps them): {s}"
    );
    assert_eq!(s["ordersPlaced"], serde_json::json!(2));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #965 acceptance (故障注入, the refuse arm): a systemic cap NOTHING
/// fits under refuses the entry outright, and the refusal is attributed —
/// `risk.accountLimit` in the strategy stats' rejectionCauses, the same
/// vocabulary the per-order cap uses, instead of an other.other shrug.
#[test]
fn a_systemic_cap_nothing_fits_refuses_and_is_bucketed() {
    let mut c = core_with_engine(HashMap::new());
    // Boot-chain seam: from_lookup is what `SystemicRiskLimits::from_env`
    // feeds from the launcher's folded .env. 0.01 against a 0.06 per-share
    // worst case (entry 0.50, stop 12%) — even one share over-commits.
    let keys = std::collections::HashMap::from([(
        "BK_RISK_MAX_SINGLE_LOSS_USD",
        rust_decimal_macros::dec!(0.01),
    )]);
    c.risk_config_mut().systemic = crate::risk::limits::SystemicRiskLimits::from_lookup(|k| {
        keys.get(k).map(|v| v.to_string())
    });
    assert!(c.risk_config().systemic.any_armed());

    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "a cap nothing fits must refuse the entry"
    );
    assert!(
        c.ome().live_orders().is_empty(),
        "no order may reach the OME under the refusal"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["risk.accountLimit"],
        serde_json::json!(1),
        "the refusal must name the systemic cap: {s}"
    );
}
