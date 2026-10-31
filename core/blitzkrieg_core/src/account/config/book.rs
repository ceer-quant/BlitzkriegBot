//! Split out of the parent module (issue #780): pure move.

use super::*;

/// Load the account book from ACCOUNT_* environment keys (issue 751).
/// `ACCOUNT_LIST` names the ids in book order; a missing/empty value →
/// `None` (the caller falls back — the toml file or the single default).
///
/// Fail-closed, the same discipline `from_str` runs: a bad status, a
/// `rounds` value outside the plugins' declared union, or a display id
/// naming no listed account REFUSES the boot.
pub fn load_account_ledgers_from_env(
    now_ms: i64,
    allowed: &[i64],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<AccountLedgers>, CoreError> {
    let Some(list) = env("ACCOUNT_LIST")
        .map(|v| env_list(Some(v)))
        .filter(|l| !l.is_empty())
    else {
        return Ok(None);
    };
    // issue 753: reject the alias BEFORE reading any per-account key, so a
    // `paper-a`/`paper_a` list never lands a book whose rows share one
    // ACCOUNT_<SFX>_* key space.
    refuse_env_suffix_collisions(&list)?;
    let mut seen = std::collections::HashSet::new();
    let mut accounts = Vec::with_capacity(list.len());
    for id in &list {
        if !seen.insert(id.clone()) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("ACCOUNT_LIST names id `{id}` twice — ids are unique"),
            ));
        }
        if id.trim().is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "ACCOUNT_LIST carries an empty id — an account without an \
                 identity cannot be addressed or audited",
            ));
        }
        let row = parse_env_account(id, env)?;
        let status = row.status.clone()?;
        let rounds = row.rounds.clone();
        let rounds_key = format!("ACCOUNT_{}_ROUNDS", env_suffix(id));
        accounts.push(Account {
            id: AccountId::from(id.as_str()),
            name: row.name.clone().unwrap_or_else(|| id.clone()),
            market_type: MarketType::Prediction,
            status,
            credential_keys: CredentialKeys {
                api_key_env: row.api_key_env.clone(),
                secret_env: row.secret_env.clone(),
                passphrase_env: row.passphrase_env.clone(),
                private_key_env: row.private_key_env.clone(),
                loaded: false,
            },
            venue: row.venue.clone().unwrap_or_default(),
            trading_enabled: row.trading_enabled.unwrap_or(true),
            strategies: row.strategies.clone(),
            rounds: validate_rounds_from(&rounds_key, rounds, allowed)?,
            updated_at_ms: now_ms,
        });
    }
    // Probe credentials (the same §9.4 posture the toml path runs).
    for a in accounts.iter_mut() {
        a.credential_keys = read_credential_keys(&a.credential_keys);
    }
    let active = if accounts.iter().any(|a| a.id == default_account_id()) {
        default_account_id()
    } else {
        accounts[0].id.clone()
    };
    let display = match env("ACCOUNT_DISPLAY").as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if !accounts.iter().any(|a| a.id.as_str() == id) {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "ACCOUNT_DISPLAY=`{id}` names no ACCOUNT_LIST account — \
                         the display pointer must name a configured account \
                         (fail-closed: fix the id or delete the key)"
                    ),
                ));
            }
            id.to_string()
        }
        _ => accounts
            .iter()
            .find(|a| matches!(a.status, AccountStatus::Active))
            .map(|a| a.id.as_str().to_string())
            .unwrap_or_else(|| accounts[0].id.as_str().to_string()),
    };
    // The arb legs, when configured, validate against this book's ids.
    if let Some(arb) = read_cross_venue_arb_from_env(env)? {
        validate_cross_venue_arb_section(
            &arb,
            &accounts
                .iter()
                .map(|a| a.id.as_str().to_string())
                .collect::<Vec<_>>(),
        )?;
    }
    let book =
        AccountLedgers::new_with_display(accounts, active, AccountId::from(display.as_str()));
    // #778(a): the env-configured book's shared-venue sweep — the same warning
    // the toml loader runs (one wallet, several ledgers behind one credential).
    book.warn_shared_venue_bindings();
    Ok(Some(book))
}

