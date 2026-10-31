use super::*;
use rust_decimal_macros::dec;

fn two_accounts() -> AccountLedgers {
    AccountLedgers::new(
        vec![
            Account {
                id: AccountId("default".into()),
                name: "main".into(),
                market_type: MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 1,
            },
            Account {
                id: AccountId("paper".into()),
                name: "paper".into(),
                market_type: MarketType::Prediction,
                status: AccountStatus::ReadOnly,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 1,
            },
        ],
        default_account_id(),
    )
}

#[test]
fn get_mut_refuses_an_unknown_account_explicitly() {
    let mut book = two_accounts();
    let err = book
        .get_mut(&AccountId("ghost".into()))
        .expect_err("an unknown account must be refused, never created");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(
        err.message.contains("unknown account `ghost`"),
        "{}",
        err.message
    );
    // And nothing was created by the refused call:
    assert!(book.account(&AccountId("ghost".into())).is_none());
}

#[test]
fn ledgers_are_isolated_per_account() {
    let mut book = two_accounts();
    book.get_mut(&AccountId("paper".into()))
        .expect("paper is configured")
        .set_balance(dec!(500));
    assert_eq!(
        book.ledger_of(&default_account_id()).unwrap().balance(),
        dec!(0)
    );
    assert_eq!(
        book.ledger_of(&AccountId("paper".into()))
            .unwrap()
            .balance(),
        dec!(500)
    );
}

#[test]
fn switch_refuses_unknown_and_moves_the_active_ledger() {
    let mut book = two_accounts();
    book.switch(&AccountId("paper".into())).expect("configured");
    assert_eq!(book.active_id().as_str(), "paper");
    book.get_mut(&AccountId("paper".into()))
        .unwrap()
        .set_balance(dec!(7));
    assert_eq!(book.active_ledger().balance(), dec!(7));
    let err = book
        .switch(&AccountId("ghost".into()))
        .expect_err("unknown");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
}

#[test]
fn status_can_only_tighten() {
    use AccountStatus::{Active, Frozen, ReadOnly, Suspended};
    let suspended = Suspended {
        reason: "ops".into(),
    };
    // The acceptance case: frozen -> active is a loosening, refused.
    assert!(!Frozen.can_tighten_to(&Active));
    // read_only -> frozen re-grants the close exemption: also loosening.
    assert!(!ReadOnly.can_tighten_to(&Frozen));
    assert!(!ReadOnly.can_tighten_to(&Active));
    // Active may go anywhere; same-posture lateral moves are allowed.
    assert!(Active.can_tighten_to(&Frozen));
    assert!(Active.can_tighten_to(&ReadOnly));
    assert!(Active.can_tighten_to(&suspended));
    assert!(Frozen.can_tighten_to(&suspended));
    assert!(suspended.can_tighten_to(&Frozen));
    // Frozen -> read_only tightens (close exemption removed).
    assert!(Frozen.can_tighten_to(&ReadOnly));
}

#[test]
fn set_status_records_and_validates_the_account() {
    let mut book = two_accounts();
    book.set_status(&AccountId("paper".into()), AccountStatus::Frozen, 42)
        .expect("configured");
    assert!(matches!(
        book.account(&AccountId("paper".into())).unwrap().status,
        AccountStatus::Frozen
    ));
    let err = book
        .set_status(&AccountId("ghost".into()), AccountStatus::Frozen, 42)
        .expect_err("unknown");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
}

