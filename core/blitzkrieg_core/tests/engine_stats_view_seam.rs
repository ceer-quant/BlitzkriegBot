//! #236 — the seam between what the kernel EMITS as `engine.stats` and what the
//! UI Kit's `EngineStatsView` can CARRY.
//!
//! The P1 defect this pins was invisible to every existing test, and the reason
//! is worth stating before the assertions: each side was correct on its own.
//! The kernel emitted `tradingFrozen` / `lastError` / `selfCheck`; the panel
//! declared those fields on its TypeScript interface. In between,
//! `IpcClient::stats` deserialized the kernel's payload into a struct with ten
//! hard-coded fields and serde's default "ignore unknown keys" silently dropped
//! everything else. So the panel's three trading-safety banners could never
//! render — a kill-switch freeze left the cockpit looking completely normal —
//! and nothing failed: no error, no log line, no red test.
//!
//! A unit test that builds an `EngineStatsView` by hand cannot catch that. It
//! tests the *consumer's* shape against itself. The seam is only pinned by
//! handing the VIEW the kernel's OWN payload — a real dry `Core`, driven through
//! the real freeze / refusal / self-check entry points, serialized by the real
//! `engine_stats()` — which is what this file does. Both halves of the contract
//! are then asserted:
//!
//! 1. **Values**: the four panel-read keys arrive populated with the values the
//!    kernel produced, not with `Default`. (Deleting a field from
//!    `EngineStatsView` makes this file fail to compile; re-spelling its serde
//!    key makes it fail at run time. Both were verified by mutation — see the
//!    PR body for #236.)
//! 2. **No silent drop**: every top-level key the kernel emits is either carried
//!    by the view or named in [`NOT_CARRIED`]. A future kernel key cannot vanish
//!    into the same hole unnoticed: the test goes red and forces the decision.

use blitzkrieg_core::model::{CoreError, CoreErrorCode, FillPolicy, OrderRequest, Side};
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_market_api::{SelfCheckItem, SelfCheckReport};
use blitzkrieg_ui_kit::EngineStatsView;
use rust_decimal_macros::dec;

/// A fixed instant (2001-09-09, 01:46:40 UTC) — a fixture constant, never "now",
/// so nothing in this file depends on the wall clock.
const NOW: i64 = 1_000_000_000_000;

/// Every `engine.stats` key the kernel emits that `EngineStatsView` deliberately
/// does NOT carry, with why. The panel reads none of them today; the drift guard
/// below fails if the kernel stops sending one (the list went stale) or if it
/// starts sending a key that is in neither this list nor the view (the #236
/// failure mode, one release later).
const NOT_CARRIED: &[(&str, &str)] = &[
    (
        "strategyLimitRejected",
        "per-strategy cap refusals; panel uses stats rows",
    ),
    (
        "accountingAudit",
        "in-kernel books audit; no panel surface yet",
    ),
    ("dailyLoss", "day-loss budget; no panel surface yet"),
    (
        "orderbookFreshness",
        "staleness budget; no panel surface yet",
    ),
    ("sizing", "per-order notional bounds; no panel surface yet"),
    (
        "exitReasons",
        "pending-close reason table; no panel surface yet",
    ),
    (
        "settlement",
        "settled-but-unredeemed claims; no panel surface yet",
    ),
    ("confirmedDetail", "per-token confirmation diagnostics"),
    ("archive", "event archive status; no panel surface yet"),
];

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
    }
}

/// The refusal a venue returns when credentials are rejected — the realistic
/// first cause of an operator-visible trading error.
fn venue_refusal() -> CoreError {
    CoreError::new(CoreErrorCode::NotAuthenticated, "auth failed").with_raw("401 unauthorized")
}

/// THE seam, in one function: run the kernel's own serializer and hand the
/// result to the view type the UI Kit deserializes with. A failure here is the
/// #236 defect (or its reintroduction), so the panic carries the payload.
fn view_of(c: &Core) -> EngineStatsView {
    let raw = c.engine_stats();
    match serde_json::from_value::<EngineStatsView>(raw.clone()) {
        Ok(v) => v,
        Err(e) => {
            panic!("EngineStatsView rejected the kernel's own engine.stats payload: {e}\n{raw}")
        }
    }
}