/// The `[cross_venue_arb]` section read from ACCOUNT_ARB_* env keys.
/// All-or-nothing: leg-a venue+account are the minimum configured pair;
/// anything else present completes the section, anything missing with the
/// minimum present is a refusal (a half-written arb config must never trade).
pub fn read_cross_venue_arb_from_env(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<CrossVenueArbConfig>, CoreError> {
    let leg_a_venue = env("ACCOUNT_ARB_LEG_A_VENUE").map(|v| v.trim().to_string());
    let leg_a_account = env("ACCOUNT_ARB_LEG_A_ACCOUNT").map(|v| v.trim().to_string());
    let Some(leg_a_venue) = leg_a_venue.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let leg_a_account = leg_a_account.filter(|v| !v.is_empty()).ok_or_else(|| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            "ACCOUNT_ARB_LEG_A_VENUE is set but ACCOUNT_ARB_LEG_A_ACCOUNT is \
             missing — an arb leg needs both (fail-closed)"
                .to_string(),
        )
    })?;
    let leg_b_venue = env("ACCOUNT_ARB_LEG_B_VENUE").filter(|v| !v.trim().is_empty());
    let leg_b_account = env("ACCOUNT_ARB_LEG_B_ACCOUNT").filter(|v| !v.trim().is_empty());
    let (leg_b_venue, leg_b_account) = match (leg_b_venue, leg_b_account) {
        (Some(v), Some(a)) => (v.trim().to_string(), a.trim().to_string()),
        (None, None) => {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "ACCOUNT_ARB_LEG_A_* is configured but leg B is missing \
                 entirely — cross-venue arb needs both legs (fail-closed)"
                    .to_string(),
            ));
        }
        _ => {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "ACCOUNT_ARB leg B is half-written (one of VENUE/ACCOUNT \
                 present, the other missing) — fix or delete both (fail-closed)"
                    .to_string(),
            ));
        }
    };
    let pairs = env_list(env("ACCOUNT_ARB_PAIRS"));
    if pairs.is_empty() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "ACCOUNT_ARB_PAIRS is missing or empty — the arb engine needs at \
             least one pair (fail-closed)"
                .to_string(),
        ));
    }
    Ok(Some(CrossVenueArbConfig {
        leg_a_venue,
        leg_a_account,
        leg_b_venue,
        leg_b_account,
        pairs: pairs.iter().map(|p| p.trim().to_uppercase()).collect(),
    }))
}

/// The ACCOUNT_* lookup: process environment FIRST, then the `.env` file at
/// `env_path` (the same file `CoreConfig::env_file_path` names and
/// `market.credentials.set` writes). The launcher loads `.env` into the
/// process environment at start, so in production the two faces carry the
/// same values; a process-env override beats the file line (12-factor — an
/// operator exporting a key at launch intends it). Tests feed a closure
/// directly to [`load_account_ledgers_from_env`].
pub fn env_lookup_from_sources(env_path: Option<&Path>) -> impl Fn(&str) -> Option<String> + '_ {
    let file_map: std::collections::HashMap<String, String> = env_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| {
            text.lines()
                .filter_map(crate::env_file::parse_line)
                .collect()
        })
        .unwrap_or_default();
    move |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| file_map.get(key).cloned())
    }
}

// ── .env write faces (issue 751) ────────────────────────────────────────────
//
// Every runtime write face upserts ACCOUNT_* keys through `crate::env_file`
// (atomic temp+rename, owner-only 0600, every line outside the managed keys
// preserved byte-for-byte — the same primitive `market.credentials.set`
// runs). Read-modify-write uses the CURRENT file first: the process env is a
// boot snapshot and is stale after the first hot write. Process values remain
// a fallback for keys absent from the file, preserving explicit launch-time
// overrides without resurrecting an old ACCOUNT_LIST after account.remove.

pub(crate) fn env_lookup_for_write_with<'a>(
    env_path: &'a Path,
    fallback: impl Fn(&str) -> Option<String> + 'a,
) -> impl Fn(&str) -> Option<String> + 'a {
    let file_map: std::collections::HashMap<String, String> = std::fs::read_to_string(env_path)
        .ok()
        .map(|text| {
            text.lines()
                .filter_map(crate::env_file::parse_line)
                .collect()
        })
        .unwrap_or_default();
    move |key: &str| file_map.get(key).cloned().or_else(|| fallback(key))
}

