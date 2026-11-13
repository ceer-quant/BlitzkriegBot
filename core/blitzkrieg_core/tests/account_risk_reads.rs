//! issue #1189 (PR-B) — the per-account READ path: risk limits and exit
//! discipline are resolved by the order's / the position's own `account_id`.
//!
//! The acceptance teeth (each one an observable behaviour, not a restated
//! formula):
//!
//!  1. `two_accounts_with_different_size_caps_are_judged_apart` — the SAME
//!     entry request shrinks under A's `max_position_size` and passes under
//!     B's: the submission gate judges the request's OWN row.
//!  2. `two_accounts_with_different_stop_loss_pct_valuate_apart` — two open
//!     positions, one per account, each applying its own account's stop on
//!     the same book: the exit sweep resolves the POSITION's row.
//!  3. `no_account_keys_is_byte_identical_to_the_single_matrix` — with no
//!     suffixed keys anywhere, the resolved matrix is the pre-#1189 single
//!     matrix: the same judgment, the same wire shape.
//!  4. `both_env_spellings_arm_the_account_row` — suffix
//!     (`BK_RISK_MAX_POSITION_SIZE_PAPER`) and infix
//!     (`BK_RISK_PAPER_MAX_POSITION_SIZE`) both take effect.
//!  5. `the_env_overlay_arms_the_boot_chain` — a `.env` file with a
//!     per-account key boots a core whose account row is armed while the
//!     default row stays at the global face.
//!
//! The fixtures build the account book in memory (`with_account_book`) and
//! drive the same public entry points the kernel uses (`Core::place`,
//! `PositionManager::tick`); no file is written and no process env is touched.

use blitzkrieg_core::account::{Account, AccountId, AccountLedgers, AccountStatus};
use blitzkrieg_core::exit_policy::BookView;
use blitzkrieg_core::model::{
    CoreErrorCode, ExitReason, FillPolicy, Mode, OrderRequest, OrderRole, OrderStatus,
    OrderbookSnapshot, Side, SignalDirection, default_account_id,
};
use blitzkrieg_core::position::{OpenParams, PositionConfig, PositionManager};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::risk::limits::{AccountRiskLimits, Bound, LimitSource, SystemicRiskLimits};
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_000_000_000_000;

