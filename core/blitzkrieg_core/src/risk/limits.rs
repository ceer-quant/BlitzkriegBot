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
//!    never shows a bare number. "35" without "toml" is not an answer an
//!    operator can act on — they would not know whether a restart, a flag, or
//!    a stale file produced it. So every limit is a [`Bound`]: value + source,
//!    in one type, all the way to the wire.

use rust_decimal::Decimal;
use serde::Serialize;

/// Where a limit's effective value came from (§4.4). The same four words the
/// kernel's startup provenance has always used ("every setting above its
/// default records its source"), now carried per-limit instead of per-boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitSource {
    /// The compiled-in default. For the nine new limits: 0 = off (§4.1).
    Default,
    /// A `user_layer/configs/*.toml` section.
    Toml,
    /// An environment variable.
    Env,
    /// A CLI flag — the strongest link of the existing chain.
    Flag,
}

impl LimitSource {
    /// The source recorded when a limit is only ever set from the config
    /// file's `[risk]` section (this Epic's only wired input; env/flag stay
    /// variants of the vocabulary so a later wiring does not change the wire).
    pub const FILE: LimitSource = LimitSource::Toml;

    /// The provenance word the boot log spells (`toml`, `env`, `flag`,
    /// `default`) — the same words the kernel's provenance report has always
    /// used, kept as a method here so the wire's vocabulary never has to
    /// reach into the config module for its spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::Default => "default",
            LimitSource::Toml => "toml",
            LimitSource::Env => "env",
            LimitSource::Flag => "flag",
        }
    }
}

/// The two vocabularies are the SAME four layers with two names for the
/// strongest one (the config layer says `cli`, the wire says `flag`). The map
/// is 1:1 by construction, so a resolved budget can name its true layer
/// without a second spelling table drifting apart.
impl From<crate::config::Source> for LimitSource {
    fn from(s: crate::config::Source) -> Self {
        match s {
            crate::config::Source::Cli => LimitSource::Flag,
            crate::config::Source::Env => LimitSource::Env,
            crate::config::Source::Toml => LimitSource::Toml,
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
}

/// Process-wide limits (§4.2). One per kernel; they bound the SUM of what all
/// accounts are doing, which no per-account view can see.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalRiskLimits {
    /// Concurrent open positions across ALL accounts (the existing per-account
    /// `max_positions` still applies; both active = the stricter wins).
    pub max_total_position: Bound,
    /// Total open notional across ALL accounts (USD).
    pub max_total_exposure_usd: Bound,
    /// Same-`asset` notional ceiling (USD). §4.2's honest definition: "same
    /// asset = same group" — per-asset exposure, max over assets. NO
    /// correlation matrix exists in 0.3 (§17).
    pub max_correlation_usd: Bound,
    /// All-accounts daily-loss kill switch (USD): the process-level stop that
    /// fires when the SUMMED day loss crosses the line.
    pub global_kill_switch_loss_usd: Bound,
}

/// The whole systemic-limit set a kernel runs under: the per-account matrix,
/// the process-wide matrix, and the resolution entry the config layer uses.
///
/// `Default` is the FACTORY posture (§4.1): every `Bound` off, every source
/// `default` — an unconfigured kernel answers `risk.limits` with the §4.4
/// example's shape and enforces nothing new.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemicRiskLimits {
    pub account: AccountRiskLimits,
    pub global: GlobalRiskLimits,
}

