use super::prelude::*;
use crate::ipc::schema::*;

// The `execution_policy` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_execution_policy(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::EXECUTION_POLICY_LIST => {
            let c = core.lock().await;
            let defaults = c
                .execution_policy_section_view("*")
                .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            let accounts: Vec<serde_json::Value> = c
                .execution_policy_account_ids()
                .into_iter()
                .filter_map(|id| {
                    c.execution_policy_section_view(&id).map(|view| {
                        serde_json::json!({
                            "accountId": id,
                            "section": serde_json::to_value(&view).unwrap_or(Value::Null),
                        })
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "loaded": c.execution_policy_loaded(),
                "defaults": defaults,
                "accounts": accounts,
            }))
        }
        method::EXECUTION_POLICY_GET => {
            match serde_json::from_value::<ExecutionPolicyGetParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    match c.execution_policy_section_view(&p.account_id) {
                        Some(view) => Ok(serde_json::to_value(view).unwrap_or(Value::Null)),
                        None => Err((
                            Failure::INVALID_PARAMS,
                            format!(
                                "execution policy: no section for account `{}` and no policy loaded",
                                p.account_id
                            ),
                            None,
                        )),
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::EXECUTION_POLICY_SET => {
            match serde_json::from_value::<ExecutionPolicySetParams>(params) {
                Ok(p) => execution_policy_set(core, p).await,
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::EXECUTION_POLICY_RESET => {
            match serde_json::from_value::<ExecutionPolicyGetParams>(params) {
                Ok(p) => execution_policy_reset(core, p).await,
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::EXECUTION_POLICY_HISTORY => {
            match serde_json::from_value::<ExecutionPolicyHistoryParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    let records: Vec<crate::execution_policy::PolicyAuditRecord> = c
                        .execution_policy_audit()
                        .into_iter()
                        .filter(|r| {
                            p.account_id
                                .as_ref()
                                .map(|id| &r.account_id == id)
                                .unwrap_or(true)
                        })
                        .collect();
                    let views: Vec<crate::ipc::schema::ExecutionPolicyAuditView> = records
                        .iter()
                        .map(|r| crate::ipc::schema::ExecutionPolicyAuditView {
                            ts_ms: r.ts_ms,
                            actor: r.actor.clone(),
                            action: r.action.clone(),
                            account_id: r.account_id.clone(),
                            before: r.before.clone(),
                            after: r.after.clone(),
                            error: r.error.clone(),
                        })
                        .collect();
                    Ok(serde_json::to_value(views).unwrap_or(Value::Null))
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::EXECUTION_POLICY_PREVIEW => {
            match serde_json::from_value::<ExecutionPolicyPreviewParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    Ok(
                        serde_json::to_value(c.execution_policy_preview(&p.account_id, 100))
                            .unwrap_or(Value::Null),
                    )
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

/// #363: the `execution_policy.set` flow — validate params, write the ONE
/// named account's section, full-reload in memory, land one audit line.
/// Separate from `handle_line` so the early returns are plain `?`/`return`
/// on a real `Result` (a dispatch arm's tail cannot `return` from the match
/// without skipping the reply encoding).
async fn execution_policy_set(
    core: &Arc<AsyncMutex<Core>>,
    p: ExecutionPolicySetParams,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    let path = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => p,
            None => {
                return Err((
                    Failure::INVALID_PARAMS,
                    "execution policy disabled (no config path)".to_string(),
                    None,
                ));
            }
        }
    };
    let before = {
        let c = core.lock().await;
        c.execution_policy_section_view(&p.account_id)
            .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
    };
    let section = section_from_set_params(&p).map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    policy_section_write(&path, &p.account_id, Some(section))
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    c.reload_execution_policy()
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    // `after` is the RELOADED view — the state the kernel will actually
    // judge under, not the in-memory state the write replaced.
    let after = c
        .execution_policy_section_view(&p.account_id)
        .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null));
    // #386: this core's OWN journal — a test core's records never land in
    // the checkout's `data/audit/…` file another parallel test counts on.
    c.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
        ts_ms: now_ms(),
        actor: crate::execution_policy::ACTOR_IPC.to_string(),
        action: "set".to_string(),
        account_id: p.account_id.clone(),
        before,
        after,
        error: None,
    });
    Ok(serde_json::json!({ "accountId": p.account_id }))
}

/// #363: the `execution_policy.reset` flow — remove the account's section
/// (it falls back to the globals), full-reload, audit.
async fn execution_policy_reset(
    core: &Arc<AsyncMutex<Core>>,
    p: ExecutionPolicyGetParams,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    let path = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => p,
            None => {
                return Err((
                    Failure::INVALID_PARAMS,
                    "execution policy disabled (no config path)".to_string(),
                    None,
                ));
            }
        }
    };
    let before = {
        let c = core.lock().await;
        c.execution_policy_section_view(&p.account_id)
            .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
    };
    policy_section_write(&path, &p.account_id, None)
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    c.reload_execution_policy()
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    // `after` is the RELOADED view (see the set arm).
    let after = c
        .execution_policy_section_view(&p.account_id)
        .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null));
    // #386: this core's OWN journal (see the set arm).
    c.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
        ts_ms: now_ms(),
        actor: crate::execution_policy::ACTOR_IPC.to_string(),
        action: "reset".to_string(),
        account_id: p.account_id.clone(),
        before,
        after,
        error: None,
    });
    Ok(serde_json::json!({ "accountId": p.account_id }))
}

