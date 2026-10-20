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
///
/// **Declaration-only.** This struct records the *names* of credential
/// environment variables and a `loaded` boolean. It is **not consumed on
/// the order path** — no code in `place_pending` / `place_gated` /
/// `OrderExecutor` reads this struct. Signing is performed per-venue by
/// the executor of that venue, reading its own env vars (see
/// `crate::market::plugin_credential_vars`).
///
/// `credentialsLoaded == true` does **not** imply "this account can place
/// orders on every venue"; it only means the declared env vars were
/// observed at load time. For the actual cross-venue signing model see
/// the engine's `set_account_venue` docs and `run_pair_orphan_sweep`.
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
    /// The venue this account trades on (a market-plugin name, e.g.
    /// `polymarket`). Empty = UNBOUND: the account may trade any venue — the
    /// 0.2 behaviour, and what `single_default()` and a file without the
    /// field produce. Non-empty: `orders.place` refuses an order whose token
    /// belongs to a different venue's round market (money must not leave
    /// through a book that was never wired to that venue).
    #[serde(default)]
    pub venue: String,
    /// The trading switch (the panel's per-account 「开启交易/关闭交易」).
    /// `false` = the account REFUSES new entries at Gate 2 while its exits
    /// keep flowing (positions stay strategy-managed until flat). It is a
    /// boolean axis SEPARATE from `status`: Frozen/ReadOnly are risk words
    /// (tighten-only, no runtime loosening) and ReadOnly refuses closes too —
    /// neither spells "stop entering, finish managing what you hold".
    /// Default `true` — a file without the field, `single_default()`, and
    /// `account.create` all produce a trading account.
    #[serde(default = "default_true")]
    pub trading_enabled: bool,
    /// THIS account's strategy ENABLEMENT set — the per-(account, strategy)
    /// 启用按钮 state the panel persists (账户↔venue 解耦语义裁决). NOT an
    /// assignment and NOT a visibility filter: every installed strategy is
    /// visible to every account; this list names the ones whose (account,
    /// strategy) scope actually RUNS. A scope trades ⇔ the strategy is
    /// installed AND the account is enabled (trading switch) AND the
    /// strategy ∈ this set. Disabling a scope that still holds positions
    /// demotes it EXITS-ONLY (no new entries; exits keep flowing to flat) —
    /// never a teardown. Names are validated at write time
    /// (`strategy.enable` / `account.strategies.set`), not at parse.
    /// Absent/empty = the explicit "this account runs no strategies" set
    /// (per-account enablement is the only truth — see
    /// [`AccountLedgers::strategy_assignment`]); on a fresh single-default
    /// deployment that spelling just leaves the account silent, the
    /// historical zero-config shape.
    #[serde(default)]
    pub strategies: Vec<String>,
    pub updated_at_ms: i64,
}

