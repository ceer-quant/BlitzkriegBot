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

impl CredentialKeys {
    /// Convert a plugin's declared credential variable names into the legacy
    /// four-slot account declaration. These fields carry names only; execution
    /// still reads the plugin's own environment directly.
    pub fn from_plugin_env(names: &[&str]) -> Self {
        let mut out = Self::default();
        for name in names {
            let slot = if name.contains("PASSPHRASE") {
                &mut out.passphrase_env
            } else if name.contains("SECRET") {
                &mut out.secret_env
            } else if name.contains("PRIVATE") || name.contains("RSA") {
                &mut out.private_key_env
            } else {
                &mut out.api_key_env
            };
            if slot.is_none() {
                *slot = Some((*name).to_string());
            }
        }
        out
    }
}

// ── E28 (§9.1 / §9.3): the account book ─────────────────────────────────────

pub mod config;

pub use config::CrossVenueArbConfig;

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
    /// THIS account's round-length FAMILY declaration (轮次多族 v5): the
    /// durations, in seconds, this account trades — each paired with the
    /// account's venue (or EVERY venue when unbound) into the (venue,
    /// duration) family set `account_duration_subscriptions` assembles for
    /// the B1c admission gate. EMPTY = NO family restriction declared: the
    /// account trades whatever duration the data plane feeds, the
    /// historical behaviour (an added permission axis is never spelled as
    /// a subtraction — an empty set admits, it does not lock out). The
    /// global single-value chain (CLI/env > 格 > TOML > 900) is NOT clipped
    /// by this field: it feeds the gate's subscription set and the boot's
    /// discovery mounting, while the timing math keeps its own fallback
    /// (B1a's three-state read). Parse-side validated against the
    /// allowed-duration whitelist (fail-closed), deduplicated with
    /// first-seen order kept.
    #[serde(default)]
    pub rounds: Vec<i64>,
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
    /// The DISPLAY pointer (显示账户配置化): which account the snapshot's
    /// `displayAccount`/`displayVenue` name. A pure presentation pointer —
    /// it does NOT feed the active/accounting/strategy-enablement axes and
    /// no trading semantics attach to it. Invariant (same discipline as
    /// `active`): it ALWAYS names a configured account — the file loader
    /// derives it (explicit key > first active > first row), `set_display`
    /// refuses unknown ids, and `repair_display_after_removal` re-derives
    /// when a removal takes the pointed-at account away.
    display: AccountId,
}

impl AccountLedgers {
    /// Build the book from configured accounts; every account starts with a
    /// fresh `Ledger` (dry seeding happens through the normal `set_balance`
    /// path, unchanged). `active` names the process-level default account
    /// (§9.5: `default` when configured, else the file's first entry; the
    /// SESSION-level switch lives at the IPC layer and never mutates this).
    pub fn new(accounts: Vec<Account>, active: AccountId) -> Self {
        // The display pointer defaults to the same account the active cell
        // gets (显示账户配置化's zero-config shape: display what the process
        // acts as, until the file or `display.account.set` says otherwise).
        let display = active.clone();
        Self::new_with_display(accounts, active, display)
    }

