//! E26 (DEV_V0_3 §4): systemic risk limits — the numbers that bound how much
//! of the account the next trade can put at risk, plus WHERE each number came
//! from.
//!
//! Two §4.1 principles shape everything here:
//!
//! 1. **Factory-silent**: every NEW limit defaults to `0 = off`. An
//!    unconfigured kernel behaves exactly as before — the same convention the
//!    existing `RiskConfig` limits already follow, and the only posture this
//!    codebase allows for new risk enforcement.
//! 2. **Provenance is part of the value**: the `risk.limits` readout (§4.4)
//!    never shows a bare number. "35" without "env" is not an answer an
//!    operator can act on — they would not know whether a restart, a flag, or
//!    a stale file produced it. So every limit is a [`Bound`]: value + source,
//!    in one type, all the way to the wire.

use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::BTreeMap;

/// Where a limit's effective value came from (§4.4). The same four words the
/// kernel's startup provenance has always used ("every setting above its
/// default records its source"), now carried per-limit instead of per-boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitSource {
    /// The compiled-in default. For the nine new limits: 0 = off (§4.1).
    Default,
    /// An environment variable (`.env` folded into the process environment).
    Env,
    /// A CLI flag — the strongest link of the existing chain.
    Flag,
}

impl LimitSource {
    /// The provenance word the boot log spells (`env`, `flag`, `default`) —
    /// the same words the kernel's provenance report has always used, kept as
    /// a method here so the wire's vocabulary never has to reach into the
    /// config module for its spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::Default => "default",
            LimitSource::Env => "env",
            LimitSource::Flag => "flag",
        }
    }
}

/// The two vocabularies are the SAME layers with two names for the strongest
/// one (the config layer says `cli`, the wire says `flag`). The map is 1:1 by
/// construction, so a resolved budget can name its true layer without a
/// second spelling table drifting apart.
impl From<crate::config::Source> for LimitSource {
    fn from(s: crate::config::Source) -> Self {
        match s {
            crate::config::Source::Cli => LimitSource::Flag,
            crate::config::Source::Env => LimitSource::Env,
            crate::config::Source::Default => LimitSource::Default,
        }
    }
}

/// One limit as the readout carries it: the effective value and its source.
///
/// The value is a `Decimal` internally and crosses the wire as a STRING
/// (§4.4's example shows `"35"`): a limit readout is a fact to display, not an
/// operand to compute with, and `Decimal::to_string()` keeps `0` looking like
/// `0` — the panel renders it verbatim with no float-shaped surprises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    value: Decimal,
    source: LimitSource,
}

impl Bound {
    /// The factory posture: 0 = off (§4.1).
    pub fn off() -> Self {
        Self {
            value: Decimal::ZERO,
            source: LimitSource::Default,
        }
    }

    /// A configured value with the source that provided it.
    pub fn new(value: Decimal, source: LimitSource) -> Self {
        Self { value, source }
    }

    /// The effective value. `0` means the limit is OFF — every consumer
    /// branches on [`Self::is_enabled`] rather than re-stating that rule.
    pub fn value(&self) -> Decimal {
        self.value
    }

    pub fn source(&self) -> LimitSource {
        self.source
    }

    /// `0 = off` (§4.1): the one branch every enforcement site uses, so the
    /// "zero disables" rule lives in exactly one place.
    pub fn is_enabled(&self) -> bool {
        self.value > Decimal::ZERO
    }
}

impl Default for Bound {
    fn default() -> Self {
        Self::off()
    }
}

impl Serialize for Bound {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Bound", 2)?;
        s.serialize_field("value", &self.value.to_string())?;
        s.serialize_field("source", &self.source)?;
        s.end()
    }
}

