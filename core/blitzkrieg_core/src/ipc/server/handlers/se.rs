use super::prelude::*;
use crate::schema::*;

// The `se` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_se(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::SE_ENABLE => {
            let now = now_ms();
            core.lock().await.shadow_evolution_enable(now);
            Ok(serde_json::json!({ "enabled": true }))
        }
        method::SE_DISABLE => {
            core.lock().await.shadow_evolution_disable();
            Ok(serde_json::json!({ "enabled": false }))
        }
        method::SE_STATUS => {
            let now = now_ms();
            let mut c = core.lock().await;
            let status = c.shadow_evolution_status(now);
            let params = c.shadow_evolution().current_params();
            let variants = c.shadow_evolution_variants(now);
            let variant_count = c.shadow_evolution().variant_count();
            // Per-strategy blocks. Isolation is only observable if the caller can
            // see each strategy's own counters and knob values side by side.
            let strategies: Vec<serde_json::Value> = c
                .shadow_evolution()
                .strategy_names()
                .into_iter()
                .map(|name| {
                    let st = c.shadow_evolution_status_for(&name, now);
                    serde_json::json!({
                        "strategy": name,
                        "status": st,
                        "params": c.shadow_evolution().params_for(&name),
                        "knobs": c.shadow_evolution().declared_knobs(&name),
                        "evolutionsApplied": c.shadow_evolution().evolution_count(&name),
                        "evolutionsRejected": c.shadow_evolution().rejected_count(&name),
                        "secondsSinceLastEvolution": c
                            .shadow_evolution()
                            .seconds_since_last_evolution(&name, now),
                    })
                })
                .collect();
            let evolved: u64 = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().evolution_count(n))
                .sum();
            let rejected: u64 = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().rejected_count(n))
                .sum();
            let since = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().seconds_since_last_evolution(n, now))
                .filter(|s| *s >= 0)
                .min()
                .unwrap_or(-1);
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                // Aggregate keys kept unchanged so an existing Node consumer keeps
                // working; `strategies` carries the per-strategy detail.
                "status": status,
                "currentParams": params,
                "variantCount": variant_count,
                "variants": variants,
                "evolutionsApplied": evolved,
                "evolutionsRejected": rejected,
                "secondsSinceLastEvolution": since,
                "strategies": strategies,
                // E13: the unattended switch + the 72h deep clock, so a UI can
                // render the checkbox and the next-round time from one call.
                "autoEvolve": c.shadow_evolution().auto_evolve(),
                "lastCycleMs": c.shadow_evolution().cycle_info().0,
                "nextCycleAtMs": c.shadow_evolution().next_cycle_at_ms(now),
                "pendingProposals": c.shadow_evolution().pending_proposal_count(),
                // #249: the ENGINE switch is a different thing from the auto
                // switch, and without it on the wire a UI cannot tell "nothing
                // qualified yet" from "nothing is even running" — the confusion
                // that made an auto-evolve label look like a lie.
                "enabled": c.shadow_evolution().is_enabled(),
                "cycleSeq": c.shadow_evolution().cycle_info().1,
                "cycleSecs": c.shadow_evolution().cycle_secs(),
                // The config FILE is the single source of truth for the
                // switches and the switch verbs write back into it, so a
                // file/runtime disagreement cannot exist anymore. The key
                // stays on the wire as an always-empty list: an older WebUI
                // that still renders the (removed) conflict banner reads `[]`
                // and shows nothing, instead of failing to parse a missing key.
                "switchConflicts": [],
            }))
        }
        method::SE_HISTORY => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let strategy = params
                .get("strategy")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let records = core
                .lock()
                .await
                .shadow_evolution_history(strategy.as_deref(), limit);
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "strategy": strategy,
                "history": records,
            }))
        }
        method::SE_ROLLBACK => {
            let now = now_ms();
            match params
                .get("strategy")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                None => Err((Failure::INVALID_PARAMS, "strategy required".into(), None)),
                Some(strategy) => {
                    match core.lock().await.shadow_evolution_rollback(strategy, now) {
                        Ok(_) => {
                            Ok(serde_json::json!({ "rolledBack": true, "strategy": strategy }))
                        }
                        Err(e) => Err((Failure::APPLICATION, e, None)),
                    }
                }
            }
        }
        method::SE_APPLY => {
            match params
                .get("strategy")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                // Manual override for ONE strategy. Shape: {strategy, params:{knob:value}},
                // validated against that strategy's own declaration + domain + gradient.
                None => Err((Failure::INVALID_PARAMS, "strategy required".into(), None)),
                Some(strategy) => {
                    let bag = params
                        .get("params")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    match serde_json::from_value::<crate::shadow_evolution::StrategyParams>(bag) {
                        Err(e) => Err((Failure::INVALID_PARAMS, format!("params: {e}"), None)),
                        Ok(typed) => {
                            let mut bag = crate::shadow_evolution::MutableParams::new();
                            bag.set_strategy(strategy, typed);
                            match core.lock().await.shadow_evolution_apply(bag, now_ms()) {
                                Ok(()) => {
                                    Ok(serde_json::json!({ "applied": true, "strategy": strategy }))
                                }
                                Err(e) => Err((Failure::APPLICATION, e, None)),
                            }
                        }
                    }
                }
            }
        }
        method::SE_PROPOSALS => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let proposals = core.lock().await.shadow_evolution_proposals(limit);
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "proposals": proposals,
            }))
        }
        method::SE_DECIDE => {
            match (
                params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty()),
                params
                    .get("decision")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty()),
            ) {
                (None, _) => Err((Failure::INVALID_PARAMS, "id required".into(), None)),
                (_, None) => Err((
                    Failure::INVALID_PARAMS,
                    "decision required (accept|reject|defer)".into(),
                    None,
                )),
                (Some(id), Some(decision)) => {
                    let now = now_ms();
                    match core.lock().await.shadow_evolution_decide(id, decision, now) {
                        Ok(result) => Ok(match result {
                            crate::shadow_evolution::DecisionResult::Accepted { proposal } => {
                                serde_json::json!({ "decision": "accepted", "proposal": proposal })
                            }
                            crate::shadow_evolution::DecisionResult::Rejected { proposal } => {
                                serde_json::json!({ "decision": "rejected", "proposal": proposal })
                            }
                            crate::shadow_evolution::DecisionResult::Deferred { proposal } => {
                                serde_json::json!({ "decision": "deferred", "proposal": proposal })
                            }
                        }),
                        Err(e) => Err((Failure::APPLICATION, e, None)),
                    }
                }
            }
        }
        method::SE_SET_AUTO => match params.get("enabled").and_then(|v| v.as_bool()) {
            None => Err((
                Failure::INVALID_PARAMS,
                "enabled (bool) required".into(),
                None,
            )),
            Some(on) => {
                let auto = core.lock().await.shadow_evolution_set_auto(on);
                Ok(serde_json::json!({ "autoEvolve": auto }))
            }
        },
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
