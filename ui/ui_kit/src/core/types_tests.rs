//! #236 — what `EngineStatsView` owes each side of the wire.
//!
//! These are the *shape* contracts, kept where they can be tested without a
//! kernel: a NEW core's full payload, an OLD core's payload (the safety keys
//! absent), and the kernel's own `null` for a block it cannot build. The
//! producer side — the kernel's real `engine_stats()` output landing in this
//! type — is pinned across the seam by
//! `core/blitzkrieg_core/tests/engine_stats_view_seam.rs`, because a test that
//! only builds the struct by hand proves nothing about the wire.

use crate::core::types::{EngineStatsView, LastErrorView, TradingFrozenView};

/// The payload of a core that predates the trading-safety keys: nothing new is
/// present, and the view must still deserialize. `#[serde(default)]` is what
/// makes this true, and it is the reason `deny_unknown_fields` must never be
/// added (it would break this direction instead).
#[test]
fn a_core_that_omits_the_safety_keys_still_deserializes() {
    let legacy = serde_json::json!({
        "books": 1, "tops": 2, "spots": 3, "rounds": 4, "evaluations": 5,
        "signals": 6, "placeRejected": 7,
        "blocked": { "timing": 8, "momentum": 9 },
        "confirmed": ["0xabc"],
        "strategies": [],
    });
    let v: EngineStatsView =
        serde_json::from_value(legacy).expect("an older core's payload must still deserialize");

    assert_eq!(v.books, 1);
    assert_eq!(v.blocked.timing, 8, "the fields that existed still arrive");
    // The four safety keys read as "unknown", which is what suppresses the
    // banners — never as "the kernel says nothing is wrong".
    assert_eq!(v.venue_rejected, 0);
    assert!(v.last_error.is_none(), "no error slot: no banner");
    assert!(v.last_venue_error.is_none());
    assert!(v.self_check.is_none(), "no report: no banner");
    assert!(v.trading_frozen.is_none(), "no freeze key: no banner");
    assert!(v.reconcile.is_none());
}

/// The kernel sends `null` for a block it cannot build — `blocked` whenever no
/// engine is installed, since `--engine` is opt-in. A `null` into a struct field
/// is a hard serde error, and because `IpcClient::stats` deserializes the whole
/// payload at once, that one null used to cost the panel EVERY counter, not just
/// the gate tallies. `de_null_default` reads it as "this block is not there".
#[test]
fn a_null_block_reads_as_zero_gates_not_an_unreadable_snapshot() {
    let with_null = serde_json::json!({
        "books": 12, "blocked": null, "tradingFrozen": { "active": false },
    });
    let v: EngineStatsView =
        serde_json::from_value(with_null).expect("a null block must not fail the payload");
    assert_eq!(v.books, 12, "the counters beside the null still arrive");
    assert_eq!(v.blocked.timing, 0);
    assert_eq!(v.blocked.momentum, 0);
}

/// A healthy kernel's payload: every safety key present and empty. This is the
/// shape a live dry core actually sends, and the panel must draw no banner from
/// it — a view that turned "nothing is wrong" into a warning is the same defect
/// pointing the other way.
#[test]
fn a_healthy_payload_raises_no_banner() {
    let healthy = serde_json::json!({
        "venueRejected": 0,
        "lastError": null,
        "lastVenueError": null,
        "selfCheck": null,
        "tradingFrozen": { "active": false },
        "reconcile": { "consecutiveSweepFailures": 0, "freezeThreshold": 3 },
    });
    let v: EngineStatsView = serde_json::from_value(healthy).expect("deserializes");

    assert_eq!(v.venue_rejected, 0);
    assert!(v.last_error.is_none());
    assert!(v.last_venue_error.is_none());
    assert!(v.self_check.is_none());
    let frozen = v
        .trading_frozen
        .expect("the live kernel always sends the key");
    assert!(!frozen.active, "a live kernel is not frozen");
    assert_eq!(frozen.reason, None, "no reason key while trading is live");
    assert_eq!(v.reconcile.map(|r| r.freeze_threshold), Some(3));
}

/// The frozen state, as the panel reads it: `active` with a reason, and the
/// structured error beside it. This is the state that was invisible in the
/// cockpit before #236, so the shape is asserted rather than assumed.
#[test]
fn a_frozen_payload_carries_what_the_banner_renders() {
    let frozen = serde_json::json!({
        "venueRejected": 5,
        "lastError": { "tsMs": 1_000_000_000_000i64, "code": "NOT_AUTHENTICATED", "message": "auth failed" },
        "lastVenueError": { "tsMs": 1_000_000_000_000i64, "message": "NotAuthenticated: auth failed" },
        "tradingFrozen": { "active": true, "reason": "5 consecutive venue rejections; last: auth failed" },
    });
    let v: EngineStatsView = serde_json::from_value(frozen).expect("deserializes");

    let err: LastErrorView = v
        .last_error
        .expect("the structured record is the banner's source");
    assert_eq!(err.code, "NOT_AUTHENTICATED");
    assert_eq!(err.ts_ms, 1_000_000_000_000);
    let f: TradingFrozenView = v.trading_frozen.expect("freeze state");
    assert!(f.active);
    assert!(f.reason.unwrap().contains("consecutive venue rejections"));
}
