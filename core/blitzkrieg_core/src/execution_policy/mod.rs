//! Account-isolated execution policy rule engine (issue #363).
//!
//! 策略只发信号，内核按 account_id 隔离的 execution_policy 决定是否下单、
//! 下多少钱。The policy is the OPERATOR's per-account expression of "how much
//! may this account commit per entry, and when must it sit a round out" — the
//! kernel keeps every structural risk control (hard stop-loss, the
//! consecutive-loss and daily-loss breakers, the risk gate) out of the rule
//! vocabulary entirely, so no rule combination can ever switch them off.
//!
//! # The config (`user_layer/configs/execution_policy.toml`)
//!
//! ```toml
//! version = 1
//!
//! [defaults]
//! budget_ratio = 0.10
//! min_budget_usd = 1.00
//! max_budget_usd = 50.00
//! min_equity_usd = 1.00
//! max_positions_per_asset = 1
//!
//! [[defaults.rules]]
//! name = "global_backstop"
//! enabled = true
//! priority = 100
//! when = "open_positions >= 2"
//! then = { action = "skip" }
//! reason = "全局兜底：持仓数达上限"
//! ```
//!
//! A rule's `then` block carries EXACTLY ONE action from a closed set:
//! `budget_ratio` / `min_budget_usd` / `max_budget_usd` (a sizing override),
//! `action = "skip"` (do not place this entry at all), or `cooldown_sec`
//! (the account places nothing for N seconds). Hard stop-loss / breakers /
//! kill switch are NOT in the enum — they are structurally untouchable,
//! enforced at compile time by the type, not by review.
//!
//! # Evaluation order (issue §)
//!
//! 1. Look up `[accounts.<id>]` by the order's account; a hit uses that
//!    account's own defaults + rules, a miss falls back to `[defaults]`.
//! 2. Sort the section's rules by `priority` ascending; the FIRST rule whose
//!    `when` predicate holds wins; no hit uses the section's defaults.
//! 3. An `account_id` the book does not know refuses the order outright —
//!    never a silent fall-through to defaults (fail-closed).
//!
//! # Safety locks
//!
//! - `budget_usd = clamp(max(available_balance × budget_ratio,
//!   min_budget_usd), ≤ max_budget_usd)` — the clamp runs inside
//!   [`evaluate`] itself, on the one path every budget takes, so no rule
//!   combination can lift the budget above `max_budget_usd` (structural,
//!   not a convention). A rule's own `max_budget_usd` override can only
//!   TIGHTEN the section ceiling, never widen it.
//! - The action enum has no stop-loss/breaker action (compile-level).
//! - `min_equity_usd` is a structural guard too: equity below it skips the
//!   entry before any rule is consulted.
//! - Out-of-range values (`budget_ratio > 1`, non-positive budgets,
//!   `version != 1`, unknown keys, missing fields) refuse the whole load:
//!   [`Policy::load`] is fail-closed and the caller refuses the boot.
//!
//! # Environment layering
//!
//! Per setting: per-account env > per-account TOML > global env > global
//! TOML > compiled default. The issue-751 single-source family is the
//! `POLICY_`-prefixed one — `POLICY_BUDGET_RATIO` / `POLICY_MIN_BUDGET_USD` /
//! `POLICY_MAX_BUDGET_USD` / `POLICY_MIN_EQUITY_USD` /
//! `POLICY_MAX_POSITIONS_PER_ASSET` / `POLICY_RULES` (the last a rules JSON
//! array string), with the account id uppercased and every non-alphanumeric
//! turned into `_`, e.g. `POLICY_BUDGET_RATIO_BINANCE_MAIN` for account
//! `binance-main`. The pre-751 bare names (`BUDGET_RATIO`, `MAX_BUDGET_USD`)
//! stay as read-only aliases. An ACCOUNT_LIST id carrying any `POLICY_<ID>_*
//!` key forms an env-only section even without a TOML row; the write face
//! (`execution_policy.set`/`.reset`) writes these keys into the `.env` and
//! refuses while the legacy toml file still exists. Env is read at load time
//! only (a restart — or an IPC write's reload — picks up changes), matching
//! the repo's restart-to-change knob discipline.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::Path;

mod rules;
// The split moved `PolicyInput` / `PolicyOutput` / `PolicyRefusal` / `evaluate`
// and the audit helpers into `part1`; they were public at this module's root
// before, so the re-export stays public — `tests/policy_env_oracle.rs` is an
// integration test and only sees this file's surface.
pub use rules::*;
#[allow(clippy::module_inception)]
mod tests;

/// The config file, resolved against the INSTALL ROOT (the executable's
/// directory and its ancestors) first, the process cwd and its ancestors
/// second — never the bare cwd (issue #380: a cwd-relative default made
/// "is the policy loaded?" depend on the launch directory, which silently
/// swapped a guarded kernel for the neutral one between two runs of the SAME
/// binary with the SAME arguments). The same lookup rule `default_strategy_dir`
/// uses in `main.rs`.
pub const EXECUTION_POLICY_CONFIG_PATH: &str = "user_layer/configs/execution_policy.toml";

/// Where a resolved DEFAULT path was found. The boot log names the winner so
/// "which base decided?" is answerable from the run log alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultPathSource {
    /// Found above the executable's directory — the install root (a release
    /// bundle, or a repo checkout running from `target/release/`).
    ExeRoot,
    /// Not beside the executable; found above the process cwd (the dev
    /// convenience run).
    Cwd,
    /// Found NOWHERE: the value is the exe-root-relative spelling, whose
    /// load will report NotFound — the #372 zero-config deployment (code
    /// defaults, no rules). The boot WARN names every directory searched.
    Fallback,
}

