#![cfg(test)]

//! Settlement & redemption end-to-end (issue #175), at the `Core` level: a
//! position that reaches its market's resolution has to become cash without ever
//! breaking the accounting identity. These tests own the acceptance criteria —
//! the settle → `run_accounting_audit` → ok proof, the idempotency proof, and the
//! loud-failure proof.

use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use rust_decimal_macros::dec;

pub(super) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-settle-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A dry core whose market expires 1s after the entry round and whose durable
/// logs (positions, trades, settlement journal) all land in `dir`.
pub(super) fn settling_core(dir: &std::path::Path, balance: Decimal) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 1,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: balance,
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        // The order log is NOT inherited from `CoreConfig::default`'s
        // cwd-relative `data/orders/orders.jsonl`: a test that places live
        // orders (or one that shares this cwd and reads the residue as
        // restored BUYs) would cross-contaminate the next `settling_core`
        // in the same process cwd.
        order_log_path: path("orders.jsonl"),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

fn entry_order(price: Decimal, size: Decimal) -> OrderRequest {
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

/// Open a 5-share position at 0.40 (taker: cost 2.00 + 0.036 fee) on a market
/// that expires at t=2000ms, and take the audit anchor with it open.
pub(super) fn open_and_anchor(c: &mut Core, balance: Decimal) {
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 1);
    assert_eq!(c.positions().open_positions()[0].expires_at_ms, 2_000);
    // The book moves to 0.95/0.96 before expiry: the market's own last mid is
    // what the dry resolution pays on (> 0.5 wins, a full 1.00/share).
    c.book_snapshot(
        "tok",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.96), dec!(100))],
        1_998,
    );
    let anchor = c.run_accounting_audit(1_999);
    assert!(anchor.ok, "anchor audit: {}", anchor.summary());
    assert_eq!(c.ledger().balance(), balance - dec!(2.036));
}

/// The resolution the venue would report: the held token pays 1.00/share.
pub(super) fn winning_resolution() -> blitzkrieg_market_api::MarketResolution {
    blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), Decimal::ONE)],
        neg_risk: false,
        source: "test".into(),
    }
}

fn settlement_json(c: &Core) -> serde_json::Value {
    c.engine_stats_at(3_000)["settlement"].clone()
}

/// #377 (治 #2): a market the archive promised a verdict for never enters
/// the dry settlement ladder — the stream settles it. Before this, the
/// ladder's coin-flip refusal (best ≤ 0.5 → withheld) stuck the legs open
/// past expiry forever and tripped the blind alert; and when book evidence
/// DID clear the bar, the dry synthesis could name a different winner than
/// the chain did. Both failure modes die with the flag: no query is
/// stamped, no payout is invented, and the replayed Resolution closes the
/// position exactly once, at the chain's verdict.
#[test]
fn archive_verdict_markets_are_settled_by_the_stream_not_the_dry_ladder() {
    let dir = scratch("archive-verdict");
    let mut c = settling_core(&dir, dec!(10));

    // Declare the market the way the converter's RoundMarkets event now
    // carries it: flagged `archive_verdict` because the dataset holds the
    // on-chain winner for this condition.
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "cond".into(),
                question_id: "q".into(),
                up_token_id: "tok".into(),
                down_token_id: "d".into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: 2_000,
                round_slot: 0,
                round_duration_sec: 1,
                archive_verdict: true,
                venue: String::new(),
                neg_risk: false,
                question: "?".into(),
            }],
            now_ms: 1,
        },
        1,
    );
    // Open the position (entry 0.40, 5 shares) and re-mark the book at
    // 0.95/0.96 — evidence that would DEFINITELY clear the dry ladder's
    // coin-flip bar. Under the flag the ladder stands down regardless.
    open_and_anchor(&mut c, dec!(10));

    // Tick past expiry (2_000) and well past it: no dry settlement may
    // fire, no query may be stamped, no blind alert may trip.
    c.tick(2_100).unwrap();
    c.tick(2_600).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "an archive-verdict market is never settled by the dry ladder"
    );
    let s = settlement_json(&c);
    assert_eq!(
        s["trackedMarkets"],
        serde_json::json!(0),
        "the market never enters the dry watch list"
    );
    assert_eq!(
        s["blindSinceMs"],
        serde_json::json!(None::<i64>),
        "no blind alert: the stream owns this verdict"
    );

    // The stream's verdict arrives (what the replay dispatches) and
    // settles the position exactly once, both sides named in the market's
    // own outcome order.
    c.engine_on_data(
        crate::engine::DataEvent::Resolution {
            condition_id: "cond".into(),
            payouts: vec![
                ("tok".to_string(), Decimal::ONE),
                ("d".to_string(), Decimal::ZERO),
            ],
            neg_risk: false,
            now_ms: 2_700,
        },
        2_700,
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the stream settles the position"
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(closed.exit_price, Decimal::ONE);
    assert_eq!(
        closed.net_pnl_usd,
        dec!(2.964),
        "payout 5 − cost 2 − entry fee 0.036: the chain's verdict, not a dry guess"
    );

    // Provenance: the settlement journal carries the stream as the source.
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    let from_stream = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| v["record"]["source"] == "archive");
    assert!(
        from_stream,
        "the settlement record names the archive as its source: {journal}"
    );

    // The feed counters carry the resolutions count.
    assert_eq!(
        c.engine_stats_at(3_000)["resolutions"],
        serde_json::json!(1),
        "one archive verdict replayed into the core"
    );

    // The accounting identity holds end to end.
    let audit = c.run_accounting_audit(3_000);
    assert!(audit.ok, "audit: {}", audit.summary());
}

