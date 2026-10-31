//! Split out of the parent module: pure move.
//! The panel's data model: tabs, actions, check stages, the `App` state
//! tree, policy drafts and their rule-editing logic.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Positions,
    /// issue 456: the kernel's order book (orders.list rows), with the [F]
    /// force-close on a selected ghost order.
    Orders,
    Trades,
    Plugins,
    Evolution,
    /// E25 (#331): the arbitration audit — every suggestion and the kernel's
    /// four-gate verdict (§13.4's TUI face).
    Decisions,
    Settings,
}

impl Tab {
    pub fn titles() -> Vec<&'static str> {
        vec![
            "1 Overview",
            "2 Positions",
            "3 Orders",
            "4 Trades",
            "5 Plugins",
            "6 Evolution",
            "7 Decisions",
            "8 Settings",
        ]
    }
    pub fn index(self) -> usize {
        match self {
            Tab::Overview => 0,
            Tab::Positions => 1,
            Tab::Orders => 2,
            Tab::Trades => 3,
            Tab::Plugins => 4,
            Tab::Evolution => 5,
            Tab::Decisions => 6,
            Tab::Settings => 7,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Positions,
            Tab::Positions => Tab::Orders,
            Tab::Orders => Tab::Trades,
            Tab::Trades => Tab::Plugins,
            Tab::Plugins => Tab::Evolution,
            Tab::Evolution => Tab::Decisions,
            Tab::Decisions => Tab::Settings,
            Tab::Settings => Tab::Overview,
        }
    }
}

/// What the main loop should do in response to a key.
pub enum Action {
    None,
    Quit,
    Refresh,
    RunCommand(String),
    /// A toggle command was built; the main loop checks
    /// `App::toggle_needs_confirmation` and either asks or dispatches.
    ConfirmToggle(String),
    /// Reload the plugin registry (used on entering the Plugins tab and after
    /// a toggling action).
    RefreshPlugins,
    /// #364: (re)read the execution-policy face's data off the render loop —
    /// effective section, list, preview and history in one worker round-trip.
    /// The writes never run here: `PolicyEdit` carries them.
    PolicyRefresh,
    /// #364: one policy write (`set` | `reset`) off the render loop. The
    /// worker lands the audit line; the reply re-reads the effective view so
    /// what the face shows next is the RELOADED state, never a local guess.
    PolicyEdit {
        action: PolicyAction,
    },
    /// Run the network self-check (`net.check`) off the render loop — the probe
    /// dials, so it answers in seconds, not milliseconds.
    NetCheck,
    /// `system.update.check` off the render loop (VERSIONING.md §7.4). The
    /// verdict travels in the next snapshot's `system_version`.
    UpdateCheck,
    /// `system.update.configure` for the AUTO switch (the checked side is only
    /// written by the operator's config or the configure verb itself).
    UpdateConfigure(bool),
    /// #393: `system.update.configure` for the CHECK switch — the outbound
    /// "may the kernel ask the release source at all" gate. The kernel takes
    /// both keys optional, so this write leaves the AUTO switch untouched.
    UpdateConfigureCheck(bool),
    /// #379 (§7.5): `system.update.stage` off the render loop — the kernel
    /// downloads + verifies the newer release into its staging directory and
    /// STOPS there; applying it is the launcher's job, never the kernel's
    /// (and never this panel's).
    UpdateStage,
    /// Run the LAUNCHER's install (`current_exe() update --install`): the
    /// panel process never replaces the kernel binary itself (P12) — it
    /// triggers the launcher-side installer and reports its output.
    UpdateInstall,
    /// 账户↔venue 解耦（本轮）：`account.list` off the render loop — the `A`
    /// overlay's read. A fresh client of its own (same reasoning as
    /// NetCheck/PolicyRefresh); the reply is the kernel's own book, raw.
    AccountsRefresh,
}

/// How far the self-check has got. Advances as snapshots finally arrive with
/// the properties each stage needs; failures keep the step red with a hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckStage {
    #[default]
    /// Nothing seen yet — socket connected flag false so far.
    Connecting,
    /// Connected but no book/top/round motion yet.
    Handshake,
    /// Connected with feed motion (books or tops seen this session).
    Ready,
}

/// #364: the two policy writes the face can land. The reset asks first
/// (same confirm bar as a dangerous toggle) — it silently reverts an
/// account to the globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAction {
    Set,
    Reset,
}

/// One bottom-bar hint a newcomer needs; once consumed, it stops rotating.
pub const HINTS: [&str; 7] = [
    "press : to type a command — try `status`",
    "1-6 switch pages (Overview/Positions/Trades/Plugins/Evolution/Settings)",
    "? for the full key & command help",
    "r refresh now · q quit",
    "start with --manage to enable start/stop commands",
    "Plugins: ↑/↓ move · Enter toggle (dangerous toggles ask y/n)",
    "n network check — is it us or the venue?",
];