/// Every key the kernel emits is carried by the view or explicitly waived.
///
/// The key-coverage half of this test is a *runtime* assertion on the view's own
/// serialization, so it is the half that stays honest if someone drops a field
/// from the struct: the key stops coming out, is in no waiver list, and the test
/// fails as an assertion rather than as a compile error.
#[test]
fn every_engine_stats_key_is_carried_or_explicitly_waived() {
    let mut c = dry_core();
    // Exercise the paths so the safety keys are not merely present-but-null: a
    // venue refusal fills the error slot, a failed sweep fills the streak, and a
    // failed self-check freezes trading (which is why it goes LAST — a frozen
    // kernel refuses new placements).
    let (id, _) = c.place_pending(intent("k1"), NOW + 2).expect("pending");
    c.reject_live_result(&id, Some(venue_refusal()), NOW + 3)
        .expect("reject");
    c.on_reconcile_failed(
        CoreError::new(CoreErrorCode::VenueError, "sweep timed out"),
        NOW + 4,
    );
    c.on_self_check(SelfCheckReport {
        ok: false,
        ts_ms: NOW + 5,
        items: vec![SelfCheckItem {
            name: "venue_balance".into(),
            ok: false,
            detail: "HTTP 401 unauthorized".into(),
        }],
    });
    assert!(
        c.is_killed(),
        "fixture: the failed self-check froze trading"
    );

    let raw = c.engine_stats();
    let raw_keys = raw.as_object().expect("engine.stats is an object").clone();
    let carried = serde_json::to_value(view_of(&c)).expect("the view serializes");
    let carried_keys = carried.as_object().expect("an object").clone();

    for key in raw_keys.keys() {
        if NOT_CARRIED.iter().any(|(k, _)| k == key) {
            continue;
        }
        assert!(
            carried_keys.contains_key(key),
            "engine.stats key `{key}` is emitted by the kernel but EngineStatsView cannot \
             carry it — the #236 silent drop. Carry it in ui/ui_kit/src/core/types.rs \
             (and render it in ui/ui_kit/src/web/mod.rs), or waive it in NOT_CARRIED with a reason."
        );
    }
    // The waiver list must describe THIS kernel: an entry the kernel no longer
    // emits is a stale claim, and a stale claim makes the guard above vacuous.
    for (key, why) in NOT_CARRIED {
        assert!(
            raw_keys.contains_key(*key),
            "NOT_CARRIED names `{key}` ({why}) but the kernel no longer emits it — \
             drop the entry (or carry the key)"
        );
    }

    // The safety block must survive BYTE-FOR-BYTE, not merely key-for-key: a
    // re-spelled or re-shaped field (the mutation that keeps the key but loses
    // the meaning) is caught here.
    for key in [
        "venueRejected",
        "lastError",
        "lastVenueError",
        "reconcile",
        "selfCheck",
        "tradingFrozen",
    ] {
        assert_eq!(
            carried_keys.get(key),
            raw_keys.get(key),
            "`{key}` reaches the panel in a different shape than the kernel sent it"
        );
    }
    // Negative control for that equality: the waived keys really are absent from
    // the view, so the loop above is not passing because everything round-trips.
    assert!(
        !carried_keys.contains_key("dailyLoss"),
        "the waived list is stale in the other direction: dailyLoss is carried now"
    );
}

