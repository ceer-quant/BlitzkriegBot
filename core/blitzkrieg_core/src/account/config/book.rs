//! Split out of the parent module (issue #780): pure move.

use super::*;

/// Load the account book from ACCOUNT_* environment keys (issue 751).
/// `ACCOUNT_LIST` names the ids in book order; a missing/empty value →
/// `None` (the caller decides what no env book means — issue 923: the
/// zero-config single default; the legacy toml fallback is retired).
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
            opening_equity: None,
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