/// Sentinel prefix for the one Settings confirmation that is not a gateway
/// command: flipping the AUTO-UPDATE switch. `y` on the confirm bar routes it
/// to [`Action::UpdateConfigure`] instead of the dispatcher.
pub const UPDATE_AUTO_CONFIRM_PREFIX: &str = "update.auto ";

/// #393 sentinel prefix: flipping the UPDATE-CHECK switch (#393). `y` routes
/// to [`Action::UpdateConfigureCheck`] — the kernel's configure verb takes
/// both switches optional, so only the check side moves.
pub const UPDATE_CHECK_CONFIRM_PREFIX: &str = "update.check ";

/// #364 sentinel prefix: a confirmed policy write. The confirm bar's text is
/// `policy.set <accountId>` / `policy.reset <accountId>`; `y` routes it to
/// [`Action::PolicyEdit`] (the params were built at ask time and ride in the
/// App's pending slot) instead of the gateway dispatcher.
pub const POLICY_CONFIRM_PREFIX: &str = "policy.";

pub struct App {
    pub snap: UiSnapshot,
    pub tab: Tab,
    pub input: String,
    pub input_active: bool,
    pub logs: Vec<String>,
    pub managed: bool,
    pub pid: Option<u32>,
    pub lifecycle_enabled: bool,
    pub socket: String,
    pub should_quit: bool,
    /// When the snapshot shown was last updated (for the "Xs ago" header).
    pub last_update: Option<Instant>,
    /// E25 (#331): the intent-audit tail as RAW rows — the Decisions tab
    /// renders exactly what `data/audit/intents.jsonl` holds, no second model.
    pub decisions: Vec<serde_json::Value>,
    /// Plugin-manager selection: 0=strategies pane column, then row index per pane.
    pub plugin_focus: usize,
    /// Evolution-tab selection: index into the pending-proposal list (the same
    /// order `ui.rs` renders, so cursor and screen agree).
    pub evo_focus: usize,
    /// issue 456: Orders-tab selection — index into `snap.orders` as rendered.
    pub orders_focus: usize,
    /// Pending confirmation for a dangerous plugin toggle: `Some(text)` shows
    /// the confirm bar; `y` executes, anything else cancels.
    pub pending_confirmation: Option<String>,
    /// Self-check stage derived from snapshots (E9-f #61).
    pub check: CheckStage,
    /// Hints already consumed this session (bottom bar stops rotating them).
    pub hints_used: [bool; HINTS.len()],
    /// History of executed commands, oldest first (↑/↓ recall).
    pub history: Vec<String>,
    /// `Some(offset)` while recalling history (0 = newest); `None` when idle.
    pub history_browse: Option<usize>,
    /// Help overlay is visible (`?` toggles).
    pub help_visible: bool,
    /// The network self-check overlay (`n` toggles). Full-report, so it sits on
    /// top and takes the keys while it is up.
    pub net_visible: bool,
    /// The last report the core answered with, kept so reopening `n` does not
    /// dial again. `None` until the first probe lands.
    pub net_report: Option<NetCheckReportView>,
    /// Why the last probe could not be answered (core unreachable, IPC error).
    /// Kept apart from a report that answered with broken paths.
    pub net_error: Option<String>,
    /// A probe is running right now (the overlay says so instead of showing an
    /// empty table).
    pub net_busy: bool,
    /// Non-empty while the core's kill switch is engaged — the body renders a
    /// full-screen red banner until `risk.resume` clears it.
    pub kill_banner: Option<String>,
    /// An update check / configure / install round-trip is in flight (the
    /// Settings pane says so instead of inviting a double click).
    pub update_busy: bool,
    /// #364: the policy face's account list (`defaults` first), each entry as
    /// the raw JSON the kernel answered with. Empty until the first refresh.
    pub policy_accounts: Vec<serde_json::Value>,
    /// #364: index into `policy_accounts` the ←/→ keys move (0 = defaults).
    pub policy_account_idx: usize,
    /// #364: the selected account's EFFECTIVE section (kernel-folded), the
    /// preview over its recent closed trades, and its audit history — all as
    /// the kernel answered (raw JSON; no second model on this side either).
    pub policy_section: Option<serde_json::Value>,
    pub policy_preview: Option<serde_json::Value>,
    pub policy_history: Vec<serde_json::Value>,
    /// #364: the condition builder's draft rule (the `[a]` form starts from
    /// the neutral draft; `[e]` on a rule row copies that rule in). Rendered
    /// and edited under the rule list; `[Enter]` in the builder commits it.
    pub policy_draft: Option<PolicyDraft>,
    /// #364: cursor row within the policy face (base params, then rules).
    /// 0..4 = the four base params; 5+ = rule rows (5 = rules[0], …).
    pub policy_focus: usize,
    /// #364: the policy editor sub-mode. `p` enters it on the Settings tab
    /// (and re-reads); inside, the policy keys own the keyboard — [a]/[e]/
    /// [d]/[空格]/[s]/[r] act on the policy and [q]/Esc RETURN out (the
    /// spec's `[q] return`), so the Settings tab's documented [a] auto-update
    /// and the global [r] refresh are untouched outside the mode.
    pub policy_mode: bool,
    /// #364: a write round-trip is in flight (the face says so instead of
    /// inviting a double press of `s`).
    pub policy_busy: bool,
    /// #364: confirmation state for the destructive rows (`[d]` delete,
    /// `[r]` rollback): the exact set/reset params, shown on the confirm bar.
    pub policy_pending: Option<(PolicyAction, serde_json::Value, String)>,
    /// 账户↔venue 解耦（本轮）：the `A` overlay's state. The kernel's own
    /// `account.list` reply, raw — same discipline as the policy face (the
    /// gateway adds no second model of the account book).
    pub accounts_doc: Option<serde_json::Value>,
    /// Cursor into the overlay's account rows (`accountsDoc.accounts[i]`).
    pub accounts_focus: usize,
    /// The overlay is visible (`A` toggles; Esc/A close). While it is up it
    /// takes the keys, exactly like the net-check overlay.
    pub accounts_visible: bool,
}

