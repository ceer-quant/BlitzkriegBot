//! Issue #842 (lane A) — the producer half of the #236 seam, now contract-side.
//!
//! The old seam test (`tests/engine_stats_view_seam.rs`) linked the UI crate as
//! a dev-dependency to feed the kernel's `engine.stats` payload straight into
//! `EngineStatsView`. That core→UI edge is exactly what the #842 acceptance
//! gates (A1/A2) forbid, so the seam is re-owned in two halves sharing one
//! fixture:
//!
//! * THIS file (core-owned, producer half): the REAL dry-core payload — driven
//!   through the real refusal / reconcile-failure / self-check entry points,
//!   serialized by the real `engine_stats_at` — must equal the frozen fixture
//!   (`ui/ui_kit/tests/fixtures/engine_stats_dry_frozen.json`) byte-identically.
//!   Any producer-side shape change fails here with a diff, before any UI
//!   consumer can silently break.
//! * The consumer half lives in `ui/ui_kit/tests/engine_stats_fixture.rs`,
//!   which deserializes the SAME JSON into the UI view type and asserts the
//!   panel-visible fields — no core→UI dependency in either direction.
//!
//! The fixture path is read through `CARGO_MANIFEST_DIR`, so the freeze stays
//! one artifact even though the two halves live in different crates.

use blitzkrieg_core::model::{CoreError, CoreErrorCode, FillPolicy, OrderRequest, Side};
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_market_api::{SelfCheckItem, SelfCheckReport};
use rust_decimal_macros::dec;

/// A fixed instant (2001-09-09, 01:46:40 UTC) — a fixture constant, never "now",
/// so nothing in this file depends on the wall clock. The frozen fixture was
/// produced at this same clock, so the timestamps line up byte-for-byte.
const NOW: i64 = 1_000_000_000_000;

/// The frozen fixture, verbatim (the shared artifact with the UI crate).
const FROZEN: &str = include_str!("../../../ui/ui_kit/tests/fixtures/engine_stats_dry_frozen.json");

/// A dry kernel with no durable state at all: `trade_log_path`/`order_log_path`/
/// `position_log_path` are cleared so a test run cannot touch an operator's data
/// directory (`CoreConfig::default()` points them at `data/…`).
fn dry_core() -> Core {
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

/// One order intent, in the shape the engine hands to `Core::place_pending`.
fn intent(key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(10),
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: blitzkrieg_core::model::default_account_id(),
    }
}

/// The producer half of the seam: the kernel's own `engine.stats` output, under
/// the exact state the fixture was frozen at (a venue refusal, a failed
/// reconcile sweep and a failed self-check), must equal the frozen wire shape
/// byte-for-byte. A drift here is a `[CONTRACT-BREAKING]` candidate: fix the
/// producer or re-freeze the fixture deliberately, never let the wire move by
/// accident.
#[test]
fn engine_stats_matches_the_frozen_dry_fixture_byte_for_byte() {
    let mut c = dry_core();
    // Exercise the paths so the safety keys are populated exactly as in the
    // frozen fixture: a venue refusal fills the error slot, a failed sweep
    // fills the streak, and a failed self-check freezes trading (which is why
    // it goes LAST — a frozen kernel refuses new placements).
    let (id, _) = c.place_pending(intent("k1"), NOW + 2).expect("pending");
    c.reject_live_result(
        &id,
        Some(
            CoreError::new(CoreErrorCode::NotAuthenticated, "auth failed")
                .with_raw("401 unauthorized"),
        ),
        NOW + 3,
    )
    .expect("reject");
    c.on_reconcile_failed(
        CoreError::new(CoreErrorCode::VenueError, "sweep timed out"),
        NOW + 4,
    );
    c.on_venue_self_check(
        "polymarket",
        SelfCheckReport {
            ok: false,
            ts_ms: NOW + 5,
            items: vec![SelfCheckItem {
                name: "venue_balance".into(),
                ok: false,
                detail: "HTTP 401 unauthorized".into(),
            }],
        },
    );
    assert!(
        c.is_killed(),
        "fixture: the failed self-check froze trading"
    );

    let raw = c.engine_stats_at(NOW + 6);

    // The `probe.fd` block is an OBSERVATION of the host's rlimit (hard = RLIMIT_NOFILE:
    // 9223372036854775807 on this macOS host, 65536 on Linux CI; `open`/`usagePct` read
    // procfs, which macOS does not have). Those numbers are environment facts, not wire
    // shape — the deleted seam test avoided them by asserting only selected keys. The
    // freeze keeps the full payload byte-exact and normalizes ONLY this block to a
    // canonical placeholder on both sides, so shape drift anywhere else still fails.
    let raw = normalize_host_facts(raw);

    let produced = serde_json::to_string_pretty(&raw).expect("engine.stats serializes");

    // Byte-for-byte: `serde_json::to_string_pretty` on both sides keeps field
    // order and formatting part of the freeze (the same discipline as the
    // schema_family_golden wire literals).
    let frozen = normalize_host_facts(
        serde_json::from_str(FROZEN).expect("the frozen fixture is valid JSON"),
    );
    let frozen = serde_json::to_string_pretty(&frozen).expect("frozen fixture re-serializes");
    if produced != frozen {
        // A produced-vs-frozen diff that names the first differing line, so a
        // red run is diagnosable without re-running under a debugger.
        let p: Vec<&str> = produced.lines().collect();
        let f: Vec<&str> = frozen.lines().collect();
        let mut first = String::from("(lengths differ)");
        for (i, (a, b)) in p.iter().zip(f.iter()).enumerate() {
            if a != b {
                first = format!("line {}: produced `{a}` vs frozen `{b}`", i + 1);
                break;
            }
        }
        panic!(
            "engine.stats drifted from the frozen fixture ({first}).\n\
             This is the #842 producer half of the #236 seam: either revert the \
             producer change or re-freeze \
             ui/ui_kit/tests/fixtures/engine_stats_dry_frozen.json deliberately \
             (and re-check the consumer half in ui/ui_kit/tests)."
        );
    }
}

