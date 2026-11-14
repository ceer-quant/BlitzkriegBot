//! Issue #842 (lane A) — the shared-fixture contract test that replaces the
//! #236 core→UI seam (`core/blitzkrieg_core/tests/engine_stats_view_seam.rs`,
//! deleted along with its `blitzkrieg-ui-kit` dev-dependency).
//!
//! The seam's job was: prove the UI's `EngineStatsView` can carry what the
//! kernel actually emits as `engine.stats`, without a review step in between.
//! The new ownership is contract-side, in two halves that share THIS fixture
//! file (`fixtures/engine_stats_dry_frozen.json`):
//!
//! 1. **Producer half (core-owned)**: `core/blitzkrieg_core/tests/
//!    engine_stats_wire.rs` freezes the REAL dry-core `engine.stats` payload —
//!    driven through the real refusal / reconcile-failure / self-check entry
//!    points — byte-identically against this JSON. The fixture here is
//!    therefore the kernel's own output, not a hand-written shape.
//! 2. **Consumer half (this file, UI-owned)**: the same JSON must deserialize
//!    into `EngineStatsView` with the panel-visible fields populated, and the
//!    safety keys must round-trip the wire values through the view untouched.
//!    `IpcClient::stats` feeds every adapter through exactly this type, so a
//!    re-spelled or dropped key fails HERE, in the UI crate, without any
//!    core→UI dependency edge.
//!
//! Both directions were previously pinned across the seam; both are pinned now,
//! with the fixture as the shared artifact (the #842 "TUI and webUI contract
//! tests run the same fixtures" posture, one lane early).

use blitzkrieg_ui_kit::EngineStatsView;

fn fixture() -> serde_json::Value {
    let raw = include_str!("fixtures/engine_stats_dry_frozen.json");
    serde_json::from_str(raw).expect("the frozen engine.stats fixture is valid JSON")
}

/// The consumer half of the seam: the kernel's REAL engine.stats payload (see
/// the fixture's producer test in blitzkrieg-core) deserializes into the UI
/// view with every panel-visible block populated — nothing may fail or default
/// away silently.
#[test]
fn the_kernels_real_engine_stats_payload_deserializes_into_the_view() {
    let raw = fixture();
    let v: EngineStatsView = serde_json::from_value(raw.clone())
        .expect("the UI view must accept the kernel's own engine.stats payload");

    // Counters.
    assert_eq!(v.venue_rejected, 1);
    assert_eq!(v.illegal_price_rejected, 0);

    // The structured error slot (the trading-error banner's source).
    let err = v.last_error.expect("lastError carried");
    assert_eq!(err.code, "VENUE_ERROR");
    assert_eq!(err.ts_ms, 1_000_000_000_005);
    assert!(err.message.contains("trading self-check failed"));

    // The legacy spelling: ONE record with the structured slot, same instant.
    let legacy = v.last_venue_error.expect("lastVenueError carried");
    assert_eq!(legacy.ts_ms, err.ts_ms);

    // E31-b: the reconcile streak + the kernel's own threshold.
    let reconcile = v.reconcile.expect("reconcile carried");
    assert_eq!(reconcile.consecutive_sweep_failures, 1);
    assert_eq!(reconcile.freeze_threshold, 3);

    // #163: the failed self-check report — the contract type re-exported from
    // bk_contract (issue #842 convergence), not a local mirror.
    let report = v.self_check.expect("selfCheck carried");
    assert!(!report.ok);
    assert_eq!(report.ts_ms, 1_000_000_000_005);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].name, "venue_balance");
    assert!(!report.items[0].ok);
    assert_eq!(report.items[0].detail, "HTTP 401 unauthorized");

    // The kill switch is down globally (the freeze below is account-scoped),
    // and the account freeze book names the reason.
    let frozen = v.trading_frozen.expect("tradingFrozen carried");
    assert!(frozen.active);
    assert!(frozen
        .reason
        .as_deref()
        .unwrap_or_default()
        .contains("self-check"));
    assert_eq!(v.account_gates.len(), 1);
    assert_eq!(v.account_gates[0].account, "global");
    assert!(v.account_gates[0].blocked);
    assert!(v.account_gates[0]
        .reason
        .as_deref()
        .unwrap_or_default()
        .contains("self-check"));

    // E9-g: the per-strategy accounting row.
    assert_eq!(v.strategies.len(), 1);
    let row = &v.strategies[0];
    assert_eq!(row.name, "spread_arb");
    assert!(!row.enabled);
    assert_eq!(row.orders_rejected, 1);
    assert_eq!(
        row.rejection_causes
            .as_ref()
            .map(|m| m.get("venue.NotAuthenticated").copied().unwrap_or(0)),
        Some(1)
    );
    assert_eq!(row.gate_exemptions, Vec::<String>::new());

    // Issue #991: the settlement block is CARRIED (no longer waived — the
    // amber overdue-receivable banner reads `oldestFallbackClaimAgeMs` /
    // `fallbackReceivableUsd` / `unverifiedExpiryPolicy` from it). The dry
    // fixture's policy is the factory default and nothing has fallen back.
    let settlement = &v.settlement;
    assert_eq!(settlement.unverified_expiry_policy, "cash");
    assert_eq!(settlement.pending_fallback_claims, 0);
    assert_eq!(settlement.fallback_receivable_usd, Some(0.0));
    assert_eq!(settlement.oldest_fallback_claim_age_ms, None);
}