/// #364: one rule mid-edit — the builder's state. Field/op move with ←/→,
/// the value (and name/reason) type in with the input bar, so nothing is
/// free-form except what the kernel validates anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDraft {
    pub name: String,
    pub priority: u32,
    pub enabled: bool,
    /// Index into the eight FIELD spellings (see `PolicyDraft::FIELDS`).
    pub field_idx: usize,
    /// Index into the seven OP spellings (`PolicyDraft::OPS`).
    pub op_idx: usize,
    /// The raw value text (a number, a quoted symbol, or `A,B,C` for `in` —
    /// exactly the string grammar `parse_condition_value` accepts).
    pub value: String,
    /// Which then-action the rule carries (index into `PolicyDraft::THENS`).
    pub then_idx: usize,
    /// The then-action's own argument (ratio/budget as text, seconds as text).
    pub then_arg: String,
    pub reason: String,
    /// `Some(idx)` when editing the existing rule at that index (replace on
    /// commit); `None` when appending a new one.
    pub edit_idx: Option<usize>,
}

impl PolicyDraft {
    /// The eight condition fields (issue list, verbatim order) the ←/→ keys
    /// cycle. `symbol` is the one text field; the rest judge numbers.
    pub const FIELDS: [&'static str; 8] = [
        "available_balance",
        "total_equity",
        "open_positions",
        "current_price",
        "time_left_sec",
        "symbol",
        "recent_pnl_1h",
        "consecutive_losses",
    ];
    /// The seven operators (symbols as the kernel's string grammar spells
    /// them; `in` takes a comma-separated symbol list).
    pub const OPS: [&'static str; 7] = ["<", "<=", ">", ">=", "==", "!=", "in"];
    /// The closed then-vocabulary — exactly one per rule. Indices into the
    /// `then` build: 0 = skip, 1-3 = sizing overrides (arg = decimal), 4 =
    /// standing cooldown (arg = seconds).
    pub const THENS: [&'static str; 5] = [
        "skip",
        "budget_ratio",
        "min_budget_usd",
        "max_budget_usd",
        "cooldown_sec",
    ];

    /// The neutral draft: skip-everything-nothing — a rule that never fires
    /// (`open_positions < 0` is unsatisfiable) so a half-typed builder state
    /// can never be committed as something dangerous by accident.
    pub fn neutral() -> Self {
        Self {
            name: String::new(),
            priority: 100,
            enabled: true,
            field_idx: 2,
            op_idx: 0,
            value: "0".to_string(),
            then_idx: 0,
            then_arg: String::new(),
            reason: String::new(),
            edit_idx: None,
        }
    }

    pub fn field(&self) -> &'static str {
        Self::FIELDS[self.field_idx]
    }
    pub fn op(&self) -> &'static str {
        Self::OPS[self.op_idx]
    }
    pub fn then(&self) -> &'static str {
        Self::THENS[self.then_idx]
    }

    /// The rule as the kernel's `set` expects it in `rules[]` (table-form
    /// `when`, plain-value `value`, exactly-one-action `then`). Numbers stay
    /// unquoted; `symbol ==` and `in` carry text / a list. The kernel's
    /// validator has the final say — this only shapes what it is asked.
    pub fn to_rule_json(&self) -> Result<serde_json::Value, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("rule needs a name".to_string());
        }
        let field = self.field();
        let text_field = field == "symbol";
        let value: serde_json::Value = if self.op() == "in" {
            let items: Vec<String> = self
                .value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return Err("`in` needs at least one symbol (comma-separated)".to_string());
            }
            serde_json::json!(items)
        } else if text_field {
            let t = self.value.trim().trim_matches('"').to_string();
            if t.is_empty() {
                return Err("symbol condition needs a value".to_string());
            }
            serde_json::json!(t)
        } else {
            let t = self.value.trim();
            let n: f64 = t.parse().map_err(|_| format!("`{t}` is not a number"))?;
            serde_json::json!(n)
        };
        let then_name = self.then();
        let then = if then_name == "skip" {
            serde_json::json!({ "action": "skip" })
        } else {
            let arg = self.then_arg.trim();
            if arg.is_empty() {
                return Err(format!("{then_name} needs a value"));
            }
            // The kernel accepts the decimal/string spelling; send strings so
            // the shortest-decimal discipline survives the hop (cooldown is
            // whole seconds).
            if then_name == "cooldown_sec" {
                let secs: i64 = arg.parse().map_err(|_| format!("`{arg}` is not seconds"))?;
                serde_json::json!({ then_name: secs })
            } else {
                serde_json::json!({ then_name: arg })
            }
        };
        let mut rule = serde_json::json!({
            "name": name,
            "priority": self.priority,
            "enabled": self.enabled,
            "when": { "field": field, "op": self.op(), "value": value },
            "then": then,
        });
        let reason = self.reason.trim().to_string();
        if !reason.is_empty() {
            rule["reason"] = serde_json::json!(reason);
        }
        Ok(rule)
    }

    /// Build a draft from an existing rule row (the `[e]` path). The wire
    /// forms the kernel may send are accepted: op as symbol or snake_case,
    /// value as plain number / string / list, then carrying exactly one key.
    pub fn from_rule(rule: &serde_json::Value, edit_idx: Option<usize>) -> Result<Self, String> {
        let get = |k: &str| rule.get(k).cloned().unwrap_or(serde_json::Value::Null);
        let name = get("name").as_str().unwrap_or_default().to_string();
        let priority = get("priority").as_u64().unwrap_or(100) as u32;
        let enabled = get("enabled").as_bool().unwrap_or(true);
        let when = get("when");
        let field = when.get("field").and_then(|f| f.as_str()).unwrap_or("");
        let field_idx = Self::FIELDS
            .iter()
            .position(|f| *f == field)
            .ok_or_else(|| format!("unknown field `{field}`"))?;
        let op_raw = when.get("op").and_then(|o| o.as_str()).unwrap_or("");
        // The kernel sends snake_case (`ge`); accept the symbol spelling too.
        let op_canon = match op_raw {
            "lt" | "<" => "<",
            "le" | "<=" => "<=",
            "gt" | ">" => ">",
            "ge" | ">=" => ">=",
            "eq" | "==" => "==",
            "ne" | "!=" => "!=",
            "in" => "in",
            other => return Err(format!("unknown op `{other}`")),
        };
        let op_idx = Self::OPS
            .iter()
            .position(|o| *o == op_canon)
            .ok_or_else(|| format!("unknown op `{op_raw}`"))?;
        let v = when
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let value = if let Some(list) = v.as_array() {
            list.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(",")
        } else if let Some(s) = v.as_str() {
            s.to_string()
        } else if let Some(n) = v.as_f64() {
            format!("{n}")
        } else {
            String::new()
        };
        let then = get("then");
        // (idx, arg) for the then-block: skip carries no argument; the sizing
        // overrides carry the decimal as the string the kernel sent; cooldown
        // carries whole seconds.
        let then_pair = if then.get("action").and_then(|a| a.as_str()) == Some("skip") {
            (0usize, String::new())
        } else if let Some(arg) = then
            .get("budget_ratio")
            .or_else(|| then.get("min_budget_usd"))
            .or_else(|| then.get("max_budget_usd"))
            .and_then(|b| b.as_str())
        {
            let key = if then.get("budget_ratio").is_some() {
                1
            } else if then.get("min_budget_usd").is_some() {
                2
            } else {
                3
            };
            (key, arg.to_string())
        } else if let Some(secs) = then.get("cooldown_sec").and_then(|c| c.as_i64()) {
            (4, secs.to_string())
        } else {
            return Err("rule carries no then-action this editor knows".to_string());
        };
        Ok(Self {
            name,
            priority,
            enabled,
            field_idx,
            op_idx,
            value,
            then_idx: then_pair.0,
            then_arg: then_pair.1,
            reason: get("reason").as_str().unwrap_or_default().to_string(),
            edit_idx,
        })
    }
}
