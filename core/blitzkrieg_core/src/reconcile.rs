//! Reconciliation — compare the local OME against an authoritative exchange
//! snapshot (current open orders + recent trades) and repair drift.
//!
//! Pure logic over snapshots so it is unit-testable and identical for the
//! live REST sweep and a replayed/offline source. It handles:
//!
//!  - missed fills: a trade exists at the venue for our order but the OME never
//!    applied it (WS gap) → synthesise an authoritative cumulative Fill
//!  - ghost orders: local thinks an order is LIVE but the venue reports it gone
//!    AND it has a fill that completed it → mark Filled; otherwise it was
//!    cancelled away → mark Cancelled. We never invent a resting order.
//!
//! Caller is responsible for fetching the snapshot and executing any venue
//! cancel for true ghosts; this module returns explicit actions.

use crate::model::*;
use crate::ome::{FillDelta, LateFill, Ome};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct VenueTrade {
    pub venue_order_id: String,
    pub trade_id: String,
    pub token_id: TokenId,
    pub side: Side,
    /// Per-trade executed size (NOT cumulative).
    pub size: rust_decimal::Decimal,
    pub price: rust_decimal::Decimal,
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// The venue's maker/taker report for this trade, when it has one. A gap
    /// fill synthesized from it must charge the fee the venue actually charged,
    /// so the role travels with the trade rather than being re-guessed (E17).
    pub maker: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct VenueSnapshot {
    /// Exchange order ids currently resting/open.
    pub open_order_ids: Vec<String>,
    /// Recent trades for our orders (any status); used to fill gaps.
    pub trades: Vec<VenueTrade>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
// `FilledGap` carries a whole `FillDelta` (~232 bytes) while the other variants
// carry only an order id, so the enum is as large as its largest variant. Boxing
// the delta would shrink every action at the cost of a heap allocation and an
// indirection on every match arm of a type with three variants — and this enum
// is constructed once per reconcile gap, not in a hot loop. Not worth it.
#[allow(clippy::large_enum_variant)]
pub enum ReconcileAction {
    /// A fill missing from the local OME was applied (WS gap repaired).
    FilledGap {
        core_order_id: OrderId,
        delta: FillDelta,
    },
    /// Local-live order is fully filled per the venue (no longer open).
    MarkedFilled { core_order_id: OrderId },
    /// Local-live order vanished from the venue with no fill → marked cancelled.
    MarkedCancelled { core_order_id: OrderId },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    pub actions: Vec<ReconcileAction>,
    /// Local-live orders with no venue presence and no completing fill; a real
    /// ghost that may warrant a cancel if the venue snapshot is trustworthy.
    pub suspect_ghost_ids: Vec<OrderId>,
    /// Trades the venue executed on orders the OME has never heard of — a
    /// manual close the operator made directly on the venue. The OME cannot
    /// apply these (no order to fill), so the caller folds them into the
    /// position book by token id; otherwise the position is a ghost the exit
    /// rules keep managing forever.
    pub unknown_fills: Vec<VenueTrade>,
    /// Fills the sweep applied to an order that had already ended (issue #178):
    /// the size is booked, the terminal status keeps, and the caller reports it.
    /// Before the fix these silently "revived" a Cancelled order into
    /// PartiallyFilled, so a cancelled leg came back to life in the exit rules.
    pub late_fills: Vec<LateFill>,
}

/// Reconcile the OME against an authoritative snapshot.
///
/// `ome.apply_fill` stays the only mutator of fill state. Status repair for
/// ghosts is done through mark_terminal so the state machine stays consistent.
pub fn reconcile(ome: &mut Ome, snap: &VenueSnapshot) -> CoreResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let open: std::collections::HashSet<&str> =
        snap.open_order_ids.iter().map(String::as_str).collect();

    // 1) Repair missed fills. Each venue trade row is its own idempotent unit
    //    (keyed by the venue's trade id, shared with the user-WS channel), so
    //    applying per row — never summing an order's cumulative total into one
    //    fresh-keyed fill — means a fill the WS already booked is a no-op here
    //    and a missing one is booked exactly once, at its own price.
    for t in &snap.trades {
        let Some(order) = ome.by_venue_or_id(&t.venue_order_id).cloned() else {
            // Not one of our orders (manual close on the venue): hand it to
            // the caller for position-book reconciliation.
            report.unknown_fills.push(t.clone());
            continue;
        };
        let trade_id = if t.trade_id.is_empty() {
            t.tx_hash.clone()
        } else {
            Some(t.trade_id.clone())
        };
        let fill = Fill {
            order_id: order.order_id.clone(),
            trade_id,
            token_id: order.token_id.clone(),
            side: order.side,
            price: t.price,
            size: t.size, // per-trade executed size, NOT cumulative
            status: FillStatus::Confirmed,
            ts_ms: t.ts_ms,
            tx_hash: t.tx_hash.clone(),
            // Carry the venue's role through the gap fill.
            maker: t.maker,
        };
        // `apply_fill_report` (not `apply_fill`): a trade for an order that has
        // already ended is absorbed into a terminal status, and the sweep must
        // hand that fact back to the caller instead of dropping it (#178).
        let outcome = ome.apply_fill_report(fill, snap.now_ms)?;
        if let Some(delta) = outcome.delta {
            report.actions.push(ReconcileAction::FilledGap {
                core_order_id: order.order_id.clone(),
                delta,
            });
        }
        if let Some(late) = outcome.late_fill {
            report.late_fills.push(late);
        }
    }
    // 2) Ghost detection over orders we still consider live. Snapshot the
    //    candidate ids first so the OME is not borrowed while we mutate it.
    let candidates: Vec<(OrderId, Option<OrderId>)> = ome
        .live_orders()
        .into_iter()
        .map(|o| (o.order_id.clone(), o.venue_order_id.clone()))
        .collect();

    for (core_id, venue_id) in candidates {
        let Some(venue_id) = venue_id else {
            // Dry orders have no venue id; never touch them from a venue snapshot.
            continue;
        };
        if open.contains(venue_id.as_str()) {
            continue; // venue still has it open — fine
        }
        // Gone from the venue. Refresh: a fill may have completed it in step 1.
        let fresh_status = ome.get(&core_id).map(|o| (o.status, o.filled_size, o.size));
        match fresh_status {
            Some((status, filled, size)) if status == OrderStatus::Filled || filled >= size => {
                if status != OrderStatus::Filled {
                    ome.mark_terminal(&core_id, OrderStatus::Filled, snap.now_ms)?;
                }
                report.actions.push(ReconcileAction::MarkedFilled {
                    core_order_id: core_id,
                });
            }
            Some((_, filled, _)) if filled > rust_decimal::Decimal::ZERO => {
                ome.mark_terminal(&core_id, OrderStatus::Cancelled, snap.now_ms)?;
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
            _ => {
                ome.mark_terminal(&core_id, OrderStatus::Cancelled, snap.now_ms)?;
                report.suspect_ghost_ids.push(core_id.clone());
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
        }
    }

    Ok(report)
}

// ── In-kernel accounting audit (issue #189) ──────────────────────────────────
//
// The sweep above repairs against a venue snapshot the CALLER fetches, which in
// practice meant an external script (`scripts/account-drift-check.mjs`) polling a
// running core over IPC. Nothing inside the kernel noticed a drifting ledger, and
// the drift sources this PR fixes (a reservation that never reached zero, an
// idempotency table that forgot a trade across a restart) accumulate silently.
//
// This section is the in-kernel counterpart: pure logic over a snapshot `Core`
// assembles from its own books, so it is unit-testable and, above all, ALERTING —
// a failing audit halts NEW ENTRIES, persists a record and reaches the panel.
//
// Three independent views of the same money, deliberately of different strength:
//
//   structural  the core's own books against each other, or against another LOCAL
//               record read under the same lock. No external fetch, no race, so
//               a failure is believed on the first audit. Halts immediately.
//   cross-source the venue's free cash against the ledger — the one figure that
//               arrives asynchronously and can be a stale read, so two failures
//               in a row are required to halt.
//
// The cash identity is the one `account-drift-check.mjs` asserts, in its anchored
// form:
//
//   balance − anchor.balance == (realized − anchor.realized)   trade records
//                             − (spent    − anchor.spent)      cash open positions paid
//                             + (received − anchor.received)   cash they already returned
//
// The anchor telescopes the unknown opening cash out (a LIVE core has no seed, and
// a restarted DRY core re-seeds its ledger without replaying history), and the
// deltas chain back to the first audit — so it is exactly as strong as the global
// form over a session while needing no knowledge of where the money started.
//
// It IS a real check, not a formality: cash that moved between two audits with no
// trade behind it — a fill the WS never delivered and the venue silently healed,
// a reservation that was never released — is exactly the residual it reports, and
// the script's own docs call that out as the drift it exists to catch.

/// Tolerance for a comparison that crosses the f64 boundary (the trade summary
/// is f64 on disk) or a venue report rounded to cents. 1e-4 USD is a hundredth
/// of a cent: far below any real drift, far above transport noise.
pub const AUDIT_TOL_USD: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

/// Tolerance for the ONE cross-source leg: the venue's reported free cash. Two
/// cents — the same figure the plugin host already uses for this comparison
/// (`market::host::venue_free_balance`) — because the two sides are computed by
/// different parties (our reservation table vs the venue's own notion of what is
/// locked), and demanding exact equality there would halt entries over a cent of
/// rounding. Everything compared WITHIN the core uses [`AUDIT_TOL_USD`].
pub const AUDIT_VENUE_TOL_USD: Decimal = Decimal::from_parts(2, 0, 0, false, 2);

/// The cash identity at one instant, for the anchor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CashIdentity {
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    /// All-time realized net PnL from the trade log (the running summary, not a
    /// truncated history window).
    #[serde(with = "crate::decimal")]
    pub realized: Decimal,
    /// Cash open positions have paid (entry cost + entry fees).
    #[serde(with = "crate::decimal")]
    pub spent: Decimal,
    /// Cash open positions have returned (proceeds − exit fees).
    #[serde(with = "crate::decimal")]
    pub received: Decimal,
}

impl CashIdentity {
    /// What this identity says the balance should be, given an anchor.
    fn expected_after(&self, anchor: &CashIdentity) -> Decimal {
        anchor.balance + (self.realized - anchor.realized) - (self.spent - anchor.spent)
            + (self.received - anchor.received)
    }
}

/// Everything the pure audit needs, assembled by `Core` from its own state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditInput {
    pub now_ms: i64,
    /// True for the modes that trade against a venue (so a venue report exists
    /// to compare with).
    pub live: bool,
    pub identity: CashIdentity,
    /// What the ledger's reservation table currently totals.
    pub reserved: Decimal,
    /// `available + reserved == balance` (structural, see `Ledger::is_balanced`).
    pub ledger_balanced: bool,
    /// Reserved notional the local balance does not cover.
    pub unfunded_reserved: Decimal,
    /// Σ `limit_price * remaining` over live BUY orders — what the reservation
    /// table must equal exactly (issue #181).
    pub open_buy_notional: Decimal,
    pub open_buys: usize,
    /// Reservation entries whose order is not a tracked live BUY: a hold nobody
    /// can release. Reported by name so the operator can act.
    pub stray_reservations: Vec<(OrderId, Decimal)>,
    /// Last free-cash figure the venue plugin reported, with its timestamp.
    pub venue_free: Option<Decimal>,
    pub venue_free_at_ms: Option<i64>,
    /// The last time the ledger's cash was SET from the venue instead of moved by
    /// a fill: `(at_ms, delta)`. The plugin host does this on purpose (when
    /// nothing rests, the venue's free cash IS our cash), so a trade-leg residual
    /// that equals this delta is the venue's own correction rather than a local
    /// inconsistency — see the trade-leg check.
    pub ledger_realigned: Option<(i64, Decimal)>,
    pub open_positions: usize,
    pub seen_trades: u64,
    /// False when no trade log is configured (the backtester, most in-process
    /// tests). The trade-leg check is then not covered rather than reported as a
    /// failure: with no log there is no record of realized PnL, and a closed
    /// position would look like cash that vanished.
    pub has_trade_log: bool,
}

/// One line of the audit: what was compared, against what, and by how much it
/// missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditCheck {
    /// Stable machine name (`cash_identity`, `reservations_match_open_buys`, …).
    pub name: String,
    pub ok: bool,
    /// A local (same-source) failure is believed at once; a cross-source one
    /// needs a second consecutive failure before it halts.
    pub structural: bool,
    #[serde(with = "crate::decimal")]
    pub expected: Decimal,
    #[serde(with = "crate::decimal")]
    pub actual: Decimal,
    #[serde(with = "crate::decimal")]
    pub drift: Decimal,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offenders: Vec<String>,
}

