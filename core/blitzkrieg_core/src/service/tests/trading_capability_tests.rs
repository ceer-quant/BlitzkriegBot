#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
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
        c.account_freeze_reason("default").is_none(),
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
        c.account_freeze_reason("default")
            .is_some_and(|r| r.contains("consecutive venue rejections")),
        "the 5th consecutive rejection must freeze the account with the streak's reason"
    );
    let stats = c.engine_stats();
    assert_eq!(stats["venueRejected"], 5);
    assert_eq!(stats["tradingFrozen"]["active"], false);
    assert!(
        stats["accountFreezes"]["default"]
            .as_str()
            .unwrap_or("")
            .contains("consecutive venue rejections"),
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
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
    );
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
    let mut c = two_book_fixture();
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
        c.account_freeze_reason("paper")
            .is_some_and(|r| r.contains("consecutive venue rejections")),
        "paper's 5-rejection streak must freeze paper"
    );
    assert!(c.account_freeze_reason("default").is_none());
    assert!(!c.is_killed(), "the global gate must stay quiet");

    // default trades on (its orders pass every gate).
    c.place_pending(pending_req("d1", "s1"), 6_000)
        .unwrap_or_else(|e| panic!("an unrelated account keeps trading: {e}"));

    // paper's new orders are refused with KillSwitchActive naming the
    // freeze.
    let e = c.place_pending(paper_key("p9"), 6_100).unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
    );
    assert!(e.message.contains("frozen"), "naming the freeze: {e}");

    // paper's CLOSE still passes (#174 hatch, per account).
    let mut close = paper_key("p10");
    close.side = Side::Sell;
    close.internal_key = "exit:strategy:tok".into();
    c.place_pending(close, 6_200)
        .unwrap_or_else(|e| panic!("a frozen account's close must flow: {e}"));

    // Manual lift re-opens entries (session-scoped: no persistence).
    c.resume_account("paper");
    assert!(c.account_freeze_reason("paper").is_none());
    c.place_pending(paper_key("p11"), 6_300)
        .unwrap_or_else(|e| panic!("a resumed account trades again: {e}"));
}

/// The frozen scope's entry candidates are dropped in the ENGINE (the
/// mirror rides every evaluate push) while its exits bypass that path;
/// the submission gate independently refuses a slipped entry, so both
/// layers must agree before an order can open on a frozen account.
#[test]
fn frozen_account_gate_refuses_entries_closes_flow() {
    let mut c = two_book_fixture();
    c.freeze_account("paper", "teeth: entries refused".into());
    let e = c.place(pending_req_for("paper", "x1"), 0, 1).unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
    );

    let mut close = pending_req_for("paper", "x2");
    close.side = Side::Sell;
    close.internal_key = "flatten:backstop:tok".into();
    c.place_pending(close, 1)
        .unwrap_or_else(|e| panic!("a frozen account's flatten must flow: {e}"));
}

fn pending_req_for(account: &str, key: &str) -> OrderRequest {
    let mut r = pending_req(key, "s1");
    r.account_id = crate::account::AccountId::from(account);
    r
}

