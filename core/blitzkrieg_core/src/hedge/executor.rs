//! The two-leg execution state machine (issue #426, items ①–④).
//!
//! One [`super::plan::HedgePlan`] armed → both legs submitted → the machine
//! drives itself on EXPLICIT events (`leg_acked`, `leg_filled`,
//! `leg_failed`) against an injected wall clock, so the fault matrix is
//! fully reproducible in tests without a real venue.
//!
//! States:
//! ```text
//! Submitting ── both acked ──→ Live
//!     │ │                          │
//!     │ └─ counter failed ─→ Protecting   (one filled, one failed/naked)
//!     │                          │
//!     ├─ both failed → BothFailedClean   (nothing left anywhere)
//!     │                          ├─ re-hedge fills → BothFilled (re-hedged)
//!     └─ both filled → BothFilled       └─ unwind fills  → Unwound
//! ```
//!
//! Two deadlines drive the machine, both issue-named constants re-exported
//! from `super`:
//! - `SUBMIT_DEADLINE_MS` (500 ms): an un-acked leg past this is FAILED —
//!   treat a venue that did not answer as one that said no.
//! - `NAKED_LEG_DEADLINE_MS` (2 s): a filled leg with no counter-leg
//!   exposure past this triggers the protective action — re-hedge the
//!   shortfall on the counter venue, or force-unwind the filled leg.
//!
//! Market-agnostic (issue #427): the machine branches on leg STATES only.
//! `Venue` rides on the leg data for submission/audit naming, never in a
//! decision path.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::audit::{HedgeAuditRecord, HedgeAuditSink};
use super::{
    FORCE_UNWIND_AFTER_REFUSALS, NAKED_LEG_DEADLINE_MS, ProtectiveAction, ProtectiveKind,
    SUBMIT_DEADLINE_MS,
};

/// Which side of the event a leg takes. Two legs hedge only when they are
/// opposite; the state machine refuses same-side arming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegSide {
    Up,
    Down,
}

/// Identity of one leg inside an execution: the venue it trades on and the
/// venue-side token. Venue is data, not logic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HedgeLegId {
    /// `blitzkrieg_market_api::unified::Venue` as a string: keeps this
    /// module's wire shape stable while the enum lives in market_api.
    pub venue: String,
    pub token_id: String,
}

impl HedgeLegId {
    pub fn new(venue: blitzkrieg_market_api::unified::Venue, token_id: impl Into<String>) -> Self {
        Self {
            venue: venue.as_str().to_string(),
            token_id: token_id.into(),
        }
    }
}

impl std::fmt::Display for HedgeLegId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.venue, self.token_id)
    }
}

/// Per-leg state through the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegState {
    /// Armed, awaiting venue ack.
    Pending,
    /// Venue accepted the order; awaiting fill or fail.
    Submitted,
    /// Filled (the size that filled rides on the leg report, not here).
    Filled,
    /// Rejected, errored, or ack timed out — this leg is dead.
    Failed,
    /// Cancelled by our own protective action (never counted as `Failed`:
    /// the leg DID trade, the machine knows what it holds).
    Cancelled,
}

/// One leg's position as the machine sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct LegReport {
    pub id: HedgeLegId,
    pub side: LegSide,
    pub state: LegState,
    pub filled: Decimal,
    /// Price the fill(s) landed at, when filled.
    pub fill_price: Option<Decimal>,
}

impl LegReport {
    fn new(id: HedgeLegId, side: LegSide) -> Self {
        Self {
            id,
            side,
            state: LegState::Pending,
            filled: Decimal::ZERO,
            fill_price: None,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self.state, LegState::Pending | LegState::Submitted)
    }

    pub fn has_exposure(&self) -> bool {
        self.filled > Decimal::ZERO
    }
}

/// Whole-execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HedgeExecutionState {
    /// Armed, waiting for acks/fails.
    Submitting,
    /// Both legs acked, waiting for fills.
    Live,
    /// One leg holds exposure alone; the protection clock is running.
    Protecting,
    /// Terminal — see [`super::HedgeOutcome`].
    Done,
}

