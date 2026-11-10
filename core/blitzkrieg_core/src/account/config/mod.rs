//! The account book's config source is the `.env` ACCOUNT_* grammar (issue
//! 751, the .env single-source ruling) — not a config file. This module owns
//! that grammar: parsing the ACCOUNT_* keys (env-only credentials are probed
//! by [`read_credential_keys`]), the runtime write faces that upsert the keys,
//! and the RESIDUE handling of the legacy `user_layer/configs/accounts.toml`
//! the single-source ruling retired: the file is no longer a config source
//! and is never read back into the book — its only remaining role in the
//! loader is the existence check that shouts at the residue (the migration
//! script removed the factory copy; an operator one is deleted by hand or
//! migrated). The toml parse/lift machinery below (`from_str`, the file
//! types, the validate faces) stays because the RESIDUE-REFUSAL and write
//! faces still describe the same shapes, and because the parse types are the
//! wire contract the env grammar mirrors field for field.
//!
//! Zero config is legal: no ACCOUNT_* keys anywhere → the single `default`
//! account (§9.5's zero-config deployment — one active account, no keys
//! named, the plugin's own env handling untouched). A PRESENT but broken
//! source (a bad status, a rounds value no plugin declared, a dangling
//! display id, a duplicate id) is refused loudly (fail-closed): a config
//! typo must never silently shrink the book to "one default account",
//! because the operator believes the restrictions in it are live.

use super::{
    Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys, default_account_id,
};
use crate::model::{CoreError, CoreErrorCode};
use blitzkrieg_market_api::MarketType;
use serde::{Deserialize, Serialize};
use std::path::Path;

mod book;
pub use book::*;

#[cfg(test)]
mod config_tests;

/// The legacy config file's path spelling (§9.5). NOT a config source any
/// more (issue 751): no loader reads it; `CoreConfig::account_env_write_paths`
/// still carries the path because a live file at this path REFUSES every
/// runtime write and the loader SHOUTS at it as residue.
pub const ACCOUNTS_CONFIG_PATH: &str = "user_layer/configs/accounts.toml";

/// One `[[account]]` table as it sits on disk. Credential fields hold the
/// NAMES of environment variables — §9.4: the value never appears in any
/// config file, log, or IPC answer.
///
/// `Serialize` exists for the toml render path the cross-venue-arb / display
/// faces once ran; the wire types are separate (§12.1), so this never leaves
/// the config layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct AccountFileEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    /// Missing `market_type` defaults to `prediction` — the only market type
    /// a plugin exists for in 0.3 (§9.6).
    #[serde(default)]
    market_type: Option<MarketType>,
    /// Missing `status` defaults to `active` (the file only ever carries
    /// restrictions; `deny_unknown_fields` keeps a typo'd variant from
    /// parsing as something permissive).
    #[serde(default)]
    status: Option<AccountStatus>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    secret_env: Option<String>,
    #[serde(default)]
    passphrase_env: Option<String>,
    #[serde(default)]
    private_key_env: Option<String>,
    /// The optional account↔venue binding: a market-plugin name the account
    /// is pinned to. Missing/empty = unbound (trades any venue — the
    /// historical behaviour; `deny_unknown_fields` is why the field must be
    /// declared here at all).
    #[serde(default)]
    venue: Option<String>,
    /// Missing = `true` (a trading account — the historical shape of a file
    /// that predates the switch). `false` = the account refuses new entries
    /// at Gate 2; its exits keep flowing.
    #[serde(default)]
    trading_enabled: Option<bool>,
    /// THIS account's strategy ENABLEMENT set (账户↔venue 解耦语义裁决) —
    /// the persisted state of the panel's per-(account, strategy) 启用按钮.
    /// NOT an assignment and NOT a visibility filter: strategies are GLOBAL —
    /// every installed package is visible to every account; this list names
    /// the ones whose (account, strategy) scope actually RUNS. Missing = the
    /// legacy pre-#552 shape (kept parse-compatible); EMPTY is the explicit
    /// "run nothing on this account" set — INHERIT is retired (#552), an
    /// empty list never follows the global toggle (`single_default` produces
    /// an empty list = zero-config accounts run nothing until enabled). A
    /// non-empty list is the enablement set verbatim (order kept for a
    /// stable diff). Names are validated at write time (`strategy.enable` /
    /// `account.strategies.set`), not at parse.
    #[serde(default)]
    strategies: Option<Vec<String>>,
    /// THIS account's round-length FAMILY declaration (轮次多族): the
    /// (single) durations, in seconds, this account trades — each combined
    /// with the account's venue (or every venue when unbound) into the
    /// (venue, duration) family set the B1c admission gate reads. Missing
    /// = the legacy shape: NO family restriction is declared and the
    /// account trades every duration the data plane feeds (the gate's
    /// empty-set reading — today's behaviour, byte-identical). EMPTY is
    /// ALSO unrestricted (rounds = [] spells "no declared family set", NOT
    /// "subscribed to nothing"): durations are an ADDED permission axis,
    /// not a subtracted one, so an empty list must never lock an account
    /// out of trading — the zero-config spellings and the explicit one
    /// converge on the same verdict. Values are validated against the
    /// market plugins' declared round-duration union AT PARSE (fail-closed,
    /// the same discipline every other key here runs: a typo'd 9000 must not
    /// silently become "no restriction"); duplicates are removed with
    /// first-seen order kept (a stable diff reads better than a
    /// canonical sort). A non-empty set does NOT clip the global chain:
    /// the round-timing math (B1a's three-state read) still falls back to
    /// the global single value for grids this account never declared.
    #[serde(default)]
    rounds: Option<Vec<i64>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountsFile {
    account: Vec<AccountFileEntry>,
    /// The DISPLAY account (显示账户配置化): which account the snapshot's
    /// `displayAccount`/`displayVenue` point at. A pure presentation pointer —
    /// it is NOT the active/accounting/strategy-enablement axis, and no
    /// trading semantics attach to it. A top-level key (sibling of
    /// `[[account]]`, never a field inside one). Missing = the file's FIRST
    /// `status=active` account (falling back to the first entry when no entry
    /// is active); an id the file does not declare REFUSES the boot
    /// (fail-closed: a dangling pointer would otherwise silently fall back to
    /// some account, which is exactly the 「账户≠行情页交易所」 accident this
    /// key exists to make explicit).
    #[serde(default)]
    display_account: Option<String>,
    /// The cross-venue arbitrage leg configuration (跨所套利弹窗 v2): user-
    /// chosen A/B legs (venue, account) + the asset pairs the engine pairs on.
    /// A top-level `[cross_venue_arb]` table, sibling of `[[account]]` — the
    /// same durable operator state `account.venue.set` and friends persist
    /// into this file. Missing = unconfigured (the pre-v2 shape: the legacy
    /// `unified.confirm` chain stays the only pairing source). A section that
    /// PARSES but is incoherent (identical venues, an account id no
    /// `[[account]]` declares, an empty pairs list) REFUSES the boot —
    /// fail-closed, exactly the discipline every other key in this file runs
    /// under: a half-written arb config must never silently trade.
    #[serde(default)]
    cross_venue_arb: Option<CrossVenueArbFileSection>,
}

