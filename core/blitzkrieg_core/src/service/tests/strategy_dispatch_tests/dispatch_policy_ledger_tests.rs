use super::*;

#[test]
fn closed_trade_lands_in_the_strategy_ledger() {
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 1);
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "maker entry rests until the book crosses"
    );

    // Fill the resting bid (ask 0.42 crosses the 0.43 bid), then push a
    // +100%-ish book so the exit engine closes the position at a profit.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "crossing ask must fill the maker entry"
    );
    c.book_snapshot(
        "up",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.97), dec!(100))],
        now + 14_000,
    );
    c.tick(now + 14_200).unwrap();
    // #496: the maker-first TP exit rests at 0.96 (one tick above the
    // bid) and the bid rising into it is the honest fill that closes.
    c.book_snapshot(
        "up",
        vec![(dec!(0.96), dec!(100))],
        vec![(dec!(0.98), dec!(100))],
        now + 15_000,
    );
    c.tick(now + 15_200).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "profit target must close the position"
    );

    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["closedTrades"], 1);
    assert_eq!(s["wins"], 1);
    assert_eq!(s["losses"], 0);
    assert!(
        dec_of(&s["netPnlUsd"]) > Decimal::ZERO,
        "expected positive realized PnL"
    );
    // E17: the entry rests on the book and is hit (maker), and the profit
    // target rests at the bid and is hit (maker), so this fixture pays NO
    // fee. Before the fix the entry was charged the taker rate while the
    // record claimed maker — that phantom fee is what made "fees > 0" pass,
    // and it is exactly the drift E17 removed. Pin the equality that
    // actually matters: the fee total IS the record's, and the PnL IS cash.
    let rec = &c.positions().closed_positions()[0];
    let rec_fees = (rec.entry_fee_pct / Decimal::ONE_HUNDRED) * rec.entry_price * rec.shares
        + (rec.exit_fee_pct / Decimal::ONE_HUNDRED) * rec.exit_price * rec.shares;
    assert_eq!(
        rec_fees,
        Decimal::ZERO,
        "both legs rested on the book, so nothing was charged"
    );
    assert_eq!(
        dec_of(&s["feesUsd"]),
        rec_fees,
        "the ledger's fee total must equal the trade record's, not exceed it"
    );
    assert_eq!(
        dec_of(&s["netPnlUsd"]),
        rec.net_pnl_usd,
        "strategy PnL must be the cash the ledger moved"
    );
    assert_eq!(
        dec_of(&s["openNotionalUsd"]),
        Decimal::ZERO,
        "closed position leaves no exposure"
    );
}

/// The cash ledger must equal `seed + Σ netPnlUsd` once nothing is reserved
/// and no position is open — the identity #75 set out to establish, and the
/// E17 acceptance criterion.
///
/// How this fixture used to fail (drift 0.0817722), for the record. A
/// `MakerThenTaker` entry rests at the bid, gets hit, and its profit target
/// exits as a maker; three places disagreed about what kind of fill happened:
///
/// 1. ENTRY — the position recorded `was_maker_entry = true` and a 0 entry
///    fee, while `apply_delta_effects` read the escalated order's
///    `mode = MakerThenTaker` and charged the TAKER fee. Cash sat BELOW the
///    record by that fee.
/// 2. EXIT — the closing delta covered 9.99 of the 10 shares (the old flat
///    0.01 share buffer), so cash received 0.01 × exit_price less proceeds
///    than the record's gross assumed.
/// 3. EXIT — the record re-derived `was_maker_exit = false` and charged a
///    taker exit fee, while the delta's `mode = Maker` charged none. Cash sat
///    ABOVE the record by that fee.
///
/// The E17 fix removes the disagreement rather than the symptom: the fee now
/// follows `FillDelta.role` (resolved from the fill), the position accrues the
/// fee the ledger actually charged, the flat share buffer is gone (exits floor
/// to the 0.01 share grid), and a sub-grid remainder is written off inside
/// `net`. See `MIGRATION_LOG.md`.
#[test]
fn dry_balance_is_seed_plus_realized_net() {
    const SEED: Decimal = dec!(1000);
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 1);
    // Fill the resting bid, then mark up so the profit target closes it.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    c.book_snapshot(
        "up",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.97), dec!(100))],
        now + 14_000,
    );
    c.tick(now + 14_200).unwrap();
    // #496: the TP exit rests at 0.96; the bid rising into it fills it.
    c.book_snapshot(
        "up",
        vec![(dec!(0.96), dec!(100))],
        vec![(dec!(0.98), dec!(100))],
        now + 15_000,
    );
    c.tick(now + 15_200).unwrap();
    assert_eq!(c.positions().open_positions().len(), 0);
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);

    let net: Decimal = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| p.net_pnl_usd)
        .sum();
    assert!(
        net > Decimal::ZERO,
        "the fixture is meant to close at a profit, got {net}"
    );
    let parts: Vec<String> = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| {
            let ef = (p.entry_fee_pct / Decimal::ONE_HUNDRED) * p.entry_price * p.shares;
            let xf = (p.exit_fee_pct / Decimal::ONE_HUNDRED) * p.exit_price * p.shares;
            format!(
                "id={} entry={} exit={} shares={} mkEntry={} mkExit={} entryFee={} exitFee={} gross={} net={}",
                p.id, p.entry_price, p.exit_price, p.shares, p.was_maker_entry,
                p.was_maker_exit, ef, xf, p.pnl_usd, p.net_pnl_usd
            )
        })
        .collect();
    assert_eq!(
        c.ledger().balance(),
        SEED + net,
        "cash must equal principal + realized net once nothing is open; closed={parts:?}"
    );
}