#[test]
fn set_venue_writes_unbinds_and_refuses_unknown_accounts() {
    let mut book = two_accounts();
    // Bind: the non-empty value pins the account to a plugin name.
    book.set_venue(&AccountId("paper".into()), "kalshi", 7)
        .expect("configured");
    let a = book.account(&AccountId("paper".into())).unwrap();
    assert_eq!(a.venue, "kalshi");
    assert_eq!(a.updated_at_ms, 7, "the write refreshes the record");
    // Unbind: the empty string is the legal unbound value again.
    book.set_venue(&AccountId("paper".into()), "", 8)
        .expect("empty venue = unbound");
    assert_eq!(book.account(&AccountId("paper".into())).unwrap().venue, "");
    // Two accounts MAY carry the same venue — same exchange, several API
    // sets, one account each, is the book's whole deployment model.
    book.set_venue(&default_account_id(), "kalshi", 9)
        .expect("same venue on a second account is legal");
    book.set_venue(&AccountId("paper".into()), "kalshi", 9)
        .expect("…and re-binding after an unbind");
    assert_eq!(book.account(&default_account_id()).unwrap().venue, "kalshi");
    assert_eq!(
        book.account(&AccountId("paper".into())).unwrap().venue,
        "kalshi"
    );
    // An unknown account is refused explicitly, never created.
    let err = book
        .set_venue(&AccountId("ghost".into()), "kalshi", 10)
        .expect_err("unknown");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(err.message.contains("unknown account `ghost`"));
    assert!(book.account(&AccountId("ghost".into())).is_none());
}

#[test]
fn single_default_matches_the_zero_config_deployment() {
    let book = AccountLedgers::single_default(7);
    assert_eq!(book.account_list().len(), 1);
    assert_eq!(book.active_id().as_str(), DEFAULT_ACCOUNT_ID);
    let a = book.account(&default_account_id()).unwrap();
    assert!(matches!(a.status, AccountStatus::Active));
    assert!(!a.credential_keys.loaded, "no keys named, nothing loaded");
}

/// An ACTIVE/inactive account bound to one venue — #778's collision input.
fn bound_account(id: &str, venue: &str, status: AccountStatus) -> Account {
    Account {
        id: AccountId(id.into()),
        name: id.into(),
        market_type: MarketType::Prediction,
        status,
        credential_keys: CredentialKeys::default(),
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        updated_at_ms: 1,
    }
}

#[test]
fn account_for_venue_is_deterministic_across_insertion_order() {
    // #778(b): two ACTIVE accounts on ONE venue. The book is a HashMap,
    // so the old `values().find` result depended on iteration order. The
    // rule is: the explicit `default` account when it is Active and bound,
    // else the lexicographically smallest id. Neither id here is
    // `default`, so `alpha` (< `zeta`) must win in every construction.
    let build = |order: [&str; 2]| {
        AccountLedgers::new(
            order
                .iter()
                .map(|id| bound_account(id, "kalshi", AccountStatus::Active))
                .collect(),
            AccountId(order[0].into()),
        )
    };
    let forward = build(["alpha", "zeta"]);
    let reversed = build(["zeta", "alpha"]);
    for _ in 0..16 {
        assert_eq!(
            forward.account_for_venue("kalshi").map(|a| a.id.as_str()),
            Some("alpha"),
            "the smallest bound id must win, not HashMap order"
        );
        assert_eq!(
            reversed.account_for_venue("kalshi").map(|a| a.id.as_str()),
            Some("alpha"),
            "reversed insertion must not change the winner"
        );
    }
    assert!(
        forward.account_for_venue("polymarket").is_none(),
        "an unowned venue still names nobody"
    );
}

#[test]
fn account_for_venue_prefers_the_default_account_and_skips_inactive_siblings() {
    // The explicit `default` account wins even though "alpha" < "default".
    let book = AccountLedgers::new(
        vec![
            bound_account("alpha", "kalshi", AccountStatus::Active),
            bound_account("default", "kalshi", AccountStatus::Active),
        ],
        default_account_id(),
    );
    assert_eq!(
        book.account_for_venue("kalshi").map(|a| a.id.as_str()),
        Some("default"),
        "the explicit default account is the venue's owner"
    );

    // A non-Active default does NOT win: status gates ownership, and the
    // smallest ACTIVE id takes the fallback.
    let book = AccountLedgers::new(
        vec![
            bound_account("alpha", "kalshi", AccountStatus::Active),
            bound_account("default", "kalshi", AccountStatus::ReadOnly),
        ],
        AccountId("alpha".into()),
    );
    assert_eq!(
        book.account_for_venue("kalshi").map(|a| a.id.as_str()),
        Some("alpha"),
        "a read-only default cannot own venue cash"
    );
}

