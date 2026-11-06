use super::super::{PeerAuth, core_err};
use super::prelude::*;
use crate::schema::*;

// The `risk` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

/// Map a systemic-limit field name (the camelCase wire spelling) to its
/// `.env` key so `risk.setSystemic` can persist the hot edit across a restart
/// (issue #966). Only the nine `SystemicRiskLimits` fields are mapped — any
/// other field name returns `None` and is skipped (a future field added to
/// the wire without an env key must not crash the write).
fn systemic_env_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "maxSingleLossUsd" => "BK_RISK_MAX_SINGLE_LOSS_USD",
        "maxDailyDrawdownUsd" => "BK_RISK_MAX_DAILY_DRAWDOWN_USD",
        "maxPositionSize" => "BK_RISK_MAX_POSITION_SIZE",
        "maxConsecutiveLosses" => "BK_RISK_MAX_CONSECUTIVE_LOSSES",
        "cooldownMinutes" => "BK_RISK_COOLDOWN_MINUTES",
        "maxTotalPosition" => "BK_RISK_MAX_TOTAL_POSITION",
        "maxTotalExposureUsd" => "BK_RISK_MAX_TOTAL_EXPOSURE_USD",
        "maxCorrelationUsd" => "BK_RISK_MAX_CORRELATION_USD",
        "globalKillSwitchLossUsd" => "BK_RISK_GLOBAL_KILL_SWITCH_LOSS_USD",
        _ => return None,
    })
}

/// Map an exit-discipline field name to its `.env` key (issue #966). The
/// five `risk.setExit` fields map to `BK_EXIT_*` keys; trailing-enabled and
/// strategy-exit-maker-first gained env support in #966 so a hot edit
/// survives a restart.
fn exit_env_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "stopLossPct" => "BK_EXIT_STOP_LOSS_PCT",
        "takeProfitPct" => "BK_EXIT_TAKE_PROFIT_PCT",
        "forceExitSec" => "BK_EXIT_FORCE_EXIT_SEC",
        "trailingEnabled" => "BK_EXIT_TRAILING_ENABLED",
        "strategyExitMakerFirst" => "BK_EXIT_STRATEGY_EXIT_MAKER_FIRST",
        _ => return None,
    })
}