/// The default-policy resolution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDefaultPath {
    /// The path the loader will read.
    pub path: std::path::PathBuf,
    /// Which base won.
    pub source: DefaultPathSource,
    /// The directory the winning base resolved against (the bundle root, the
    /// repo root, …) — the boot log names it so "WHICH install root decided?"
    /// is answerable without re-deriving the ancestor walk. `None` only for
    /// [`DefaultPathSource::Fallback`], where no base won.
    pub base_dir: Option<std::path::PathBuf>,
    /// Every directory the lookup probed, in order — the WARN's "I looked
    /// here and here and found nothing" evidence.
    pub searched: Vec<std::path::PathBuf>,
}

/// What [`resolve_execution_policy_path`] settles for a configured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPolicyPath {
    /// The operator named a path (`--execution-policy <path>` / the config
    /// field): honoured VERBATIM, cwd-relative spellings included — an
    /// explicit path is a promise about which file to read, not a default to
    /// second-guess.
    Explicit(std::path::PathBuf),
    /// The shipped default: resolved per #380.
    Default(ResolvedDefaultPath),
}

/// Walk `start` upward through its ancestors. The chain INCLUDES `start`
/// itself: for a release bundle the exe sits at `<bundle>/bin/blitzkrieg-core`
/// and the configs land at `<bundle>/configs`, so the first probe from the
/// exe's own directory already covers the dev-checkout shape
/// (`target/release/` → the repo root two levels up).
fn ancestor_chain(start: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        out.push(dir.to_path_buf());
        cur = dir.parent();
    }
    out
}

/// Settle which file the policy loads from (issue #380). `Some(path)` = the
/// operator's explicit choice, honoured verbatim; `None` = the shipped
/// default, resolved against the install root first and the cwd second.
///
/// Default lookup order, first EXISTING-FILE hit wins:
///   1. `<each ancestor of exe_dir>/user_layer/configs/execution_policy.toml`
///      — the install root: a release bundle (`bin/` + `configs/`), a repo
///      checkout running from `target/release/`, or any launcher that keeps
///      the shipped layout beside the binary. This is what makes the policy
///      load identically no matter which directory the process started in.
///   2. `<each ancestor of cwd>/user_layer/configs/execution_policy.toml` —
///      the dev convenience (a core run from an unusual binary location but
///      the repo root still finds the shipped file). Ordered BELOW the
///      exe-root so a bundle's own config can never be shadowed by an
///      unrelated checkout the process happens to run from.
///   3. Nothing exists → the exe-root-relative spelling, source
///      [`DefaultPathSource::Fallback`]. The load then reports NotFound,
///      which is the #372 zero-config deployment: code defaults, no rules.
///      The boot WARN lists every directory that was searched, so "the
///      policy did not load" is visible instead of silent.
///
/// `None` inputs (current_exe/current_dir failed) simply shorten their
/// chain — a probe that cannot name a directory is skipped, not an error.
pub fn resolve_execution_policy_path(explicit: Option<&str>) -> ResolvedPolicyPath {
    let Some(path) = explicit else {
        return ResolvedPolicyPath::Default(resolve_default_config_path(
            std::env::current_exe()
                .ok()
                .as_deref()
                .and_then(Path::parent),
            std::env::current_dir().ok().as_deref(),
        ));
    };
    ResolvedPolicyPath::Explicit(std::path::PathBuf::from(path))
}

/// The default-path half of [`resolve_execution_policy_path`]. Kept separate
/// and parameterised so the PRIORITY ORDER is unit-testable without
/// mutating the process's exe path or cwd (tests inject directories).
pub fn resolve_default_config_path(
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> ResolvedDefaultPath {
    let relative = Path::new(EXECUTION_POLICY_CONFIG_PATH);
    let mut searched = Vec::new();
    if let Some(exe_dir) = exe_dir {
        for dir in ancestor_chain(exe_dir) {
            let candidate = dir.join(relative);
            searched.push(candidate.clone());
            if candidate.is_file() {
                return ResolvedDefaultPath {
                    path: candidate,
                    source: DefaultPathSource::ExeRoot,
                    base_dir: Some(dir),
                    searched,
                };
            }
        }
    }
    if let Some(cwd) = cwd {
        for dir in ancestor_chain(cwd) {
            let candidate = dir.join(relative);
            searched.push(candidate.clone());
            if candidate.is_file() {
                return ResolvedDefaultPath {
                    path: candidate,
                    source: DefaultPathSource::Cwd,
                    base_dir: Some(dir),
                    searched,
                };
            }
        }
    }
    let fallback = exe_dir
        .map(|d| d.join(relative))
        .unwrap_or_else(|| relative.to_path_buf());
    ResolvedDefaultPath {
        path: fallback,
        source: DefaultPathSource::Fallback,
        base_dir: None,
        searched,
    }
}

/// Where the policy-change audit trail lands. Runtime data beside the other
/// audit journals (`data/` is git-ignored by charter).
pub const EXECUTION_POLICY_AUDIT_PATH: &str = "data/audit/execution_policy.jsonl";

/// The only config version this parser accepts. A different number is a
/// rewrite this code has not seen — refuse rather than guess (fail-closed).
pub const POLICY_VERSION: u32 = 1;

// ── Wire types ──────────────────────────────────────────────────────────────

/// TOML/JSON → Decimal with the ONE precision rule the repo's own
/// `config.rs` uses for file decimals: floats go through [`Decimal::from_f64`]
/// (the shortest round-tripping decimal — `0.10` arrives as `0.1`, not the
/// binary expansion), integers exactly, strings via `from_str_exact`. The
/// default serde path for `Decimal` keeps f64 excess precision, which would
/// make `0.10` in a file unequal to `dec!(0.10)` in code — this module is
/// why that cannot happen.
pub mod wire_dec {
    use rust_decimal::Decimal;
    use rust_decimal::prelude::FromPrimitive as _;
    use serde::{Deserializer, Serializer};

    pub fn deserialize<'de, D>(d: D) -> Result<Decimal, D::Error>
    where
        D: Deserializer<'de>,
    {
        d.deserialize_any(DecVisitor)
    }

    pub fn serialize<S>(d: &Decimal, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&d.to_string())
    }

    pub mod opt {
        use rust_decimal::Decimal;
        use serde::de::Error as _;
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn deserialize<'de, D>(d: D) -> Result<Option<Decimal>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let v = Option::<serde_json::Value>::deserialize(d)?;
            match v {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(s)) => Decimal::from_str_exact(&s)
                    .map(Some)
                    .map_err(|e| D::Error::custom(e.to_string())),
                Some(serde_json::Value::Number(n)) => super::dec_from_json_number(&n)
                    .map(Some)
                    .ok_or_else(|| D::Error::custom("value is not a finite decimal")),
                Some(other) => Err(D::Error::custom(format!(
                    "expected number/string, got {other}"
                ))),
            }
        }

        pub fn serialize<S>(d: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match d {
                None => s.serialize_none(),
                Some(v) => super::serialize(v, s),
            }
        }
    }

    struct DecVisitor;

    impl serde::de::Visitor<'_> for DecVisitor {
        type Value = Decimal;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a decimal (number or string)")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Decimal, E> {
            Decimal::from_str_exact(v.trim())
                .map_err(|e| E::custom(format!("`{v}` is not a decimal: {e}")))
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Decimal, E> {
            Decimal::from_f64(v).ok_or_else(|| E::custom(format!("{v} is not a finite decimal")))
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Decimal, E> {
            Ok(Decimal::from(v))
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Decimal, E> {
            Ok(Decimal::from(v))
        }
    }

    /// One JSON number → Decimal, same shortest-decimal rule.
    pub(crate) fn dec_from_json_number(n: &serde_json::Number) -> Option<Decimal> {
        if let Some(i) = n.as_i64() {
            return Some(Decimal::from(i));
        }
        if let Some(u) = n.as_u64() {
            return Some(Decimal::from(u));
        }
        n.as_f64().and_then(Decimal::from_f64)
    }
}

