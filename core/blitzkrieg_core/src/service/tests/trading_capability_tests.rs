#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal_macros::dec;

fn core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        ..Default::default()
    })
}

fn pending_req(key: &str, strategy: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.4),
        size: dec!(10),
        internal_key: key.into(),
        strategy: strategy.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn venue_err() -> crate::model::CoreError {
    crate::model::CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::NotAuthenticated,
        "auth failed",
    )
    .with_raw("401 unauthorized")
}

/// After the venue refuses an order, a retry of the SAME intent must wait
/// out its backoff locally instead of re-POSTing at full tick rate (the
/// 380+-rejection storm this regression pins). A DIFFERENT intent is
/// unaffected, and the intent opens again once its backoff expires.
#[test]
fn venue_rejection_pauses_only_that_intent() {
    let mut c = core();
    let (id, _) = c.place_pending(pending_req("k1", "s1"), 1_000).unwrap();
    c.reject_live_result(&id, Some(venue_err()), 1_100).unwrap();

    // Same intent, immediately after: locally paused, with the real reason.
    // Goes through place() (the placement gate), not just the adapter path.
    let e = c.place(pending_req("k1", "s1"), 0, 1_200).unwrap_err();
    assert_eq!(e.code, blitzkrieg_market_api::CoreErrorCode::RiskRejected);
    assert!(e.message.contains("rejection cooldown"));

    // A different intent is free to go.
    c.place_pending(pending_req("k2", "s1"), 1_200).unwrap();

    // Backoff expiry re-opens the intent (dry settles it → position opens).
    // The ask rests AT the 0.4 buy limit, deep enough for the honest FOK.
    c.book_snapshot("tok", vec![], vec![(dec!(0.4), dec!(500))], 3_000);
    c.place(pending_req("k1", "s1"), 0, 1_100 + 2_000).unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);
}

/// Consecutive venue refusals freeze the ACCOUNT (the account-scoped kill
/// switch): the panel sees a reason, and further placement into that
/// account is refused with KillSwitchActive while the GLOBAL gate stays
/// untouched. One rejection short of the threshold must NOT freeze.
#[test]
fn consecutive_venue_rejections_freeze_trading() {
    let mut c = core();
    for i in 0..4 {
        let (id, _) = c
            .place_pending(pending_req(&format!("k{i}"), "s1"), i * 1_000 + 1)
            .unwrap();
        c.reject_live_result(&id, Some(venue_err()), i * 1_000 + 100)
            .unwrap();
    }
    assert!(
        !c.is_killed(),
        "4 consecutive rejections must not freeze yet"
    );
    assert!(
        c.account_freeze_reason("default", now_ms()).is_none(),
        "and the account is not frozen yet either"
    );
    let (id, _) = c.place_pending(pending_req("k4", "s1"), 5_000).unwrap();
    c.reject_live_result(&id, Some(venue_err()), 5_100).unwrap();
    // Per-account semantics (PR G): the ACCOUNT freezes, the global kill
    // switch does not. On a single-account deployment the refusal shape
    // is identical (KillSwitchActive at the gate) — the isolation teeth
    // live in the multi-account tests below.
    assert!(
        !c.is_killed(),
        "the venue-rejection streak freezes the ACCOUNT, never the global gate"
    );
    assert!(
        c.account_freeze_reason("default", now_ms())
            .is_some_and(|r| r.contains("consecutive venue rejections")),
        "the 5th consecutive rejection must freeze the account with the streak's reason"
    );
    let stats = c.engine_stats();
    assert_eq!(stats["venueRejected"], 5);
    assert_eq!(stats["tradingFrozen"]["active"], false);
    assert!(
        stats["accountGates"]
            .as_array()
            .map(|arr| {
                arr.iter().any(|g| {
                    g.get("account").and_then(|a| a.as_str()) == Some("default")
                        && g.get("blocked").and_then(|v| v.as_bool()).unwrap_or(false)
                        && g.get("reason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("")
                            .contains("consecutive venue rejections")
                })
            })
            .unwrap_or(false),
        "the freeze book carries the reason for the panel: {stats}"
    );
    assert!(
        stats["lastVenueError"]["message"]
            .as_str()
            .unwrap()
            .contains("NotAuthenticated"),
        "the panel must show the RAW venue error, not the freeze message: {stats}"
    );
    // #180: and the slot is structured now — the raw message AND the code
    // that classifies it, so a client can branch without parsing text.
    assert_eq!(
        stats["lastError"]["code"], "NOT_AUTHENTICATED",
        "the slot keeps the ROOT CAUSE, not the KILL_SWITCH_ACTIVE paraphrase"
    );
    assert_eq!(stats["lastError"]["message"], "auth failed");
    // Both keys are the same record: one timestamp, one writer.
    assert_eq!(stats["lastVenueError"]["tsMs"], stats["lastError"]["tsMs"]);
    assert_eq!(
        stats["lastVenueError"]["message"], "NotAuthenticated: auth failed",
        "the legacy key keeps its pre-#180 rendering exactly"
    );
    let e = c.place_pending(pending_req("k9", "s1"), 6_000).unwrap_err();
    assert_eq!(e.code, blitzkrieg_market_api::CoreErrorCode::AccountLimit);
    // The escape hatch is per account, exactly as the global #174 hatch:
    // a CLOSING intent on the frozen account passes.
    let mut close = pending_req("k10", "s1");
    close.side = Side::Sell;
    close.internal_key = "flatten:manual:tok".into();
    c.place_pending(close, 6_100)
        .unwrap_or_else(|e| panic!("a close must pass the account freeze: {e}"));
}

/// PR G teeth: a venue-rejection streak on account `paper` freezes ONLY
/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
#[test]
fn a_venue_rejection_streak_freezes_only_that_account() {
    let mut c = two_book_fixture_2();
    let paper_key = |k: &str| {
        let mut r = pending_req(k, "s1");
        r.account_id = crate::account::AccountId::from("paper");
        r
    };
    for i in 0..5 {
        let (id, _) = c
            .place_pending(paper_key(&format!("p{i}")), i * 1_000 + 1)
            .unwrap();
        c.reject_live_result(&id, Some(venue_err()), i * 1_000 + 100)
            .unwrap();
    }
    // paper is frozen with the streak's reason; default is untouched.
    assert!(
        c.account_freeze_reason("paper", now_ms())
            .is_some_and(|r| r.contains("consecutive venue rejections")),
        "paper's 5-rejection streak must freeze paper"
    );
    assert!(c.account_freeze_reason("default", now_ms()).is_none());
    assert!(!c.is_killed(), "the global gate must stay quiet");

    // default trades on (its orders pass every gate).
    c.place_pending(pending_req("d1", "s1"), 6_000)
        .unwrap_or_else(|e| panic!("an unrelated account keeps trading: {e}"));

    // paper's new orders are refused with AccountLimit naming the
    // freeze.
    let e = c.place_pending(paper_key("p9"), 6_100).unwrap_err();
    assert_eq!(e.code, blitzkrieg_market_api::CoreErrorCode::AccountLimit);
    assert!(e.message.contains("frozen"), "naming the freeze: {e}");

    // paper's CLOSE still passes (#174 hatch, per account).
    let mut close = paper_key("p10");
    close.side = Side::Sell;
    close.internal_key = "exit:strategy:tok".into();
    c.place_pending(close, 6_200)
        .unwrap_or_else(|e| panic!("a frozen account's close must flow: {e}"));

    // Manual lift re-opens entries (session-scoped: no persistence).
    c.resume_account("paper");
    assert!(c.account_freeze_reason("paper", now_ms()).is_none());
    c.place_pending(paper_key("p11"), 6_300)
        .unwrap_or_else(|e| panic!("a resumed account trades again: {e}"));
}

/// The frozen scope's entry candidates are dropped in the ENGINE (the
/// mirror rides every evaluate push) while its exits bypass that path;
/// the submission gate independently refuses a slipped entry, so both
/// layers must agree before an order can open on a frozen account.
#[test]
fn frozen_account_gate_refuses_entries_closes_flow() {
    let mut c = two_book_fixture_2();
    c.freeze_account("paper", "teeth: entries refused".into());
    let e = c
        .place(pending_req_for("paper", "x1"), 0, 1)
        .unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::AccountLimit
    );

    let mut close = pending_req_for("paper", "x2");
    close.side = Side::Sell;
    close.internal_key = "flatten:backstop:tok".into();
    c.place_pending(close, 1)
        .unwrap_or_else(|e| panic!("a frozen account's close must flow: {e}"));
}

