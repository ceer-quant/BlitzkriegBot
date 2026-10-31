use super::*;
use crate::model::SignalDirection;
use crate::position::CashFlows;
use rust_decimal_macros::dec;

fn position(id: &str, token: &str, shares: Decimal, entered_ms: i64) -> OpenPosition {
    OpenPosition {
        id: id.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: SignalDirection::Up,
        token_id: token.into(),
        condition_id: "cond-1".into(),
        entry_price: dec!(0.40),
        current_price: dec!(0.40),
        prev_price: dec!(0.40),
        shares,
        cost_usd: dec!(4),
        was_maker_entry: true,
        entry_fee_pct: Decimal::ZERO,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Pending,
        target_exit_price: None,
        entered_at_ms: entered_ms,
        expires_at_ms: entered_ms + 300_000,
        account_id: crate::model::default_account_id(),
        venue: String::new(),
        partner_token_id: String::new(),
        // F6 (main): no book has been seen for this fixture position.
        last_book_ts: 0,
        round_duration_sec: 0,
        round_slot: 0,
        state: crate::exit_policy::ExitState::new(dec!(0.40), entered_ms),
        flows: CashFlows {
            entry_cost_usd: dec!(4),
            opened_shares: shares,
            ..Default::default()
        },
    }
}

fn resolution(payouts: Vec<(&str, Decimal)>) -> MarketResolution {
    MarketResolution {
        condition_id: "cond-1".into(),
        resolved: true,
        payouts: payouts
            .into_iter()
            .map(|(t, p)| (t.to_string(), p))
            .collect(),
        neg_risk: true,
        source: "test".into(),
    }
}

fn win() -> MarketResolution {
    resolution(vec![("up", dec!(1)), ("down", dec!(0))])
}

/// `win()` for another condition: the winner pays 1.00/share on the named
/// token, the way a second venue's market resolves on its own.
fn win_for(condition_id: &str, winner: &str) -> MarketResolution {
    let mut r = resolution(vec![(winner, dec!(1)), ("down", dec!(0))]);
    r.condition_id = condition_id.into();
    r
}

fn book_with_journal() -> (SettlementBook, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "bksettle-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settlements.jsonl");
    (SettlementBook::new(Some(&path)), path)
}

#[test]
fn booking_prices_a_win_and_a_loss() {
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    assert_eq!(b.payout_per_share, dec!(1));
    assert_eq!(b.payout_usd, dec!(10));
    assert!(b.winning);
    assert_eq!(b.outcome_index, Some(0));

    let losing = resolution(vec![("up", dec!(0)), ("down", dec!(1))]);
    let b = booking_for(&p, &losing).unwrap();
    assert_eq!(b.payout_usd, Decimal::ZERO);
    assert!(!b.winning);
}

#[test]
fn booking_ignores_a_resolution_for_another_market_or_token() {
    let p = position("hft-1", "up", dec!(10), 1_000);
    let mut other = win();
    other.condition_id = "cond-2".into();
    assert!(booking_for(&p, &other).is_none());
    let partial = resolution(vec![("other-token", dec!(1))]);
    assert!(booking_for(&p, &partial).is_none());
}