/// The safety keys must survive BYTE-FOR-BYTE through the view: a re-spelled
/// or re-shaped field (the mutation that keeps the key but loses the meaning)
/// is caught by round-tripping the view and diffing the keys the panel reads.
#[test]
fn safety_keys_round_trip_the_wire_values_through_the_view() {
    let raw = fixture();
    let v: EngineStatsView =
        serde_json::from_value(raw.clone()).expect("the view accepts the fixture");
    let carried = serde_json::to_value(&v).expect("the view serializes back");

    for key in [
        "venueRejected",
        "lastError",
        "lastVenueError",
        "reconcile",
        "selfCheck",
        "tradingFrozen",
    ] {
        assert_eq!(
            carried.get(key),
            raw.get(key),
            "`{key}` reaches the panel in a different shape than the kernel sent it"
        );
    }
    // Issue #991: `settlement` is carried as a SLICE — the view keeps the
    // fields the panel reads (`receivableUsd` + the fallback-aging surface)
    // and drops the bookkeeping rest, so the round-trip is per-field, not
    // whole-block. Every carried field must equal the wire's value exactly.
    let raw_settlement = raw
        .get("settlement")
        .and_then(|s| s.as_object())
        .expect("the fixture carries the settlement block");
    let carried_settlement = carried
        .get("settlement")
        .and_then(|s| s.as_object())
        .expect("the view serializes the settlement slice back");
    for key in carried_settlement.keys() {
        assert_eq!(
            carried_settlement.get(key),
            raw_settlement.get(key),
            "`settlement.{key}` reaches the panel in a different shape than the kernel sent it"
        );
    }
}

/// Every key the fixture carries must be carried by the view or explicitly
/// waived (with why). This is the runtime half of the old seam's drift guard:
/// a key that appears in a future kernel payload and lands in NEITHER list
/// fails here, in the UI crate, the same way the #236 silent drop would have.
#[test]
fn every_fixture_key_is_carried_or_explicitly_waived() {
    let raw = fixture();
    let v: EngineStatsView =
        serde_json::from_value(raw.clone()).expect("the view accepts the fixture");
    let carried = serde_json::to_value(&v).expect("the view serializes back");
    let raw_keys = raw.as_object().expect("engine.stats is an object");
    let carried_keys = carried.as_object().expect("the view serializes an object");

    for key in raw_keys.keys() {
        if NOT_CARRIED.iter().any(|(k, _)| k == key) {
            continue;
        }
        assert!(
            carried_keys.contains_key(key),
            "engine.stats key `{key}` is emitted by the kernel but EngineStatsView cannot \
             carry it — the #236 silent drop. Carry it in ui/ui_kit/src/core/types.rs \
             (and render it in ui/ui_kit/src/web/render.rs), or waive it in NOT_CARRIED \
             with a reason."
        );
    }
    // The waiver list must describe THIS payload: an entry the fixture no
    // longer carries is a stale claim, and a stale claim makes the guard
    // above vacuous.
    for (key, why) in NOT_CARRIED {
        assert!(
            raw_keys.contains_key(*key),
            "NOT_CARRIED names `{key}` ({why}) but the frozen fixture no longer carries it — \
             drop the entry (or carry the key)"
        );
    }
}

/// Every `engine.stats` key the kernel emits that `EngineStatsView` deliberately
/// does NOT carry, with why. Inherited verbatim from the deleted seam test's
/// waiver table (the reasons are kernel-side facts and did not change).
const NOT_CARRIED: &[(&str, &str)] = &[
    (
        "strategyLimitRejected",
        "per-strategy cap refusals; panel uses stats rows",
    ),
    (
        "accountingAudit",
        "in-kernel books audit; no panel surface yet",
    ),
    (
        "orderbookFreshness",
        "staleness budget; no panel surface yet",
    ),
    ("sizing", "per-order notional bounds; no panel surface yet"),
    (
        "exitReasons",
        "pending-close reason table; no panel surface yet",
    ),
    ("confirmedDetail", "per-token confirmation diagnostics"),
    (
        "kline",
        "K-line aggregator diagnostics (E29); the chart polls kline.history, \
         no panel surface reads engine.stats for it yet",
    ),
    (
        "resolutions",
        "#377 (治 #2) archive verdict events replayed into the core; \
         settlement progress is already surfaced via the settlement block, \
         no panel surface reads the feed counter yet",
    ),
    (
        "cashflowRebates",
        "#388 wallet-level cash rows credited from the stream (count); \
         the backtest report carries the number, no panel surface reads it yet",
    ),
    (
        "cashflowRewards",
        "#388 wallet-level cash rows credited from the stream (count); \
         the backtest report carries the number, no panel surface reads it yet",
    ),
    (
        "cashflowRebatesUsd",
        "#388 wallet-level cash rows credited from the stream (USD); \
         the backtest report carries the number, no panel surface reads it yet",
    ),
    (
        "cashflowRewardsUsd",
        "#388 wallet-level cash rows credited from the stream (USD); \
         the backtest report carries the number, no panel surface reads it yet",
    ),
    (
        "probe",
        "任务二 1-3 环境差异排查探针（NTP 时钟偏差/tick 断流/spot 失效）； \
         排查面，no panel surface yet — operator reads it from engine.stats \
         directly (tests/env_probes.rs pins the field names)",
    ),
];