#[test]
fn account_list_is_sorted_for_a_stable_wire_order() {
    let book = two_accounts();
    let ids: Vec<&str> = book.account_list().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["default", "paper"]);
}

#[test]
fn permits_order_enforces_gate_2_postures() {
    let mut book = two_accounts(); // default = Active, paper = ReadOnly
    assert!(book.permits_order(&default_account_id(), false).is_ok());
    assert!(book.permits_order(&default_account_id(), true).is_ok());
    // ReadOnly refuses BOTH sides — the one full stop.
    let err = book
        .permits_order(&AccountId("paper".into()), false)
        .expect_err("read_only refuses entries");
    assert_eq!(err.code, CoreErrorCode::AccountLimit);
    assert!(err.message.contains("is read_only"), "{}", err.message);
    let err = book
        .permits_order(&AccountId("paper".into()), true)
        .expect_err("read_only refuses closes");
    assert_eq!(err.code, CoreErrorCode::AccountLimit);
    // Frozen refuses entries but must never trap a close.
    book.set_status(&AccountId("paper".into()), AccountStatus::Frozen, 2)
        .unwrap();
    assert!(
        book.permits_order(&AccountId("paper".into()), false)
            .is_err()
    );
    assert!(book.permits_order(&AccountId("paper".into()), true).is_ok());
    // An unknown account is refused explicitly, never created.
    let err = book
        .permits_order(&AccountId("ghost".into()), false)
        .expect_err("unknown account");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(book.account(&AccountId("ghost".into())).is_none());
}

/// The trading switch (「关=拒开仓、放平仓」) is an INDEPENDENT axis from
/// AccountStatus: OFF refuses entries but never traps a close, even with
/// the account otherwise Active; ON restores entries; the stamp rides
/// the writes; unknown ids refuse.
#[test]
fn trading_switch_refuses_entries_passes_closes() {
    let mut book = two_accounts();
    book.set_trading(&default_account_id(), false, 5)
        .expect("the default account takes the switch");
    // Entry refused, close allowed — the exact contract of the switch.
    let err = book
        .permits_order(&default_account_id(), false)
        .expect_err("trading-off refuses entries");
    assert_eq!(err.code, CoreErrorCode::AccountLimit);
    assert!(
        err.message.contains("trading-closed"),
        "the refusal names the switch, not the status: {}",
        err.message
    );
    assert!(
        book.permits_order(&default_account_id(), true).is_ok(),
        "trading-off never traps a close"
    );
    // The status axis is untouched: the account is still Active.
    assert!(matches!(
        book.account(&default_account_id()).unwrap().status,
        AccountStatus::Active
    ));
    // Re-open restores entries; the stamp rode the writes.
    book.set_trading(&default_account_id(), true, 9).unwrap();
    assert!(book.permits_order(&default_account_id(), false).is_ok());
    assert_eq!(
        book.account(&default_account_id()).unwrap().updated_at_ms,
        9
    );
    // Unknown ids refuse on the switch path too.
    assert!(
        book.set_trading(&AccountId("ghost".into()), false, 11)
            .is_err()
    );
}