/// One `[cross_venue_arb]` section as it sits on disk. `cross_venue_arb` is
/// the STRATEGY PACKAGE NAME the config feeds — the engine pushes these
/// values into that package's `bk.params()` bag, and the lua_loader's
/// account-stamp reads them as the venue→account leg map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CrossVenueArbFileSection {
    leg_a_venue: String,
    leg_a_account: String,
    leg_b_venue: String,
    leg_b_account: String,
    pairs: Vec<String>,
}

/// The public reading of a `[cross_venue_arb]` section (the parse type is
/// file-private; the wire and the engine see THIS shape). `CrossVenueArbStatus`
/// is the runtime truth the IPC get answers from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossVenueArbConfig {
    pub leg_a_venue: String,
    pub leg_a_account: String,
    pub leg_b_venue: String,
    pub leg_b_account: String,
    /// Upper-cased asset names, file order kept.
    pub pairs: Vec<String>,
}

impl CrossVenueArbConfig {
    /// The venue→account leg map the adapter's account-stamp consults: the
    /// configured venue trades under its OWN leg account; any other venue
    /// falls back to the instance's own account.
    pub fn leg_account_for_venue(&self, venue: &str) -> Option<&str> {
        let v = venue.trim();
        if v == self.leg_a_venue.trim() {
            Some(self.leg_a_account.trim())
        } else if v == self.leg_b_venue.trim() {
            Some(self.leg_b_account.trim())
        } else {
            None
        }
    }

    /// Whether `asset` is in the configured pairs set (case-insensitive — the
    /// file may spell `btc`, the scanner spells `BTC`).
    pub fn admits_asset(&self, asset: &str) -> bool {
        let up = asset.trim().to_uppercase();
        self.pairs.iter().any(|p| p.trim().to_uppercase() == up)
    }

    /// The `bk.params()` entries the strategy reads its configuration from
    /// (the st.params push path — no new strategy API). Values are the file
    /// spellings trimmed; `pairs` joins on `,` (the strategy splits it).
    pub fn as_params(&self) -> Vec<(String, String)> {
        vec![
            (
                "arb_leg_a_venue".to_string(),
                self.leg_a_venue.trim().to_string(),
            ),
            (
                "arb_leg_a_account".to_string(),
                self.leg_a_account.trim().to_string(),
            ),
            (
                "arb_leg_b_venue".to_string(),
                self.leg_b_venue.trim().to_string(),
            ),
            (
                "arb_leg_b_account".to_string(),
                self.leg_b_account.trim().to_string(),
            ),
            (
                "arb_pairs".to_string(),
                self.pairs
                    .iter()
                    .map(|p| p.trim().to_uppercase())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ]
    }
}

impl AccountFileEntry {
    fn into_account(self, now_ms: i64, allowed: &[i64]) -> Result<Account, CoreError> {
        if self.id.trim().is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "accounts.toml: an [[account]] entry has an empty id — an \
                 account without an identity cannot be addressed or audited",
            ));
        }
        Ok(Account {
            id: AccountId::from(self.id.as_str()),
            name: self.name.unwrap_or_else(|| self.id.clone()),
            market_type: self.market_type.unwrap_or(MarketType::Prediction),
            status: self.status.unwrap_or(AccountStatus::Active),
            credential_keys: CredentialKeys {
                api_key_env: self.api_key_env,
                secret_env: self.secret_env,
                passphrase_env: self.passphrase_env,
                private_key_env: self.private_key_env,
                // Set by the kernel after probing its environment
                // (`read_credential_keys`); a config file can only name keys.
                loaded: false,
            },
            // The account↔venue binding (§9.1 first batch): empty = unbound —
            // the account trades any venue, the historical behaviour. A
            // non-empty value pins `orders.place` to that venue's markets.
            venue: self.venue.unwrap_or_default(),
            trading_enabled: self.trading_enabled.unwrap_or(true),
            strategies: self.strategies.unwrap_or_default(),
            rounds: validate_rounds(self.rounds, allowed)?,
            opening_equity: None,
            updated_at_ms: now_ms,
        })
    }
}

