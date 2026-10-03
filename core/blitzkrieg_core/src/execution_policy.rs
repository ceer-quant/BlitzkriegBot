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
//! TOML > compiled default. The global env vars are `BUDGET_RATIO` and
//! `MAX_BUDGET_USD`; the per-account form is the same name with the account
//! id uppercased and every non-alphanumeric turned into `_`, e.g.
//! `BUDGET_RATIO_BINANCE_MAIN` for account `binance-main`. Env is read at
//! load time only (a restart picks up changes), matching the repo's
//! restart-to-change knob discipline.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// The config file, relative to the process cwd (the repo root for the
/// production shell — the same lookup rule `accounts.toml` uses).
pub const EXECUTION_POLICY_CONFIG_PATH: &str = "user_layer/configs/execution_policy.toml";

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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ConditionValue {
    Num(Decimal),
    Text(String),
    TextList(Vec<String>),
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
/// The 10%/$1/$50 the issue names as today's numbers are the SHIPPED FILE's
/// `[defaults]`, not compiled constants — operators edit them there.
/// Defaults MUST NOT drift: the replay depends on it.
pub fn code_defaults() -> SectionDefaults {
    SectionDefaults {
        budget_ratio: Decimal::ONE,
        min_budget_usd: Decimal::ZERO,
        max_budget_usd: dec!(1_000_000_000),
        min_equity_usd: Decimal::ZERO,
        max_positions_per_asset: 1,
        rules: vec![Rule {
            name: "global_backstop".to_string(),
            priority: 100,
            enabled: true,
            when: When {
                field: ConditionField::OpenPositions,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(2)),
            },
            then: Action {
                action: Some(SkipAction::Skip),
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: Some("全局兜底：持仓数达上限（open_positions >= 2）".to_string()),
        }],
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
    /// zero-config deployment (code defaults, no account sections); a file
    /// that exists but does not parse, misses a field, or carries an
    /// out-of-range value REFUSES (fail-closed) — the caller refuses the
    /// boot rather than trading under a policy the operator did not write.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
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
        // every non-alphanumeric to `_` (§: BUDGET_RATIO_<ACCOUNT_ID大写非
        // 字母数字转下划线>).
        let mut accounts = HashMap::new();
        for (id, section) in &file.accounts {
            let label = format!("[accounts.{id}]");
            let mut resolved = defaults.clone();
            layer_section(&mut resolved, section, &label)?;
            apply_env(&mut resolved, Some(id), &env)?;
            resolved.validate(&label)?;
            accounts.insert(id.clone(), resolved);
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
/// (`BUDGET_RATIO`, `MAX_BUDGET_USD`); `Some(id)` = the per-account names
/// (`BUDGET_RATIO_<ID>`, `MAX_BUDGET_USD_<ID>`). Applied AFTER the TOML
/// layer so env outranks it.
fn apply_env(
    section: &mut SectionDefaults,
    account_id: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let suffix = account_id
        .map(|id| format!("_{}", env_account_suffix(id)))
        .unwrap_or_default();
    let ratio_key = format!("BUDGET_RATIO{suffix}");
    let max_key = format!("MAX_BUDGET_USD{suffix}");
    if let Some(raw) = env(&ratio_key) {
        section.budget_ratio = Decimal::from_str_exact(raw.trim())
            .map_err(|_| format!("env {ratio_key}={raw:?} does not parse as a decimal"))?;
    }
    if let Some(raw) = env(&max_key) {
        section.max_budget_usd = Decimal::from_str_exact(raw.trim())
            .map_err(|_| format!("env {max_key}={raw:?} does not parse as a decimal"))?;
    }
    Ok(())
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

impl When {
    /// Does this condition hold for the given facts?
    pub fn holds(&self, input: &PolicyInput) -> bool {
        match (self.field, &self.value) {
            (ConditionField::Symbol, ConditionValue::Text(t)) => match self.op {
                CompareOp::Eq => &input.symbol == t,
                CompareOp::Ne => &input.symbol != t,
                _ => false,
            },
            (ConditionField::Symbol, ConditionValue::TextList(list)) => match self.op {
                CompareOp::In => list.contains(&input.symbol),
                _ => false,
            },
            (ConditionField::Symbol, ConditionValue::Num(_)) => false,
            (field, ConditionValue::Num(n)) => {
                let Some(v) = field.numeric_value(input) else {
                    return false;
                };
                match self.op {
                    CompareOp::Lt => v < *n,
                    CompareOp::Le => v <= *n,
                    CompareOp::Gt => v > *n,
                    CompareOp::Ge => v >= *n,
                    CompareOp::Eq => v == *n,
                    CompareOp::Ne => v != *n,
                    // `in` on a numeric field has no reading: the grammar says
                    // `in` is the symbol-list form. An empty match is honest.
                    CompareOp::In => false,
                }
            }
            // Text/list values on numeric fields were refused at load time;
            // a `When` built in code with a mismatched pair holds nothing.
            (_, ConditionValue::Text(_) | ConditionValue::TextList(_)) => false,
        }
    }
}

// ── Facts / output ──────────────────────────────────────────────────────────

/// The eight condition facts for ONE candidate order. Built by the host per
/// evaluation cycle, so a rule sees the account the order actually names.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyInput {
    /// The requesting account's book balance (gross; the ledger's `balance()`).
    pub available_balance: Decimal,
    /// Balance + open-position cost basis (cash equity view).
    pub total_equity: Decimal,
    /// Positions currently open for this account.
    pub open_positions: u32,
    /// The signal's price.
    pub current_price: Decimal,
    /// Seconds until the round resolves (engine's round timing).
    pub time_left_sec: i64,
    /// The asset the entry names ("BTC", "ETH", ...).
    pub symbol: String,
    /// Net realized PnL over the trailing hour for this account.
    pub recent_pnl_1h: Decimal,
    /// The requesting strategy's current consecutive-loss streak.
    pub consecutive_losses: u32,
}

impl PolicyInput {
    /// The all-zero facts (a validator's "no opinion" input, used where a
    /// `When` must be type-checked without facts to judge).
    pub const ZERO: PolicyInput = PolicyInput {
        available_balance: Decimal::ZERO,
        total_equity: Decimal::ZERO,
        open_positions: 0,
        current_price: Decimal::ZERO,
        time_left_sec: 0,
        symbol: String::new(),
        recent_pnl_1h: Decimal::ZERO,
        consecutive_losses: 0,
    };
}

/// What the policy decided for one candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyOutput {
    /// Place the order with this budget (already clamped ≤ max_budget_usd).
    Place {
        budget_usd: Decimal,
        /// The knobs actually in force, for the audit/log line.
        ratio_used: Decimal,
        min_used: Decimal,
        max_used: Decimal,
    },
    /// Do not place. `reason` is the rule's own text — what the log shows.
    Skip { reason: String },
    /// The winning rule put the ACCOUNT into a cooldown: place nothing for
    /// this account for N seconds. The CALLER owns the clock (it owns every
    /// other clock in the kernel); the rule only states the duration.
    Cooldown { seconds: i64 },
}

impl PolicyOutput {
    /// The refusal bucket a policy skip is reported under. Deliberately NOT
    /// `positions.already_in` or any existing vocabulary — a policy skip is
    /// its own cause.
    pub const SKIP_BUCKET: &'static str = "policy.skip";
}

/// A policy refusal: the account is unknown, or the policy itself is in a
/// state that cannot judge (never produced by a valid load — fail-closed
/// happens at load time).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRefusal {
    pub reason: String,
}

// ── evaluate ────────────────────────────────────────────────────────────────

/// Judge one candidate order under `policy` for `account_id`.
///
/// Ordering: the section's `min_equity_usd` is a structural guard consulted
/// first; then rules ascending by `priority`, first `enabled && when(input)`
/// wins; no hit uses the section defaults. The budget clamp runs on the one
/// budget constructor below, so no rule combination can lift `budget_usd`
/// above `max_budget_usd`.
pub fn evaluate(
    policy: &Policy,
    account_id: &str,
    input: &PolicyInput,
) -> Result<PolicyOutput, PolicyRefusal> {
    // "account_id 缺失 → 拒绝下单": an empty id is not a section name, and
    // never silently the defaults.
    if account_id.trim().is_empty() {
        return Err(PolicyRefusal {
            reason: "execution_policy: order carries no account_id — refusing (not silently \
                     judged under defaults)"
                .to_string(),
        });
    }
    let section = policy.section_for(account_id);
    // Structural equity guard: below the floor the account sits out, before
    // any rule is consulted.
    if input.total_equity < section.min_equity_usd {
        return Ok(PolicyOutput::Skip {
            reason: format!(
                "equity {} below min_equity_usd {}",
                input.total_equity, section.min_equity_usd
            ),
        });
    }
    // Rules ascending by priority; first hit wins (§: 规则按 priority 从小到
    // 大，第一条 when 为真者生效). `sort_by_key` is stable, so equal
    // priorities keep file order.
    let mut ordered: Vec<&Rule> = section.rules.iter().filter(|r| r.enabled).collect();
    ordered.sort_by_key(|r| r.priority);
    for rule in ordered {
        if rule.when.holds(input) {
            return apply_rule(rule, section, input);
        }
    }
    Ok(place_budget(&section_budget(section), input))
}

/// Apply the winning rule to the facts.
fn apply_rule(
    rule: &Rule,
    section: &SectionDefaults,
    input: &PolicyInput,
) -> Result<PolicyOutput, PolicyRefusal> {
    let then = &rule.then;
    if then.action == Some(SkipAction::Skip) {
        let reason = rule
            .reason
            .clone()
            .unwrap_or_else(|| format!("rule `{}`: skip", rule.name));
        return Ok(PolicyOutput::Skip { reason });
    }
    if let Some(sec) = then.cooldown_sec {
        // The cooldown is enforced by the CALLER (it owns the clocks); the
        // rule only states it. A non-positive cooldown is a typo — refuse.
        if sec <= 0 {
            return Err(PolicyRefusal {
                reason: format!("rule `{}`: cooldown_sec {sec} must be > 0", rule.name),
            });
        }
        return Ok(PolicyOutput::Cooldown { seconds: sec });
    }
    // A sizing override: each knob independently replaces the default, but
    // the section's max_budget_usd stays the CEILING — a rule may tighten it,
    // never widen it (the safety lock, applied to the override itself).
    let scoped = ScopedDefaults {
        budget_ratio: then.budget_ratio.unwrap_or(section.budget_ratio),
        min_budget_usd: then.min_budget_usd.unwrap_or(section.min_budget_usd),
        max_budget_usd: then
            .max_budget_usd
            .map_or(section.max_budget_usd, |v| v.min(section.max_budget_usd)),
    };
    if let Err(e) = scoped.validate(rule.name.as_str()) {
        return Err(PolicyRefusal { reason: e });
    }
    Ok(place_budget(&scoped, input))
}

/// The three sizing knobs one evaluation actually uses (defaults or the
/// winning rule's overrides).
#[derive(Debug, Clone, Copy)]
struct ScopedDefaults {
    budget_ratio: Decimal,
    min_budget_usd: Decimal,
    max_budget_usd: Decimal,
}

impl ScopedDefaults {
    fn validate(&self, label: &str) -> Result<(), String> {
        if self.budget_ratio <= Decimal::ZERO || self.budget_ratio > Decimal::ONE {
            return Err(format!(
                "{label}: budget_ratio {} outside (0, 1]",
                self.budget_ratio
            ));
        }
        // min_budget_usd 0 is the NEUTRAL floor (raises nothing); a negative
        // one is a typo. The max must stay positive — it is the ceiling.
        if self.min_budget_usd < Decimal::ZERO || self.max_budget_usd <= Decimal::ZERO {
            return Err(format!("{label}: budget floors must be >= 0, ceiling > 0"));
        }
        if self.max_budget_usd < self.min_budget_usd {
            return Err(format!(
                "{label}: max_budget_usd {} < min_budget_usd {}",
                self.max_budget_usd, self.min_budget_usd
            ));
        }
        Ok(())
    }
}

fn section_budget(section: &SectionDefaults) -> ScopedDefaults {
    ScopedDefaults {
        budget_ratio: section.budget_ratio,
        min_budget_usd: section.min_budget_usd,
        max_budget_usd: section.max_budget_usd,
    }
}

/// THE budget arithmetic — the one place the formula lives:
///
/// `budget_usd = clamp(max(available_balance × budget_ratio, min_budget_usd),
/// ≤ max_budget_usd)`
///
/// Every path through [`evaluate`] funnels here, so the safety lock ("no
/// rule combination may lift the budget above max_budget_usd") is structural.
fn place_budget(scoped: &ScopedDefaults, input: &PolicyInput) -> PolicyOutput {
    let raw = input.available_balance * scoped.budget_ratio;
    let floored = raw.max(scoped.min_budget_usd);
    let budget_usd = floored.min(scoped.max_budget_usd);
    PolicyOutput::Place {
        budget_usd,
        ratio_used: scoped.budget_ratio,
        min_used: scoped.min_budget_usd,
        max_used: scoped.max_budget_usd,
    }
}

// ── Audit record ────────────────────────────────────────────────────────────

/// Who made a policy change and what it looked like. One JSONL line per
/// change (`data/audit/execution_policy.jsonl`), same discipline as the
/// settlement and intent journals: append-only, best-effort, never fatal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyAuditRecord {
    /// ms epoch of the change.
    pub ts_ms: i64,
    /// "boot" (load at startup / load refused) or "ipc" (set/reset).
    pub actor: String,
    /// "set" | "reset" | "load-refused".
    pub action: String,
    /// The account the change names ("*" for the global section).
    pub account_id: String,
    /// What the section looked like before (None = absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    /// What it looks like after (None = absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<serde_json::Value>,
    /// On load-refused: the parse/validation error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Append one audit line. Best-effort (the in-memory policy stays
/// authoritative); returns whether a line landed.
pub fn append_audit(record: &PolicyAuditRecord) -> bool {
    crate::jsonl::append(Path::new(EXECUTION_POLICY_AUDIT_PATH), record)
}

/// Read the audit trail back (file order). Unparseable lines are skipped
/// with one warning — the shared JSONL loader discipline.
pub fn load_audit() -> Vec<PolicyAuditRecord> {
    crate::jsonl::load(
        Path::new(EXECUTION_POLICY_AUDIT_PATH),
        "execution policy audit: skipped unparseable lines",
    )
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// The actor spelling for a boot-time record.
pub const ACTOR_BOOT: &str = "boot";
/// The actor spelling for an IPC change.
pub const ACTOR_IPC: &str = "ipc";

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic 10%/$1/$50 section the first 43 tests were written
    /// against — now the SHIPPED FILE's defaults, not the compiled ones
    /// (compiled defaults are the neutral kernel; see `code_defaults`).
    fn classic() -> SectionDefaults {
        SectionDefaults {
            budget_ratio: dec!(0.10),
            min_budget_usd: dec!(1.00),
            max_budget_usd: dec!(50.00),
            min_equity_usd: dec!(1.00),
            max_positions_per_asset: 1,
            rules: vec![Rule {
                name: "global_backstop".to_string(),
                priority: 100,
                enabled: true,
                when: When {
                    field: ConditionField::OpenPositions,
                    op: CompareOp::Ge,
                    value: ConditionValue::Num(Decimal::from(2)),
                },
                then: Action {
                    action: Some(SkipAction::Skip),
                    budget_ratio: None,
                    min_budget_usd: None,
                    max_budget_usd: None,
                    cooldown_sec: None,
                },
                reason: Some("全局兜底：持仓数达上限".to_string()),
            }],
        }
    }

    fn classic_policy() -> Policy {
        Policy {
            defaults: classic(),
            accounts: HashMap::new(),
        }
    }

    fn input(balance: i64, open: u32) -> PolicyInput {
        PolicyInput {
            available_balance: Decimal::from(balance),
            total_equity: Decimal::from(balance),
            open_positions: open,
            current_price: dec!(0.50),
            time_left_sec: 300,
            symbol: "BTC".to_string(),
            recent_pnl_1h: Decimal::ZERO,
            consecutive_losses: 0,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn no_rule_hit_uses_defaults() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(100, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(10.00)); // 100 × 0.10
    }

    #[test]
    fn first_hit_wins_among_holders() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![skip_rule(), ratio_rule()];
        let out = evaluate(&policy, "default", &input(100, 5)).unwrap();
        assert!(matches!(out, PolicyOutput::Skip { .. }));
    }

    #[test]
    fn lower_priority_number_beats_higher() {
        let mut policy = classic_policy();
        let mut first = ratio_rule();
        first.priority = 5;
        let mut second = skip_rule();
        second.priority = 10;
        policy.defaults.rules = vec![second, first];
        let out = evaluate(&policy, "default", &input(19, 5)).unwrap();
        let PolicyOutput::Place { ratio_used, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(ratio_used, dec!(0.05));
    }

    fn ratio_rule() -> Rule {
        Rule {
            name: "small_balance_ratio".into(),
            priority: 100,
            enabled: true,
            when: When {
                field: ConditionField::AvailableBalance,
                op: CompareOp::Lt,
                value: ConditionValue::Num(Decimal::from(20)),
            },
            then: Action {
                action: None,
                budget_ratio: Some(dec!(0.05)),
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: Some("小余额降杠杆".into()),
        }
    }

    fn skip_rule() -> Rule {
        Rule {
            name: "backstop".into(),
            priority: 100,
            enabled: true,
            when: When {
                field: ConditionField::OpenPositions,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(2)),
            },
            then: Action {
                action: Some(SkipAction::Skip),
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: Some("全局兜底：持仓数达上限".into()),
        }
    }

    #[test]
    fn balance_below_threshold_uses_lower_ratio() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![ratio_rule()];
        let out = evaluate(&policy, "default", &input(19, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 19 × 0.05 = 0.95 → floored to min_budget_usd 1.00
        assert_eq!(budget_usd, dec!(1.00));
    }

    #[test]
    fn open_positions_ge_2_skips() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(100, 2)).unwrap();
        let PolicyOutput::Skip { reason } = out else {
            panic!("expected Skip, got {out:?}")
        };
        assert!(reason.contains("全局兜底"));
    }

    #[test]
    fn min_budget_floor_applies() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(5, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 5 × 0.10 = 0.50 → floor 1.00
        assert_eq!(budget_usd, dec!(1.00));
    }

    #[test]
    fn max_budget_ceiling_applies() {
        let policy = classic_policy();
        let out = evaluate(&policy, "default", &input(1_000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        // 1000 × 0.10 = 100 → ceiling 50
        assert_eq!(budget_usd, dec!(50.00));
    }

    #[test]
    fn rule_cannot_lift_budget_above_section_ceiling() {
        // A rule with budget_ratio = 1 (allowed range) on a huge balance: the
        // SECTION ceiling (50) still binds — the clamp runs on the one budget
        // constructor no path skips.
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "all_in".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: Some(Decimal::ONE),
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(10_000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(50.00));
    }

    #[test]
    fn rule_max_override_cannot_widen_ceiling() {
        // then = { max_budget_usd = 1000 } on a section ceiling of 50: the
        // override can only TIGHTEN. Budget stays ≤ 50.
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "widen".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: Some(dec!(1000)),
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(10_000, 0)).unwrap();
        let PolicyOutput::Place {
            budget_usd,
            max_used,
            ..
        } = out
        else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(50.00));
        assert_eq!(max_used, dec!(50.00));
    }

    #[test]
    fn rule_max_override_can_tighten_ceiling() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "tighten".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::Symbol,
                op: CompareOp::Eq,
                value: ConditionValue::Text("BTC".into()),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: Some(dec!(2)),
                cooldown_sec: None,
            },
            reason: None,
        }];
        let out = evaluate(&policy, "default", &input(1000, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place, got {out:?}")
        };
        assert_eq!(budget_usd, dec!(2.00));
    }

    #[test]
    fn min_equity_floor_skips_before_rules() {
        let mut policy = classic_policy();
        policy.defaults.rules = Vec::new();
        policy.defaults.min_equity_usd = dec!(10);
        let mut facts = input(5, 0);
        facts.total_equity = dec!(5);
        let out = evaluate(&policy, "default", &facts).unwrap();
        let PolicyOutput::Skip { reason } = out else {
            panic!("expected Skip, got {out:?}")
        };
        assert!(reason.contains("min_equity_usd"));
        // At the floor (or above) it places again.
        facts.total_equity = dec!(10);
        assert!(matches!(
            evaluate(&policy, "default", &facts).unwrap(),
            PolicyOutput::Place { .. }
        ));
    }

    #[test]
    fn account_isolation_a_vs_b() {
        let mut policy = classic_policy();
        policy.accounts.insert(
            "binance-main".to_string(),
            SectionDefaults {
                budget_ratio: dec!(0.20),
                ..code_defaults()
            },
        );
        let a = evaluate(&policy, "binance-main", &input(100, 0)).unwrap();
        let b = evaluate(&policy, "other", &input(100, 0)).unwrap();
        let PolicyOutput::Place {
            budget_usd: a_usd, ..
        } = a
        else {
            panic!("expected Place")
        };
        let PolicyOutput::Place {
            budget_usd: b_usd, ..
        } = b
        else {
            panic!("expected Place")
        };
        assert_eq!(a_usd, dec!(20.00));
        assert_eq!(b_usd, dec!(10.00));
    }

    #[test]
    fn unconfigured_account_inherits_defaults() {
        let mut policy = classic_policy();
        policy.accounts.insert(
            "binance-main".to_string(),
            SectionDefaults {
                budget_ratio: dec!(0.20),
                ..code_defaults()
            },
        );
        let out = evaluate(&policy, "totally-unconfigured", &input(100, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place")
        };
        assert_eq!(budget_usd, dec!(10.00));
    }

    #[test]
    fn missing_account_id_refuses() {
        let policy = classic_policy();
        let err = evaluate(&policy, "", &input(100, 0)).unwrap_err();
        assert!(err.reason.contains("no account_id"));
        let err = evaluate(&policy, "   ", &input(100, 0)).unwrap_err();
        assert!(err.reason.contains("no account_id"));
    }

    #[test]
    fn disabled_rule_does_not_fire() {
        let mut policy = classic_policy();
        let mut r = skip_rule();
        r.enabled = false;
        policy.defaults.rules = vec![r];
        let out = evaluate(&policy, "default", &input(100, 2)).unwrap();
        assert!(matches!(out, PolicyOutput::Place { .. }));
    }

    #[test]
    fn parse_valid_toml() {
        let text = r#"
version = 1
[defaults]
budget_ratio = 0.10
min_budget_usd = 1.00
max_budget_usd = 50.00
min_equity_usd = 1.00
max_positions_per_asset = 1

[[defaults.rules]]
name = "global_backstop"
enabled = true
priority = 100
when = "open_positions >= 2"
then = { action = "skip" }
reason = "全局兜底：持仓数达上限"

[accounts.binance-main]
budget_ratio = 0.05

[[accounts.binance-main.rules]]
name = "tiny"
enabled = true
priority = 1
when = "available_balance < 20"
then = { budget_ratio = 0.05 }
"#;
        let policy = Policy::load_with_env(text, no_env).unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
        assert_eq!(policy.defaults.max_budget_usd, dec!(50.00));
        assert_eq!(policy.defaults.max_positions_per_asset, 1);
        assert_eq!(policy.defaults.rules.len(), 1);
        let acct = policy.accounts.get("binance-main").unwrap();
        assert_eq!(acct.budget_ratio, dec!(0.05));
        assert_eq!(acct.rules.len(), 1);
        // The account kept the global backstop? No: its own rules REPLACE.
        assert_eq!(acct.rules[0].name, "tiny");
    }

    #[test]
    fn toml_float_arrives_as_clean_decimal() {
        // The precision rule: 0.10 must equal dec!(0.10) — not the binary
        // expansion the default serde path would carry.
        let policy =
            Policy::load_with_env("version = 1\n[defaults]\nbudget_ratio = 0.10\n", no_env)
                .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
    }

    #[test]
    fn invalid_version_refuses() {
        let err = Policy::load_with_env("version = 2\n", no_env).unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn unknown_field_refuses() {
        let err = Policy::load_with_env("version = 1\n[defaults]\nbudget_ration = 0.1\n", no_env)
            .unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn out_of_range_ratio_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[defaults]\nbudget_ratio = 1.5\nmax_budget_usd = 50\nmin_budget_usd = 1\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("outside (0, 1]"), "{err}");
    }

    #[test]
    fn per_account_out_of_range_refuses() {
        let err = Policy::load_with_env("version = 1\n[accounts.bad]\nbudget_ratio = 0\n", no_env)
            .unwrap_err();
        assert!(err.contains("[accounts.bad]"), "{err}");
    }

    #[test]
    fn missing_required_field_refuses() {
        // A rule without `when`.
        let err = Policy::load_with_env(
            "version = 1\n[[defaults.rules]]\nname = \"x\"\nenabled = true\nthen = { action = \"skip\" }\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn rule_with_two_actions_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[[defaults.rules]]\nname = \"x\"\nenabled = true\nwhen = \"symbol == \\\"BTC\\\"\"\nthen = { action = \"skip\", budget_ratio = 0.5 }\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("exactly one action"), "{err}");
    }

    #[test]
    fn max_below_min_refuses() {
        let err = Policy::load_with_env(
            "version = 1\n[defaults]\nbudget_ratio = 0.1\nmin_budget_usd = 10\nmax_budget_usd = 5\n",
            no_env,
        )
        .unwrap_err();
        assert!(err.contains("must not sit below the floor"), "{err}");
    }

    #[test]
    fn env_overrides_toml_globally() {
        let text = "version = 1\n[defaults]\nbudget_ratio = 0.10\n";
        let policy =
            Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "0.02".to_string()))
                .unwrap();
        assert_eq!(policy.defaults.budget_ratio, dec!(0.02));
    }

    #[test]
    fn per_account_env_overrides_per_account_toml() {
        let text = "version = 1\n[defaults]\nbudget_ratio = \"0.10\"\n\n[accounts.binance-main]\nbudget_ratio = \"0.05\"\n";
        let policy = Policy::load_with_env(text, |k| {
            (k == "BUDGET_RATIO_BINANCE_MAIN").then(|| "0.07".to_string())
        })
        .unwrap();
        assert_eq!(policy.accounts["binance-main"].budget_ratio, dec!(0.07));
        // Another account keeps its own (or the global) layer.
        assert_eq!(policy.defaults.budget_ratio, dec!(0.10));
    }

    #[test]
    fn global_env_sits_below_account_toml() {
        let text = "version = 1\n[accounts.binance-main]\nbudget_ratio = 0.05\n";
        let policy =
            Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "0.02".to_string()))
                .unwrap();
        // 全局 env (0.02) > 全局 TOML (0.10)，但账户级 TOML (0.05) 又高于全局 env。
        assert_eq!(policy.accounts["binance-main"].budget_ratio, dec!(0.05));
        assert_eq!(policy.defaults.budget_ratio, dec!(0.02));
    }

    #[test]
    fn account_env_does_not_touch_other_accounts() {
        let text =
            "version = 1\n[accounts.a]\nbudget_ratio = 0.05\n[accounts.b]\nbudget_ratio = 0.06\n";
        let policy = Policy::load_with_env(text, |k| {
            (k == "BUDGET_RATIO_A").then(|| "0.09".to_string())
        })
        .unwrap();
        assert_eq!(policy.accounts["a"].budget_ratio, dec!(0.09));
        assert_eq!(policy.accounts["b"].budget_ratio, dec!(0.06));
    }

    #[test]
    fn malformed_env_value_refuses_load() {
        let text = "version = 1\n";
        let err = Policy::load_with_env(text, |k| (k == "BUDGET_RATIO").then(|| "abc".to_string()))
            .unwrap_err();
        assert!(err.contains("BUDGET_RATIO"), "{err}");
    }

    #[test]
    fn when_string_form_parses_and_holds() {
        let w = parse_when("open_positions >= 2").unwrap();
        assert_eq!(
            w,
            When {
                field: ConditionField::OpenPositions,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(2)),
            }
        );
        assert!(w.holds(&input(100, 2)));
        assert!(!w.holds(&input(100, 1)));
    }

    #[test]
    fn when_symbol_in_list_parses_and_holds() {
        let w = parse_when("symbol in [\"BTC\", \"ETH\"]").unwrap();
        assert!(matches!(w.value, ConditionValue::TextList(_)));
        assert!(w.holds(&input(100, 0)));
        let mut other = input(100, 0);
        other.symbol = "SOL".to_string();
        assert!(!w.holds(&other));
    }

    #[test]
    fn when_all_operators() {
        let i = input(100, 1);
        assert!(parse_when("available_balance < 101").unwrap().holds(&i));
        assert!(parse_when("available_balance <= 100").unwrap().holds(&i));
        assert!(parse_when("available_balance > 99").unwrap().holds(&i));
        assert!(parse_when("available_balance >= 100").unwrap().holds(&i));
        assert!(parse_when("available_balance == 100").unwrap().holds(&i));
        assert!(parse_when("available_balance != 99").unwrap().holds(&i));
        assert!(parse_when("symbol in [\"BTC\"]").unwrap().holds(&i));
        assert!(!parse_when("available_balance < 100").unwrap().holds(&i));
    }

    #[test]
    fn when_unknown_field_refuses() {
        assert!(parse_when("price_of_bitcoin > 2").is_err());
        assert!(parse_when("no operator here").is_err());
    }

    #[test]
    fn when_symbol_vs_number_refuses() {
        assert!(parse_when("symbol == 5").is_err());
        assert!(parse_when("available_balance < \"BTC\"").is_err());
    }

    #[test]
    fn when_recent_pnl_and_losses_holders() {
        let mut i = input(100, 0);
        i.recent_pnl_1h = dec!(-2.5);
        i.consecutive_losses = 3;
        assert!(parse_when("recent_pnl_1h < 0").unwrap().holds(&i));
        assert!(parse_when("consecutive_losses >= 3").unwrap().holds(&i));
        assert!(!parse_when("consecutive_losses > 3").unwrap().holds(&i));
    }

    #[test]
    fn env_account_suffix_mapping() {
        assert_eq!(env_account_suffix("binance-main"), "BINANCE_MAIN");
        assert_eq!(env_account_suffix("default"), "DEFAULT");
        assert_eq!(env_account_suffix("a.b:c"), "A_B_C");
    }

    #[test]
    fn cooldown_rule_reports_cooldown_not_skip() {
        let mut policy = classic_policy();
        policy.defaults.rules = vec![Rule {
            name: "cool".into(),
            priority: 1,
            enabled: true,
            when: When {
                field: ConditionField::ConsecutiveLosses,
                op: CompareOp::Ge,
                value: ConditionValue::Num(Decimal::from(3)),
            },
            then: Action {
                action: None,
                budget_ratio: None,
                min_budget_usd: None,
                max_budget_usd: None,
                cooldown_sec: Some(60),
            },
            reason: None,
        }];
        let mut i = input(100, 0);
        i.consecutive_losses = 3;
        let out = evaluate(&policy, "default", &i).unwrap();
        assert_eq!(out, PolicyOutput::Cooldown { seconds: 60 });
        // Non-positive cooldown is refused at evaluate time.
        policy.defaults.rules[0].then.cooldown_sec = Some(0);
        let err = evaluate(&policy, "default", &i).unwrap_err();
        assert!(err.reason.contains("must be > 0"));
    }

    #[test]
    fn audit_record_round_trips_through_json() {
        let rec = PolicyAuditRecord {
            ts_ms: 1,
            actor: ACTOR_IPC.to_string(),
            action: "set".to_string(),
            account_id: "default".to_string(),
            before: None,
            after: Some(serde_json::json!({"budget_ratio": "0.05"})),
            error: None,
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: PolicyAuditRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.account_id, "default");
        assert_eq!(back.action, "set");
        assert_eq!(back.actor, "ipc");
    }

    #[test]
    fn ratio_of_exactly_one_is_allowed() {
        let mut policy = classic_policy();
        policy.defaults.budget_ratio = Decimal::ONE;
        let out = evaluate(&policy, "default", &input(10, 0)).unwrap();
        let PolicyOutput::Place { budget_usd, .. } = out else {
            panic!("expected Place")
        };
        assert_eq!(budget_usd, dec!(10.00));
    }

    #[test]
    fn empty_account_section_inherits_globals() {
        // The globals come from the FILE's [defaults] here (the classic
        // 10%/$1/$50) — the compiled defaults are the neutral kernel.
        let policy = Policy::load_with_env(
            "version = 1\n\n[defaults]\nbudget_ratio = \"0.10\"\nmin_budget_usd = \"1\"\n\n[accounts.empty-acct]\n",
            no_env,
        )
        .unwrap();
        let section = policy.section_for("empty-acct");
        assert_eq!(section.budget_ratio, dec!(0.10));
        // The section carries no `rules` of its own, so it inherits the
        // globals' — including the code default's backstop. "未配置账户继承
        // defaults" means the WHOLE resolved section, rules included.
        assert_eq!(section.rules.len(), 1, "inherited the global backstop");
        assert_eq!(section.rules[0].name, "global_backstop");
    }

    #[test]
    fn section_default_for_ipc_serialization() {
        // The IPC layer stores section summaries as JSON; Decimal fields are
        // strings on that wire (the crate's decimal convention).
        let section = SectionFields {
            budget_ratio: Some(dec!(0.05)),
            ..SectionFields::default()
        };
        let v = serde_json::to_value(&section).unwrap();
        assert_eq!(v["budget_ratio"], serde_json::json!("0.05"));
        let back: SectionFields = serde_json::from_value(v).unwrap();
        assert_eq!(back.budget_ratio, Some(dec!(0.05)));
    }

    #[test]
    fn when_table_form_from_toml() {
        let text = r#"
version = 1
[[defaults.rules]]
name = "t"
enabled = true
when = { field = "open_positions", op = ">=", value = 2 }
then = { action = "skip" }
"#;
        let policy = Policy::load_with_env(text, no_env).unwrap();
        assert!(matches!(
            evaluate(&policy, "default", &input(100, 2)).unwrap(),
            PolicyOutput::Skip { .. }
        ));
    }

    #[test]
    fn mixed_field_value_pair_in_table_form_refuses() {
        let text = r#"
version = 1
[[defaults.rules]]
name = "t"
enabled = true
when = { field = "symbol", op = "==", value = 5 }
then = { action = "skip" }
"#;
        let err = Policy::load_with_env(text, no_env).unwrap_err();
        assert!(err.contains("compares against text"), "{err}");
    }
}
