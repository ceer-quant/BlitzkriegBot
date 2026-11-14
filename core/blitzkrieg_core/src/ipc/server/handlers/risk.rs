use super::super::{PeerAuth, core_err};
use super::prelude::*;
use crate::schema::*;

// The `risk` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

/// Persist a set of field→value changes to the kernel's store (issue #966;
/// #1199: the durable half runs through the store seam). Skipped silently
/// when the core has no store (backtester, tests that don't wire one) — the
/// hot edit still lives in memory for this process. A write failure is
/// logged but does NOT fail the RPC: the in-memory apply already succeeded;
/// the operator sees the change immediately and the log tells them
/// persistence failed.
fn persist_changes_to_env(c: &Core, account_id: Option<&str>, changes: &[(&str, String)]) -> bool {
    let store = match c.store.as_deref() {
        Some(s) => s,
        None => return false,
    };
    match store.save_risk_bounds(account_id, &field_changes(changes)) {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            tracing::warn!(
                "risk.setSystemic: persistence to the store failed ({e}); the in-memory \
                 change is live for this process but will NOT survive a restart"
            );
            false
        }
    }
}

/// Same as [`persist_changes_to_env`] but for exit-discipline field names.
fn persist_exit_changes_to_env(
    c: &Core,
    account_id: Option<&str>,
    changes: &[(&str, String)],
) -> bool {
    let store = match c.store.as_deref() {
        Some(s) => s,
        None => return false,
    };
    match store.save_exit(account_id, &field_changes(changes)) {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            tracing::warn!(
                "risk.setExit: persistence to the store failed ({e}); the in-memory \
                 change is live for this process but will NOT survive a restart"
            );
            false
        }
    }
}

/// The wire audit's `(&'static str, String)` pairs to the store's owned
/// `(String, String)` field list.
fn field_changes(changes: &[(&str, String)]) -> Vec<(String, String)> {
    changes
        .iter()
        .map(|(f, v)| (f.to_string(), v.clone()))
        .collect()
}

/// Serialize an apply reply with `persisted` + `persistence` on top (issue
/// #966, receipt contract completed in #965). The panel shows "已持久化到
/// .env" only when `persisted` is true; an unwired env path (a backtester, or
/// a test that does not wire one) leaves the in-memory edit live for this
/// process only, and saying so is the honest answer. `persistence` spells the
/// carrier in words — `"env"` when the upsert landed, `"none"` when it did
/// not — so a receipt never just says "已保存" without WHERE it went.
fn with_persisted(update: serde_json::Value, persisted: bool) -> Value {
    let mut reply = update;
    if let Value::Object(map) = &mut reply {
        map.insert("persisted".into(), Value::Bool(persisted));
        map.insert(
            "persistence".into(),
            Value::String(if persisted { "env" } else { "none" }.to_string()),
        );
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
            // File-sourced (operator ruling 2026-11-08): the cockpit sections
            // of the readout come from the `.env` file at QUERY time — the
            // panel reads the file (through the kernel), never a cache that
            // can diverge from it (the 2026-11-08 panel-ON/file-OFF class).
            //
            // issue #1199 (the readout asymmetry): with the SQLite store open
            // the DB is the cockpit carrier — the working copies already carry
            // the DB rows (boot overlay + the default-face mirror in
            // `open_account_risk_db`), and an env overlay here would resurrect
            // the file's STALE `BK_*` / `BK_EXIT_*` keys over them on every
            // query. The overlay is the env-carrier backend's readout face
            // only; with the DB open the snapshot IS the answer.
            let file = if c.account_risk_db_open() {
                None
            } else {
                c.cockpit_env_path()
                    .and_then(|p| crate::env_file::read_env_vars(p).ok())
            };
            let mut doc = crate::schema::RiskLimitsResult::snapshot(
                &c.risk_config().systemic,
                &c.config().positions.exit,
            );
            if let Some(vars) = &file {
                crate::cockpit::overlay_risk_limits(vars, &mut doc.exit, &mut doc.account);
            }
            Ok(serde_json::to_value(doc).unwrap_or(Value::Null))
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
                    // The apply's own lock is scoped to the call (no
                    // `core.lock()` inside a `match` scrutinee — that would
                    // re-lock the same mutex while the guard is alive).
                    let applied = core
                        .lock()
                        .await
                        .apply_systemic_limits(&p, &actor, now_ms());
                    match applied {
                        Ok(update) => {
                            // #966: persist every changed bound so a restart
                            // re-arms the same values. The in-memory apply
                            // already succeeded; a persistence failure is
                            // logged but does not fail the RPC. #1199: the
                            // durable half goes through the store seam.
                            let changes: Vec<(&str, String)> = update
                                .applied
                                .iter()
                                .map(|c| (c.field, c.to.clone()))
                                .collect();
                            let persisted = if update.persisted {
                                true
                            } else {
                                let c = core.lock().await;
                                persist_changes_to_env(&c, p.account_id.as_deref(), &changes)
                            };
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
                // The apply's own lock is scoped to the call (no
                // `core.lock()` inside a `match` scrutinee — that would
                // re-lock the same mutex while the guard is alive).
                let applied = core.lock().await.apply_exit_set(&p, &actor, now_ms());
                match applied {
                    Ok(update) => {
                        // #966: persist the exit-discipline changes. #1199:
                        // the durable half goes through the store seam.
                        let changes: Vec<(&str, String)> = update
                            .applied
                            .iter()
                            .map(|c| (c.field, c.to.clone()))
                            .collect();
                        let persisted = if update.persisted {
                            true
                        } else {
                            let c = core.lock().await;
                            persist_exit_changes_to_env(&c, p.account_id.as_deref(), &changes)
                        };
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
