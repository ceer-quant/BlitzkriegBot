//! Acceptance harness for the accounting-truth fixes (issues #178, #181, #189).
//!
//! Everything here drives the PUBLIC core API (`Core` + `Ledger` + the OME's own
//! accessors) so the properties are asserted where an operator deploys them, not
//! on a private helper:
//!
//!  * **#178** — a trade id re-delivered after a RESTART is booked exactly once
//!    (the applied-fill table survives the process), a fill that lands on an
//!    order whose lifecycle already ended keeps the terminal status (no
//!    resurrection into `PartiallyFilled`), an over-reported fill is capped and
//!    shouted about, and 5000+ later mutations never evict a still-active key.
//!  * **#181** — a price-improved fill and a partial-fill-then-cancel each leave
//!    `reserved` at exactly zero, and the ledger identity
//!    `available + reserved == balance` holds on every path, including restart.
//!  * **#189** — in-kernel periodic active reconciliation: artificially created
//!    drift is alerted, persisted and blocks NEW entries within one interval
//!    (30s default), while exits stay possible; healing resumes entries.

use blitzkrieg_core::ipc::schema::Event;
use blitzkrieg_core::model::{
    CoreErrorCode, Fill, FillPolicy, FillStatus, Mode, OrderRequest, OrderStatus, Side,
};
use blitzkrieg_core::ome::APPLIED_CAP;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedReceiver;

/// Starting cash for every fixture. Large enough that the reservation gates never
/// interfere with what the test is actually measuring.
const SEED: Decimal = dec!(100000);

// ── fixtures ────────────────────────────────────────────────────────────────

/// A scratch directory unique to one test (process id + clock), so tests run in
/// parallel without sharing durable state.
fn tmp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "bk-accounting-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn join(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().to_string()
}

/// Config shared by the acceptance tests: DRY mode (no venue), no automation, and
/// durable logs only where the test asks for them.
fn base_config() -> CoreConfig {
    CoreConfig {
        mode: Mode::Dry,
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        risk: RiskConfig {
            max_order_notional: dec!(100000),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A config that persists orders/positions/trades under `dir` — the deployment
/// shape a restart has to survive.
fn persistent_config(dir: &Path) -> CoreConfig {
    CoreConfig {
        order_log_path: Some(join(dir, "orders.jsonl")),
        position_log_path: Some(join(dir, "positions.jsonl")),
        trade_log_path: Some(join(dir, "trades.jsonl")),
        ..base_config()
    }
}

fn core() -> Core {
    let mut c = Core::new(base_config());
    c.set_balance(SEED);
    c
}

fn buy(size: Decimal, price: Decimal, key: &str) -> OrderRequest {
    req(Side::Buy, size, price, key)
}

fn sell_req(size: Decimal, price: Decimal, key: &str) -> OrderRequest {
    req(Side::Sell, size, price, key)
}

fn req(side: Side, size: Decimal, price: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side,
        mode: FillPolicy::Maker,
        price,
        size,
        internal_key: key.into(),
        strategy: "acc".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
    }
}

/// A cumulative fill report as the venue sends it. `maker: Some(true)` keeps the
/// fixture's arithmetic fee-free, so the reservation (#181) is what a failure
/// points at.
fn fill(order_id: &str, trade: &str, side: Side, price: Decimal, size: Decimal) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 0,
        tx_hash: None,
        maker: Some(true),
    }
}

/// Place and acknowledge a BUY, returning its id.
fn live_buy(c: &mut Core, size: Decimal, price: Decimal, key: &str, now_ms: i64) -> String {
    let (id, _) = c.place_pending(buy(size, price, key), now_ms).unwrap();
    c.confirm_live(&id, now_ms).unwrap();
    id
}

/// Drain the order stream and return the `RiskAlert` texts seen so far.
fn alerts(rx: &mut UnboundedReceiver<Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            out.push(message);
        }
    }
    out
}

fn subscribed_core() -> (Core, UnboundedReceiver<Event>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = core();
    c.set_event_sink(tx);
    (c, rx)
}

// ── #178 · idempotency across a restart ─────────────────────────────────────