/// One configured account, active and unbound (the venue guard is not the
/// subject here; the account's own risk row is).
fn account(id: &str) -> Account {
    Account {
        id: AccountId::from(id),
        name: id.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: AccountStatus::Active,
        credential_keys: Default::default(),
        venue: String::new(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        opening_equity: None,
        updated_at_ms: NOW,
    }
}

/// A dry core with the two-account book (default + paper), every account
/// seeded from `dry_seed_balance` (the locally-settling boot path).
fn two_account_core(risk: RiskConfig, positions: PositionConfig) -> Core {
    let book = AccountLedgers::new(
        vec![account("default"), account("paper")],
        default_account_id(),
    );
    let cfg = CoreConfig {
        mode: Mode::Dry,
        risk,
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.set_balance(dec!(1000));
    c
}

/// The order helper with an explicit account — the request the kernel judges.
fn order_for(
    account: &str,
    tok: &str,
    price: Decimal,
    size: Decimal,
    key: &str,
    now: i64,
) -> OrderRequest {
    OrderRequest {
        token_id: tok.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: now / 1000 / 900,
        account_id: AccountId::from(account),
    }
}

/// A `max_position_size`-armed matrix: the default row off, one row per
/// named account.
fn size_matrix(caps: &[(&str, Decimal)]) -> SystemicRiskLimits {
    let mut m = SystemicRiskLimits::default();
    for (id, cap) in caps {
        m.accounts.insert(
            (*id).to_string(),
            AccountRiskLimits {
                max_position_size: Bound::new(*cap, LimitSource::Env),
                ..Default::default()
            },
        );
    }
    m
}

/// Matrix where the DEFAULT row carries `cap` and only `overrides` get their
/// own row — the "missing row falls back to the global matrix" shape.
fn matrix_with_default(cap: Decimal, overrides: &[(&str, Decimal)]) -> SystemicRiskLimits {
    let mut m = SystemicRiskLimits::default();
    m.account.max_position_size = Bound::new(cap, LimitSource::Env);
    for (id, cap) in overrides {
        m.accounts.insert(
            (*id).to_string(),
            AccountRiskLimits {
                max_position_size: Bound::new(*cap, LimitSource::Env),
                ..Default::default()
            },
        );
    }
    m
}

/// Open one LONG directly in the position manager under `account`, so the
/// exit sweep has a position to resolve a row for.
fn open_long(
    pm: &mut PositionManager,
    account: &str,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    now: i64,
) -> String {
    let pos = pm.open(
        OpenParams {
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: tok.into(),
            condition_id: "cond".into(),
            entry_price: entry,
            expires_at_ms: now + 900_000,
            was_maker: false,
            target_exit_price: None,
            account_id: AccountId::from(account),
            venue: String::new(),
            partner_token_id: String::new(),
            round_duration_sec: 0,
            round_slot: 0,
        },
        now,
    );
    pm.apply_entry_fill(&pos.id, shares, entry, Decimal::ZERO, OrderRole::Taker);
    pos.id
}

// ── 1. two accounts, two size caps ──────────────────────────────────────────

#[test]
fn two_accounts_with_different_size_caps_are_judged_apart() {
    let risk = RiskConfig {
        systemic: size_matrix(&[("default", dec!(3)), ("paper", dec!(8))]),
        ..Default::default()
    };
    let mut c = two_account_core(risk, PositionConfig::default());
    // The book crosses at 0.40 for both accounts; the same 6-share request.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );

    // default's cap is 3: the request shrinks to exactly the cap and fills.
    let (_id_a, st_a) = c
        .place(
            order_for("default", "tok", dec!(0.40), dec!(6), "entry:a", NOW),
            0,
            NOW,
        )
        .expect("the default entry is accepted");
    assert_eq!(st_a, OrderStatus::Filled);
    assert_eq!(c.ome().all()[0].size, dec!(3), "default's cap shrunk 6 → 3");
    assert_eq!(
        c.positions()
            .open_positions()
            .iter()
            .find(|p| p.account_id.as_str() == "default")
            .map(|p| p.shares),
        Some(dec!(3)),
        "the position carries the shrunk size"
    );

    // paper's cap is 8: the SAME request passes whole.
    c.book_snapshot(
        "tok2",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );
    let (id_b, st_b) = c
        .place(
            order_for("paper", "tok2", dec!(0.40), dec!(6), "entry:b", NOW),
            0,
            NOW,
        )
        .expect("the paper entry is accepted");
    assert_eq!(st_b, OrderStatus::Filled);
    let orders = c.ome().all();
    let placed_b = orders
        .iter()
        .find(|o| o.order_id == id_b)
        .expect("paper's order");
    assert_eq!(placed_b.size, dec!(6), "paper's cap leaves 6 whole");
    assert_eq!(
        c.positions()
            .open_positions()
            .iter()
            .find(|p| p.account_id.as_str() == "paper")
            .map(|p| p.shares),
        Some(dec!(6)),
        "the position carries the full size"
    );
}

/// An account with NO row falls back to the default row (the legacy
/// single-matrix posture).
#[test]
fn an_account_without_a_row_falls_back_to_the_default_row() {
    let risk = RiskConfig {
        systemic: matrix_with_default(dec!(3), &[]),
        ..Default::default()
    };
    let mut c = two_account_core(risk, PositionConfig::default());
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );
    // paper has no row: it inherits default's cap of 3.
    let (_id, st) = c
        .place(
            order_for("paper", "tok", dec!(0.40), dec!(6), "entry:p", NOW),
            0,
            NOW,
        )
        .expect("the entry is accepted");
    assert_eq!(st, OrderStatus::Filled);
    assert_eq!(
        c.positions()
            .open_positions()
            .iter()
            .find(|p| p.account_id.as_str() == "paper")
            .map(|p| p.shares),
        Some(dec!(3)),
        "paper inherited default's cap"
    );
}

/// A cap below one whole share refuses on the request's own account only —
/// the sibling account is untouched.
#[test]
fn a_sub_one_share_cap_refuses_only_its_own_account() {
    let risk = RiskConfig {
        systemic: size_matrix(&[("default", dec!(0.5))]),
        ..Default::default()
    };
    let mut c = two_account_core(risk, PositionConfig::default());
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );
    let err = c
        .place(
            order_for("default", "tok", dec!(0.40), dec!(6), "entry:a", NOW),
            0,
            NOW,
        )
        .expect_err("default's sub-one-share cap refuses");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("whole share"), "{}", err.message);

    // paper has no row: the same request passes.
    c.book_snapshot(
        "tok2",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );
    c.place(
        order_for("paper", "tok2", dec!(0.40), dec!(6), "entry:b", NOW),
        0,
        NOW,
    )
    .expect("paper has no cap");
}

// ── 2. two accounts, two stop widths ────────────────────────────────────────