/// Per-account limits (§4.2). Each account holds one; two accounts never see
/// each other's budgets — the same independence the E28 ledgers already
/// guarantee for cash.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRiskLimits {
    /// Largest tolerable loss on ONE entry (USD), derived at Gate 2 from the
    /// bound stop price. Over-limit entries are `SizeReduced` down to exactly
    /// the cap; a size that cannot reach the cap is rejected.
    pub max_single_loss_usd: Bound,
    /// Daily drawdown ceiling (USD). THE SAME BUDGET as the existing
    /// `PositionConfig::max_daily_loss_usd` — configuring this writes the
    /// existing field; no second daily-loss counter exists.
    pub max_daily_drawdown_usd: Bound,
    /// Per-account, per-token position-size ceiling (shares).
    pub max_position_size: Bound,
    /// Consecutive-loss breaker (count). Feeds the existing `LossBreakers`
    /// semantics at account grain — no new streak counter is invented.
    pub max_consecutive_losses: Bound,
    /// Cooldown after the breaker fires (minutes → seconds at the existing
    /// consumer; the unit here matches the config file's, per §4.2).
    pub cooldown_minutes: Bound,
    /// Per-order notional cap (USD) for THIS account — folds the old
    /// process-level `--max-order-notional` CLI flag into the account-level
    /// systemic matrix so it persists to `.env`, hot-loads without restart,
    /// and is judged per-account rather than process-wide (issue #983).
    pub max_order_notional_usd: Bound,
}

impl AccountRiskLimits {
    /// True when any bound on THIS account is armed. The per-account form of
    /// [`SystemicRiskLimits::any_armed`], used by the admission path once the
    /// account has been resolved.
    pub fn any_armed(&self) -> bool {
        self.max_single_loss_usd.is_enabled()
            || self.max_daily_drawdown_usd.is_enabled()
            || self.max_position_size.is_enabled()
            || self.max_consecutive_losses.is_enabled()
            || self.cooldown_minutes.is_enabled()
            || self.max_order_notional_usd.is_enabled()
    }
}

/// The whole systemic-limit set a kernel runs under: the per-account matrix
/// only. Risk control follows the account's capital — there is NO process-wide
/// (global) cap; every bound here is judged against the request's OWN account
/// (issue #983).
///
/// `Default` is the FACTORY posture (§4.1): every `Bound` off, every source
/// `default` — an unconfigured kernel answers `risk.limits` with the §4.4
/// example's shape and enforces nothing new.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicRiskLimits {
    /// The factory/default matrix. Kept as the fallback for accounts without
    /// their own row (and for the legacy global env face), so an unconfigured
    /// kernel behaves exactly as before.
    pub account: AccountRiskLimits,
    /// Per-account overrides, keyed by `AccountId`. A hit here wins over
    /// `account`; a miss falls back to it. This is the map the SQLite
    /// `account_risk` table projects into at boot and the map the panel write
    /// path updates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, AccountRiskLimits>,
}

impl SystemicRiskLimits {
    /// The limits in force for `account_id`: the account's own row when one
    /// exists, otherwise the default/fallback matrix. The one lookup every
    /// consumer uses, so a missing account can never silently get another
    /// account's caps.
    pub fn account_for(&self, account_id: &str) -> &AccountRiskLimits {
        self.accounts.get(account_id).unwrap_or(&self.account)
    }

    /// Install (or replace) one account's row. The default matrix is left
    /// untouched — the caller decides whether this account is also the
    /// process default and should update `account` for backward compatibility.
    pub fn set_account(&mut self, account_id: &str, limits: AccountRiskLimits) {
        self.accounts.insert(account_id.to_string(), limits);
    }

    /// Every account with an explicit row, sorted for a stable read.
    pub fn account_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.accounts.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }
}