/// ACCEPTANCE: re-delivering the same trade id after a restart books it once.
#[test]
fn a_redelivered_trade_after_a_restart_is_booked_once() {
    let dir = tmp_dir("restart");
    let mut c = Core::new(persistent_config(&dir));
    c.set_balance(SEED);

    let id = live_buy(&mut c, dec!(20), dec!(0.5), "entry:restart", 1);
    let report = fill(&id, "trade-restart-1", Side::Buy, dec!(0.5), dec!(20));
    c.ingest_fill(report.clone(), 2).unwrap();

    let booked_balance = c.ledger().balance();
    assert_eq!(booked_balance, SEED - dec!(10));
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(20));
    assert!(c.ome().already_applied("trade-restart-1"));

    // The idempotency table reached the disk the deployment derived from the
    // order log (no new config needed): the format is one JSON object per
    // mutation, so a restart can fold it back.
    let applied_log = dir.join("applied-fills.jsonl");
    let text = std::fs::read_to_string(&applied_log).expect("applied-fill log written");
    assert!(
        text.contains("trade-restart-1"),
        "the applied key must be durable, got: {text}"
    );

    // ── restart ────────────────────────────────────────────────────────────
    drop(c);
    let mut c2 = Core::new(persistent_config(&dir));
    c2.set_balance(SEED);
    // The order is terminal, so the restart neither re-adopts it nor keeps it in
    // the compacted log — exactly the shape where the old table (memory-only) had
    // forgotten the key and a re-delivered fill was booked a second time onto
    // whatever order resolution picked next.
    assert_eq!(c2.restore_orders(), 0);
    assert!(c2.ome().get(&id).is_none());
    assert_eq!(c2.restore_positions(), 1, "the open position is recovered");
    assert!(
        c2.ome().already_applied("trade-restart-1"),
        "the applied table must survive the restart"
    );

    c2.ingest_fill(report, 3).unwrap();

    // The restart seeds cash from the venue, so what matters is that the
    // re-delivered trade moved NOTHING: cash is exactly what the fresh process
    // started with, and the recovered position is still 20 shares.
    assert_eq!(
        c2.ledger().balance(),
        SEED,
        "a re-delivered trade id must not move cash"
    );
    assert_eq!(
        c2.positions().open_positions()[0].shares,
        dec!(20),
        "a re-delivered trade id must not double the position"
    );
    // Nothing about the books is inconsistent afterwards.
    let audit = c2.run_accounting_audit(4);
    assert!(audit.ok, "{}", audit.note);
    assert!(c2.ledger().is_balanced());

    let _ = std::fs::remove_dir_all(&dir);
}

/// ACCEPTANCE (eviction): after 5000+ mutations the EARLIEST still-active key is
/// still idempotent. The table is over its cap, so eviction ran many times — and
/// it must never have picked the entry belonging to an order that is still live.
#[test]
fn an_active_key_survives_the_cap_and_stays_idempotent() {
    let mut c = core();
    // The key that must survive: a partially filled order that is still live
    // (it will never fill the rest, so it stays `PartiallyFilled` forever).
    let live = live_buy(&mut c, dec!(5), dec!(0.5), "entry:keep", 1);
    c.ingest_fill(fill(&live, "keep-me", Side::Buy, dec!(0.5), dec!(1)), 2)
        .unwrap();
    assert_eq!(
        c.ome().get(&live).unwrap().status,
        OrderStatus::PartiallyFilled
    );

    // Push the table well past its cap with TERMINAL orders — the only entries
    // eviction is allowed to take.
    let bulk = APPLIED_CAP + 50;
    for i in 0..bulk {
        let id = live_buy(&mut c, dec!(1), dec!(0.5), &format!("bulk:{i}"), 10);
        c.ingest_fill(
            fill(
                &id,
                &format!("bulk-trade-{i}"),
                Side::Buy,
                dec!(0.5),
                dec!(1),
            ),
            11,
        )
        .unwrap();
    }
    assert!(
        c.ome().applied_len() <= APPLIED_CAP,
        "the cap must still bound a table whose entries are all evictable (len {})",
        c.ome().applied_len()
    );
    assert!(
        c.ome().already_applied("keep-me"),
        "an entry belonging to a live order must never be evicted"
    );

    // The real assertion: the old, still-active trade re-delivered is a no-op.
    let balance = c.ledger().balance();
    let held = c.ome().get(&live).unwrap().filled_size;
    c.ingest_fill(fill(&live, "keep-me", Side::Buy, dec!(0.5), dec!(1)), 12)
        .unwrap();
    assert_eq!(c.ome().get(&live).unwrap().filled_size, held);
    assert_eq!(c.ledger().balance(), balance);
    assert_eq!(
        c.ome().get(&live).unwrap().status,
        OrderStatus::PartiallyFilled
    );
}

