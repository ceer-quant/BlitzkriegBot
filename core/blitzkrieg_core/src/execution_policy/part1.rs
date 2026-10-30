//! Split out of the parent module (issue #780): pure move.

use super::*;

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

/// Append one audit line to the journal at `path`. Best-effort (the
/// in-memory policy stays authoritative); returns whether a line landed.
///
/// The path is the CALLER's decision, not this module's: production resolves
/// `CoreConfig::execution_policy_audit_path` (or the shipped
/// [`EXECUTION_POLICY_AUDIT_PATH`] spelling when unset), and a test core
/// points at its own private file — two appenders racing on one journal used
/// to be able to fuse their records into one unparseable line, silently
/// losing both (the #386 flake; `jsonl::append` now lands each record in one
/// atomic `write_all`, and the test cores carry private journals on top).
pub fn append_audit_to(path: &Path, record: &PolicyAuditRecord) -> bool {
    crate::jsonl::append(path, record)
}

/// Read the audit trail back from the journal at `path` (file order).
/// Unparseable lines are skipped with one warning — the shared JSONL loader
/// discipline. A missing file is an empty trail.
pub fn load_audit_from(path: &Path) -> Vec<PolicyAuditRecord> {
    crate::jsonl::load(path, "execution policy audit: skipped unparseable lines")
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// The actor spelling for a boot-time record.
pub const ACTOR_BOOT: &str = "boot";
/// The actor spelling for an IPC change.
pub const ACTOR_IPC: &str = "ipc";