/// A `when` condition: exactly one field, one operator, one value. The
/// grammar is deliberately NOT a scripting language — a struct with eight
/// fields cannot express anything the kernel did not already hand over, and
/// parsing it needs no evaluator security story.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct When {
    pub field: ConditionField,
    pub op: CompareOp,
    pub value: ConditionValue,
}

impl<'de> Deserialize<'de> for When {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Two spellings, one meaning: the string form ("open_positions >= 2",
        // readable aloud, the form the issue's default file uses) and the
        // table form ({ field, op, value }). The string form goes through
        // [`parse_when`], so both spellings share one grammar and one
        // validation — a rule can never be valid in one form and refused in
        // the other.
        let raw = serde_json::Value::deserialize(d)?;
        match raw {
            serde_json::Value::String(s) => parse_when(&s).map_err(serde::de::Error::custom),
            other => {
                let fields = TableWhen::deserialize(other).map_err(serde::de::Error::custom)?;
                Ok(When {
                    field: fields.field,
                    op: fields.op,
                    value: fields.value,
                })
            }
        }
    }
}

/// The table spelling of a `when` (an intermediate for the deserializer).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct TableWhen {
    field: ConditionField,
    #[serde(with = "op_symbol")]
    op: CompareOp,
    value: ConditionValue,
}

/// The eight condition fields an order can be judged on (the issue's list,
/// verbatim). Kept an enum rather than free strings so a typo is a parse
/// refusal, not a silently-always-false predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionField {
    AvailableBalance,
    TotalEquity,
    OpenPositions,
    CurrentPrice,
    TimeLeftSec,
    Symbol,
    RecentPnl1h,
    ConsecutiveLosses,
}

impl ConditionField {
    /// The numeric value this field reads out of a [`PolicyInput`]. `symbol`
    /// is a string field and answers `None`.
    fn numeric_value(self, input: &PolicyInput) -> Option<Decimal> {
        Some(match self {
            Self::AvailableBalance => input.available_balance,
            Self::TotalEquity => input.total_equity,
            Self::OpenPositions => Decimal::from(input.open_positions),
            Self::CurrentPrice => input.current_price,
            Self::TimeLeftSec => Decimal::from(input.time_left_sec),
            Self::RecentPnl1h => input.recent_pnl_1h,
            Self::ConsecutiveLosses => Decimal::from(input.consecutive_losses),
            Self::Symbol => return None,
        })
    }
}

/// The comparison operators (`< <= > >= == != in`). `in` is membership in a
/// symbol list and is the ONLY list form — there is no general collection
/// expression, which is what keeps the grammar tiny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    In,
}

impl CompareOp {
    /// The operator's symbol spelling as it reads in a `when` string.
    fn symbol(self) -> &'static str {
        match self {
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::In => "in",
        }
    }
}

/// Serde shim letting a table-form `op` carry the SYMBOL spelling
/// (`op = ">="`) as well as the snake_case one (`op = "ge"`): the two
/// spellings must not diverge into "one parses, one refuses".
pub mod op_symbol {
    use super::CompareOp;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn deserialize<'de, D>(d: D) -> Result<CompareOp, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "<" => Ok(CompareOp::Lt),
            "<=" => Ok(CompareOp::Le),
            ">" => Ok(CompareOp::Gt),
            ">=" => Ok(CompareOp::Ge),
            "==" | "=" => Ok(CompareOp::Eq),
            "!=" => Ok(CompareOp::Ne),
            "in" => Ok(CompareOp::In),
            // Fall back to the snake_case spellings.
            other => {
                serde_json::from_value::<CompareOp>(serde_json::Value::String(other.to_string()))
                    .map_err(serde::de::Error::custom)
            }
        }
    }

    pub fn serialize<S>(op: &CompareOp, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(op.symbol())
    }
}