/// Persist a set of field→value changes to the kernel's `.env` file (issue
/// #966). Skipped silently when the core has no `env_file_path` (backtester,
/// tests that don't wire one) — the hot edit still lives in memory for this
/// process. A write failure is logged but does NOT fail the RPC: the in-memory
/// apply already succeeded; the operator sees the change immediately and the
/// log tells them persistence failed.
fn persist_changes_to_env(
    env_path: Option<&std::path::Path>,
    changes: &[(&'static str, String)],
) -> bool {
    let Some(env_path) = env_path else {
        return false;
    };
    let vars: std::collections::BTreeMap<String, String> = changes
        .iter()
        .filter_map(|(field, value)| Some((systemic_env_key(field)?.to_string(), value.clone())))
        .collect();
    if vars.is_empty() {
        return false;
    }
    if let Err(e) = crate::env_file::upsert_env_vars(env_path, &vars) {
        tracing::warn!(
            "risk.setSystemic: persistence to {} failed ({e}); the in-memory \
             change is live for this process but will NOT survive a restart",
            env_path.display()
        );
        return false;
    }
    true
}

/// Same as [`persist_changes_to_env`] but for exit-discipline field names.
fn persist_exit_changes_to_env(
    env_path: Option<&std::path::Path>,
    changes: &[(&'static str, String)],
) -> bool {
    let Some(env_path) = env_path else {
        return false;
    };
    let vars: std::collections::BTreeMap<String, String> = changes
        .iter()
        .filter_map(|(field, value)| Some((exit_env_key(field)?.to_string(), value.clone())))
        .collect();
    if vars.is_empty() {
        return false;
    }
    if let Err(e) = crate::env_file::upsert_env_vars(env_path, &vars) {
        tracing::warn!(
            "risk.setExit: persistence to {} failed ({e}); the in-memory \
             change is live for this process but will NOT survive a restart",
            env_path.display()
        );
        return false;
    }
    true
}

/// Serialize an apply reply with `persisted` on top (issue #966). The panel
/// shows "已持久化到 .env" only when this is true; an unwired env path (a
/// backtester, or a test that does not wire one) leaves the in-memory edit live
/// for this process only, and saying so is the honest answer.
fn with_persisted(update: serde_json::Value, persisted: bool) -> Value {
    let mut reply = update;
    if let Value::Object(map) = &mut reply {
        map.insert("persisted".into(), Value::Bool(persisted));
    }
    reply
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_risk(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    peer: &PeerAuth,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::RISK_LIMITS => {
            let c = core.lock().await;
            Ok(
                serde_json::to_value(crate::schema::RiskLimitsResult::snapshot(
                    &c.risk_config().systemic,
                    &c.config().positions.exit,
                ))
                .unwrap_or(Value::Null),
            )
        }
        method::RISK_KILL => {
            let reason = params
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("manual")
                .to_string();
            core.lock().await.kill(reason);
            Ok(serde_json::json!({ "killed": true }))
        }
        method::RISK_RESUME => {
            core.lock().await.resume();
            Ok(serde_json::json!({ "killed": false }))
        }
        method::RISK_SET_LIMITS => {
            if let Some((key, why)) = params.as_object().and_then(|obj| {
                obj.keys()
                    .find_map(|k| crate::risk::hot_reload_refusal(k).map(|why| (k.as_str(), why)))
            }) {
                Err((
                    Failure::INVALID_PARAMS,
                    format!(
                        "{key} is not hot-reloadable: {why}; a restart applies it. \
                         hot-reloadable: {}",
                        crate::risk::HOT_RELOADABLE.join(", ")
                    ),
                    None,
                ))
            } else {
                match serde_json::from_value::<SetRiskLimitsParams>(params.clone()) {
                    Err(e) => Err((
                        Failure::INVALID_PARAMS,
                        format!(
                            "{e} — nothing was applied; hot-reloadable: {}",
                            crate::risk::HOT_RELOADABLE.join(", ")
                        ),
                        None,
                    )),
                    Ok(p) => {
                        // The actor is the peer's KERNEL-recorded uid, not a claim
                        // by the caller: the audit's "who" has to survive a client
                        // that would say anything it liked.
                        let actor = match peer {
                            PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                            _ => "uid:unknown".to_string(),
                        };
                        match core.lock().await.apply_risk_limits(&p, &actor, now_ms()) {
                            Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                            Err(e) => Err(core_err(e)),
                        }
                    }
                }
            }
        }
        method::RISK_SET_SYSTEMIC => {
            match serde_json::from_value::<SetSystemicLimitsParams>(params.clone()) {
                Err(e) => Err((
                    Failure::INVALID_PARAMS,
                    format!(
                        "{e} — nothing was applied; the nine systemic fields are \
                         maxSingleLossUsd, maxDailyDrawdownUsd, maxPositionSize, \
                         maxConsecutiveLosses, cooldownMinutes, maxTotalPosition, \
                         maxTotalExposureUsd, maxCorrelationUsd, globalKillSwitchLossUsd"
                    ),
                    None,
                )),
                Ok(p) => {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    // Hoisted above the `match` for the same reason as
                    // `RISK_SET_EXIT`: no `core.lock()` may run inside an arm
                    // whose scrutinee still holds the guard.
                    let env_file = core.lock().await.config().env_file_path.clone();
                    match core
                        .lock()
                        .await
                        .apply_systemic_limits(&p, &actor, now_ms())
                    {
                        Ok(update) => {
                            // #966: persist every changed bound to `.env` so a
                            // restart re-arms the same values. The in-memory
                            // apply already succeeded; a persistence failure is
                            // logged but does not fail the RPC.
                            let env_path = env_file.as_deref().map(std::path::Path::new);
                            let changes: Vec<(&'static str, String)> = update
                                .applied
                                .iter()
                                .map(|c| (c.field, c.to.clone()))
                                .collect();
                            let persisted = persist_changes_to_env(env_path, &changes);
                            Ok(with_persisted(
                                serde_json::to_value(update).unwrap_or(Value::Null),
                                persisted,
                            ))
                        }
                        Err(e) => Err(core_err(e)),
                    }
                }
            }
        }
        method::RISK_SET_EXIT => match serde_json::from_value::<SetExitParams>(params.clone()) {
            Err(e) => Err((
                Failure::INVALID_PARAMS,
                format!(
                    "{e} — nothing was applied; the exit fields are \
                         stopLossPct (percent of entry price, > 0), takeProfitPct \
                         (percent of entry price, > 0), forceExitSec (seconds, \
                         0 = the guillotine is OFF), trailingEnabled (bool), \
                         strategyExitMakerFirst (bool)"
                ),
                None,
            )),
            Ok(p) => {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                // Read the env path BEFORE taking the lock for the apply:
                // a `core.lock().await` inside the match arm would re-lock the
                // same mutex while the scrutinee's guard is still alive (the
                // guard lives to the end of the `match`), deadlocking the RPC.
                let env_file = core.lock().await.config().env_file_path.clone();
                match core.lock().await.apply_exit_set(&p, &actor, now_ms()) {
                    Ok(update) => {
                        // #966: persist the exit-discipline changes to `.env`.
                        let env_path = env_file.as_deref().map(std::path::Path::new);
                        let changes: Vec<(&'static str, String)> = update
                            .applied
                            .iter()
                            .map(|c| (c.field, c.to.clone()))
                            .collect();
                        let persisted = persist_exit_changes_to_env(env_path, &changes);
                        Ok(with_persisted(
                            serde_json::to_value(update).unwrap_or(Value::Null),
                            persisted,
                        ))
                    }
                    Err(e) => Err(core_err(e)),
                }
            }
        },
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