fn env_lookup_for_write(env_path: &Path) -> impl Fn(&str) -> Option<String> + '_ {
    env_lookup_for_write_with(env_path, |key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    })
}

/// The full ACCOUNT_* key set ONE account id owns — the remove face's sweep
/// list.
fn account_env_keys(account_id: &str) -> Vec<String> {
    let sfx = env_suffix(account_id);
    [
        "NAME",
        "VENUE",
        "STATUS",
        "TRADING_ENABLED",
        "STRATEGIES",
        "ROUNDS",
        "API_KEY_ENV",
        "SECRET_ENV",
        "PASSPHRASE_ENV",
        "PRIVATE_KEY_ENV",
    ]
    .iter()
    .map(|tail| format!("ACCOUNT_{sfx}_{tail}"))
    .collect()
}

/// The write face's precondition (fail-closed): every runtime writer REFUSES
/// while the legacy toml file still exists — the migration script owns the
/// one-way move, a runtime write must never silently fork the book across
/// two sources (the loader reads env first, so a toml written here would be
/// dead data the operator believes is live).
fn refuse_while_legacy_toml(toml_path: &Path, op: &str) -> Result<(), CoreError> {
    if toml_path.exists() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "refusing the {op} write: the legacy {} still exists — run \
                 scripts/migrate-config-to-env.sh first (issue 751: one source; \
                 the loader reads ACCOUNT_* env over this file, so a write here \
                 would be dead data)",
                toml_path.display()
            ),
        ));
    }
    Ok(())
}

/// Upsert `vars` into the `.env`, wrapped into the CoreError posture every
/// persist function reports.
fn env_upsert(
    env_path: &Path,
    vars: &std::collections::BTreeMap<String, String>,
    op: &str,
) -> Result<(), CoreError> {
    crate::env_file::upsert_env_vars(env_path, vars)
        .map(|_written| ())
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(".env {op} write failed: {e}"),
            )
        })
}

/// The env-state view one writer needs: the ACCOUNT_LIST as configured, plus
/// the lookup. Refuses a boot-shaped inconsistency only when the writer must
/// know the book (add/remove/display).
pub(crate) fn current_account_list(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, CoreError> {
    Ok(env("ACCOUNT_LIST")
        .map(|v| env_list(Some(v)))
        .unwrap_or_default())
}

/// `account.venue.set`'s persisted half — upsert `ACCOUNT_<ID>_VENUE`
/// (empty venue = remove the key: the env grammar spells "unbound" by
/// absence, the same shape the toml face ran). The CALLER owns the runtime
/// validations; this layer owns the file and the legacy-toml refusal.
pub fn env_persist_account_venue(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    venue: &str,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "venue")?;
    let mut vars = std::collections::BTreeMap::new();
    let key = account_env_keys(account_id)
        .into_iter()
        .find(|k| k.ends_with("_VENUE"))
        .unwrap();
    if venue.is_empty() {
        crate::env_file::remove_env_vars(env_path, &[&key]).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(".env venue remove failed: {e}"),
            )
        })?;
    } else {
        vars.insert(key, venue.to_string());
        env_upsert(env_path, &vars, "venue")?;
    }
    Ok(())
}

