//! The in-kernel accounting audit and its journal (#780).

use crate::ledger::Ledger;
use crate::model::*;
use crate::reconcile::{AuditInput, AuditReport, CashIdentity};
use crate::schema::Event;
use rust_decimal::Decimal;

use crate::service::{AUDIT_HEARTBEAT_MS, f64_to_decimal};

use super::Core;

impl Core {
    /// Assemble the audit's input from the core's own state. Cheap enough to run
    /// on the maintenance tick: a handful of sums over live orders, open
    /// positions and the reservation table.
    pub(crate) fn build_audit_input(&self, now_ms: i64) -> AuditInput {
        // The audit's cash identity describes the ACTIVE account's book (§9.5,
        // E28): `reserved` reads the active ledger, so the live-BUY notional it
        // is compared against must count only the ACTIVE account's orders. A
        // sibling account's live BUY is spoken for in ITS book — counting it
        // here made `reservations_match_open_buys` drift by that notional
        // whenever any other account held an open order.
        let active_id = self.accounts.active_id().clone();
        let mut open_buy_notional = Decimal::ZERO;
        let mut open_buys = 0usize;
        for o in self.ome.live_orders() {
            if o.side != Side::Buy || o.account_id != active_id {
                continue;
            }
            open_buys += 1;
            open_buy_notional += o.price * (o.size - o.filled_size).max(Decimal::ZERO);
        }
        // A reservation is "stray" when the order behind it is not a tracked live
        // BUY: nobody can ever release it, so it is a permanent phantom hold.
        let stray_reservations: Vec<(OrderId, Decimal)> = self
            .accounts
            .active_ledger()
            .reservations_snapshot()
            .into_iter()
            .filter(|(id, _)| {
                !self
                    .ome
                    .get(id)
                    .map(|o| o.side == Side::Buy && o.status.is_live())
                    .unwrap_or(false)
            })
            .collect();
        // E28 (§9.2): every fold below scopes to the ACTIVE account — the
        // identity describes ONE book, so a sibling account's realized PnL,
        // pending redemption, or open position must not move it (they once
        // did: the global summary/global receivable/all-books positions fold
        // made `trade_ledger_identity` drift by exactly what the sibling
        // account traded).
        let (realized, seen_trades, has_trade_log) = match self.trade_db.as_ref() {
            Some(db) => {
                let s = db.summary_for(active_id.as_str());
                (f64_to_decimal(s.total_net_pnl), s.total_trades, true)
            }
            None => (Decimal::ZERO, 0, false),
        };
        let mut spent = Decimal::ZERO;
        let mut received = Decimal::ZERO;
        for p in self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
        {
            spent += p.flows.entry_cost_usd + p.flows.entry_fee_usd;
            received += p.flows.proceeds_usd - p.flows.exit_fee_usd;
            // The modeled maker rebate the ledger already credited rides the
            // position's RECEIVED side until the close folds it into the trade
            // record's net — the identity must cover the cash at every
            // instant, not only after the position settles.
            received += p.flows.maker_rebate_usd;
        }
        let open_positions = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == active_id)
            .count();
        // E28: the audit's cash identity describes the ACTIVE account's book
        // (§9.5). Cross-account isolation is asserted by the dedicated gate
        // (account-cross-isolation-check.mjs), not by widening this identity.
        let active = self.accounts.active_ledger();
        // The venue report comparable to THIS book is the report of the venue
        // the active account is BOUND to (#505): a sibling venue's cash is a
        // different book's truth, and comparing across books once produced a
        // phantom `venue_free_cash` drift of exactly (other venue's cash −
        // this book's balance).
        let active_venue = self.accounts.active_venue().to_string();
        let venue_free = self
            .freeze_book
            .venue_free_by_venue
            .get(&active_venue)
            .map(|(t, b)| (*t, *b));
        let ledger_realigned = self.audit.last_realign.get(&active_venue).copied();
        // The venue leg compares the ledger against a PAPER site's own figure
        // when this venue declared itself SIMULATED (its plugin's declaration
        // snapshot — the same `set_plugin_modes` injection the strategy
        // handshake reads): the paper site freezes its reported free cash on a
        // resting BUY and never nets the reservation back out, so the leg's
        // `balance − reserved` expectation is fee-level cents off by
        // construction and two strikes halted new entries over arithmetic that
        // could never agree (external operator report, issue #669). The
        // declaration is the plugin's identity — it cannot change mid-session.
        let venue_simulated = self
            .plugin_modes
            .iter()
            .find(|p| p.name == active_venue)
            .map(|p| {
                p.modes.iter().any(|m| {
                    m.capabilities
                        .satisfies(blitzkrieg_market_api::MarketCapabilities::SIMULATED)
                })
            })
            .unwrap_or(false);
        AuditInput {
            now_ms,
            live: self.config.mode == Mode::Live,
            identity: CashIdentity {
                balance: active.balance(),
                // Settled but not yet redeemed: owed to US — this book's us,
                // not the process's (each claim carries the account it was
                // booked from, E28 §9.2).
                receivable: self.settlement.receivable_usd_for(active_id.as_str())
                    + self.settlement.merge_receivable_usd_for(active_id.as_str()),
                realized,
                spent,
                received,
            },
            reserved: active.reserved(),
            ledger_balanced: active.is_balanced(),
            unfunded_reserved: active.unfunded_reserved(),
            open_buy_notional,
            open_buys,
            stray_reservations,
            venue_free: venue_free.map(|(_, b)| b),
            venue_free_at_ms: venue_free.map(|(t, _)| t),
            ledger_realigned,
            open_positions,
            seen_trades,
            has_trade_log,
            venue_simulated,
        }
    }

    /// Run the accounting audit now and act on its verdict: alert, persist, and
    /// block/halt new entries. Returns the report (also kept for the panel).
    ///
    /// Called from `tick` on `audit_interval_sec`, and directly by tests.
    pub fn run_accounting_audit(&mut self, now_ms: i64) -> AuditReport {
        let input = self.build_audit_input(now_ms);
        let report = crate::reconcile::audit_accounting(&input, self.audit.anchor.as_ref());

        // Anchor: taken at the first audit the STRUCTURAL checks pass, then never
        // moved while anything is failing — re-anchoring on a failure would hide
        // exactly the movement the identity exists to expose.
        if self.audit.anchor.is_none() && !report.structural_failure {
            self.audit.anchor = Some(input.identity);
        }
        self.audit.runs += 1;
        if report.ok {
            self.audit.cross_source_failures = 0;
        } else {
            self.audit.failures += 1;
            if report.structural_failure {
                // Local inconsistency: no fetch race to forgive.
                self.audit.cross_source_failures = 0;
            } else {
                self.audit.cross_source_failures += 1;
            }
        }
        let cross_strike = self.audit.cross_source_failures >= 2;
        // A halt is warranted by a structural failure immediately, by a venue
        // mismatch only after it repeats (a single stale read must not stop
        // entries), and only when the operator left the brake enabled.
        let should_halt = !report.ok
            && (report.structural_failure || cross_strike)
            && self.config.audit_halt_entries;
        let was_halted = self.audit.halted;
        let mut resumed = false;

        if should_halt && !was_halted {
            self.audit.halted = true;
            self.audit.halt_reason = report.note.clone();
            let message = format!(
                "accounting audit FAILED — new entries blocked until it passes: {}",
                report.note
            );
            // The reason rides the structured channels (tracing / error slot /
            // RiskAlert); the stream write stays a static literal — kernel
            // text reaching the stream sink is the cleartext-logging shape
            // CodeQL flags (#693 family).
            tracing::error!(drift = %report.drift_usd, "{}", message);
            eprintln!("core: accounting audit FAILED — see the error log line above");
            // The halt is an error of the highest order (entries are blocked), so
            // it also lands in the one panel-visible slot (#180).
            self.note_error(
                &CoreError::new(CoreErrorCode::Internal, message.clone()),
                now_ms,
            );
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if should_halt {
            self.audit.halt_reason = report.note.clone();
        } else if was_halted {
            self.audit.halted = false;
            self.audit.halt_reason.clear();
            resumed = true;
            let message = format!(
                "accounting audit recovered — new entries resumed ({})",
                report.note
            );
            tracing::info!("{message}");
            eprintln!("core: accounting audit recovered — see the info log line above");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if !report.ok {
            // Failing but not (yet) halting: still loud, still persisted.
            let message = format!("accounting audit drift: {}", report.note);
            tracing::warn!(drift = %report.drift_usd, "{}", message);
            eprintln!("core: accounting audit drift — see the warn log line above");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        }

        // Persist on every failure, every halt/resume, and otherwise as a
        // heartbeat — a healthy audit is the evidence that the check RAN. The
        // very first audit is written too: a fresh process must be able to prove
        // it checked before anything went wrong.
        let heartbeat = self.audit.last_heartbeat_ms == 0
            || now_ms - self.audit.last_heartbeat_ms >= AUDIT_HEARTBEAT_MS;
        if !report.ok || should_halt || was_halted || resumed || heartbeat {
            self.audit.last_heartbeat_ms = now_ms;
            self.persist_audit_report(&report);
        }
        self.audit.report = Some(report.clone());
        self.audit.last_at_ms = now_ms;
        report
    }

    /// The latest audit verdict, for the panel (and for a caller that wants the
    /// report without running one).
    pub fn accounting_audit(&self) -> Option<&AuditReport> {
        self.audit.report.as_ref()
    }

    /// New entries are blocked by a failing accounting audit (issue #189).
    pub fn audit_blocks_entries(&self) -> bool {
        self.audit.halted
    }

    /// Why new entries are blocked (empty when they are not).
    pub fn audit_halt_reason(&self) -> &str {
        &self.audit.halt_reason
    }

    /// The gate itself: refuses a NEW entry while the audit is failing. Exits
    /// (SELL) never consult this — a drifted ledger must not be able to trap the
    /// bot in a position, which is what routing this through the kill switch did
    /// (#174).
    pub(crate) fn audit_entry_gate(&self) -> CoreResult<()> {
        if self.audit.halted {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "accounting audit halted new entries: {}",
                    self.audit.halt_reason
                ),
            ));
        }
        Ok(())
    }

    /// Mutable ledger access, for tests that must put the books into the exact
    /// state a bug leaves behind (a stranded reservation, cash that moved with no
    /// trade) and assert the audit NOTICES it. Gated behind the `test-support`
    /// feature the crate enables for its own test targets, so no production build
    /// can reach it.
    #[cfg(feature = "test-support")]
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        // E28: tests poking the books target the ACTIVE account's ledger —
        // the same default a 0.2 deployment has exactly one of.
        self.accounts.active_ledger_mut()
    }

    /// Append one audit verdict (or a late-fill record) to the audit log.
    pub(crate) fn persist_audit_value(&self, value: serde_json::Value) {
        let Some(path) = self.audit_log_path() else {
            return;
        };
        crate::jsonl::append(std::path::Path::new(path), &value);
    }

    pub(super) fn persist_audit_report(&self, report: &AuditReport) {
        match serde_json::to_value(report) {
            Ok(v) => self.persist_audit_value(v),
            Err(e) => tracing::warn!(error = %e, "cannot serialize audit report"),
        }
    }

    pub(crate) fn audit_log_path(&self) -> Option<&str> {
        self.audit.audit_log_path.as_deref()
    }
}