impl SystemicRiskLimits {
    /// Phase-5 (issue #967): the env face is the ONLY effective source. `.env`
    /// (folded into the process environment by the launcher) supplies the five
    /// per-account `BK_RISK_*` keys; a missing key stays the factory
    /// `Bound::off()`. No TOML layer is consulted — a `[risk]` section in a
    /// config file is inert.
    ///
    /// A key that is present but unparseable is surfaced as a `tracing::warn`
    /// naming the key and its raw value; an absent key stays the silent
    /// factory `Bound::off()` (§4.1). The boot layer emits a separate WARN
    /// when `.env` itself is missing or unreadable and all five keys are
    /// absent, so an operator bypassing the `blitzkrieg` wrapper is not left
    /// guessing why every limit stayed off.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// The pure lookup form of [`Self::from_env`] (tests drive it without
    /// touching the process environment). `lookup` returns the raw string so a
    /// bad value can be named in the warning.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let one = |key: &str| match lookup(key) {
            Some(raw) => match raw.trim().parse::<Decimal>() {
                Ok(v) => Bound::new(v, LimitSource::Env),
                Err(_) => {
                    tracing::warn!(
                        key = %key,
                        raw = %raw,
                        "risk key value unparseable — limit stays off (factory default)"
                    );
                    Bound::off()
                }
            },
            None => Bound::off(),
        };
        Self {
            account: AccountRiskLimits {
                max_single_loss_usd: one("BK_RISK_MAX_SINGLE_LOSS_USD"),
                max_daily_drawdown_usd: one("BK_RISK_MAX_DAILY_DRAWDOWN_USD"),
                max_position_size: one("BK_RISK_MAX_POSITION_SIZE"),
                max_consecutive_losses: one("BK_RISK_MAX_CONSECUTIVE_LOSSES"),
                cooldown_minutes: one("BK_RISK_COOLDOWN_MINUTES"),
                max_order_notional_usd: one("BK_RISK_MAX_ORDER_NOTIONAL_USD"),
            },
            accounts: BTreeMap::new(),
        }
    }

    /// The per-account form of [`Self::from_lookup`]: first resolve the
    /// default/global face, then overlay every account-suffixed key. Both
    /// spellings are accepted so an operator can write either
    /// `BK_RISK_MAX_POSITION_SIZE_PAPER` (suffix, the execution-policy
    /// convention) or `BK_RISK_PAPER_MAX_POSITION_SIZE` (infix). The first
    /// one found wins; a present-but-unparseable value warns and leaves the
    /// fallback bound standing.
    pub fn from_lookup_with_accounts<I, S>(
        accounts: I,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Self::from_lookup(&lookup);
        out.overlay_accounts_from_lookup(accounts, lookup);
        out
    }

    /// Overlay account-suffixed keys onto THIS matrix's existing default row
    /// (`self.account`). Unlike [`Self::from_lookup_with_accounts`], the base
    /// is the EFFECTIVE global matrix — including CLI-resolved values — so a
    /// per-account row inherits the same daily-loss/order-notional bounds the
    /// process resolved before the account-specific keys are applied.
    pub fn overlay_accounts_from_lookup<I, S>(
        &mut self,
        accounts: I,
        lookup: impl Fn(&str) -> Option<String>,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for account in accounts {
            let account = account.as_ref();
            let suffix = crate::config::account_env_suffix(account);
            let mut row = self.account.clone();
            let mut touched = false;
            for (field, env_suffix) in [
                ("max_single_loss_usd", "MAX_SINGLE_LOSS_USD"),
                ("max_daily_drawdown_usd", "MAX_DAILY_DRAWDOWN_USD"),
                ("max_position_size", "MAX_POSITION_SIZE"),
                ("max_consecutive_losses", "MAX_CONSECUTIVE_LOSSES"),
                ("cooldown_minutes", "COOLDOWN_MINUTES"),
                ("max_order_notional_usd", "MAX_ORDER_NOTIONAL_USD"),
            ] {
                let keys = [
                    format!("BK_RISK_{env_suffix}_{suffix}"),
                    format!("BK_RISK_{suffix}_{env_suffix}"),
                ];
                let Some(raw) = keys.iter().find_map(|k| lookup(k)) else {
                    continue;
                };
                match raw.trim().parse::<Decimal>() {
                    Ok(v) => {
                        set_account_bound(&mut row, field, Bound::new(v, LimitSource::Env));
                        touched = true;
                    }
                    Err(_) => tracing::warn!(
                        key = %keys[0],
                        raw = %raw,
                        "per-account risk key value unparseable — fallback bound stays"
                    ),
                }
            }
            if touched {
                self.accounts.insert(account.to_string(), row);
            }
        }
    }

    /// The process-environment form of [`Self::from_lookup_with_accounts`].
    pub fn from_env_with_accounts<I, S>(accounts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::from_lookup_with_accounts(accounts, |key| std::env::var(key).ok())
    }

    /// The boot-log face (§4 provenance): one line per ARMED limit, naming its
    /// source the way the kernel's provenance report has always spelled values.
    /// A factory kernel prints the single "at factory" line — the silent
    /// posture (§4.1) stays observable from the log, not invisible.
    pub fn report_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |name: &str, b: &Bound| {
            if b.is_enabled() {
                out.push(format!(
                    "risk.systemic: {}={} ({})",
                    name,
                    b.value(),
                    b.source().as_str()
                ));
            }
        };
        push("max_single_loss_usd", &self.account.max_single_loss_usd);
        push(
            "max_daily_drawdown_usd",
            &self.account.max_daily_drawdown_usd,
        );
        push("max_position_size", &self.account.max_position_size);
        push(
            "max_consecutive_losses",
            &self.account.max_consecutive_losses,
        );
        push("cooldown_minutes", &self.account.cooldown_minutes);
        push(
            "max_order_notional_usd",
            &self.account.max_order_notional_usd,
        );
        if out.is_empty() {
            out.push("risk.systemic: at factory (all six per-account limits off)".to_string());
        }
        out
    }

    /// True when ANY bound is armed (§4.2). The pipeline's factory gate: with
    /// nothing armed the systemic block produces no trace at all, so the
    /// shipped pipeline's output stays byte-identical (the audit's P1 parity
    /// is a byte gate, and an extra "nothing armed" trace would break it).
    pub fn any_armed(&self) -> bool {
        let a = &self.account;
        a.any_armed() || self.accounts.values().any(AccountRiskLimits::any_armed)
    }
}

