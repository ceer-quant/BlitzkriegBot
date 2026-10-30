use super::super::{PeerAuth, SessionState, effective_default_account, typed};
use super::prelude::*;
use crate::ipc::schema::*;

// The `system` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_system(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    peer: &PeerAuth,
    update_state: &Arc<crate::ipc::version::UpdateState>,
    session: &mut SessionState,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::SYSTEM_VERSION => Ok(serde_json::to_value(
            crate::ipc::version::system_version_payload(
                crate::ipc::build_info::BUILD_INFO,
                &update_state.snapshot(),
            ),
        )
        .unwrap_or(Value::Null)),
        method::SYSTEM_ENV_RELOAD => {
            let mut c = core.lock().await;
            let accounts = c.reload_accounts();
            let policy = c.reload_execution_policy();
            let shadow = c.reload_shadow();
            let failed = accounts.is_err() || policy.is_err() || shadow.is_err();
            let result = serde_json::json!({
                "ok": !failed,
                "accounts": accounts.err(),
                "policy": policy.err(),
                "shadow": shadow.err(),
                "runtime": "restart required",
            });
            Ok(result)
        }
        method::SYSTEM_UPDATE_CONFIGURE => {
            match serde_json::from_value::<crate::ipc::version::UpdateConfigureParams>(
                params.clone(),
            ) {
                Err(e) => Err((Failure::INVALID_PARAMS, format!("{e}"), None)),
                Ok(req) => {
                    // Audit: who turned "may auto-replace binaries" on. A
                    // security-relevant state change, recorded at risk.kill's
                    // level; the actor is the kernel-recorded peer uid, not a
                    // caller's claim.
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    update_state.set_enabled(req.check_enabled, req.auto_update);
                    match crate::ipc::version::persist(update_state) {
                        // A switch that did not land on disk silently reverts on
                        // restart while the operator believes it was on — so the
                        // caller MUST hear about it.
                        Err(e) => Err((
                            Failure::APPLICATION,
                            format!("update state not persisted: {e}"),
                            None,
                        )),
                        Ok(()) => {
                            let s = update_state.snapshot();
                            crate::ipc::version::audit_event(&serde_json::json!({
                                "ts": now_ms(),
                                "event": "update.configure",
                                "actor": actor,
                                "checkEnabled": s.check_enabled,
                                "autoUpdate": s.auto_update,
                            }));
                            Ok(serde_json::json!({
                                "checkEnabled": s.check_enabled,
                                "autoUpdate": s.auto_update,
                            }))
                        }
                    }
                }
            }
        }
        method::SYSTEM_UPDATE_CHECK => {
            // The manual check is ALSO bound by check_enabled: the switch means
            // "may go out at all", not "may go out automatically". A disabled
            // button answers clearly instead of sneaking a request out.
            if !update_state.snapshot().check_enabled {
                Err((
                    Failure::APPLICATION,
                    "update checking is disabled (checkEnabled=false); enable it in \
                     user_layer/configs/update.toml or via system.update.configure"
                        .to_string(),
                    None,
                ))
            } else {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                let local = crate::ipc::build_info::BUILD_INFO.version.to_string();
                let token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
                    .ok()
                    .filter(|t| !t.trim().is_empty());
                // Ok and Err are both values, never escapes: the fetch result is
                // consumed by check_once, which writes whatever happened into the
                // state cell. The verdict is PERSISTED here too (check_and_persist):
                // the manual arm historically skipped the disk write, so a failed
                // check evaporated on restart while the state file kept
                // `lastCheckMs: 0` forever.
                crate::ipc::version::check_and_persist(
                    update_state,
                    &local,
                    now_ms() as u64,
                    crate::ipc::version::fetch_latest(token),
                )
                .await;
                let s = update_state.snapshot();
                // The failure DETAIL rides the audit line: an operator reading
                // data/update/audit.jsonl must be able to tell "proxy path down"
                // from "rate limited" without reproducing the check by hand.
                let failure_detail = match &s.outcome {
                    crate::ipc::version::CheckOutcome::Failed { detail } => Some(detail.clone()),
                    _ => None,
                };
                crate::ipc::version::audit_event(&serde_json::json!({
                    "ts": now_ms(),
                    "event": "update.check",
                    "actor": actor,
                    "outcome": s.outcome.token(),
                    "latest": s.latest,
                    "detail": failure_detail,
                }));
                Ok(
                    serde_json::to_value(crate::ipc::version::system_version_payload(
                        crate::ipc::build_info::BUILD_INFO,
                        &s,
                    ))
                    .unwrap_or(Value::Null),
                )
            }
        }
        method::SYSTEM_UPDATE_STAGE => {
            if !crate::ipc::version::stage_enabled(update_state) {
                Err((
                    Failure::APPLICATION,
                    "update staging is disabled (autoUpdate=false); enable it via \
                     system.update.configure"
                        .to_string(),
                    None,
                ))
            } else {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                let local_target = crate::ipc::build_info::BUILD_INFO.target.to_string();
                let token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
                    .ok()
                    .filter(|t| !t.trim().is_empty());
                let staging = crate::ipc::version::staging_dir();
                let fetch_target = local_target.clone();
                crate::ipc::version::stage_once(
                    update_state,
                    &local_target,
                    now_ms() as u64,
                    &staging,
                    move |tag: String, version: String| async move {
                        crate::ipc::version::fetch_release_assets(
                            &tag,
                            &version,
                            &fetch_target,
                            token,
                        )
                        .await
                    },
                )
                .await;
                let stage = update_state.stage_snapshot();
                crate::ipc::version::audit_event(&serde_json::json!({
                    "ts": now_ms(),
                    "event": "update.stage",
                    "actor": actor,
                    "phase": stage.phase.token(),
                    "version": stage.version,
                    "asset": stage.asset,
                }));
                Ok(serde_json::to_value(&stage).unwrap_or(Value::Null))
            }
        }
        method::SYSTEM_RESET_TRADING => {
            let explicit_account = params.get("accountId").is_some();
            typed(params, |p: ResetTradingParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let mut p = p;
                    if !explicit_account {
                        let c = core.lock().await;
                        p.account_id = Some(
                            effective_default_account(session, c.accounts())
                                .as_str()
                                .to_string(),
                        );
                    }
                    let result = core.lock().await.reset_trading(&p, &actor, now_ms())?;
                    Ok(serde_json::to_value(result).unwrap_or(Value::Null))
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
