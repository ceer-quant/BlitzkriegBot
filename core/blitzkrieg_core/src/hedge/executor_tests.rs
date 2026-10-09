//! Unit tests for the two-leg state machine — the fault matrix lives here
//! first (issue #426's mock-venue matrix is the integration-level twin).
//!
//! Every test drives the SAME machine production runs: arm → events →
//! tick → inspect outcome. The mock venue is a fault script, not a stub
//! that skips the machine.

use super::*;
use crate::hedge::ProtectiveKind;
use crate::hedge::audit::HedgeAuditSink;
use crate::hedge::executor::ProtectiveDemand;
use crate::hedge::executor::{ExecutorHooks, HedgeExecution, HedgeLegId, LegSide};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A scripted venue: what each submit/protective/cancel call does is queued
/// ahead of time. Recorded calls prove the machine DEMANDED them.
#[derive(Default)]
struct MockVenue {
    /// What `execute_protective` does: Some(action) = place it, None = refuse.
    protective_result: Option<ProtectiveKind>,
    protective_calls: usize,
    cancel_calls: usize,
    submit_calls: usize,
}

impl ExecutorHooks for MockVenue {
    fn submit_leg(&mut self, _leg: &HedgeLegId, _side: LegSide, _price: Decimal, _size: Decimal) {
        self.submit_calls += 1;
    }
    fn execute_protective(
        &mut self,
        _action: &ProtectiveDemand,
        at_ms: i64,
    ) -> Option<crate::hedge::ProtectiveAction> {
        self.protective_calls += 1;
        self.protective_result
            .map(|kind| crate::hedge::ProtectiveAction {
                leg: HedgeLegId::new(
                    blitzkrieg_market_api::unified::Venue::Polymarket,
                    "protective",
                ),
                kind,
                at_ms,
            })
    }
    fn cancel_leg(&mut self, _leg: &HedgeLegId) {
        self.cancel_calls += 1;
    }
}

fn silent_audit() -> HedgeAuditSink {
    HedgeAuditSink::at(None)
}

fn ids() -> [HedgeLegId; 2] {
    use blitzkrieg_market_api::unified::Venue;
    [
        HedgeLegId::new(Venue::Polymarket, "pm-token"),
        HedgeLegId::new(Venue::Kalshi, "kx-token"),
    ]
}

fn arm(at_ms: i64) -> HedgeExecution {
    let [a, b] = ids();
    HedgeExecution::arm(
        "exec-1",
        "event-1",
        [(a, LegSide::Up), (b, LegSide::Down)],
        at_ms,
    )
    .expect("two opposite legs arm cleanly")
}

#[test]
fn happy_path_both_fill_no_protection() {
    let mut ex = arm(1_000);
    let mut venue = MockVenue::default();
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    // Before 2 s: no protective demand even though one leg is naked.
    ex.tick(1_100 + 1_999, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 0);
    // Counter fills within the deadline → BothFilled, zero protection.
    ex.leg_filled(1, dec!(10), dec!(0.38), 1_500);
    assert_eq!(ex.state, HedgeExecutionState::Done);
    assert_eq!(ex.outcome, Some(crate::hedge::HedgeOutcome::BothFilled));
    assert!(ex.protective_actions.is_empty());
}

#[test]
fn leg_filled_alone_past_2s_with_dead_counter_rehedges() {
    // One fills; the counter FAILED (venue said no). Past the naked deadline
    // the machine must demand a re-hedge on the counter venue.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ReHedge),
        ..Default::default()
    };
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    assert_eq!(ex.state, HedgeExecutionState::Protecting);

    // Just before the deadline: nothing.
    ex.tick(1_100 + 1_999, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 0);

    // At the deadline: re-hedge demanded and taken.
    ex.tick(1_100 + 2_000, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 1);
    assert_eq!(ex.protective_actions.len(), 1);
    assert_eq!(ex.protective_actions[0].kind, ProtectiveKind::ReHedge);

    // The re-hedge fills the shortfall on the counter leg → whole again.
    ex.leg_filled(1, dec!(10), dec!(0.38), 1_100 + 2_100);
    assert_eq!(ex.outcome, Some(crate::hedge::HedgeOutcome::ReHedged));
}

#[test]
fn leg_filled_alone_with_live_but_unfilled_counter_rehedges() {
    // Counter leg is acked but never fills: past the deadline the exposure
    // is still naked — re-hedge fires even though the order is live.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ReHedge),
        ..Default::default()
    };
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    ex.tick(1_100 + 2_000, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 1);
    assert_eq!(ex.protective_actions[0].kind, ProtectiveKind::ReHedge);
}

#[test]
fn protective_unwind_when_counter_venue_cannot_take() {
    // The machine prefers re-hedge only when the counter side can hold
    // exposure. Here the counter leg failed AND the protective hook reports
    // the venue cannot take it — the demand becomes force-unwind.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ForceUnwind),
        ..Default::default()
    };
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);

    // Drive the tick far past the deadline; the protective demand fires.
    ex.tick(1_100 + 2_500, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 1);

    // The unwind reduces the naked leg's exposure to zero (reported through
    // the same rail as any fill, via `leg_reduced`).
    ex.leg_reduced(0, Decimal::ZERO, dec!(0.55), 1_100 + 2_600);
    assert_eq!(ex.outcome, Some(crate::hedge::HedgeOutcome::Unwound));
}