/// Test-fixture helper: the historical kernel whitelist, now spelled as the
/// plugins' declared union (polymarket's five families ∪ kalshi's 900). Tests
/// pass this where production threads the registry union through
/// [`load_account_ledgers`] / [`validate_rounds`], so no test invents new
/// assertion semantics — the values are exactly what the shipped plugins
/// declare. `pub` (not `pub(crate)`) because the integration tests are
/// external crates; `#[doc(hidden)]` keeps it out of the documented API.
#[doc(hidden)]
pub const TEST_ALLOWED_ROUNDS: &[i64] = &[300, 900, 3600, 14400, 86400];

/// The parse-side validation of one account's `rounds` declaration
/// (轮次多族). `None`/empty both come out as an EMPTY vec — the gate's
/// "no family restriction declared" reading (rounds is a permission axis:
/// an empty declaration must never lock the account out of trading).
/// Values outside the market plugins' DECLARED union (`allowed` — the
/// registry's `allowed_round_secs` the boot threads through) refuse the
/// WHOLE boot (fail-closed: a typo'd duration must not silently become
/// "no restriction" — the operator believes the restriction is live).
/// Duplicates are dropped with first-seen order kept.
pub(crate) fn validate_rounds(
    rounds: Option<Vec<i64>>,
    allowed: &[i64],
) -> Result<Vec<i64>, CoreError> {
    validate_rounds_from("accounts.toml: `rounds`", rounds, allowed)
}

/// [`validate_rounds`] with a caller-supplied source label so the env grammar
/// can name the actual key (`ACCOUNT_<ID>_ROUNDS`) in a refusal.
fn validate_rounds_from(
    source: &str,
    rounds: Option<Vec<i64>>,
    allowed: &[i64],
) -> Result<Vec<i64>, CoreError> {
    let Some(rounds) = rounds else {
        return Ok(Vec::new());
    };
    let mut out: Vec<i64> = Vec::with_capacity(rounds.len());
    for sec in rounds {
        if !allowed.contains(&sec) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "{source} names {sec}s, which is not declared by any \
                     market plugin (declared union: {allowed:?}) — fix the value or delete \
                     the key (fail-closed: a declared set that cannot be honoured must not \
                     degrade into \"no restriction\")"
                ),
            ));
        }
        if !out.contains(&sec) {
            out.push(sec);
        }
    }
    Ok(out)
}

/// Load the account book from `path` (§9.5). Missing file → the single
/// `default` account (zero config change for a 0.2 deployment); a malformed,
/// empty, or duplicated-id file is a hard error (fail-closed) — the caller
/// refuses the boot rather than trading with a book that is not what the
/// operator wrote.
///
/// `allowed` is the market plugins' DECLARED round-duration union (the
/// registry's `allowed_round_secs` the boot threads through) — the parse side
/// refuses an account `rounds` row naming a duration no plugin declared, but
/// judges nothing on its own: which durations are legal is venue knowledge
/// (§7.2), reported by the declaration face, not a kernel constant.
///
/// This is the TOML-ONLY reader: the tests' parse-text entry (via
/// [`from_str`]) and the legacy fixture reader. NO production path calls it
/// since issue 923 removed the migration-window fallback — the boot and the
/// reload read the ACCOUNT_* env grammar only.
pub fn load_account_ledgers(
    path: &Path,
    now_ms: i64,
    allowed: &[i64],
) -> Result<AccountLedgers, CoreError> {
    let file_present = path.exists();
    if !file_present {
        return Ok(AccountLedgers::single_default(now_ms));
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable: {e}"),
        )
    })?;
    from_str(&text, now_ms, allowed)
}

/// The production boot entry (issue 751, the .env single-source ruling;
/// issue 923 retired the fallback): the book comes from the ACCOUNT_* keys —
/// process environment first, then the `.env` file the launcher owns. There
/// is NO legacy toml fallback any more: a boot with no ACCOUNT_* keys
/// anywhere is the zero-config deployment and loads the single `default`
/// account.
///
/// `legacy_path` names the retired `accounts.toml` — no longer read for
/// configuration; its existence is only a RESIDUE signal: when the file is
/// still on disk (the factory copy was deleted from the repo and production;
/// an operator copy migrated otherwise) the boot warns that it is IGNORED so
/// a stale file never silently masquerades as the live book. `env_path` is
/// the `.env` file (the same one `env_file_path` names — `None` skips the
/// file face and reads the process environment alone).
pub fn load_account_ledgers_single_source(
    legacy_path: &Path,
    env_path: Option<&Path>,
    now_ms: i64,
    allowed: &[i64],
) -> Result<AccountLedgers, CoreError> {
    let env = env_lookup_from_sources(env_path);
    let from_env = load_account_ledgers_from_env(now_ms, allowed, &env)?;
    match from_env {
        Some(book) => {
            if legacy_path.exists() {
                tracing::warn!(
                    "accounts: ACCOUNT_* keys are in force; the legacy {} still \
                     exists and is IGNORED — delete it \
                     (scripts/migrate-config-to-env.sh)",
                    legacy_path.display()
                );
            }
            Ok(book)
        }
        None => {
            // Issue 923: the toml fallback is REMOVED — env accounts only.
            // No ACCOUNT_* keys anywhere is the zero-config deployment: the
            // single default account. A residue file at the legacy path is
            // still shouted at (same family as the env-wins branch) so an
            // operator who still believes the toml rules the book learns it
            // does not.
            if legacy_path.exists() {
                tracing::warn!(
                    "accounts: NO ACCOUNT_* keys found — booting the zero-config \
                     single-default book; the legacy {} is no longer a config \
                     source (the toml fallback was removed, issue 751 / issue \
                     923) and is IGNORED — run scripts/migrate-config-to-env.sh \
                     to migrate or delete it",
                    legacy_path.display()
                );
            }
            Ok(AccountLedgers::single_default(now_ms))
        }
    }
}