/// Declare one binary market (the user's zombie scenario, issue for root
/// cause B): the held leg priced toward zero, the partner leg DECISIVELY
/// bid — the counter-evidence key the dry ladder has been blind to.
fn declare_partner_market(c: &mut Core, up: &str, down: &str) {
    // The stamp reads the scanner's market table, so the fixture core
    // needs the engine the production path has.
    if !c.has_engine() {
        c.enable_engine(crate::engine::Engine::new(
            crate::engine::EngineConfig::default(),
        ));
    }
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "cond".into(),
                question_id: "q".into(),
                up_token_id: up.into(),
                down_token_id: down.into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: 2_000,
                round_slot: 0,
                round_duration_sec: 1,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: false,
                question: "?".into(),
            }],
            now_ms: 1,
        },
        1,
    );
}

/// Counter-evidence settlement: our leg's own book is dead (bid toward
/// zero, below the coin-flip bar) but the PARTNER token is decisively bid
/// at 0.99 — the market has already priced the winner. The old ladder saw
/// only `query.token_ids` (our own leg), found no evidence above 0.5, and
/// withheld forever: the position sat past expiry blocking capacity
/// (the ETH/BTC UP zombie positions). The partner's decisive bid is the
/// venue's own pricing of the verdict — at expiry nobody pays 0.99 for a
/// coin still in the air. Our leg settles as the loser, at 0.
#[test]
fn a_decisive_partner_bid_settles_held_leg_as_the_loser() {
    let dir = scratch("partner-loser");
    let mut c = settling_core(&dir, dec!(10));
    declare_partner_market(&mut c, "tok", "d");

    // Entry at 0.40 (taker) — `entry_order` stamps the partner from the
    // market table declared above.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(
        c.positions().open_positions()[0].partner_token_id,
        "d",
        "the entry stamps the condition's other token"
    );

    // Expiry: our leg's book dies toward zero (bid 0.02, below the
    // coin-flip bar) while the partner is decisively bid at 0.99.
    c.book_snapshot("tok", vec![(dec!(0.02), dec!(100))], vec![], 1_998);
    c.book_snapshot("d", vec![(dec!(0.99), dec!(100))], vec![], 1_998);

    c.tick(2_100).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "the partner's decisive bid settles our leg as the loser — no zombie"
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(
        closed.exit_price,
        Decimal::ZERO,
        "a named loser pays 0 — the money was lost on the trade, not invented here"
    );
    assert_eq!(
        closed.net_pnl_usd,
        dec!(-2.036),
        "cost 2 + fee 0.036, fully realized: the loss is on the books"
    );

    // Provenance: this is a DRY synthesis naming the partner the winner —
    // the journal must say so.
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    let rec = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["record"]["positionId"] == c.positions().closed_positions()[0].id)
        .expect("the settlement is journaled");
    assert_eq!(rec["record"]["source"], "core-dry");
    assert_eq!(
        rec["record"]["winningTokenId"],
        serde_json::json!("d"),
        "the synthesized verdict names the partner the winner"
    );
    assert_eq!(
        rec["record"]["payoutPerShare"],
        serde_json::json!(0.0),
        "our leg is the loser: nothing to redeem, nothing invented"
    );

    // The accounting identity holds end to end.
    let audit = c.run_accounting_audit(2_500);
    assert!(audit.ok, "audit: {}", audit.summary());
}

/// The gate is the partner's decisiveness, not its mere existence: a
/// partner bid at 0.70 (a market still in doubt) settles nothing. The
/// threshold is Gamma's own DECISIVE_HIGH (0.99) — the same bar the live
/// venue path requires before it calls a payout final.
#[test]
fn an_undecided_partner_bid_keeps_the_old_withhold() {
    let dir = scratch("partner-undecided");
    let mut c = settling_core(&dir, dec!(10));
    declare_partner_market(&mut c, "tok", "d");

    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);

    c.book_snapshot("tok", vec![(dec!(0.02), dec!(100))], vec![], 1_998);
    c.book_snapshot("d", vec![(dec!(0.70), dec!(100))], vec![], 1_998);

    c.tick(2_100).unwrap();

    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "a 0.70 partner bid is not a verdict — the position stays open loudly"
    );
}

/// Old snapshots carry no partner (empty string): no counter evidence,
/// no counter verdict — the old withhold stands. The pre-existing zombie
/// positions stay on the manual path; this fix only stops NEW ones. The
/// fixture opens the position with NO market declared (the restore
/// situation: the scanner has no row for that condition), so the entry
/// stamps an empty partner.
#[test]
fn a_partnerless_position_withholds_as_before() {
    let dir = scratch("partnerless");
    let mut c = settling_core(&dir, dec!(10));

    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(
        c.positions().open_positions()[0].partner_token_id,
        "",
        "no market on the scanner — nothing to stamp"
    );

    c.book_snapshot("tok", vec![(dec!(0.02), dec!(100))], vec![], 1_998);
    c.book_snapshot("d", vec![(dec!(0.99), dec!(100))], vec![], 1_998);

    c.tick(2_100).unwrap();

    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "no partner stamped — no counter verdict (the manual path stays)"
    );
}

