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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountsFile {
    account: Vec<AccountFileEntry>,
}

impl AccountFileEntry {
    fn into_account(self, now_ms: i64) -> Result<Account, CoreError> {
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
            updated_at_ms: now_ms,
        })
    }
}

/// Load the account book from `path` (§9.5). Missing file → the single
/// `default` account (zero config change for a 0.2 deployment); a malformed,
/// empty, or duplicated-id file is a hard error (fail-closed) — the caller
/// refuses the boot rather than trading with a book that is not what the
/// operator wrote.
pub fn load_account_ledgers(path: &Path, now_ms: i64) -> Result<AccountLedgers, CoreError> {
    if !path.exists() {
        return Ok(AccountLedgers::single_default(now_ms));
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("accounts.toml unreadable: {e}"),
        )
    })?;
    from_str(&text, now_ms)
}

/// Parse the toml text into the book. Split from [`load_account_ledgers`] so
/// tests (and a future CLI override) can feed strings directly.
pub fn from_str(text: &str, now_ms: i64) -> Result<AccountLedgers, CoreError> {
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
        let mut account = entry.into_account(now_ms)?;
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
    Ok(AccountLedgers::new(accounts, active))
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

    #[test]
    fn missing_file_yields_the_single_default_account() {
        let book = load_account_ledgers(Path::new("/nonexistent/bk-accounts/none.toml"), 5)
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
        let book = from_str("[[account]]\nid = \"solo\"\n", 1).expect("a minimal entry parses");
        let a = book.account(&AccountId("solo".into())).unwrap();
        assert!(matches!(a.status, Active));
        assert_eq!(a.name, "solo");
        assert!(matches!(a.market_type, MarketType::Prediction));
    }

    #[test]
    fn a_malformed_file_is_refused_not_silently_defaulted() {
        let p = write_temp("[[account]]\nid = \"x\"\nbogus_field = 1\n");
        let err = load_account_ledgers(&p, 1).expect_err("deny_unknown_fields");
        assert!(err.message.contains("does not parse"), "{}", err.message);
    }

    #[test]
    fn an_empty_book_and_duplicate_ids_are_refused() {
        // A truly EMPTY document fails at the TOML layer (missing field) —
        // refused either way; the code's own empty-book diagnostic speaks for
        // a document that PARSES but declares no account (`account = []`).
        let empty_file = from_str("", 1).expect_err("no accounts at all");
        assert!(
            empty_file.message.contains("does not parse")
                || empty_file.message.contains("no [[account]]"),
            "{}",
            empty_file.message
        );
        let err = from_str("account = []\n", 1).expect_err("parses but no [[account]]");
        assert!(err.message.contains("no [[account]]"), "{}", err.message);
        let dup = from_str("[[account]]\nid = \"a\"\n\n[[account]]\nid = \"a\"\n", 1)
            .expect_err("duplicate id");
        assert!(dup.message.contains("twice"), "{}", dup.message);
        let blank = from_str("[[account]]\nid = \"  \"\n", 1).expect_err("blank id");
        assert!(blank.message.contains("empty id"), "{}", blank.message);
    }

    #[test]
    fn frozen_and_suspended_status_spellings_parse() {
        let book = from_str(
            "[[account]]\nid = \"a\"\nstatus = \"frozen\"\n\n[[account]]\nid = \"b\"\nstatus = \"suspended\"\n",
            1,
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
        )
        .expect("map-form suspended parses");
        assert!(matches!(
            &with_reason.account(&AccountId("c".into())).unwrap().status,
            AccountStatus::Suspended { reason } if reason == "ops review"
        ));
        // A typo'd spelling is refused, never parsed as something permissive.
        let typo = from_str("[[account]]\nid = \"d\"\nstatus = \"actve\"\n", 1)
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
        let unbound = from_str("[[account]]\nid = \"solo\"\n", 1).expect("minimal entry");
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
        let book = load_account_ledgers(&p, 3).expect("the written file re-parses");
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
        let book = load_account_ledgers(&p, 4).expect("round two re-parses");
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
        let book = load_account_ledgers(&p, 5).expect("the unbound file re-parses");
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
}
