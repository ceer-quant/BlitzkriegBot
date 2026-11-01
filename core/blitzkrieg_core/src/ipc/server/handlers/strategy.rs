use super::super::{PeerAuth, client_err, core_err};
use super::prelude::*;
use crate::schema::*;

// The `strategy` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_strategy(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    peer: &PeerAuth,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::STRATEGY_LIST => {
            let c = core.lock().await;
            let enabled = c.enabled_strategy_names();
            let strategies: Vec<_> = c
                .strategy_names()
                .into_iter()
                .map(|name| {
                    let is_on = enabled.contains(&name);
                    // E27 (§8.3): the row carries the strategy's declared modes
                    // as the §2.3 wire objects (`null` = undeclared), the
                    // compatible flag, and the refusal reason when the
                    // handshake would refuse it. Every 0.2 strategy reads as
                    // `modes: null, compatible: true, incompatibleReason: null`.
                    let (modes, compatible, incompatible_reason) = c.strategy_compat_view(&name);
                    // 账户↔venue 解耦（本轮）：每行还带 `accounts` —— 该策略
                    // 当前在哪些账户 scope 上有实例（engine 的注册表读数，
                    // 排序稳定）。单 scope 部署 = `["default"]`；双账户都跑
                    // spread_arb 时两行同名列各自带出真实归属，面板的策略×
                    // 账户矩阵不再靠猜。老核不写字段 → 前端按缺省降级。
                    //
                    // `enabled` 口径（issue 552 审计）：这是 master-power 位
                    // —— 默认 scope 的全局开关，**不是** (账户,策略) 复合键
                    // 的真值。复合真值在 `accounts`（有 scope = 该账户的启
                    // 用集点名了它）∪ accounts.toml 启用集；面板的策略页启
                    // 用按钮按「账户+策略」读写（feature-detect），不直接
                    // 消费这个位。is_on 只在单账户/老面板降级面上近似正确。
                    let accounts = c
                        .engine()
                        .map(|e| {
                            let mut v = e.strategy_scopes(&name);
                            v.sort();
                            v
                        })
                        .unwrap_or_default();
                    serde_json::json!({
                        "name": name,
                        "enabled": is_on,
                        "modes": modes,
                        "compatible": compatible,
                        "incompatibleReason": incompatible_reason,
                        "accounts": accounts,
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "strategies": strategies,
            }))
        }
        method::STRATEGY_ENABLE => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = params
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let account_id = params
                .get("accountId")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("default");
            if account_id == "default" {
                // E27 (§8.2): the enable runs through the handshake. A
                // disabling toggle is never gated (standing down is always
                // allowed); an enable the active plugin cannot serve is
                // REFUSED with the §8.2 diagnostic —
                // `{ name, enabled: false, found: true, reason }` — and the
                // dispatch keeps the strategy off.
                let (mut found, mut applied, reason) = core
                    .lock()
                    .await
                    .set_strategy_enabled_checked(name, enabled);
                // #537: on a successful toggle the default account's
                // enablement set must move WITH it — the boot applies the
                // accounts file AFTER the state file and turns every scope
                // the file names back ON unconditionally, so a toggle that
                // moved only the engine flag resurrected at the next boot
                // and a strategy the file named could never be turned off
                // for good. Same persisting write the non-default branch
                // runs; the not-found and refusal replies below keep their
                // exact legacy bytes.
                //
                // Bugfix (2026-10, 0.4.1): the legacy global toggle reads the
                // engine's LIVE dispatch. A boot whose accounts.toml gives
                // every account an empty (or absent) enablement set
                // instantiates NO scopes — `apply_account_strategy_assignments_
                // at_boot` only builds scopes for non-empty sets — so the
                // toggle finds no instance (`applied == false`) even though
                // the package is registered, and gating the mirror on
                // `applied` left the default account unable to enable
                // anything from that shape: the first enable worked (the boot
                // had registered 11 disabled default scopes), its own disable
                // then tore the scope down, and every later enable in that
                // process silently no-oped (`found: true, applied: false,
                // reason: null`, nothing persisted, nothing rebuilt). The
                // scoped write below owns the enablement semantics now, so it
                // runs whenever the toggle did not REFUSE — and its success
                // is the honest `applied: true` for a name the live dispatch
                // had simply never hosted.
                let mut mirror_error: Option<(i32, String, Option<ErrorData>)> = None;
                let mut kept_exits_only: Vec<String> = Vec::new();
                // Gate-sandboxes run with no accounts.toml on disk at all —
                // there "no file = persistence disabled" (the boot rule this
                // mirror inherits) and the toggle must keep its legacy
                // engine-only success. Only a path that EXISTS but refuses
                // the write is an error.
                let mirror_persistence = core
                    .lock()
                    .await
                    .config()
                    .accounts_config_path
                    .as_deref()
                    .is_some_and(|p| std::path::Path::new(p).exists());
                if reason.is_none() && mirror_persistence {
                    let default_id = crate::account::default_account_id();
                    let mut c = core.lock().await;
                    match c.set_scoped_strategy_enabled(
                        &default_id,
                        name,
                        enabled,
                        // Same actor posture as the other panel writes: the
                        // KERNEL-recorded uid, never a caller's claim.
                        match peer {
                            PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                            _ => "uid:unknown".to_string(),
                        }
                        .as_str(),
                        if enabled {
                            "strategy.enable (default account)"
                        } else {
                            "strategy.disable (default account)"
                        },
                        now_ms(),
                    ) {
                        Ok((kept, _set_now)) => {
                            kept_exits_only = kept;
                            // The scoped write IS the default account's
                            // enablement now (see the bugfix note above): a
                            // success means the name was registered and the
                            // toggle took — the legacy global flag that
                            // reported `applied == false` was answering
                            // against a dispatch that simply hosted no
                            // instance. Correct the reply so the panel's
                            // 找不到/没生效 families stay honest; the
                            // handshake refusal above keeps its bytes.
                            if reason.is_none() {
                                found = true;
                                applied = true;
                            }
                        }
                        Err(e) => {
                            // A persist/refusal failure while the legacy
                            // toggle ALSO found nothing keeps the not-found
                            // legacy bytes (`ui-plugin-check`: found=false,
                            // 200); a toggle that found the name surfaces
                            // the real error instead of a lying success.
                            if !found {
                                mirror_error = None;
                            } else {
                                mirror_error = Some(client_err(e));
                            }
                        }
                    }
                }
                if let Some(e) = mirror_error {
                    return Err((e.0, e.1, e.2));
                }
                Ok(serde_json::json!({
                    "name": name,
                    "enabled": if reason.is_some() { false } else { enabled && applied },
                    "found": found,
                    "applied": applied,
                    "reason": reason,
                    "accountId": account_id,
                    "keptExitsOnly": kept_exits_only,
                }))
            } else {
                let id = AccountId::from(account_id);
                let mut c = core.lock().await;
                // Existence first: the assignment write's own refusal names
                // unknown ids too, but a 404-shaped row on the panel should
                // say WHICH half was wrong (the account, not the strategy).
                if let Err(e) = c.accounts().require(&id) {
                    Err(client_err(e))
                } else {
                    // Current assignment of THIS account (engine scopes are
                    // the mirror; the book is the truth). Empty = runs
                    // nothing yet.
                    let mut assignment: Vec<String> = c
                        .accounts()
                        .account(&id)
                        .map(|a| a.strategies.clone())
                        .unwrap_or_default();
                    let present = assignment.iter().any(|s| s == name);
                    if enabled && !present {
                        assignment.push(name.to_string());
                    } else if !enabled {
                        assignment.retain(|s| s != name);
                    }
                    let kept_exits_only = match c.set_account_strategies_persisting(
                        &id,
                        assignment,
                        // Same actor posture as the other panel writes: the
                        // KERNEL-recorded uid, never a caller's claim.
                        match peer {
                            PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                            _ => "uid:unknown".to_string(),
                        }
                        .as_str(),
                        params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("strategy.enable (account-scoped)"),
                        now_ms(),
                    ) {
                        Ok(kept) => kept,
                        Err(e) => {
                            let e = client_err(e);
                            return Err((e.0, e.1, e.2));
                        }
                    };
                    Ok(serde_json::json!({
                        "name": name,
                        "enabled": enabled,
                        "found": true,
                        "applied": true,
                        "reason": Value::Null,
                        "accountId": account_id,
                        "keptExitsOnly": kept_exits_only,
                    }))
                }
            }
        }
        method::STRATEGY_LOAD => {
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                Err((Failure::INVALID_PARAMS, "path required".into(), None))
            } else {
                let outcome = core.lock().await.load_strategy_lib(path);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }
        }
        method::STRATEGY_UNLOAD => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                Err((Failure::INVALID_PARAMS, "name required".into(), None))
            } else {
                let outcome = core.lock().await.unload_strategy(name);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }
        }
        method::STRATEGY_RELOAD => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() || path.is_empty() {
                Err((
                    Failure::INVALID_PARAMS,
                    "name and path required".into(),
                    None,
                ))
            } else {
                let outcome = core.lock().await.reload_strategy(name, path);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }
        }
        method::STRATEGY_PARAMS_GET => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                Err((Failure::INVALID_PARAMS, "name required".into(), None))
            } else {
                let c = core.lock().await;
                Ok(serde_json::to_value(c.strategy_params_readout(name)).unwrap_or(Value::Null))
            }
        }
        method::STRATEGY_PARAMS_SET => {
            match serde_json::from_value::<StrategyParamsSetParams>(params.clone()) {
                Err(e) => Err((
                    Failure::INVALID_PARAMS,
                    format!(
                        "{e} — nothing was applied; the body is \
                         {{ name, params: {{ knob: value, … }}, reason? }} and every \
                         value is an exact decimal string"
                    ),
                    None,
                )),
                Ok(p) => {
                    if p.name.is_empty() {
                        Err((Failure::INVALID_PARAMS, "name required".into(), None))
                    } else {
                        // The actor is the peer's KERNEL-recorded uid, not a
                        // claim by the caller (same posture as risk.setExit).
                        let actor = match peer {
                            PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                            _ => "uid:unknown".to_string(),
                        };
                        match core.lock().await.apply_strategy_params_set(
                            &p.name,
                            &p.params,
                            &actor,
                            p.reason,
                            now_ms(),
                        ) {
                            Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                            Err(e) => Err(core_err(e)),
                        }
                    }
                }
            }
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