/// PR G found the E28-era audit scope error: `reservations_match_open_buys`
/// compared the ACTIVE ledger's `reserved` against the unfilled notional of
/// ALL accounts' live BUYs, so a sibling account's open order read as drift
/// and blocked new entries on the active book. The sibling's notional is
/// spoken for in ITS ledger — here it must not trip the audit.
#[test]
fn audit_reservation_scope_is_per_account() {
    let mut c = two_book_fixture();
    // default (active) carries a live BUY; paper carries one too.
    c.place_pending(pending_req("d1", "s1"), 1).unwrap();
    c.place_pending(pending_req_for("paper", "p1"), 2).unwrap();
    // The audit reads the active book: 1 live BUY, reserved == its notional.
    let report = c.run_accounting_audit(10);
    assert!(
        report.ok,
        "a sibling account's live BUY is not the active book's drift: {:?}",
        report
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
    let res = report
        .checks
        .iter()
        .find(|c| c.name == "reservations_match_open_buys")
        .unwrap();
    assert_eq!(res.actual, dec!(4), "default's own unfilled notional only");
}

/// The `audit_fix_tests::two_book_core` fixture, module-local: two
/// Active accounts (default + paper), no trade log, single config shape.
fn two_book_fixture() -> Core {
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

/// A failed capability self-check freezes trading, and the report stays
/// visible in the diagnostics; a passing report does not.
#[test]
fn failed_self_check_freezes_trading() {
    let mut c = core();
    c.on_venue_self_check(
        "polymarket",
        blitzkrieg_market_api::SelfCheckReport {
            ok: false,
            ts_ms: 1_000,
            items: vec![blitzkrieg_market_api::SelfCheckItem {
                name: "balance".into(),
                ok: false,
                detail: "401 unauthorized".into(),
            }],
        },
    );
    // The failing probe is polymarket's: the freeze lands on the accounts
    // bound to that venue. The in-process `core()` has no bound accounts,
    // so the fallback keeps the legacy global freeze — the assertion is
    // unchanged.
    assert!(c.is_killed());
    let stats = c.engine_stats();
    assert_eq!(stats["selfCheck"]["ok"], false);
    assert_eq!(stats["tradingFrozen"]["active"], true);

    // An explicit resume clears the freeze (operator decision).
    c.risk.resume();
    assert!(!c.is_killed());
}

/// #624 acceptance, venue-scoped kill switch (2026-10-25 多场馆雪崩定谳):
/// ONE venue's failed probes freeze ONLY the accounts bound to it — the
/// sibling venue's account keeps trading and the GLOBAL kill switch stays
/// down. The fixture carries two ACTIVE accounts on different venues (the
/// injected-book constructor; no real accounts.toml is touched), then drives
/// both frozen-freeze routes through their real entry points:
///
/// - a failed venue self-check freezes polymarket's account immediately;
/// - polymarket's reconcile-failure streak (3 in a row) must NOT disturb
///   anything else — and MUST NOT re-freeze globally either;
/// - polysimulator's OWN streak must reach its own threshold from zero:
///   2 failures freeze nothing (under SWEEP_FAILURE_FREEZE=3), the 3rd
///   freezes polysimulator's account — per-venue streaks are independent.
#[test]
fn venue_scoped_freeze_does_not_touch_sibling_venues() {
    let mut c = two_venue_book_fixture();
    let polymarket_req = |key: &str| {
        let mut r = pending_req(key, "s1");
        r.account_id = crate::account::AccountId::from("poly-acct");
        r
    };
    let polysim_req = |key: &str| {
        let mut r = pending_req(key, "s1");
        r.account_id = crate::account::AccountId::from("polysim-acct");
        r
    };
    let sweep_err = || {
        crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::VenueError,
            "sweep timed out",
        )
    };

    // ── 1. A failed self-check on polymarket freezes ONLY its account ──
    c.on_venue_self_check(
        "polymarket",
        blitzkrieg_market_api::SelfCheckReport {
            ok: false,
            ts_ms: 1_000,
            items: vec![blitzkrieg_market_api::SelfCheckItem {
                name: "venue_balance".into(),
                ok: false,
                detail: "HTTP 401 unauthorized".into(),
            }],
        },
    );
    assert!(
        c.account_freeze_reason("poly-acct")
            .is_some_and(|r| r.contains("self-check failed on polymarket")),
        "the failed polymarket self-check must freeze the account bound to it: {:?}",
        c.account_freeze_reason("poly-acct")
    );
    assert!(
        c.account_freeze_reason("polysim-acct").is_none(),
        "the sibling venue's account must stay unbound by the failure"
    );
    assert!(
        !c.is_killed(),
        "a venue with bound accounts must NOT fire the global kill switch"
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["tradingFrozen"]["active"], false,
        "the global freeze banner stays down: {stats}"
    );
    assert!(
        stats["accountFreezes"]["poly-acct"]
            .as_str()
            .unwrap_or("")
            .contains("self-check failed on polymarket"),
        "the freeze book carries the reason for the panel: {stats}"
    );
    assert!(
        stats["accountFreezes"].get("polysim-acct").is_none(),
        "the sibling account is in no freeze book: {stats}"
    );

    // Teeth at the gate: the frozen account's NEW entry is refused with
    // KillSwitchActive while the sibling places happily.
    let e = c.place_pending(polymarket_req("pk1"), 1_100).unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
    );
    assert!(e.message.contains("poly-acct"), "naming the account: {e}");
    c.place_pending(polysim_req("ps1"), 1_200)
        .unwrap_or_else(|e| panic!("an unbound-by-failure sibling keeps trading: {e}"));

    // The #174 hatch is per account here exactly as it is globally: the
    // frozen account's CLOSING intent passes.
    let mut close = polymarket_req("pk2");
    close.side = Side::Sell;
    close.internal_key = "flatten:manual:tok".into();
    c.place_pending(close, 1_300)
        .unwrap_or_else(|e| panic!("a frozen account's close must flow: {e}"));

    // ── 2. polymarket's reconcile streak: already frozen stays frozen, ──
    //    the sibling stays untouched, nothing goes global.
    for i in 0..3 {
        c.on_venue_reconcile_failed("polymarket", sweep_err(), 2_000 + i);
    }
    assert!(
        c.account_freeze_reason("poly-acct")
            .is_some_and(|r| r.contains("self-check failed on polymarket")),
        "freeze is idempotent: the first reason wins, the streak must not overwrite it"
    );
    assert!(
        c.account_freeze_reason("polysim-acct").is_none(),
        "three polymarket sweep failures must not freeze the sibling account"
    );
    assert!(
        !c.is_killed(),
        "a full polymarket streak with a bound account must not go global"
    );
    c.place_pending(polysim_req("ps2"), 2_500)
        .unwrap_or_else(|e| panic!("the sibling still trades through the streak: {e}"));

    // ── 3. polysimulator's OWN streak is independent: 2 of 3 freeze ──
    //    nothing, the 3rd freezes exactly its account.
    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 3_000);
    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 3_100);
    assert!(
        c.account_freeze_reason("polysim-acct").is_none(),
        "two failures are under the threshold of {SWEEP_FAILURE_FREEZE}: nothing freezes"
    );
    c.place_pending(polysim_req("ps3"), 3_200)
        .unwrap_or_else(|e| panic!("under-threshold failures must not freeze: {e}"));

    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 3_300);
    assert!(
        c.account_freeze_reason("polysim-acct").is_some_and(
            |r| r.contains("reconciliation sweep failed 3 times in a row on polysimulator")
        ),
        "polysimulator's own 3rd failure must freeze ITS account: {:?}",
        c.account_freeze_reason("polysim-acct")
    );
    // Both venues are now frozen per-account — and the global switch is
    // STILL quiet: two account freezes are not a kill switch.
    assert!(
        c.account_freeze_reason("poly-acct").is_some(),
        "the sibling venue's earlier freeze is untouched by polysimulator's streak"
    );
    assert!(
        !c.is_killed(),
        "two per-account freezes must not collapse into the global kill switch"
    );
    let e = c.place_pending(polysim_req("ps4"), 3_400).unwrap_err();
    assert_eq!(
        e.code,
        blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["tradingFrozen"]["active"], false,
        "the panel's global freeze banner never lit: {stats}"
    );

    // ── 4. A successful sweep clears the per-venue streaks (E31-b) ──────
    //    Without a bound-account freeze to lift automatically, the point is
    //    the counter itself: after the clear, a fresh streak starts from 1,
    //    so 2 more failures do NOT freeze — the pre-clear failures are
    //    forgotten. (An already-frozen account stays frozen: only an
    //    operator or a restart lifts a freeze — never a passing sweep.)
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 4_000,
    })
    .unwrap();
    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 4_100);
    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 4_200);
    assert!(
        !c.is_killed(),
        "a cleared streak rebuilt from 1 must stay under the threshold"
    );
    assert!(
        c.account_freeze_reason("polysim-acct")
            .is_some_and(|r| r.contains("reconciliation sweep failed 3 times")),
        "the earlier freeze is deliberate state, not streak fallout — it persists"
    );
    c.on_venue_reconcile_failed("polysimulator", sweep_err(), 4_300);
    assert!(
        c.account_freeze_reason("polysim-acct").is_some_and(
            |r| r.contains("sweep failed 3 times in a row on polysimulator: sweep timed out")
        ),
        "the freeze was idempotent from the first streak: the reason the operator saw first stands"
    );
    assert!(!c.is_killed(), "still account-scoped, never global");
}