impl AuditCheck {
    fn new(
        name: &str,
        structural: bool,
        expected: Decimal,
        actual: Decimal,
        detail: String,
    ) -> Self {
        Self::with_tol(name, structural, expected, actual, detail, AUDIT_TOL_USD)
    }

    /// Same check with an explicit tolerance — used by the venue leg, which is
    /// compared across two independent sources.
    fn with_tol(
        name: &str,
        structural: bool,
        expected: Decimal,
        actual: Decimal,
        detail: String,
        tol: Decimal,
    ) -> Self {
        let drift = actual - expected;
        Self {
            name: name.to_string(),
            ok: drift.abs() <= tol,
            structural,
            expected,
            actual,
            drift,
            detail,
            offenders: Vec::new(),
        }
    }

    fn pass(name: &str, structural: bool, detail: String, value: Decimal) -> Self {
        Self {
            name: name.to_string(),
            ok: true,
            structural,
            expected: value,
            actual: value,
            drift: Decimal::ZERO,
            detail,
            offenders: Vec::new(),
        }
    }
}

/// The audit's verdict for one instant. Serializes straight into the audit JSONL
/// and into the panel's `engineStats.accountingAudit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditReport {
    pub at_ms: i64,
    /// True when a venue is in play (the venue leg was evaluated).
    pub live: bool,
    pub ok: bool,
    /// A structural check failed: the core's own books disagree.
    pub structural_failure: bool,
    /// The largest absolute drift across all failing checks.
    #[serde(with = "crate::decimal")]
    pub drift_usd: Decimal,
    /// This audit's own verdict: halt new entries or not.
    pub halt_entries: bool,
    pub checks: Vec<AuditCheck>,
    /// The baseline this audit compared against (`None` on the first audit: the
    /// anchored identity is not defined until a clean audit takes the anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<CashIdentity>,
    pub open_positions: usize,
    pub open_buys: usize,
    pub seen_trades: u64,
    pub note: String,
}