// ── #178 · terminal absorption + the late-fill signal ───────────────────────

/// ACCEPTANCE: `Cancelled` + a late partial fill keeps the terminal status, books
/// the real size, and reaches the operator as a `late_fill` alert.
#[test]
fn a_cancelled_order_absorbs_a_late_fill_and_stays_terminal() {
    let (mut c, mut rx) = subscribed_core();
    let id = live_buy(&mut c, dec!(20), dec!(0.6), "entry:late", 1);
    c.cancel(&id, 2).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);

    // The venue reports an execution that raced our cancel.
    c.ingest_fill(fill(&id, "late-1", Side::Buy, dec!(0.6), dec!(10)), 3)
        .unwrap();

    let o = c.ome().get(&id).unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Cancelled,
        "a terminal order must never be revived into PartiallyFilled"
    );
    assert_eq!(
        o.filled_size,
        dec!(10),
        "the shares are real and must be booked"
    );
    assert_eq!(
        c.ome().remaining(&id),
        dec!(10),
        "size is the total; the remainder is derived"
    );
    assert_eq!(c.positions().open_positions()[0].shares, dec!(10));
    assert_eq!(c.ledger().balance(), SEED - dec!(6));
    assert_eq!(
        c.ledger().reserved(),
        Decimal::ZERO,
        "a cancelled order holds nothing"
    );
    assert!(c.ledger().is_balanced());

    let msgs = alerts(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.contains("late_fill") && m.contains("trade=late-1")),
        "the operator must be told: {msgs:?}"
    );

    let audit = c.run_accounting_audit(4);
    assert!(audit.ok, "{}", audit.note);
}

/// The other half of terminal absorption: a venue over-report is CAPPED at the
/// order's own size and flagged as an overfill (the one case that could exceed
/// our books).
#[test]
fn an_over_reported_fill_is_capped_and_flagged() {
    let (mut c, mut rx) = subscribed_core();
    let id = live_buy(&mut c, dec!(20), dec!(0.6), "entry:over", 1);
    // The venue claims 25 shares on a 20-share order.
    c.ingest_fill(fill(&id, "over-1", Side::Buy, dec!(0.6), dec!(25)), 2)
        .unwrap();

    assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(20));
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    assert_eq!(
        c.ledger().balance(),
        SEED - dec!(12),
        "cash follows the CAPPED size, never the over-report"
    );

    let msgs = alerts(&mut rx);
    let msg = msgs
        .iter()
        .find(|m| m.contains("late_fill") && m.contains("over-1"))
        .unwrap_or_else(|| panic!("an overfill must be alerted: {msgs:?}"));
    assert!(msg.contains("overfill=true"), "{msg}");
    assert!(msg.contains("reported=25"), "{msg}");
}

// ── #181 · reservations reach exactly zero ─────────────────────────────────

/// ACCEPTANCE: a price-improved fill leaves no residue in the reservation table.
#[test]
fn a_price_improved_fill_leaves_no_residue() {
    let mut c = core();
    let id = live_buy(&mut c, dec!(20), dec!(0.6), "entry:improved", 1);
    assert_eq!(
        c.ledger().reserved(),
        dec!(12),
        "0.60 × 20 reserved up front"
    );

    // Filled in full at 0.50 — better than the limit we reserved at.
    c.ingest_fill(fill(&id, "improved-1", Side::Buy, dec!(0.5), dec!(20)), 2)
        .unwrap();

    assert_eq!(
        c.ledger().reserved(),
        Decimal::ZERO,
        "the 0.10/share difference must not stay reserved (nothing will ever release it)"
    );
    assert_eq!(c.ledger().balance(), SEED - dec!(10));
    assert_eq!(
        c.ledger().available(),
        c.ledger().balance(),
        "stranded cash is unusable cash"
    );
    assert!(c.ledger().is_balanced());

    let audit = c.run_accounting_audit(3);
    assert!(audit.ok, "{}", audit.note);
    assert_eq!(audit.drift_usd, Decimal::ZERO);
}