/// Reload entry: the edited file must beat the process copy loaded at startup.
/// Env-only (issue 923): the ACCOUNT_* keys from the `.env` file decide the
/// whole book — no ACCOUNT_LIST in the file reloads the zero-config single
/// default (no legacy toml fallback exists any more). A malformed `.env`
/// (unreadable, or keys that fail the fail-closed parse) refuses the reload
/// and the caller keeps the current configuration.
pub fn load_account_ledgers_for_reload(
    env_path: &Path,
    now_ms: i64,
    allowed: &[i64],
) -> Result<AccountLedgers, CoreError> {
    let text = std::fs::read_to_string(env_path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("{} unreadable: {e}", env_path.display()),
        )
    })?;
    let file_map: std::collections::HashMap<String, String> = text
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    match load_account_ledgers_from_env(now_ms, allowed, &|key| file_map.get(key).cloned())? {
        Some(book) => Ok(book),
        None => Ok(AccountLedgers::single_default(now_ms)),
    }
}

/// Parse the toml text into the book. Split from [`load_account_ledgers`] so
/// tests can feed strings directly. The ENV grammar mirrors this parse face
/// shape for shape (the same refusals, the same defaults); production loads
/// the env face, never this file text.
pub fn from_str(text: &str, now_ms: i64, allowed: &[i64]) -> Result<AccountLedgers, CoreError> {
    let file: AccountsFile = toml::from_str(text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse: {e}"),
        )
    })?;
    if file.account.is_empty() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "accounts.toml parses but declares no [[account]] — either delete \
             the file (single default account) or declare at least one account",
        ));
    }
    // issue 753: reject ids that collapse to one ACCOUNT_<SFX>_* key space
    // BEFORE parsing any table — the boot must not accept a legacy book the
    // migration would later turn into colliding env keys.
    refuse_env_suffix_collisions(
        &file
            .account
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut accounts = Vec::new();
    for entry in file.account {
        let mut account = entry.into_account(now_ms, allowed)?;
        // E28 (§9.4): the kernel probes ITS OWN environment at load time so
        // `account.list` can answer `credentialsLoaded` as a boolean. The
        // values themselves never enter any struct.
        account.credential_keys = read_credential_keys(&account.credential_keys);
        accounts.push(account);
    }
    // Duplicate ids would silently collapse into one HashMap slot: the second
    // table would win the status and the first would keep trading. Refuse.
    let mut seen = std::collections::HashSet::new();
    for a in &accounts {
        if !seen.insert(a.id.clone()) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml declares account id `{}` twice", a.id),
            ));
        }
    }
    // §9.5: the process-level active account is `default` when the file
    // configures one, else the file's FIRST entry — `AccountLedgers`'s
    // invariant ("the active id always names a configured account") must hold
    // at construction, or the first `active_ledger()` read would panic on a
    // book whose file simply didn't name `default`.
    let active = if accounts.iter().any(|a| a.id == default_account_id()) {
        default_account_id()
    } else {
        accounts[0].id.clone()
    };
    // The display pointer (显示账户配置化): explicit key > first
    // `status=active` entry > first entry. An explicit id that names no
    // configured account refuses here — the boot's fail-closed posture, the
    // same one a malformed file already gets (a dangling display pointer must
    // never silently fall back to some other account: the snapshot would show
    // an exchange the operator never chose, the exact accident the key
    // exists to make explicit).
    let display = match file.display_account.as_deref() {
        Some(id) => {
            if !accounts.iter().any(|a| a.id.as_str() == id) {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "accounts.toml: `display_account = \"{id}\"` names no \
                         `[[account]]` — the display pointer must name a \
                         configured account (fail-closed: fix the id or delete \
                         the key)"
                    ),
                ));
            }
            id.to_string()
        }
        None => accounts
            .iter()
            .find(|a| matches!(a.status, AccountStatus::Active))
            .map(|a| a.id.as_str().to_string())
            .unwrap_or_else(|| accounts[0].id.as_str().to_string()),
    };
    // The cross-venue arb section (跨所套利弹窗 v2): a section that exists
    // must be COHERENT or the boot refuses — the same fail-closed posture the
    // display pointer runs (a dangling leg would silently trade one account's
    // money under a pairing the file cannot support).
    if let Some(arb) = &file.cross_venue_arb {
        validate_cross_venue_arb_section(
            &lift_cross_venue_arb(arb),
            &accounts
                .iter()
                .map(|a| a.id.as_str().to_string())
                .collect::<Vec<_>>(),
        )?;
    }
    let book =
        AccountLedgers::new_with_display(accounts, active, AccountId::from(display.as_str()));
    // #778(a): boot configured several accounts on one venue — they will share
    // that venue's GLOBAL env credential (one wallet, several ledgers). Warn
    // per collision; the binding is still accepted (live configs may rely on
    // it) — the operator owns the ban/per-order-credentials decision.
    book.warn_shared_venue_bindings();
    Ok(book)
}