/// Acceptance: settle → `run_accounting_audit` → ok, with the payout held as a
/// receivable (not cash) until the redemption confirms.
#[test]
fn settlement_keeps_the_accounting_identity_true() {
    let dir = scratch("identity");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    let balance_before = c.ledger().balance();

    c.on_market_resolution(winning_resolution(), 2_001);

    // The position is closed at its redemption value, reason `settlement`.
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(closed.exit_price, Decimal::ONE);
    assert_eq!(
        closed.net_pnl_usd,
        dec!(2.964),
        "payout 5 − cost 2 − entry fee 0.036"
    );
    let record = serde_json::to_value(crate::trade_db::TradeRecord::from_closed(closed)).unwrap();
    assert_eq!(record["exitReason"], "settlement");
    assert_eq!(record["netPnlUsd"], serde_json::json!(2.964));

    // Cash did NOT move: the payout is an account receivable (the wallet
    // holds conditional tokens, not USDC, until the redeem is mined).
    assert_eq!(c.ledger().balance(), balance_before);
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
    assert_eq!(stats["settledPositions"], serde_json::json!(1));
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(1));
    assert_eq!(
        stats["trackedMarkets"],
        serde_json::json!(0),
        "nothing left to watch"
    );

    // The audit is green with the receivable on the books: `total` explains
    // the settlement exactly (Δrealized 2.964 + Δspent 2.036 = 5.0).
    let report = c.run_accounting_audit(2_002);
    assert!(report.ok, "audit after settlement: {}", report.summary());

    // The claim the venue has to execute.
    let due = c.take_pending_redemptions(2_003);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "cond");
    assert_eq!(due[0].condition_id, "cond");
    assert_eq!(due[0].outcome_shares, vec![dec!(5), dec!(0)]);
    assert_eq!(due[0].expected_payout_usd, dec!(5));
    assert!(!due[0].neg_risk);
    assert!(
        c.take_pending_redemptions(2_003).is_empty(),
        "not re-sent in flight"
    );

    // The redemption confirms: receivable → cash, and `total` does not move.
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: "cond".into(),
            condition_id: "cond".into(),
            tx_hash: Some("0xabc".into()),
            block_number: Some(42),
            failure: None,
            at_ms: 2_004,
        },
        2_004,
    );
    assert_eq!(c.ledger().balance(), balance_before + dec!(5));
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
    assert_eq!(stats["redeemedClaims"], serde_json::json!(1));
    let report = c.run_accounting_audit(2_005);
    assert!(report.ok, "audit after redemption: {}", report.summary());
    assert_eq!(
        c.engine_stats_at(2_005)["accountingAudit"]["receivableUsd"],
        serde_json::json!(0.0)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Idempotency: a re-delivered resolution (the venue's own retry, a restart,
/// a duplicated answer) must not book, close or pay the same position twice.
#[test]
fn a_redelivered_resolution_does_not_settle_twice() {
    let dir = scratch("idempotent");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));

    c.on_market_resolution(winning_resolution(), 2_001);
    // The same answer again, and a second query round's answer as well.
    c.on_market_resolution(winning_resolution(), 2_002);
    c.on_market_resolution(winning_resolution(), 2_003);

    assert_eq!(c.positions().closed_positions().len(), 1, "one close only");
    let trades = c.trade_summary(None);
    assert_eq!(trades["totalTrades"], serde_json::json!(1));
    assert_eq!(trades["totalNetPnl"], serde_json::json!(2.964));
    let stats = settlement_json(&c);
    assert_eq!(
        stats["receivableUsd"],
        serde_json::json!(5.0),
        "paid once, not 15"
    );
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(1));
    assert!(c.run_accounting_audit(2_004).ok);

    // The same resolution after the claim was redeemed: still nothing new.
    let due = c.take_pending_redemptions(2_005);
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: due[0].id.clone(),
            condition_id: "cond".into(),
            tx_hash: Some("0xabc".into()),
            block_number: Some(42),
            failure: None,
            at_ms: 2_006,
        },
        2_006,
    );
    c.on_market_resolution(winning_resolution(), 2_007);
    assert_eq!(c.ledger().balance(), dec!(10) - dec!(2.036) + dec!(5));
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A redemption failure is loud, credited with nothing, retried on the
/// book's backoff, and never blocks the next entry.
#[test]
fn a_failed_redemption_is_loud_and_retried() {
    let dir = scratch("failure");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    c.on_market_resolution(winning_resolution(), 2_001);
    let balance_before = c.ledger().balance();

    let due = c.take_pending_redemptions(2_002);
    let id = due[0].id.clone();
    let mut errors = Vec::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: id.clone(),
            condition_id: "cond".into(),
            tx_hash: None,
            block_number: None,
            failure: Some(blitzkrieg_market_api::RedemptionFailure {
                message: "nonce too low".into(),
                manual: false,
            }),
            at_ms: 2_003,
        },
        2_003,
    );
    while let Ok(ev) = rx.try_recv() {
        if let Event::Error { error } = ev {
            errors.push(error.message);
        }
    }
    assert_eq!(errors.len(), 1, "a failed redemption must be loud");
    assert!(errors[0].contains("nonce too low"), "{}", errors[0]);
    // Nothing was credited, and the money is still owed to us.
    assert_eq!(c.ledger().balance(), balance_before);
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
    assert_eq!(stats["retryableRedemptions"], serde_json::json!(1));
    assert_eq!(stats["lastError"]["message"], "nonce too low");
    assert!(
        c.run_accounting_audit(2_004).ok,
        "a failed redeem is not a books failure"
    );

    // Backoff: not re-attempted immediately, attempted again once armed.
    assert!(c.take_pending_redemptions(2_005).is_empty());
    let retry_at = c.settlement.claim(&id).unwrap().next_attempt_ms;
    let due = c.take_pending_redemptions(retry_at);
    assert_eq!(due.len(), 1, "the retry is armed");

    // A failed redemption never blocks a new entry (a different asset, so
    // the settlement's exit cooldown on this one does not mask the result).
    c.book_snapshot("tok2", vec![], vec![(dec!(0.40), dec!(100))], 2_006);
    let order = OrderRequest {
        token_id: "tok2".into(),
        condition_id: "cond2".into(),
        asset: "ETH".into(),
        ..entry_order(dec!(0.40), dec!(2))
    };
    let (_id, status) = c.place(order, 0, 2_006).unwrap();
    assert_eq!(status, OrderStatus::Filled, "entries keep working");

    // The retry lands: exactly one credit.
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id,
            condition_id: "cond".into(),
            tx_hash: Some("0xdef".into()),
            block_number: Some(43),
            failure: None,
            at_ms: retry_at + 1,
        },
        retry_at + 1,
    );
    assert_eq!(
        c.ledger().balance(),
        balance_before + dec!(5) - dec!(0.8) - dec!(0.0144)
    );
    // A duplicate confirmation for an already-redeemed claim credits nothing.
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: "cond".into(),
            condition_id: "cond".into(),
            tx_hash: Some("0xdef".into()),
            block_number: Some(43),
            failure: None,
            at_ms: retry_at + 2,
        },
        retry_at + 2,
    );
    assert_eq!(
        c.ledger().balance(),
        balance_before + dec!(5) - dec!(0.8) - dec!(0.0144)
    );
    assert!(c.run_accounting_audit(retry_at + 3).ok, "audit stays green");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manual verdict (the signer cannot move the positions) is reported, not