/// The reverse-acceptance target: settle twice, book once.
#[test]
fn settlement_is_idempotent_in_memory() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    let first = book.book(&b, &win(), 10).expect("first booking");
    assert_eq!(first.claim_id, "cond-1");
    assert!(
        book.book(&b, &win(), 11).is_none(),
        "second booking refused"
    );
    assert_eq!(book.applied_count(), 1);
    assert_eq!(book.receivable_usd(), dec!(10));
    // A diag read of the journal: one line, one settlement.
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn settlement_survives_a_restart_and_is_not_rebooked() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    book.book(&b, &win(), 10).unwrap();
    drop(book);

    let mut reopened = SettlementBook::new(Some(&path));
    assert!(reopened.is_applied(&settlement_key(&p)));
    assert_eq!(reopened.receivable_usd(), dec!(10));
    assert_eq!(reopened.pending_claim_count(), 1);
    assert!(reopened.book(&b, &win(), 20).is_none(), "no rebooking");
    // ...and the restored claim is redeemable.
    let due = reopened.take_redemptions(100);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].condition_id, "cond-1");
    assert_eq!(due[0].outcome_shares, vec![dec!(10), Decimal::ZERO]);
    assert!(due[0].neg_risk, "the neg-risk flag survives a restart");
    assert_eq!(due[0].winning_token_id, "up");

    // Confirming it clears the receivable and is durable too. The payout
    // carries its account (empty here: the test positions carry the
    // default account id, so the claim names `default`).
    let (account, payout) = reopened
        .note_confirmed(&due[0].id, Some("0xabc".into()), Some(7), 101)
        .unwrap();
    assert_eq!(payout, dec!(10));
    assert_eq!(account, "default");
    assert_eq!(reopened.receivable_usd(), Decimal::ZERO);
    drop(reopened);

    let mut after = SettlementBook::new(Some(&path));
    assert_eq!(
        after.receivable_usd(),
        Decimal::ZERO,
        "redeemed stays redeemed"
    );
    assert!(after.take_redemptions(1_000_000).is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn recover_reports_positions_whose_close_never_ran() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    book.book(&b, &win(), 10).unwrap();
    // The position book still holds it (the close was interrupted).
    let restored = vec![p.clone()];
    let stale = book.recover(&restored);
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "hft-1");
    // A position that was never settled is not reported.
    let fresh = position("hft-2", "down", dec!(3), 2_000);
    assert!(book.recover(&[fresh]).is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn two_positions_on_one_market_share_a_claim() {
    let (mut book, path) = book_with_journal();
    let up = position("hft-1", "up", dec!(10), 1_000);
    let down = position("hft-2", "down", dec!(4), 1_500);
    let mut r = win();
    r.neg_risk = true;
    let bu = booking_for(&up, &r).unwrap();
    let bd = booking_for(&down, &r).unwrap();
    let c1 = book.book(&bu, &r, 10).unwrap().claim_id;
    let c2 = book.book(&bd, &r, 11).unwrap().claim_id;
    assert_eq!(c1, c2, "one market, one claim");
    let claim = book.claim(&c1).unwrap();
    assert_eq!(claim.outcome_shares, vec![dec!(10), dec!(4)]);
    assert_eq!(claim.payout_usd, dec!(10));
    assert_eq!(claim.position_ids, vec!["hft-1", "hft-2"]);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// ── #647: per-venue settlement queues ───────────────────────────────────

/// A position whose entry stamped a venue (the live path writes the
/// position's execution venue at open) books a claim of THAT venue.
fn position_on_venue(
    id: &str,
    token: &str,
    shares: Decimal,
    entered_ms: i64,
    venue: &str,
) -> OpenPosition {
    let mut p = position(id, token, shares, entered_ms);
    p.venue = venue.into();
    p
}

/// Two plugins hold settlement claims at once (each on its OWN venue's
/// collateral): each venue's drain takes only its own request, take-once —
/// a sibling executor never redeems (or merges) a foreign venue's claim.
/// This is the mis-post the audit's P1-1 predicted for the second venue
/// holding an on-chain settlement position.
#[test]
fn per_venue_drains_take_only_their_own_claims() {
    let (mut book, path) = book_with_journal();
    // polymarket holds a settled win; kalshi holds the same on ITS market.
    let mut poly = position_on_venue("hft-1", "up", dec!(10), 1_000, "polymarket");
    poly.condition_id = "cond-poly".into();
    let r_poly = win_for("cond-poly", "up");
    let bu = booking_for(&poly, &r_poly).unwrap();
    assert_eq!(
        bu.venue, "polymarket",
        "the booking carries the position's venue"
    );
    book.book(&bu, &r_poly, 10).unwrap();

    let mut kal = position_on_venue("hft-2", "kal-up", dec!(4), 1_100, "kalshi");
    kal.condition_id = "cond-kal".into();
    let r_kal = win_for("cond-kal", "kal-up");
    let bk = booking_for(&kal, &r_kal).unwrap();
    book.book(&bk, &r_kal, 11).unwrap();

    // Each drain hands out exactly its own venue's request…
    let poly_due = book.take_redemptions_for_venue("polymarket", true, 100);
    assert_eq!(poly_due.len(), 1);
    assert_eq!(poly_due[0].condition_id, "cond-poly");
    assert_eq!(poly_due[0].venue, "polymarket");
    let kal_due = book.take_redemptions_for_venue("kalshi", true, 100);
    assert_eq!(kal_due.len(), 1);
    assert_eq!(kal_due[0].condition_id, "cond-kal");
    assert_eq!(kal_due[0].venue, "kalshi");
    // …and the dispatch stamp is take-once: neither drain re-sees a claim.
    assert!(
        book.take_redemptions_for_venue("polymarket", true, 100)
            .is_empty()
    );
    assert!(
        book.take_redemptions_for_venue("kalshi", true, 100)
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The untagged fallback: a claim stamped before venue labels existed
/// (empty venue — a legacy journal) drains to the host's single ACTIVE
/// plugin (the `untagged_fallback` arm) and to no sibling. The untagged
/// world keeps the pre-#647 behaviour byte-identically.
#[test]
fn untagged_claims_drain_to_the_active_plugin_fallback_only() {
    let (mut book, path) = book_with_journal();
    let legacy = position("hft-1", "up", dec!(10), 1_000);
    assert!(booking_for(&legacy, &win()).unwrap().venue.is_empty());
    book.book(&booking_for(&legacy, &win()).unwrap(), &win(), 10)
        .unwrap();

    // The active plugin's drain takes the untagged claim…
    let due = book.take_redemptions_for_venue("polymarket", true, 100);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].venue, "", "the request itself stays untagged");
    // …the sibling does not…
    assert!(
        book.take_redemptions_for_venue("kalshi", true, 100)
            .is_empty()
    );
    // …and a no-fallback drain (a handle whose bucket names no plugin,
    // e.g. a named executor that is not the active one) never dispatches
    // the untagged claim either — a second drain stamps nothing.
    let attempts = book.claim("cond-1").unwrap().attempts;
    assert!(
        book.take_redemptions_for_venue("kalshi", false, 6_000)
            .is_empty()
    );
    assert_eq!(
        book.claim("cond-1").unwrap().attempts,
        attempts,
        "no-fallback never dispatched the untagged claim"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The venue attribution survives a restart: a claim booked with a venue
/// replays from the journal still naming it, so post-restart dispatches
/// land on the same executor as pre-restart ones.
#[test]
fn a_claim_venue_survives_a_journal_replay() {
    let (mut book, path) = book_with_journal();
    let p = position_on_venue("hft-1", "up", dec!(10), 1_000, "polymarket");
    book.book(&booking_for(&p, &win()).unwrap(), &win(), 10)
        .unwrap();
    drop(book);

    let mut reopened = SettlementBook::new(Some(&path));
    let due = reopened.take_redemptions_for_venue("polymarket", true, 100);
    assert_eq!(due.len(), 1, "the replayed claim still routes to its venue");
    assert_eq!(due[0].venue, "polymarket");
    // Journals written before the field existed replay untagged: the same
    // restore-or-empty rule the account attribution lives by.
    let mut old = SettlementBook::new(None);
    let legacy = position("hft-2", "up", dec!(3), 2_000);
    old.book(&booking_for(&legacy, &win()).unwrap(), &win(), 20)
        .unwrap();
    assert_eq!(
        old.take_redemptions_for_venue("polymarket", true, 100)
            .len(),
        1,
        "an untagged claim rides the active-plugin fallback"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The merge queue is bucketed by venue the same way: a merge burned on
/// polymarket's pairs drains to polymarket's executor only.
#[test]
fn merge_claims_drain_to_their_own_venue() {
    let (mut book, path) = book_with_journal();
    book.note_merge_burned("cond-poly", "default", "polymarket", false, dec!(5), 10);
    book.note_merge_burned("cond-kal", "default", "kalshi", false, dec!(3), 11);

    let poly = book.take_merges_for_venue("polymarket", true, 100);
    assert_eq!(poly.len(), 1);
    assert_eq!(poly[0].condition_id, "cond-poly");
    assert_eq!(poly[0].venue, "polymarket");
    let kal = book.take_merges_for_venue("kalshi", true, 100);
    assert_eq!(kal.len(), 1);
    assert_eq!(kal[0].condition_id, "cond-kal");
    assert_eq!(kal[0].venue, "kalshi");
    assert!(
        book.take_merges_for_venue("polymarket", true, 100)
            .is_empty()
    );
    assert!(book.take_merges_for_venue("kalshi", true, 100).is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_market_settled_again_after_a_redeem_gets_a_fresh_claim_id() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    let first = book.book(&b, &win(), 10).unwrap().claim_id;
    let due = book.take_redemptions(100);
    book.note_confirmed(&due[0].id, Some("0x1".into()), Some(1), 101);
    assert_eq!(first, "cond-1");
    // A later entry on the same market (a re-open in the same round) settles
    // into a NEW claim: the redeemed id is never reused.
    let p2 = position("hft-9", "up", dec!(2), 5_000);
    let b2 = booking_for(&p2, &win()).unwrap();
    let second = book.book(&b2, &win(), 5_001).unwrap().claim_id;
    assert_eq!(second, "cond-1#2", "the second claim on the market");
    assert_eq!(book.receivable_usd(), dec!(2));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Redemption failure handling: loud (recorded), backed off, and a manual
/// verdict stops the loop instead of hammering the chain.
#[test]
fn failures_are_recorded_and_backed_off() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    let b = booking_for(&p, &win()).unwrap();
    let id = book.book(&b, &win(), 10).unwrap().claim_id;
    assert_eq!(book.take_redemptions(100).len(), 1);
    assert!(
        book.take_redemptions(100).is_empty(),
        "in flight, not re-sent"
    );
    book.note_failure(
        &id,
        &RedemptionFailure {
            message: "nonce too low".into(),
            manual: false,
        },
        200,
    );
    let (attempts, next_attempt_ms, last_error) = {
        let c = book.claim(&id).unwrap();
        (c.attempts, c.next_attempt_ms, c.last_error.clone())
    };
    assert_eq!(last_error.as_deref(), Some("nonce too low"));
    assert_eq!(attempts, 1);
    assert!(next_attempt_ms > 200, "backoff armed");
    assert!(book.take_redemptions(201).is_empty());
    assert_eq!(book.take_redemptions(next_attempt_ms).len(), 1);

    // A manual verdict is terminal: no further automatic attempt.
    book.note_failure(
        &id,
        &RedemptionFailure {
            message: "positions held by another address".into(),
            manual: true,
        },
        300,
    );
    assert_eq!(book.manual_claim_count(), 1);
    assert!(book.take_redemptions(i64::MAX - 1).is_empty());
    // The money is still owed to us, so the receivable stays on the books.
    assert_eq!(book.receivable_usd(), dec!(10));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn blind_detection_needs_an_unanswered_query_older_than_the_grace() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    book.track_markets(std::slice::from_ref(&p), 400_000);
    assert_eq!(book.tracked_markets(), 1);
    let q = book.take_queries(400_000);
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].token_ids, vec!["up".to_string()]);
    assert!(book.blind_since_ms(400_000 + BLIND_AFTER_MS + 1).is_some());
    // A venue that answers "not resolved yet" is alive: not blind, and no
    // storm of re-queries either.
    book.note_answer("cond-1", false, 400_001);
    assert!(
        book.blind_since_ms(400_001 + BLIND_AFTER_MS).is_none(),
        "a fresh answer is a sign of life"
    );
    assert!(book.take_queries(400_001 + REQUERY_MS - 1).is_empty());
    // The re-query clock re-arms after REQUERY_MS; a lost query (no answer
    // at all) is re-sent on the same cadence.
    assert_eq!(book.take_queries(400_001 + REQUERY_MS + 1).len(), 1);
    assert_eq!(book.take_queries(400_001 + 2 * REQUERY_MS + 2).len(), 1);
    // Answer once and then never again: the newest sign of life ages out.
    assert!(
        book.blind_since_ms(400_001 + REQUERY_MS + 1 + BLIND_AFTER_MS)
            .is_some()
    );
    // The alert fires on the healthy → blind edge, once.
    assert!(book.note_blind_state(true));
    assert!(!book.note_blind_state(true));
    assert!(!book.note_blind_state(false));
    assert!(book.note_blind_state(true), "a new episode alerts again");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn unresolved_markets_are_not_tracked_before_expiry() {
    let (mut book, path) = book_with_journal();
    let p = position("hft-1", "up", dec!(10), 1_000);
    // Before the round expires there is nothing to ask the venue.
    assert_eq!(book.track_markets(std::slice::from_ref(&p), 1_500), 0);
    assert_eq!(book.tracked_markets(), 0);
    assert_eq!(book.track_markets(&[p], 400_000), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A market whose recorded end lies decades behind the position's entry is
/// carrying a value that is not a market end (the order path takes
/// `expires_at_ms` from the order's declared slot, and a slot on another
/// clock — the parity fixtures declare `roundSlot: 1` on an epoch clock —
/// lands in 1970). Nothing may ever be settled on it.
#[test]
fn a_market_that_ended_before_we_entered_is_never_tracked() {
    let (mut book, path) = book_with_journal();
    let mut p = position("hft-1", "up", dec!(10), 1_756_000_000_000);
    p.expires_at_ms = 2_000; // `(roundSlot 1 + 1) * 1000ms` against an epoch clock
    let now = 1_756_000_000_001;
    assert_eq!(book.track_markets(std::slice::from_ref(&p), now), 0);
    assert_eq!(book.tracked_markets(), 0);
    assert!(book.take_queries(now).is_empty());

    // A market end just behind the entry is still that market's (a venue fill
    // is reported a little after the round it belongs to): it IS watched.
    let mut late = position("hft-2", "up", dec!(10), now);
    late.expires_at_ms = now - EXPIRY_TRUST_MS + 1;
    assert_eq!(
        book.track_markets(std::slice::from_ref(&late), now + 1),
        1,
        "a late-reported fill on a just-ended round is settleable"
    );
    assert_eq!(book.tracked_markets(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