/// The file-section → public-config lift shared by the boot loader and the
/// read path (`None` = the section is absent = unconfigured).
fn lift_cross_venue_arb(a: &CrossVenueArbFileSection) -> CrossVenueArbConfig {
    CrossVenueArbConfig {
        leg_a_venue: a.leg_a_venue.trim().to_string(),
        leg_a_account: a.leg_a_account.trim().to_string(),
        leg_b_venue: a.leg_b_venue.trim().to_string(),
        leg_b_account: a.leg_b_account.trim().to_string(),
        pairs: a.pairs.iter().map(|p| p.trim().to_uppercase()).collect(),
    }
}

/// The coherence rules a `[cross_venue_arb]` section must satisfy, shared by
/// the boot loader (`from_str`) and the runtime write path (`persist_…`'s
/// caller validates BEFORE the file is touched — a refused set moves nothing).
/// Every refusal names the offending field.
pub fn validate_cross_venue_arb_section(
    arb: &CrossVenueArbConfig,
    account_ids: &[String],
) -> Result<(), CoreError> {
    let err = |field: &str, msg: String| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml [cross_venue_arb]: {field} — {msg}"),
        )
    };
    if arb.leg_a_venue.trim().is_empty() {
        return Err(err("leg_a_venue", "must be a non-empty venue name".into()));
    }
    if arb.leg_b_venue.trim().is_empty() {
        return Err(err("leg_b_venue", "must be a non-empty venue name".into()));
    }
    if arb.leg_a_venue.trim() == arb.leg_b_venue.trim() {
        return Err(err(
            "leg_b_venue",
            format!(
                "`{}` is the same venue as leg_a_venue — the two legs must \
                 sit on DIFFERENT venues (the pair exists to span them)",
                arb.leg_b_venue.trim()
            ),
        ));
    }
    if arb.leg_a_account.trim().is_empty() {
        return Err(err(
            "leg_a_account",
            "must be a non-empty account id".into(),
        ));
    }
    if arb.leg_b_account.trim().is_empty() {
        return Err(err(
            "leg_b_account",
            "must be a non-empty account id".into(),
        ));
    }
    for (field, id) in [
        ("leg_a_account", &arb.leg_a_account),
        ("leg_b_account", &arb.leg_b_account),
    ] {
        if !account_ids.iter().any(|a| a == id.trim()) {
            return Err(err(
                field,
                format!(
                    "`{}` names no `[[account]]` in this book — configured \
                     accounts are: {}",
                    id.trim(),
                    if account_ids.is_empty() {
                        "(none)".to_string()
                    } else {
                        account_ids.join(", ")
                    }
                ),
            ));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for p in &arb.pairs {
        let up = p.trim().to_uppercase();
        if up.is_empty() {
            return Err(err("pairs", "an entry is empty — name an asset".into()));
        }
        if !seen.insert(up.clone()) {
            return Err(err(
                "pairs",
                format!("`{}` appears twice — the list is a set", up),
            ));
        }
    }
    if arb.pairs.is_empty() {
        return Err(err(
            "pairs",
            "must name at least one asset — an empty list pairs nothing \
             (delete the section to unconfigure instead)"
                .into(),
        ));
    }
    Ok(())
}

/// The persisted write of a NEW account (`account.create`): append one
/// `[[account]]` row to the book — read-modify-write over the whole file, the
/// exact posture the retired toml venue writer ran (a non-parsing file is
/// REFUSED, never flattened; atomic same-directory temp + rename; the toml
/// re-render does not carry the operator's comments — the same accepted
/// trade-off, every FIELD round-trips because the parse is
/// `deny_unknown_fields`). The row carries `id`/`name`/`venue` only:
/// `market_type` and `status` are the file's own defaults (`prediction`,
/// `active`) — a runtime mint that wrote an explicit restriction would be a
/// loosening spelled backwards; tightening stays `account.status`'s job.
/// Credential ENV-NAMES are NOT written here — the create call carries no
/// credential surface at all (§9.4); the panel edits the file (or a later
/// venue-style verb) for key names.
pub fn persist_account_create(
    path: &Path,
    account_id: &str,
    name: Option<&str>,
    venue: Option<&str>,
) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the create write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the create write: {e}"),
        )
    })?;
    if file.account.iter().any(|a| a.id == account_id) {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml already declares `[[account]]` with id `{account_id}`"),
        ));
    }
    // issue 753: a new row may not alias an existing id in the env key space
    // (`paper-a` vs `paper_a`) — checked before the push/render, so a refusal
    // leaves the original file byte-identical.
    {
        let mut ids: Vec<String> = file.account.iter().map(|a| a.id.clone()).collect();
        ids.push(account_id.to_string());
        refuse_env_suffix_collisions(&ids)?;
    }
    file.account.push(AccountFileEntry {
        id: account_id.to_string(),
        name: name.map(|n| n.to_string()),
        market_type: None,
        status: None,
        api_key_env: None,
        secret_env: None,
        passphrase_env: None,
        private_key_env: None,
        venue: venue.filter(|v| !v.is_empty()).map(|v| v.to_string()),
        trading_enabled: None,
        strategies: None,
        rounds: None,
    });
    let out = toml::to_string(&file).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not render: {e}"),
        )
    })?;
    write_accounts_file_atomic(path, &out, "create")
}