/// Environment-dependent numbers in the payload, normalized to a canonical
/// placeholder so the byte freeze compares SHAPE on every host. Only the
/// `probe.fd` rlimit/procfs observation is host-dependent; everything else in
/// `engine.stats_at` is derived from the fixture's fixed virtual clock and the
/// deterministic dry core. Field NAMES stay frozen on both sides.
fn normalize_host_facts(mut v: serde_json::Value) -> serde_json::Value {
    let placeholder = serde_json::json!({
        "soft": 0, "hard": 0, "open": null, "usagePct": null, "critical": false,
    });
    if let Some(probe) = v.get_mut("probe").and_then(|p| p.as_object_mut()) {
        probe.insert("fd".to_string(), placeholder);
    }
    v
}

/// Fixture guard, kernel side: the frozen fixture really does carry what the
/// consumer half asserts. If a future re-freeze drops a safety key, this fails
/// BEFORE the UI test sends anyone hunting through the view type.
#[test]
fn frozen_fixture_carries_the_panel_safety_keys() {
    let v: serde_json::Value = serde_json::from_str(FROZEN).expect("the fixture is valid JSON");
    for key in [
        "venueRejected",
        "lastError",
        "lastVenueError",
        "reconcile",
        "selfCheck",
        "tradingFrozen",
        "accountGates",
        "strategies",
    ] {
        assert!(
            v.get(key).is_some(),
            "the frozen fixture lost the `{key}` key — re-freeze deliberately"
        );
    }
    assert_eq!(
        v["selfCheck"]["items"][0]["name"], "venue_balance",
        "the fixture's self-check probe is the consumer half's assertion target"
    );
    // Issue #991: the settlement block is a CARRIED key (the amber
    // overdue-receivable banner reads its fallback slice), so the fixture
    // must keep the block AND its #991 additive fields. Presence, not
    // non-null: `oldestFallbackClaimAgeMs` is legitimately `null` when no
    // fallback claim is open.
    let settlement = v["settlement"]
        .as_object()
        .expect("the fixture carries the settlement block (issue #991's receivable surface)");
    for key in [
        "fallbackReceivableUsd",
        "pendingFallbackClaims",
        "oldestFallbackClaimAgeMs",
        "unverifiedExpiryPolicy",
    ] {
        assert!(
            settlement.contains_key(key),
            "the frozen fixture lost `settlement.{key}` — re-freeze deliberately \
             (issue #991's receivable-aging surface)"
        );
    }
}