/// ACCEPTANCE: partial fill then cancel leaves nothing behind.
#[test]
fn a_partial_fill_then_cancel_leaves_no_residue() {
    let mut c = core();
    let id = live_buy(&mut c, dec!(20), dec!(0.6), "entry:partial", 1);
    c.ingest_fill(fill(&id, "partial-1", Side::Buy, dec!(0.6), dec!(8)), 2)
        .unwrap();

    assert_eq!(
        c.ledger().reserved(),
        dec!(7.2),
        "the 12 unfilled shares stay committed while the order rests"
    );
    assert!(c.ledger().is_balanced());

    c.cancel(&id, 3).unwrap();

    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
    assert_eq!(c.ledger().available(), c.ledger().balance());
    assert!(c.ledger().is_balanced());
    let audit = c.run_accounting_audit(4);
    assert!(audit.ok, "{}", audit.note);
    assert!(
        audit
            .checks
            .iter()
            .all(|c| c.name != "reservations_match_open_buys" || c.ok)
    );
}

/// The identity survives a restart: a re-adopted resting BUY re-reserves exactly
/// its own outstanding notional (a fresh ledger would otherwise believe the whole
/// balance is free and let the next entries over-commit).
#[test]
fn a_restart_re_reserves_the_unfilled_notional() {
    let dir = tmp_dir("reserve-restart");
    let mut c = Core::new(persistent_config(&dir));
    c.set_balance(SEED);
    let id = live_buy(&mut c, dec!(20), dec!(0.6), "entry:restore", 1);
    c.ingest_fill(fill(&id, "restore-1", Side::Buy, dec!(0.6), dec!(8)), 2)
        .unwrap();
    drop(c);

    let mut c2 = Core::new(persistent_config(&dir));
    c2.set_balance(SEED);
    assert_eq!(c2.restore_orders(), 1, "the resting order is re-adopted");
    assert_eq!(
        c2.ledger().reserved(),
        dec!(7.2),
        "the unfilled notional is committed again"
    );
    assert!(c2.ledger().is_balanced());
    assert_eq!(c2.ome().get(&id).unwrap().filled_size, dec!(8));
    let audit = c2.run_accounting_audit(3);
    assert!(audit.ok, "{}", audit.note);

    // A trade re-delivered for the RE-ADOPTED order is a no-op too (the key came
    // back with the applied journal, not just with the order log).
    c2.ingest_fill(fill(&id, "restore-1", Side::Buy, dec!(0.6), dec!(8)), 4)
        .unwrap();
    assert_eq!(c2.ome().get(&id).unwrap().filled_size, dec!(8));
    assert_eq!(c2.ledger().reserved(), dec!(7.2));
    assert!(c2.ledger().is_balanced());

    let _ = std::fs::remove_dir_all(&dir);
}

// ── #189 · in-kernel active reconciliation ─────────────────────────────────

