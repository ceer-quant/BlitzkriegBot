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
use serde::Deserialize;
use std::path::Path;

/// The optional config file (§9.5). Absent → single `default` account.
pub const ACCOUNTS_CONFIG_PATH: &str = "user_layer/configs/accounts.toml";

/// One `[[account]]` table as it sits on disk. Credential fields hold the
/// NAMES of environment variables — §9.4: the value never appears in any
/// config file, log, or IPC answer.
#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
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
}