/// The right-hand side of a `when`. `symbol` conditions compare against a
/// string (or a list via `in`); every other field compares against a number.
/// Manual `Deserialize` (rather than `untagged`) so numbers land as clean
/// shortest decimals via [`wire_dec`]'s rule.
///
/// `Serialize` is ALSO manual: the derived form would emit the externally
/// tagged map `{"Text": "BTC"}` — a shape the deserializer above cannot read
/// back, so a `set` carrying rules would write a file the kernel refuses at
/// the next reload. Plain values (`"BTC"`, `2`, `0.4`, `["BTC","ETH"]`) are
/// what both the TOML file and the JSON wire share, and they are exactly
/// what [`parse_condition_value`] accepts in the string grammar.
#[derive(Debug, Clone, PartialEq)]
pub enum ConditionValue {
    Num(Decimal),
    Text(String),
    TextList(Vec<String>),
}

impl Serialize for ConditionValue {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use rust_decimal::prelude::ToPrimitive as _;
        use std::str::FromStr as _;
        match self {
            Self::Text(v) => s.serialize_str(v),
            Self::TextList(items) => s.collect_seq(items),
            Self::Num(d) => {
                // Integral values stay integers (TOML `value = 2`, JSON 2);
                // everything else rides f64 through the shortest-decimal
                // discipline the deserializer's visit_f64 already reads
                // back. A value neither path can carry refuses the render —
                // the set is refused before any file is touched
                // (fail-closed).
                if d.scale() == 0
                    && let Some(i) = d.to_i64()
                {
                    return s.serialize_i64(i);
                }
                match f64::from_str(&d.to_string()) {
                    Ok(f) if f.is_finite() => s.serialize_f64(f),
                    _ => Err(serde::ser::Error::custom(format!(
                        "condition value `{d}` cannot be written as a plain number"
                    ))),
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ConditionValue {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ConditionValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a number, a symbol string, or a list of symbols")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ConditionValue::Text(v.to_string()))
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    out.push(s);
                }
                Ok(ConditionValue::TextList(out))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(ConditionValue::Num(Decimal::from(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(ConditionValue::Num(Decimal::from(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                rust_decimal::prelude::FromPrimitive::from_f64(v)
                    .map(ConditionValue::Num)
                    .ok_or_else(|| E::custom(format!("{v} is not a finite decimal")))
            }
        }
        d.deserialize_any(V)
    }
}

/// The `then` block: EXACTLY ONE action from the closed set. This enum is
/// the compile-level safety lock — hard stop-loss, the consecutive-loss
/// breaker and the daily-loss breaker are not variants, so a rule CANNOT
/// name them, and `deny_unknown_fields` refuses any key that is not one of
/// the five below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Action {
    /// `action = "skip"` — refuse this entry outright. Absent = the rule is
    /// a sizing override, not a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<SkipAction>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub budget_ratio: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub min_budget_usd: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub max_budget_usd: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_sec: Option<i64>,
}

/// The refusal spelling. Its only value is `"skip"`, so the string
/// `action = "skip"` in TOML maps onto `Some(SkipAction::Skip)` and anything
/// else fails the parse — the "禁用硬止损 → 拒绝" lock lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipAction {
    Skip,
}

impl Action {
    /// Exactly one action per `then`: mixing two sizing overrides, or a skip
    /// alongside a sizing override, is an ambiguous rule — refuse it.
    fn validate(&self, rule_name: &str) -> Result<(), String> {
        let set = [
            self.action.is_some(),
            self.budget_ratio.is_some(),
            self.min_budget_usd.is_some(),
            self.max_budget_usd.is_some(),
            self.cooldown_sec.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if set != 1 {
            return Err(format!(
                "rule `{rule_name}`: then-block must carry exactly one action \
                 (action=\"skip\" | budget_ratio | min_budget_usd | max_budget_usd | \
                 cooldown_sec), found {set}"
            ));
        }
        Ok(())
    }
}

/// One rule. `priority` sorts ascending and the first true `when` wins;
/// default 100 so hand-written rules with no priority sit behind an
/// operator's numbered tiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub enabled: bool,
    pub when: When,
    pub then: Action,
    /// Human-readable note for the audit trail (free text, never parsed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_priority() -> u32 {
    100
}

/// One account section (or the global `[defaults]`): the budget triple, the
/// equity floor, the per-asset position cap and this section's rules. Every
/// field optional — an absent field inherits from the layer below.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SectionFields {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub budget_ratio: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub min_budget_usd: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub max_budget_usd: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub min_equity_usd: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_positions_per_asset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
}

/// The file-level defaults (same shape as an account section).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DefaultsSection {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub budget_ratio: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub min_budget_usd: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub max_budget_usd: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "wire_dec::opt"
    )]
    pub min_equity_usd: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_positions_per_asset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PolicyFile {
    pub version: u32,
    #[serde(default)]
    pub defaults: DefaultsSection,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub accounts: HashMap<String, SectionFields>,
}

impl Default for PolicyFile {
    fn default() -> Self {
        Self {
            version: POLICY_VERSION,
            defaults: DefaultsSection::default(),
            accounts: HashMap::new(),
        }
    }
}

// ── Resolved config ─────────────────────────────────────────────────────────

/// The compiled defaults after env layering. The shipped values replicate
/// the pre-policy behaviour bit for bit.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionDefaults {
    pub budget_ratio: Decimal,
    pub min_budget_usd: Decimal,
    pub max_budget_usd: Decimal,
    pub min_equity_usd: Decimal,
    pub max_positions_per_asset: u32,
    pub rules: Vec<Rule>,
}