/// PR G teeth: a venue-rejection streak on account `paper` freezes ONLY
/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
fn pending_req_for(account: &str, key: &str) -> OrderRequest {
    let mut r = pending_req(key, "s1");
    r.account_id = crate::account::AccountId::from(account);
    r
}

/// #379: venue-rejection streak on account `paper` freezes ONLY
/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
fn two_book_fixture_2() -> Core {
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
                opening_equity: None,
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
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // `CoreConfig::default` points the order log at the process cwd's
        // `data/orders/orders.jsonl` — these tests must not leave live
        // orders there for the next cwd-sharing fixture to restore.
        order_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

/// #379: venue-rejection streak on account `paper` freezes ONLY
/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
fn two_venue_book_fixture() -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("poly-acct"),
                name: "polymarket 主账户".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polymarket".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("polysim-acct"),
                name: "polysimulator 练习账户".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polysimulator".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("poly-acct"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // `CoreConfig::default` points the order log at the process cwd's
        // `data/orders/orders.jsonl` — these tests must not leave live
        // orders there for the next cwd-sharing fixture to restore.
        order_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
fn two_venue_book_fixture_2() -> Core {
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
                opening_equity: None,
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
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // `CoreConfig::default` points the order log at the process cwd's
        // `data/orders/orders.jsonl` — these tests must not leave live
        // orders there for the next cwd-sharing fixture to restore.
        order_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

/// #379: venue-rejection streak on account `paper` freezes ONLY
/// `paper` — `default` keeps placing, the global kill switch stays quiet,
/// and nothing automatic lifts the freeze. The fixture lives in
/// `audit_fix_tests`; re-declared here because module fixtures are
/// deliberately not shared.
fn two_venue_book_fixture_3() -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("poly-acct"),
                name: "polymarket 主账户".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polymarket".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("polysim-acct"),
                name: "polysimulator 练习账户".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polysimulator".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("poly-acct"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // `CoreConfig::default` points the order log at the process cwd's
        // `data/orders/orders.jsonl` — these tests must not leave live
        // orders there for the next cwd-sharing fixture to restore.
        order_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}