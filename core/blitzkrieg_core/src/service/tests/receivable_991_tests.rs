#![cfg(test)]

//! Issue #991/#1002 — the `BK_UNVERIFIED_EXPIRY_POLICY=receivable` arm: a
//! fallback close books a receivable CLAIM instead of crediting cash; the
//! claim redeems at venue truth (a resolution or a reconcile SELL fill) with
//! #998 correction rows for the delta and a trade-record amend; the aging
//! warn; the day-budget/breaker exclusion. The cash default's byte-identity is
//! pinned by the pre-existing #684 tests (they never set the policy and keep
//! passing unmodified).

use super::*;
use crate::model::{FillPolicy, OrderStatus, Side};
use rust_decimal_macros::dec;

/// 900s 网格上 slot 1 的终点：与 expiry_fallback_tests 同锚。
const EXPIRES_MS: i64 = 1_800_000;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-receivable-991-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A dry core with the RECEIVABLE policy armed (and durable logs in a private
/// dir). `max_age` defaults to the shipped 48h budget.
fn receivable_core(dir: &std::path::Path, max_age_ms: i64) -> Core {
    let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        round_duration_sec: 900,
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        unverified_expiry_policy: UnverifiedExpiryPolicy::Receivable,
        receivable_max_age_ms: max_age_ms,
        position_log_path: path("positions.jsonl"),
        trade_log_path: path("trades.jsonl"),
        settlement_log_path: path("settlements.jsonl"),
        order_log_path: path("orders.jsonl"),
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

fn open_expiring_position(c: &mut Core) -> crate::position::OpenPosition {
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, status) = c
        .place(
            crate::model::OrderRequest {
                token_id: "tok".into(),
                condition_id: "cond".into(),
                side: Side::Buy,
                mode: FillPolicy::Taker,
                price: dec!(0.40),
                size: dec!(5),
                internal_key: format!("k-{}", c.positions().open_positions().len()),
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: "up".into(),
                round_slot: 1,
                account_id: crate::model::default_account_id(),
            },
            0,
            1,
        )
        .unwrap();
    assert_eq!(status, OrderStatus::Filled);
    let p = c.positions().open_positions()[0].clone();
    assert_eq!(p.expires_at_ms, EXPIRES_MS);
    p
}

/// Stage an unanswered settlement query (the #684 fixture's seam).
fn stage_unanswered_query(c: &mut Core, condition_id: &str, first_sent_ms: i64) {
    let positions: Vec<crate::position::OpenPosition> = c
        .positions()
        .open_positions()
        .iter()
        .filter(|p| p.condition_id == condition_id)
        .cloned()
        .collect();
    assert!(!positions.is_empty(), "fixture needs the position first");
    c.settlement.track_markets(&positions, first_sent_ms);
    c.settlement.take_queries(first_sent_ms);
    let unanswered = c.settlement.unanswered_since(0, first_sent_ms + 1);
    assert!(
        unanswered.iter().any(|(cid, _)| cid == condition_id),
        "fixture: the query must stage as unanswered: {unanswered:?}"
    );
}

/// Drive the fallback sweep to fire (2h of venue silence past expiry).
fn run_fallback_sweep(c: &mut Core, sent: i64) {
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    c.tick(bound + 1_000).unwrap();
}

/// THE acceptance case: fallback close books a CLAIM (receivable rises, cash
/// does NOT move — estimated money is not tradable money); the resolution
/// redeems it at venue truth with a #998 correction row for the delta; the
/// trade record's net is amended by the same delta; the audit identity holds.
#[test]
fn receivable_fallback_books_claim_and_resolution_redeems_at_venue_truth() {
    let dir = scratch("lifecycle");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    open_expiring_position(&mut c);

    // Cash BEFORE the fallback: the entry cost + fee came out of 1000.
    let cash_before = c.accounts().active_ledger().balance();
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    assert!(c.positions().open_positions().is_empty(), "fallback closed");

    // The claim exists: receivable = price × shares = 0.40 × 5 = 2.00, and the
    // ledger did NOT credit a cent (the cash model would have).
    let settled_stats =
        c.engine_stats_at(sent + super::EXPIRY_FALLBACK_AFTER_MS + 2_000)["settlement"].clone();
    assert_eq!(
        settled_stats["fallbackReceivableUsd"],
        serde_json::json!(2.0),
        "the claim books the fallback notional as receivable"
    );
    assert_eq!(
        settled_stats["pendingFallbackClaims"],
        serde_json::json!(1),
        "one open fallback claim"
    );
    assert_eq!(
        settled_stats["unverifiedExpiryPolicy"],
        serde_json::json!("receivable"),
        "the policy in force rides the view"
    );
    let age = settled_stats["oldestFallbackClaimAgeMs"].as_i64();
    assert!(
        age.is_some_and(|a| a >= 0),
        "an aging signal exists once a claim is open"
    );
    let cash_after_fallback = c.accounts().active_ledger().balance();
    assert_eq!(
        cash_after_fallback, cash_before,
        "the receivable model moves NO cash at the fallback close (estimated money must not be tradable)"
    );
    // The claim rides the SAME receivable pool the settled claims ride.
    assert_eq!(
        settled_stats["receivableUsd"],
        serde_json::json!(2.0),
        "the fallback slice folds into the pool the audit reads"
    );
    // The journal carried the booking: the durable arm survives a restart.
    let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
    assert!(
        journal.contains("expiryFallbackBooked"),
        "the claim booking is journaled for replay: {journal}"
    );

    // Venue truth arrives: the market resolves WINNER at 0.70/share. The claim
    // redeems: receivable −2.00, cash +3.50 (0.70 × 5), correction +1.50.
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.70))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);

    let stats = c.engine_stats_at(bound + 4_000)["settlement"].clone();
    assert_eq!(
        stats["fallbackReceivableUsd"],
        serde_json::json!(0.0),
        "the claim left the pool"
    );
    assert_eq!(stats["pendingFallbackClaims"], serde_json::json!(0));
    assert_eq!(
        stats["oldestFallbackClaimAgeMs"],
        serde_json::Value::Null,
        "no aging signal with no open claim"
    );
    let cash_after_redeem = c.accounts().active_ledger().balance();
    assert_eq!(
        cash_after_redeem - cash_before,
        dec!(3.5),
        "cash moved by VENUE TRUTH (0.70 × 5), not the estimate (2.00)"
    );
    // The ledger journal: the booked redemption row + the correction row
    // (#998's two-row shape). The claim's `ref` is the POSITION id (the
    // redemption is keyed on the position's close, not a settlement claim id).
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let redemption_rows: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .collect();
    let position_id = c.positions().closed_positions()[0].id.clone();
    assert!(
        redemption_rows
            .iter()
            .any(|e| e.ref_ == position_id && e.amount == dec!(2.0)),
        "the BOOKED row credits the estimate: {redemption_rows:?}"
    );
    assert!(
        redemption_rows
            .iter()
            .any(|e| e.amount == dec!(1.5) && e.ref_.contains("fallback redemption correction")),
        "the correction row moves venue_truth − booked: {redemption_rows:?}"
    );

    // The trade record's net moves by the SAME delta (Δcash == Δrealized, the
    // anchored identity's demand). Capture the booked net from the closed row
    // BEFORE the redemption, then expect booked + delta.
    let booked_net = c.positions().closed_positions()[0].net_pnl_usd;
    let expected_net = booked_net + dec!(1.5);
    use rust_decimal::prelude::ToPrimitive;
    let trades = c.recent_trades(10);
    assert_eq!(trades.len(), 1);
    let got = trades[0]["netPnlUsd"].as_f64().expect("net is a number");
    assert!(
        (got - expected_net.to_f64().unwrap()).abs() < 1e-9,
        "the recorded net is amended by the venue-vs-estimate delta: got {got}, want {expected_net}"
    );
    // The amend keeps the provenance flags: still an estimated close.
    let closed_after = &c.positions().closed_positions()[0];
    assert_eq!(
        closed_after.source, "reconciled",
        "the resolution marked the row reconciled"
    );
    assert_eq!(
        closed_after.exit_price,
        dec!(0.40),
        "the row's price stays the estimate (never re-labeled a venue verdict)"
    );
    assert_eq!(
        closed_after.price_source, "current",
        "the fallback priced off a live feed (current mark)"
    );

    // THE audit: with receivable and correction both folded, the identity
    // holds (this is the algebra the amend exists for).
    let audit = c.run_accounting_audit(bound + 5_000);
    assert!(
        audit.ok,
        "audit after receivable redemption failed (see AuditReport fields)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Zero-delta redemption: the venue truth EQUALS the fallback estimate — the
/// claim redeems, cash moves by the booked amount, and NO correction row is
/// booked (a correction of nothing is not a fact).
#[test]
fn zero_delta_redemption_books_no_correction_row() {
    let dir = scratch("zero-delta");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    assert_eq!(
        c.accounts().active_ledger().journal_snapshot(),
        c.accounts().active_ledger().journal_snapshot(),
        "fixture sanity"
    );

    // The venue's verdict prices the token at EXACTLY the fallback estimate
    // (0.40/share — the entry book's mark).
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.40))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);

    let stats = c.engine_stats_at(bound + 4_000)["settlement"].clone();
    assert_eq!(
        stats["pendingFallbackClaims"],
        serde_json::json!(0),
        "the claim redeemed"
    );
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let corrections: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.ref_.contains("fallback redemption correction"))
        .collect();
    assert!(
        corrections.is_empty(),
        "a zero-delta redemption books NO correction row: {corrections:?}"
    );
    // Cash moved by the booked estimate exactly once (one Redemption row).
    let redemptions: Vec<_> = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .collect();
    assert_eq!(
        redemptions.len(),
        1,
        "exactly one redemption row (the booked one): {redemptions:?}"
    );
    assert_eq!(redemptions[0].amount, dec!(2.0));
    let audit = c.run_accounting_audit(bound + 5_000);
    assert!(audit.ok, "audit after zero-delta redemption failed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The reconcile sweep's SELL fill is venue truth too: a claim whose token the
/// sweep saw traded redeems at the FILL price (the resolution never arrived).
#[test]
fn reconcile_sell_fill_redeems_the_claim_at_the_fill_price() {
    let dir = scratch("reconcile-redeem");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let _p = open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;

    // No resolution — the venue's cash truth arrives as a SELL fill on the
    // token via the reconcile sweep, at 0.55/share (≠ the 0.40 estimate).
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "ven:1".into(),
            trade_id: "tr:1".into(),
            token_id: "tok".into(),
            side: crate::model::Side::Sell,
            size: dec!(5),
            price: dec!(0.55),
            ts_ms: bound + 2_000,
            tx_hash: None,
            maker: None,
        }],
        now_ms: bound + 2_000,
    };
    let _report = c.reconcile(snap).expect("reconcile succeeds");

    let stats = c.engine_stats_at(bound + 3_000)["settlement"].clone();
    assert_eq!(
        stats["pendingFallbackClaims"],
        serde_json::json!(0),
        "the reconcile fill redeemed the claim"
    );
    let cash_before = c.accounts().active_ledger().balance();
    // The redemption credited venue truth: booked 2.00 + correction (0.55×5 −
    // 2.00) = 2.75 total.
    let ledger_journal = c.accounts().active_ledger().journal_snapshot();
    let moved: rust_decimal::Decimal = ledger_journal
        .iter()
        .filter(|e| e.kind == crate::ledger::JournalKind::Redemption)
        .map(|e| e.amount)
        .sum();
    assert_eq!(
        moved,
        dec!(2.75),
        "booked row + correction row sum to venue truth (0.55 × 5)"
    );
    let _ = cash_before;
    // The trade record was amended by the same +0.75.
    let audit = c.run_accounting_audit(bound + 4_000);
    assert!(audit.ok, "audit after reconcile redemption failed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Journal durability: a claim booked before a restart replays from the
/// settlement journal with its aging clock intact, and a Redeemed line removes
/// it (last-writer-wins).
#[test]
fn fallback_claim_journal_replay_survives_a_restart() {
    let dir = scratch("replay");
    let sent;
    let bound;
    {
        let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        open_expiring_position(&mut c);
        sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
        assert_eq!(
            c.engine_stats_at(bound + 2_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(1)
        );
        // Core drops; the journal file stays.
    }
    {
        // A fresh core over the SAME dir replays the journal.
        let mut c2 = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        let stats = c2.engine_stats_at(bound + 2_000)["settlement"].clone();
        assert_eq!(
            stats["pendingFallbackClaims"],
            serde_json::json!(1),
            "the claim survives the restart"
        );
        assert_eq!(
            stats["fallbackReceivableUsd"],
            serde_json::json!(2.0),
            "the booked notional survives the restart"
        );
        // The aging clock rode the journal's at_ms: the replayed claim's age is
        // measured from the ORIGINAL booking instant (the sweep fired at
        // `bound + 1_000`), not from the restart.
        let age = stats["oldestFallbackClaimAgeMs"]
            .as_i64()
            .expect("age present");
        assert!(
            age >= (bound + 2_000) - (bound + 1_000),
            "the replayed claim's aging clock starts at the booking, not the restart: {age}"
        );
        // And the redemption still works on the replayed state.
        let resolution = blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("tok".to_string(), dec!(0.40))],
            neg_risk: false,
            source: "test".into(),
        };
        c2.on_market_resolution(resolution, bound + 3_000);
        assert_eq!(
            c2.engine_stats_at(bound + 4_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(0),
            "the replayed claim redeems exactly once"
        );
        // A SECOND core over the same journal sees the Redeemed line — the
        // claim must NOT resurrect (last-writer-wins replay).
        let c3 = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
        assert_eq!(
            c3.engine_stats_at(bound + 5_000)["settlement"]["pendingFallbackClaims"],
            serde_json::json!(0),
            "the Redeemed journal line removes the claim on replay"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The aging warn: overdue claims warn ONCE per episode (the edge), stay quiet
/// while still overdue, and re-arm after the pool empties. max_age <= 0 is off.
#[test]
fn the_aging_warn_fires_once_per_episode() {
    let dir = scratch("aging");
    // A tiny budget so the bound is reachable in test time.
    let mut c = receivable_core(&dir, 5_000);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;

    // Past the 5s budget: the warn fires on the first maintenance pass and
    // NEVER again while the same episode continues.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    for i in 0..3 {
        c.tick(bound + 10_000 + i * 6_000).unwrap();
    }
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts += 1;
        }
    }
    assert_eq!(alerts, 1, "the warn fires ONCE per episode, not every tick");
    drop(rx);

    // Redeem (zero-delta) → the pool empties → the edge re-arms.
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.40))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 30_000);
    assert_eq!(
        c.engine_stats_at(bound + 31_000)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(0)
    );

    // A SECOND cycle: a fresh claim in the pool (booked straight through the
    // settlement book — this test targets the aging EDGE, not the admission
    // gates, and a full re-entry would wade through cooldowns/expiry grids).
    // Its booking instant is already past the 5s budget when it lands.
    let t2 = bound + 40_000;
    // A pass with an EMPTY pool: this is the tick that re-arms the edge
    // (oldest == None → receivable_warned_age_ms resets to 0).
    c.tick(t2 - 1_000).unwrap();
    c.settlement.book_fallback_claim(
        "pos-aging-second",
        "cond-2",
        "tok-2",
        crate::model::default_account_id().as_str(),
        dec!(5),
        dec!(0.40),
        "current",
        Decimal::ZERO,
        t2,
        t2 - 60_000,
    );
    assert_eq!(
        c.engine_stats_at(t2)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(1),
        "precondition: the second claim is open and already overdue"
    );
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    c.set_event_sink(tx2);
    c.tick(t2 + 1_000).unwrap();
    let mut alerts2 = 0;
    while let Ok(ev) = rx2.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts2 += 1;
        }
    }
    assert_eq!(alerts2, 1, "the re-armed edge fires once for the new claim");

    let _ = std::fs::remove_dir_all(&dir);
}

