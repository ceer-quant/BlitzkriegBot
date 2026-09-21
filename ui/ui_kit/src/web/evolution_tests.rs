//! #251 — the panel-facing half of the decision-reason seam.
//!
//! `evolution_tests.rs` pins the kernel's tagged object landing in
//! `EvolutionProposalView`; this pins the other half, the view reaching
//! `/api/snapshot` as the camelCase JSON the panel's `EvolutionProposalRow`
//! TypeScript interface reads. Both halves are needed for the same reason as
//! #236: a type that carries the field and a renderer that never forwards it
//! still leaves the 台账 blank.

use crate::core::types::{DecisionReasonView, EvolutionProposalView, UiSnapshot};
use crate::web::render_json_full;

fn snapshot_json(s: &UiSnapshot) -> serde_json::Value {
    let body = render_json_full(s, None, None);
    serde_json::from_str(&body).expect("/api/snapshot is JSON")
}

fn row(state: &str, reason: Option<DecisionReasonView>) -> EvolutionProposalView {
    EvolutionProposalView {
        id: "prop-1-alpha".into(),
        strategy: "alpha".into(),
        state: state.into(),
        decided_by: Some("auto".into()),
        decided_at_ms: Some(1_790_000_000_000),
        decided_reason: reason,
        ..Default::default()
    }
}

/// The refused-adoption row — the exact case that read as an unexplained
/// "已拒绝" — reaches the panel with the lock and the guard's own words.
#[test]
fn the_snapshot_forwards_the_decision_reason() {
    let s = UiSnapshot {
        connected: true,
        evolution_proposals: vec![row(
            "rejected",
            Some(DecisionReasonView::GuardFailed {
                guard: "gradient".into(),
                detail: "GradientTooLarge { field: \"cap\", change: 0.0583, limit: 0.05 }".into(),
            }),
        )],
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let p = &doc["evolution"]["proposals"][0];

    assert_eq!(p["state"], "rejected");
    assert_eq!(p["decidedReason"]["kind"], "guardFailed");
    assert_eq!(p["decidedReason"]["guard"], "gradient");
    assert!(
        p["decidedReason"]["detail"]
            .as_str()
            .unwrap()
            .contains("GradientTooLarge"),
        "the guard's own words reach the panel: {p}"
    );
    // The supersede case renames the field on the wire, so the spelling the
    // panel branches on is asserted rather than assumed.
    let s2 = UiSnapshot {
        connected: true,
        evolution_proposals: vec![row(
            "superseded",
            Some(DecisionReasonView::Superseded {
                by_id: "prop-2-alpha".into(),
            }),
        )],
        ..Default::default()
    };
    assert_eq!(
        snapshot_json(&s2)["evolution"]["proposals"][0]["decidedReason"]["byId"],
        "prop-2-alpha"
    );
}

/// A row with no reason forwards `null` (never a missing key, never an empty
/// object): the panel's fallback — "原因未上报" — is what a null means, and an
/// absent key would be indistinguishable from an old gateway that sends no
/// `decidedReason` field at all.
#[test]
fn a_row_without_a_reason_forwards_null() {
    let s = UiSnapshot {
        connected: true,
        evolution_proposals: vec![row("rejected", None)],
        ..Default::default()
    };
    let p = &snapshot_json(&s)["evolution"]["proposals"][0];
    assert!(p.get("decidedReason").is_some(), "the key is present");
    assert!(p["decidedReason"].is_null(), "and null, not an object");
}