/// The protective decisions the machine can DEMAND from its executor
/// (the venue-facing side). The machine decides WHEN and WHAT; the
/// injected `ExecutorHooks` carry the HOW — so the mock fault matrix
/// exercises the exact machine production runs.
pub enum ProtectiveDemand {
    /// Place a marketable order for `shortfall` shares on the NAKED leg's
    /// side (recover the missing exposure on the counter venue).
    ReHedge { leg: HedgeLegId, shortfall: Decimal },
    /// Flatten the filled leg entirely (its exposure is all naked).
    ForceUnwind { leg: HedgeLegId, size: Decimal },
}

impl std::fmt::Debug for ProtectiveDemand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtectiveDemand::ReHedge { leg, shortfall } => {
                write!(f, "ReHedge({leg}, shortfall={shortfall})")
            }
            ProtectiveDemand::ForceUnwind { leg, size } => {
                write!(f, "ForceUnwind({leg}, size={size})")
            }
        }
    }
}

impl Clone for ProtectiveDemand {
    fn clone(&self) -> Self {
        match self {
            ProtectiveDemand::ReHedge { leg, shortfall } => ProtectiveDemand::ReHedge {
                leg: leg.clone(),
                shortfall: *shortfall,
            },
            ProtectiveDemand::ForceUnwind { leg, size } => ProtectiveDemand::ForceUnwind {
                leg: leg.clone(),
                size: *size,
            },
        }
    }
}

/// Venue-facing hooks the machine calls. The real wiring submits through
/// the order path; tests drive a mock venue. All hooks are `&mut self` so
/// a mock can mutate its fault script.
pub trait ExecutorHooks {
    /// Submit one leg. Return `Ok(())` = accepted (acked) — the machine then
    /// waits for `leg_filled`/`leg_failed` events. `Err` = rejected.
    fn submit_leg(&mut self, leg: &HedgeLegId, side: LegSide, price: Decimal, size: Decimal);
    /// Execute the protective demand (re-hedge or unwind). `Ok` = the order
    /// was placed; its outcome arrives through the normal leg events.
    fn execute_protective(
        &mut self,
        action: &ProtectiveDemand,
        at_ms: i64,
    ) -> Option<ProtectiveAction>;
    /// Cancel a still-live leg (used when the counter leg's failure strands
    /// an UNFILLED live leg — never leave an order nobody wants).
    fn cancel_leg(&mut self, leg: &HedgeLegId);
}

/// One execution of one plan.
pub struct HedgeExecution {
    pub execution_id: String,
    pub event_id: String,
    pub legs: [LegReport; 2],
    pub state: HedgeExecutionState,
    /// Set on `Done`.
    pub outcome: Option<super::HedgeOutcome>,
    /// When the plan was armed.
    armed_at_ms: i64,
    /// When the first leg reached `Filled` (naked-clock start), if any.
    naked_since_ms: Option<i64>,
    /// Which leg is naked (the SIZE-gap side) while protecting.
    naked_leg: Option<usize>,
    /// The gap size the clock was armed against; a change restarts it.
    naked_gap: Option<Decimal>,
    /// Every protective action taken, in order — audited verbatim.
    pub protective_actions: Vec<ProtectiveAction>,
    /// How many times the protective demand was REFUSED by the hooks (venue
    /// unreachable, order refused). A refusal is itself audited — a naked
    /// leg whose protection silently no-ops is exactly the #426 horror.
    pub protective_refusals: u64,
    /// The demand currently in flight: (kind, gap it targets). Cleared when
    /// the gap closes or changes — prevents re-demanding the same gap every
    /// past-deadline tick (the over-order bug).
    protective_in_flight: Option<(ProtectiveKind, Decimal)>,
}

impl HedgeExecution {
    /// Arm a plan: legs go `Pending`, state `Submitting`. Same-side plans
    /// are refused — a hedge needs two OPPOSITE legs.
    pub fn arm(
        execution_id: impl Into<String>,
        event_id: impl Into<String>,
        legs: [(HedgeLegId, LegSide); 2],
        at_ms: i64,
    ) -> Result<Self, String> {
        if legs[0].1 == legs[1].1 {
            return Err("both legs are the same side — nothing to hedge".into());
        }
        Ok(Self {
            execution_id: execution_id.into(),
            event_id: event_id.into(),
            legs: [
                LegReport::new(legs[0].0.clone(), legs[0].1),
                LegReport::new(legs[1].0.clone(), legs[1].1),
            ],
            state: HedgeExecutionState::Submitting,
            outcome: None,
            armed_at_ms: at_ms,
            naked_since_ms: None,
            naked_leg: None,
            naked_gap: None,
            protective_actions: Vec::new(),
            protective_refusals: 0,
            protective_in_flight: None,
        })
    }

