//! The `.env` state face of the execution policy — the read of the POLICY_*
//! keys for one section (defaults = no suffix, accounts = `_ID` suffix) and
//! the write face's whole-footprint map. Split from `mod.rs` verbatim (#780
//! ratchet: pure code motion, no logic edits) to keep both files inside the
//! 1,500-line ratchet.

use super::*;

/// Read the env layer for one section. `account_id: None` = the global names
/// (`POLICY_BUDGET_RATIO`, `POLICY_MIN_BUDGET_USD`, …); `Some(id)` = the
/// per-account names (`POLICY_BUDGET_RATIO_<ID>`, …). Applied AFTER the TOML
/// layer so env outranks it. The two pre-#751 bare names (`BUDGET_RATIO`,
/// `MAX_BUDGET_USD`) stay as compatibility ALIASES read only when the
/// POLICY_-prefixed key is absent — the write face only ever writes the
/// prefixed family.
pub(super) fn apply_env(
    section: &mut SectionDefaults,
    account_id: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let suffix = account_id
        .map(|id| format!("_{}", env_account_suffix(id)))
        .unwrap_or_default();
    // Prefixed family first; the pre-#751 bare names stay as read-only
    // aliases when the prefixed key is absent.
    let ratio_raw = env(&format!("POLICY_BUDGET_RATIO{suffix}"))
        .or_else(|| env(&format!("BUDGET_RATIO{suffix}")));
    if let Some(raw) = ratio_raw {
        section.budget_ratio = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_BUDGET_RATIO{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MIN_BUDGET_USD{suffix}")) {
        section.min_budget_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MIN_BUDGET_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    let max_raw = env(&format!("POLICY_MAX_BUDGET_USD{suffix}"))
        .or_else(|| env(&format!("MAX_BUDGET_USD{suffix}")));
    if let Some(raw) = max_raw {
        section.max_budget_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MAX_BUDGET_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MIN_EQUITY_USD{suffix}")) {
        section.min_equity_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MIN_EQUITY_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MAX_POSITIONS_PER_ASSET{suffix}")) {
        let v: u32 = raw.trim().parse().map_err(|_| {
            format!("env POLICY_MAX_POSITIONS_PER_ASSET{suffix}={raw:?} does not parse as u32")
        })?;
        section.max_positions_per_asset = v;
    }
    if let Some(raw) = env(&format!("POLICY_RULES{suffix}")) {
        let rules: Vec<Rule> = serde_json::from_str(raw.trim()).map_err(|e| {
            format!("env POLICY_RULES{suffix} does not parse as a rules JSON array: {e}")
        })?;
        for r in &rules {
            r.then
                .validate(&r.name)
                .map_err(|e| format!("env POLICY_RULES{suffix}: {e}"))?;
        }
        section.rules = rules;
    }
    Ok(())
}

/// The `.env` footprint of ONE account section under the issue-751 spelling:
/// every key the section owns, mapped to the value a section write gives it
/// (`Some` = written, `None` = swept/absent).
///
/// This is the write-side twin of [`apply_env`]. The IPC writer renders the
/// `Some` values into the file; the whole map is ALSO kept in the in-memory
/// snapshot a same-process reload consults (#757), so a stale deployment
/// value cannot shadow the write, and a `None` tombstone stops a `reset`
/// from resurrecting the value the process env still carries.
pub fn section_env_state(
    account_id: &str,
    section: Option<&SectionFields>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    // The DEFAULTS pseudo-account (the panel's selector chip): the READ side
    // loads the defaults section with NO account suffix (`apply_env(.., None,
    // ..)` — `POLICY_RULES`, not `POLICY_RULES_DEFAULTS`), so the WRITE side
    // must normalize the same spelling or the operator's save lands on a key
    // no reader reads. 2026-11-08 live find: a defaults-section save wrote
    // `POLICY_RULES_DEFAULTS` (the ge-20 rule) while the loader kept reading
    // the bare `POLICY_RULES` (ge 2) — the edit round-tripped as a no-op.
    // `defaults` reserved as a section selector: it can never be a real
    // account id on this wire face.
    let is_defaults = account_id.eq_ignore_ascii_case("defaults");
    let sfx = if is_defaults {
        String::new()
    } else {
        env_account_suffix(account_id)
    };
    let key = |tail: &str| {
        if sfx.is_empty() {
            format!("POLICY_{tail}")
        } else {
            format!("POLICY_{tail}_{sfx}")
        }
    };
    // The six keys are the section's whole footprint — absent fields are
    // swept, exactly the TOML section-replacement semantics.
    let mut state: std::collections::BTreeMap<String, Option<String>> = [
        "BUDGET_RATIO",
        "MIN_BUDGET_USD",
        "MAX_BUDGET_USD",
        "MIN_EQUITY_USD",
        "MAX_POSITIONS_PER_ASSET",
        "RULES",
    ]
    .into_iter()
    .map(|tail| (key(tail), None))
    .collect();
    // The pre-fix writer's ghost: a defaults-section save used to land on the
    // `POLICY_RULES_DEFAULTS`-shaped keys no reader reads. Every defaults
    // write sweeps them with the write, so the pollution self-heals on the
    // operator's next save instead of lingering forever.
    if is_defaults {
        for tail in [
            "BUDGET_RATIO",
            "MIN_BUDGET_USD",
            "MAX_BUDGET_USD",
            "MIN_EQUITY_USD",
            "MAX_POSITIONS_PER_ASSET",
            "RULES",
        ] {
            state.insert(format!("POLICY_{tail}_DEFAULTS"), None);
        }
    }
    let Some(s) = section else {
        return Ok(state);
    };
    if let Some(v) = s.budget_ratio {
        state.insert(key("BUDGET_RATIO"), Some(v.to_string()));
    }
    if let Some(v) = s.min_budget_usd {
        state.insert(key("MIN_BUDGET_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.max_budget_usd {
        state.insert(key("MAX_BUDGET_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.min_equity_usd {
        state.insert(key("MIN_EQUITY_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.max_positions_per_asset {
        state.insert(key("MAX_POSITIONS_PER_ASSET"), Some(v.to_string()));
    }
    if let Some(rules) = &s.rules {
        let json = serde_json::to_string(rules)
            .map_err(|e| format!("rules do not render as JSON: {e}"))?;
        state.insert(key("RULES"), Some(json));
    }
    Ok(state)
}

/// Whether ANY per-account POLICY_* key names this id — the "this account has
/// an env section" probe that registers env-only sections for accounts the
/// TOML has no `[accounts.<id>]` row for (issue 751: after the toml retires,
/// ACCOUNT_LIST + these keys ARE the per-account policy surface).
pub(super) fn env_has_policy_section(env: &impl Fn(&str) -> Option<String>, id: &str) -> bool {
    let sfx = env_account_suffix(id);
    [
        format!("POLICY_BUDGET_RATIO_{sfx}"),
        format!("POLICY_MIN_BUDGET_USD_{sfx}"),
        format!("POLICY_MAX_BUDGET_USD_{sfx}"),
        format!("POLICY_MIN_EQUITY_USD_{sfx}"),
        format!("POLICY_MAX_POSITIONS_PER_ASSET_{sfx}"),
        format!("POLICY_RULES_{sfx}"),
    ]
    .iter()
    .any(|k| env(k).is_some())
}

/// The env suffix for an account id: uppercased, non-alphanumerics → `_`.
pub(super) fn env_account_suffix(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_ascii_uppercase()
}