/// `account.create`'s persisted half — append the id to ACCOUNT_LIST and
/// write its NAME/VENUE keys plus the selected plugin's credential variable
/// names. Values remain owned by `market.credentials.set` and never enter the
/// account request or response.
pub fn env_persist_account_create(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    name: Option<&str>,
    venue: Option<&str>,
    credential_keys: &CredentialKeys,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "create")?;
    let lookup = env_lookup_for_write(env_path);
    let list = current_account_list(&lookup)?;
    if list.iter().any(|a| a == account_id) {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("ACCOUNT_LIST already names `{account_id}`"),
        ));
    }
    // issue 753: a new id may not alias an existing one in the env key space.
    // This runs BEFORE the ACCOUNT_LIST upsert, so a refusal leaves .env
    // byte-identical (no partial row, no list growth).
    {
        let mut ids = list.clone();
        ids.push(account_id.to_string());
        refuse_env_suffix_collisions(&ids)?;
    }
    let mut new_list = list;
    new_list.push(account_id.to_string());
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("ACCOUNT_LIST".to_string(), new_list.join(","));
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        let sfx = env_suffix(account_id);
        vars.insert(format!("ACCOUNT_{sfx}_NAME"), n.to_string());
    }
    if let Some(v) = venue.filter(|v| !v.is_empty()) {
        let sfx = env_suffix(account_id);
        vars.insert(format!("ACCOUNT_{sfx}_VENUE"), v.to_string());
    }
    let sfx = env_suffix(account_id);
    for (tail, value) in [
        ("API_KEY_ENV", credential_keys.api_key_env.as_deref()),
        ("SECRET_ENV", credential_keys.secret_env.as_deref()),
        ("PASSPHRASE_ENV", credential_keys.passphrase_env.as_deref()),
        (
            "PRIVATE_KEY_ENV",
            credential_keys.private_key_env.as_deref(),
        ),
    ] {
        if let Some(value) = value {
            vars.insert(format!("ACCOUNT_{sfx}_{tail}"), value.to_string());
        }
    }
    env_upsert(env_path, &vars, "create")
}

/// `account.trading.set`'s persisted half — `ACCOUNT_<ID>_TRADING_ENABLED`:
/// `false` is written explicitly; `true` REMOVES the key (the env grammar
/// spells "trading" by absence, the exact shape a pre-switch .env had).
pub fn env_persist_account_trading(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    enabled: bool,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "trading")?;
    let key = account_env_keys(account_id)
        .into_iter()
        .find(|k| k.ends_with("_TRADING_ENABLED"))
        .unwrap();
    if enabled {
        crate::env_file::remove_env_vars(env_path, &[&key]).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(".env trading remove failed: {e}"),
            )
        })?;
    } else {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert(key, "false".to_string());
        env_upsert(env_path, &vars, "trading")?;
    }
    Ok(())
}

/// `account.strategies.set`'s persisted half — `ACCOUNT_<ID>_STRATEGIES`
/// (empty list = remove the key: absence is "no restriction").
pub fn env_persist_account_strategies(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    strategies: &[String],
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "strategies")?;
    let key = account_env_keys(account_id)
        .into_iter()
        .find(|k| k.ends_with("_STRATEGIES"))
        .unwrap();
    if strategies.is_empty() {
        crate::env_file::remove_env_vars(env_path, &[&key]).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(".env strategies remove failed: {e}"),
            )
        })?;
    } else {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert(key, strategies.join(","));
        env_upsert(env_path, &vars, "strategies")?;
    }
    Ok(())
}

/// `account.rounds.set`'s persisted half — `ACCOUNT_<ID>_ROUNDS` (empty list
/// = remove the key: absence is "no declared family set").
pub fn env_persist_account_rounds(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    rounds: &[i64],
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "rounds")?;
    let key = account_env_keys(account_id)
        .into_iter()
        .find(|k| k.ends_with("_ROUNDS"))
        .unwrap();
    if rounds.is_empty() {
        crate::env_file::remove_env_vars(env_path, &[&key]).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(".env rounds remove failed: {e}"),
            )
        })?;
    } else {
        let joined = rounds
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert(key, joined);
        env_upsert(env_path, &vars, "rounds")?;
    }
    Ok(())
}