impl SystemicRiskLimits {
    /// Resolve the config-file face (all-optional `[risk]` keys) into the
    /// effective set. This Epic's only wired input is the TOML layer, so a
    /// present key is a `toml`-sourced `Bound` and an absent one stays the
    /// factory `Bound::off()`; `env`/`flag` remain members of the source
    /// vocabulary (§4.4) for a later wiring, not dead variants — the wire
    /// contract names all four.
    pub fn from_file(file: &crate::config::RiskFile) -> Self {
        let one = |v: Option<Decimal>| {
            v.map(|v| Bound::new(v, LimitSource::Toml))
                .unwrap_or_default()
        };
        Self {
            account: AccountRiskLimits {
                max_single_loss_usd: one(file.max_single_loss_usd),
                max_daily_drawdown_usd: one(file.max_daily_drawdown_usd),
                max_position_size: one(file.max_position_size),
                max_consecutive_losses: one(file.max_consecutive_losses),
                cooldown_minutes: one(file.cooldown_minutes),
            },
            global: GlobalRiskLimits {
                max_total_position: one(file.max_total_position),
                max_total_exposure_usd: one(file.max_total_exposure_usd),
                max_correlation_usd: one(file.max_correlation_usd),
                global_kill_switch_loss_usd: one(file.global_kill_switch_loss_usd),
            },
        }
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
        push("max_total_position", &self.global.max_total_position);
        push(
            "max_total_exposure_usd",
            &self.global.max_total_exposure_usd,
        );
        push("max_correlation_usd", &self.global.max_correlation_usd);
        push(
            "global_kill_switch_loss_usd",
            &self.global.global_kill_switch_loss_usd,
        );
        if out.is_empty() {
            out.push("risk.systemic: at factory (all nine limits off)".to_string());
        }
        out
    }