    /// A leg was accepted by its venue.
    pub fn leg_acked(&mut self, idx: usize) {
        if self.state != HedgeExecutionState::Submitting {
            return;
        }
        if matches!(self.legs[idx].state, LegState::Pending) {
            self.legs[idx].state = LegState::Submitted;
        }
    }

    /// A leg reported a fill. `size` is the CUMULATIVE filled size on that
    /// leg (the OME/reporter side owns the accumulation); the machine keeps
    /// the max — fills only ever grow.
    ///
    /// A fill is an EXPOSURE FACT, not an order event: it is accepted on a
    /// leg in ANY state. The re-hedge rail reuses the counter leg even when
    /// its original order was rejected — the venue took a NEW order, and
    /// the exposure it produced is real. (Only the unwind rail uses
    /// `leg_reduced`, which keeps its guard.)
    pub fn leg_filled(&mut self, idx: usize, size: Decimal, price: Decimal, at_ms: i64) {
        self.legs[idx].state = LegState::Filled;
        if size > self.legs[idx].filled {
            self.legs[idx].filled = size;
        }
        self.legs[idx].fill_price = Some(price);
        self.refresh_state(at_ms);
    }

    /// A leg reports its exposure was REDUCED (the force-unwind path reports
    /// through the same rail): cumulative size shrinks toward zero.
    pub fn leg_reduced(&mut self, idx: usize, remaining: Decimal, price: Decimal, at_ms: i64) {
        if matches!(self.legs[idx].state, LegState::Failed) {
            return;
        }
        self.legs[idx].filled = remaining;
        if !remaining.is_zero() {
            self.legs[idx].state = LegState::Filled;
            self.legs[idx].fill_price = Some(price);
        }
        self.refresh_state(at_ms);
    }

    /// A leg failed (rejected / errored / dead). Its live order, if any, is
    /// over — no exposure came from it.
    pub fn leg_failed(&mut self, idx: usize, at_ms: i64) {
        self.legs[idx].state = LegState::Failed;
        if self.legs[idx].filled.is_zero() {
            self.legs[idx].fill_price = None;
        }
        self.refresh_state(at_ms);
    }