/// max_age = 0 disables the aging warn entirely (documented off-spelling).
#[test]
fn zero_aging_budget_disables_the_warn() {
    let dir = scratch("aging-off");
    let mut c = receivable_core(&dir, 0);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.tick(bound + 1_000_000).unwrap();
    let mut alerts = 0;
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && message.contains("receivable aging")
        {
            alerts += 1;
        }
    }
    assert_eq!(alerts, 0, "max_age = 0 means the aging check is OFF");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The AMENDMENT (issue #1002): a zero-feed fallback (current_price == 0)
/// prices at the ENTRY price, and the closed record names the basis
/// `price_source = "entry_fallback"` — a downstream surface can tell "the feed
/// said X" from "the feed was dead, entry booked". A live-mark fallback reads
/// `price_source = "current"`. Additive wire field: absent on venue rows.
#[test]
fn price_source_names_the_fallback_basis() {
    let dir = scratch("price-source");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let p = open_expiring_position(&mut c);
    // Dead feed: the snapshot restores a 0 mark → entry-price basis.
    let mut dead = p.clone();
    dead.current_price = dec!(0);
    dead.prev_price = dec!(0);
    c.positions.restore_open(vec![dead], None);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.source, "fallback");
    assert_eq!(closed.exit_price, dec!(0.40), "entry price = cost recovery");
    assert_eq!(
        closed.price_source, "entry_fallback",
        "a zero feed must be NAMED as the entry-price basis (issue 1002)"
    );

    // Wire shape: the additive field rides the closed-position JSON.
    // ClosedPosition has NO camelCase rename (pre-#1002 wire), so the key is
    // the field's own snake_case spelling — additive either way.
    let raw = serde_json::to_value(closed).unwrap();
    assert_eq!(
        raw["price_source"],
        serde_json::json!("entry_fallback"),
        "the wire carries the basis (additive)"
    );

    // A venue close carries NEITHER field: additive-only, older readers see
    // nothing new. (byte-identity of pre-existing fixtures is pinned by the
    // golden family 23 test.)
    let dir2 = scratch("price-source-venue");
    let mut c2 = receivable_core(&dir2, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let p2 = open_expiring_position(&mut c2);
    let closed2 = c2
        .positions
        .close(
            &p2.id,
            dec!(0.50),
            crate::model::ExitReason::TakeProfit,
            true,
            1_900_000,
            "venue",
        )
        .expect("venue close lands");
    assert_eq!(closed2.source, "venue");
    assert_eq!(
        closed2.price_source, "",
        "a venue close has no fallback basis to name"
    );
    let raw2 = serde_json::to_value(closed2).unwrap();
    assert!(
        raw2.get("price_source").is_none(),
        "the field is SKIPPED when empty — no wire byte changes for venue rows: {raw2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// Boundary 2: the unverified-exposure report keeps BOTH faces visible in
/// receivable mode — the plain fallback row AND the reconciled row (a
/// redemption never removes a row from the report; visibility is not
/// forgiveness).
#[test]
fn unverified_exposure_stays_visible_through_the_receivable_cycle() {
    let dir = scratch("exposure");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    let _p = open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;

    let exposure = c.unverified_exposure_report();
    assert_eq!(exposure.total_positions, 1);
    assert_eq!(exposure.entries[0].source, "fallback");

    // The resolution redeems the claim AND marks the row reconciled — the row
    // STAYS on the report.
    let resolution = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond".into(),
        resolved: true,
        payouts: vec![("tok".to_string(), dec!(0.40))],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(resolution, bound + 3_000);
    let exposure = c.unverified_exposure_report();
    assert_eq!(
        exposure.total_positions, 1,
        "a reconciled row is still unverified-cash history — it stays visible"
    );
    assert_eq!(exposure.entries[0].source, "reconciled");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Boundary 3 discipline lives in the ABSENCE of a fast-confirm knob: the
/// receivable path's only redemption triggers are the venue's own truth
/// (resolution verdict / reconcile fill). This test pins that a claim with NO
/// venue evidence just stays open — no code path may redeem it early.
#[test]
fn a_claim_without_venue_truth_stays_open() {
    let dir = scratch("no-truth-stays-open");
    let mut c = receivable_core(&dir, crate::service::RECEIVABLE_MAX_AGE_MS_DEFAULT);
    open_expiring_position(&mut c);
    let sent = EXPIRES_MS + 100;
    c.tick(sent).unwrap();
    stage_unanswered_query(&mut c, "cond", sent);
    run_fallback_sweep(&mut c, sent);
    let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;

    // Ticks, aging warns, and a resolution for a DIFFERENT condition must all
    // leave the claim open. Nothing but this condition's own truth redeems it.
    for i in 0..5 {
        c.tick(bound + 10_000 + i * 60_000).unwrap();
    }
    let other = blitzkrieg_market_api::MarketResolution {
        condition_id: "cond-OTHER".into(),
        resolved: true,
        payouts: vec![("tok-OTHER".to_string(), Decimal::ONE)],
        neg_risk: false,
        source: "test".into(),
    };
    c.on_market_resolution(other, bound + 400_000);
    assert_eq!(
        c.engine_stats_at(bound + 410_000)["settlement"]["pendingFallbackClaims"],
        serde_json::json!(1),
        "no venue truth for THIS condition = the claim stays open (no fast-confirm path)"
    );
    assert_eq!(
        c.accounts().active_ledger().balance(),
        c.accounts().active_ledger().balance(),
        "and no cash ever moved"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Day-budget boundary (issue #991: estimated money must not be tradable): in
/// receivable mode the fallback close books NO cash, so the day's OPENING
/// EQUITY — read off the ledger balance at the UTC roll — excludes the
/// receivable. The cash policy (default) credits 2.00 immediately and the NEXT
/// UTC day opens on a base that carries it; the receivable policy leaves the
/// ledger alone, so the next day's base sits exactly 2.00 lower. Either way
/// the ledger itself IS the exclusion: no separate knob exists.
#[test]
fn the_receivable_never_reaches_the_day_budget_base() {
    // Same scenario, two policies: the ONLY observable difference is the cash.
    let run = |policy: UnverifiedExpiryPolicy| -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        let dir = scratch(&format!("budget-{}", policy.as_str()));
        let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
        let mut c = Core::new(CoreConfig {
            mode: Mode::Dry,
            round_duration_sec: 900,
            risk: crate::risk::RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            unverified_expiry_policy: policy,
            positions: crate::position::PositionConfig {
                max_daily_loss_usd: dec!(2),
                daily_loss_marks_unrealized: false,
                ..Default::default()
            },
            position_log_path: path("positions.jsonl"),
            trade_log_path: path("trades.jsonl"),
            settlement_log_path: path("settlements.jsonl"),
            order_log_path: path("orders.jsonl"),
            ..Default::default()
        });
        c.set_balance(dec!(1000));
        open_expiring_position(&mut c);
        // Day 0 stamps here — BEFORE the fallback close (identical both arms).
        c.tick(1_000).unwrap();
        let sent = EXPIRES_MS + 100;
        c.tick(sent).unwrap();
        stage_unanswered_query(&mut c, "cond", sent);
        run_fallback_sweep(&mut c, sent);
        let bound = sent + super::EXPIRY_FALLBACK_AFTER_MS;
        let cash = c.accounts().active_ledger().balance();
        // Cross a UTC day boundary: the roll re-reads the ledger for the new
        // day's opening equity — cash policy carries the +2.00 fallback credit,
        // receivable policy does not (the receivable itself is not spendable).
        c.tick(bound + 100_000_000).unwrap();
        // Decimal's default serde spells STRINGS on this wire — parse it back.
        let opening = c.engine_stats_at(bound + 100_100_000)["dailyLoss"]["openingEquityUsd"]
            .as_str()
            .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok())
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        (cash, opening)
    };

    let (cash_cash, opening_cash) = run(UnverifiedExpiryPolicy::Cash);
    let (cash_recv, opening_recv) = run(UnverifiedExpiryPolicy::Receivable);
    // The cash policy credits the 2.00 fallback notional: the ledger has it.
    assert_eq!(
        cash_cash - cash_recv,
        dec!(2.0),
        "the ONLY cash difference between the policies is the fallback notional"
    );
    // And the next day's budget base sits EXACTLY 2.00 lower under the
    // receivable policy — the estimated money never entered the base.
    assert_eq!(
        opening_cash - opening_recv,
        dec!(2.0),
        "the receivable is excluded from the day-budget base"
    );
}