/// Set one named bound on an [`AccountRiskLimits`] row. The string field
/// names are the same ones the cockpit write path uses.
pub(crate) fn set_account_bound(row: &mut AccountRiskLimits, field: &str, bound: Bound) {
    match field {
        "max_single_loss_usd" => row.max_single_loss_usd = bound,
        "max_daily_drawdown_usd" => row.max_daily_drawdown_usd = bound,
        "max_position_size" => row.max_position_size = bound,
        "max_consecutive_losses" => row.max_consecutive_losses = bound,
        "cooldown_minutes" => row.cooldown_minutes = bound,
        "max_order_notional_usd" => row.max_order_notional_usd = bound,
        other => debug_assert!(false, "unmapped account risk field {other}"),
    }
}

/// The single-loss cap's verdict for one entry (§4.2): shrink to exactly the
/// cap when the suggestion over-commits, refuse when even one share cannot
/// fit, pass otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleLossVerdict {
    /// The suggested size already fits under the cap.
    Within,
    /// Shrink to `approved` shares — the largest whole-share size whose
    /// worst-case loss (entry → bound stop) does not exceed the cap.
    /// `approved < suggested` holds by construction.
    Reduce { approved: Decimal },
    /// Even one share would over-commit (`缩不下`): Gate 2 rejects.
    Refuse,
}

/// Worst-case loss per share for an entry stopped at `stop_pct` below entry.
///
/// The stop percent is the BOUND stop (`effective_stop_pct`), so the number is
/// the same one the exit walker will enforce — the cap is computed against the
/// discipline that will actually fire, never a flatter assumption.
///
/// issue #963: the bound is measured at the venue's own resolution. A stop
/// fills at the best RESTING bid — a 0.01 tick AT or below the stop level,
/// never inside a tick — so a stop level that lands between ticks realizes
/// one tick more per share than the continuous number. Sizing against the
/// continuous figure approved sizes whose grid-realized worst case breached
/// the cap (entry 0.30, stop 12%: the level 0.264 fills at the 0.26 bid —
/// 13.33% per share, not 12%). The floor below is the tick-exact worst case
/// the ladder can actually book; the invariant `approved * loss_per_share
/// <= cap` now holds on the grid, not just on paper.
pub fn loss_per_share(entry_price: Decimal, stop_pct: Decimal) -> Decimal {
    let stop_price = entry_price * (Decimal::ONE - stop_pct / Decimal::from(100));
    let fillable = (stop_price * Decimal::from(100)).floor() / Decimal::from(100);
    entry_price - fillable
}

/// Judge one entry against `max_single_loss_usd` (§4.2). Whole-share only:
/// `approved` is floored, so `approved * loss_per_share <= cap` holds by
/// construction — the cap bounds, it never approximates upward.
pub fn single_loss_verdict(
    max_single_loss: Decimal,
    entry_price: Decimal,
    stop_pct: Decimal,
    suggested: Decimal,
) -> SingleLossVerdict {
    let per_share = loss_per_share(entry_price, stop_pct);
    if per_share <= Decimal::ZERO {
        // A non-positive worst case cannot over-commit the cap.
        return SingleLossVerdict::Within;
    }
    let approved = (max_single_loss / per_share).floor();
    if approved < Decimal::ONE {
        return SingleLossVerdict::Refuse;
    }
    if approved >= suggested {
        return SingleLossVerdict::Within;
    }
    SingleLossVerdict::Reduce { approved }
}

