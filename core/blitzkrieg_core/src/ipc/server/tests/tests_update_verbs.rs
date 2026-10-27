#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

// ── VERSIONING.md §7.4: the update verbs, at the wire ────────────────────

/// The §4.4 factory readout: all nine NEW limits silent (0, "default"),
/// the exit triple at its factory resolution (12 / 100 / 120), camelCase
/// keys, and the version tag. This is the wire face of §4.1's
/// factory-silence — an unconfigured kernel REPORTS that it enforces
/// nothing new, with provenance saying where every number came from.
#[tokio::test]
async fn the_factory_risk_limits_readout_is_silent_and_provenanced() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    let r = &reply["result"];
    assert_eq!(r["version"], "1.1");
    assert_eq!(r["account"]["id"], "default");
    for key in [
        "maxSingleLossUsd",
        "maxDailyDrawdownUsd",
        "maxPositionSize",
        "maxConsecutiveLosses",
        "cooldownMinutes",
    ] {
        assert_eq!(r["account"]["limits"][key]["value"], "0", "{key}");
        assert_eq!(r["account"]["limits"][key]["source"], "default", "{key}");
    }
    for key in [
        "maxTotalPosition",
        "maxTotalExposureUsd",
        "maxCorrelationUsd",
        "globalKillSwitchLossUsd",
    ] {
        assert_eq!(r["global"]["limits"][key]["value"], "0", "{key}");
        assert_eq!(r["global"]["limits"][key]["source"], "default", "{key}");
    }
    assert_eq!(r["exit"]["stopLossPct"].as_f64(), Some(12.0));
    assert_eq!(r["exit"]["takeProfitPct"].as_f64(), Some(100.0));
    // Calibrated default: the guillotine ships OFF (`0` disables the rule).
    assert_eq!(r["exit"]["forceExitSec"].as_i64(), Some(0));
}

/// A configured limit crosses the wire as a STRING with its provenance —
/// §4.4's rule that "35" without "toml" is not an answer an operator can
/// act on. And the untouched siblings stay factory-silent in the same
/// readout: one configured key must not leak posture onto the others.
#[tokio::test]
async fn a_configured_limit_crosses_the_wire_as_a_string_with_provenance() {
    use crate::risk::limits::{AccountRiskLimits, Bound, LimitSource, SystemicRiskLimits};
    use rust_decimal_macros::dec;
    let cfg = CoreConfig {
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(3),
            systemic: SystemicRiskLimits {
                account: AccountRiskLimits {
                    max_single_loss_usd: Bound::new(dec!(35), LimitSource::Toml),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let core = Arc::new(AsyncMutex::new(Core::new(cfg)));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"risk.limits","params":{}}"#.to_string(),
    )
    .await;
    let single = &reply["result"]["account"]["limits"]["maxSingleLossUsd"];
    assert_eq!(single["value"], "35");
    assert_eq!(single["source"], "toml");
    assert_eq!(
        reply["result"]["global"]["limits"]["maxTotalPosition"]["value"],
        "0"
    );
    assert_eq!(
        reply["result"]["global"]["limits"]["maxTotalPosition"]["source"],
        "default"
    );
}

/// A2's wire shape: with the switch off, `system.update.check` answers a
/// clear APPLICATION error — and the state stays NotChecked, which is only
/// possible because the fetch future is never even polled (INV-3).
#[tokio::test]
async fn a_disabled_update_check_is_refused_by_name() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut SessionState::default(),
        r#"{"jsonrpc":"2.0","id":1,"method":"system.update.check","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "a disabled check must be refused, not silently answered: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("checkEnabled=false"),
        "the refusal must say WHY: {reply}"
    );
    assert_eq!(
        updates.snapshot().outcome,
        crate::ipc::version::CheckOutcome::NotChecked,
        "no request may have been made, not even a failed one"
    );
}

/// #379 (§7.5): the staging arm is refused BY NAME when autoUpdate is
/// off — the switch guards the download whoever asks for it, and the
/// refusal says which switch to flip (A4's shape for staging).
#[tokio::test]
async fn a_disabled_update_stage_is_refused_by_name() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut SessionState::default(),
        r#"{"jsonrpc":"2.0","id":1,"method":"system.update.stage","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "a disabled stage must be refused, not silently answered: {reply}"
    );
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("autoUpdate"),
        "the refusal must name the switch: {reply}"
    );
    assert_eq!(
        updates.stage_snapshot().phase,
        crate::ipc::version::StagePhase::Idle,
        "no download may have been started"
    );
}

/// #379: the staging reply IS the staging state machine (§7.5 wire): the
/// phase travels on the reply, and with nothing `Available` a permitted
/// call is a no-op that stays `idle` — not an error.
#[tokio::test]
async fn a_permitted_stage_with_no_available_verdict_answers_idle() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, true));
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut SessionState::default(),
        r#"{"jsonrpc":"2.0","id":1,"method":"system.update.stage","params":{}}"#.to_string(),
    )
    .await;
    let result = reply
        .get("result")
        .expect("a permitted no-op stage answers a result, not an error");
    assert_eq!(
        result["phase"], "idle",
        "no Available verdict → nothing to stage: {result}"
    );
}

/// The configure verb: the answer carries the new switch values, the state
/// cell holds them, and they LAND ON DISK (`data/update/state.json`) —
/// which is the whole point: a switch that silently reverts on restart is
/// a switch the operator believed was on. The audit line lands beside it.
#[tokio::test]
async fn update_configure_answers_and_persists() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut SessionState::default(),
        r#"{"jsonrpc":"2.0","id":1,"method":"system.update.configure","params":{"checkEnabled":true,"autoUpdate":false}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "a valid configure must succeed: {reply}"
    );
    assert_eq!(reply["result"]["checkEnabled"], serde_json::json!(true));
    assert_eq!(reply["result"]["autoUpdate"], serde_json::json!(false));
    assert!(updates.snapshot().check_enabled, "the cell must hold it");

    // The state.json the next startup will read back.
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string("data/update/state.json")
            .expect("configure must persist the switch state"),
    )
    .expect("persisted state is JSON");
    assert_eq!(doc["checkEnabled"], serde_json::json!(true));
    assert_eq!(doc["autoUpdate"], serde_json::json!(false));

    // The audit trail names the event (the actor test is uid-shape only —
    // the fixture's peer is the test process's own uid).
    let audit = std::fs::read_to_string("data/update/audit.jsonl")
        .expect("configure must append an audit line");
    assert!(audit.contains("\"event\":\"update.configure\""), "{audit}");

    // Cleanup: these two files belong to this test alone.
    let _ = std::fs::remove_dir_all("data/update");
}