/// Two positions (one per account) on the SAME book path: the default
/// account's 5% stop fires where paper's 20% stop holds — the exit sweep
/// resolves the POSITION's account row, not a process-wide config.
#[test]
fn two_accounts_with_different_stop_loss_pct_valuate_apart() {
    let mut positions = PositionConfig::default();
    positions.exit.stop_loss_pct = dec!(10);
    positions.exit.stop_tighten_start_sec = 0; // the ramp must not move the base
    positions.exit.exit_grace_sec = 0;
    positions.account_exit.insert(
        "default".into(),
        blitzkrieg_core::exit_policy::ExitConfig {
            stop_loss_pct: dec!(5),
            stop_tighten_start_sec: 0,
            exit_grace_sec: 0,
            ..positions.exit.clone()
        },
    );
    positions.account_exit.insert(
        "paper".into(),
        blitzkrieg_core::exit_policy::ExitConfig {
            stop_loss_pct: dec!(20),
            stop_tighten_start_sec: 0,
            exit_grace_sec: 0,
            ..positions.exit.clone()
        },
    );
    let mut pm = PositionManager::new(positions);
    let a = open_long(&mut pm, "default", "tok-a", dec!(0.50), dec!(10), NOW);
    let b = open_long(&mut pm, "paper", "tok-b", dec!(0.50), dec!(10), NOW);

    // −7%: inside default's 5% stop, far above paper's 20% one.
    let book_a = OrderbookSnapshot::from_levels(
        "tok-a",
        vec![(dec!(0.465), dec!(1000))],
        vec![(dec!(0.47), dec!(1000))],
        NOW + 1_000,
    );
    let book_b = OrderbookSnapshot::from_levels(
        "tok-b",
        vec![(dec!(0.465), dec!(1000))],
        vec![(dec!(0.47), dec!(1000))],
        NOW + 1_000,
    );
    // The tick state (low_pnl_pct etc.) is stop-agnostic by construction:
    // `update_exit_state` tracks price history, not the stop width. The
    // observable that moves with the stop is check_exits's verdict below.

    // The exit SWEEP: default's position produces a stop request, paper's
    // does not — the sweep resolves the POSITION's account row.
    let reqs = pm.check_exits(
        &|token| match token {
            "tok-a" => Some(BookView::from_snapshot(&book_a)),
            "tok-b" => Some(BookView::from_snapshot(&book_b)),
            _ => None,
        },
        NOW + 1_000,
    );
    let reasons: Vec<_> = reqs
        .iter()
        .map(|r| (r.position_id.clone(), r.reason))
        .collect();
    assert!(
        reasons
            .iter()
            .any(|(id, reason)| *id == a && *reason == ExitReason::StopLoss),
        "default must exit on its 5% stop: {reasons:?}"
    );
    assert!(
        !reasons.iter().any(|(id, _)| *id == b),
        "paper must hold through a 7% decline on its 20% stop: {reasons:?}"
    );
}

/// An account with NO `account_exit` row uses the global config — the
/// shipped behaviour, byte for byte.
#[test]
fn an_account_without_an_exit_row_uses_the_global_config() {
    let mut positions = PositionConfig::default();
    positions.exit.stop_loss_pct = dec!(5);
    positions.exit.stop_tighten_start_sec = 0;
    positions.exit.exit_grace_sec = 0;
    let mut pm = PositionManager::new(positions);
    let id = open_long(&mut pm, "default", "tok", dec!(0.50), dec!(10), NOW);
    let book = OrderbookSnapshot::from_levels(
        "tok",
        vec![(dec!(0.465), dec!(1000))],
        vec![(dec!(0.47), dec!(1000))],
        NOW + 1_000,
    );
    let reqs = pm.check_exits(&|_| Some(BookView::from_snapshot(&book)), NOW + 1_000);
    assert!(
        reqs.iter()
            .any(|r| r.position_id == id && r.reason == ExitReason::StopLoss),
        "the global 5% stop must fire with no account row: {reqs:?}"
    );
}

// ── 3/4. the env face: suffix, infix, and the byte-identical default ────────