#[test]
fn both_fail_within_submit_window_is_clean() {
    let mut ex = arm(1_000);
    let venue = MockVenue::default();
    let _audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_failed(0, 1_050);
    ex.leg_failed(1, 1_060);
    assert_eq!(ex.state, HedgeExecutionState::Done);
    assert_eq!(
        ex.outcome,
        Some(crate::hedge::HedgeOutcome::BothFailedClean)
    );
    assert!(ex.protective_actions.is_empty());
    assert_eq!(venue.protective_calls, 0);
}

#[test]
fn submit_timeout_treats_silent_venue_as_failed() {
    // 500 ms with no ack = failed; if BOTH legs are silent the execution is
    // a clean failure (nothing was left anywhere).
    let mut ex = arm(1_000);
    let mut venue = MockVenue::default();
    let audit = silent_audit();

    ex.tick(1_000 + 499, &mut venue, &audit);
    assert_eq!(ex.state, HedgeExecutionState::Submitting);

    ex.tick(1_000 + 500, &mut venue, &audit);
    assert_eq!(ex.state, HedgeExecutionState::Done);
    assert_eq!(
        ex.outcome,
        Some(crate::hedge::HedgeOutcome::BothFailedClean)
    );
    assert!(matches!(ex.legs[0].state, LegState::Failed));
    assert!(matches!(ex.legs[1].state, LegState::Failed));
}

#[test]
fn submit_timeout_one_silent_one_filled_protects() {
    // One leg silent past 500 ms, the other filled: the silent one is
    // failed, the fill is naked — protection must fire.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ReHedge),
        ..Default::default()
    };
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    // 600 ms: the ack deadline already passed for leg 1.
    ex.tick(1_600, &mut venue, &audit);
    assert!(matches!(ex.legs[1].state, LegState::Failed));
    // Naked deadline starts at the fill (1_100): 1_600 < 3_100, nothing yet.
    assert_eq!(venue.protective_calls, 0);
    ex.tick(3_100, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 1);
}

#[test]
fn same_side_plan_is_refused() {
    let [a, b] = ids();
    let err = HedgeExecution::arm(
        "exec-2",
        "event-1",
        [(a, LegSide::Up), (b, LegSide::Up)],
        1_000,
    );
    assert!(
        err.is_err(),
        "same-side arming must refuse — nothing to hedge"
    );
}

#[test]
fn protective_refusal_keeps_protection_clock_running() {
    // If the protective hook REFUSES (venue unreachable), the machine stays
    // Protecting and retries on later ticks — a refused demand is not a
    // finished protection.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: None, // refuses everything
        ..Default::default()
    };
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);

    ex.tick(3_500, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 1);
    assert_eq!(ex.state, HedgeExecutionState::Protecting);
    assert!(ex.protective_actions.is_empty());

    ex.tick(4_500, &mut venue, &audit);
    assert_eq!(venue.protective_calls, 2);
    assert_eq!(ex.state, HedgeExecutionState::Protecting);
}

#[test]
fn partial_counter_fill_rehedges_only_shortfall() {
    // Leg 0 fills 10, counter fills 4: the shortfall is 6, and the audit
    // detail names it. The demand must be the SHORTFALL, not the whole size.
    let mut ex = arm(1_000);
    let mut demand_seen: Option<ProtectiveDemand> = None;
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ReHedge),
        ..Default::default()
    };
    struct DemandCapture<'a> {
        inner: &'a mut MockVenue,
        seen: &'a mut Option<ProtectiveDemand>,
    }
    impl ExecutorHooks for DemandCapture<'_> {
        fn submit_leg(&mut self, l: &HedgeLegId, s: LegSide, p: Decimal, sz: Decimal) {
            self.inner.submit_leg(l, s, p, sz);
        }
        fn execute_protective(
            &mut self,
            action: &ProtectiveDemand,
            at_ms: i64,
        ) -> Option<crate::hedge::ProtectiveAction> {
            *self.seen = Some(action.clone());
            self.inner.execute_protective(action, at_ms)
        }
        fn cancel_leg(&mut self, l: &HedgeLegId) {
            self.inner.cancel_leg(l);
        }
    }
    let audit = silent_audit();

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    ex.leg_filled(1, dec!(4), dec!(0.38), 1_200);

    let mut hooks = DemandCapture {
        inner: &mut venue,
        seen: &mut demand_seen,
    };
    ex.tick(1_200 + 2_000, &mut hooks, &audit);
    match demand_seen {
        Some(ProtectiveDemand::ReHedge { shortfall, .. }) => assert_eq!(shortfall, dec!(6)),
        other => panic!("expected ReHedge demand for shortfall 6, got {other:?}"),
    }
}

#[test]
fn audit_disabled_sink_writes_nothing_and_never_panics() {
    // The None-path sink: the machine runs identically with zero disk.
    let mut ex = arm(1_000);
    let mut venue = MockVenue {
        protective_result: Some(ProtectiveKind::ForceUnwind),
        ..Default::default()
    };
    let audit = silent_audit();
    ex.audit_armed(1_000, &audit);
    ex.leg_acked(0);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    ex.tick(3_100, &mut venue, &audit);
    assert_eq!(ex.protective_actions.len(), 1);
    // The unwind's fill is reported back through the same rail as any fill.
    ex.leg_reduced(0, Decimal::ZERO, dec!(0.55), 3_200);
    assert_eq!(ex.outcome, Some(crate::hedge::HedgeOutcome::Unwound));
    ex.audit_done(3_300, &audit);
}
