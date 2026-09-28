//! Account identity and status — DEV_V0_3 §9.1 / §14.0.
//!
//! Wave 0 lands the four frozen names (issue #329 row 6): `AccountId`,
//! `DEFAULT_ACCOUNT_ID`, `AccountStatus`, `CredentialKeys`. E25's audit
//! records, E28's account book and E27's `market.list` all read them, so they
//! must exist once — later Epics must not define private mirrors.
//!
//! Split by ownership (§11.1): `AccountId` / `DEFAULT_ACCOUNT_ID` /
//! `default_account_id()` live in `blitzkrieg_market_api::account` because the
//! strategy API has to name the account a round targets and may NOT depend on
//! the core; they are re-exported here so `blitzkrieg_core::account::AccountId`
//! is the SAME type both sides see. `AccountStatus` / `CredentialKeys` are
//! core-domain and are defined here directly.
//!
//! The rest of §9 — `Account`, `AccountLedgers` (per-account `Ledger`
//! instances), config loading and the credential reader — lands with **E28**.
//! Wave 0 has no behavior: no file is read, no credential is loaded, nothing
//! here runs.

use serde::{Deserialize, Serialize};

pub use blitzkrieg_market_api::account::{AccountId, DEFAULT_ACCOUNT_ID, default_account_id};

/// Account lifecycle state (DEV_V0_3 §9.1). Wire = `snake_case`.
///
/// Enforced at the arbitration pipeline's Gate 2 (E25/E28): only `Active`
/// places new entries. Tightening (`Active -> anything`) is allowed at
/// runtime via `account.status`; loosening is NOT — an unwind that a single
/// IPC call could undo is not an unwind (§12.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// May place entries (still subject to risk gates and the kill switch).
    Active,
    /// Read-only: view and reconcile, but any place is refused at Gate 2
    /// (`AccountLimit`).
    ReadOnly,
    /// No new entries; **closing is exempt** — same posture as the kill
    /// switch. A freeze must never be the reason a position is trapped.
    Frozen,
    /// Same as `Frozen` plus an operator-facing reason string.
    Suspended { reason: String },
}

/// Hand-written so the TOML file (§9.5) can spell a status EITHER way:
/// the bare string `status = "suspended"` (a suspension with no stated
/// reason — recorded with an honest default, never silently dropped into a
/// permissive posture) or the map form `status = { suspended = { reason =
/// "ops" } }`. Derived deserialization would refuse the bare-string form of
/// the struct variant outright, which would fail-closed the whole FILE for a
/// spelling the spec shows — the wrong refusal surface. Unknown spellings
/// still refuse (fail-closed): a typo'd status must never parse as
/// something permissive.
impl<'de> Deserialize<'de> for AccountStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        // The map form first: `{"frozen": null}`, `{"suspended": {"reason":
        // "ops"}}` — serde's externally-tagged enum shape (what the WIRE
        // speaks too, matching the derived Serialize above).
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) => match s.as_str() {
                "active" => Ok(AccountStatus::Active),
                "read_only" => Ok(AccountStatus::ReadOnly),
                "frozen" => Ok(AccountStatus::Frozen),
                "suspended" => Ok(AccountStatus::Suspended {
                    reason: "suspended (reason not stated in config)".into(),
                }),
                other => Err(D::Error::custom(format!(
                    "unknown account status `{other}` — expected active, \
                     read_only, frozen or suspended"
                ))),
            },
            serde_json::Value::Object(map) if map.len() == 1 => {
                let (tag, body) = map.into_iter().next().expect("len == 1 checked");
                match tag.as_str() {
                    "active" if body.is_null() => Ok(AccountStatus::Active),
                    "read_only" if body.is_null() => Ok(AccountStatus::ReadOnly),
                    "frozen" if body.is_null() => Ok(AccountStatus::Frozen),
                    "suspended" => {
                        let reason = body
                            .get("reason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("suspended (reason not stated in config)");
                        Ok(AccountStatus::Suspended {
                            reason: reason.to_string(),
                        })
                    }
                    other => Err(D::Error::custom(format!(
                        "unknown account status `{other}` — expected active, \
                         read_only, frozen or suspended"
                    ))),
                }
            }
            _ => Err(D::Error::custom(
                "account status must be a string (\"frozen\") or a \
                 single-key map ({ suspended = { reason = \"...\" } })",
            )),
        }
    }
}

