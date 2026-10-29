//! `user_layer/configs/accounts.toml` — the account book's config source
//! (DEV_V0_3 §9.5), parsed under the same precedence family as every other
//! config file (CLI > env > toml > default). This module is the toml + default
//! half; reading credential VALUES from the kernel's environment is
//! [`read_credential_keys`] below.
//!
//! The file is OPTIONAL: a missing file → the single `default` account (§9.5:
//! a 0.2 deployment without the file keeps exactly today's behaviour — one
//! active account, no keys named, the plugin's own env handling untouched).
//! A file that EXISTS but does not parse, declares no account, or declares a
//! duplicate id is refused loudly (fail-closed): a config typo must never
//! silently shrink the book to "one default account", because the operator
//! believes the restrictions in it are live.

use super::{
    Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys, default_account_id,
};
use crate::model::{CoreError, CoreErrorCode};
use blitzkrieg_market_api::MarketType;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The optional config file (§9.5). Absent → single `default` account.
pub const ACCOUNTS_CONFIG_PATH: &str = "user_layer/configs/accounts.toml";

/// One `[[account]]` table as it sits on disk. Credential fields hold the
/// NAMES of environment variables — §9.4: the value never appears in any
/// config file, log, or IPC answer.
///
/// `Serialize` exists for the venue write-back only ([`persist_account_venue`]
/// re-renders the parsed file); the wire types are separate (§12.1), so this
/// never leaves the config layer.
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
    let Some(rounds) = rounds else {
        return Ok(Vec::new());
    };
    let mut out: Vec<i64> = Vec::with_capacity(rounds.len());
    for sec in rounds {
        if !allowed.contains(&sec) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "accounts.toml: `rounds` names {sec}s, which is not declared by any \
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
/// This is the TOML-ONLY reader: the migration-window fallback and the tests'
/// entry. Production boots through [`load_account_ledgers_single_source`],
/// which consults the ACCOUNT_* env grammar first.
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

/// The production boot entry (issue 751, the .env single-source ruling):
/// ACCOUNT_* (process environment first, then the `.env` file the launcher
/// owns) beats the toml file; the toml file beats the single default. Env and
/// file NEVER merge — when ACCOUNT_LIST is in force and the legacy toml file
/// still exists, env wins and the residue is shouted at (one source, the
/// migration script removes the file).
///
/// `toml_path` is the legacy `accounts.toml` (compat fallback only);
/// `env_path` is the `.env` file (the same one `env_file_path` names —
/// `None` skips the file face and reads the process environment alone).
pub fn load_account_ledgers_single_source(
    toml_path: &Path,
    env_path: Option<&Path>,
    now_ms: i64,
    allowed: &[i64],
) -> Result<AccountLedgers, CoreError> {
    let env = env_lookup_from_sources(env_path);
    let from_env = load_account_ledgers_from_env(now_ms, allowed, &env)?;
    let toml_present = toml_path.exists();
    match from_env {
        Some(book) => {
            if toml_present {
                tracing::warn!(
                    "accounts: ACCOUNT_* keys are in force; the legacy {} still \
                     exists and is IGNORED — delete it \
                     (scripts/migrate-config-to-env.sh)",
                    toml_path.display()
                );
            }
            Ok(book)
        }
        None => load_account_ledgers(toml_path, now_ms, allowed),
    }
}

/// Parse the toml text into the book. Split from [`load_account_ledgers`] so
/// tests (and a future CLI override) can feed strings directly.
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
    Ok(AccountLedgers::new_with_display(
        accounts,
        active,
        AccountId::from(display.as_str()),
    ))
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

/// Read the `[cross_venue_arb]` section back from the file on disk (the
/// `arb.config.get` shape: `None` = unconfigured). A file that does not parse
/// is REFUSED — the get answers the truth, it does not paper over a broken
/// book (the boot would refuse the same file anyway).
pub fn read_cross_venue_arb(path: &Path) -> Result<Option<CrossVenueArbConfig>, CoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable: {e}"),
        )
    })?;
    let file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse: {e}"),
        )
    })?;
    Ok(file.cross_venue_arb.as_ref().map(lift_cross_venue_arb))
}

/// The persisted write of the `[cross_venue_arb]` section: read-modify-write
/// over the whole file, the exact fail-closed posture every other accounts.toml
/// writer runs (a non-parsing file is REFUSED, never flattened; same-directory
/// temp + rename; the re-render does not carry comments — every FIELD
/// round-trips because the parse is `deny_unknown_fields`). The CALLER owns
/// the semantic validations (venues distinct, accounts exist — it has the
/// book); this layer only owns the file.
pub fn persist_cross_venue_arb(path: &Path, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the arb config write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the arb config write: {e}"),
        )
    })?;
    file.cross_venue_arb = Some(CrossVenueArbFileSection {
        leg_a_venue: cfg.leg_a_venue.trim().to_string(),
        leg_a_account: cfg.leg_a_account.trim().to_string(),
        leg_b_venue: cfg.leg_b_venue.trim().to_string(),
        leg_b_account: cfg.leg_b_account.trim().to_string(),
        pairs: cfg.pairs.iter().map(|p| p.trim().to_uppercase()).collect(),
    });
    let out = toml::to_string(&file).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not render: {e}"),
        )
    })?;
    write_accounts_file_atomic(path, &out, "arbcfg")
}

/// The persisted removal of the `[cross_venue_arb]` section (`arb.config.clear`):
/// the section key is REMOVED — the file returns to the pre-v2 shape a file
/// without the key has. Same fail-closed posture as every writer above.
pub fn clear_cross_venue_arb(path: &Path) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the arb config clear: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the arb config clear: {e}"),
        )
    })?;
    file.cross_venue_arb = None;
    let out = toml::to_string(&file).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not render: {e}"),
        )
    })?;
    write_accounts_file_atomic(path, &out, "arbcfg")
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