/// The enablement write (`account.strategies.set` book half): names ride
/// verbatim (the CALLER validated them against the registered set), an
/// empty list is the explicit empty set (spelled "runs nothing" — the
/// reader answers None), and unknown ids refuse.
#[test]
fn strategy_set_rides_verbatim_and_normalizes_empty_to_nothing() {
    let mut book = two_accounts();
    let paper = AccountId("paper".into());
    book.set_strategies(&paper, vec!["spread_arb".into()], 3)
        .expect("a named assignment rides");
    assert_eq!(
        book.strategy_assignment(&paper).unwrap(),
        &vec!["spread_arb".to_string()]
    );
    // The EMPTY SET: stores and reads back as None — the absent key is
    // the "this account runs nothing" spelling (per-account enablement is
    // the only truth: an empty set never follows the global toggle).
    book.set_strategies(&paper, Vec::new(), 4).unwrap();
    assert!(
        book.strategy_assignment(&paper).is_none(),
        "empty assignment = runs nothing (None)"
    );
    // Unknown id refuses.
    assert!(
        book.set_strategies(&AccountId("ghost".into()), vec!["x".into()], 5)
            .is_err()
    );
}

// ── account.create / account.remove 的簿内半（存储层）─────────────────────

fn fresh_account(id: &str, now_ms: i64) -> Account {
    Account {
        id: AccountId::from(id),
        name: id.to_string(),
        market_type: MarketType::Prediction,
        status: AccountStatus::Active,
        credential_keys: CredentialKeys::default(),
        venue: String::new(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        updated_at_ms: now_ms,
    }
}

#[test]
fn insert_registers_the_account_with_a_fresh_zero_ledger() {
    let mut book = two_accounts();
    book.insert(fresh_account("poly-main", 7))
        .expect("a new id inserts");
    let a = book.account(&AccountId("poly-main".into())).unwrap();
    assert_eq!(a.id.as_str(), "poly-main");
    assert!(matches!(a.status, AccountStatus::Active));
    assert_eq!(a.updated_at_ms, 7, "the caller's stamp rides");
    // The new book is a ZERO book: present, funded by nothing.
    assert_eq!(
        book.ledger_of(&AccountId("poly-main".into()))
            .unwrap()
            .balance(),
        dec!(0)
    );
    // It is addressable by the standard gates and listable in order.
    book.require(&AccountId("poly-main".into()))
        .expect("require accepts the minted account");
    let ids: Vec<&str> = book.account_list().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["default", "paper", "poly-main"]);
}

#[test]
fn insert_refuses_a_duplicate_id_and_default_never_slips_through() {
    let mut book = two_accounts();
    let err = book
        .insert(fresh_account("paper", 7))
        .expect_err("duplicate id");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(err.message.contains("already exists"), "{}", err.message);
    // The refused insert created nothing: the existing record is intact.
    assert_eq!(
        book.account(&AccountId("paper".into())).unwrap().name,
        "paper"
    );
}

#[test]
fn remove_drops_account_and_ledger_including_default() {
    let mut book = two_accounts();
    book.get_mut(&AccountId("paper".into()))
        .unwrap()
        .set_balance(dec!(42));
    book.remove(&AccountId("paper".into())).expect("retires");
    assert!(book.account(&AccountId("paper".into())).is_none());
    assert!(
        book.ledger_of(&AccountId("paper".into())).is_err(),
        "the ledger goes with the account — no orphan book"
    );
    // `default` is an ordinary account now: removable like any other
    // (the last-account refusal lives in the runtime caller, not here).
    book.remove(&default_account_id()).expect("default retires");
    assert!(book.account(&default_account_id()).is_none());
    // An unknown id is the standard refusal.
    book.insert(fresh_account("other", 1)).unwrap();
    let err = book
        .remove(&AccountId("ghost".into()))
        .expect_err("unknown");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(err.message.contains("unknown account `ghost`"));
}

#[test]
fn removing_the_active_account_repairs_the_cell_to_default() {
    let mut book = two_accounts();
    book.switch(&AccountId("paper".into())).unwrap();
    book.remove(&AccountId("paper".into())).unwrap();
    book.repair_active_after_removal(&AccountId("paper".into()));
    assert_eq!(
        book.active_id().as_str(),
        DEFAULT_ACCOUNT_ID,
        "the retired active falls back to the reserved default"
    );
    // The cell repair is a no-op when the removed id was not active.
    book.switch(&AccountId("paper".into())).ok(); // unknown now — refused
    book.insert(fresh_account("other", 1)).unwrap();
    book.switch(&AccountId("other".into())).unwrap();
    // Removing a NON-active account (default here) leaves the cell alone.
    book.remove(&default_account_id()).expect("default retires");
    book.repair_active_after_removal(&default_account_id());
    assert_eq!(
        book.active_id().as_str(),
        "other",
        "an unrelated removal leaves the active cell alone"
    );
}