/// retried forever.
#[test]
fn a_manual_redemption_verdict_stops_the_retries() {
    let dir = scratch("manual");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    c.on_market_resolution(winning_resolution(), 2_001);
    let due = c.take_pending_redemptions(2_002);
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: due[0].id.clone(),
            condition_id: "cond".into(),
            tx_hash: None,
            block_number: None,
            failure: Some(blitzkrieg_market_api::RedemptionFailure {
                message: "positions are held by another address".into(),
                manual: true,
            }),
            at_ms: 2_003,
        },
        2_003,
    );
    let stats = settlement_json(&c);
    assert_eq!(stats["manualRedemptions"], serde_json::json!(1));
    assert_eq!(stats["retryableRedemptions"], serde_json::json!(0));
    assert!(
        c.take_pending_redemptions(i64::MAX / 2).is_empty(),
        "no automatic attempt after a manual verdict"
    );
    assert_eq!(
        stats["receivableUsd"],
        serde_json::json!(5.0),
        "still owed to us"
    );
    assert!(c.run_accounting_audit(2_004).ok);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A worthless leg is closed at 0 (the loss was already cash) and books no
/// claim: there is nothing on-chain to redeem.
#[test]
fn a_losing_position_settles_to_zero_without_a_claim() {
    let dir = scratch("loser");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    assert!(c.run_accounting_audit(1_999).ok);

    c.on_market_resolution(
        blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("tok".to_string(), Decimal::ZERO)],
            neg_risk: false,
            source: "test".into(),
        },
        2_001,
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(
        closed.net_pnl_usd,
        dec!(-2.036),
        "cost + entry fee, nothing back"
    );
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
    assert!(
        c.take_pending_redemptions(2_002).is_empty(),
        "nothing to redeem"
    );
    let report = c.run_accounting_audit(2_003);
    assert!(report.ok, "audit after a loss: {}", report.summary());
    // Total cash is short by exactly the loss: the money is accounted for,
    // not held hostage by a position nobody can convert.
    assert_eq!(c.ledger().balance(), dec!(10) - dec!(2.036));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A resolution that does not price a held token leaves the position open and
/// says so: inventing a payout would be worse than the drift it fixes.
#[test]
fn an_unpriced_token_is_reported_and_left_open() {
    let dir = scratch("unpriced");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.on_market_resolution(
        blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("someone-elses-token".to_string(), Decimal::ONE)],
            neg_risk: false,
            source: "test".into(),
        },
        2_001,
    );
    let mut errors = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Error { error } = ev {
            errors.push(error.message);
        }
    }
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("prices no outcome"), "{}", errors[0]);
    assert_eq!(c.positions().open_positions().len(), 1, "still open");
    assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(0.0));
    assert!(
        c.run_accounting_audit(2_002).ok,
        "an open position is not drift"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// E29 (§12.2) throttle semantics, at the Core: the still-growing bars
/// preview at most 1/s per `(symbol, interval)` on the tick clock, while
/// CLOSED bars push IMMEDIATELY from `engine_on_data` — never folded, even
/// when nine intervals close at once and the wall clock hasn't moved (a
/// swallowed close would leave the chart missing that bar forever).
#[test]
fn kline_previews_are_throttled_but_closes_are_never_folded() {
    use crate::kline::KlineInterval;
    use rust_decimal_macros::dec;
    let mut c = Core::new(CoreConfig::default());
    CoreConfig::default()
        .install_engine(&mut c)
        .expect("engine install");
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    let feed = |c: &mut Core, ts_ms: i64, price: Decimal| {
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".to_string(),
                bids: vec![(price, Decimal::ONE)],
                asks: vec![(price + Decimal::ONE, Decimal::ONE)],
                now_ms: ts_ms,
            },
            ts_ms,
        );
    };
    // Drain the sink, splitting closed from preview pushes.
    let drain = |rx: &mut mpsc::UnboundedReceiver<Event>| -> (Vec<Event>, Vec<Event>) {
        let mut closed = Vec::new();
        let mut previews = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::KlineUpdate { ref kline } = ev {
                if kline.is_closed {
                    closed.push(ev);
                } else {
                    previews.push(ev);
                }
            }
        }
        (closed, previews)
    };

    // One bucket @60_000 (min1) with two ticks inside it. Previews ride
    // the TICK clock, so tick first.
    feed(&mut c, 60_000, dec!(1));
    feed(&mut c, 60_100, dec!(2));
    c.tick(60_200).unwrap();
    // First preview per bucket: every interval's growing bar pushes ONCE.
    let (_, previews) = drain(&mut rx);
    let min1_previews = previews
        .iter()
        .filter(|ev| matches!(ev, Event::KlineUpdate { kline } if kline.interval == KlineInterval::Min1))
        .count();
    assert_eq!(min1_previews, 1, "first preview per bucket: {previews:?}");
    assert_eq!(previews.len(), 9, "one preview per configured interval");

    // Half a second later: NOTHING new — the 1/s fold holds per bucket.
    c.tick(60_700).unwrap();
    let (closed, previews) = drain(&mut rx);
    assert!(closed.is_empty() && previews.is_empty(), "throttled hard");

    // Past the 1s boundary: the preview releases, again exactly once.
    c.tick(61_250).unwrap();
    let (_, previews) = drain(&mut rx);
    assert_eq!(previews.len(), 9, "released after the fold window");

    // Now cross the bucket: @120_000 closes exactly the intervals whose
    // bucket boundary lies in (60_000, 120_000] — Sec1/Sec5/Sec15/Min1,
    // four bars at the same wall-clock instant — and NONE of them is
    // throttled. (Min5/Min15/Hour1/Hour4/Day1 keep their bars open:
    // @60_000 and @120_000 share one Min5 bucket, folding not closing.)
    feed(&mut c, 120_000, dec!(3));
    let (closed, previews) = drain(&mut rx);
    assert!(previews.is_empty(), "the feed itself never previews");
    assert_eq!(
        closed.len(),
        4,
        "一 bar 一条，一条不少: every bucket boundary crossed closes, never folded"
    );
    for ev in &closed {
        let Event::KlineUpdate { kline } = ev else {
            unreachable!("filtered above")
        };
        assert_eq!(kline.open_time_ms, 60_000, "{kline:?}");
    }

    // A preview in the SAME wall second as a close cannot suppress it.
    c.tick(120_050).unwrap(); // previews the bars
    let (_, previews) = drain(&mut rx);
    assert_eq!(previews.len(), 9);
    feed(&mut c, 180_000, dec!(4)); // closes Sec1/5/15/Min1 again, same wall second
    let (closed, previews) = drain(&mut rx);
    assert!(previews.is_empty());
    assert_eq!(closed.len(), 4, "closes are never subject to the fold");
}