// ── #363: the execution-policy verdict seam, wired into the pipeline ────

/// A dip-buying core with an operator-supplied policy. The point of the
/// seam: a policy verdict happens BEFORE arbitration, so the RiskGate
/// never sees a refused intent (the policy may remove, never relax),
/// and the refusal is bucketed `policy.skip`.
fn policy_core(mut c: Core, policy: crate::execution_policy::Policy) -> Core {
    c.execution_policy = Some(policy);
    c
}

// ── #380: the boot-time policy-path contract ────────────────────────────

/// A scratch tree with the shipped policy file at
/// `<root>/user_layer/configs/execution_policy.toml`, carrying ONE
/// identifying rule (the backstop) so a loaded file is distinguishable
/// from the neutral kernel by `execution_policy_loaded()` alone.
fn policy_tree_with_backstop(root: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bk380-boot-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        root.file_name().and_then(|s| s.to_str()).unwrap_or("t")
    ));
    std::fs::create_dir_all(dir.join("user_layer").join("configs")).expect("config tree");
    let file = dir
        .join("user_layer")
        .join("configs")
        .join("execution_policy.toml");
    std::fs::write(
        &file,
        r#"
version = 1

[defaults]
budget_ratio = 0.10
min_budget_usd = 1.00
max_budget_usd = 50.00
min_equity_usd = 1.00
max_positions_per_asset = 1

[[defaults.rules]]
name = "global_backstop"
enabled = true
priority = 100
when = "open_positions >= 2"
then = { action = "skip" }
"#,
    )
    .expect("write policy fixture");
    file
}

/// #380: an EXPLICIT policy path is honoured verbatim by the boot load —
/// a file written anywhere the cwd's spelling names is loaded, with the
/// file's own rule in force. (The install-root resolution itself is
/// unit-tested in `execution_policy.rs`; this pins the SERVICE's half:
/// the path the config carries is the path the boot actually reads.)
#[test]
fn boot_loads_the_explicit_policy_path_verbatim() {
    let tree = std::env::temp_dir().join(format!("bk380-explicit-{}", std::process::id()));
    let file = policy_tree_with_backstop(&tree);
    let cfg = CoreConfig {
        execution_policy_path: Some(file.to_string_lossy().into_owned()),
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::new(cfg);
    assert!(
        c.execution_policy_loaded(),
        "the explicit file exists: the boot must load it"
    );
    // The loaded policy is the FILE's, not the neutral kernel: the
    // backstop rule the fixture wrote is in force.
    let section = c
        .execution_policy_section_view("default")
        .expect("the file's [defaults] section");
    let rules = section.rules.expect("the file's backstop rule");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["name"], serde_json::json!("global_backstop"));
    let _ = std::fs::remove_dir_all(&tree);
}

