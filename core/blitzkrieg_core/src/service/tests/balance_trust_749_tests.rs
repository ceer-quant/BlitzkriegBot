#![cfg(test)]
//! issue 749: the live balance projection must key on "a VENUE reported this
//! book", not on "this account declared credential env names".
//!
//! The old predicate (`CredentialKeys::loaded`) failed in BOTH directions: an
//! account trading through a plugin that signs with its own env declares
//! nothing, so real venue money stayed hidden from the panel forever; and an
//! account whose keys did load still had no number until the venue answered,
//! so the credential fact was never the same fact as "this is the balance".
//! These tests pin the replacement: `Ledger::venue_reported`.

use super::*;
use rust_decimal_macros::dec;

fn acct(id: &str, name: &str, venue: &str, credentials_loaded: bool) -> crate::account::Account {
    crate::account::Account {
        id: crate::account::AccountId::from(id),
        name: name.into(),
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        status: crate::account::AccountStatus::Active,
        credential_keys: crate::account::CredentialKeys {
            loaded: credentials_loaded,
            ..Default::default()
        },
        venue: venue.into(),
        trading_enabled: true,
        strategies: Vec::new(),
        rounds: Vec::new(),
        updated_at_ms: 0,
    }
}

/// A live core over an explicit book. Every log path is disabled so the
/// fixture writes nothing outside memory (the shared checkout's `data/` is
/// never a test target).
fn core_over(mode: Mode, accounts: Vec<crate::account::Account>, active: &str) -> Core {
    Core::with_account_book(
        CoreConfig {
            mode,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            dry_seed_balance: Decimal::from(1_000),
            ..Default::default()
        },
        crate::account::AccountLedgers::new(accounts, crate::account::AccountId::from(active)),
    )
}

fn view_of<'a>(
    views: &'a [crate::schema::AccountView],
    id: &str,
) -> &'a crate::schema::AccountView {
    views
        .iter()
        .find(|v| v.id == id)
        .expect("the account is configured")
}

/// The regression: a live book carrying leftover dry seed (or a boot-replayed
/// dry journal) must not cross the wire as "the balance" until a venue has
/// actually spoken about it.
#[test]
fn a_live_book_with_no_venue_report_projects_zero() {
    let mut c = core_over(
        Mode::Live,
        vec![acct("default", "main", "", false)],
        "default",
    );
    // The exact shape the operator reported: a 1000 book under a live mode.
    c.set_balance(dec!(1000));
    assert_eq!(
        c.ledger().balance(),
        dec!(1000),
        "the book itself still holds it"
    );
    assert!(!c.active_ledger_venue_reported(), "no venue has spoken yet");

    let views = c.account_views(0);
    let v = view_of(&views, "default");
    assert_eq!(v.balance, Decimal::ZERO);
    assert_eq!(v.available, Decimal::ZERO);
    assert_eq!(v.reserved, Decimal::ZERO);
}

/// The positive half: a venue that reports its cash opens the view — and it is
/// the venue's number that shows, not the leftover book.
#[test]
fn a_venue_report_opens_the_live_balance_view() {
    let mut c = core_over(
        Mode::Live,
        vec![acct("default", "main", "", false)],
        "default",
    );
    c.set_balance(dec!(1000));

    c.realign_venue_cash("polymarket", dec!(25));
    assert!(
        c.active_ledger_venue_reported(),
        "the venue reported this book"
    );
    assert_eq!(
        c.ledger().balance(),
        dec!(25),
        "the report replaced the leftover"
    );

    let views = c.account_views(0);
    let v = view_of(&views, "default");
    assert_eq!(v.balance, dec!(25));
    assert_eq!(v.available, dec!(25));
}

/// The old predicate's first failure mode: `loaded == true` says the declared
/// env names were observed at boot, which is not the same fact as "a venue
/// told us this book's cash". With no report behind it, the book is still
/// leftovers and must project zero.
#[test]
fn declared_credentials_do_not_substitute_for_a_venue_report() {
    let mut c = core_over(
        Mode::Live,
        vec![acct("default", "main", "", true)],
        "default",
    );
    c.set_balance(dec!(1000));

    let views = c.account_views(0);
    let v = view_of(&views, "default");
    assert!(
        v.credentials_loaded,
        "the declaration is still reported as-is"
    );
    assert_eq!(
        v.balance,
        Decimal::ZERO,
        "a credential declaration is not a venue report"
    );
}

/// The old predicate's second failure mode: a sibling's credential state must
/// never lift this book's zero-fold — and, symmetrically, a sibling's venue
/// report must not either.
#[test]
fn one_accounts_venue_report_does_not_open_a_siblings_book() {
    let mut c = core_over(
        Mode::Live,
        vec![
            acct("default", "main", "polymarket", true),
            acct("kalshi-acct", "k", "kalshi", true),
        ],
        "default",
    );
    c.realign_venue_cash("polymarket", dec!(25));

    let views = c.account_views(0);
    assert_eq!(view_of(&views, "default").balance, dec!(25));
    assert_eq!(
        view_of(&views, "kalshi-acct").balance,
        Decimal::ZERO,
        "kalshi never reported; its sibling's report says nothing about it"
    );

    // And switching the active pointer moves the PROCESS-level view with it —
    // the venue-report fact is per book, so the new active book is still folded.
    c.accounts
        .switch(&crate::account::AccountId::from("kalshi-acct"))
        .unwrap();
    assert!(!c.active_ledger_venue_reported());
}

/// Dry and read-only are unchanged: their book IS the money there is (a seed,
/// a funding call), so the projection must not fold it — there is no venue to
/// report anything in those modes.
#[test]
fn dry_and_read_only_still_report_their_seeded_book() {
    for mode in [Mode::Dry, Mode::ReadOnly] {
        let mut c = core_over(mode, vec![acct("default", "main", "", false)], "default");
        c.set_balance(dec!(500));
        assert!(!c.active_ledger_venue_reported());

        let views = c.account_views(0);
        assert_eq!(
            view_of(&views, "default").balance,
            dec!(500),
            "{mode:?} keeps its seeded book"
        );
    }
}