/// The compiled defaults: the NEUTRAL kernel. The engine's own sizing
/// (share band, absolute or equity-relative budget) decides the ticket, and
/// a policy budget of the FULL balance can never shrink it — so a
/// deployment without a config file (or without an account section) keeps
/// its historical behaviour BIT FOR BIT, which is the exit-economics
/// four-window replay's contract and the two #202 sizing tests' acceptance.
/// The 10%/$1/$50 the issue names as today's numbers — and the
/// `open_positions >= 2 → skip` backstop — are the SHIPPED FILE's
/// `[defaults]`, not compiled constants — operators edit them there.
/// Defaults MUST NOT drift: the replay depends on it.
pub fn code_defaults() -> SectionDefaults {
    SectionDefaults {
        budget_ratio: Decimal::ONE,
        min_budget_usd: Decimal::ZERO,
        max_budget_usd: dec!(1_000_000_000),
        min_equity_usd: Decimal::ZERO,
        max_positions_per_asset: 1,
        // NO rules. The backstop (`open_positions >= 2 → skip`) is the
        // SHIPPED FILE's rule, not a compiled one: a zero-config deployment
        // must be the neutral kernel in FULL — no skips, no cooldowns — or
        // the bit-for-bit contract above is a lie and every no-file replay
        // silently trades under a rule the operator never wrote (it skewed
        // the #355 almach restore replay: 11,864 silent skips on the 5m
        // stream).
        rules: Vec::new(),
    }
}

impl SectionDefaults {
    /// Validate the value ranges. Called on every resolved section (both the
    /// file's and the code's), so an out-of-range value fails the load no
    /// matter which layer supplied it (fail-closed).
    fn validate(&self, label: &str) -> Result<(), String> {
        if self.budget_ratio <= Decimal::ZERO || self.budget_ratio > Decimal::ONE {
            return Err(format!(
                "{label}: budget_ratio {} outside (0, 1]",
                self.budget_ratio
            ));
        }
        if self.min_budget_usd < Decimal::ZERO {
            return Err(format!(
                "{label}: min_budget_usd {} must be >= 0",
                self.min_budget_usd
            ));
        }
        if self.max_budget_usd <= Decimal::ZERO {
            return Err(format!(
                "{label}: max_budget_usd {} must be > 0",
                self.max_budget_usd
            ));
        }
        if self.max_budget_usd < self.min_budget_usd {
            return Err(format!(
                "{label}: max_budget_usd {} < min_budget_usd {} — the ceiling must not \
                 sit below the floor",
                self.max_budget_usd, self.min_budget_usd
            ));
        }
        if self.min_equity_usd < Decimal::ZERO {
            return Err(format!(
                "{label}: min_equity_usd {} must be >= 0",
                self.min_equity_usd
            ));
        }
        if self.max_positions_per_asset == 0 {
            return Err(format!(
                "{label}: max_positions_per_asset must be >= 1 (0 would refuse every \
                 entry)"
            ));
        }
        for r in &self.rules {
            validate_when(&r.when).map_err(|e| format!("{label}: rule `{}`: {e}", r.name))?;
        }
        Ok(())
    }
}

/// Field/value type agreement for a `when` — enforced at load time so a
/// mismatched pair is a refusal, not a predicate that silently never holds.
fn validate_when(w: &When) -> Result<(), String> {
    let numeric_field = w.field.numeric_value(&PolicyInput::ZERO).is_some();
    match (&w.value, numeric_field) {
        (ConditionValue::Num(_), true) | (ConditionValue::Text(_), false) => Ok(()),
        (ConditionValue::TextList(_), false) => Ok(()),
        (ConditionValue::Num(_), false) => Err(format!(
            "field `{}` compares against text, not a number",
            w.field.field_name()
        )),
        (_, true) => Err(format!(
            "field `{}` is numeric and cannot compare against text",
            w.field.field_name()
        )),
    }
}

impl ConditionField {
    fn field_name(self) -> &'static str {
        match self {
            Self::AvailableBalance => "available_balance",
            Self::TotalEquity => "total_equity",
            Self::OpenPositions => "open_positions",
            Self::CurrentPrice => "current_price",
            Self::TimeLeftSec => "time_left_sec",
            Self::Symbol => "symbol",
            Self::RecentPnl1h => "recent_pnl_1h",
            Self::ConsecutiveLosses => "consecutive_losses",
        }
    }
}

/// The whole loaded policy: resolved defaults + the per-account sections.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Layered defaults (code < global TOML < global env).
    pub defaults: SectionDefaults,
    /// Fully-resolved per-account sections (globals < account TOML < account
    /// env), keyed by the account id exactly as the file spells it.
    pub accounts: HashMap<String, SectionDefaults>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            defaults: code_defaults(),
            accounts: HashMap::new(),
        }
    }
}

