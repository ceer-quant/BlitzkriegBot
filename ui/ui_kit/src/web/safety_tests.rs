//! #236 — the panel-facing half of the seam, tested where both ends are visible.
//!
//! The kernel-side half (a real dry core's `engine_stats()` payload landing in
//! `EngineStatsView`) is pinned by `core/blitzkrieg_core/tests/
//! engine_stats_view_seam.rs`. This file pins the other direction: the view
//! reaching `/api/snapshot` as the camelCase JSON the panel's `EngineStats`
//! TypeScript interface reads.
//!
//! Both halves matter because the defect was a chain, not a point: the kernel
//! emitted the keys, the view type could not carry them, and the renderer only
//! forwards what the view carries. Fixing the type alone would still have left
//! the banners dark if `render_json_full` had not forwarded them, and that is
//! exactly the kind of gap a struct-only test cannot see.

use crate::core::types::{
    EngineStatsView, LastErrorView, LegacyVenueErrorView, ReconcileView, SelfCheckItemView,
    SelfCheckView, TradingFrozenView, UiSnapshot,
};
use crate::web::render_json_full;

/// A frozen kernel with a refusal behind it: the exact state in which the panel
/// used to draw nothing at all.
fn frozen_snapshot() -> UiSnapshot {
    UiSnapshot {
        connected: true,
        stats: Some(EngineStatsView {
            books: 12,
            venue_rejected: 5,
            last_error: Some(LastErrorView {
                ts_ms: 1_000_000_000_000,
                code: "NOT_AUTHENTICATED".into(),
                message: "auth failed".into(),
            }),
            last_venue_error: Some(LegacyVenueErrorView {
                ts_ms: 1_000_000_000_000,
                message: "NotAuthenticated: auth failed".into(),
            }),
            reconcile: Some(ReconcileView {
                consecutive_sweep_failures: 2,
                freeze_threshold: 3,
            }),
            self_check: Some(SelfCheckView {
                ok: false,
                ts_ms: 1_000_000_000_000,
                items: vec![SelfCheckItemView {
                    name: "venue_balance".into(),
                    ok: false,
                    detail: "HTTP 401 unauthorized".into(),
                }],
            }),
            trading_frozen: Some(TradingFrozenView {
                active: true,
                reason: Some("5 consecutive venue rejections; last: auth failed".into()),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn snapshot_json(s: &UiSnapshot) -> serde_json::Value {
    let body = render_json_full(s, None, None);
    serde_json::from_str(&body).expect("/api/snapshot is JSON")
}

/// Every key the panel's banner block reads, in the spelling the panel reads it.
#[test]
fn the_snapshot_forwards_the_trading_safety_block() {
    let doc = snapshot_json(&frozen_snapshot());
    let st = &doc["stats"];

    assert_eq!(st["books"], 12, "the counters beside them are unchanged");
    assert_eq!(st["venueRejected"], 5, "refusal count reaches the panel");
    assert_eq!(st["lastError"]["tsMs"], 1_000_000_000_000i64);
    assert_eq!(st["lastError"]["code"], "NOT_AUTHENTICATED");
    assert_eq!(st["lastError"]["message"], "auth failed");
    assert_eq!(
        st["lastVenueError"]["message"], "NotAuthenticated: auth failed",
        "the legacy spelling is still forwarded for a panel that has not migrated"
    );
    assert_eq!(st["reconcile"]["consecutiveSweepFailures"], 2);
    assert_eq!(st["reconcile"]["freezeThreshold"], 3);
    assert_eq!(st["selfCheck"]["ok"], false);
    assert_eq!(st["selfCheck"]["items"][0]["name"], "venue_balance");
    assert_eq!(
        st["selfCheck"]["items"][0]["detail"],
        "HTTP 401 unauthorized"
    );
    assert_eq!(st["tradingFrozen"]["active"], true);
    assert_eq!(
        st["tradingFrozen"]["reason"],
        "5 consecutive venue rejections; last: auth failed"
    );
}

/// The reason key is omitted, not nulled, while trading is live — the kernel's
/// own shape, and what the panel's `reason || '交易已被冻结'` fallback reads.
#[test]
fn a_live_kernel_renders_no_banner() {
    let s = UiSnapshot {
        connected: true,
        stats: Some(EngineStatsView {
            trading_frozen: Some(TradingFrozenView {
                active: false,
                reason: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let st = &doc["stats"];

    assert_eq!(st["tradingFrozen"]["active"], false);
    assert!(
        st["tradingFrozen"].get("reason").is_none(),
        "no reason key while trading is live, got: {}",
        st["tradingFrozen"]
    );
    assert!(st["lastError"].is_null(), "nothing has failed");
    assert!(st["selfCheck"].is_null(), "no self-check has run");
    assert!(st["lastVenueError"].is_null());
    assert_eq!(st["venueRejected"], 0);
}

/// An older core: the view carries only what the old keys provided, and the
/// snapshot renders `null` for the rest. The panel reads that as "no banner" —
/// the graceful degradation this fix must not break in either direction.
#[test]
fn an_old_core_renders_nulls_and_the_panel_shows_no_banner() {
    let s = UiSnapshot {
        connected: true,
        // What `IpcClient::stats` produces from a core whose payload has only
        // the ten original keys.
        stats: Some(EngineStatsView {
            books: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    let doc = snapshot_json(&s);
    let st = &doc["stats"];

    assert_eq!(st["books"], 1);
    assert!(st["venueRejected"].is_number(), "the counter degrades to 0");
    assert!(st["lastError"].is_null());
    assert!(st["lastVenueError"].is_null());
    assert!(st["selfCheck"].is_null());
    assert!(st["tradingFrozen"].is_null());
    assert!(st["reconcile"].is_null());
}