/// No account-suffixed key anywhere: the resolved matrix is the single
/// matrix, the account map is empty, and the wire shape is unchanged.
#[test]
fn no_account_keys_is_byte_identical_to_the_single_matrix() {
    let vars = std::collections::HashMap::from([
        ("BK_RISK_MAX_POSITION_SIZE".to_string(), "7".to_string()),
        ("BK_RISK_COOLDOWN_MINUTES".to_string(), "30".to_string()),
    ]);
    let lookup = |k: &str| vars.get(k).cloned();
    let plain = SystemicRiskLimits::from_lookup(lookup);
    let with_accounts = SystemicRiskLimits::from_lookup_with_accounts(["default", "paper"], lookup);
    assert!(with_accounts.accounts.is_empty());
    assert_eq!(plain, with_accounts);
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        serde_json::to_value(&with_accounts).unwrap()
    );
    // The default row is the one the single matrix carried.
    assert_eq!(with_accounts.account.max_position_size.value(), dec!(7));
}

/// Both spellings arm the account's own row, and the suffix spelling wins
/// when both are present (the write path's spelling is the primary).
#[test]
fn both_env_spellings_arm_the_account_row() {
    // suffix only
    let s = SystemicRiskLimits::from_lookup_with_accounts(["paper"], |k| {
        (k == "BK_RISK_MAX_POSITION_SIZE_PAPER").then(|| "5".to_string())
    });
    assert_eq!(s.account_for("paper").max_position_size.value(), dec!(5));
    // infix only
    let s = SystemicRiskLimits::from_lookup_with_accounts(["paper"], |k| {
        (k == "BK_RISK_PAPER_MAX_POSITION_SIZE").then(|| "9".to_string())
    });
    assert_eq!(s.account_for("paper").max_position_size.value(), dec!(9));
    // both present: the suffix spelling (first in the probe order) wins
    let s = SystemicRiskLimits::from_lookup_with_accounts(["paper"], |k| match k {
        "BK_RISK_MAX_POSITION_SIZE_PAPER" => Some("5".to_string()),
        "BK_RISK_PAPER_MAX_POSITION_SIZE" => Some("9".to_string()),
        _ => None,
    });
    assert_eq!(
        s.account_for("paper").max_position_size.value(),
        dec!(5),
        "the suffix spelling is the write path's, so it wins"
    );
    // the other account is untouched by paper's key
    assert!(!s.account_for("live").max_position_size.is_enabled());
}

/// The boot overlay: a `.env` file with a per-account key arms that
/// account's row in the booted core while the default row stays at the
/// global face.
#[test]
fn the_env_overlay_arms_the_boot_chain() {
    let dir = std::env::temp_dir().join(format!(
        "bk-1189-reads-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "BK_RISK_MAX_POSITION_SIZE=100\nBK_RISK_MAX_POSITION_SIZE_PAPER=5\n",
    )
    .expect("write fixture");
    let book = AccountLedgers::new(
        vec![account("default"), account("paper")],
        default_account_id(),
    );
    let cfg = CoreConfig {
        mode: Mode::Dry,
        dry_seed_balance: dec!(1000),
        env_file_path: Some(env_path.display().to_string()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::with_account_book(cfg, book);
    // The global face resolved from the file: 100 on the default row.
    assert_eq!(
        c.risk_config().systemic.account.max_position_size.value(),
        dec!(100)
    );
    // paper's own row: the suffixed key overrode the inherited 100.
    assert_eq!(
        c.risk_config()
            .systemic
            .account_for("paper")
            .max_position_size
            .value(),
        dec!(5)
    );
    // default reads its own (global) row.
    assert_eq!(
        c.risk_config()
            .systemic
            .account_for("default")
            .max_position_size
            .value(),
        dec!(100)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The exit overlay: a per-account `BK_EXIT_STOP_LOSS_PCT_<ACCOUNT>` key
/// gives that account its own row; the other accounts keep the global.
#[test]
fn the_env_overlay_arms_the_exit_row() {
    let vars = std::collections::HashMap::from([
        ("BK_EXIT_STOP_LOSS_PCT".to_string(), "10".to_string()),
        ("BK_EXIT_STOP_LOSS_PCT_PAPER".to_string(), "25".to_string()),
    ]);
    let out = blitzkrieg_core::config::account_exit_overrides(
        &blitzkrieg_core::exit_policy::ExitConfig::default(),
        ["default", "paper"],
        |k| vars.get(k).cloned(),
    );
    assert!(
        !out.contains_key("default"),
        "no row for the global-only account"
    );
    assert_eq!(out["paper"].stop_loss_pct, dec!(25));
    // The global config itself is untouched by the overlay: an account with
    // no keys of its own gets NO row and falls back at read time.
    assert!(
        blitzkrieg_core::config::account_exit_overrides(
            &blitzkrieg_core::exit_policy::ExitConfig::default(),
            ["default"],
            |k| vars.get(k).cloned(),
        )
        .is_empty()
    );
}