/// Names of the environment variables an account's credentials come from —
/// **never the values** (§9.4 / §14.0 row 6). Wire = `camelCase`.
///
/// This struct is safe to serialize by construction: no path that constructs
/// it ever stores a credential value. The kernel process is the only reader
/// (from its own environment); IPC answers `credentialsLoaded: bool` and
/// nothing else, and the `account:credential-check` gate greps every response
/// for a planted sentinel value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialKeys {
    pub api_key_env: Option<String>,
    pub secret_env: Option<String>,
    pub passphrase_env: Option<String>,
    pub private_key_env: Option<String>,
    /// Whether the keys above were read successfully at startup — a boolean,
    /// never a value.
    pub loaded: bool,
}

// ── E28 (§9.1 / §9.3): the account book ─────────────────────────────────────

pub mod config;

use crate::ledger::Ledger;
use crate::model::{CoreError, CoreErrorCode, CoreResult};
use blitzkrieg_market_api::MarketType;
use std::collections::HashMap;

impl AccountStatus {
    /// The wire tag §12.1 spells: `active` / `read_only` / `frozen` /
    /// `suspended`. A suspension's reason travels BESIDE the tag (the
    /// `AccountView.statusReason` field / the `account.status` result's
    /// `reason`), never inside it.
    pub fn wire_tag(&self) -> &'static str {
        match self {
            AccountStatus::Active => "active",
            AccountStatus::ReadOnly => "read_only",
            AccountStatus::Frozen => "frozen",
            AccountStatus::Suspended { .. } => "suspended",
        }
    }

    /// Gate 2 (§9.1): only `Active` places NEW entries — and even then still
    /// subject to the risk gates and the kill switch on top.
    pub fn allows_entries(&self) -> bool {
        matches!(self, AccountStatus::Active)
    }

    /// Closes are the one thing a freeze must never trap (§9.1: the kill
    /// switch posture — a freeze must not become the reason a position is
    /// stuck). `ReadOnly` is the only full stop.
    pub fn allows_closes(&self) -> bool {
        !matches!(self, AccountStatus::ReadOnly)
    }

    /// `account.status` may only TIGHTEN (§12.1): a runtime call must never
    /// GRANT a capability the account did not have. `frozen -> active` is the
    /// acceptance case (refused, INVALID_PARAMS); `read_only -> frozen` is
    /// equally a loosening — it re-grants the close exemption — and is
    /// refused. Lateral moves inside one posture (`frozen <-> suspended`,
    /// same capability set) only restate the freeze and are allowed:
    /// `Suspended` carries the operator-facing reason.
    pub fn can_tighten_to(&self, new: &AccountStatus) -> bool {
        (!new.allows_entries() || self.allows_entries())
            && (!new.allows_closes() || self.allows_closes())
    }
}

/// Operator-facing spelling for refusal messages and logs (`status is
/// read_only`, `suspended (ops)`). The WIRE spelling stays serde's
/// `snake_case`; this is only for human text.
impl std::fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountStatus::Active => write!(f, "active"),
            AccountStatus::ReadOnly => write!(f, "read_only"),
            AccountStatus::Frozen => write!(f, "frozen"),
            AccountStatus::Suspended { reason } => write!(f, "suspended ({reason})"),
        }
    }
}

/// One configured account (§9.1): identity, lifecycle status, and the NAMES of
/// the environment variables its credentials come from — never the values
/// (§9.4; `CredentialKeys` is safe to serialize by construction).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub market_type: MarketType,
    pub status: AccountStatus,
    #[serde(default)]
    pub credential_keys: CredentialKeys,
    pub updated_at_ms: i64,
}

