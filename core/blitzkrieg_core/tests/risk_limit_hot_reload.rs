//! P2 #191 — the ISSUE-level behaviour of the limited hot reload, not the unit.
//!
//! `risk.setLimits` is allowed to move the ENTRY limits and nothing else. Two
//! promises of that boundary are pinned here, through the public core API the
//! kernel itself uses:
//!
//! * the change is IN MEMORY ONLY, so a restart falls back to the startup
//!   flags — an operator who tightened a cap must not be left believing it
//!   survived the restart (and a `Core` built from the same startup config must
//!   not accidentally inherit it);
//! * a patch that cannot be applied is refused WHOLE, so a bad field never
//!   leaves a half-applied limit behind (the same atomicity the RPC refusal
//!   promises, seen from the core side).
//!
//! The wire-level half — the audit record, the named refusal and the next order
//! being judged by the new value — lives in `ipc::server`'s tests, next to the
//! dispatch it polices.

use blitzkrieg_core::ipc::schema::SetRiskLimitsParams;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal_macros::dec;

/// The startup flags an operator would have: a 100 USD per-order cap and the
/// shipped [10, 10] share band.
///
/// Every log path is None on purpose: these tests are about config, and a test
/// must never write a durable order/position log into whatever checkout it is
/// run from.
fn startup_config() -> CoreConfig {
    CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    }
}

/// A core started exactly as `serve` starts one: the engine installed from the
/// startup config (so its sizing is the boot-time COPY, not a live view).
fn booted(cfg: &CoreConfig) -> Core {
    let mut c = Core::new(cfg.clone());
    cfg.install_engine(&mut c);
    c
}

/// The numbers one entry can commit, as the panel reads them.
fn sizing(c: &Core) -> serde_json::Value {
    c.engine_stats()["sizing"].clone()
}

/// #191 counter-test: the hot change is memory-only. The LIVE core takes it (the
/// engine's own boot-time copy included), while the startup config object stays
/// untouched — so a restart, which is a new core built from those same flags,
/// comes back at the startup values.
#[test]
fn a_hot_limit_change_is_memory_only_and_a_restart_falls_back_to_the_startup_flags() {
    let cfg = startup_config();
    let mut live = booted(&cfg);
    assert_eq!(
        sizing(&live)["maxOrderNotionalUsd"],
        serde_json::json!(100.0)
    );
    assert_eq!(sizing(&live)["minShares"], serde_json::json!(10.0));

    let update = live
        .apply_risk_limits(
            &SetRiskLimitsParams {
                max_order_notional: Some(dec!(5)),
                min_shares: Some(dec!(2)),
                max_shares: Some(dec!(4)),
                reason: Some("tighten after a drawdown".into()),
                ..Default::default()
            },
            "uid:501",
            1_000,
        )
        .expect("the safe subset must apply");
    assert_eq!(update.applied.len(), 3);
    assert!(
        !update.persisted,
        "the reply must say the change is not durable"
    );

    // In force NOW, on the engine's own copy as well as the gate's cap.
    assert_eq!(sizing(&live)["maxOrderNotionalUsd"], serde_json::json!(5.0));
    assert_eq!(sizing(&live)["minShares"], serde_json::json!(2.0));
    assert_eq!(sizing(&live)["maxShares"], serde_json::json!(4.0));

    // The STARTUP config object was not written back to: nothing to persist, so
    // there is nothing for a restart to pick up.
    assert_eq!(cfg.risk.max_order_notional, dec!(100));
    assert_eq!(cfg.min_shares, dec!(10));
    assert_eq!(cfg.max_shares, dec!(10));

    // A restart — a new core from the same flags — is back at the startup values.
    let restarted = booted(&cfg);
    assert_eq!(
        sizing(&restarted)["maxOrderNotionalUsd"],
        serde_json::json!(100.0),
        "a restart must re-apply the startup cap, not the hot-changed one"
    );
    assert_eq!(sizing(&restarted)["minShares"], serde_json::json!(10.0));
    assert_eq!(sizing(&restarted)["maxShares"], serde_json::json!(10.0));
}

/// #191: a patch the core cannot apply is refused WHOLE. A negative cap and an
/// inverted share band are both refused at the door, and the LIVE limits are
/// left exactly as they were — no half-applied patch, which is what makes a
/// refusal safe to answer with an error.
#[test]
fn an_unapplicable_patch_is_refused_whole_and_moves_nothing() {
    let cfg = startup_config();
    let mut c = booted(&cfg);
    let before = sizing(&c);

    let negative = c.apply_risk_limits(
        &SetRiskLimitsParams {
            max_order_notional: Some(dec!(-1)),
            ..Default::default()
        },
        "uid:501",
        1_000,
    );
    let err = negative.expect_err("a negative cap is not a limit");
    assert!(
        err.message.contains("maxOrderNotional") && err.message.contains(">= 0"),
        "the refusal must name the field and the rule: {}",
        err.message
    );

    let inverted = c.apply_risk_limits(
        &SetRiskLimitsParams {
            min_shares: Some(dec!(20)),
            max_shares: Some(dec!(4)),
            ..Default::default()
        },
        "uid:501",
        1_000,
    );
    let err = inverted.expect_err("an inverted band is not a band");
    assert!(
        err.message.contains("minShares 20") && err.message.contains("maxShares 4"),
        "the refusal must show both ends of the range: {}",
        err.message
    );

    // An empty patch is a no-op the caller must be told about, not a silent
    // success that looks like a change.
    let empty = c.apply_risk_limits(&SetRiskLimitsParams::default(), "uid:501", 1_000);
    let err = empty.expect_err("an empty patch changes nothing and must say so");
    assert!(
        err.message.contains("at least one field"),
        "the refusal must say what is missing: {}",
        err.message
    );

    assert_eq!(
        sizing(&c),
        before,
        "no refused patch may leave a limit moved"
    );
}