/// Dry mode answers its own queries on the tick: the whole path (query →
/// resolution → close → receivable → simulated redemption) runs without a
/// chain, and the ledger ends up holding the payout as cash.
#[test]
fn dry_mode_settles_on_its_own_tick_without_a_chain() {
    let dir = scratch("dry-tick");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    let balance_before = c.ledger().balance();

    c.tick(2_001).unwrap();

    assert!(c.positions().open_positions().is_empty());
    assert_eq!(
        c.positions().closed_positions()[0].exit_reason,
        ExitReason::Settlement
    );
    // The simulated redemption confirmed, so the payout is cash.
    assert_eq!(c.ledger().balance(), balance_before + dec!(5));
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
    assert_eq!(stats["redeemedClaims"], serde_json::json!(1));
    assert_eq!(stats["settledPositions"], serde_json::json!(1));
    assert!(c.run_accounting_audit(2_002).ok);
    // The journal records the dry redemption as dry, never as a real tx.
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    assert!(journal.contains("dry-simulated"), "{journal}");
    assert!(journal.contains("\"kind\":\"closed\""), "{journal}");

    // Nothing settles twice on later ticks.
    c.tick(2_500).unwrap();
    assert_eq!(c.ledger().balance(), balance_before + dec!(5));
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Reverse acceptance (calibration): a bid-less WINNER must settle at $1,
/// never at 0. The buy side being swept is not evidence the token lost —
/// at expiry nobody sells a sure dollar cheap, so the ask IS the verdict
/// when there is no bid. The pre-calibration `dry_resolution` read the
/// one-sided mid (zeroed by `from_levels`), fell through to a stale
/// valuation, found nothing above the coin-flip and booked 0 for everyone:
/// a real winner silently zeroed. This test is RED on that code.
#[test]
fn a_bidless_winner_settles_at_one_not_zero() {
    let dir = scratch("bidless-winner");
    let mut c = settling_core(&dir, dec!(10));
    // Entry at 0.40 (taker: cost 2.00 + 0.036 fee) on the expiring market.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    // The buy side is swept before expiry: NO bid at all, only an ask at
    // 0.97 — nobody offers a dollar, nobody bids for one either.
    c.book_snapshot("tok", vec![], vec![(dec!(0.97), dec!(100))], 1_998);

    c.tick(2_001).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "settled, not held: the ask is evidence enough"
    );
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(
        closed.exit_price,
        Decimal::ONE,
        "the winner pays $1/share — the old code booked 0 here"
    );
    assert_eq!(
        closed.net_pnl_usd,
        dec!(2.964),
        "payout 5 − cost 2 − entry fee 0.036"
    );
    // The simulated redemption confirmed: the payout is cash.
    assert_eq!(c.ledger().balance(), dec!(10) - dec!(2.036) + dec!(5));
    assert!(c.run_accounting_audit(2_002).ok);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Reverse acceptance (calibration): with NO evidence above the coin-flip
/// the resolution is WITHHELD, not booked as a zero payout. A binary
/// market always has a winner on-chain; "our view is dark" must hold the
/// position open (loudly, via the blind alert) until evidence arrives —
/// and the query must re-arm so late evidence still settles it.
#[test]
fn a_dark_market_withholds_settlement_instead_of_zeroing() {
    let dir = scratch("dark-withhold");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
    assert_eq!(status, OrderStatus::Filled);
    // Sub-coin-flip evidence only: a lone 0.30 bid (or nothing) must not
    // name a winner.
    c.book_snapshot("tok", vec![(dec!(0.30), dec!(100))], vec![], 1_998);

    c.tick(2_001).unwrap();

    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "held open: booking 0 here zeroes a real winner"
    );
    let stats = settlement_json(&c);
    assert_eq!(stats["settledPositions"], serde_json::json!(0));
    assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));

    // Evidence arrives late (the re-armed query): the market settles.
    c.book_snapshot("tok", vec![], vec![(dec!(0.97), dec!(100))], 120_000);
    c.tick(130_000).unwrap();
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(closed.exit_price, Decimal::ONE);
    assert!(c.run_accounting_audit(130_001).ok);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Reverse acceptance (calibration): the venue's own answer is booked as it