/// ACCEPTANCE: artificially created drift is alerted, persisted and blocks new
/// entries within one audit interval — and healing resumes entries. Exits are
/// never blocked (a drifted book must not trap the bot in a position).
#[test]
fn drift_is_alerted_persisted_and_blocks_new_entries() {
    let dir = tmp_dir("audit");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = Core::new(CoreConfig {
        audit_interval_sec: 30,
        ..persistent_config(&dir)
    });
    c.set_event_sink(tx);
    c.set_balance(SEED);

    // First maintenance tick: the audit runs, the books are clean, and the
    // anchored baseline is taken.
    c.tick(1_000).unwrap();
    assert!(!c.audit_blocks_entries());
    let first = c.accounting_audit().expect("audit ran on the first tick");
    assert!(first.ok, "{}", first.note);
    assert!(
        first.anchor.is_none(),
        "the first audit COMPARES against nothing; it establishes the baseline"
    );
    assert_eq!(
        c.accounting_audit_view()["anchored"],
        serde_json::json!(true),
        "the first clean audit takes the anchor"
    );

    // 5 USD leaves the ledger with no trade to explain it (the classic missed
    // fill / quietly healed venue balance).
    c.set_balance(SEED - dec!(5));

    // Not due yet: the audit is PERIODIC, not a per-tick re-derivation.
    c.tick(20_000).unwrap();
    assert!(!c.audit_blocks_entries(), "the interval has not elapsed");

    // Due 30s after the previous one.
    c.tick(31_000).unwrap();
    assert!(c.audit_blocks_entries(), "drift must block new entries");
    assert!(!c.audit_halt_reason().is_empty());
    let report = c.accounting_audit().unwrap();
    assert!(!report.ok);
    assert_eq!(report.drift_usd, dec!(5));
    assert!(report.structural_failure);
    assert!(
        report.anchor.is_some(),
        "the failing audit still compares against the original baseline"
    );

    let msgs = alerts(&mut rx);
    assert!(
        msgs.iter().any(|m| m.contains("accounting audit FAILED")),
        "the drift must be alerted: {msgs:?}"
    );

    // A new ENTRY is refused...
    let err = c
        .place_pending(buy(dec!(10), dec!(0.5), "entry:blocked"), 31_500)
        .expect_err("an entry must not open on a drifted book");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("accounting audit"), "{}", err.message);
    // ...while a CLOSE is still possible.
    assert!(
        c.place_pending(sell_req(dec!(10), dec!(0.5), "exit:allowed"), 31_500)
            .is_ok(),
        "exits must never be blocked by a failing audit"
    );

    // The verdict is on disk, in its own JSONL, and on the panel.
    let log = std::fs::read_to_string(dir.join("reconcile.jsonl")).expect("audit log written");
    let last = log.lines().last().expect("at least one audit record");
    assert!(last.contains("\"ok\":false"), "{last}");
    assert!(last.contains("\"halt_entries\":true"), "{last}");
    let panel = c.engine_stats_at(31_500);
    let view = &panel["accountingAudit"];
    assert_eq!(view["halted"], true, "{panel}");
    assert!(!view["summary"].as_str().unwrap_or("").is_empty());

    // Healed: the next audit passes and entries resume on its own.
    c.set_balance(SEED);
    c.tick(62_000).unwrap();
    assert!(!c.audit_blocks_entries());
    assert!(c.accounting_audit().unwrap().ok);
    let msgs = alerts(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.contains("accounting audit recovered")),
        "the recovery must be reported: {msgs:?}"
    );
    assert!(
        c.place_pending(buy(dec!(10), dec!(0.5), "entry:resumed"), 62_500)
            .is_ok()
    );

    // The whole log is the durable record of what happened, in order.
    let log = std::fs::read_to_string(dir.join("reconcile.jsonl")).unwrap();
    assert!(
        log.lines().count() >= 3,
        "every verdict was appended: {log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// In LIVE the plugin host SETS the ledger from the venue's free cash whenever
/// nothing rests (that is the designed repair for a fill the WS never delivered).
/// A residual that equals that correction is still reported and persisted, but it
/// does not halt entries on the first audit: the correcting party is the venue,
/// so the leg is cross-source and needs a repeat.
#[test]
fn a_venue_ledger_correction_is_not_treated_as_local_drift() {
    let dir = tmp_dir("audit-live");
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        audit_interval_sec: 30,
        ..persistent_config(&dir)
    });
    // A LIVE core starts at zero and is seeded from the venue.
    assert_eq!(c.ledger().balance(), Decimal::ZERO);
    c.set_balance(SEED);
    c.tick(1_000).unwrap();
    assert!(!c.audit_blocks_entries());
    assert!(c.accounting_audit().unwrap().ok);

    // The venue reports 5 USD less than our records explain and the host realigns.
    c.set_balance(SEED - dec!(5));
    c.tick(31_000).unwrap();

    let report = c.accounting_audit().unwrap();
    assert!(
        !report.ok,
        "the trade record still disagrees with the money"
    );
    assert_eq!(report.drift_usd, dec!(5));
    assert!(
        !report.structural_failure,
        "the venue set that figure; it is not a local inconsistency: {}",
        report.note
    );
    assert!(
        !c.audit_blocks_entries(),
        "one cross-source strike must not stop trading"
    );

    // It repeats on the next audit (nothing repaired it), so entries stop.
    c.tick(62_000).unwrap();
    assert!(c.audit_blocks_entries());
    let log = std::fs::read_to_string(dir.join("reconcile.jsonl")).unwrap();
    assert!(log.lines().count() >= 2, "{log}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A reservation stranded in the ledger (a phantom hold nothing can release) is a
/// STRUCTURAL failure: it halts on the first audit rather than after a strike.
#[test]
fn a_stranded_reservation_halts_immediately() {
    let dir = tmp_dir("audit-residue");
    let mut c = Core::new(CoreConfig {
        audit_interval_sec: 30,
        ..persistent_config(&dir)
    });
    c.set_balance(SEED);

    // The residue the pre-#181 settle path used to leave: cash reserved for an
    // order that no longer exists.
    c.ledger_mut().reserve("ghost-order", dec!(7)).unwrap();

    c.tick(1_000).unwrap();
    assert!(c.audit_blocks_entries());
    let report = c.accounting_audit().unwrap();
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "reservations_match_open_buys")
        .expect("the reservation check runs");
    assert!(!check.ok);
    assert!(check.structural);
    assert!(
        check.offenders.iter().any(|o| o.contains("ghost-order")),
        "{check:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