/// serde default for `trading_enabled` — a named fn because `bool::default`
/// spells `false`, the wrong default for a trading switch.
fn default_true() -> bool {
    true
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
                // Zero-config deployment: unbound — trades whatever venue is
                // active, exactly the 0.2 behaviour.
                venue: String::new(),
                trading_enabled: true,
                // Zero-config deployment: no enablement key — the account's
                // scopes follow the GLOBAL strategy toggle, exactly the 0.2
                // behaviour.
                strategies: Vec::new(),
                updated_at_ms: now_ms,
            }],
            default_account_id(),
        )
    }

    /// Seed EVERY account's ledger with `seed` (E28, locally-settling modes):
    /// a multi-account book is not one wallet with n aliases — each account
    /// owns a book that starts on its own seed, so the reserve/overspend gate
    /// is meaningful per account exactly as it was for the single one. With
    /// one configured account this is the same single `seed` 0.2 ran
    /// (bit-identical path for the existing deployment). Journal kind =
    /// `Seed` (never `Realign`): the boot placement is a seed, and the
    /// boot-replay logic keys on that spelling.
    pub fn seed_all(&mut self, seed: rust_decimal::Decimal) {
        for ledger in self.ledgers.values_mut() {
            ledger.seed(seed);
        }
    }

    /// `system.reset_trading`'s re-seed: every account returns to the
    /// configured seed, and the cash journal records the gap as ONE `Reset`
    /// row per account (append-only evidence — a Dry reset is an audited
    /// wipe, not a trail truncation). Distinct from [`Self::seed_all`]'s
    /// `Seed` kind so a replay can tell "boot funding" from "the operator
    /// reset the environment".
    pub fn reset_all_to_seed(&mut self, seed: rust_decimal::Decimal) {
        for ledger in self.ledgers.values_mut() {
            ledger.reset_to_seed(seed);
        }
    }

    /// The process-level default account (§9.5). Session-level switches live
    /// at the IPC layer per connection and NEVER mutate this.
    pub fn active_id(&self) -> &AccountId {
        &self.active
    }

    /// Attach the durable cash-journal file to every account's ledger
    /// (`data/ledger/<account>.journal.jsonl` in the caller-provided
    /// directory) and REPLAY it: the last row's `balance_after` becomes the
    /// account's opening balance. The boot seed (`seed_all`) runs AFTER this
    /// and the caller must SKIP it for accounts that replayed a journal —
    /// [`Core::with_account_book`] reports which did through
    /// [`Self::any_journal_on_disk`]-style checks at the call site.
    pub(crate) fn attach_journals(&mut self, dir: &std::path::Path) {
        for (id, ledger) in self.ledgers.iter_mut() {
            let path = dir.join(format!("{}.journal.jsonl", id.as_str()));
            ledger.set_journal_path(path.clone());
            let rows = crate::jsonl::load::<crate::ledger::JournalEntry>(
                &path,
                "ledger journal: skipped unparseable lines",
            );
            if !rows.is_empty() {
                tracing::info!(
                    account = %id.as_str(),
                    rows = rows.len(),
                    path = %path.display(),
                    "replaying ledger cash journal — balance restored from the last row"
                );
            }
            ledger.replay_journal(rows);
        }
    }

    /// Whether ANY account's journal file already exists on disk. The boot
    /// seed decision reads this: a deployment with journals replays them and
    /// must NOT be re-seeded (a re-seed would overwrite the restored balance
    /// and vaporize the盈亏 history — the exact fault this migration exists
    /// to fix); a fresh deployment (no journal files) seeds exactly as before.
    pub fn any_journal_on_disk(&self, dir: &std::path::Path) -> bool {
        self.accounts
            .keys()
            .any(|id| dir.join(format!("{}.journal.jsonl", id.as_str())).exists())
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

    /// The account↔venue binding write (`account.venue.set`): an EMPTY string
    /// is the legal "unbound" value (the account trades any venue — the
    /// historical behaviour), a non-empty one pins the account to that
    /// market-plugin name. Multiple accounts MAY share one venue — same
    /// exchange, several API sets, one account each, is precisely the
    /// deployment model the book exists for, so no uniqueness is enforced
    /// here or anywhere downstream.
    ///
    /// Forward-looking: this field is the input face of the next batch's
    /// plugin-ENABLEMENT derivation (boot reads the account book and enables
    /// every venue at least one account is bound to), so both arms of the
    /// write — this in-memory one and the accounts.toml persist beside it —
    /// must stay exactly correct.
    pub fn set_venue(&mut self, id: &AccountId, venue: &str, now_ms: i64) -> CoreResult<()> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| unknown_account(id))?;
        account.venue = venue.to_string();
        account.updated_at_ms = now_ms;
        Ok(())
    }

    /// The trading-switch write (`account.trading.set`): `false` = the
    /// account's NEW ENTRIES are refused at Gate 2 while its EXITS keep
    /// flowing; `true` re-opens entries. A separate boolean axis from
    /// `status` — this is the operator's day-to-day switch, `status` is the
    /// risk posture (tighten-only). The caller owns the accounts.toml
    /// persist (memory first, disk after — a persist failure refuses the
    /// whole write upstream).
    pub fn set_trading(&mut self, id: &AccountId, enabled: bool, now_ms: i64) -> CoreResult<()> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| unknown_account(id))?;
        account.trading_enabled = enabled;
        account.updated_at_ms = now_ms;
        Ok(())
    }

    /// The strategy-ENABLEMENT write (账户↔venue 解耦语义): the names are
    /// stored verbatim (the CALLER validated them against the registered
    /// set); an empty list is the explicit "this account runs no strategies"
    /// set (per-account enablement is the only truth — the old "empty =
    /// inherit the global toggle" reading is retired). The engine's scope
    /// rebuild rides a separate path — this only moves the book's fact.
    pub fn set_strategies(
        &mut self,
        id: &AccountId,
        strategies: Vec<String>,
        now_ms: i64,
    ) -> CoreResult<()> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| unknown_account(id))?;
        account.strategies = strategies;
        account.updated_at_ms = now_ms;
        Ok(())
    }

    /// THIS account's strategy ENABLEMENT set (账户↔venue 解耦语义裁决):
    /// the panel's per-(account, strategy) 启用按钮 state. NOT an
    /// assignment and NOT a visibility filter — every installed strategy is
    /// visible to every account; this list names the ones whose scope
    /// actually RUNS. `None` (empty/missing key) = this account runs
    /// NOTHING explicitly (per-account enablement is the only truth; the
    /// old "absence = the GLOBAL toggle governs" reading is retired — on a
    /// fresh deployment the absent key simply leaves the default account
    /// silent, which is the zero-config shape). Non-empty: exactly these
    /// (strategy, account) scopes run (the engine hosts one instance per
    /// pair). Names are validated at write time (`strategy.enable` /
    /// `account.strategies.set`), not here.
    pub fn strategy_assignment(&self, id: &AccountId) -> Option<&Vec<String>> {
        self.accounts
            .get(id)
            .filter(|a| !a.strategies.is_empty())
            .map(|a| &a.strategies)
    }

    /// The runtime-mint half of `account.create`: register a NEW account with
    /// a fresh zero `Ledger`. The storage layer refuses a duplicate id (the
    /// caller's arms refuse the reserved `default` and the empty spellings
    /// before this is reached — here the invariant is a pure book one: two
    /// accounts with one id would collapse into a HashMap slot, the exact
    /// accident the file loader refuses at boot). The account arrives as the
    /// caller built it (`Active` + `Prediction` is the only posture the IPC
    /// verb mints) and its ledger starts at zero — funding rides the normal
    /// journal/seed paths, never an implicit balance.
    pub fn insert(&mut self, account: Account) -> CoreResult<()> {
        if self.accounts.contains_key(&account.id) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account `{}` already exists — accounts are unique, a second \
                     book under one id would silently pay for the first's orders",
                    account.id
                ),
            ));
        }
        self.ledgers.insert(account.id.clone(), Ledger::new());
        self.accounts.insert(account.id.clone(), account);
        Ok(())
    }

    /// The retirement half of `account.remove`: drop the account AND its
    /// ledger. Every id is removable here — `default` included (the runtime
    /// keeps at least ONE account via the caller's last-account refusal; the
    /// zero-config fallback re-derives on the next boot when the file lacks
    /// the row). An unknown id is the standard refusal; removing the ACTIVE
    /// account is legal at this layer — the caller repairs the active cell
    /// beside it (the session switchers resolve against the book on their
    /// next read).
    pub fn remove(&mut self, id: &AccountId) -> CoreResult<()> {
        if self.accounts.remove(id).is_none() {
            return Err(unknown_account(id));
        }
        self.ledgers.remove(id);
        Ok(())
    }

    /// Repair the process-level active cell after a removal: when the retired
    /// id WAS active, fall back to `default` (when the surviving book still
    /// configures it — the same resolution the file loader runs); when it
    /// does not, the first surviving entry takes the cell.
    pub fn repair_active_after_removal(&mut self, removed: &AccountId) {
        if self.active == *removed && !self.accounts.contains_key(&self.active) {
            self.active = if self.accounts.contains_key(&default_account_id()) {
                default_account_id()
            } else {
                // `remove` left at least one account (the empty-book refusal
                // upstream); HashMap order is not stable, so take the lowest
                // id — the file loader's "first entry" analogue for a book
                // whose order is now memory-only.
                self.accounts
                    .keys()
                    .min()
                    .cloned()
                    .unwrap_or_else(default_account_id)
            };
        }
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
            // A closed trading switch never traps a position: exits flow on
            // every posture and every trading_enabled value — 「关=拒开仓、
            // 放平仓」 is exactly this line plus the entry arm below.
            account.status.allows_closes()
        } else {
            account.trading_enabled && account.status.allows_entries()
        };
        if !allowed {
            let why = if !account.trading_enabled && !is_close {
                "trading-closed (开仓拒、平仓放行)".to_string()
            } else {
                account.status.to_string()
            };
            return Err(CoreError::new(
                CoreErrorCode::AccountLimit,
                format!(
                    "account `{id}` is {why} — {} refused at Gate 2",
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
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    updated_at_ms: 1,
                },
                Account {
                    id: AccountId("paper".into()),
                    name: "paper".into(),
                    market_type: MarketType::Prediction,
                    status: AccountStatus::ReadOnly,
                    credential_keys: CredentialKeys::default(),
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
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
    fn set_venue_writes_unbinds_and_refuses_unknown_accounts() {
        let mut book = two_accounts();
        // Bind: the non-empty value pins the account to a plugin name.
        book.set_venue(&AccountId("paper".into()), "kalshi", 7)
            .expect("configured");
        let a = book.account(&AccountId("paper".into())).unwrap();
        assert_eq!(a.venue, "kalshi");
        assert_eq!(a.updated_at_ms, 7, "the write refreshes the record");
        // Unbind: the empty string is the legal unbound value again.
        book.set_venue(&AccountId("paper".into()), "", 8)
            .expect("empty venue = unbound");
        assert_eq!(book.account(&AccountId("paper".into())).unwrap().venue, "");
        // Two accounts MAY carry the same venue — same exchange, several API
        // sets, one account each, is the book's whole deployment model.
        book.set_venue(&default_account_id(), "kalshi", 9)
            .expect("same venue on a second account is legal");
        book.set_venue(&AccountId("paper".into()), "kalshi", 9)
            .expect("…and re-binding after an unbind");
        assert_eq!(book.account(&default_account_id()).unwrap().venue, "kalshi");
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().venue,
            "kalshi"
        );
        // An unknown account is refused explicitly, never created.
        let err = book
            .set_venue(&AccountId("ghost".into()), "kalshi", 10)
            .expect_err("unknown");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("unknown account `ghost`"));
        assert!(book.account(&AccountId("ghost".into())).is_none());
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

    /// The trading switch (「关=拒开仓、放平仓」) is an INDEPENDENT axis from
    /// AccountStatus: OFF refuses entries but never traps a close, even with
    /// the account otherwise Active; ON restores entries; the stamp rides
    /// the writes; unknown ids refuse.
    #[test]
    fn trading_switch_refuses_entries_passes_closes() {
        let mut book = two_accounts();
        book.set_trading(&default_account_id(), false, 5)
            .expect("the default account takes the switch");
        // Entry refused, close allowed — the exact contract of the switch.
        let err = book
            .permits_order(&default_account_id(), false)
            .expect_err("trading-off refuses entries");
        assert_eq!(err.code, CoreErrorCode::AccountLimit);
        assert!(
            err.message.contains("trading-closed"),
            "the refusal names the switch, not the status: {}",
            err.message
        );
        assert!(
            book.permits_order(&default_account_id(), true).is_ok(),
            "trading-off never traps a close"
        );
        // The status axis is untouched: the account is still Active.
        assert!(matches!(
            book.account(&default_account_id()).unwrap().status,
            AccountStatus::Active
        ));
        // Re-open restores entries; the stamp rode the writes.
        book.set_trading(&default_account_id(), true, 9).unwrap();
        assert!(book.permits_order(&default_account_id(), false).is_ok());
        assert_eq!(
            book.account(&default_account_id()).unwrap().updated_at_ms,
            9
        );
        // Unknown ids refuse on the switch path too.
        assert!(
            book.set_trading(&AccountId("ghost".into()), false, 11)
                .is_err()
        );
    }

    /// The enablement write (`account.strategies.set` book half): names ride
    /// verbatim (the CALLER validated them against the registered set), an
    /// empty list is the explicit empty set (spelled "runs nothing" — the
    /// reader answers None), and unknown ids refuse.
    #[test]
    fn strategy_assignment_rides_verbatim_and_normalizes_empty_to_inherit() {
        let mut book = two_accounts();
        let paper = AccountId("paper".into());
        book.set_strategies(&paper, vec!["spread_arb".into()], 3)
            .expect("a named assignment rides");
        assert_eq!(
            book.strategy_assignment(&paper).unwrap(),
            &vec!["spread_arb".to_string()]
        );
        // The EMPTY SET: stores and reads back as None — the absent key is
        // the "this account runs nothing" spelling (the zero-config shape:
        // a fresh default account stays silent).
        book.set_strategies(&paper, Vec::new(), 4).unwrap();
        assert!(
            book.strategy_assignment(&paper).is_none(),
            "empty assignment = inherit-all (None)"
        );
        // Unknown id refuses.
        assert!(
            book.set_strategies(&AccountId("ghost".into()), vec!["x".into()], 5)
                .is_err()
        );
    }

    // ── account.create / account.remove 的簿内半（存储层）─────────────────────

    fn fresh_account(id: &str, now_ms: i64) -> Account {
        Account {
            id: AccountId::from(id),
            name: id.to_string(),
            market_type: MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            updated_at_ms: now_ms,
        }
    }

    #[test]
    fn insert_registers_the_account_with_a_fresh_zero_ledger() {
        let mut book = two_accounts();
        book.insert(fresh_account("poly-main", 7))
            .expect("a new id inserts");
        let a = book.account(&AccountId("poly-main".into())).unwrap();
        assert_eq!(a.id.as_str(), "poly-main");
        assert!(matches!(a.status, AccountStatus::Active));
        assert_eq!(a.updated_at_ms, 7, "the caller's stamp rides");
        // The new book is a ZERO book: present, funded by nothing.
        assert_eq!(
            book.ledger_of(&AccountId("poly-main".into()))
                .unwrap()
                .balance(),
            dec!(0)
        );
        // It is addressable by the standard gates and listable in order.
        book.require(&AccountId("poly-main".into()))
            .expect("require accepts the minted account");
        let ids: Vec<&str> = book.account_list().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "paper", "poly-main"]);
    }

    #[test]
    fn insert_refuses_a_duplicate_id_and_default_never_slips_through() {
        let mut book = two_accounts();
        let err = book
            .insert(fresh_account("paper", 7))
            .expect_err("duplicate id");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("already exists"), "{}", err.message);
        // The refused insert created nothing: the existing record is intact.
        assert_eq!(
            book.account(&AccountId("paper".into())).unwrap().name,
            "paper"
        );
    }

    #[test]
    fn remove_drops_account_and_ledger_including_default() {
        let mut book = two_accounts();
        book.get_mut(&AccountId("paper".into()))
            .unwrap()
            .set_balance(dec!(42));
        book.remove(&AccountId("paper".into())).expect("retires");
        assert!(book.account(&AccountId("paper".into())).is_none());
        assert!(
            book.ledger_of(&AccountId("paper".into())).is_err(),
            "the ledger goes with the account — no orphan book"
        );
        // `default` is an ordinary account now: removable like any other
        // (the last-account refusal lives in the runtime caller, not here).
        book.remove(&default_account_id()).expect("default retires");
        assert!(book.account(&default_account_id()).is_none());
        // An unknown id is the standard refusal.
        book.insert(fresh_account("other", 1)).unwrap();
        let err = book
            .remove(&AccountId("ghost".into()))
            .expect_err("unknown");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("unknown account `ghost`"));
    }

    #[test]
    fn removing_the_active_account_repairs_the_cell_to_default() {
        let mut book = two_accounts();
        book.switch(&AccountId("paper".into())).unwrap();
        book.remove(&AccountId("paper".into())).unwrap();
        book.repair_active_after_removal(&AccountId("paper".into()));
        assert_eq!(
            book.active_id().as_str(),
            DEFAULT_ACCOUNT_ID,
            "the retired active falls back to the reserved default"
        );
        // The cell repair is a no-op when the removed id was not active.
        book.switch(&AccountId("paper".into())).ok(); // unknown now — refused
        book.insert(fresh_account("other", 1)).unwrap();
        book.switch(&AccountId("other".into())).unwrap();
        // Removing a NON-active account (default here) leaves the cell alone.
        book.remove(&default_account_id()).expect("default retires");
        book.repair_active_after_removal(&default_account_id());
        assert_eq!(
            book.active_id().as_str(),
            "other",
            "an unrelated removal leaves the active cell alone"
        );
    }

    #[test]
    fn removing_default_repairs_the_cell_to_the_first_survivor() {
        // default active + default removed: the cell re-activates the first
        // surviving entry (lowest id) — the file loader's resolution for a
        // book whose `default` row left.
        let mut book = two_accounts();
        assert_eq!(book.active_id().as_str(), DEFAULT_ACCOUNT_ID);
        book.remove(&default_account_id()).unwrap();
        book.repair_active_after_removal(&default_account_id());
        assert_eq!(
            book.active_id().as_str(),
            "paper",
            "the active cell never dangles: first survivor takes it"
        );
        // When a surviving `default` exists it wins over the lowest id.
        let mut book2 = two_accounts();
        book2.remove(&AccountId("paper".into())).unwrap();
        book2.insert(fresh_account("aaa", 1)).unwrap();
        book2.switch(&AccountId("aaa".into())).unwrap();
        book2.remove(&AccountId("aaa".into())).unwrap();
        book2.repair_active_after_removal(&AccountId("aaa".into()));
        assert_eq!(
            book2.active_id().as_str(),
            DEFAULT_ACCOUNT_ID,
            "a surviving default is preferred over the lowest id"
        );
    }
}