/// The persisted write of ONE account's trading switch
/// (`account.trading.set`): read-modify-write over the whole file, the exact
/// fail-closed posture [`persist_account_create`] runs (a non-parsing file is
/// REFUSED, never flattened; same-directory temp + rename; the re-render
/// does not carry comments — every FIELD round-trips). `false` is written
/// explicitly; `true` REMOVES the key (the file spells "trading" by
/// absence, the same shape a pre-switch file had).
pub fn persist_account_trading(
    path: &Path,
    account_id: &str,
    enabled: bool,
) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the trading write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the trading write: {e}"),
        )
    })?;
    let target = file
        .account
        .iter_mut()
        .find(|a| a.id == account_id)
        .ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml declares no `[[account]]` with id `{account_id}`"),
            )
        })?;
    target.trading_enabled = Some(enabled).filter(|v| !v);
    write_accounts_file_atomic(
        path,
        &toml::to_string(&file).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml does not render: {e}"),
            )
        })?,
        "trading",
    )
}

/// The persisted write of ONE account's strategy ENABLEMENT set
/// (`account.strategies.set` / the per-account half of `strategy.enable`).
/// An EMPTY list REMOVES the key — the file spells "this account runs no
/// strategies" by absence (per-account enablement is the ONLY truth: the
/// old "absence = inherit the global toggle" reading is retired; an absent
/// key on a fresh deployment still yields a silent default account, which
/// is the zero-config shape the boot path observes). A non-empty list is
/// the enablement set stored verbatim. The write-time validation against
/// the registered set is the CALLER's job (it has the plugin/strategy
/// registry; this layer only owns the file).
pub fn persist_account_strategies(
    path: &Path,
    account_id: &str,
    strategies: &[String],
) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the strategies write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the strategies write: {e}"),
        )
    })?;
    let target = file
        .account
        .iter_mut()
        .find(|a| a.id == account_id)
        .ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml declares no `[[account]]` with id `{account_id}`"),
            )
        })?;
    target.strategies = if strategies.is_empty() {
        None
    } else {
        Some(strategies.to_vec())
    };
    write_accounts_file_atomic(
        path,
        &toml::to_string(&file).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml does not render: {e}"),
            )
        })?,
        "strategies",
    )
}

/// The persisted write of ONE account's round-length FAMILY set
/// (`account.rounds.set`, 轮次多族 B2). Read-modify-write over the whole
/// file, the exact posture [`persist_account_create`] runs (a non-parsing
/// file is REFUSED, never flattened; atomic same-directory temp + rename via
/// [`write_accounts_file_atomic`]; the toml re-render does not carry the
/// operator's comments — the same accepted trade-off, every FIELD
/// round-trips because the parse is `deny_unknown_fields`).
///
/// An EMPTY (or already-deduplicated-to-empty) list REMOVES the `rounds`
/// key — the file spells "no declared family set" by absence, the exact
/// shape a pre-declaration file had; missing and `[]` parse identically at
/// the next boot, and a deleted key is cleaner than a persisted empty array.
/// The union/dedup validation is the CALLER's job (the service layer
/// reuses [`validate_rounds`] before it ever calls here); this layer only
/// owns the file.
pub fn persist_account_rounds(
    path: &Path,
    account_id: &str,
    rounds: &[i64],
) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the rounds write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the rounds write: {e}"),
        )
    })?;
    let target = file
        .account
        .iter_mut()
        .find(|a| a.id == account_id)
        .ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml declares no `[[account]]` with id `{account_id}`"),
            )
        })?;
    target.rounds = if rounds.is_empty() {
        None
    } else {
        Some(rounds.to_vec())
    };
    write_accounts_file_atomic(
        path,
        &toml::to_string(&file).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml does not render: {e}"),
            )
        })?,
        "rounds",
    )
}

/// The persisted removal of ONE account (`account.remove`): drop the named
/// `[[account]]` from the book — read-modify-write, the same fail-closed
/// posture as every accounts.toml writer above. The CALLER owns the
/// refusals that need runtime facts (open positions, the last-account
/// guard); this layer refuses only what the file itself decides (an unknown
/// id, a file that does not parse) and never leaves the last row
/// removable-broken: an empty book would fail the next boot's parse, so
/// removing the FINAL row is refused here with the honest diagnostic.
/// `default` is not special-cased — every account is removable.
pub fn persist_account_remove(path: &Path, account_id: &str) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the remove write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the remove write: {e}"),
        )
    })?;
    let before = file.account.len();
    file.account.retain(|a| a.id != account_id);
    if file.account.len() == before {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml declares no `[[account]]` with id `{account_id}`"),
        ));
    }
    if file.account.is_empty() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "removing `[[account]]` `{account_id}` would leave accounts.toml \
                 empty — the next boot refuses an empty book; delete the file \
                 itself (single default account) instead"
            ),
        ));
    }
    // The display pointer must not survive the removal as a dangling id —
    // the next boot's loader refuses an unknown `display_account`
    // (fail-closed), so a removal that takes the displayed account away
    // re-derives the key IN THE SAME ATOMIC WRITE (explicit > first active >
    // first row, the loader's own order). Removing a non-displayed account
    // leaves the pointer untouched.
    if file.display_account.as_deref() == Some(account_id) {
        file.display_account = file
            .account
            .iter()
            .find(|a| matches!(a.status, Some(AccountStatus::Active) | None))
            .map(|a| a.id.clone())
            .or_else(|| file.account.first().map(|a| a.id.clone()));
    }
    let out = toml::to_string(&file).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not render: {e}"),
        )
    })?;
    write_accounts_file_atomic(path, &out, "remove")
}