#[test]
fn removing_default_repairs_the_cell_to_the_first_survivor() {
    // default active + default removed: the cell re-activates the first
    // surviving entry (lowest id) — the file loader's resolution for a
    // book whose `default` row left.
    let mut book = two_accounts();
    assert_eq!(book.active_id().as_str(), DEFAULT_ACCOUNT_ID);
    book.remove(&default_account_id()).unwrap();
    book.repair_active_after_removal(&default_account_id());
    assert_eq!(
        book.active_id().as_str(),
        "paper",
        "the active cell never dangles: first survivor takes it"
    );
    // When a surviving `default` exists it wins over the lowest id.
    let mut book2 = two_accounts();
    book2.remove(&AccountId("paper".into())).unwrap();
    book2.insert(fresh_account("aaa", 1)).unwrap();
    book2.switch(&AccountId("aaa".into())).unwrap();
    book2.remove(&AccountId("aaa".into())).unwrap();
    book2.repair_active_after_removal(&AccountId("aaa".into()));
    assert_eq!(
        book2.active_id().as_str(),
        DEFAULT_ACCOUNT_ID,
        "a surviving default is preferred over the lowest id"
    );
}

#[test]
fn removing_the_displayed_account_repairs_the_pointer_and_set_display_validates() {
    // The pointer follows the SAME discipline as the active cell: a
    // removal that takes the displayed account away re-derives it (first
    // ACTIVE survivor, else lowest id) — the snapshot must never name a
    // removed account.
    let mut book = two_accounts();
    book.set_display(&AccountId("paper".into())).unwrap();
    assert_eq!(book.display_id(), "paper");
    book.remove(&AccountId("paper".into())).unwrap();
    book.repair_display_after_removal(&AccountId("paper".into()));
    assert_eq!(
        book.display_id(),
        DEFAULT_ACCOUNT_ID,
        "the retired display falls back to the first active survivor"
    );
    // display_venue follows the pointed-at account (empty = unbound).
    let mut bound = two_accounts();
    bound
        .set_venue(&AccountId("paper".into()), "kalshi", 1)
        .unwrap();
    bound.set_display(&AccountId("paper".into())).unwrap();
    assert_eq!(bound.display_venue(), "kalshi");
    // An unknown id never moves the pointer (set_display validates).
    assert!(bound.set_display(&AccountId("ghost".into())).is_err());
    assert_eq!(bound.display_id(), "paper");
    // A removal of a NON-displayed account leaves the pointer alone.
    bound.remove(&default_account_id()).unwrap();
    bound.repair_display_after_removal(&default_account_id());
    assert_eq!(bound.display_id(), "paper");
}

#[test]
fn config_reload_keeps_money_and_refuses_ledger_topology_changes() {
    let mut book = two_accounts();
    book.get_mut(&default_account_id())
        .unwrap()
        .set_balance(dec!(42));

    let mut fresh = two_accounts();
    fresh.accounts.get_mut(&default_account_id()).unwrap().name = "renamed".into();
    book.reload_config(fresh).unwrap();
    assert_eq!(
        book.ledger_of(&default_account_id()).unwrap().balance(),
        dec!(42)
    );
    assert_eq!(book.account(&default_account_id()).unwrap().name, "renamed");

    let changed = AccountLedgers::single_default(2);
    assert!(book.reload_config(changed).is_err());
    assert_eq!(
        book.ledger_of(&default_account_id()).unwrap().balance(),
        dec!(42)
    );
}