    /// Tick the clock: enforce the submit deadline and the naked-leg
    /// deadline, returning every protective action TAKEN this tick (already
    /// applied to state). Call this from the pump loop; the machine never
    /// sleeps on its own.
    pub fn tick(&mut self, now_ms: i64, hooks: &mut dyn ExecutorHooks, audit: &HedgeAuditSink) {
        // 500 ms: an un-acked leg is a failed leg. A venue that did not
        // answer in the deadline is treated exactly like one that said no.
        // The deadline is PER-LEG (armed_at + 500), not gated on the whole
        // execution still being Submitting: one leg can fill while the
        // other is still silent, and the silent one must still time out.
        if now_ms - self.armed_at_ms >= SUBMIT_DEADLINE_MS {
            for idx in 0..2 {
                if matches!(self.legs[idx].state, LegState::Pending) {
                    self.legs[idx].state = LegState::Failed;
                    audit.record(&HedgeAuditRecord {
                        ts_ms: now_ms,
                        execution_id: self.execution_id.clone(),
                        event_id: self.event_id.clone(),
                        action: "submit_timeout",
                        leg: Some(self.legs[idx].id.clone()),
                        side: Some(self.legs[idx].side),
                        protective: None,
                        detail: Some(format!(
                            "no ack within {SUBMIT_DEADLINE_MS}ms — treating as failed"
                        )),
                    });
                }
            }
            self.refresh_state(now_ms);
        }

        // 2 s: one leg holding exposure alone past the deadline demands its
        // protective action NOW.
        //
        // The ladder: re-hedge FIRST — a rejected ORDER does not make the
        // counter VENUE dead, and recovering the exposure is the good end;
        // force-unwind only after repeated refusals prove the counter venue
        // cannot take the missing side (burning fees on both ends is the
        // admitted loss, so it is the LAST resort).
        //
        // A taken demand is IN FLIGHT until the gap it targets changes or
        // closes: re-demanding the same gap every tick would place the same
        // re-hedge order twice — the over-order bug.
        let Some(naked) = self.naked_leg else {
            return;
        };
        let Some(since) = self.naked_since_ms else {
            return;
        };
        if now_ms - since < NAKED_LEG_DEADLINE_MS || self.state != HedgeExecutionState::Protecting {
            return;
        }
        // Defensive default: sizes can only have changed through
        // refresh_state, which re-arms the gap. Zero means "call was
        // spurious".
        let gap = self.naked_gap.unwrap_or(Decimal::ZERO);
        if gap.is_zero() {
            return;
        }
        // In-flight guard: a demand for THIS gap is already out.
        if self
            .protective_in_flight
            .is_some_and(|(_, in_flight_gap)| in_flight_gap == gap)
        {
            return;
        }
        let counter = 1 - naked;
        let (demand_kind, demand_detail) =
            if self.protective_refusals >= FORCE_UNWIND_AFTER_REFUSALS {
                // The counter venue repeatedly refused the re-hedge: nobody
                // will take the other side — force-unwind the naked leg.
                (ProtectiveKind::ForceUnwind, format!("force-unwind {gap}"))
            } else {
                (
                    ProtectiveKind::ReHedge,
                    format!("re-hedge {gap} on counter venue"),
                )
            };
        let demand = match demand_kind {
            ProtectiveKind::ReHedge => ProtectiveDemand::ReHedge {
                leg: self.legs[counter].id.clone(),
                shortfall: gap,
            },
            ProtectiveKind::ForceUnwind => ProtectiveDemand::ForceUnwind {
                leg: self.legs[naked].id.clone(),
                size: gap,
            },
        };
        let leg_id = self.legs[naked].id.clone();
        let side = self.legs[naked].side;
        let demand_detail = Some(demand_detail);
        if let Some(action) = hooks.execute_protective(&demand, now_ms) {
            self.protective_actions.push(action.clone());
            // The protective order goes through the same leg channel: mark
            // the COUNTER leg as submitted (re-hedge) or the naked leg as
            // cancelling (unwind) so outcomes land on the right rail.
            match action.kind {
                ProtectiveKind::ReHedge => {
                    self.legs[counter].state = LegState::Submitted;
                }
                ProtectiveKind::ForceUnwind => {
                    self.legs[naked].state = LegState::Cancelled;
                }
            }
            // In flight until the gap closes or changes.
            self.protective_in_flight = Some((action.kind, gap));
            audit.record(&HedgeAuditRecord {
                ts_ms: now_ms,
                execution_id: self.execution_id.clone(),
                event_id: self.event_id.clone(),
                action: "protective_action",
                leg: Some(leg_id),
                side: Some(side),
                protective: Some(action.kind),
                detail: demand_detail,
            });
            self.refresh_state(now_ms);
        } else {
            // The hooks REFUSED the demand. The refusal is itself audited
            // and counted: a naked leg whose protection silently no-ops is
            // exactly the #426 horror — the next tick retries, and every
            // retry leaves a line.
            self.protective_refusals += 1;
            audit.record(&HedgeAuditRecord {
                ts_ms: now_ms,
                execution_id: self.execution_id.clone(),
                event_id: self.event_id.clone(),
                action: "protective_refused",
                leg: Some(leg_id),
                side: Some(side),
                protective: Some(demand_kind),
                detail: demand_detail,
            });
        }
    }