/// Which rejection vocabulary a systemic verdict speaks — mapped 1:1 onto the
/// arbitration pipeline's existing [`crate::arbitration::RejectReason`] arms.
/// Only the per-account matrix can refuse here (issue #983: no global caps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemicReject {
    /// The account matrix refused / could not size the entry.
    AccountLimit,
}

/// Gate 2's systemic verdict for one entry (§4.2): pass, shrink, or refuse.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryJudgment {
    /// Every armed bound is satisfied by the suggestion as-is.
    Pass,
    /// The entry fits only smaller. `limit` names the binding cap — the one
    /// that actually bit, when several could have.
    Reduce {
        approved: Decimal,
        limit: &'static str,
    },
    /// The entry is refused outright.
    Reject {
        reject: SystemicReject,
        detail: String,
    },
}

/// Judge one ENTRY against the per-account systemic limits (issue #983: risk
/// control follows the account's capital — NO process-wide caps). The account
/// sizing bounds (share cap, single-loss cap) SHRINK rather than refuse when
/// they can. A factory kernel has every bound off, so every suggestion passes.
///
/// `bound_stop_pct` is the BOUND stop (`effective_stop_pct` at entry time) —
/// the same number the survival binding carries, so the single-loss cap is
/// computed against the discipline that will actually fire.
pub fn judge_entry(
    limits: &SystemicRiskLimits,
    entry_price: Decimal,
    entry_size: Decimal,
    bound_stop_pct: Decimal,
) -> EntryJudgment {
    judge_entry_account(&limits.account, entry_price, entry_size, bound_stop_pct)
}

/// [`judge_entry`] against ONE account's already-resolved row. The
/// arbitration pipeline and the submission gate call this form after looking
/// the account up, so two accounts with different `max_position_size` /
/// `max_single_loss_usd` are judged against their own numbers.
pub fn judge_entry_account(
    limits: &AccountRiskLimits,
    entry_price: Decimal,
    entry_size: Decimal,
    bound_stop_pct: Decimal,
) -> EntryJudgment {
    // Account sizing: the share cap and the single-loss cap both SHRINK the
    // entry; the tighter survives and names itself. A cap that cannot fit
    // even the reduced entry refuses (`缩不下`).
    let mut approved = entry_size;
    let mut binding: Option<&'static str> = None;
    let size_cap = &limits.max_position_size;
    if size_cap.is_enabled() {
        // Whole shares only (prediction markets trade whole shares): a
        // fractional cap floors to the largest honest size.
        let cap = size_cap.value().floor();
        if cap < Decimal::ONE {
            return EntryJudgment::Reject {
                reject: SystemicReject::AccountLimit,
                detail: format!(
                    "max_position_size {}: fewer than one whole share fits",
                    size_cap.value()
                ),
            };
        }
        if approved > cap {
            approved = cap;
            binding = Some("max_position_size");
        }
    }
    let loss_cap = &limits.max_single_loss_usd;
    if loss_cap.is_enabled() {
        match single_loss_verdict(loss_cap.value(), entry_price, bound_stop_pct, approved) {
            SingleLossVerdict::Within => {}
            SingleLossVerdict::Reduce { approved: shrunk } => {
                approved = shrunk;
                binding = Some("max_single_loss_usd");
            }
            SingleLossVerdict::Refuse => {
                return EntryJudgment::Reject {
                    reject: SystemicReject::AccountLimit,
                    detail: format!(
                        "max_single_loss_usd {}: one share at stop {}% would lose {} — nothing fits",
                        loss_cap.value(),
                        bound_stop_pct,
                        loss_per_share(entry_price, bound_stop_pct)
                    ),
                };
            }
        }
    }
    match binding {
        Some(limit) => EntryJudgment::Reduce { approved, limit },
        None => EntryJudgment::Pass,
    }
}

/// The reserved [`crate::risk::LossBreakers`] key carrying the ACCOUNT-level
/// consecutive-loss streak (§4.2). One breaker per account, living in the same
/// map as the strategy breakers (KI-10) under a prefix strategy names can
/// never take — the kernel's strategy names come from library registration,
/// and an account key halting a same-named strategy would name itself in the
/// audit trace either way.
pub const ACCOUNT_BREAKER_KEY_PREFIX: &str = "__account__:";