/// #363: turn `execution_policy.set` params into the section that lands in
/// the TOML. Decimals arrive as strings so the shortest-decimal rule (0.10
/// is the decimal 0.10, not a binary expansion) survives the JSON hop; they
/// are validated HERE by round-tripping through the real parser before any
/// file is touched — a bad ratio is refused on the wire, not discovered by a
/// later boot refusal.
fn section_from_set_params(
    p: &ExecutionPolicySetParams,
) -> Result<crate::execution_policy::SectionFields, String> {
    use crate::execution_policy::SectionFields;
    let mut s = SectionFields::default();
    if let Some(v) = &p.budget_ratio {
        s.budget_ratio = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("budgetRatio `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.min_budget_usd {
        s.min_budget_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("minBudgetUsd `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.max_budget_usd {
        s.max_budget_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("maxBudgetUsd `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.min_equity_usd {
        s.min_equity_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("minEquityUsd `{v}` is not a decimal"))?,
        );
    }
    s.max_positions_per_asset = p.max_positions_per_asset;
    if let Some(rules) = &p.rules {
        let mut out = Vec::with_capacity(rules.len());
        for r in rules {
            out.push(
                serde_json::from_value::<crate::execution_policy::Rule>(r.clone())
                    .map_err(|e| format!("rules entry does not parse: {e}"))?,
            );
        }
        s.rules = Some(out);
    }
    // The section must validate as the file would see it: round-trip it the
    // way `policy_section_write` actually writes — insert into a typed file,
    // render the WHOLE file, parse it back. Same parser, same refusals.
    // (Rendering the bare section under a hand-typed header is NOT the same
    // thing: `toml::to_string_pretty` spells the rules array `[[rules]]`
    // without the `[accounts.<id>]` prefix, so a section with rules could
    // never pass that probe — #364 fixed the validator to match the writer.)
    let mut probe_file = crate::execution_policy::PolicyFile::default();
    probe_file
        .accounts
        .insert("__probe__".to_string(), s.clone());
    let rendered =
        toml::to_string_pretty(&probe_file).map_err(|e| format!("section does not render: {e}"))?;
    crate::execution_policy::Policy::load_from_str(&rendered)
        .map_err(|e| format!("section rejected: {e}"))?;
    Ok(s)
}

/// #363: rewrite ONE account section inside the policy TOML (read file →
/// swap the map entry → write back). The section is REPLACED wholesale —
/// `set` never touches another account's lines, and `reset` (section =
/// None) removes the entry so the account falls back to the globals.
/// Cross-account safety is structural: the caller names exactly one
/// account id and this function writes exactly that map key.
fn policy_section_write(
    path: &str,
    account_id: &str,
    section: Option<crate::execution_policy::SectionFields>,
) -> Result<(), String> {
    if account_id.trim().is_empty() {
        return Err("account id must not be empty".to_string());
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut file: crate::execution_policy::PolicyFile = toml::from_str(&text)
        .map_err(|e| format!("existing file does not parse; refusing to edit: {e}"))?;
    match section {
        Some(s) => {
            file.accounts.insert(account_id.to_string(), s);
        }
        None => {
            if file.accounts.remove(account_id).is_none() {
                return Err(format!(
                    "execution policy: no section for account `{account_id}` to reset"
                ));
            }
        }
    }
    let out =
        toml::to_string_pretty(&file).map_err(|e| format!("policy file does not render: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("policy file write failed: {e}"))?;
    Ok(())
}