/// A venue-refusal storm freezes trading. Before #236 the panel drew nothing at
/// all here — the freeze, the reason and the refusal count were all dropped one
/// layer above the kernel.
#[test]
fn a_freeze_reaches_the_panel_through_the_view_type() {
    let mut c = dry_core();
    // Drive refusals until the kernel's own streak threshold freezes trading
    // (the threshold is the kernel's, so the loop is bounded, not assumed).
    let mut rejections = 0u64;
    while !c.is_killed() && rejections < 16 {
        let (id, _) = c
            .place_pending(intent(&format!("k{rejections}")), NOW + rejections as i64)
            .expect("pending");
        c.reject_live_result(&id, Some(venue_refusal()), NOW + rejections as i64 + 100)
            .expect("reject");
        rejections += 1;
    }
    assert!(
        c.is_killed(),
        "fixture: {rejections} consecutive venue refusals must freeze trading"
    );

    let raw = c.engine_stats();
    // Fixture guard, on the kernel's side of the seam: the raw payload really
    // does carry what the panel needs, so a failure below is the view's fault.
    assert_eq!(raw["tradingFrozen"]["active"], serde_json::json!(true));
    assert_eq!(raw["venueRejected"], serde_json::json!(rejections));

    let view = view_of(&c);

    // 1. The refusal counter.
    assert_eq!(
        view.venue_rejected, rejections,
        "venueRejected must survive the view type, not arrive as Default"
    );

    // 2. The freeze banner's source.
    let frozen = view
        .trading_frozen
        .as_ref()
        .expect("tradingFrozen must survive the view type — the freeze banner has no other source");
    assert!(
        frozen.active,
        "the kernel is frozen; the view says otherwise"
    );
    let reason = frozen
        .reason
        .as_deref()
        .expect("a freeze carries its reason");
    assert!(
        reason.contains("consecutive venue rejections"),
        "the freeze reason must reach the panel verbatim, got: {reason}"
    );

    // 3. The error banner's structured source, and the legacy fallback that a
    //    panel which has not migrated yet still reads.
    let err = view
        .last_error
        .as_ref()
        .expect("lastError must survive the view type — the refusal reason has no other source");
    assert_eq!(err.code, "NOT_AUTHENTICATED", "code is the CoreErrorCode");
    assert_eq!(err.message, "auth failed");
    assert!(err.ts_ms > 0, "the record carries the instant it happened");
    let legacy = view
        .last_venue_error
        .as_ref()
        .expect("lastVenueError is the fallback a not-yet-migrated panel reads");
    assert_eq!(
        legacy.ts_ms, err.ts_ms,
        "both spellings are ONE record: same instant, never a second, staler copy"
    );
    // Rendered as the kernel renders it: Rust's `Debug` spelling of the code, a
    // colon, the message. Pinned here because a panel that parses this string
    // (rather than reading `lastError.code`) is depending on that spelling.
    assert_eq!(legacy.message, "NotAuthenticated: auth failed");

    // 4. E31-b: the streak that explains how close the sweep is to freezing.
    let reconcile = view
        .reconcile
        .as_ref()
        .expect("reconcile must survive the view type");
    assert!(
        reconcile.freeze_threshold > 0,
        "the kernel's own threshold travels with the streak"
    );
}

/// A failed trading self-check freezes trading and is the only source of the
/// panel's self-check banner. Same seam, different observable state.
#[test]
fn a_failed_self_check_reaches_the_panel_through_the_view_type() {
    let mut c = dry_core();
    c.on_self_check(SelfCheckReport {
        ok: false,
        ts_ms: NOW,
        items: vec![
            SelfCheckItem {
                name: "venue_balance".into(),
                ok: false,
                detail: "HTTP 401 unauthorized".into(),
            },
            SelfCheckItem {
                name: "reconcile_sweep".into(),
                ok: true,
                detail: "ok".into(),
            },
        ],
    });

    let raw = c.engine_stats();
    assert_eq!(raw["selfCheck"]["ok"], serde_json::json!(false));

    let view = view_of(&c);
    let report = view
        .self_check
        .as_ref()
        .expect("selfCheck must survive the view type — the banner has no other source");
    assert!(!report.ok, "a failed probe must not read as a pass");
    assert_eq!(report.ts_ms, NOW, "the report keeps the kernel's instant");
    assert_eq!(report.items.len(), 2, "every probe travels, passing or not");
    assert_eq!(report.items[0].name, "venue_balance");
    assert!(!report.items[0].ok);
    assert_eq!(report.items[0].detail, "HTTP 401 unauthorized");
    assert!(report.items[1].ok, "the passing probe is kept as evidence");

    // The freeze that follows a failed self-check: the second red banner.
    let frozen = view
        .trading_frozen
        .as_ref()
        .expect("a failed self-check freezes trading");
    assert!(frozen.active);
    assert!(
        frozen
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("self-check"),
        "the freeze names the self-check as its cause"
    );
}

/// A healthy kernel must not raise the banners: `active: false` with no reason,
/// and `null` for the error slot. A view that turned "no news" into a banner
/// would be the same defect pointing the other way.
#[test]
fn a_healthy_kernel_raises_no_banner() {
    let c = dry_core();
    let view = view_of(&c);

    let frozen = view
        .trading_frozen
        .as_ref()
        .expect("the key is always sent by a kernel that has it");
    assert!(!frozen.active, "a fresh dry kernel is not frozen");
    assert_eq!(frozen.reason, None, "no reason key when trading is live");
    assert!(view.last_error.is_none(), "nothing has failed yet");
    assert!(view.last_venue_error.is_none());
    assert!(view.self_check.is_none(), "no self-check has run yet");
    assert_eq!(view.venue_rejected, 0);
}
