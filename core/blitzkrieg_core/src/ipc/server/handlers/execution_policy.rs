use super::prelude::*;
use crate::schema::*;

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
                    let views: Vec<crate::schema::ExecutionPolicyAuditView> = records
                        .iter()
                        .map(|r| crate::schema::ExecutionPolicyAuditView {
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
    let (path, env_path) = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => (
                p,
                c.config()
                    .env_file_path
                    .clone()
                    .map(std::path::PathBuf::from),
            ),
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
    let written = policy_section_write(&path, env_path.as_deref(), &p.account_id, Some(section))
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    // #757: this write is authoritative for its keys — the reload below
    // resolves them from here, not from the stale startup export.
    c.execution_policy_record_env_write(written);
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
    let (path, env_path) = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => (
                p,
                c.config()
                    .env_file_path
                    .clone()
                    .map(std::path::PathBuf::from),
            ),
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
    let written = policy_section_write(&path, env_path.as_deref(), &p.account_id, None)
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    // #757: the swept keys are authoritative ABSENCE — the reload must not
    // fall back to the value the process still carries from boot.
    c.execution_policy_record_env_write(written);
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
    // Zero-env parse: the probe judges the SECTION's shape, never the
    // process environment's POLICY_* layer (which must not veto an
    // operator's unrelated write).
    crate::execution_policy::Policy::load_with_env(&rendered, |_| None)
        .map_err(|e| format!("section rejected: {e}"))?;
    Ok(s)
}

/// #363: write ONE account section to the `.env` single source (issue 751).
/// The section is REPLACED wholesale — every POLICY_<ID>_* key for the
/// account is swept first, then the `Some` fields land (an absent field =
/// no key = inherit from the globals, exactly the TOML section-replacement
/// semantics). `reset` (section = None) sweeps all keys so the account
/// falls back to the globals.
///
/// Returns the section's whole env footprint (`Some` = written value,
/// `None` = swept). The caller records it in the core's in-memory snapshot
/// BEFORE the reload (#757): the process already carries the startup export
/// of the OLD value, and a reload must resolve these keys from this write,
/// not from that stale snapshot. A rejected write returns `Err` before
/// touching the file or the snapshot.
///
/// Fail-closed precondition: while the legacy `execution_policy.toml` still
/// exists the write REFUSES — the loader reads env over that file, so a
/// write here would be dead data the operator believes is live; the
/// migration script owns the one-way move.
fn policy_section_write(
    path: &str,
    env_path: Option<&std::path::Path>,
    account_id: &str,
    section: Option<crate::execution_policy::SectionFields>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    if account_id.trim().is_empty() {
        return Err("account id must not be empty".to_string());
    }
    let Some(env_path) = env_path else {
        return Err(
            "execution policy: no .env path configured — persistence is \
             disabled for this core (backtests and in-process tests); \
             nothing was applied"
                .to_string(),
        );
    };
    if std::path::Path::new(path).exists() {
        return Err(format!(
            "refusing the policy write: the legacy {path} still exists — run \
             scripts/migrate-config-to-env.sh first (issue 751: one source; \
             the loader reads POLICY_* env over this file, so a write here \
             would be dead data)"
        ));
    }
    // The section's whole env footprint, spelled by the loader's own rule
    // (`execution_policy::section_env_state`) — one spelling for the write
    // face and the reload's in-memory snapshot.
    let state = crate::execution_policy::section_env_state(account_id, section.as_ref())?;
    // `reset` of a section that is not ON FILE refuses by name (the fat-finger
    // contract): the six keys are the section's whole footprint, none present
    // = nothing to reset. The file face is the truth here — a section that
    // lives only in the process environment is launcher-owned and this write
    // face never claimed it.
    if section.is_none() {
        let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(env_path)
            .unwrap_or_default()
            .lines()
            .filter_map(crate::env_file::parse_line)
            .collect();
        let present = state.keys().any(|k| on_file.contains_key(k));
        if !present {
            return Err(format!(
                "execution policy: no section for account `{account_id}` to reset"
            ));
        }
    }
    // Sweep-then-write (the account layer's posture): remove EVERY section
    // key for the account, then upsert the `Some` fields — absent = key gone
    // = inherit from the globals (the TOML section-replacement semantics),
    // and `reset` rides the same sweep with nothing re-written.
    let sweep_refs: Vec<&str> = state.keys().map(|k| k.as_str()).collect();
    crate::env_file::remove_env_vars(env_path, &sweep_refs)
        .map_err(|e| format!("policy env sweep failed: {e}"))?;
    let vars: std::collections::BTreeMap<String, String> = state
        .iter()
        .filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v)))
        .collect();
    crate::env_file::upsert_env_vars(env_path, &vars)
        .map_err(|e| format!("policy env write failed: {e}"))?;
    Ok(state)
}