/// reports it — an UMA-invalid market pays 0.5/share to BOTH sides, and
/// that half is real collateral, not a rounding artifact to snap to 0 or 1.
/// The booking layer takes any payout the venue reports; the DRY source
/// keeps its own 0/1 verdict shape (pinned by the bid-less tests above) —
/// two sources, both traceable through the journal's `source` field.
#[test]
fn an_uma_invalid_half_payout_books_and_redeems_as_half() {
    let dir = scratch("uma-invalid");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    let balance_before = c.ledger().balance();

    c.on_market_resolution(
        blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("tok".to_string(), dec!(0.5))],
            neg_risk: false,
            source: "uma-invalid".into(),
        },
        2_001,
    );

    // The position closes at exactly the reported half — 0 would zero real
    // collateral, 1 would invent a winner.
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.exit_reason, ExitReason::Settlement);
    assert_eq!(closed.exit_price, dec!(0.5));
    assert_eq!(
        closed.net_pnl_usd,
        dec!(0.464),
        "payout 2.5 − cost 2 − entry fee 0.036"
    );

    // The half is held as a receivable until the redeem confirms.
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(2.5));
    assert_eq!(stats["pendingRedemptions"], serde_json::json!(1));
    assert!(c.run_accounting_audit(2_002).ok);

    // The venue-sourced redemption credits 2.5 to cash.
    let due = c.take_pending_redemptions(2_002);
    assert_eq!(due.len(), 1);
    c.on_redemption_result(
        blitzkrieg_market_api::RedemptionResult {
            id: due[0].id.clone(),
            condition_id: "cond".into(),
            tx_hash: Some("0xinvalid".into()),
            block_number: Some(42),
            failure: None,
            at_ms: 2_003,
        },
        2_003,
    );
    assert_eq!(c.ledger().balance(), balance_before + dec!(2.5));
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
    assert_eq!(stats["redeemedClaims"], serde_json::json!(1));
    assert!(c.run_accounting_audit(2_004).ok);

    // The journal carries the source verbatim: a simulated settlement can
    // never be mistaken for a venue one (and vice versa).
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    assert!(journal.contains("\"source\":\"uma-invalid\""), "{journal}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A two-legged BUY order on one condition (pair_discount_arb's shape).
fn pair_order(
    token: &str,
    direction: &str,
    price: Decimal,
    size: Decimal,
    key: &str,
) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "pair_discount_arb".into(),
        asset: "BTC".into(),
        direction: direction.into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

/// Acceptance (task 2.2): a strategy close intent tagged "merge" collects
/// the pair's collateral — $1.00 per complete share-pair — through
/// `PositionManager::merge_condition` + `Ledger::credit_merge`, closes both
/// legs as `ExitReason::Merge` at $0.50/share, and the E17 accounting
/// identity survives: the trade records' exit notionals (2×$2.50) sum to
/// exactly the $5.00 the ledger credited. RED before the MERGE channel
/// existed: the intent was read as a SELL of one leg at the market price.
#[test]
fn a_merge_intent_collects_one_dollar_per_pair_and_the_audit_stays_true() {
    let dir = scratch("merge-pair");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok-up", vec![], vec![(dec!(0.48), dec!(100))], 1);
    let (_id, status) = c
        .place(
            pair_order("tok-up", "up", dec!(0.48), dec!(5), "k-up"),
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    // Second leg, same condition, opposite side. Entry gates would refuse
    // it here ("Already in BTC") — this kernel has no engine to read a
    // hold-to-settlement declaration from; production reaches this state
    // through the pair-completion exemption, so the test opens it gate-free.
    c.book_snapshot("tok-down", vec![], vec![(dec!(0.45), dec!(100))], 1);
    let outcome = c
        .place_inner(
            pair_order("tok-down", "down", dec!(0.45), dec!(5), "k-down"),
            0,
            1,
            false,
        )
        .unwrap();
    assert_eq!(outcome.status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 2);
    let after_entries = c.ledger().balance();
    // Anchor BEFORE the collection, so the audit must explain the $5 move.
    assert!(c.run_accounting_audit(2).ok);

    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok-up".into(),
            reason: "merge".into(),
            account_id: crate::account::default_account_id().to_string(),
        });
    c.tick(3).unwrap();

    assert!(
        c.positions().open_positions().is_empty(),
        "both legs consumed by the merge"
    );
    let closed = c.positions().closed_positions();
    assert_eq!(closed.len(), 2);
    assert!(closed.iter().all(|r| r.exit_reason == ExitReason::Merge));
    assert!(
        closed.iter().all(|r| r.exit_price == dec!(0.50)),
        "each leg leaves at $0.50/share so the pair's notionals sum to $1"
    );
    // Gross per leg: 5 shares × $0.50 = $2.50 notional − basis (fees are
    // zero on the merge itself).
    let up = closed
        .iter()
        .find(|r| r.direction == SignalDirection::Up)
        .unwrap();
    let down = closed
        .iter()
        .find(|r| r.direction == SignalDirection::Down)
        .unwrap();
    assert_eq!(up.pnl_usd, dec!(2.5) - dec!(2.4));
    assert_eq!(down.pnl_usd, dec!(2.5) - dec!(2.25));
    // #461: the burned $5 is a RECEIVABLE at burn time, NOT cash — the
    // balance is untouched until the merge receipt arrives (dry simulates
    // it on a LATER tick: the burn happens inside `run_exit_checks`, after
    // this tick's settlement pass already ran).
    assert_eq!(c.ledger().balance(), after_entries, "no cash until mined");
    let audit_mid = c.engine_stats_at(4)["accountingAudit"].clone();
    assert_eq!(
        audit_mid["mergeReceivableUsd"],
        serde_json::json!(5.0),
        "the $5 pends as a merge receivable"
    );
    // Next tick: the dry merge receipt lands, exactly $1 per share-pair.
    c.tick(4).unwrap();
    assert_eq!(c.ledger().balance(), after_entries + dec!(5));
    // Trade records and cash agree — the identity survives the new channel.
    let report = c.run_accounting_audit(5);
    assert!(report.ok, "audit after merge: {}", report.summary());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Reverse acceptance (task 2.2): a merge intent with NO complete pair
/// behind it collects nothing — one-sided inventory waits for settlement,
/// the ledger is untouched, no trade record appears.
#[test]
fn a_merge_intent_without_a_pair_collects_nothing() {
    let dir = scratch("merge-one-sided");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok-up", vec![], vec![(dec!(0.48), dec!(100))], 1);
    let (_id, status) = c
        .place(
            pair_order("tok-up", "up", dec!(0.48), dec!(5), "k-up"),
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    let after_entry = c.ledger().balance();

    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok-up".into(),
            reason: "merge".into(),
            account_id: crate::account::default_account_id().to_string(),
        });
    c.tick(3).unwrap();

    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "one-sided inventory waits for settlement"
    );
    assert!(c.positions().closed_positions().is_empty());
    assert_eq!(c.ledger().balance(), after_entry, "no pair, no collateral");
    assert!(c.run_accounting_audit(4).ok);

    let _ = std::fs::remove_dir_all(&dir);
}