/// #380: the DEFAULT spelling absent everywhere is the zero-config
/// deployment — the boot loads the NEUTRAL kernel (code defaults, no
/// rules, no account sections). This is the exact behaviour the /tmp A/B
/// in the issue saw from a cwd without the file; what changes in #380 is
/// that the BINARY says so (WARN with the searched directories) instead
/// of degrading silently. The kernel side is unchanged: a missing file
/// loads `Policy::default()` — the same `Some` shape a zero-config boot
/// always produced, bit for bit.
#[test]
fn boot_with_the_default_spelling_missing_is_the_neutral_kernel() {
    // A tree with no policy file anywhere above it — the default spelling
    // (`user_layer/configs/...`) resolves to nothing there.
    let bare = std::env::temp_dir().join(format!("bk380-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&bare).expect("bare dir");
    let resolved =
        crate::execution_policy::resolve_default_config_path(Some(&bare.join("bin")), Some(&bare));
    assert_eq!(
        resolved.source,
        crate::execution_policy::DefaultPathSource::Fallback,
        "the premise: nothing found in the bare tree"
    );
    let cfg = CoreConfig {
        // What `main.rs` stores after a fallback resolution: the
        // exe-root-relative spelling, pointing at a file that does not
        // exist here.
        execution_policy_path: Some(resolved.path.to_string_lossy().into_owned()),
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::new(cfg);
    // The boot SUCCEEDS (#372 semantics: missing = zero-config, never a
    // refusal), and the policy in force is the NEUTRAL kernel: no rules
    // anywhere, the code defaults exactly.
    let policy = c.execution_policy.as_ref().expect("#363: always Some");
    assert!(policy.defaults.rules.is_empty(), "neutral: no rules");
    assert!(policy.accounts.is_empty(), "neutral: no account sections");
    assert_eq!(
        policy.defaults.budget_ratio,
        crate::execution_policy::code_defaults().budget_ratio,
        "the neutral budget ratio, not a file's"
    );
    assert_eq!(
        c.execution_policy_section_view("default").unwrap().rules,
        Some(Vec::new()),
        "the section view reports an EMPTY rule list for the neutral kernel"
    );
    let _ = std::fs::remove_dir_all(&bare);
}

/// The compiled default: no rules, budget unbound — the zero-config
/// kernel. (The `open_positions >= 2 → skip` backstop lives in the
/// SHIPPED FILE, not here.)
fn neutral_policy() -> crate::execution_policy::Policy {
    crate::execution_policy::Policy::default()
}

/// A place() request for the dip core's book (taker at the given price),
/// naming an account (or a blank one for the missing-id test).
fn policy_req(
    price: Decimal,
    size: Decimal,
    key: &str,
    account: &str,
) -> crate::model::OrderRequest {
    crate::model::OrderRequest {
        token_id: "btc_up".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "dip".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::AccountId::from(account),
    }
}

#[test]
fn policy_skip_refuses_the_entry_before_the_risk_gate() {
    // The shipped file's backstop (open_positions >= 2 → skip) can only
    // bite at 2+ positions, so here judge the STRUCTURAL facts instead:
    // a policy whose ONLY content is a skip-everything rule.
    let mut policy = neutral_policy();
    policy.defaults.rules = vec![crate::execution_policy::Rule {
        name: "sit_out".into(),
        priority: 1,
        enabled: true,
        when: crate::execution_policy::When {
            field: crate::execution_policy::ConditionField::OpenPositions,
            op: crate::execution_policy::CompareOp::Ge,
            value: crate::execution_policy::ConditionValue::Num(Decimal::ZERO),
        },
        then: crate::execution_policy::Action {
            action: Some(crate::execution_policy::SkipAction::Skip),
            budget_ratio: None,
            min_budget_usd: None,
            max_budget_usd: None,
            cooldown_sec: None,
        },
        reason: Some("测试：一票否决".into()),
    }];
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), policy);
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "policy skip removes the order"
    );
    assert!(c.list_orders().is_empty(), "nothing reached place()");
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "dip");
    assert_eq!(s["rejectionCauses"]["policy.skip"], 1, "{s}");
}

/// The negative half of "规则不能绕过 RiskEngine": with the policy
/// REMOVED (its Place path), an order that the risk cap rejects is STILL
/// rejected — a policy budget never widens anything. Here the dip ticket
/// (size_usd 2.5 at 0.44 → 5 shares → 2.20 USD) sails under the default
/// 1000 notional cap, so the assertion pins the budget does not RAISE
/// the ticket: the engine's own band decided 5 shares and the policy
/// (neutral, budget = full balance) leaves it exactly alone.
#[test]
fn policy_place_leaves_the_ticket_untouched_when_neutral() {
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), neutral_policy());
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    let orders = c.list_orders();
    assert_eq!(orders.len(), 1);
    assert_eq!(
        orders[0].size,
        dec!(6),
        "2.5/0.44 → 6 shares, unchanged by the neutral policy"
    );
}