impl Policy {
    /// Load + parse + validate the config at `path`. A MISSING file is the
    /// zero-config deployment (code defaults, no account sections) — with the
    /// #751 env layer still folded on top (POLICY_* keys in the process
    /// environment are the whole surface once the toml retires); a file that
    /// exists but does not parse, misses a field, or carries an out-of-range
    /// value REFUSES (fail-closed) — the caller refuses the boot rather than
    /// trading under a policy the operator did not write.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // A version-1 empty file: the typed empty PolicyFile the env-only
            // boot layers on top of (the bare empty string would parse as
            // version 0 and refuse).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::load_from_str("version = 1\n");
            }
            Err(e) => return Err(format!("{} unreadable: {e}", path.display())),
        };
        Self::load_from_str(&text)
    }

    /// Parse the TOML text with the PROCESS environment as the env layer.
    pub fn load_from_str(text: &str) -> Result<Self, String> {
        Self::load_with_env(text, |key| {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        })
    }

    /// The single-source load (issue 751): same toml-then-env layering, but
    /// the env reader ALSO sees the `.env` file the launcher owns — the write
    /// face lands POLICY_* keys there, and a same-process reload must observe
    /// them before the next launcher restart puts them into the process
    /// environment. Process environment first (12-factor), file lines second.
    pub fn load_single_source(path: &Path, env_path: Option<&Path>) -> Result<Self, String> {
        let file_map: std::collections::HashMap<String, String> = env_path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(crate::env_file::parse_line)
                    .collect()
            })
            .unwrap_or_default();
        let env = move |key: &str| -> Option<String> {
            std::env::var(key)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| file_map.get(key).cloned())
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "version = 1\n".to_string(),
            Err(e) => return Err(format!("{} unreadable: {e}", path.display())),
        };
        Self::load_with_env(&text, env)
    }

    /// Same parser as boot, but an explicit reload resolves the writable
    /// `.env` file first instead of the startup process-environment snapshot.
    ///
    /// `written` is the in-memory snapshot of the keys the IPC write face has
    /// made authoritative (`Some` = the value written, `None` = the key was
    /// swept by a reset / an absent section field). It lets a same-process
    /// reload see a just-written tightening before the next launcher restart
    /// exports the file into the process env — and, for a key the write face
    /// REMOVED, stops the stale boot-time export from resurrecting it (#757:
    /// `set` wrote 5, the file said 5, but the effective policy still read the
    /// 100 the launcher had exported; `reset` was shadowed the same way).
    ///
    /// Precedence: file line > IPC write snapshot > process environment. The
    /// file stays first so an operator editing `.env` by hand still wins;
    /// keys that only ever arrived from the deployment env are absent from
    /// both the file and `written`, so they keep resolving exactly as at boot.
    pub fn load_for_reload(
        path: &Path,
        env_path: &Path,
        written: &std::collections::BTreeMap<String, Option<String>>,
    ) -> Result<Self, String> {
        Self::load_for_reload_with_env(path, env_path, written, |key| {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        })
    }

    /// The test seam for [`Self::load_for_reload`]: the process-environment
    /// reader is injected so the precedence can be proven without mutating
    /// global state (parallel tests race on `std::env`).
    fn load_for_reload_with_env(
        path: &Path,
        env_path: &Path,
        written: &std::collections::BTreeMap<String, Option<String>>,
        process_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let file_map: std::collections::HashMap<String, String> = std::fs::read_to_string(env_path)
            .map_err(|e| format!("{} unreadable: {e}", env_path.display()))?
            .lines()
            .filter_map(crate::env_file::parse_line)
            .collect();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "version = 1\n".to_string(),
            Err(e) => return Err(format!("{} unreadable: {e}", path.display())),
        };
        let env = |key: &str| -> Option<String> {
            if let Some(v) = file_map.get(key) {
                return Some(v.clone());
            }
            // An IPC write owns its keys even when the file no longer
            // carries them: `None` is a reset tombstone, and the stale
            // process value must NOT come back through the fallback.
            if let Some(v) = written.get(key) {
                return v.clone();
            }
            process_env(key)
        };
        Self::load_with_env(&text, env)
    }

    /// Parse with an injected env reader — the test seam (mutating the
    /// process environment to test precedence would be global state that
    /// parallel tests race on, the same reason `main.rs` has `EnvVars`).
    pub fn load_with_env(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let file: PolicyFile = toml::from_str(text)
            .map_err(|e| format!("execution_policy.toml does not parse: {e}"))?;
        if file.version != POLICY_VERSION {
            return Err(format!(
                "execution_policy.toml: version {} unsupported (this kernel speaks {})",
                file.version, POLICY_VERSION
            ));
        }
        // Global layer, precedence ascending: code defaults < global TOML <
        // global env (§: 全局环境变量 > 全局 TOML > 代码默认).
        let mut defaults = code_defaults();
        layer_section(&mut defaults, &file.defaults, "[defaults]")?;
        apply_env(&mut defaults, None, &env)?;
        defaults.validate("[defaults]")?;

        // Per-account sections: globals < account TOML < account env. The
        // map key is the account id; the env suffix uppercases it and maps
        // every non-alphanumeric to `_` (§: POLICY_BUDGET_RATIO_<ACCOUNT_ID
        // 大写非字母数字转下划线>).
        let mut accounts = HashMap::new();
        for (id, section) in &file.accounts {
            let label = format!("[accounts.{id}]");
            let mut resolved = defaults.clone();
            layer_section(&mut resolved, section, &label)?;
            apply_env(&mut resolved, Some(id), &env)?;
            resolved.validate(&label)?;
            accounts.insert(id.clone(), resolved);
        }
        // Env-only account sections (issue 751): an ACCOUNT_LIST id carrying
        // POLICY_<ID>_* keys but no TOML row still forms a section (defaults
        // < env), so the per-account surface survives the toml's retirement.
        // Accounts unknown to the toml AND carrying no keys are skipped —
        // no section, judge under the globals (the established semantics).
        if let Some(list) = env("ACCOUNT_LIST") {
            for id in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if accounts.contains_key(id) || !env_has_policy_section(&env, id) {
                    continue;
                }
                let label = format!("[accounts.{id}] (env)");
                let mut resolved = defaults.clone();
                apply_env(&mut resolved, Some(id), &env)?;
                resolved.validate(&label)?;
                accounts.insert(id.to_string(), resolved);
            }
        }
        Ok(Self { defaults, accounts })
    }

    /// The section an order on `account_id` is judged under. The caller has
    /// already refused unknown accounts (fail-closed), so the fall-through
    /// here is the CONFIGURED no-account-section case, not a silent rescue.
    pub fn section_for(&self, account_id: &str) -> &SectionDefaults {
        self.accounts.get(account_id).unwrap_or(&self.defaults)
    }

    /// The per-asset position cap for one account.
    pub fn max_positions_per_asset(&self, account_id: &str) -> u32 {
        self.section_for(account_id).max_positions_per_asset
    }
}