/// #461 Fix 1 reverse acceptance: a FAILED merge receipt does NOT credit
/// cash — a retryable failure re-arms on the backoff and a manual one
/// stops the automatic retry for good. The receivable survives both.
#[test]
fn a_failed_merge_receipt_never_credits_cash() {
    let dir = scratch("merge-failed");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok-up", vec![], vec![(dec!(0.48), dec!(100))], 1);
    let (_id, status) = c
        .place(
            pair_order("tok-up", "up", dec!(0.48), dec!(5), "k-up"),
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    c.book_snapshot("tok-down", vec![], vec![(dec!(0.45), dec!(100))], 1);
    let outcome = c
        .place_inner(
            pair_order("tok-down", "down", dec!(0.45), dec!(5), "k-down"),
            0,
            1,
            false,
        )
        .unwrap();
    assert_eq!(outcome.status, OrderStatus::Filled);
    let after_entries = c.ledger().balance();

    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok-up".into(),
            reason: "merge".into(),
            account_id: crate::account::default_account_id().to_string(),
        });
    c.tick(3).unwrap();
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(c.ledger().balance(), after_entries, "burn ≠ cash");

    // A retryable failure: balance untouched, claim still pending.
    let failure = blitzkrieg_market_api::MergeResult::failure(
        &blitzkrieg_market_api::MergeRequest {
            id: "cond#merge".into(),
            condition_id: "cond".into(),
            neg_risk: false,
            pairs_usd: dec!(5),
            venue: String::new(),
        },
        "RPC connect failed",
        false,
        5,
    );
    c.on_merge_result(failure, 5);
    assert_eq!(
        c.ledger().balance(),
        after_entries,
        "failure credits nothing"
    );
    assert_eq!(c.settlement.merge_receivable_usd(), dec!(5));

    // Backoff stamped: nothing due within the retry window…
    assert!(c.settlement.take_merges(5 + 4_999).is_empty());
    // …and the claim is due again after it.
    assert_eq!(c.settlement.take_merges(5 + 5_000).len(), 1);

    // A MANUAL failure (e.g. neg-risk merge the bridge cannot run): the
    // claim never dispatches again — it waits for the operator.
    let manual = blitzkrieg_market_api::MergeResult::failure(
        &blitzkrieg_market_api::MergeRequest {
            id: "cond#merge".into(),
            condition_id: "cond".into(),
            neg_risk: false,
            pairs_usd: dec!(5),
            venue: String::new(),
        },
        "neg-risk merge is not automated",
        true,
        6,
    );
    c.on_merge_result(manual, 6);
    assert_eq!(
        c.settlement.take_merges(i64::MAX).len(),
        0,
        "manual never retried"
    );
    assert_eq!(c.settlement.merge_receivable_usd(), dec!(5));
    // Cash still untouched through every failure.
    assert_eq!(c.ledger().balance(), after_entries);
    assert!(
        c.run_accounting_audit(7).ok,
        "audit with a stuck merge claim"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #461 Fix 1 reverse acceptance: a merge success for an UNKNOWN claim is
/// ignored — no cash appears, no journal entry, nothing to replay.
#[test]
fn a_merge_result_for_an_unknown_claim_credits_nothing() {
    let dir = scratch("merge-unknown");
    let mut c = settling_core(&dir, dec!(10));
    let balance = c.ledger().balance();
    let ghost = blitzkrieg_market_api::MergeResult::success(
        &blitzkrieg_market_api::MergeRequest {
            id: "ghost#merge".into(),
            condition_id: "ghost".into(),
            neg_risk: false,
            pairs_usd: dec!(9),
            venue: String::new(),
        },
        "0xdeadbeef",
        Some(42),
        5,
    );
    c.on_merge_result(ghost, 5);
    assert_eq!(c.ledger().balance(), balance, "unknown claim pays nothing");
    assert!(c.run_accounting_audit(6).ok);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #461 Fix 1: the burned-pair claim survives a restart — the journal
/// replays it as a pending merge receivable, the dry simulator confirms it
/// on the next tick, and the cash lands exactly once.
#[test]
fn a_merge_claim_survives_a_restart_and_credits_once() {
    let dir = scratch("merge-restart");
    let mut c = settling_core(&dir, dec!(10));
    c.book_snapshot("tok-up", vec![], vec![(dec!(0.48), dec!(100))], 1);
    let (_id, status) = c
        .place(
            pair_order("tok-up", "up", dec!(0.48), dec!(5), "k-up"),
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    c.book_snapshot("tok-down", vec![], vec![(dec!(0.45), dec!(100))], 1);
    let outcome = c
        .place_inner(
            pair_order("tok-down", "down", dec!(0.45), dec!(5), "k-down"),
            0,
            1,
            false,
        )
        .unwrap();
    assert_eq!(outcome.status, OrderStatus::Filled);

    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok-up".into(),
            reason: "merge".into(),
            account_id: crate::account::default_account_id().to_string(),
        });
    c.tick(3).unwrap();
    assert_eq!(c.settlement.merge_receivable_usd(), dec!(5));

    // The process dies between the burn and the receipt.
    drop(c);
    let mut reborn = settling_core(&dir, dec!(10));
    reborn.restore_orders();
    assert_eq!(
        reborn.settlement.merge_receivable_usd(),
        dec!(5),
        "the claim replays from the journal"
    );
    // The reborn core's balance comes from its own seed/restore; what
    // matters is that the $5 claim is NOT cash yet, and the delta after
    // the receipt is exactly $5.
    let reborn_base = reborn.ledger().balance();

    // Next tick after restart: the dry receipt lands, exactly once.
    reborn.tick(9).unwrap();
    assert_eq!(reborn.ledger().balance(), reborn_base + dec!(5));
    reborn.tick(10).unwrap();
    assert_eq!(
        reborn.ledger().balance(),
        reborn_base + dec!(5),
        "a confirmed claim is never paid twice"
    );
    assert_eq!(reborn.settlement.merge_receivable_usd(), dec!(0));
    assert!(reborn.run_accounting_audit(11).ok);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash between the settlement commit and the close is repaired on the
/// next start, exactly once.
#[test]
fn an_interrupted_settlement_recovers_once_on_restart() {
    let dir = scratch("recover");
    let balance = dec!(10);
    let mut c = settling_core(&dir, balance);
    open_and_anchor(&mut c, balance);
    let pos = c.positions().open_positions()[0].clone();
    c.persist_positions();
    drop(c); // the process dies here

    // What the crash left behind: the settlement is durable, the close is not.
    {
        let journal = dir.join("settlements.jsonl");
        let mut book = crate::settlement::SettlementBook::new(Some(&journal));
        let booking =
            crate::settlement::booking_for(&pos, &winning_resolution()).expect("bookable");
        assert!(book.book(&booking, &winning_resolution(), 2_001).is_some());
        assert_eq!(book.receivable_usd(), dec!(5));
    }

    // Restart: the position log still holds it, and the settlement stands.
    let mut c = settling_core(&dir, balance - dec!(2.036));
    assert_eq!(c.restore_positions(), 1);
    assert!(
        c.positions().open_positions().is_empty(),
        "closed by recovery"
    );
    assert_eq!(
        c.positions().closed_positions()[0].exit_reason,
        ExitReason::Settlement
    );
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));
    assert_eq!(
        c.trade_summary(None)["totalNetPnl"],
        serde_json::json!(2.964)
    );
    let stats = settlement_json(&c);
    assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
    let report = c.run_accounting_audit(3_001);
    assert!(report.ok, "audit after recovery: {}", report.summary());

    // A second restart must not close or record it again.
    let mut c = settling_core(&dir, balance - dec!(2.036));
    assert_eq!(c.restore_positions(), 0, "the position log is empty now");
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));
    assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(5.0));
    c.on_market_resolution(winning_resolution(), 3_002);
    assert_eq!(c.trade_summary(None)["totalTrades"], serde_json::json!(1));
    assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(5.0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The panel sees settled-but-unredeemed money through the existing
/// `engine_stats` outlet, with no new event type.
#[test]
fn the_panel_can_see_a_settled_but_unredeemed_claim() {
    let dir = scratch("panel");
    let mut c = settling_core(&dir, dec!(10));
    open_and_anchor(&mut c, dec!(10));
    c.on_market_resolution(winning_resolution(), 2_001);

    let stats = settlement_json(&c);
    assert_eq!(stats["claims"][0]["id"], "cond");
    assert_eq!(stats["claims"][0]["payoutUsd"], serde_json::json!(5.0));
    assert_eq!(stats["claims"][0]["positions"], serde_json::json!(1));
    assert_eq!(stats["claims"][0]["attempts"], serde_json::json!(0));
    assert_eq!(stats["claims"][0]["manual"], serde_json::json!(false));
    assert_eq!(stats["blindSinceMs"], serde_json::Value::Null);
    // And the audit view names the receivable beside the balance it is not part of.
    let audit = c.engine_stats_at(2_002)["accountingAudit"].clone();
    assert_eq!(audit["receivableUsd"], serde_json::json!(5.0));

    let _ = std::fs::remove_dir_all(&dir);
}