impl AuditReport {
    /// One-line summary for the log/panel.
    pub fn summary(&self) -> String {
        let failed: Vec<&str> = self
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect();
        if failed.is_empty() {
            format!(
                "ok balance={} reserved_open_buys={} open_positions={}",
                self.checks
                    .first()
                    .map(|c| c.actual.to_string())
                    .unwrap_or_default(),
                self.checks
                    .iter()
                    .find(|c| c.name == "reservations_match_open_buys")
                    .map(|c| c.expected.to_string())
                    .unwrap_or_default(),
                self.open_positions
            )
        } else {
            format!(
                "FAILED {} drift={} {}",
                failed.join(","),
                self.drift_usd,
                self.note
            )
        }
    }
}

/// Compare the core's accounting views. Pure: no I/O, no clocks, no mutation.
pub fn audit_accounting(input: &AuditInput, anchor: Option<&CashIdentity>) -> AuditReport {
    let id = &input.identity;
    let mut checks: Vec<AuditCheck> = Vec::new();

    // 1) The ledger's own cash identity. Structural: both sides are the same
    //    struct, so a failure is a bug in the ledger, not a stale read.
    checks.push(AuditCheck {
        name: "cash_identity".into(),
        ok: input.ledger_balanced && input.unfunded_reserved <= AUDIT_TOL_USD,
        structural: true,
        expected: Decimal::ZERO,
        actual: input.unfunded_reserved,
        drift: input.unfunded_reserved,
        detail: format!(
            "available ({}) + reserved ({}) == balance ({}); unfunded {}",
            id.balance - input.reserved,
            input.reserved,
            id.balance,
            input.unfunded_reserved
        ),
        offenders: Vec::new(),
    });

    // 2) Every reservation is exactly the outstanding notional of a live BUY
    //    order — no residue from a settled fill, no phantom hold (issue #181).
    let mut reservation = AuditCheck::new(
        "reservations_match_open_buys",
        true,
        input.open_buy_notional,
        input.reserved,
        format!(
            "reserved must equal the unfilled notional of {} live BUY order(s)",
            input.open_buys
        ),
    );
    if !input.stray_reservations.is_empty() {
        reservation.ok = false;
        reservation.offenders = input
            .stray_reservations
            .iter()
            .map(|(id, v)| format!("{id}={v}"))
            .collect();
        reservation.detail = format!(
            "{} reservation(s) held for orders that are not live BUYs: {}",
            input.stray_reservations.len(),
            reservation.offenders.join(", ")
        );
    }
    checks.push(reservation);

    // 3) The trade ledger against the cash ledger, anchored. Structural: both
    //    records are local and read under the same lock, so there is no race to
    //    forgive — and a manual venue fill the reconcile sweep folded into cash
    //    without a trade record is a real (and permanent) inconsistency an
    //    operator must see. The remedy is explicit: fix the trade record, or
    //    restart (which re-anchors), or turn `audit_halt_entries` off.
    //
    //    One exception makes it cross-source: the plugin host SETS the ledger's
    //    cash from the venue's free cash when nothing rests (`set_balance`), so a
    //    residual that equals that correction is the VENUE's number, not ours.
    //    It stays a failure — the trade record genuinely disagrees with the
    //    money — but it halts on a repeat like the venue leg instead of on the
    //    first audit, because the correcting party is external.
    if !input.has_trade_log {
        checks.push(AuditCheck::pass(
            "trade_ledger_identity",
            true,
            "no trade log configured: not covered".into(),
            id.balance,
        ));
    } else {
        match anchor {
            None => checks.push(AuditCheck::pass(
                "trade_ledger_identity",
                true,
                "anchor pending (first clean audit establishes the baseline)".into(),
                id.balance,
            )),
            Some(a) => {
                let expected = id.expected_after(a);
                let realign = input
                    .ledger_realigned
                    .filter(|(_, d)| (id.balance - expected - d).abs() <= AUDIT_TOL_USD);
                let mut c = AuditCheck::new(
                    "trade_ledger_identity",
                    realign.is_none(),
                    expected,
                    id.balance,
                    format!(
                        "balance since anchor vs trade records: realized {} → {} ({:+}), \
                         open cash paid {} → {} ({:+}), returned {} → {} ({:+})",
                        a.realized,
                        id.realized,
                        id.realized - a.realized,
                        a.spent,
                        id.spent,
                        id.spent - a.spent,
                        a.received,
                        id.received,
                        id.received - a.received,
                    ),
                );
                if let Some((at_ms, d)) = realign {
                    c.detail.push_str(&format!(
                        "; residual is the venue's own ledger correction at {at_ms} (Δ{d}): \
                         cash was set from the venue, the trade record has not caught up"
                    ));
                }
                if input.seen_trades == 0 {
                    // Nothing has been closed yet, so the trade record says nothing
                    // about the cash beyond "no realized PnL" — still a real check
                    // (an unexplained cash move shows up immediately), but say so.
                    c.detail.push_str("; no closed trades yet");
                }
                checks.push(c);
            }
        }
    }

    // 4) The venue's free cash against what the ledger believes is free
    //    (cross-source: a fetch race or a stale figure is possible, and the two
    //    reporting conventions differ while orders rest — see the plugin host).
    if input.live {
        match input.venue_free {
            None => checks.push(AuditCheck::pass(
                "venue_free_cash",
                false,
                "no venue balance reported yet".into(),
                id.balance,
            )),
            Some(free) => {
                let expected = id.balance - input.reserved;
                let mut c = AuditCheck::with_tol(
                    "venue_free_cash",
                    false,
                    expected,
                    free,
                    format!(
                        "ledger free cash vs venue free cash (reported {}ms ago)",
                        input
                            .venue_free_at_ms
                            .map(|t| (input.now_ms - t).max(0))
                            .unwrap_or(0)
                    ),
                    AUDIT_VENUE_TOL_USD,
                );
                if input.open_buys > 0 {
                    // With orders resting, the venue may or may not net their
                    // commitments out of the figure it reports, so only a large
                    // gap is evidence — and it is still cross-source.
                    c.detail
                        .push_str("; resting BUY orders: the venue may not net them out");
                }
                checks.push(c);
            }
        }
    }

    let drift_usd = checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.drift.abs())
        .max()
        .unwrap_or(Decimal::ZERO);
    let structural_failure = checks.iter().any(|c| !c.ok && c.structural);
    let ok = checks.iter().all(|c| c.ok);
    let note = if ok {
        format!(
            "balance={} reserved={} open_buys={} open_positions={}",
            id.balance, input.reserved, input.open_buys, input.open_positions
        )
    } else {
        checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| format!("{}: {} (drift {})", c.name, c.detail, c.drift))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    AuditReport {
        at_ms: input.now_ms,
        live: input.live,
        ok,
        structural_failure,
        drift_usd,
        // The caller may still downgrade this (a first cross-source failure is
        // not yet a halt) — `Core` owns the consecutive-failure policy. What the
        // audit states is whether this instant's evidence is bad enough to halt.
        halt_entries: structural_failure,
        checks,
        anchor: anchor.copied(),
        open_positions: input.open_positions,
        open_buys: input.open_buys,
        seen_trades: input.seen_trades,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ome::SubmitParams;
    use rust_decimal_macros::dec;

    fn req(key: &str, size: rust_decimal::Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: dec!(0.4),
            size,
            internal_key: key.into(),
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn live_order(ome: &mut Ome, key: &str, core_id: &str, venue_id: &str, now: i64) {
        ome.submit(SubmitParams {
            order_id: core_id.into(),
            request: req(key, dec!(10)),
            submitted_at_ms: now,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.bind_venue(core_id, venue_id.into(), now).unwrap();
        ome.mark_live(core_id, now).unwrap();
    }

    #[test]
    fn repairs_missed_fill_from_rest_trade() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        // WS gap: no fill applied locally. Venue reports a full 10 filled.
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Buy,
                size: dec!(10),
                price: dec!(0.4),
                ts_ms: 2,
                tx_hash: None,
                maker: Some(true),
            }],
            now_ms: 3,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::FilledGap { .. }))
        );
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Filled);
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(10));
    }

    #[test]
    fn repairs_only_the_missing_increment_of_a_partially_known_fill() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        // The user-WS stream already booked trade t1: 4 of the order's 10
        // shares are locally known. A SELL, so the wrong increment would
        // settle phantom sale proceeds into the ledger.
        ome.apply_fill(
            Fill {
                order_id: "c1".into(),
                trade_id: Some("t1".into()),
                token_id: "tok".into(),
                side: Side::Sell,
                price: dec!(0.5),
                size: dec!(4),
                status: FillStatus::Confirmed,
                ts_ms: 2,
                tx_hash: None,
                maker: Some(true),
            },
            2,
        )
        .unwrap();
        // The REST sweep reports t1 (4) plus t2 (2): the order's cumulative
        // filled size is 6 — the missing increment is 2, not 6.
        let snap = VenueSnapshot {
            open_order_ids: vec!["v1".into()],
            trades: vec![
                VenueTrade {
                    venue_order_id: "v1".into(),
                    trade_id: "t1".into(),
                    token_id: "tok".into(),
                    side: Side::Sell,
                    size: dec!(4),
                    price: dec!(0.5),
                    ts_ms: 2,
                    tx_hash: None,
                    maker: Some(true),
                },
                VenueTrade {
                    venue_order_id: "v1".into(),
                    trade_id: "t2".into(),
                    token_id: "tok".into(),
                    side: Side::Sell,
                    size: dec!(2),
                    price: dec!(0.5),
                    ts_ms: 3,
                    tx_hash: None,
                    maker: Some(false),
                },
            ],
            now_ms: 5,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        // Exactly one gap action, carrying exactly the missing increment.
        let gaps: Vec<&ReconcileAction> = report
            .actions
            .iter()
            .filter(|a| matches!(a, ReconcileAction::FilledGap { .. }))
            .collect();
        assert_eq!(gaps.len(), 1);
        if let ReconcileAction::FilledGap { delta, .. } = gaps[0] {
            assert_eq!(delta.delta, dec!(2));
        }
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(6));
        // A second identical sweep stays stable (no re-application).
        let again = reconcile(&mut ome, &snap).unwrap();
        assert!(
            again
                .actions
                .iter()
                .all(|a| !matches!(a, ReconcileAction::FilledGap { .. }))
        );
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(6));
    }

    #[test]
    fn detects_ghost_order_without_fill() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        // Venue no longer lists it open and there are no trades → ghost/cancelled.
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 5,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert_eq!(report.suspect_ghost_ids, vec!["c1".to_string()]);
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Cancelled);
    }

    #[test]
    fn leaves_open_orders_untouched_and_is_idempotent() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        let snap = VenueSnapshot {
            open_order_ids: vec!["v1".into()],
            trades: vec![],
            now_ms: 2,
        };
        let r1 = reconcile(&mut ome, &snap).unwrap();
        assert!(r1.actions.is_empty());
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Live);
        // Idempotent: still nothing on a second identical sweep.
        let r2 = reconcile(&mut ome, &snap).unwrap();
        assert!(r2.actions.is_empty());
    }

    #[test]
    fn dry_orders_without_venue_id_are_ignored() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "dry1".into(),
            request: req("k9", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("dry1", 1).unwrap();
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 2,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert!(report.actions.is_empty());
        assert_eq!(ome.get("dry1").unwrap().status, OrderStatus::Live);
    }

    /// A venue trade on an order the OME never issued (a manual close made
    /// directly on the venue) must surface as an unknown fill for the caller
    /// to fold into the position book — not be silently dropped, which left
    /// ghost positions the exit rules kept managing forever.
    #[test]
    fn manual_close_trade_surfaces_as_unknown_fill() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        let snap = VenueSnapshot {
            open_order_ids: vec!["v1".into()],
            trades: vec![
                // Ours: known venue id → OME gap-fill path.
                VenueTrade {
                    venue_order_id: "v1".into(),
                    trade_id: "t1".into(),
                    token_id: "tok".into(),
                    side: Side::Sell,
                    size: dec!(2),
                    price: dec!(0.5),
                    ts_ms: 10,
                    tx_hash: None,
                    maker: Some(false),
                },
                // Manual: an id the OME has never heard of.
                VenueTrade {
                    venue_order_id: "manual-1".into(),
                    trade_id: "t2".into(),
                    token_id: "tok".into(),
                    side: Side::Sell,
                    size: dec!(10),
                    price: dec!(0.6),
                    ts_ms: 20,
                    tx_hash: None,
                    maker: Some(false),
                },
            ],
            now_ms: 30,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert_eq!(
            report.unknown_fills.len(),
            1,
            "exactly the manual trade is unknown"
        );
        assert_eq!(report.unknown_fills[0].venue_order_id, "manual-1");
        assert_eq!(report.unknown_fills[0].size, dec!(10));
        // The known order got its gap fill; the OME is untouched by the manual one.
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(2));
    }

    /// A sweep can deliver a trade for an order that has already ended. The
    /// report must say so (issue #178) — the fill is booked, the status keeps.
    #[test]
    fn sweep_reports_a_fill_absorbed_by_a_terminal_order() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        ome.mark_terminal("c1", OrderStatus::Cancelled, 2).unwrap();
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t-late".into(),
                token_id: "tok".into(),
                side: Side::Buy,
                size: dec!(3),
                price: dec!(0.4),
                ts_ms: 3,
                tx_hash: None,
                maker: Some(true),
            }],
            now_ms: 4,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert_eq!(report.late_fills.len(), 1);
        let late = &report.late_fills[0];
        assert_eq!(late.order_id, "c1");
        assert_eq!(late.absorbed_by, Some(OrderStatus::Cancelled));
        assert_eq!(late.size, dec!(3));
        assert!(!late.overfill);
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Cancelled);
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(3));
    }

    // ── #189: the audit itself ───────────────────────────────────────────────

    fn clean_input() -> AuditInput {
        AuditInput {
            now_ms: 1_000,
            live: false,
            identity: CashIdentity {
                balance: dec!(100),
                realized: dec!(0),
                spent: Decimal::ZERO,
                received: Decimal::ZERO,
            },
            reserved: Decimal::ZERO,
            ledger_balanced: true,
            unfunded_reserved: Decimal::ZERO,
            open_buy_notional: Decimal::ZERO,
            open_buys: 0,
            stray_reservations: Vec::new(),
            venue_free: None,
            venue_free_at_ms: None,
            ledger_realigned: None,
            open_positions: 0,
            seen_trades: 0,
            has_trade_log: true,
        }
    }

    #[test]
    fn clean_books_pass_every_check() {
        let input = clean_input();
        let r = audit_accounting(&input, None);
        assert!(r.ok, "{:?}", r.checks);
        assert!(!r.structural_failure);
        assert!(!r.halt_entries);
        assert_eq!(r.drift_usd, Decimal::ZERO);
        assert_eq!(r.anchor, None);
        // Every check is named and present (the venue leg is skipped when not live).
        let names: Vec<&str> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "cash_identity",
                "reservations_match_open_buys",
                "trade_ledger_identity"
            ]
        );
    }

    /// The invariant #181 restores: a reservation that outlives its order (the
    /// old price-improvement residue) is a STRUCTURAL failure — it must halt new
    /// entries on the first audit, because a phantom hold shrinks the capital
    /// every later entry is gated against.
    #[test]
    fn reservation_residue_is_a_structural_failure() {
        let mut input = clean_input();
        input.reserved = dec!(0.5);
        input.stray_reservations = vec![("o-gone".into(), dec!(0.5))];
        let r = audit_accounting(&input, None);
        assert!(!r.ok);
        assert!(r.structural_failure);
        assert!(r.halt_entries, "structural failures halt immediately");
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "reservations_match_open_buys")
            .unwrap();
        assert_eq!(c.drift, dec!(0.5));
        assert_eq!(c.offenders, vec!["o-gone=0.5"]);
    }

    /// A reservation total that disagrees with the live BUY commitments is caught
    /// even when every key belongs to a tracked order (a stale amount, not a
    /// stale key).
    #[test]
    fn reservation_total_mismatch_is_caught() {
        let mut input = clean_input();
        input.open_buys = 1;
        input.open_buy_notional = dec!(4);
        input.reserved = dec!(6);
        let r = audit_accounting(&input, None);
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "reservations_match_open_buys")
            .unwrap();
        assert!(!c.ok);
        assert_eq!(c.drift, dec!(2));
    }

    /// The anchored cash identity: the trade records explain every movement of
    /// the ledger since the anchor. Cash that moved with no trade behind it — the
    /// classic missed fill, or the venue quietly healing a balance — shows up as
    /// the residual.
    #[test]
    fn anchored_identity_catches_unexplained_cash() {
        let anchor = CashIdentity {
            balance: dec!(100),
            realized: dec!(0),
            spent: Decimal::ZERO,
            received: Decimal::ZERO,
        };
        // Since the anchor: a closed trade realized +4 (its entry cost and exit
        // proceeds live INSIDE `realized`; the two flow sums below belong to
        // positions still open — one paid 6 to enter, one partial exit returned
        // 10). 100 + 4 - 6 + 10 = 108.
        let mut input = clean_input();
        input.identity = CashIdentity {
            balance: dec!(108),
            realized: dec!(4),
            spent: dec!(6),
            received: dec!(10),
        };
        let r = audit_accounting(&input, Some(&anchor));
        assert!(r.ok, "{:?}", r.checks);

        // Now 5 USD leaves the ledger with no trade to explain it.
        input.identity.balance = dec!(103);
        let r = audit_accounting(&input, Some(&anchor));
        assert!(!r.ok);
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "trade_ledger_identity")
            .unwrap();
        assert_eq!(c.expected, dec!(108));
        assert_eq!(c.actual, dec!(103));
        assert_eq!(c.drift, dec!(-5));
        assert_eq!(r.drift_usd, dec!(5));
        // Both records are local and read under one lock, so there is no fetch
        // race to forgive: a local inconsistency halts on the first audit.
        assert!(c.structural);
        assert!(r.structural_failure);
        assert!(r.halt_entries);
    }

    /// Without a trade log there is no record of realized PnL, so the trade leg
    /// is explicitly not covered instead of reported as drift — otherwise every
    /// close in a logless replay would look like cash that vanished.
    #[test]
    fn trade_leg_is_skipped_without_a_trade_log() {
        let anchor = CashIdentity {
            balance: dec!(100),
            realized: dec!(0),
            spent: Decimal::ZERO,
            received: Decimal::ZERO,
        };
        let mut input = clean_input();
        input.has_trade_log = false;
        // A close that moved cash but could never have been recorded.
        input.identity = CashIdentity {
            balance: dec!(104),
            realized: dec!(0),
            spent: Decimal::ZERO,
            received: Decimal::ZERO,
        };
        let r = audit_accounting(&input, Some(&anchor));
        assert!(r.ok, "{:?}", r.checks);
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "trade_ledger_identity")
            .unwrap();
        assert!(c.detail.contains("not covered"));
    }

    /// The venue leg compares like with like: the ledger's free cash against the
    /// venue's free cash, and it is only evaluated for a live core that has
    /// actually reported a balance.
    #[test]
    fn venue_leg_compares_free_cash_when_live() {
        let mut input = clean_input();
        input.live = true;
        input.identity.balance = dec!(100);
        input.reserved = dec!(40);
        input.open_buys = 1;
        input.open_buy_notional = dec!(40);
        // Not reported yet: nothing to compare, no failure.
        let r = audit_accounting(&input, None);
        assert!(r.ok);
        assert!(r.checks.iter().any(|c| c.name == "venue_free_cash"
            && c.ok
            && c.detail.contains("no venue balance reported")));
        // Reported and matching (free = balance − reserved).
        input.venue_free = Some(dec!(60));
        input.venue_free_at_ms = Some(900);
        let r = audit_accounting(&input, None);
        assert!(r.ok, "{:?}", r.checks);
        // A cent of venue rounding is not drift: the two sides are computed by
        // different parties, so this leg carries its own (2-cent) tolerance.
        input.venue_free = Some(dec!(60.01));
        let r = audit_accounting(&input, None);
        assert!(r.ok, "{:?}", r.checks);
        // Reported and short by 5: cross-source failure, named with its drift.
        input.venue_free = Some(dec!(55));
        let r = audit_accounting(&input, None);
        assert!(!r.ok);
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "venue_free_cash")
            .unwrap();
        assert_eq!(c.drift, dec!(-5));
        assert!(!c.structural);
        assert!(!r.structural_failure);
    }

    /// A residual the VENUE itself created (the host sets the ledger's cash from
    /// the venue's free cash when nothing rests) is not a local inconsistency: it
    /// is downgraded to a cross-source failure — reported and persisted, but
    /// halting on a repeat instead of on the first audit. An EXPLAINED residual is
    /// still a failure: the trade record disagrees with the money.
    #[test]
    fn a_venue_correction_downgrades_the_trade_leg_to_cross_source() {
        let anchor = CashIdentity {
            balance: dec!(100),
            realized: dec!(0),
            spent: Decimal::ZERO,
            received: Decimal::ZERO,
        };
        let mut input = clean_input();
        // Cash is 5 short of what the trade records explain…
        input.identity.balance = dec!(95);
        let r = audit_accounting(&input, Some(&anchor));
        let c = |r: &AuditReport| {
            r.checks
                .iter()
                .find(|c| c.name == "trade_ledger_identity")
                .unwrap()
                .clone()
        };
        // …unexplained: structural, halts at once.
        assert!(!r.ok);
        assert!(r.structural_failure);
        assert!(c(&r).structural);

        // The same 5 USD, but the host set the ledger from the venue by exactly
        // that amount: the correcting party is external, so it is cross-source.
        input.ledger_realigned = Some((900, dec!(-5)));
        let r = audit_accounting(&input, Some(&anchor));
        assert!(
            !r.ok,
            "still a failure: the trade record disagrees with the cash"
        );
        assert!(!r.structural_failure);
        assert!(!c(&r).structural);
        assert_eq!(c(&r).drift, dec!(-5));
        assert!(c(&r).detail.contains("venue's own ledger correction"));

        // A realignment of a DIFFERENT size does not explain it.
        input.ledger_realigned = Some((900, dec!(-3)));
        let r = audit_accounting(&input, Some(&anchor));
        assert!(r.structural_failure);
        assert!(c(&r).structural);
    }

    /// A restarted core has no anchor: the first audit reports the structural
    /// checks and says the cash identity is not yet defined, rather than
    /// inventing a baseline.
    #[test]
    fn first_audit_defers_the_anchored_check() {
        let input = clean_input();
        let r = audit_accounting(&input, None);
        let c = r
            .checks
            .iter()
            .find(|c| c.name == "trade_ledger_identity")
            .unwrap();
        assert!(c.ok);
        assert!(c.detail.contains("anchor pending"));
        assert_eq!(r.anchor, None);
        // With an anchor the same books still pass and the anchor is echoed back.
        let anchor = input.identity;
        let r = audit_accounting(&input, Some(&anchor));
        assert!(r.ok);
        assert_eq!(r.anchor, Some(anchor));
    }

    /// A ledger whose reserved exceeds its balance fails the local identity.
    #[test]
    fn unbalanced_ledger_is_structural() {
        let mut input = clean_input();
        input.ledger_balanced = false;
        input.unfunded_reserved = dec!(10);
        let r = audit_accounting(&input, None);
        assert!(r.structural_failure);
        assert!(r.halt_entries);
        let c = r.checks.iter().find(|c| c.name == "cash_identity").unwrap();
        assert!(!c.ok);
        assert_eq!(c.drift, dec!(10));
    }
}