/// The per-account money truth (§9.3): one `Ledger` instance per `AccountId`
/// plus the process-wide active account. `Ledger` itself is UNTOUCHED —
/// reserve/release/settle keep their exact signatures; the only change is that
/// "the ledger" is now a lookup instead of a singleton.
///
/// `get_mut` REFUSES an unknown account instead of implicitly creating an
/// empty book. An implicit ledger is how A's money ends up paying for B's
/// order — the exact accident this book exists to prevent (reverse acceptance
/// C pins the refusal).
#[derive(Debug)]
pub struct AccountLedgers {
    accounts: HashMap<AccountId, Account>,
    ledgers: HashMap<AccountId, Ledger>,
    active: AccountId,
}

impl AccountLedgers {
    /// Build the book from configured accounts; every account starts with a
    /// fresh `Ledger` (dry seeding happens through the normal `set_balance`
    /// path, unchanged). `active` names the process-level default account
    /// (§9.5: `default` when configured, else the file's first entry; the
    /// SESSION-level switch lives at the IPC layer and never mutates this).
    pub fn new(accounts: Vec<Account>, active: AccountId) -> Self {
        let mut by_id = HashMap::new();
        let mut ledgers = HashMap::new();
        for a in accounts {
            ledgers.insert(a.id.clone(), Ledger::new());
            by_id.insert(a.id.clone(), a);
        }
        Self {
            accounts: by_id,
            ledgers,
            active,
        }
    }

    /// The single `default` account a 0.2 deployment gets when
    /// `user_layer/configs/accounts.toml` does not exist (§9.5: file missing =
    /// zero config change). Credential key names stay empty — the plugin's own
    /// environment handling is untouched (0.2 behaviour).
    pub fn single_default(now_ms: i64) -> Self {
        Self::new(
            vec![Account {
                id: default_account_id(),
                name: "default".into(),
                market_type: MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                updated_at_ms: now_ms,
            }],
            default_account_id(),
        )
    }

    /// Seed EVERY account's ledger with `seed` (E28, locally-settling modes):
    /// a multi-account book is not one wallet with n aliases — each account
    /// owns a book that starts on its own seed, so the reserve/overspend gate
    /// is meaningful per account exactly as it was for the single one. With
    /// one configured account this is the same single `set_balance` 0.2 ran
    /// (bit-identical path for the existing deployment).
    pub fn seed_all(&mut self, seed: rust_decimal::Decimal) {
        for ledger in self.ledgers.values_mut() {
            ledger.set_balance(seed);
        }
    }

    /// The process-level default account (§9.5). Session-level switches live
    /// at the IPC layer per connection and NEVER mutate this.
    pub fn active_id(&self) -> &AccountId {
        &self.active
    }

    pub fn active_ledger(&self) -> &Ledger {
        self.ledgers
            .get(&self.active)
            .expect("active account always has a ledger (invariant of new/switch)")
    }

    pub fn active_ledger_mut(&mut self) -> &mut Ledger {
        self.ledgers
            .get_mut(&self.active)
            .expect("active account always has a ledger (invariant of new/switch)")
    }

    /// The account's ledger for READING (views, risk equity). Same refusal as
    /// [`Self::get_mut`] — an unknown account is never silently empty.
    pub fn ledger_of(&self, id: &AccountId) -> CoreResult<&Ledger> {
        self.ledgers.get(id).ok_or_else(|| unknown_account(id))
    }

    /// The account's ledger for MOVING MONEY. Explicitly refuses an unknown
    /// account (§9.3) — no implicit empty book, ever.
    pub fn get_mut(&mut self, id: &AccountId) -> CoreResult<&mut Ledger> {
        self.ledgers.get_mut(id).ok_or_else(|| unknown_account(id))
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.get(id)
    }

    /// Refuse with the standard `unknown account` error when `id` is not
    /// configured — the existence half of the Gate-2 check, for callers
    /// (`account.switch`) that need the account to EXIST without judging
    /// its posture (switching TO a frozen account to look at it is legal).
    pub fn require(&self, id: &AccountId) -> CoreResult<()> {
        if self.accounts.contains_key(id) {
            Ok(())
        } else {
            Err(unknown_account(id))
        }
    }