/// The persisted write-back of the DISPLAY pointer (`display.account.set`):
/// set the top-level `display_account` key of accounts.toml — read-modify-write
/// over the whole file, the exact fail-closed posture every accounts.toml
/// writer above runs (a non-parsing file is REFUSED, never flattened;
/// same-directory temp + rename via [`write_accounts_file_atomic`]; the toml
/// re-render does not carry the operator's comments — the same accepted
/// trade-off, every FIELD round-trips because the parse is
/// `deny_unknown_fields`). The CALLER owns the runtime validations (the id
/// must exist and be `active`); this layer only owns the file.
pub fn persist_display_account(path: &Path, account_id: &str) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the display write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the display write: {e}"),
        )
    })?;
    file.display_account = Some(account_id.to_string());
    write_accounts_file_atomic(
        path,
        &toml::to_string(&file).map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml does not render: {e}"),
            )
        })?,
        "display",
    )
}

/// The atomic tail both accounts.toml writers share: write the sibling temp
/// first, then rename over the original — the boot's read and a concurrent
/// reload can never see a partial document. `op` names the caller in the
/// diagnostics (the same spelling each writer's own messages carry).
fn write_accounts_file_atomic(path: &Path, contents: &str, op: &str) -> Result<(), CoreError> {
    let tmp = path.with_extension(format!("toml.{op}.tmp"));
    std::fs::write(&tmp, contents)
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("accounts.toml {op} write failed: {e}"),
            )
        })
        .and_then(|()| {
            std::fs::rename(&tmp, path).map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("accounts.toml {op} rename failed: {e}"),
                )
            })
        })
}

/// Probe the kernel's OWN environment for the configured key names (§9.4: the
/// kernel process is the only credential reader; strategies never see keys or
/// values). The returned `CredentialKeys` carries the names through untouched
/// and sets `loaded` — true only when at least one key is named AND every
/// named key resolved to a non-empty value. Values are never read into any
/// struct: presence is checked via `env::var_os`, and nothing here can leak
/// onto the wire (`account.list` answers `credentialsLoaded: bool`).
pub fn read_credential_keys(keys: &CredentialKeys) -> CredentialKeys {
    let named = [
        keys.api_key_env.as_deref(),
        keys.secret_env.as_deref(),
        keys.passphrase_env.as_deref(),
        keys.private_key_env.as_deref(),
    ];
    let any_named = named.iter().any(|k| k.is_some());
    let all_present = named.iter().all(|k| match k {
        None => true, // not configured: nothing to load, no failure
        Some(name) => std::env::var_os(name)
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    });
    CredentialKeys {
        api_key_env: keys.api_key_env.clone(),
        secret_env: keys.secret_env.clone(),
        passphrase_env: keys.passphrase_env.clone(),
        private_key_env: keys.private_key_env.clone(),
        loaded: any_named && all_present,
    }
}

// ── .env single-source (issue 751, 2026-10-28 operator ruling) ──────────────
//
// The operator retired the 10-23 judgment (ENV_MIGRATION.md §1/§6 kept
// accounts.toml): ALL account configuration now lives in .env under the
// ACCOUNT_* prefix, and every runtime write face (account.venue.set,
// account.rounds.set, account.strategies.set, account.trading.set,
// account.create/remove, display.account.set, arb.config.*) UPSERTS those
// keys through `crate::env_file` (atomic + 0600 + comment-preserving — the
// same primitive market.credentials.set runs).
//
// Key grammar, per account id spelled like the policy env suffix (uppercase,
// non-alphanumeric → `_`):
//   ACCOUNT_LIST                    comma-separated ids, book order (e.g.
//                                   "default,paper,polysimulator")
//   ACCOUNT_DISPLAY                 the display pointer (the old
//                                   `display_account` key)
//   ACCOUNT_<ID>_NAME / _VENUE / _STATUS / _TRADING_ENABLED /
//                   _STRATEGIES (comma list) / _ROUNDS (comma list) /
//                   _API_KEY_ENV / _SECRET_ENV / _PASSPHRASE_ENV /
//                   _PRIVATE_KEY_ENV   (credential key NAMES only, never values)
//   ACCOUNT_ARB_LEG_A_VENUE / _LEG_A_ACCOUNT / _LEG_B_VENUE / _LEG_B_ACCOUNT /
//                   _ARB_PAIRS (comma list)   — the [cross_venue_arb] legs
//
// Precedence (issue 923: the migration window is closed): the ACCOUNT_* keys
// are the ONLY source — no ACCOUNT_LIST anywhere is the zero-config
// deployment (single default). The legacy accounts.toml is no longer a fallback
// and no loader reads it: a residue file on disk is IGNORED and shouted at,
// and while it exists every runtime write face still refuses (one source —
// the operator deletes or migrates the file).