/// "account_id 缺失 → 拒绝下单": an intent with a blank account id is
/// refused (never silently judged under the defaults) and the refusal
/// is visible in the strategy's rejection causes.
#[test]
fn a_missing_account_id_refuses_the_order() {
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), neutral_policy());
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    // Blank the account id on the produced intent before the verdict is
    // consulted: exactly what a mis-wired strategy host would send.
    // The verdict map keys on the SAME id, so the lookup itself must
    // refuse — drive it through a custom request via place_gated is
    // simpler: place() with an empty account id.
    assert!(c.list_orders().is_empty(), "engine_evaluate not run yet");
    // place() path: the intent carries a BLANK account id. The policy's
    // own refusal must reject it (never silently judge under defaults).
    let r = c.place(
        policy_req(dec!(0.44), dec!(5), "policy-missing-account", ""),
        0,
        now + 1_000,
    );
    assert!(r.is_err(), "an empty account id must refuse: {r:?}");
}

/// The per-asset cap reads the policy file: `max_positions_per_asset = 2`
/// admits a SECOND same-asset position (UP + DOWN of one asset — the
/// @Almach replay's shape) where the compiled 1 would refuse, and a third
/// same-asset entry is refused with the exact historical vocabulary.
#[test]
fn per_asset_cap_comes_from_the_policy_section() {
    let now = 1_000_000i64;
    // Boot-time resolution: CoreConfig's explicit value wins over any
    // policy file (which this test disables — the CONFIG path is what
    // the flag exercises).
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        size_usd: dec!(2.5),
        min_shares: dec!(1),
        max_shares: dec!(1000),
        max_positions_per_asset: Some(2),
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
    };
    cfg.execution_policy_path = None; // no file in unit tests
    let mut c = Core::new(cfg);
    c.set_balance(dec!(100));
    // Books for BOTH of BTC's sides — two positions on ONE asset.
    for side in ["up", "down"] {
        c.engine_on_data(
            DataEvent::Book {
                token_id: format!("btc_{side}"),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
    }
    let leg = |token: &str, key: &str| {
        let mut r = policy_req(dec!(0.45), dec!(5), key, "default");
        r.token_id = token.to_string();
        r
    };
    // Two same-ASSET entries on DIFFERENT tokens, both admitted and filled.
    let a = c.place(leg("btc_up", "k1"), 0, now);
    assert!(a.is_ok(), "first same-asset entry: {a:?}");
    let b = c.place(leg("btc_down", "k2"), 0, now);
    assert!(
        b.is_ok(),
        "second same-asset entry admitted at cap 2: {b:?}"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "the fixture's premise: two BTC positions actually opened"
    );
    // A THIRD same-asset entry (a new token, same BTC) is refused at the
    // cap, with the exact historical vocabulary ("Already in {asset}")
    // that classify_rejection buckets as positions.already_in.
    let r = c.place(leg("btc_another", "k3"), 0, now);
    let reason = match r {
        Err(e) => e.to_string(),
        Ok((_, status)) => {
            panic!("third entry must not be admitted at the cap (status {status:?})")
        }
    };
    assert!(reason.contains("Already in BTC"), "got: {reason}");
}

/// A policy cooldown: the winning rule puts the ACCOUNT on ice; the
/// very next entry is refused with `policy.skip` until the clock runs
/// out, then entries resume — the caller owns the clock.
#[test]
fn a_cooldown_rule_ices_the_account_then_thaws_it() {
    let mut policy = neutral_policy();
    // The rule arms while the account is funded (balance >= 100): the
    // triggering cycle consumes the entry AND starts the 60s ice. Later
    // the balance is drawn down so the rule no longer matches — proving
    // the THAW comes from the parked clock expiring, not from a fresh
    // rule hit re-icing the account every cycle.
    policy.defaults.rules = vec![crate::execution_policy::Rule {
        name: "ice".into(),
        priority: 1,
        enabled: true,
        when: crate::execution_policy::When {
            field: crate::execution_policy::ConditionField::AvailableBalance,
            op: crate::execution_policy::CompareOp::Ge,
            value: crate::execution_policy::ConditionValue::Num(dec!(100)),
        },
        then: crate::execution_policy::Action {
            action: None,
            budget_ratio: None,
            min_budget_usd: None,
            max_budget_usd: None,
            cooldown_sec: Some(60),
        },
        reason: None,
    }];
    let mut c = policy_core(equity_core(dec!(100), Decimal::ZERO), policy);
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    // First cycle: the cooldown verdict parks the account — the intent
    // that TRIGGERED it is consumed (the rule hit on its facts).
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "cooldown verdict consumes the triggering entry"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "dip");
    assert_eq!(s["rejectionCauses"]["policy.skip"], 1, "{s}");
    // Second cycle, still inside 60s: refused by the parked cooldown —
    // and note the rule is now OFF (balance drawn below 100), so this
    // refusal is the parked clock talking, not a fresh rule hit.
    c.set_balance(dec!(50));
    feed_btc_dip(&mut c, now + 2_000);
    assert_eq!(c.engine_evaluate(now + 3_000), 0, "account is on ice");
    // After the clock runs out: the entry passes (the rule is off and the
    // ice has melted — the caller-owned clock ran out).
    feed_btc_dip(&mut c, now + 61_000);
    assert_eq!(
        c.engine_evaluate(now + 62_000),
        1,
        "thawed: the entry places"
    );
}