/// Fold one file section over the running layer. `None` fields inherit;
/// present fields replace; `Some(rules)` REPLACES the inherited list (the
/// section owns its rules entirely — inheriting + appending would make "what
/// rules apply to account A" unreadable).
fn layer_section(
    into: &mut SectionDefaults,
    from: &impl SectionLike,
    label: &str,
) -> Result<(), String> {
    if let Some(v) = from.budget_ratio() {
        into.budget_ratio = v;
    }
    if let Some(v) = from.min_budget_usd() {
        into.min_budget_usd = v;
    }
    if let Some(v) = from.max_budget_usd() {
        into.max_budget_usd = v;
    }
    if let Some(v) = from.min_equity_usd() {
        into.min_equity_usd = v;
    }
    if let Some(v) = from.max_positions_per_asset() {
        into.max_positions_per_asset = v;
    }
    if let Some(rules) = from.rules() {
        let mut out = Vec::with_capacity(rules.len());
        for r in rules {
            r.then
                .validate(&r.name)
                .map_err(|e| format!("{label}: {e}"))?;
            out.push(r.clone());
        }
        into.rules = out;
    }
    Ok(())
}

/// The field set shared by `[defaults]` and `[accounts.<id>]` so one layering
/// helper covers both shapes.
trait SectionLike {
    fn budget_ratio(&self) -> Option<Decimal>;
    fn min_budget_usd(&self) -> Option<Decimal>;
    fn max_budget_usd(&self) -> Option<Decimal>;
    fn min_equity_usd(&self) -> Option<Decimal>;
    fn max_positions_per_asset(&self) -> Option<u32>;
    fn rules(&self) -> Option<&Vec<Rule>>;
}

macro_rules! impl_section_like {
    ($t:ty) => {
        impl SectionLike for $t {
            fn budget_ratio(&self) -> Option<Decimal> {
                self.budget_ratio
            }
            fn min_budget_usd(&self) -> Option<Decimal> {
                self.min_budget_usd
            }
            fn max_budget_usd(&self) -> Option<Decimal> {
                self.max_budget_usd
            }
            fn min_equity_usd(&self) -> Option<Decimal> {
                self.min_equity_usd
            }
            fn max_positions_per_asset(&self) -> Option<u32> {
                self.max_positions_per_asset
            }
            fn rules(&self) -> Option<&Vec<Rule>> {
                self.rules.as_ref()
            }
        }
    };
}

impl_section_like!(DefaultsSection);
impl_section_like!(SectionFields);