    /// All configured accounts, sorted by id — a stable wire order for
    /// `account.list` (HashMap iteration order must not leak onto the wire).
    pub fn account_list(&self) -> Vec<&Account> {
        let mut v: Vec<&Account> = self.accounts.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Session-independent switch of the PROCESS-level active account. The
    /// IPC `account.switch` is session-level and does NOT come through here.
    pub fn switch(&mut self, id: &AccountId) -> CoreResult<()> {
        if !self.accounts.contains_key(id) {
            return Err(unknown_account(id));
        }
        self.active = id.clone();
        Ok(())
    }

    /// Raw status write (§12.1). Tighten-only is enforced at the IPC arm with
    /// the operator-facing diagnostic; this is the storage path.
    pub fn set_status(
        &mut self,
        id: &AccountId,
        status: AccountStatus,
        now_ms: i64,
    ) -> CoreResult<()> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| unknown_account(id))?;
        account.status = status;
        account.updated_at_ms = now_ms;
        Ok(())
    }

    /// E28 Gate 2 (§9.1): the account's lifecycle status must PERMIT this
    /// order — only `Active` places new entries; closes pass every posture
    /// except `ReadOnly` (a freeze must never be the reason a position is
    /// trapped — the kill-switch posture). An unknown account is refused with
    /// the standard `unknown account` error, never implicitly created
    /// (reverse acceptance C).
    pub fn permits_order(&self, id: &AccountId, is_close: bool) -> CoreResult<()> {
        let Some(account) = self.accounts.get(id) else {
            return Err(unknown_account(id));
        };
        let allowed = if is_close {
            account.status.allows_closes()
        } else {
            account.status.allows_entries()
        };
        if !allowed {
            return Err(CoreError::new(
                CoreErrorCode::AccountLimit,
                format!(
                    "account `{id}` is {} — {} refused at Gate 2",
                    account.status,
                    if is_close { "closes" } else { "new entries" },
                ),
            ));
        }
        Ok(())
    }
}

fn unknown_account(id: &AccountId) -> CoreError {
    CoreError::new(
        CoreErrorCode::InvalidParams,
        format!(
            "unknown account `{id}` — accounts are deployment facts \
             (user_layer/configs/accounts.toml + restart), never created \
             implicitly at runtime"
        ),
    )
}

