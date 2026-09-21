//! #251 — what a decided evolution proposal owes the panel.
//!
//! The defect was a visibility gap, not a missing feature: a proposal refused by
//! the guard chain at decision time was closed with `state: "rejected"` and
//! nothing else, so the 台账 could not say whether a human said no, the gradient
//! lock refused it, or nobody looked at it for 7 days. Three different
//! situations, one word on screen.
//!
//! These tests pin the two shapes that make the distinction readable: the
//! kernel's tagged `decidedReason` object landing in the view, and that view
//! reaching `/api/snapshot` in the camelCase spelling the panel reads. Rows
//! written before the field existed must keep deserializing — the panel then
//! says "未上报" rather than inventing a reason.

use crate::core::types::{DecisionReasonView, EvolutionProposalView};

/// The kernel's own shape for a refusal: `{"kind":"guardFailed","guard":…,
/// "detail":…}` (serde's internally-tagged enum).
#[test]
fn a_guard_refusal_carries_the_lock_that_refused_it() {
    let wire = serde_json::json!({
        "id": "prop-1-alpha",
        "strategy": "alpha",
        "state": "rejected",
        "decidedBy": "auto",
        "decidedAtMs": 1_790_000_000_000i64,
        "decidedReason": {
            "kind": "guardFailed",
            "guard": "gradient",
            "detail": "GradientTooLarge { field: \"cap\", change: 0.0583, limit: 0.05 }",
        },
    });
    let p: EvolutionProposalView = serde_json::from_value(wire).expect("deserializes");

    assert_eq!(p.state, "rejected");
    let r = p.decided_reason.expect("the reason reaches the view");
    assert_eq!(r.failed_guard(), Some("gradient"));
    match r {
        DecisionReasonView::GuardFailed { detail, .. } => {
            assert!(
                detail.contains("GradientTooLarge"),
                "the guard's words: {detail}"
            );
        }
        other => panic!("expected a guard refusal, got {other:?}"),
    }
}

/// The other three endings are their own tags, so the panel never has to infer
/// them from the state word.
#[test]
fn the_other_endings_are_distinguishable() {
    let reason = |v: serde_json::Value| {
        let p: EvolutionProposalView = serde_json::from_value(serde_json::json!({
            "id": "p", "state": "rejected", "decidedReason": v,
        }))
        .expect("deserializes");
        p.decided_reason.expect("reason present")
    };

    assert_eq!(
        reason(serde_json::json!({"kind": "rejected"})),
        DecisionReasonView::Rejected
    );
    assert_eq!(
        reason(serde_json::json!({"kind": "expired"})),
        DecisionReasonView::Expired
    );
    assert_eq!(
        reason(serde_json::json!({"kind": "superseded", "byId": "prop-2-alpha"})),
        DecisionReasonView::Superseded {
            by_id: "prop-2-alpha".into()
        }
    );
    // Only a guard refusal names a lock; the others must not answer with one.
    assert_eq!(
        reason(serde_json::json!({"kind": "expired"})).failed_guard(),
        None
    );
}

/// A core that predates the field, and a proposal still pending, both arrive
/// with nothing to render — and both must deserialize (the row is otherwise
/// complete, and losing the whole row over a missing reason would be worse than
/// the gap this field closes).
#[test]
fn a_row_without_a_reason_still_deserializes() {
    let legacy: EvolutionProposalView = serde_json::from_value(serde_json::json!({
        "id": "prop-1-alpha", "strategy": "alpha", "state": "rejected",
        "decidedBy": "user", "decidedAtMs": 1_790_000_000_000i64,
    }))
    .expect("an old row deserializes");
    assert_eq!(legacy.decided_reason, None);

    let pending: EvolutionProposalView = serde_json::from_value(serde_json::json!({
        "id": "prop-2-alpha", "strategy": "alpha", "state": "proposed",
        "decidedBy": null, "decidedAtMs": null, "decidedReason": null,
    }))
    .expect("a pending row deserializes");
    assert!(pending.is_pending());
    assert_eq!(pending.decided_reason, None);
}
