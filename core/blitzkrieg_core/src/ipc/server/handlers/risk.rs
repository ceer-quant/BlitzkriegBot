use super::super::{PeerAuth, core_err};
use super::prelude::*;
use crate::schema::*;

// The `risk` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

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
                    match core
                        .lock()
                        .await
                        .apply_systemic_limits(&p, &actor, now_ms())
                    {
                        Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
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
                         0 = the guillotine is OFF), trailingEnabled (bool)"
                ),
                None,
            )),
            Ok(p) => {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                match core.lock().await.apply_exit_set(&p, &actor, now_ms()) {
                    Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                    Err(e) => Err(core_err(e)),
                }
            }
        },
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