    /// True when ANY bound is armed (§4.2). The pipeline's factory gate: with
    /// nothing armed the systemic block produces no trace at all, so the
    /// shipped pipeline's output stays byte-identical (the audit's P1 parity
    /// is a byte gate, and an extra "nothing armed" trace would break it).
    pub fn any_armed(&self) -> bool {
        let a = &self.account;
        let g = &self.global;
        a.max_single_loss_usd.is_enabled()
            || a.max_daily_drawdown_usd.is_enabled()
            || a.max_position_size.is_enabled()
            || a.max_consecutive_losses.is_enabled()
            || a.cooldown_minutes.is_enabled()
            || g.max_total_position.is_enabled()
            || g.max_total_exposure_usd.is_enabled()
            || g.max_correlation_usd.is_enabled()
            || g.global_kill_switch_loss_usd.is_enabled()
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
pub fn loss_per_share(entry_price: Decimal, stop_pct: Decimal) -> Decimal {
    entry_price * stop_pct / Decimal::from(100)
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

/// The aggregates the systemic block judges (§4.2), read from the books the
/// kernel already maintains — carried per judgment so the verdict below stays
/// a pure function and the pipeline never re-derives a threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemicFacts {
    /// Open positions across ALL accounts (the global caps are process-wide).
    pub open_positions: usize,
    /// Sum of open cost basis, all accounts.
    pub open_exposure_usd: Decimal,
    /// Open cost basis in the incoming entry's OWN asset — the honest
    /// "correlation" proxy (§4.2): same asset = same settlement driver.
    pub same_asset_exposure_usd: Decimal,
    /// The day's realized PnL (the #173 budget's own number; negative when
    /// losing). The drawdown cap IS this budget (§4.2: no second counter) —
    /// it is enforced by the existing daily-loss breaker, never re-judged
    /// here; the kill-switch cap below reads the same fact.
    pub day_realized_usd: Decimal,
}

/// Which rejection vocabulary a systemic verdict speaks — mapped 1:1 onto the
/// arbitration pipeline's existing [`crate::arbitration::RejectReason`] arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemicReject {
    /// The account matrix refused / could not size the entry.
    AccountLimit,
    /// A process-wide cap refused the entry.
    GlobalLimit,
    /// The day's loss reached the kill-switch cap: stop opening new wounds.
    KillSwitch,
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

/// Judge one ENTRY against the systemic limits (§4.2). Order is deliberate:
/// the kill-switch cap first (a day at its loss ceiling must not open new
/// wounds), then the global caps (process-wide facts), then the account
/// sizing bounds (which SHRINK rather than refuse when they can). A factory
/// kernel has every bound off, so every suggestion passes — the shipped
/// behavior, structurally silent until `[risk]` speaks.
///
/// `bound_stop_pct` is the BOUND stop (`effective_stop_pct` at entry time) —
/// the same number the survival binding carries, so the single-loss cap is
/// computed against the discipline that will actually fire.
pub fn judge_entry(
    limits: &SystemicRiskLimits,
    facts: &SystemicFacts,
    entry_price: Decimal,
    entry_size: Decimal,
    bound_stop_pct: Decimal,
) -> EntryJudgment {
    // 1. Kill-switch cap: the day is lost — refuse new exposure. Closing is
    //    not this path's business (close intents never reach here).
    let ks = &limits.global.global_kill_switch_loss_usd;
    if ks.is_enabled() && facts.day_realized_usd <= -ks.value() {
        return EntryJudgment::Reject {
            reject: SystemicReject::KillSwitch,
            detail: format!(
                "global_kill_switch_loss_usd {}: day realized {} — new entries refused",
                ks.value(),
                facts.day_realized_usd
            ),
        };
    }
    // 2. Global position count: the table cannot grow past the cap.
    let count_cap = &limits.global.max_total_position;
    if count_cap.is_enabled() && Decimal::from(facts.open_positions + 1) > count_cap.value() {
        return EntryJudgment::Reject {
            reject: SystemicReject::GlobalLimit,
            detail: format!(
                "max_total_position {}: {} open + this entry exceeds the cap",
                count_cap.value(),
                facts.open_positions
            ),
        };
    }
    // 3. Global exposure: open commitment plus this entry's notional.
    let incoming = entry_price * entry_size;
    let exposure_cap = &limits.global.max_total_exposure_usd;
    if exposure_cap.is_enabled() && facts.open_exposure_usd + incoming > exposure_cap.value() {
        return EntryJudgment::Reject {
            reject: SystemicReject::GlobalLimit,
            detail: format!(
                "max_total_exposure_usd {}: {} open + {} incoming exceeds the cap",
                exposure_cap.value(),
                facts.open_exposure_usd,
                incoming
            ),
        };
    }
    // 4. Correlation: same-asset commitment plus this entry (§4.2's honest
    //    definition — no correlation matrix, one settlement driver per asset).
    let corr_cap = &limits.global.max_correlation_usd;
    if corr_cap.is_enabled() && facts.same_asset_exposure_usd + incoming > corr_cap.value() {
        return EntryJudgment::Reject {
            reject: SystemicReject::GlobalLimit,
            detail: format!(
                "max_correlation_usd {}: {} in this asset + {} incoming exceeds the cap",
                corr_cap.value(),
                facts.same_asset_exposure_usd,
                incoming
            ),
        };
    }
    // 5+6. Account sizing: the share cap and the single-loss cap both SHRINK
    //    the entry; the tighter survives and names itself. A cap that cannot
    //    fit even the reduced entry refuses (`缩不下`).
    let mut approved = entry_size;
    let mut binding: Option<&'static str> = None;
    let size_cap = &limits.account.max_position_size;
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
    let loss_cap = &limits.account.max_single_loss_usd;
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
        ] {
            assert!(!b.is_enabled(), "{name} must ship disabled");
            assert_eq!(b.source(), LimitSource::Default, "{name} provenance");
        }
        let g = GlobalRiskLimits::default();
        assert!(!g.max_total_exposure_usd.is_enabled());
        assert!(!g.global_kill_switch_loss_usd.is_enabled());
    }

    #[test]
    fn zero_is_off_and_positive_is_on() {
        assert!(!Bound::new(Decimal::ZERO, LimitSource::Toml).is_enabled());
        assert!(Bound::new(dec!(0.01), LimitSource::Toml).is_enabled());
        assert!(Bound::new(dec!(35), LimitSource::Flag).is_enabled());
    }