    /// Refresh the machine state from leg states.
    ///
    /// Nakedness is a SIZE fact, not a boolean: legs hedge when their
    /// FILLED SIZES are equal — a counter leg that filled 4 against a 10
    /// leaves a 6-share gap, and the gap, not a state label, is what the
    /// protection clock tracks. Terminal outcomes read the same fact:
    /// equal sizes = whole (BothFilled / ReHedged), zeroed naked leg =
    /// Unwound, both zero = clean failure.
    fn refresh_state(&mut self, now_ms: i64) {
        if self.state == HedgeExecutionState::Done {
            return;
        }
        let (sa, sb) = (self.legs[0].filled, self.legs[1].filled);
        let (a_live, b_live) = (self.legs[0].is_live(), self.legs[1].is_live());
        let rehedge_pending = self
            .protective_actions
            .iter()
            .any(|p| p.kind == ProtectiveKind::ReHedge);
        let unwind_pending = self
            .protective_actions
            .iter()
            .any(|p| p.kind == ProtectiveKind::ForceUnwind);

        // Balanced: the two legs hold equal size — whole, however it got
        // there. The re-hedge path labels the outcome honestly.
        if sa == sb {
            if sa > Decimal::ZERO {
                self.finish(if rehedge_pending {
                    super::HedgeOutcome::ReHedged
                } else {
                    super::HedgeOutcome::BothFilled
                });
            } else if !a_live && !b_live {
                // Zero on both with NO live leg anywhere: nothing can still
                // arrive. The unwind label applies when fills existed and
                // were flattened; otherwise this is the clean both-failed
                // exit.
                if unwind_pending {
                    self.finish(super::HedgeOutcome::Unwound);
                } else {
                    self.finish(super::HedgeOutcome::BothFailedClean);
                }
            }
            self.naked_leg = None;
            self.naked_since_ms = None;
            self.naked_gap = None;
            self.protective_in_flight = None;
            return;
        }

        // Size gap: the bigger leg is naked by `gap`. Which leg is "the
        // naked one" follows the sizes; a gap appearing or CHANGING restarts
        // the protection clock (a new quote is a new situation) and retires
        // any in-flight protective order aimed at the OLD gap.
        let naked = if sa > sb { 0 } else { 1 };
        let gap = if sa > sb { sa - sb } else { sb - sa };
        let naked_changed =
            self.naked_leg != Some(naked) || self.naked_gap.map(|g| g != gap).unwrap_or(true);
        if naked_changed {
            self.naked_leg = Some(naked);
            self.naked_gap = Some(gap);
            self.naked_since_ms = Some(now_ms);
            self.protective_in_flight = None;
        }
        self.state = HedgeExecutionState::Protecting;
    }

    fn finish(&mut self, outcome: super::HedgeOutcome) {
        self.state = HedgeExecutionState::Done;
        self.outcome = Some(outcome);
        self.naked_leg = None;
        self.naked_gap = None;
        self.naked_since_ms = None;
    }

    /// Which leg currently holds the size gap (the naked one), for wiring
    /// and tests: fills for a taken re-hedge land on the OTHER leg.
    pub fn naked_leg_index(&self) -> Option<usize> {
        self.naked_leg
    }

    /// The audit line for the arming (proposed) event.
    pub fn audit_armed(&self, at_ms: i64, audit: &HedgeAuditSink) {
        audit.record(&HedgeAuditRecord {
            ts_ms: at_ms,
            execution_id: self.execution_id.clone(),
            event_id: self.event_id.clone(),
            action: "proposed",
            leg: None,
            side: None,
            protective: None,
            detail: Some(format!("armed {} vs {}", self.legs[0].id, self.legs[1].id)),
        });
    }

    /// The audit line for the terminal outcome.
    pub fn audit_done(&self, at_ms: i64, audit: &HedgeAuditSink) {
        let Some(outcome) = self.outcome else {
            return;
        };
        audit.record(&HedgeAuditRecord {
            ts_ms: at_ms,
            execution_id: self.execution_id.clone(),
            event_id: self.event_id.clone(),
            action: outcome.as_str(),
            leg: None,
            side: None,
            protective: None,
            detail: Some(format!(
                "legs: {} ({} {:?} filled={}), {} ({} {:?} filled={}); {} protective action(s)",
                self.legs[0].id,
                self.legs[0].state_str(),
                self.legs[0].side,
                self.legs[0].filled,
                self.legs[1].id,
                self.legs[1].state_str(),
                self.legs[1].side,
                self.legs[1].filled,
                self.protective_actions.len(),
            )),
        });
    }
}

impl LegReport {
    /// Stable snake_case name for the audit line (state enum is serde-
    /// spelled the same way).
    pub fn state_str(&self) -> &'static str {
        match self.state {
            LegState::Pending => "pending",
            LegState::Submitted => "submitted",
            LegState::Filled => "filled",
            LegState::Failed => "failed",
            LegState::Cancelled => "cancelled",
        }
    }
}