/// Read the env layer for one section. `account_id: None` = the global names
/// (`POLICY_BUDGET_RATIO`, `POLICY_MIN_BUDGET_USD`, …); `Some(id)` = the
/// per-account names (`POLICY_BUDGET_RATIO_<ID>`, …). Applied AFTER the TOML
/// layer so env outranks it. The two pre-#751 bare names (`BUDGET_RATIO`,
/// `MAX_BUDGET_USD`) stay as compatibility ALIASES read only when the
/// POLICY_-prefixed key is absent — the write face only ever writes the
/// prefixed family.
fn apply_env(
    section: &mut SectionDefaults,
    account_id: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let suffix = account_id
        .map(|id| format!("_{}", env_account_suffix(id)))
        .unwrap_or_default();
    // Prefixed family first; the pre-#751 bare names stay as read-only
    // aliases when the prefixed key is absent.
    let ratio_raw = env(&format!("POLICY_BUDGET_RATIO{suffix}"))
        .or_else(|| env(&format!("BUDGET_RATIO{suffix}")));
    if let Some(raw) = ratio_raw {
        section.budget_ratio = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_BUDGET_RATIO{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MIN_BUDGET_USD{suffix}")) {
        section.min_budget_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MIN_BUDGET_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    let max_raw = env(&format!("POLICY_MAX_BUDGET_USD{suffix}"))
        .or_else(|| env(&format!("MAX_BUDGET_USD{suffix}")));
    if let Some(raw) = max_raw {
        section.max_budget_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MAX_BUDGET_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MIN_EQUITY_USD{suffix}")) {
        section.min_equity_usd = Decimal::from_str_exact(raw.trim()).map_err(|_| {
            format!("env POLICY_MIN_EQUITY_USD{suffix}={raw:?} does not parse as a decimal")
        })?;
    }
    if let Some(raw) = env(&format!("POLICY_MAX_POSITIONS_PER_ASSET{suffix}")) {
        let v: u32 = raw.trim().parse().map_err(|_| {
            format!("env POLICY_MAX_POSITIONS_PER_ASSET{suffix}={raw:?} does not parse as u32")
        })?;
        section.max_positions_per_asset = v;
    }
    if let Some(raw) = env(&format!("POLICY_RULES{suffix}")) {
        let rules: Vec<Rule> = serde_json::from_str(raw.trim()).map_err(|e| {
            format!("env POLICY_RULES{suffix} does not parse as a rules JSON array: {e}")
        })?;
        for r in &rules {
            r.then
                .validate(&r.name)
                .map_err(|e| format!("env POLICY_RULES{suffix}: {e}"))?;
        }
        section.rules = rules;
    }
    Ok(())
}

/// The `.env` footprint of ONE account section under the issue-751 spelling:
/// every key the section owns, mapped to the value a section write gives it
/// (`Some` = written, `None` = swept/absent).
///
/// This is the write-side twin of [`apply_env`]. The IPC writer renders the
/// `Some` values into the file; the whole map is ALSO kept in the in-memory
/// snapshot a same-process reload consults (#757), so a stale deployment
/// value cannot shadow the write, and a `None` tombstone stops a `reset`
/// from resurrecting the value the process env still carries.
pub fn section_env_state(
    account_id: &str,
    section: Option<&SectionFields>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    let sfx = env_account_suffix(account_id);
    let key = |tail: &str| format!("POLICY_{tail}_{sfx}");
    // The six keys are the section's whole footprint — absent fields are
    // swept, exactly the TOML section-replacement semantics.
    let mut state: std::collections::BTreeMap<String, Option<String>> = [
        "BUDGET_RATIO",
        "MIN_BUDGET_USD",
        "MAX_BUDGET_USD",
        "MIN_EQUITY_USD",
        "MAX_POSITIONS_PER_ASSET",
        "RULES",
    ]
    .into_iter()
    .map(|tail| (key(tail), None))
    .collect();
    let Some(s) = section else {
        return Ok(state);
    };
    if let Some(v) = s.budget_ratio {
        state.insert(key("BUDGET_RATIO"), Some(v.to_string()));
    }
    if let Some(v) = s.min_budget_usd {
        state.insert(key("MIN_BUDGET_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.max_budget_usd {
        state.insert(key("MAX_BUDGET_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.min_equity_usd {
        state.insert(key("MIN_EQUITY_USD"), Some(v.to_string()));
    }
    if let Some(v) = s.max_positions_per_asset {
        state.insert(key("MAX_POSITIONS_PER_ASSET"), Some(v.to_string()));
    }
    if let Some(rules) = &s.rules {
        let json = serde_json::to_string(rules)
            .map_err(|e| format!("rules do not render as JSON: {e}"))?;
        state.insert(key("RULES"), Some(json));
    }
    Ok(state)
}

/// Whether ANY per-account POLICY_* key names this id — the "this account has
/// an env section" probe that registers env-only sections for accounts the
/// TOML has no `[accounts.<id>]` row for (issue 751: after the toml retires,
/// ACCOUNT_LIST + these keys ARE the per-account policy surface).
fn env_has_policy_section(env: &impl Fn(&str) -> Option<String>, id: &str) -> bool {
    let sfx = env_account_suffix(id);
    [
        format!("POLICY_BUDGET_RATIO_{sfx}"),
        format!("POLICY_MIN_BUDGET_USD_{sfx}"),
        format!("POLICY_MAX_BUDGET_USD_{sfx}"),
        format!("POLICY_MIN_EQUITY_USD_{sfx}"),
        format!("POLICY_MAX_POSITIONS_PER_ASSET_{sfx}"),
        format!("POLICY_RULES_{sfx}"),
    ]
    .iter()
    .any(|k| env(k).is_some())
}

/// The env suffix for an account id: uppercased, non-alphanumerics → `_`.
fn env_account_suffix(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_ascii_uppercase()
}

// ── The `when` string form ──────────────────────────────────────────────────

/// Parse a `when` written as a string (`"open_positions >= 2"`). The FILE
/// carries the table form (`{ field = "open_positions", op = ">=", value =
/// 2 }`); the string form exists so rules can be read aloud and so the
/// loader accepts both spellings — one tiny parser, no expression language.
pub fn parse_when(text: &str) -> Result<When, String> {
    let text = text.trim();
    // Longest symbol operators first so `>=` is not read as `>` + `=...`.
    // `in` is whitespace-bounded so it cannot match inside a field name.
    for (op, spelling) in [
        (CompareOp::Ge, ">="),
        (CompareOp::Le, "<="),
        (CompareOp::Ne, "!="),
        (CompareOp::Eq, "=="),
        (CompareOp::Lt, "<"),
        (CompareOp::Gt, ">"),
    ] {
        if let Some(idx) = text.find(spelling) {
            return build_when(text, idx, spelling.len(), op);
        }
    }
    if let Some(idx) = text.find(" in ") {
        return build_when(text, idx + 1, 2, CompareOp::In);
    }
    Err(format!("when `{text}`: no operator found"))
}

fn build_when(text: &str, op_start: usize, op_len: usize, op: CompareOp) -> Result<When, String> {
    let field = text[..op_start].trim();
    let rest = text[op_start + op_len..].trim();
    let field = match field {
        "available_balance" => ConditionField::AvailableBalance,
        "total_equity" => ConditionField::TotalEquity,
        "open_positions" => ConditionField::OpenPositions,
        "current_price" => ConditionField::CurrentPrice,
        "time_left_sec" => ConditionField::TimeLeftSec,
        "symbol" => ConditionField::Symbol,
        "recent_pnl_1h" => ConditionField::RecentPnl1h,
        "consecutive_losses" => ConditionField::ConsecutiveLosses,
        other => return Err(format!("when: unknown field `{other}`")),
    };
    let value = parse_condition_value(rest)?;
    validate_when(&When {
        field,
        op,
        value: value.clone(),
    })?;
    Ok(When { field, op, value })
}

/// Parse the RHS: `["BTC","ETH"]` (in), a quoted symbol, or a number.
fn parse_condition_value(text: &str) -> Result<ConditionValue, String> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        let items = inner
            .split(',')
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Err(format!("when: empty list `{text}`"));
        }
        return Ok(ConditionValue::TextList(items));
    }
    if (text.starts_with('"') && text.ends_with('"') && text.len() >= 2)
        || (text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2)
    {
        return Ok(ConditionValue::Text(text[1..text.len() - 1].to_string()));
    }
    Decimal::from_str_exact(text)
        .map(ConditionValue::Num)
        .map_err(|_| {
            format!("when: value `{text}` is neither a number, a quoted symbol nor a list")
        })
}