/// The persisted write-back of ONE account's venue binding
/// (`account.venue.set`). Read-modify-write over the whole file: parse the
/// existing toml, update the named `[[account]]`'s `venue` key (an empty
/// `venue` REMOVES the key — the file spells "unbound" by absence, the same
/// shape a pre-binding file had), render, atomically replace (same-directory
/// temp + rename, so a half-written file can never be what the next boot
/// reads).
///
/// Rendering disclosure: the toml crate re-serialises the whole document, so
/// comments and layout above/inside the file do NOT survive a venue write —
/// the re-rendered file carries every FIELD (the parse is `deny_unknown_
/// fields`, so nothing unparseable could have been in there anyway) but not
/// the operator's comments. Accepted trade-off: hand-maintaining a
/// comment-preserving TOML editor is not worth it for a value the panel now
/// owns; every semantic field round-trips, which is the invariant the tests
/// pin. A file that does not parse is REFUSED, never overwritten (fail-
/// closed: a write must not be able to "fix" a typo by flattening the
/// operator's book into a shape they never wrote).
///
/// Forward-looking: `Account.venue` is the next batch's plugin-ENABLEMENT
/// derivation input (boot reads this book and enables every venue at least
/// one account is bound to), so this persist is the durability half of the
/// whole multi-plugin model — losing a sibling account's fields here would
/// silently shrink both the book and the derived enablement set.
pub fn persist_account_venue(path: &Path, account_id: &str, venue: &str) -> Result<(), CoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable, refusing the venue write: {e}"),
        )
    })?;
    let mut file: AccountsFile = toml::from_str(&text).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not parse; refusing the venue write: {e}"),
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
    if venue.is_empty() {
        target.venue = None;
    } else {
        target.venue = Some(venue.to_string());
    }
    let out = toml::to_string(&file).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml does not render: {e}"),
        )
    })?;
    // Atomic replace: write the sibling temp first, then rename over the
    // original — the boot's read and a concurrent reload can never see a
    // partial document.
    let tmp = path.with_extension("toml.venue.tmp");
    std::fs::write(&tmp, out).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml venue write failed: {e}"),
        )
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml venue rename failed: {e}"),
        )
    })
}

/// The persisted write of a NEW account (`account.create`): append one
/// `[[account]]` row to the book — read-modify-write over the whole file, the
/// exact posture [`persist_account_venue`] runs (a non-parsing file is
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
/// fail-closed posture [`persist_account_venue`] runs (a non-parsing file is
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
/// file, the exact posture [`persist_account_venue`] runs (a non-parsing
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
// Precedence: ACCOUNT_* env > accounts.toml (compat fallback during the
// migration window) > single default. A file that parses coexists with env
// keys ONLY when ACCOUNT_LIST is absent (env keys are then ignored — one
// source wins, no merge); the migration script removes the file.

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