#[cfg(test)]
mod book_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn two_accounts() -> AccountLedgers {
        AccountLedgers::new(
            vec![
                Account {
                    id: AccountId("default".into()),
                    name: "main".into(),
                    market_type: MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: CredentialKeys::default(),
                    updated_at_ms: 1,
                },
                Account {
                    id: AccountId("paper".into()),
                    name: "paper".into(),
                    market_type: MarketType::Prediction,
                    status: AccountStatus::ReadOnly,
                    credential_keys: CredentialKeys::default(),
                    updated_at_ms: 1,
                },
            ],
            default_account_id(),
        )
    }

    #[test]
    fn get_mut_refuses_an_unknown_account_explicitly() {
        let mut book = two_accounts();
        let err = book
            .get_mut(&AccountId("ghost".into()))
            .expect_err("an unknown account must be refused, never created");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("unknown account `ghost`"),
            "{}",
            err.message
        );
        // And nothing was created by the refused call:
        assert!(book.account(&AccountId("ghost".into())).is_none());
    }

    #[test]
    fn ledgers_are_isolated_per_account() {
        let mut book = two_accounts();
        book.get_mut(&AccountId("paper".into()))
            .expect("paper is configured")
            .set_balance(dec!(500));
        assert_eq!(
            book.ledger_of(&default_account_id()).unwrap().balance(),
            dec!(0)
        );
        assert_eq!(
            book.ledger_of(&AccountId("paper".into()))
                .unwrap()
                .balance(),
            dec!(500)
        );
    }

    #[test]
    fn switch_refuses_unknown_and_moves_the_active_ledger() {
        let mut book = two_accounts();
        book.switch(&AccountId("paper".into())).expect("configured");
        assert_eq!(book.active_id().as_str(), "paper");
        book.get_mut(&AccountId("paper".into()))
            .unwrap()
            .set_balance(dec!(7));
        assert_eq!(book.active_ledger().balance(), dec!(7));
        let err = book
            .switch(&AccountId("ghost".into()))
            .expect_err("unknown");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
    }

    #[test]
    fn status_can_only_tighten() {
        use AccountStatus::{Active, Frozen, ReadOnly, Suspended};
        let suspended = Suspended {
            reason: "ops".into(),
        };
        // The acceptance case: frozen -> active is a loosening, refused.
        assert!(!Frozen.can_tighten_to(&Active));
        // read_only -> frozen re-grants the close exemption: also loosening.
        assert!(!ReadOnly.can_tighten_to(&Frozen));
        assert!(!ReadOnly.can_tighten_to(&Active));
        // Active may go anywhere; same-posture lateral moves are allowed.
        assert!(Active.can_tighten_to(&Frozen));
        assert!(Active.can_tighten_to(&ReadOnly));
        assert!(Active.can_tighten_to(&suspended));
        assert!(Frozen.can_tighten_to(&suspended));
        assert!(suspended.can_tighten_to(&Frozen));
        // Frozen -> read_only tightens (close exemption removed).
        assert!(Frozen.can_tighten_to(&ReadOnly));
    }

    #[test]
    fn set_status_records_and_validates_the_account() {
        let mut book = two_accounts();
        book.set_status(&AccountId("paper".into()), AccountStatus::Frozen, 42)
            .expect("configured");
        assert!(matches!(
            book.account(&AccountId("paper".into())).unwrap().status,
            AccountStatus::Frozen
        ));
        let err = book
            .set_status(&AccountId("ghost".into()), AccountStatus::Frozen, 42)
            .expect_err("unknown");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
    }

    #[test]
    fn single_default_matches_the_zero_config_deployment() {
        let book = AccountLedgers::single_default(7);
        assert_eq!(book.account_list().len(), 1);
        assert_eq!(book.active_id().as_str(), DEFAULT_ACCOUNT_ID);
        let a = book.account(&default_account_id()).unwrap();
        assert!(matches!(a.status, AccountStatus::Active));
        assert!(!a.credential_keys.loaded, "no keys named, nothing loaded");
    }

    #[test]
    fn account_list_is_sorted_for_a_stable_wire_order() {
        let book = two_accounts();
        let ids: Vec<&str> = book.account_list().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "paper"]);
    }

    #[test]
    fn permits_order_enforces_gate_2_postures() {
        let mut book = two_accounts(); // default = Active, paper = ReadOnly
        assert!(book.permits_order(&default_account_id(), false).is_ok());
        assert!(book.permits_order(&default_account_id(), true).is_ok());
        // ReadOnly refuses BOTH sides — the one full stop.
        let err = book
            .permits_order(&AccountId("paper".into()), false)
            .expect_err("read_only refuses entries");
        assert_eq!(err.code, CoreErrorCode::AccountLimit);
        assert!(err.message.contains("is read_only"), "{}", err.message);
        let err = book
            .permits_order(&AccountId("paper".into()), true)
            .expect_err("read_only refuses closes");
        assert_eq!(err.code, CoreErrorCode::AccountLimit);
        // Frozen refuses entries but must never trap a close.
        book.set_status(&AccountId("paper".into()), AccountStatus::Frozen, 2)
            .unwrap();
        assert!(
            book.permits_order(&AccountId("paper".into()), false)
                .is_err()
        );
        assert!(book.permits_order(&AccountId("paper".into()), true).is_ok());
        // An unknown account is refused explicitly, never created.
        let err = book
            .permits_order(&AccountId("ghost".into()), false)
            .expect_err("unknown account");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(book.account(&AccountId("ghost".into())).is_none());
    }
}