    /// [`Self::new`] with the display pointer SUPPLIED — the accounts.toml
    /// loader's constructor (the file is the display pointer's only config
    /// source). `display` MUST name a configured account; enforcing that is
    /// the caller's job (the loader validates before calling, fail-closed).
    pub fn new_with_display(accounts: Vec<Account>, active: AccountId, display: AccountId) -> Self {
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
            display,
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
                // Zero-config deployment: no enablement key — an EMPTY set,
                // which is the explicit "run nothing here" reading (per-account
                // enablement is the only truth; the old 0.2 "empty follows the
                // GLOBAL toggle" behaviour is retired).
                strategies: Vec::new(),
                rounds: Vec::new(),
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

    /// issue 561 账户维重置：只翻新 `id` 这一本账 —— 余额回种子、journal
    /// 重写为单行 Reset 行（见 [`crate::ledger::Ledger::reset_account_to_seed`]
    /// 的语义论证）。显式拒绝未知账户，与 [`Self::get_mut`] 同一纪律；
    /// `ledger_of` 的只读口径也在此复用于回执余额。返回磁盘重写结果透传
    /// （false = 文件重写失败，内存已翻新，调用方决定告警）。
    pub fn reset_account_to_seed(
        &mut self,
        id: &AccountId,
        seed: rust_decimal::Decimal,
    ) -> CoreResult<bool> {
        self.get_mut(id)?.reset_account_to_seed(seed);
        Ok(true)
    }

    /// The process-level default account (§9.5). Session-level switches live
    /// at the IPC layer per connection and NEVER mutate this.
    pub fn active_id(&self) -> &AccountId {
        &self.active
    }

    /// The DISPLAY pointer's id (显示账户配置化): the account the snapshot's
    /// `displayAccount` names. Pure presentation — nothing here feeds the
    /// active/accounting/strategy axes.
    pub fn display_id(&self) -> &str {
        self.display.as_str()
    }

    /// The pointed-at account's venue binding (the snapshot's `displayVenue`).
    /// An empty string = the account is unbound (trades any venue) — the
    /// snapshot renders absence, the same spelling the binding itself uses.
    pub fn display_venue(&self) -> &str {
        self.accounts
            .get(&self.display)
            .map(|a| a.venue.as_str())
            .unwrap_or("")
    }

    /// The ACTIVE account's venue binding — the one venue whose cash report is
    /// comparable to the active book in the accounting audit (a sibling
    /// venue's cash is a different book's truth). Empty = unbound.
    pub fn active_venue(&self) -> &str {
        self.accounts
            .get(&self.active)
            .map(|a| a.venue.as_str())
            .unwrap_or("")
    }

    /// The ACTIVE-status account bound to `venue` (#505 account↔venue
    /// binding). Venue-scoped cash truth routes through here: a venue's
    /// balance belongs to the account that trades it. None = no active
    /// account carries that binding (the caller decides the fallback — the
    /// legacy single-account routing lands on the active ledger).
    ///
    /// #778: the result is DETERMINISTIC even when several active accounts
    /// share one venue. The book is a `HashMap`, so a plain `values().find`
    /// picked whoever iteration order produced first and venue cash landed on
    /// an arbitrary account. The rule is the book's own fallback order (the
    /// same one `repair_active_after_removal` / `repair_display_after_removal`
    /// run): the explicit `default` account wins when it is Active and bound
    /// to `venue`, otherwise the lexicographically SMALLEST account id.
    ///
    /// Note that same-venue accounts still share the venue's GLOBAL env
    /// credential at the executor (one wallet, several ledgers) — the
    /// collision itself needs an operator decision (ban the shared binding or
    /// give each account per-order credentials), referenced by issue 778; a
    /// loud warning fires at every binding write
    /// ([`Self::warn_shared_venue_binding`]). Determinism here only makes the
    /// cash ATTRIBUTION stable, it does not make the wallet split real.
    pub fn account_for_venue(&self, venue: &str) -> Option<&Account> {
        if let Some(a) = self.accounts.get(&default_account_id())
            && a.status == AccountStatus::Active
            && a.venue == venue
        {
            return Some(a);
        }
        self.accounts
            .values()
            .filter(|a| a.status == AccountStatus::Active && a.venue == venue)
            .min_by(|a, b| a.id.cmp(&b.id))
    }

    /// Every ACTIVE-status account bound to `venue` — the venue-scoped
    /// freeze's routing table (2026-10-25 多场馆雪崩定谳): one venue's failed
    /// sweep/self-check freezes the accounts that trade it, never the whole
    /// book. Empty = nobody trades this venue (the caller decides the
    /// fallback — the legacy global freeze keeps unbound deployments loud).
    pub fn account_ids_bound_to_venue(&self, venue: &str) -> Vec<String> {
        self.accounts
            .values()
            .filter(|a| a.status == AccountStatus::Active && a.venue == venue)
            .map(|a| a.id.to_string())
            .collect()
    }

    /// Re-point the display pointer (`display.account.set`'s memory half).
    /// The id must name a CONFIGURED account — the pointer's invariant; the
    /// ACTIVE-status check and the persistence are the CALLER's job (the
    /// service layer owns the plan-first refusals and the file write).
    pub fn set_display(&mut self, id: &AccountId) -> CoreResult<()> {
        if !self.accounts.contains_key(id) {
            return Err(unknown_account(id));
        }
        self.display = id.clone();
        Ok(())
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

    /// Apply freshly parsed account metadata without replacing any money book.
    /// Hot reload may change account settings, but adding or removing an account
    /// needs a restart because that changes the ledger topology.
    pub fn reload_config(&mut self, fresh: AccountLedgers) -> Result<(), String> {
        let mut current_ids: Vec<_> = self.accounts.keys().cloned().collect();
        let mut fresh_ids: Vec<_> = fresh.accounts.keys().cloned().collect();
        current_ids.sort();
        fresh_ids.sort();
        if current_ids != fresh_ids {
            return Err("account ids changed; restart the core to add or remove accounts".into());
        }

        self.accounts = fresh.accounts;
        self.active = fresh.active;
        self.display = fresh.display;
        Ok(())
    }

    /// Replace ONE account's round-family declaration (轮次多族) in memory.
    /// The durable half is the caller's job (B2's `account.rounds.set`
    /// persists into accounts.toml beside this write — the venue.set
    /// transaction posture). Unknown ids refuse (the book never mints an
    /// implicit account). The value arrives parse-validated and
    /// deduplicated; the set path re-checks the plugins' DECLARED union
    /// (`allowed` — the registry union the Core threads through) so an IPC
    /// caller cannot bypass what the parser enforces (same fail-closed
    /// posture at every door; an empty union refuses every value).
    pub fn set_rounds(
        &mut self,
        id: &AccountId,
        rounds: Vec<i64>,
        allowed: &[i64],
    ) -> CoreResult<()> {
        let account = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| unknown_account(id))?;
        for sec in &rounds {
            if !allowed.contains(sec) {
                return Err(crate::model::CoreError::new(
                    crate::model::CoreErrorCode::InvalidParams,
                    format!(
                        "rounds value {sec}s is not declared by any market plugin \
                         (declared union: {allowed:?})"
                    ),
                ));
            }
        }
        account.rounds = rounds;
        Ok(())
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
        {
            let account = self
                .accounts
                .get_mut(id)
                .ok_or_else(|| unknown_account(id))?;
            account.venue = venue.to_string();
            account.updated_at_ms = now_ms;
        }
        // #778(a): the binding is now written — shout when it makes two
        // accounts share the venue's GLOBAL credential (one wallet split
        // across two ledgers).
        self.warn_shared_venue_binding(id, venue);
        Ok(())
    }

    /// #778(a): WARN — loudly and per occurrence — when `id`'s binding to
    /// `venue` means another account already owns that venue. The executor
    /// signs with the venue's GLOBAL environment credential, so the two
    /// accounts trade ONE wallet while the ledger books them separately: fills
    /// land in one book, the other's risk math sees cash that is not its own.
    /// Uniqueness is deliberately NOT enforced (that would break live operator
    /// configs); the collision still needs an operator decision — ban the
    /// shared binding or give each account per-order credentials (issue 778).
    /// One warn per already-bound sibling, naming both ids and the venue.
    fn warn_shared_venue_binding(&self, id: &AccountId, venue: &str) {
        if venue.is_empty() {
            return;
        }
        let mut owners: Vec<&str> = self
            .accounts
            .values()
            .filter(|a| a.id != *id && a.venue == venue)
            .map(|a| a.id.as_str())
            .collect();
        owners.sort_unstable();
        for owner in owners {
            tracing::warn!(
                account = %id,
                other_account = %owner,
                venue = %venue,
                "account `{id}` is bound to venue `{venue}` which `{owner}` already \
                 owns: both accounts will sign with the venue's GLOBAL env credential \
                 — ONE wallet, two ledgers (#778). Not refused (live configs may rely \
                 on it); the same-wallet collision needs an operator decision: ban the \
                 shared binding or give each account per-order credentials",
            );
        }
    }

    /// #778(a): the boot-time sweep — warn for every configured account whose
    /// venue binding collides with an EARLIER (sorted-id) account's. Runtime
    /// writes warn through [`Self::warn_shared_venue_binding`] directly; the
    /// file/env loaders construct the book wholesale, so they call this once
    /// after construction.
    pub(crate) fn warn_shared_venue_bindings(&self) {
        let sorted = self.account_list();
        for (i, account) in sorted.iter().enumerate() {
            if account.venue.is_empty() {
                continue;
            }
            for owner in sorted[..i].iter().filter(|p| p.venue == account.venue) {
                tracing::warn!(
                    account = %account.id,
                    other_account = %owner.id,
                    venue = %account.venue,
                    "account `{}` is bound to venue `{}` which `{}` already owns: both \
                     accounts will sign with the venue's GLOBAL env credential — ONE \
                     wallet, two ledgers (#778). Not refused; the same-wallet collision \
                     needs an operator decision (ban or per-order credentials)",
                    account.id,
                    account.venue,
                    owner.id,
                );
            }
        }
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
        // #778(a): account.create's binding path — `insert` bypasses
        // `set_venue`, so the shared-venue warning must fire here too.
        let id = account.id.clone();
        let venue = account.venue.clone();
        self.accounts.insert(account.id.clone(), account);
        self.warn_shared_venue_binding(&id, &venue);
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

    /// The display pointer's removal companion: when the removed account is
    /// the one being displayed, re-derive the pointer the boot loader's own
    /// order (first `active` entry, else lowest id — the file can no longer
    /// answer, so memory spells the same default the next boot would read).
    /// The PERSISTED file is the CALLER's concern: the service layer rewrites
    /// the top-level `display_account` key beside the removal so the next
    /// boot never reads a dangling id (which the loader refuses, fail-closed).
    pub fn repair_display_after_removal(&mut self, removed: &AccountId) {
        if self.display == *removed && !self.accounts.contains_key(&self.display) {
            self.display = self
                .accounts
                .values()
                .filter(|a| matches!(a.status, AccountStatus::Active))
                .map(|a| a.id.clone())
                .min()
                .unwrap_or_else(|| {
                    self.accounts
                        .keys()
                        .min()
                        .cloned()
                        .unwrap_or_else(default_account_id)
                });
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
mod tests;