/// `default` for `ACCOUNT_<ID>_TRADING_ENABLED`; absent key = true.
fn env_status(raw: Option<String>) -> Result<AccountStatus, CoreError> {
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(AccountStatus::Active),
        Some("active") => Ok(AccountStatus::Active),
        Some("read_only") => Ok(AccountStatus::ReadOnly),
        Some("frozen") => Ok(AccountStatus::Frozen),
        Some("suspended") => Ok(AccountStatus::Suspended {
            reason: "suspended via .env (no reason spelled)".into(),
        }),
        Some(other) => Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "ACCOUNT_*_STATUS=`{other}` is not a status (active / \
                 read_only / frozen / suspended) — fix .env and restart \
                 (fail-closed, the same discipline the toml face runs)"
            ),
        )),
    }
}

/// The env-var suffix for one account id: uppercase, non-alphanumeric → `_`
/// (the exact spelling the execution policy's per-account env suffix uses).
fn env_suffix(account_id: &str) -> String {
    account_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Refuse two DISTINCT ids that normalize to the same `ACCOUNT_<SFX>_*` key
/// space (issue 753). The env grammar folds case and every non-alphanumeric
/// byte to `_`, so `paper-a`, `paper_a` (and `a`/`A`) would silently share
/// venue/strategies/trading/credential keys — and `account.remove`'s sweep
/// would wipe the OTHER account's row. Fail-closed, naming BOTH ids: the
/// operator must rename one rather than discover the alias at trade time.
/// Equal raw ids are skipped here (the caller's duplicate check owns that
/// message).
fn refuse_env_suffix_collisions(ids: &[String]) -> Result<(), CoreError> {
    let mut seen: std::collections::HashMap<String, &str> =
        std::collections::HashMap::with_capacity(ids.len());
    for id in ids {
        let sfx = env_suffix(id);
        match seen.get(&sfx) {
            Some(other) if *other != id.as_str() => {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account ids `{other}` and `{id}` both normalize to \
                         `ACCOUNT_{sfx}_*` — they would share venue/strategies/\
                         trading/credential keys (and deleting one would sweep \
                         the other's config); ids must stay distinct after \
                         uppercasing and folding every non-alphanumeric to `_` \
                         (fail-closed: rename one of them)"
                    ),
                ));
            }
            Some(_) => {}
            None => {
                seen.insert(sfx, id.as_str());
            }
        }
    }
    Ok(())
}

/// A `true`/`false` switch parsed STRICTLY (issue 754): `None` = the key is
/// genuinely absent (the documented default applies); a present value the
/// grammar cannot read refuses the boot instead of falling back to `true`
/// (the pre-fix `and_then` turned a typo into "trading enabled").
fn parse_env_bool(key: &str, raw: Option<String>) -> Result<Option<bool>, CoreError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(Some(true)),
        "false" | "0" | "off" | "no" => Ok(Some(false)),
        _ => Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "{key}=`{raw}` is not a boolean (true/false, 1/0, on/off, yes/no) \
                 — fix .env and restart (fail-closed: a switch the kernel cannot \
                 read must never fall back to `true` and enable trading)"
            ),
        )),
    }
}

/// Parse a comma-separated ROUNDS value STRICTLY (issue 754): `None` = the key
/// is absent (no declared family set); a present entry that is not an `i64`
/// (a typo or an overflow) refuses the boot instead of collapsing the WHOLE
/// list to "absent" — the pre-fix `collect::<Option<Vec<_>>>` turned one bad
/// entry into an empty set, which the risk gate reads as "no round limit".
fn parse_env_rounds(key: &str, raw: Option<String>) -> Result<Option<Vec<i64>>, CoreError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let sec = entry.parse::<i64>().map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "{key}=`{raw}` carries `{entry}`, not an i64 round duration \
                     ({e}) — fix .env and restart (fail-closed: one bad entry \
                     must not empty the declared restriction set)"
                ),
            )
        })?;
        out.push(sec);
    }
    Ok(Some(out))
}

/// Parse one comma-separated env value into a Vec (empty → empty vec).
fn env_list(raw: Option<String>) -> Vec<String> {
    raw.map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// One account's parsed env row. `None` fields = key absent (defaults apply).
struct EnvAccount {
    name: Option<String>,
    venue: Option<String>,
    status: Result<AccountStatus, CoreError>,
    trading_enabled: Option<bool>,
    strategies: Vec<String>,
    rounds: Option<Vec<i64>>,
    api_key_env: Option<String>,
    secret_env: Option<String>,
    passphrase_env: Option<String>,
    private_key_env: Option<String>,
}

fn parse_env_account(
    id: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<EnvAccount, CoreError> {
    let sfx = env_suffix(id);
    let key = |tail: &str| format!("ACCOUNT_{sfx}_{tail}");
    Ok(EnvAccount {
        name: env(&key("NAME")),
        venue: env(&key("VENUE")),
        status: env_status(env(&key("STATUS"))),
        trading_enabled: parse_env_bool(&key("TRADING_ENABLED"), env(&key("TRADING_ENABLED")))?,
        strategies: env_list(env(&key("STRATEGIES"))),
        rounds: parse_env_rounds(&key("ROUNDS"), env(&key("ROUNDS")))?,
        api_key_env: env(&key("API_KEY_ENV")),
        secret_env: env(&key("SECRET_ENV")),
        passphrase_env: env(&key("PASSPHRASE_ENV")),
        private_key_env: env(&key("PRIVATE_KEY_ENV")),
    })
}