/// The production deadlock sequence (default-enable bugfix, 2026-10):
/// a boot whose accounts.toml gives every account an EMPTY enablement
/// set hosts no live scopes; the operator's first disable tears down the
/// boot-registered default scopes, and every later global toggle reports
/// `applied == false` forever (the reply bytes lied: `found: true,
/// applied: false, reason: null`). The handler's fix trusts the SCOPED
/// write as the default account's enablement owner — this test pins the
/// exact readouts that decision consumes: after the teardown, the
/// scoped enable still lands `Ok` (the package is registered) while the
/// legacy toggle's `applied` stays false. The handler turns that Ok
/// into the honest `found/applied = true` reply.
#[test]
fn empty_set_teardown_then_enable_scoped_write_still_lands_ok() {
    let dir = std::env::temp_dir().join(format!("bk-default-enable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // Boot shape: the default scope hosts the package DISABLED (the
    // kernel enables nothing by itself).
    let default_id = crate::account::default_account_id();
    assert!(
        c.strategy_names().iter().any(|n| n == "spread_arb"),
        "the package is registered even with zero enabled scopes"
    );
    // First disable tears the flat default scope down (the production
    // first toggle).
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", false, "test", "off", 1)
        .expect("the disable lands");
    assert!(kept.is_empty() && set_now.is_empty());
    // Deadlock readout: the legacy toggle now finds NO live instance —
    // `applied == false` is exactly what gated the old mirror write.
    let (found, applied, reason) = c.set_strategy_enabled_checked("spread_arb", true);
    assert!(found, "the package registry still names it");
    assert!(
        !applied && reason.is_none(),
        "the live dispatch hosts no instance — the old gate starved here"
    );
    // The fix's authority: the scoped write still lands Ok (persist +
    // rebuild; a Lua source rebuilds ENABLED — the test adapter is a
    // non-Lua source, so the ADD phase skips the instance exactly like
    // the #537 probe2 note: the PERSIST is what survives to the next
    // boot, and that persistence is what the handler reports as the
    // honest applied=true).
    let (kept2, set_now2) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", true, "test", "on", 2)
        .expect("the scoped enable lands even after the teardown");
    assert_eq!(set_now2, vec!["spread_arb".to_string()]);
    assert!(kept2.is_empty());
    assert!(
        c.accounts
            .account(&default_id)
            .expect("default configured")
            .strategies
            .contains(&"spread_arb".to_string()),
        "the durable enablement set names it — the operator's intent \
         stuck and the next boot re-derives the scope from it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── #779: Decimal panic guards on the live order path ───────────────────
//
// `rust_decimal`'s `Mul`/`Div` PANIC on overflow and on division by zero.
// A tiny positive price or an out-of-band share count used to kill the
// trading task, leaving in-flight orders unmanaged. These tests pin the
// failure mode shut.

/// Emits ONE UP signal for its fixed token at a shared, mutable price —
/// the value the sizing paths divide by. A test flips it between an absurd
/// tiny quote and a normal one.
struct FixedPriceProbe {
    price: std::sync::Arc<std::sync::Mutex<Decimal>>,
    token: String,
    asset: String,
    condition: String,
}

impl crate::strategies::EngineStrategy for FixedPriceProbe {
    fn name(&self) -> &str {
        "fixed_price_probe"
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn gate_exemptions(&self) -> crate::strategies::GateExemptions {
        crate::strategies::GateExemptions::all()
    }
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let price = *self.price.lock().expect("price lock");
        vec![crate::signal::TradeSignal {
            strategy: "fixed_price_probe".into(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            asset: self.asset.clone(),
            direction: crate::model::SignalDirection::Up,
            token_id: self.token.clone(),
            condition_id: self.condition.clone(),
            price,
            reason: "fixed price probe".into(),
            shares: None,
            pair: None,
        }]
    }
}

fn core_with_fixed_price_probe(price: std::sync::Arc<std::sync::Mutex<Decimal>>) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        min_round_age_sec: 0,
        auto_exits_enabled: false,
        assets: vec!["BTC".into()],
        size_usd: dec!(2.5),
        min_shares: dec!(1),
        max_shares: dec!(10),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg);
    eng.register_user_strategy(
        Box::new(FixedPriceProbe {
            price,
            token: "probe-up".into(),
            asset: "BTC".into(),
            condition: "probe-cond".into(),
        }),
        "test".into(),
    )
    .expect("the probe registers");
    assert!(eng.set_strategy_enabled("fixed_price_probe", true));
    c.enable_engine(eng);
    c
}

fn feed_probe_market(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![CryptoMarket {
                asset: "BTC".into(),
                condition_id: "probe-cond".into(),
                question_id: "q".into(),
                up_token_id: "probe-up".into(),
                down_token_id: "probe-down".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: now + 900_000,
                round_slot: now / 1000 / 900 + 1,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: true,
                question: "probe".into(),
            }],
            now_ms: now,
        },
        now,
    );
}

/// PRE-FIX: `(budget_usd / req.price).floor()` in the policy `Place` arm
/// overflows `Decimal::div` — a PANIC — for a tiny positive quote.
#[test]
fn a_tiny_positive_price_is_refused_without_panicking_the_trading_task() {
    let price = std::sync::Arc::new(std::sync::Mutex::new(dec!(0.0000000000000000000000000001)));
    let mut c = policy_core(
        core_with_fixed_price_probe(price.clone()),
        crate::execution_policy::Policy::default(),
    );
    let now = 1_700_000_000_000i64;
    feed_probe_market(&mut c, now);

    // No panic; the absurd ticket is refused (no order reaches the OME).
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "an overflowing ticket is not an order"
    );
    assert!(c.list_orders().is_empty(), "nothing was placed");

    // The trading task is still alive: a normal quote sizes and places.
    *price.lock().expect("price lock") = dec!(0.40);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        1,
        "a subsequent normal order still works"
    );
    assert_eq!(c.list_orders().len(), 1);
}