    #[test]
    fn bound_serializes_as_string_value_plus_source() {
        let doc = serde_json::to_value(Bound::new(dec!(35), LimitSource::Toml)).unwrap();
        assert_eq!(doc["value"], "35");
        assert_eq!(doc["source"], "toml");
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
        ] {
            assert!(doc.get(key).is_some(), "missing wire key {key}");
        }
        let doc = serde_json::to_value(GlobalRiskLimits::default()).unwrap();
        for key in [
            "maxTotalPosition",
            "maxTotalExposureUsd",
            "maxCorrelationUsd",
            "globalKillSwitchLossUsd",
        ] {
            assert!(doc.get(key).is_some(), "missing wire key {key}");
        }
    }

    #[test]
    fn report_lines_at_factory_are_the_single_silent_line() {
        let lines = SystemicRiskLimits::default().report_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("at factory"), "{}", lines[0]);
        assert!(lines[0].contains("all nine limits off"), "{}", lines[0]);
    }

    #[test]
    fn report_lines_name_only_the_armed_limits_with_their_source() {
        let mut sys = SystemicRiskLimits::default();
        sys.account.max_single_loss_usd = Bound::new(dec!(35), LimitSource::Toml);
        sys.global.global_kill_switch_loss_usd = Bound::new(dec!(1000), LimitSource::Flag);
        let lines = sys.report_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("max_single_loss_usd=35 (toml)"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("global_kill_switch_loss_usd=1000 (flag)"),
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
        let facts = SystemicFacts {
            open_positions: 999,
            open_exposure_usd: dec!(999_999),
            same_asset_exposure_usd: dec!(999_999),
            day_realized_usd: dec!(-999_999),
        };
        assert_eq!(
            judge_entry(
                &SystemicRiskLimits::default(),
                &facts,
                dec!(0.5),
                dec!(100),
                dec!(12)
            ),
            EntryJudgment::Pass
        );
    }

    #[test]
    fn kill_switch_cap_refuses_new_entries_when_the_day_is_lost() {
        let mut limits = SystemicRiskLimits::default();
        limits.global.global_kill_switch_loss_usd = Bound::new(dec!(50), LimitSource::Toml);
        let facts = SystemicFacts {
            day_realized_usd: dec!(-50),
            ..Default::default()
        };
        match judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::KillSwitch,
                detail,
            } => assert!(detail.contains("global_kill_switch_loss_usd"), "{detail}"),
            other => panic!("expected kill-switch rejection, got {other:?}"),
        }
        // Not yet AT the cap: the entry passes (the cap bounds, it does not
        // fire early).
        let facts = SystemicFacts {
            day_realized_usd: dec!(-49.99),
            ..Default::default()
        };
        assert_eq!(
            judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)),
            EntryJudgment::Pass
        );
    }

    #[test]
    fn total_position_cap_refuses_when_the_table_is_full() {
        let mut limits = SystemicRiskLimits::default();
        limits.global.max_total_position = Bound::new(dec!(2), LimitSource::Toml);
        let facts = SystemicFacts {
            open_positions: 2,
            ..Default::default()
        };
        match judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::GlobalLimit,
                detail,
            } => assert!(detail.contains("max_total_position"), "{detail}"),
            other => panic!("expected global rejection, got {other:?}"),
        }
        // One slot free: the entry takes it.
        let facts = SystemicFacts {
            open_positions: 1,
            ..Default::default()
        };
        assert_eq!(
            judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)),
            EntryJudgment::Pass
        );
    }

    #[test]
    fn total_exposure_cap_refuses_when_open_plus_incoming_over_commits() {
        let mut limits = SystemicRiskLimits::default();
        limits.global.max_total_exposure_usd = Bound::new(dec!(100), LimitSource::Toml);
        let facts = SystemicFacts {
            open_exposure_usd: dec!(95),
            ..Default::default()
        };
        // 95 + 10 = 105 > 100.
        match judge_entry(&limits, &facts, dec!(0.5), dec!(20), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::GlobalLimit,
                detail,
            } => assert!(detail.contains("max_total_exposure_usd"), "{detail}"),
            other => panic!("expected global rejection, got {other:?}"),
        }
        // 95 + 5 = 100 <= 100: exactly at the cap fits.
        assert_eq!(
            judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)),
            EntryJudgment::Pass
        );
    }

    #[test]
    fn correlation_cap_bounds_the_same_asset_commitment() {
        let mut limits = SystemicRiskLimits::default();
        limits.global.max_correlation_usd = Bound::new(dec!(20), LimitSource::Toml);
        let facts = SystemicFacts {
            same_asset_exposure_usd: dec!(18),
            ..Default::default()
        };
        // 18 + 5 = 23 > 20 (§4.2: same asset = one settlement driver).
        match judge_entry(&limits, &facts, dec!(0.5), dec!(10), dec!(12)) {
            EntryJudgment::Reject {
                reject: SystemicReject::GlobalLimit,
                detail,
            } => assert!(detail.contains("max_correlation_usd"), "{detail}"),
            other => panic!("expected global rejection, got {other:?}"),
        }
    }

    #[test]
    fn position_size_cap_shrinks_to_whole_shares() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(7.5), LimitSource::Toml);
        match judge_entry(
            &limits,
            &SystemicFacts::default(),
            dec!(0.5),
            dec!(100),
            dec!(12),
        ) {
            EntryJudgment::Reduce { approved, limit } => {
                assert_eq!(approved, dec!(7), "the cap floors to whole shares");
                assert_eq!(limit, "max_position_size");
            }
            other => panic!("expected a reduction, got {other:?}"),
        }
    }

    #[test]
    fn single_loss_cap_shrinks_and_names_itself_when_tighter_than_size_cap() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(100), LimitSource::Toml);
        // entry 0.50, stop 12% → per-share 0.06 → 583 shares fit the loss
        // cap; the size cap allows 100 → the loss cap does not bite.
        limits.account.max_single_loss_usd = Bound::new(dec!(35), LimitSource::Toml);
        assert_eq!(
            judge_entry(
                &limits,
                &SystemicFacts::default(),
                dec!(0.5),
                dec!(100),
                dec!(12)
            ),
            EntryJudgment::Pass
        );
        // A tighter loss cap bites: 6 × 0.06 = 0.36 <= 0.40 < 7 × 0.06.
        limits.account.max_single_loss_usd = Bound::new(dec!(0.40), LimitSource::Toml);
        match judge_entry(
            &limits,
            &SystemicFacts::default(),
            dec!(0.5),
            dec!(100),
            dec!(12),
        ) {
            EntryJudgment::Reduce { approved, limit } => {
                assert_eq!(approved, dec!(6));
                assert_eq!(limit, "max_single_loss_usd");
            }
            other => panic!("expected a reduction, got {other:?}"),
        }
    }

    #[test]
    fn a_loss_cap_nothing_fits_refuses_instead_of_guessing() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_single_loss_usd = Bound::new(dec!(0.01), LimitSource::Toml);
        // One share's worst case (0.06) alone over-commits the 0.01 cap.
        match judge_entry(
            &limits,
            &SystemicFacts::default(),
            dec!(0.5),
            dec!(100),
            dec!(12),
        ) {
            EntryJudgment::Reject {
                reject: SystemicReject::AccountLimit,
                detail,
            } => assert!(detail.contains("max_single_loss_usd"), "{detail}"),
            other => panic!("expected account rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_size_cap_below_one_share_refuses() {
        let mut limits = SystemicRiskLimits::default();
        limits.account.max_position_size = Bound::new(dec!(0.5), LimitSource::Toml);
        match judge_entry(
            &limits,
            &SystemicFacts::default(),
            dec!(0.5),
            dec!(100),
            dec!(12),
        ) {
            EntryJudgment::Reject {
                reject: SystemicReject::AccountLimit,
                detail,
            } => assert!(detail.contains("whole share"), "{detail}"),
            other => panic!("expected account rejection, got {other:?}"),
        }
    }
}