fn parse_env_account(id: &str, env: &dyn Fn(&str) -> Option<String>) -> EnvAccount {
    let sfx = env_suffix(id);
    let key = |tail: &str| format!("ACCOUNT_{sfx}_{tail}");
    let bool_of = |raw: Option<String>| -> Option<bool> {
        raw.and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        })
    };
    EnvAccount {
        name: env(&key("NAME")),
        venue: env(&key("VENUE")),
        status: env_status(env(&key("STATUS"))),
        trading_enabled: bool_of(env(&key("TRADING_ENABLED"))),
        strategies: env_list(env(&key("STRATEGIES"))),
        rounds: env_list(env(&key("ROUNDS")))
            .iter()
            .map(|s| s.parse::<i64>().ok())
            .collect::<Option<Vec<_>>>(),
        api_key_env: env(&key("API_KEY_ENV")),
        secret_env: env(&key("SECRET_ENV")),
        passphrase_env: env(&key("PASSPHRASE_ENV")),
        private_key_env: env(&key("PRIVATE_KEY_ENV")),
    }
}

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
        let row = parse_env_account(id, env);
        let status = row.status.clone()?;
        let rounds = row.rounds.clone();
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
            rounds: validate_rounds(rounds, allowed)?,
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
    Ok(Some(AccountLedgers::new_with_display(
        accounts,
        active,
        AccountId::from(display.as_str()),
    )))
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
// runs). Read-modify-write over the CURRENT env state (process env first,
// file second — the loader's own order), so a write can never shrink the
// book: sibling accounts' keys survive untouched.

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
fn current_account_list(env: &dyn Fn(&str) -> Option<String>) -> Result<Vec<String>, CoreError> {
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
/// write its NAME/VENUE keys. No credential names are written (§9.4: the
/// create call carries no credential surface; the panel edits .env or a
/// later verb names keys).
pub fn env_persist_account_create(
    toml_path: &Path,
    env_path: &Path,
    account_id: &str,
    name: Option<&str>,
    venue: Option<&str>,
) -> Result<(), CoreError> {
    refuse_while_legacy_toml(toml_path, "create")?;
    let lookup = env_lookup_from_sources(Some(env_path));
    let list = current_account_list(&lookup)?;
    if list.iter().any(|a| a == account_id) {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("ACCOUNT_LIST already names `{account_id}`"),
        ));
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
    let lookup = env_lookup_from_sources(Some(env_path));
    let list = current_account_list(&lookup)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use AccountStatus::{Active, Frozen, ReadOnly};

    fn write_temp(text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bk-accounts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, text).expect("write temp config");
        p
    }

    fn fresh_env_file(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bk-accounts-env-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(".env")
    }

    fn static_lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    // ── the env grammar (issue 751) ─────────────────────────────────────────

    #[test]
    fn env_book_loads_with_defaults_and_probes_credentials() {
        let env = static_lookup(&[
            ("ACCOUNT_LIST", "default,paper"),
            ("ACCOUNT_DEFAULT_NAME", "Poly Main"),
            ("ACCOUNT_DEFAULT_VENUE", "polymarket"),
            ("ACCOUNT_PAPER_STATUS", "read_only"),
        ]);
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &env)
            .expect("a well-formed ACCOUNT_* set loads")
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);
        assert_eq!(book.active_id().as_str(), "default");
        let d = book.account(&AccountId::from("default")).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(d.venue, "polymarket");
        assert!(d.trading_enabled, "absent key defaults to true");
        let p = book.account(&AccountId::from("paper")).unwrap();
        assert!(matches!(p.status, AccountStatus::ReadOnly));
        // Display defaults to the first active account.
        assert_eq!(book.display_id(), "default");
    }

    #[test]
    fn env_book_refuses_a_bad_status_and_a_dangling_display() {
        let bad_status = static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_A_STATUS", "magical")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &bad_status)
            .expect_err("an unknown status refuses (fail-closed)");
        assert!(err.message.contains("STATUS=`magical`"), "{}", err.message);

        let dangling = static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_DISPLAY", "ghost")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dangling)
            .expect_err("a display id naming no listed account refuses");
        assert!(err.message.contains("ACCOUNT_DISPLAY"), "{}", err.message);
    }

    #[test]
    fn env_book_refuses_duplicate_ids_and_bad_rounds() {
        let dup = static_lookup(&[("ACCOUNT_LIST", "a,a")]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dup).is_err());

        let bad_rounds =
            static_lookup(&[("ACCOUNT_LIST", "a"), ("ACCOUNT_A_ROUNDS", "900,424242")]);
        let err = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &bad_rounds)
            .expect_err("a round duration no plugin declared refuses");
        assert!(err.message.contains("424242"), "{}", err.message);
    }

    #[test]
    fn env_book_validates_the_arb_legs_against_the_list() {
        let ok = static_lookup(&[
            ("ACCOUNT_LIST", "a,b"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_LEG_A_ACCOUNT", "a"),
            ("ACCOUNT_ARB_LEG_B_VENUE", "kalshi"),
            ("ACCOUNT_ARB_LEG_B_ACCOUNT", "b"),
            ("ACCOUNT_ARB_PAIRS", "btc, eth"),
        ]);
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &ok)
            .expect("coherent legs load")
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);
        // The legs validate against the list AND read back through the
        // dedicated reader (the boot's arb_leg_config source).
        let cfg = read_cross_venue_arb_from_env(&ok)
            .expect("the coherent section reads")
            .expect("legs configured");
        assert_eq!(cfg.leg_a_venue, "polymarket");
        assert_eq!(cfg.pairs, vec!["BTC", "ETH"]);

        let dangling_leg = static_lookup(&[
            ("ACCOUNT_LIST", "a"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_LEG_A_ACCOUNT", "ghost"),
            ("ACCOUNT_ARB_LEG_B_VENUE", "kalshi"),
            ("ACCOUNT_ARB_LEG_B_ACCOUNT", "a"),
            ("ACCOUNT_ARB_PAIRS", "btc"),
        ]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &dangling_leg).is_err());

        let half_leg = static_lookup(&[
            ("ACCOUNT_LIST", "a"),
            ("ACCOUNT_ARB_LEG_A_VENUE", "polymarket"),
            ("ACCOUNT_ARB_PAIRS", "btc"),
        ]);
        assert!(load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &half_leg).is_err());
    }

    #[test]
    fn single_source_env_wins_over_the_legacy_toml() {
        let dir = std::env::temp_dir().join(format!("bk-single-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("accounts.toml");
        std::fs::write(
            &toml,
            r#"
[[account]]
id = "from-file"
"#,
        )
        .unwrap();
        let dot_env = dir.join(".env");
        std::fs::write(&dot_env, "ACCOUNT_LIST=from-env\n").unwrap();
        // process env is empty for these keys (the test harness carries none).
        let book =
            load_account_ledgers_single_source(&toml, Some(&dot_env), 5, TEST_ALLOWED_ROUNDS)
                .expect("env wins");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.account_list()[0].id.as_str(), "from-env");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_source_falls_back_to_the_toml_then_the_default() {
        // No ACCOUNT_LIST anywhere: the legacy file still rules.
        let toml = write_temp(
            r#"
[[account]]
id = "legacy"
"#,
        );
        let missing_env = Path::new("/nonexistent/bk-none/.env");
        let book =
            load_account_ledgers_single_source(&toml, Some(missing_env), 5, TEST_ALLOWED_ROUNDS)
                .expect("toml fallback");
        assert_eq!(book.account_list()[0].id.as_str(), "legacy");

        // No file, no env: the single default.
        let book = load_account_ledgers_single_source(
            Path::new("/nonexistent/bk-none/accounts.toml"),
            Some(missing_env),
            5,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("zero-config default");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.active_id().as_str(), "default");
    }

    #[test]
    fn env_write_faces_upsert_and_refuse_the_legacy_toml() {
        // A PRIVATE toml path (the write faces only probe its existence): the
        // shared write_temp dir races with sibling tests re-creating their
        // files mid-run, which would resurrect the legacy refusal mid-test.
        let toml = fresh_env_file("write-faces").with_file_name("legacy-accounts.toml");
        std::fs::write(&toml, "[[account]]\nid = \"default\"\n").unwrap();
        let dot_env = fresh_env_file("write-faces");
        // The refusal: a legacy file alive means every runtime write refuses.
        let err = env_persist_display_account(&toml, &dot_env, "default")
            .expect_err("the legacy toml alive refuses the write (one source)");
        assert!(
            err.message.contains("migrate-config-to-env.sh"),
            "{}",
            err.message
        );
        assert!(
            !dot_env.exists(),
            "the refusal must not have created the env"
        );

        std::fs::remove_file(&toml).unwrap();
        // Display upsert.
        env_persist_display_account(&toml, &dot_env, "default").expect("display write");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_DISPLAY=\"default\""), "{text}");

        // Venue write, then venue REMOVE (empty = unbound, absence spells it).
        env_persist_account_venue(&toml, &dot_env, "default", "polymarket").unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_VENUE=\"polymarket\""),
            "{text}"
        );
        env_persist_account_venue(&toml, &dot_env, "default", "").unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("ACCOUNT_DEFAULT_VENUE"), "{text}");

        // Trading false is explicit; true removes the key.
        env_persist_account_trading(&toml, &dot_env, "default", false).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_TRADING_ENABLED=\"false\""),
            "{text}"
        );
        env_persist_account_trading(&toml, &dot_env, "default", true).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("TRADING_ENABLED"), "{text}");

        // Rounds/strategies: value write and remove-by-absence.
        env_persist_account_rounds(&toml, &dot_env, "default", &[900, 3600]).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_DEFAULT_ROUNDS=\"900,3600\""),
            "{text}"
        );
        env_persist_account_rounds(&toml, &dot_env, "default", &[]).unwrap();
        assert!(
            !std::fs::read_to_string(&dot_env)
                .unwrap()
                .contains("ROUNDS")
        );

        // Arb config upsert + clear sweep all five keys.
        let cfg = CrossVenueArbConfig {
            leg_a_venue: "polymarket".into(),
            leg_a_account: "default".into(),
            leg_b_venue: "kalshi".into(),
            leg_b_account: "k".into(),
            pairs: vec!["btc".into()],
        };
        env_persist_cross_venue_arb(&toml, &dot_env, &cfg).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(
            text.contains("ACCOUNT_ARB_LEG_A_VENUE=\"polymarket\""),
            "{text}"
        );
        assert!(text.contains("ACCOUNT_ARB_PAIRS=\"BTC\""), "{text}");
        env_clear_cross_venue_arb(&toml, &dot_env).unwrap();
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(!text.contains("ACCOUNT_ARB"), "{text}");
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_create_appends_to_the_list_and_remove_sweeps_the_row() {
        let toml = Path::new("/nonexistent/bk-none/accounts.toml");
        let dot_env = fresh_env_file("create-remove");
        env_persist_account_create(toml, &dot_env, "default", Some("Main"), Some("polymarket"))
            .expect("first create writes the list");
        env_persist_account_create(toml, &dot_env, "paper", None, None)
            .expect("second create appends");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_LIST=\"default,paper\""), "{text}");
        assert!(text.contains("ACCOUNT_DEFAULT_NAME=\"Main\""), "{text}");
        // Duplicate create refuses.
        assert!(env_persist_account_create(toml, &dot_env, "paper", None, None).is_err());

        // The book the two writes produced loads and reads back.
        let lookup = env_lookup_from_sources(Some(&dot_env));
        let book = load_account_ledgers_from_env(5, TEST_ALLOWED_ROUNDS, &lookup)
            .unwrap()
            .expect("the env grammar produced a book");
        assert_eq!(book.account_list().len(), 2);

        // Remove sweeps every per-id key and re-derives the display pointer.
        env_persist_display_account(toml, &dot_env, "paper").unwrap();
        env_persist_account_remove(toml, &dot_env, "paper").expect("remove");
        let text = std::fs::read_to_string(&dot_env).unwrap();
        assert!(text.contains("ACCOUNT_LIST=\"default\""), "{text}");
        assert!(text.contains("ACCOUNT_DISPLAY=\"default\""), "{text}");
        assert!(!text.contains("ACCOUNT_PAPER_"), "{text}");
        // Removing the last account refuses (an empty book fails the boot).
        assert!(env_persist_account_remove(toml, &dot_env, "default").is_err());
        std::fs::remove_dir_all(dot_env.parent().unwrap()).unwrap();
    }

    #[test]
    fn env_suffix_matches_the_policy_spelling() {
        assert_eq!(env_suffix("default"), "DEFAULT");
        assert_eq!(env_suffix("polysimulator"), "POLYSIMULATOR");
        assert_eq!(env_suffix("my-account.1"), "MY_ACCOUNT_1");
    }

    #[test]
    fn missing_file_yields_the_single_default_account() {
        let book = load_account_ledgers(
            Path::new("/nonexistent/bk-accounts/none.toml"),
            5,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a missing file is the zero-config deployment");
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.active_id().as_str(), "default");
    }

    #[test]
    fn the_design_example_parses_with_its_restrictions() {
        // The §9.5 example, verbatim shape: two accounts, one read-only.
        let book = from_str(
            r#"
[[account]]
id = "default"
name = "Poly Main"
market_type = "prediction"
api_key_env = "POLY_API_KEY"
secret_env = "POLY_SECRET"
passphrase_env = "POLY_PASSPHRASE"

[[account]]
id = "paper"
name = "Paper Trading"
market_type = "prediction"
status = "read_only"
"#,
            7,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("the design example must parse");
        assert_eq!(book.account_list().len(), 2);
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            ReadOnly
        ));
        assert!(matches!(
            book.account(&default_account_id()).unwrap().status,
            Active
        ));
        // Only key NAMES traveled; nothing loaded (this test process has no
        // POLY_API_KEY in its environment).
        let keys = &book.account(&default_account_id()).unwrap().credential_keys;
        assert_eq!(keys.api_key_env.as_deref(), Some("POLY_API_KEY"));
        assert!(!keys.loaded);
    }

    #[test]
    fn a_missing_status_defaults_to_active_and_a_missing_name_to_the_id() {
        let book = from_str("[[account]]\nid = \"solo\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("a minimal entry parses");
        let a = book.account(&AccountId("solo".into())).unwrap();
        assert!(matches!(a.status, Active));
        assert_eq!(a.name, "solo");
        assert!(matches!(a.market_type, MarketType::Prediction));
    }

    // ── 轮次多族 (B1d): the `rounds` family declaration ─────────────────────

    #[test]
    fn rounds_parse_deduplicate_and_keep_first_seen_order() {
        let book = from_str(
            r#"
[[account]]
id = "multi"
rounds = [3600, 900, 3600, 86_400]
"#,
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a rounds declaration parses");
        let a = book.account(&AccountId("multi".into())).unwrap();
        assert_eq!(a.rounds, vec![3600, 900, 86_400], "dedup, order kept");
    }

    #[test]
    fn a_missing_or_empty_rounds_key_means_no_family_restriction() {
        // Missing key: the legacy shape. Explicit empty: the same verdict —
        // rounds is a permission axis; an empty declaration must never lock
        // the account out of trading.
        let book = from_str(
            "[[account]]\nid = \"a\"\nrounds = []\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("an empty rounds parses");
        assert!(
            book.account(&AccountId("a".into()))
                .unwrap()
                .rounds
                .is_empty()
        );
        let book2 = from_str("[[account]]\nid = \"b\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("absent rounds parses");
        assert!(
            book2
                .account(&AccountId("b".into()))
                .unwrap()
                .rounds
                .is_empty()
        );
    }

    #[test]
    fn a_rounds_value_outside_the_whitelist_refuses_the_boot() {
        let err = from_str(
            "[[account]]\nid = \"x\"\nrounds = [9000]\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("a typo'd duration is fail-closed");
        assert!(err.message.contains("9000"), "{}", err.message);
        assert!(
            err.message.contains("not declared by any market plugin"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_malformed_file_is_refused_not_silently_defaulted() {
        let p = write_temp("[[account]]\nid = \"x\"\nbogus_field = 1\n");
        let err =
            load_account_ledgers(&p, 1, TEST_ALLOWED_ROUNDS).expect_err("deny_unknown_fields");
        assert!(err.message.contains("does not parse"), "{}", err.message);
    }

    #[test]
    fn an_empty_book_and_duplicate_ids_are_refused() {
        // A truly EMPTY document fails at the TOML layer (missing field) —
        // refused either way; the code's own empty-book diagnostic speaks for
        // a document that PARSES but declares no account (`account = []`).
        let empty_file = from_str("", 1, TEST_ALLOWED_ROUNDS).expect_err("no accounts at all");
        assert!(
            empty_file.message.contains("does not parse")
                || empty_file.message.contains("no [[account]]"),
            "{}",
            empty_file.message
        );
        let err = from_str("account = []\n", 1, TEST_ALLOWED_ROUNDS)
            .expect_err("parses but no [[account]]");
        assert!(err.message.contains("no [[account]]"), "{}", err.message);
        let dup = from_str(
            "[[account]]\nid = \"a\"\n\n[[account]]\nid = \"a\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("duplicate id");
        assert!(dup.message.contains("twice"), "{}", dup.message);
        let blank =
            from_str("[[account]]\nid = \"  \"\n", 1, TEST_ALLOWED_ROUNDS).expect_err("blank id");
        assert!(blank.message.contains("empty id"), "{}", blank.message);
    }

    #[test]
    fn frozen_and_suspended_status_spellings_parse() {
        let book = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"frozen\"\n\n[[account]]\nid = \"b\"\nstatus = \"suspended\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("snake_case statuses parse");
        assert!(matches!(
            book.account(&AccountId("a".into())).unwrap().status,
            Frozen
        ));
        // A bare `suspended` string carries no operator reason: the parsed
        // record states that honestly instead of inventing one.
        let b = book.account(&AccountId("b".into())).unwrap();
        assert!(matches!(b.status, AccountStatus::Suspended { .. }));
        // The map form carries the reason through untouched.
        let with_reason = from_str(
            "[[account]]\nid = \"c\"\nstatus = { suspended = { reason = \"ops review\" } }\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("map-form suspended parses");
        assert!(matches!(
            &with_reason.account(&AccountId("c".into())).unwrap().status,
            AccountStatus::Suspended { reason } if reason == "ops review"
        ));
        // A typo'd spelling is refused, never parsed as something permissive.
        let typo = from_str(
            "[[account]]\nid = \"d\"\nstatus = \"actve\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("unknown spelling");
        assert!(
            typo.message.contains("unknown account status"),
            "{}",
            typo.message
        );
    }

    #[test]
    fn the_venue_binding_parses_and_defaults_to_unbound() {
        // An explicit binding rides through as data…
        let bound = from_str(
            "[[account]]\nid = \"poly\"\nvenue = \"polymarket\"\n\n[[account]]\nid = \"kal\"\nvenue = \"kalshi\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("venue bindings parse");
        assert_eq!(
            bound.account(&AccountId("poly".into())).unwrap().venue,
            "polymarket"
        );
        assert_eq!(
            bound.account(&AccountId("kal".into())).unwrap().venue,
            "kalshi"
        );
        // …a missing field is UNBOUND (the 0.2 behaviour)…
        let unbound = from_str("[[account]]\nid = \"solo\"\n", 1, TEST_ALLOWED_ROUNDS)
            .expect("minimal entry");
        assert_eq!(
            unbound.account(&AccountId("solo".into())).unwrap().venue,
            ""
        );
        // …and the zero-config default account is unbound too.
        let default_book = AccountLedgers::single_default(1);
        assert_eq!(
            default_book.account(&default_account_id()).unwrap().venue,
            ""
        );
    }

    #[test]
    fn credential_probe_reports_loaded_only_when_every_named_key_resolves() {
        // No keys named → nothing was loaded by the kernel (the plugin's own
        // env path is outside this book) — `loaded` stays false, honestly.
        let none = read_credential_keys(&CredentialKeys::default());
        assert!(!none.loaded);
        // A name that cannot resolve in this process → not loaded. Read-only
        // env access with a name nothing sets: deterministic, no env mutation.
        let missing = read_credential_keys(&CredentialKeys {
            api_key_env: Some("BK_E28_DEFINITELY_UNSET_KEY".into()),
            ..CredentialKeys::default()
        });
        assert!(!missing.loaded);
        // The name itself is carried through untouched — names are the wire
        // content, never values.
        assert_eq!(
            missing.api_key_env.as_deref(),
            Some("BK_E28_DEFINITELY_UNSET_KEY")
        );
    }

    // ── account.venue.set persistence (write-back of the venue binding) ──────

    /// A multi-field, multi-account fixture — the file the persist must NOT
    /// shrink: every field of BOTH entries must survive a one-account venue
    /// write.
    const MULTI_FIELD_BOOK: &str = r#"
[[account]]
id = "default"
name = "Poly Main"
market_type = "prediction"
api_key_env = "POLY_API_KEY"
secret_env = "POLY_SECRET"
passphrase_env = "POLY_PASSPHRASE"
private_key_env = "POLY_PRIVATE"

[[account]]
id = "paper"
name = "Paper Trading"
market_type = "prediction"
status = "read_only"
venue = "kalshi"
"#;

    #[test]
    fn persist_account_venue_writes_back_and_every_sibling_field_survives() {
        // Mutation note: this test is the "the persist call happens" guard.
        // Delete the persist_account_venue call from the venue.set arm (or
        // make it a no-op) and the reloaded book below still parses — but the
        // `venue` assertion on the RELOADED file goes red: the in-memory
        // write alone never reaches this file.
        let dir = std::env::temp_dir().join(format!("bk-venue-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Bind `default` to polymarket: the write lands, and a FRESH load —
        // the same call the boot makes — sees the binding plus EVERY sibling
        // field of BOTH accounts.
        persist_account_venue(&p, "default", "polymarket").expect("the write lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the new binding is durable"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi",
            "a sibling account's binding is untouched"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert!(matches!(d.market_type, MarketType::Prediction));
        assert!(matches!(d.status, Active));
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY")
        );
        assert_eq!(d.credential_keys.secret_env.as_deref(), Some("POLY_SECRET"));
        assert_eq!(
            d.credential_keys.passphrase_env.as_deref(),
            Some("POLY_PASSPHRASE")
        );
        assert_eq!(
            d.credential_keys.private_key_env.as_deref(),
            Some("POLY_PRIVATE")
        );
        // The other account's status — the file's only restriction — rode
        // through: a persist must never loosen what the operator wrote.
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            ReadOnly
        ));

        // Re-binding the SECOND account over its existing value: same
        // guarantees on a file the writer itself produced (round two of the
        // read-modify-write cycle).
        persist_account_venue(&p, "paper", "polymarket").expect("rebind lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "polymarket"
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the first write survived the second"
        );

        // Unbind: venue = "" REMOVES the key — the file returns to the
        // "unbound by absence" shape a pre-binding deployment had.
        persist_account_venue(&p, "paper", "").expect("unbind lands");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("paper") || !text.split("[[account]]").nth(2).unwrap().contains("venue"),
            "the unbound entry carries no venue key: {text}"
        );
        let book =
            load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS).expect("the unbound file re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "",
            "absence loads as unbound"
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().venue,
            "polymarket",
            "the other account's binding survives the unbind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_venue_refuses_unknown_ids_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("bk-venue-persist2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // An id the file never declared: refused, and the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_venue(&p, "ghost", "polymarket").expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_venue(&p, "x", "polymarket").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err = persist_account_venue(&dir.join("nope.toml"), "x", "polymarket")
            .expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_rounds_writes_back_and_every_sibling_field_survives() {
        let dir = std::env::temp_dir().join(format!("bk-rounds-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // The write lands with every sibling field intact (the same
        // read-modify-write guarantee the venue writer pins — one regression
        // here would silently shrink the book at the next boot).
        persist_account_rounds(&p, "default", &[900, 3600]).expect("the write lands");
        let book = load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the file re-parses");
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().rounds,
            vec![900, 3600],
            "the declaration is what the next boot reads"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY")
        );
        assert_eq!(d.credential_keys.secret_env.as_deref(), Some("POLY_SECRET"));
        assert_eq!(
            d.credential_keys.passphrase_env.as_deref(),
            Some("POLY_PASSPHRASE")
        );
        assert_eq!(
            d.credential_keys.private_key_env.as_deref(),
            Some("POLY_PRIVATE")
        );
        // The sibling account's own state (status restriction + existing
        // binding) rode through untouched.
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi"
        );
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            AccountStatus::ReadOnly
        ));

        // Round two of the read-modify-write cycle: a rewrite over a file
        // the writer itself produced keeps both accounts' declarations.
        persist_account_rounds(&p, "paper", &[300]).expect("round two lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().rounds,
            vec![300]
        );
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().rounds,
            vec![900, 3600],
            "the first write survived the second"
        );

        // The EMPTY set REMOVES the key — the file returns to the
        // "unrestricted by absence" shape a pre-declaration file had; the
        // deleted key parses exactly like a missing one.
        persist_account_rounds(&p, "default", &[]).expect("the delete lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let default_block = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !default_block.contains("rounds"),
            "the unrestricted entry carries no rounds key: {text}"
        );
        let book = load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS)
            .expect("the deleted-key file re-parses");
        assert!(
            book.account(&AccountId("default".into()))
                .unwrap()
                .rounds
                .is_empty(),
            "absence loads as the unrestricted set"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().rounds,
            vec![300],
            "the other account's declaration survives the delete"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_account_rounds_refuses_unknown_ids_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("bk-rounds-persist2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // An id the file never declared: refused, and the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_rounds(&p, "ghost", &[900]).expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_rounds(&p, "x", &[900]).expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err = persist_account_rounds(&dir.join("nope.toml"), "x", &[900])
            .expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── display_account（显示账户配置化）：解析 / 缺省 / fail-closed ─────────

    #[test]
    fn display_account_parses_and_defaults_to_the_first_active_entry() {
        // Explicit: the key rides through as the display id verbatim. NOTE
        // the TOML spelling: a top-level key must sit BEFORE the first
        // [[account]] (TOML binds anything after an array-of-tables header
        // to that table) — the writers re-render it at the top, a hand edit
        // below the last block is refused by the entry's own
        // `deny_unknown_fields` (fail-closed, with the field list named).
        let explicit = from_str(
            "display_account = \"b\"\n\n[[account]]\nid = \"a\"\n\n[[account]]\nid = \"b\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("an explicit display_account parses");
        assert_eq!(explicit.display_id(), "b");

        // …missing: the FIRST status=active entry — `b` here, `a`'s read_only
        // posture must not win the pointer just by file order.
        let defaulted = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"read_only\"\n\n[[account]]\nid = \"b\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a file without the key parses");
        assert_eq!(
            defaulted.display_id(),
            "b",
            "absence = the first ACTIVE account, not the first row"
        );

        // No active entry at all: fall back to the first row (the book still
        // boots — the pointer is presentation-only, so the zero-active shape
        // is not a boot refusal, it is an honest "nothing to show").
        let no_active = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"read_only\"\n\n[[account]]\nid = \"b\"\nstatus = \"frozen\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect("a book with no active entry parses");
        assert_eq!(no_active.display_id(), "a");

        // Zero-config deployment (file missing): the single default account
        // displays itself.
        assert_eq!(
            AccountLedgers::single_default(1).display_id(),
            default_account_id().as_str()
        );
    }

    #[test]
    fn display_account_names_an_unknown_id_refuses_the_boot() {
        // Fail-closed: a dangling display pointer refuses the parse — a
        // silent fallback to "some account" is the exact 「账户交易所 ≠
        // 行情页交易所」 accident the key exists to make explicit.
        let err = from_str(
            "display_account = \"ghost\"\n\n[[account]]\nid = \"a\"\n",
            1,
            TEST_ALLOWED_ROUNDS,
        )
        .expect_err("unknown display id");
        assert!(
            err.message.contains("display_account") && err.message.contains("ghost"),
            "{}",
            err.message
        );
        // An EMPTY id names no configured account either — the same
        // fail-closed refusal (the file spells "follow the default" by
        // deleting the key, never by an empty string).
        assert!(
            from_str(
                "display_account = \"\"\n\n[[account]]\nid = \"a\"\n",
                1,
                TEST_ALLOWED_ROUNDS
            )
            .is_err()
        );
    }

    #[test]
    fn persist_display_account_writes_back_top_level_and_siblings_survive() {
        let dir = std::env::temp_dir().join(format!("bk-display-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // The write lands as a TOP-LEVEL key — never inside an [[account]]
        // block — and a FRESH load (the boot's call) reads it back with every
        // sibling field of BOTH accounts intact.
        persist_display_account(&p, "paper").expect("the write lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let first_row = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !first_row.contains("display_account = \"paper\""),
            "the key is top-level, not inside the first [[account]] block: {text}"
        );
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert_eq!(book.display_id(), "paper", "the new pointer is durable");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi",
            "a sibling account's binding survives the display write"
        );
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY"),
            "every sibling field round-trips"
        );

        // Re-point at the OTHER account: the read-modify-write cycle holds
        // round two, the first write survives.
        persist_display_account(&p, "default").expect("the re-point lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("round two re-parses");
        assert_eq!(book.display_id(), "default");

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_display_account(&p, "x").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );

        // A file that cannot even be read: refused with the same posture.
        let err =
            persist_display_account(&dir.join("nope.toml"), "x").expect_err("unreadable file");
        assert!(err.message.contains("unreadable"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── account.create / account.remove 的持久化半（文件层）───────────────────

    #[test]
    fn persist_account_create_appends_a_default_row_and_siblings_survive() {
        let dir = std::env::temp_dir().join(format!("bk-acct-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Create: the write lands, and a FRESH load — the boot's call — sees
        // the new account with the file's own defaults (active + prediction,
        // unbound) plus EVERY sibling field of BOTH old accounts.
        persist_account_create(&p, "poly-main", Some("Poly Main 2"), Some("polymarket"))
            .expect("the create lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        let created = book.account(&AccountId("poly-main".into())).unwrap();
        assert!(
            matches!(created.status, Active),
            "the file default is active"
        );
        assert!(
            matches!(created.market_type, MarketType::Prediction),
            "the file default is prediction"
        );
        assert_eq!(created.venue, "polymarket");
        assert_eq!(created.name, "Poly Main 2");
        // Both old accounts rode through untouched.
        assert_eq!(
            book.account(&AccountId("default".into())).unwrap().name,
            "Poly Main"
        );
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi"
        );

        // A create with neither name nor venue: the row is the bare id, the
        // name falls back to the id and the binding is unbound.
        persist_account_create(&p, "bare", None, None).expect("the bare create lands");
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("re-parses");
        let bare = book.account(&AccountId("bare".into())).unwrap();
        assert_eq!(bare.name, "bare");
        assert_eq!(bare.venue, "");

        // A duplicate id is refused (in-memory AND at the file layer) and the
        // file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_create(&p, "paper", None, None).expect_err("duplicate");
        assert!(
            err.message
                .contains("already declares `[[account]]` with id `paper`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused create leaves the file byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trading switch and strategy ENABLEMENT set round-trip through
    /// accounts.toml: an OFF switch and a non-empty set are the only shapes
    /// that occupy file keys (an absent strategies key is the empty set —
    /// "this account runs nothing explicitly"), sibling
    /// rows survive, and unknown ids refuse with the file untouched.
    #[test]
    fn persist_account_trading_and_strategies_round_trip_by_absence_and_presence() {
        let dir = std::env::temp_dir().join(format!("bk-acct-trade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // OFF writes an explicit key; a fresh load reads it as false.
        persist_account_trading(&p, "default", false).expect("the off write lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert!(
            !book
                .account(&AccountId("default".into()))
                .unwrap()
                .trading_enabled,
            "the OFF switch is durable"
        );
        // A sibling row rode through untouched (still trading, no key).
        assert!(
            book.account(&AccountId("paper".into()))
                .unwrap()
                .trading_enabled,
            "the sibling's default ON survives"
        );

        // ON removes the key — the file returns to the pre-switch shape.
        persist_account_trading(&p, "default", true).expect("the on write lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let default_row = text.split("[[account]]").nth(1).unwrap();
        assert!(
            !default_row.contains("trading_enabled"),
            "ON is spelled by absence: {text}"
        );
        let book = load_account_ledgers(&p, 4, TEST_ALLOWED_ROUNDS).expect("re-parses");
        assert!(
            book.account(&AccountId("default".into()))
                .unwrap()
                .trading_enabled,
            "absence loads as ON"
        );

        // The strategy ENABLEMENT set rides verbatim; a fresh load reads it
        // whole (the file is the durable half of the 启用按钮).
        persist_account_strategies(&p, "paper", &["spread_arb".into(), "mad_dog".into()])
            .expect("the enablement write lands");
        let book = load_account_ledgers(&p, 5, TEST_ALLOWED_ROUNDS).expect("re-parses");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().strategies,
            vec!["spread_arb".to_string(), "mad_dog".to_string()],
            "the enablement set is durable, verbatim and ordered"
        );

        // The zero-config spelling: an empty list REMOVES the key.
        persist_account_strategies(&p, "paper", &[]).expect("the inherit lands");
        let text = std::fs::read_to_string(&p).unwrap();
        let paper_row = text.split("[[account]]").nth(2).unwrap();
        assert!(
            !paper_row.contains("strategies"),
            "inherit is spelled by absence: {text}"
        );

        // Unknown ids refuse on both writers; the file is untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(persist_account_trading(&p, "ghost", false).is_err());
        assert!(persist_account_strategies(&p, "ghost", &["x".into()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused write leaves the file byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn persist_account_remove_drops_the_row_refuses_unknown_and_the_last_row() {
        let dir = std::env::temp_dir().join(format!("bk-acct-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join("accounts.toml");
        std::fs::write(&p, MULTI_FIELD_BOOK).expect("fixture");

        // Remove: the row leaves, the survivor keeps every field, and the
        // fresh load (the boot's call) agrees.
        persist_account_remove(&p, "paper").expect("the remove lands");
        let book =
            load_account_ledgers(&p, 3, TEST_ALLOWED_ROUNDS).expect("the written file re-parses");
        assert!(book.account(&AccountId("paper".into())).is_none());
        let d = book.account(&AccountId("default".into())).unwrap();
        assert_eq!(d.name, "Poly Main");
        assert_eq!(
            d.credential_keys.api_key_env.as_deref(),
            Some("POLY_API_KEY"),
            "a sibling account's fields survive the removal"
        );

        // Unknown id: refused, file untouched.
        let before = std::fs::read_to_string(&p).unwrap();
        let err = persist_account_remove(&p, "ghost").expect_err("unknown id");
        assert!(
            err.message.contains("no `[[account]]` with id `ghost`"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "a refused remove leaves the file byte-identical"
        );

        // The LAST row: refused — the empty book would fail the next boot.
        let err = persist_account_remove(&p, "default").expect_err("the last row");
        assert!(
            err.message.contains("empty") && err.message.contains("next boot"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "the empty-book refusal leaves the file byte-identical"
        );

        // A file that does not parse: refused fail-closed, never flattened.
        std::fs::write(&p, "[[account]]\nid = \"x\"\nbogus = 1\n").unwrap();
        let err = persist_account_remove(&p, "x").expect_err("does not parse");
        assert!(
            err.message.contains("does not parse; refusing"),
            "{}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