/// PRE-FIX: the orphan-chase snapshot divides `p.cost_usd / p.shares` for
/// every governed naked leg; a zero-share row PANICS on division by zero.
#[test]
fn a_zero_share_orphan_is_not_divided_by_in_the_chase_snapshot() {
    let mut virtual_limits = HashMap::new();
    virtual_limits.insert(
        "pairstrat".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(1000)),
            accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
        },
    );
    let mut c = Core::new(CoreConfig {
        virtual_limits,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        ..Default::default()
    });
    let entered = 1_000_000i64;
    let p = c.positions.open(
        crate::position::OpenParams {
            strategy: "pairstrat".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "naked-up".into(),
            condition_id: "cond".into(),
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: "partner-down".into(),
            entry_price: dec!(0.40),
            expires_at_ms: entered + 60_000,
            was_maker: false,
            target_exit_price: None,
            round_duration_sec: 900,
            round_slot: 1,
        },
        entered,
    );
    assert_eq!(p.shares, Decimal::ZERO, "the premise: a zero-share row");

    // No panic; the zero-share row is not a naked leg.
    c.run_pair_orphan_sweep(entered + 10_000);
    assert_eq!(c.positions.open_positions().len(), 1);
    assert!(
        c.list_orders().is_empty(),
        "no chase/flatten order is emitted for a zero-share row"
    );
}