/// `account.remove`'s persisted half — drop the id from ACCOUNT_LIST and
/// sweep ALL its ACCOUNT_<ID>_* keys in the same write. Refuses when the id
/// is not in the list, and when the removal would empty the book (the next
/// boot refuses an empty ACCOUNT_LIST; delete the keys themselves for the
/// single-default deployment). The display pointer, when it named the
/// removed id, is re-derived in the same write (explicit > first active >
/// first row — the loader's own order; this layer re-derives from STATUS
/// keys because it cannot see a runtime book).
pub fn env_persist_account_remove(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "remove")?;
    let lookup = env_lookup_for_write(env_path);
    let list = current_account_list(&lookup)?;
    // issue 753: never sweep a key space two ids share — the loader refuses
    // such a book at boot, and the delete face must refuse it too rather than
    // silently wipe the alias's venue/trading/credential row.
    refuse_env_suffix_collisions(&list)?;
    if !list.iter().any(|a| a == account_id) {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("ACCOUNT_LIST names no `{account_id}`"),
        ));
    }
    if list.len() == 1 {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "removing `{account_id}` would leave ACCOUNT_LIST empty — the \
                 next boot refuses an empty book; delete the ACCOUNT_* keys \
                 themselves (single default account) instead"
            ),
        ));
    }
    let remaining: Vec<String> = list.into_iter().filter(|a| a != account_id).collect();
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("ACCOUNT_LIST".to_string(), remaining.join(","));
    // Re-derive the display pointer when it named the removed id (explicit >
    // first active > first row — the loader's own order; this layer reads
    // STATUS keys because it cannot see a runtime book).
    let mut sweep = account_env_keys(account_id);
    let display_now = lookup("ACCOUNT_DISPLAY").map(|v| v.trim().to_string());
    if display_now.as_deref() == Some(account_id) {
        let derived = remaining
            .iter()
            .find(|id| {
                lookup(&format!("ACCOUNT_{}_STATUS", env_suffix(id)))
                    .map(|s| s.trim().eq_ignore_ascii_case("active") || s.trim().is_empty())
                    .unwrap_or(true)
            })
            .or_else(|| remaining.first())
            .cloned();
        match derived {
            Some(d) => {
                vars.insert("ACCOUNT_DISPLAY".to_string(), d);
            }
            None => sweep.push("ACCOUNT_DISPLAY".to_string()),
        }
    }
    let sweep_refs: Vec<&str> = sweep.iter().map(|s| s.as_str()).collect();
    let removed = crate::env_file::remove_env_vars(env_path, &sweep_refs).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(".env remove sweep failed: {e}"),
        )
    })?;
    let _ = removed;
    env_upsert(env_path, &vars, "remove")
}

/// `display.account.set`'s persisted half — `ACCOUNT_DISPLAY` upsert.
pub fn env_persist_display_account(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "display")?;
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("ACCOUNT_DISPLAY".to_string(), account_id.to_string());
    env_upsert(env_path, &vars, "display")
}

/// `arb.config.set`'s persisted half — the five ACCOUNT_ARB_* keys upserted
/// as one write. The CALLER owns the semantic validations (distinct venues,
/// ids exist, pairs non-empty) — it has the book.
pub fn env_persist_cross_venue_arb(
    toml_path: &Path,
    env_path: &Path,
    cfg: &CrossVenueArbConfig,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "arb config")?;
    let mut vars = std::collections::BTreeMap::new();
    vars.insert(
        "ACCOUNT_ARB_LEG_A_VENUE".to_string(),
        cfg.leg_a_venue.trim().to_string(),
    );
    vars.insert(
        "ACCOUNT_ARB_LEG_A_ACCOUNT".to_string(),
        cfg.leg_a_account.trim().to_string(),
    );
    vars.insert(
        "ACCOUNT_ARB_LEG_B_VENUE".to_string(),
        cfg.leg_b_venue.trim().to_string(),
    );
    vars.insert(
        "ACCOUNT_ARB_LEG_B_ACCOUNT".to_string(),
        cfg.leg_b_account.trim().to_string(),
    );
    vars.insert(
        "ACCOUNT_ARB_PAIRS".to_string(),
        cfg.pairs
            .iter()
            .map(|p| p.trim().to_uppercase())
            .collect::<Vec<_>>()
            .join(","),
    );
    env_upsert(env_path, &vars, "arb config")
}

/// `arb.config.clear`'s persisted half — sweep the five ACCOUNT_ARB_* keys.
pub fn env_clear_cross_venue_arb(toml_path: &Path, env_path: &Path) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "arb config clear")?;
    crate::env_file::remove_env_vars(
        env_path,
        &[
            "ACCOUNT_ARB_LEG_A_VENUE",
            "ACCOUNT_ARB_LEG_A_ACCOUNT",
            "ACCOUNT_ARB_LEG_B_VENUE",
            "ACCOUNT_ARB_LEG_B_ACCOUNT",
            "ACCOUNT_ARB_PAIRS",
        ],
    )
    .map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(".env arb config clear failed: {e}"),
        )
    })?;
    Ok(())
}