/// The breaker key for `account_id`'s own streak. The ONE spelling of the
/// convention — the settlement side writes it, the pipeline reads it.
pub fn account_breaker_key(account_id: &str) -> String {
    format!("{ACCOUNT_BREAKER_KEY_PREFIX}{account_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn factory_limits_are_all_off_with_default_source() {
        let a = AccountRiskLimits::default();
        for (name, b) in [
            ("max_single_loss_usd", &a.max_single_loss_usd),
            ("max_daily_drawdown_usd", &a.max_daily_drawdown_usd),
            ("max_position_size", &a.max_position_size),
            ("max_consecutive_losses", &a.max_consecutive_losses),
            ("cooldown_minutes", &a.cooldown_minutes),
            ("max_order_notional_usd", &a.max_order_notional_usd),
        ] {
            assert!(!b.is_enabled(), "{name} must ship disabled");
            assert_eq!(b.source(), LimitSource::Default, "{name} provenance");
        }
    }

    #[test]
    fn zero_is_off_and_positive_is_on() {
        assert!(!Bound::new(Decimal::ZERO, LimitSource::Env).is_enabled());
        assert!(Bound::new(dec!(0.01), LimitSource::Env).is_enabled());
        assert!(Bound::new(dec!(35), LimitSource::Flag).is_enabled());
    }

    /// Issue #967 (Phase-5): the env face is the only effective source; every
    /// armed bound must name `env` and a missing key must stay at the factory
    /// posture (no TOML fallback anywhere in the chain).
    #[test]
    fn env_lookup_arms_all_six_and_names_env() {
        let keys = std::collections::HashMap::from([
            ("BK_RISK_MAX_SINGLE_LOSS_USD", dec!(0.5)),
            ("BK_RISK_MAX_DAILY_DRAWDOWN_USD", dec!(5)),
            ("BK_RISK_MAX_POSITION_SIZE", dec!(5)),
            ("BK_RISK_MAX_CONSECUTIVE_LOSSES", dec!(4)),
            ("BK_RISK_COOLDOWN_MINUTES", dec!(30)),
            ("BK_RISK_MAX_ORDER_NOTIONAL_USD", dec!(6)),
        ]);
        let s = SystemicRiskLimits::from_lookup(|k| keys.get(k).map(|v| v.to_string()));
        for (name, b) in [
            ("max_single_loss_usd", &s.account.max_single_loss_usd),
            ("max_daily_drawdown_usd", &s.account.max_daily_drawdown_usd),
            ("max_position_size", &s.account.max_position_size),
            ("max_consecutive_losses", &s.account.max_consecutive_losses),
            ("cooldown_minutes", &s.account.cooldown_minutes),
            ("max_order_notional_usd", &s.account.max_order_notional_usd),
        ] {
            assert!(b.is_enabled(), "{name} must be armed by .env");
            assert_eq!(b.source(), LimitSource::Env, "{name} provenance");
        }
        // No key → factory posture, never an env-sourced one.
        let empty = SystemicRiskLimits::from_lookup(|_| None as Option<String>);
        assert!(!empty.account.max_position_size.is_enabled());
        assert_eq!(
            empty.account.max_position_size.source(),
            LimitSource::Default
        );
    }

    #[test]
    fn bound_serializes_as_string_value_plus_source() {
        let doc = serde_json::to_value(Bound::new(dec!(35), LimitSource::Env)).unwrap();
        assert_eq!(doc["value"], "35");
        assert_eq!(doc["source"], "env");
        let doc = serde_json::to_value(Bound::off()).unwrap();
        assert_eq!(doc["value"], "0");
        assert_eq!(doc["source"], "default");
    }

    #[test]
    fn wire_camel_case_keys_match_the_contract_example() {
        let doc = serde_json::to_value(AccountRiskLimits::default()).unwrap();
        for key in [
            "maxSingleLossUsd",
            "maxDailyDrawdownUsd",
            "maxPositionSize",
            "maxConsecutiveLosses",
            "cooldownMinutes",
            "maxOrderNotionalUsd",
        ] {
            assert!(doc.get(key).is_some(), "missing wire key {key}");
        }
    }

    #[test]
    fn report_lines_at_factory_are_the_single_silent_line() {
        let lines = SystemicRiskLimits::default().report_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("at factory"), "{}", lines[0]);
        assert!(
            lines[0].contains("all six per-account limits off"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn report_lines_name_only_the_armed_limits_with_their_source() {
        let mut sys = SystemicRiskLimits::default();
        sys.account.max_single_loss_usd = Bound::new(dec!(35), LimitSource::Env);
        sys.account.max_position_size = Bound::new(dec!(1000), LimitSource::Flag);
        let lines = sys.report_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("max_single_loss_usd=35 (env)"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("max_position_size=1000 (flag)"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn single_loss_verdict_passes_a_size_that_fits() {
        // cap 35, entry 0.50, stop 12% => per-share 0.06 => 583 shares fit;
        // a suggestion of 100 is nowhere near the cap.
        let v = single_loss_verdict(dec!(35), dec!(0.5), dec!(12), dec!(100));
        assert_eq!(v, SingleLossVerdict::Within);
    }

    #[test]
    fn single_loss_verdict_reduces_to_the_whole_share_under_the_cap() {
        let v = single_loss_verdict(dec!(35), dec!(0.5), dec!(12), dec!(600));
        assert_eq!(
            v,
            SingleLossVerdict::Reduce {
                approved: dec!(583)
            }
        );
        // The invariant the cap is FOR: the approved size's worst case stays
        // under the cap, and one more share would not.
        let per = loss_per_share(dec!(0.5), dec!(12));
        assert!(dec!(583) * per <= dec!(35));
        assert!(
            dec!(584) * per > dec!(35),
            "584 is the first size over the cap"
        );
    }

    #[test]
    fn single_loss_verdict_refuses_when_one_share_over_commits() {
        // cap 0.01 against a 0.06 per-share worst case: even one share
        // over-commits, so Gate 2 must reject rather than shrink.
        let v = single_loss_verdict(dec!(0.01), dec!(0.5), dec!(12), dec!(10));
        assert_eq!(v, SingleLossVerdict::Refuse);
    }

    #[test]
    fn single_loss_verdict_ignores_a_non_positive_stop() {
        // A degenerate binding (stop_pct 0) cannot over-commit anything.
        assert_eq!(
            single_loss_verdict(dec!(35), dec!(0.5), Decimal::ZERO, dec!(10)),
            SingleLossVerdict::Within
        );
    }

    #[test]
    fn single_loss_verdict_grid_holds_the_cap_invariant() {
        // The deterministic feeder for the gate's randomized sweep (batch 5):
        // over a grid of caps, prices, stops and one large suggestion, an
        // approved size's worst case never exceeds the cap and never lands
        // above `suggested`.
        for cap_i in 1..=20 {
            for price_i in 1..=20 {
                for stop_i in [1_i64, 5, 12, 50, 100] {
                    let cap = Decimal::from(cap_i);
                    let price = Decimal::from(price_i) / Decimal::from(20);
                    let stop = Decimal::from(stop_i);
                    let per = loss_per_share(price, stop);
                    let suggested = Decimal::from(1000);
                    match single_loss_verdict(cap, price, stop, suggested) {
                        SingleLossVerdict::Within => {}
                        SingleLossVerdict::Reduce { approved } => {
                            assert!(approved < suggested);
                            assert!(approved >= Decimal::ONE);
                            assert!(
                                approved * per <= cap,
                                "cap {cap} price {price} stop {stop}: {approved} shares lose {}",
                                approved * per
                            );
                        }
                        SingleLossVerdict::Refuse => {
                            assert!(Decimal::ONE * per > cap);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn judge_entry_at_factory_passes_everything() {
        // §4.1: with every bound off, no suggestion is ever judged — not even
        // an absurd one. The shipped behavior, structurally silent.
        assert_eq!(
            judge_entry(
                &SystemicRiskLimits::default(),
                dec!(0.5),
                dec!(100),
                dec!(12)
            ),
            EntryJudgment::Pass
        );
    }

    #[test]
    fn position_size_cap_shrinks_to_whole_shares() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(7.5), LimitSource::Env);
        match judge_entry(&limits, dec!(0.5), dec!(100), dec!(12)) {
            EntryJudgment::Reduce { approved, limit } => {
                assert_eq!(approved, dec!(7), "the cap floors to whole shares");
                assert_eq!(limit, "max_position_size");
            }
            // Diagnostic only: the assert_eq! arm above is the verification;
            // the judgment value is not interpolated into the panic message
            // (CodeQL rust/cleartext-logging treats risk-decision values as
            // sensitive on the log sink).
            _other => panic!("expected a reduction"),
        }
    }

    #[test]
    fn single_loss_cap_shrinks_and_names_itself_when_tighter_than_size_cap() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(100), LimitSource::Env);
        // entry 0.50, stop 12% → per-share 0.06 → 583 shares fit the loss
        // cap; the size cap allows 100 → the loss cap does not bite.
        limits.account.max_single_loss_usd = Bound::new(dec!(35), LimitSource::Env);
        assert_eq!(
            judge_entry(&limits, dec!(0.5), dec!(100), dec!(12)),
            EntryJudgment::Pass
        );
        // A tighter loss cap bites: 6 × 0.06 = 0.36 <= 0.40 < 7 × 0.06.
        limits.account.max_single_loss_usd = Bound::new(dec!(0.40), LimitSource::Env);
        match judge_entry(&limits, dec!(0.5), dec!(100), dec!(12)) {
            EntryJudgment::Reduce { approved, limit } => {
                assert_eq!(approved, dec!(6));
                assert_eq!(limit, "max_single_loss_usd");
            }
            _other => panic!("expected a reduction"),
        }
    }

    #[test]
    fn a_loss_cap_nothing_fits_refuses_instead_of_guessing() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_single_loss_usd = Bound::new(dec!(0.01), LimitSource::Env);
        // One share's worst case (0.06) alone over-commits the 0.01 cap.
        match judge_entry(&limits, dec!(0.5), dec!(100), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::AccountLimit,
                detail,
            } => assert!(detail.contains("max_single_loss_usd"), "{detail}"),
            _other => panic!("expected account rejection"),
        }
    }

    #[test]
    fn a_size_cap_below_one_share_refuses() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(0.5), LimitSource::Env);
        match judge_entry(&limits, dec!(0.5), dec!(100), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::AccountLimit,
                detail,
            } => assert!(detail.contains("whole share"), "{detail}"),
            _other => panic!("expected account rejection"),
        }
    }

    /// issue #1189: two accounts with different rows are judged against their
    /// own caps. A row that is absent falls back to the default matrix.
    #[test]
    fn two_accounts_have_independent_position_size_caps() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(100), LimitSource::Default);
        limits.accounts.insert(
            "small".into(),
            AccountRiskLimits {
                max_position_size: Bound::new(dec!(5), LimitSource::Env),
                ..Default::default()
            },
        );
        limits.accounts.insert(
            "large".into(),
            AccountRiskLimits {
                max_position_size: Bound::new(dec!(50), LimitSource::Env),
                ..Default::default()
            },
        );
        assert_eq!(
            limits.account_for("small").max_position_size.value(),
            dec!(5)
        );
        assert_eq!(
            limits.account_for("large").max_position_size.value(),
            dec!(50)
        );
        assert_eq!(
            limits.account_for("unconfigured").max_position_size.value(),
            dec!(100)
        );
        assert_eq!(
            judge_entry_account(limits.account_for("small"), dec!(0.5), dec!(10), dec!(12)),
            EntryJudgment::Reduce {
                approved: dec!(5),
                limit: "max_position_size",
            }
        );
        assert_eq!(
            judge_entry_account(limits.account_for("large"), dec!(0.5), dec!(10), dec!(12)),
            EntryJudgment::Pass
        );
    }

    /// The per-account env face accepts both the suffix and infix spellings;
    /// the suffix is what the write path emits.
    #[test]
    fn per_account_risk_env_keys_overlay_the_global_face() {
        let vars = std::collections::HashMap::from([
            ("BK_RISK_MAX_POSITION_SIZE".to_string(), "100".to_string()),
            (
                "BK_RISK_MAX_POSITION_SIZE_PAPER".to_string(),
                "7".to_string(),
            ),
            (
                "BK_RISK_INF_MAX_SINGLE_LOSS_USD".to_string(),
                "0.25".to_string(),
            ),
        ]);
        let s = SystemicRiskLimits::from_lookup_with_accounts(["paper", "inf"], |k| {
            vars.get(k).cloned()
        });
        assert_eq!(s.account.max_position_size.value(), dec!(100));
        assert_eq!(s.account_for("paper").max_position_size.value(), dec!(7));
        assert_eq!(s.account_for("inf").max_single_loss_usd.value(), dec!(0.25));
        // An account with no suffixed keys has no row: the global fallback
        // still applies (and a later global change remains visible).
        assert!(!s.accounts.contains_key("unconfigured"));
    }
}