/// The two-venue book this file's venue-scoped-freeze acceptance uses:
/// BOTH accounts are ACTIVE and each is bound to its own venue, so
/// `account_ids_bound_to_venue` has a routing table to work with.
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
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("poly-acct"),
    );
    let cfg = CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // Same discipline as `two_book_fixture`: the cwd-relative default
        // order log must not collect these tests' orders.
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

/// A venue sell on an order the OME never issued (a manual close made
/// directly on the venue) must fold into the position book: the position
/// disappears with a Manual close and the ledger receives the proceeds —
/// instead of haunting the exit rules forever.
#[test]
fn manual_venue_close_reconciles_the_position() {
    let mut c = core();
    // Ask at the buy limit, deep enough for the honest FOK entry to fill.
    c.book_snapshot("tok", vec![], vec![(dec!(0.4), dec!(500))], 900);
    c.place(buy_taker(dec!(0.4), dec!(10)), 0, 1_000).unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);
    let cost = c.positions().open_positions()[0].cost_usd;
    let seed = dec!(1000);

    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "manual-1".into(),
            trade_id: "t1".into(),
            token_id: "tok".into(),
            side: Side::Sell,
            size: dec!(10),
            price: dec!(0.6),
            ts_ms: 2_000,
            tx_hash: None,
            maker: Some(false),
        }],
        now_ms: 3_000,
    };
    c.reconcile(snap).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "the manually closed position must vanish from the book"
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Manual);
    assert_eq!(closed.shares, dec!(10));
    // Cash identity: seed - entry (cost + taker fee) + exit proceeds - exit fee.
    let entry_fee = (crate::exit_policy::taker_fee_pct(dec!(0.4)) / dec!(100)) * dec!(4);
    let fee = (crate::exit_policy::taker_fee_pct(dec!(0.6)) / dec!(100)) * dec!(6);
    assert_eq!(
        c.ledger().balance(),
        seed - cost - entry_fee + dec!(6) - fee,
        "the ledger must reflect the cash the manual close actually moved"
    );
    assert_eq!(c.recon_watermark_ms, 2_000);

    // Idempotent: replaying the same sweep must not double-count.
    let snap2 = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "manual-1".into(),
            trade_id: "t1".into(),
            token_id: "tok".into(),
            side: Side::Sell,
            size: dec!(10),
            price: dec!(0.6),
            ts_ms: 2_000,
            tx_hash: None,
            maker: Some(false),
        }],
        now_ms: 4_000,
    };
    c.reconcile(snap2).unwrap();
    assert_eq!(
        c.positions().closed_positions().len(),
        1,
        "no duplicate close on a replayed sweep"
    );
    assert_eq!(
        c.ledger().balance(),
        seed - cost - entry_fee + dec!(6) - fee
    );
}

fn buy_taker(price: Decimal, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: "k1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}
