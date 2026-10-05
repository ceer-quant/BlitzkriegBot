//! Panel application state — pure data + key handling. Rendering is in `ui.rs`.

use blitzkrieg_ui_kit::core::types::{EvolutionProposalView, NetCheckReportView};
use blitzkrieg_ui_kit::gateway::command_verbs;
use blitzkrieg_ui_kit::UiSnapshot;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Positions,
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
            "3 Trades",
            "4 Plugins",
            "5 Evolution",
            "6 Decisions",
            "7 Settings",
        ]
    }
    pub fn index(self) -> usize {
        match self {
            Tab::Overview => 0,
            Tab::Positions => 1,
            Tab::Trades => 2,
            Tab::Plugins => 3,
            Tab::Evolution => 4,
            Tab::Decisions => 5,
            Tab::Settings => 6,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Positions,
            Tab::Positions => Tab::Trades,
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
    /// #379 (§7.5): `system.update.stage` off the render loop — the kernel
    /// downloads + verifies the newer release into its staging directory and
    /// STOPS there; applying it is the launcher's job, never the kernel's
    /// (and never this panel's).
    UpdateStage,
    /// Run the LAUNCHER's install (`current_exe() update --install`): the
    /// panel process never replaces the kernel binary itself (P12) — it
    /// triggers the launcher-side installer and reports its output.
    UpdateInstall,
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

/// Cap the in-panel log so a long soak can't grow it without bound.
const LOG_CAP: usize = 500;

impl App {
    pub fn new(socket: String, lifecycle_enabled: bool) -> Self {
        Self {
            snap: UiSnapshot::default(),
            tab: Tab::Overview,
            input: String::new(),
            input_active: false,
            logs: Vec::new(),
            managed: false,
            pid: None,
            lifecycle_enabled,
            socket,
            should_quit: false,
            last_update: None,
            decisions: Vec::new(),
            plugin_focus: 0,
            evo_focus: 0,
            pending_confirmation: None,
            check: CheckStage::default(),
            hints_used: [false; HINTS.len()],
            history: Vec::new(),
            history_browse: None,
            help_visible: false,
            net_visible: false,
            net_report: None,
            net_error: None,
            net_busy: false,
            kill_banner: None,
            update_busy: false,
            policy_accounts: Vec::new(),
            policy_account_idx: 0,
            policy_section: None,
            policy_preview: None,
            policy_history: Vec::new(),
            policy_draft: None,
            policy_focus: 0,
            policy_mode: false,
            policy_busy: false,
            policy_pending: None,
        }
    }

    pub fn log(&mut self, line: impl Into<String>) {
        self.logs.push(line.into());
        if self.logs.len() > LOG_CAP {
            let drop = self.logs.len() - LOG_CAP;
            self.logs.drain(0..drop);
        }
    }

    pub fn on_snapshot(&mut self, snap: UiSnapshot, managed: bool, pid: Option<u32>) {
        self.snap = snap;
        self.managed = managed;
        self.pid = pid;
        self.last_update = Some(Instant::now());
        // Self-check progress: connected → handshake → feed motion.
        self.check = match self.check {
            CheckStage::Connecting if self.snap.connected => CheckStage::Handshake,
            CheckStage::Connecting => CheckStage::Connecting,
            stage => {
                let st = self.snap.stats.as_ref();
                if self.snap.connected
                    && st.is_some_and(|s| s.books > 0 || s.tops > 0 || s.rounds > 0)
                {
                    CheckStage::Ready
                } else {
                    stage
                }
            }
        };
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Pending dangerous-action confirmation intercepts everything but `y`.
        if let Some(text) = self.pending_confirmation.clone() {
            self.pending_confirmation = None; // one key resolves it either way
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.log(format!("> {text} (confirmed)"));
                    // The update switch is not a gateway command; it routes to
                    // the update verbs instead of the dispatcher.
                    if let Some(on) = text.strip_prefix(UPDATE_AUTO_CONFIRM_PREFIX) {
                        return Action::UpdateConfigure(on == "on");
                    }
                    // A confirmed policy write carries its params in the
                    // pending slot (the bar text is only the display name).
                    if let Some((action, _, _)) = self.policy_pending.take() {
                        return Action::PolicyEdit { action };
                    }
                    return Action::RunCommand(text);
                }
                _ => {
                    self.log("toggle cancelled".to_string());
                    self.policy_pending = None;
                    return Action::None;
                }
            }
        }
        // The network self-check overlay sits on top of everything: while it is
        // up only `n`/Esc (close), `r` (probe again) and `q` act. A full report
        // the operator is reading must not be switched out from under them by a
        // stray digit, and `n` is otherwise unused at this level.
        if self.net_visible {
            return match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.net_visible = false;
                    Action::None
                }
                KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.net_busy = true;
                    Action::NetCheck
                }
                _ => Action::None,
            };
        }
        // Command bar owns input while focused.
        if self.input_active {
            match key.code {
                KeyCode::Enter => {
                    let cmd = self.input.trim().to_string();
                    self.input.clear();
                    self.input_active = false;
                    self.history_browse = None;
                    if cmd.is_empty() {
                        return Action::None;
                    }
                    self.hints_used[0] = true; // command bar consumed
                    self.history.push(cmd.clone());
                    self.log(format!("> {cmd}"));
                    return Action::RunCommand(cmd);
                }
                KeyCode::Esc => {
                    self.input_active = false;
                    self.input.clear();
                    self.history_browse = None;
                    return Action::None;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    self.history_browse = None;
                    return Action::None;
                }
                KeyCode::Up => {
                    // Recall: walk backward through executed commands.
                    if !self.history.is_empty() {
                        let n = self.history.len();
                        let off = self.history_browse.unwrap_or(0).min(n - 1);
                        let off = self.history_browse.replace(off).map(|_| off).unwrap_or(0);
                        let idx = n - 1 - off;
                        self.input = self.history[idx].clone();
                        let next = (off + 1).min(n);
                        self.history_browse = Some(next);
                    }
                    return Action::None;
                }
                KeyCode::Down => {
                    // Forward through history; past the end clears the line.
                    if let Some(off) = self.history_browse {
                        let n = self.history.len();
                        match off.checked_sub(1) {
                            Some(next_off) => {
                                self.history_browse = Some(next_off);
                                let idx = n - 1 - next_off;
                                self.input = self.history[idx].clone();
                            }
                            None => {
                                self.history_browse = None;
                                self.input.clear();
                            }
                        }
                    }
                    return Action::None;
                }
                KeyCode::Tab => {
                    self.input = complete(&self.input);
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    self.history_browse = None;
                    if c == '?' && self.input.trim() == "?" {
                        // `?` as the first thing typed opens help instead.
                        self.input.clear();
                        self.input_active = false;
                        self.help_visible = true;
                    }
                    return Action::None;
                }
                _ => return Action::None,
            }
        }

        // #364: the policy editor's sub-mode owns the keyboard while active —
        // it must sit BEFORE the global arms, or `q` would quit the panel and
        // `r` would refresh the snapshot instead of rolling the policy back.
        // Inside the mode: ←/→ account · ↑/↓ cursor · [e] edit · [a] add ·
        // [d] delete (asks) · [空格] on/off (asks) · [s] save (asks) · [r]
        // rollback (asks) · [q]/Esc RETURN out (the spec's `[q] return`).
        // Outside the mode the Settings tab's documented [a] auto-update and
        // the global [r] refresh keep their meaning — the mode is opt-in.
        if self.policy_mode {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
                    if self.policy_pending.is_none() =>
                {
                    self.policy_mode = false;
                    self.policy_draft = None;
                    self.policy_focus = 0;
                    Action::None
                }
                KeyCode::Left if !self.policy_busy => {
                    self.policy_account_idx = self.policy_account_idx.saturating_sub(1);
                    self.policy_focus = 0;
                    Action::PolicyRefresh
                }
                KeyCode::Right if !self.policy_busy => {
                    // The chips are `defaults` + one per list entry: the cap
                    // is the list LENGTH (index 0 is defaults, so the last
                    // valid index is len, not len-1).
                    if self.policy_account_idx < self.policy_accounts.len() {
                        self.policy_account_idx += 1;
                    }
                    self.policy_focus = 0;
                    Action::PolicyRefresh
                }
                KeyCode::Up if self.policy_draft.is_none() => {
                    self.policy_focus = self.policy_focus.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down if self.policy_draft.is_none() => {
                    self.policy_focus = self.policy_focus.saturating_add(1);
                    Action::None
                }
                KeyCode::Char(c)
                    if self.policy_draft.is_none()
                        && !self.policy_busy
                        && matches!(c, 'e' | 'a' | 'd' | 's' | 'r' | ' ') =>
                {
                    self.policy_key(c)
                }
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            // Esc closes whichever overlay is up. The help panel has always
            // advertised "Esc to close" in its title, so this is the key that
            // makes that true as well as the one that closes the net-check.
            KeyCode::Esc if self.help_visible || self.net_visible => {
                self.help_visible = false;
                self.net_visible = false;
                Action::None
            }
            KeyCode::Char('?') => {
                self.help_visible = !self.help_visible;
                self.hints_used[2] = true;
                Action::None
            }
            // Network self-check: `n` opens the diagnosis overlay and probes on
            // the first press; afterwards it shows the report already held, and
            // `r` re-probes. The probe dials, so it never runs on the render
            // loop — the action hands it to a worker thread.
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.net_visible = true;
                self.hints_used[6] = true;
                if self.net_busy {
                    Action::None
                } else if self.net_report.is_none() {
                    self.net_busy = true;
                    Action::NetCheck
                } else {
                    Action::None
                }
            }
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.input_active = true;
                self.input.clear();
                self.history_browse = None;
                Action::None
            }
            KeyCode::Char('r') | KeyCode::Char('R') => Action::Refresh,
            KeyCode::Char('1') => {
                self.tab = Tab::Overview;
                Action::None
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Positions;
                Action::None
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Trades;
                Action::None
            }
            KeyCode::Char('4') => {
                self.tab = Tab::Plugins;
                Action::RefreshPlugins
            }
            KeyCode::Char('5') => {
                self.tab = Tab::Evolution;
                Action::None // the 1 s poller re-fetches proposals with the snapshot
            }
            KeyCode::Char('6') => {
                self.tab = Tab::Settings;
                Action::None // the snapshot poller carries system_version
            }
            // #364: `p` on the Settings tab enters the policy editor's
            // sub-mode (its keys and the [q] return are handled above).
            KeyCode::Char('p') if self.tab == Tab::Settings => {
                self.policy_mode = true;
                self.policy_focus = 0;
                Action::PolicyRefresh
            }
            // Settings keys (VERSIONING.md §6.2). `a` is the one switch that
            // decides "may the launcher auto-replace binaries later", so it
            // always asks — nothing flips silently.
            KeyCode::Char('c') if self.tab == Tab::Settings && !self.update_busy => {
                if self.snap.system_version.as_ref().map(|v| v.check_enabled) == Some(false) {
                    self.log(
                        "update check is disabled — enable it in user_layer/configs/update.toml \
                         or the WebUI settings first"
                            .to_string(),
                    );
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateCheck
                }
            }
            KeyCode::Char('a') if self.tab == Tab::Settings && !self.update_busy => {
                let on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                self.pending_confirmation = Some(format!(
                    "{UPDATE_AUTO_CONFIRM_PREFIX}{}",
                    if on { "off" } else { "on" }
                ));
                Action::None
            }
            // #379: stage the newer release (download + verify, §7.5). Like
            // `c`, it is refused up front when the governing switch is off —
            // and staging additionally requires an Available verdict.
            KeyCode::Char('s') if self.tab == Tab::Settings && !self.update_busy => {
                let auto_on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                if !auto_on {
                    self.log(
                        "staging needs auto-update ON (press a first) — the kernel downloads                          into data/update/staging/ and never installs on its own"
                            .to_string(),
                    );
                    Action::None
                } else if self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.update_available)
                    != Some(Some(true))
                {
                    self.log("nothing to stage — check first; staging follows an                               'available' verdict"
                        .to_string());
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateStage
                }
            }
            KeyCode::Char('i') if self.tab == Tab::Settings && !self.update_busy => {
                let auto_on = self
                    .snap
                    .system_version
                    .as_ref()
                    .map(|v| v.auto_update)
                    .unwrap_or(false);
                if !auto_on {
                    self.log(
                        "install needs auto-update ON (press a first) — the switch is OFF by \
                         default and the kernel never installs on its own"
                            .to_string(),
                    );
                    Action::None
                } else {
                    self.update_busy = true;
                    Action::UpdateInstall
                }
            }
            KeyCode::Tab => {
                let was_plugins = self.tab == Tab::Plugins;
                self.tab = self.tab.next();
                if self.tab == Tab::Plugins && !was_plugins {
                    Action::RefreshPlugins
                } else {
                    Action::None
                }
            }
            KeyCode::Up if self.tab == Tab::Plugins => {
                self.plugin_focus = self.plugin_focus.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if self.tab == Tab::Plugins => {
                self.plugin_focus = self.plugin_focus.saturating_add(1);
                Action::None
            }
            KeyCode::Enter if self.tab == Tab::Plugins => match self.plugin_toggle_command() {
                Some(cmd) => Action::ConfirmToggle(cmd),
                None => {
                    self.log("no toggleable row selected (use ↑/↓ in the Plugins tab)".to_string());
                    Action::None
                }
            },
            KeyCode::Up if self.tab == Tab::Evolution => {
                self.evo_focus = self.evo_focus.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if self.tab == Tab::Evolution => {
                self.evo_focus = self.evo_focus.saturating_add(1);
                Action::None
            }
            KeyCode::Char('a') if self.tab == Tab::Evolution => self.evo_decide("accept"),
            KeyCode::Char('x') if self.tab == Tab::Evolution => self.evo_decide("reject"),
            KeyCode::Char('d') if self.tab == Tab::Evolution => self.evo_decide("defer"),
            KeyCode::Char('e') if self.tab == Tab::Evolution => self.evo_toggle_auto(),
            KeyCode::Char('m') if self.tab == Tab::Evolution => self.evo_toggle_engine(),
            KeyCode::Char('u') if self.tab == Tab::Evolution => self.evo_rollback(),
            _ => Action::None,
        }
    }

    /// #364: the Settings-tab policy keys, dispatched from `on_key` only when
    /// the face owns the key (right tab, no draft open, no write in flight).
    /// Each arm mirrors one requirement: [e] edit, [a] add rule, [d] delete
    /// (asks), [↑/↓] priority, [空格] enable/disable, [s] save, [r] rollback.
    fn policy_key(&mut self, c: char) -> Action {
        let Some(account) = self.policy_account_id() else {
            self.log("policy: no account list yet (still loading)".to_string());
            return Action::None;
        };
        let rules_len = self
            .policy_section
            .as_ref()
            .and_then(|s| s.get("rules"))
            .and_then(|r| r.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let on_rule = self.policy_focus >= 5 && self.policy_focus - 5 < rules_len;
        match c {
            'e' => {
                if self.policy_focus < 4 {
                    self.log(
                        "param edit: use the WebUI for base params — here rules are first-class \
                         (the four numbers are shown for reference)"
                            .to_string(),
                    );
                } else if on_rule {
                    let idx = self.policy_focus - 5;
                    let rule = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .and_then(|a| a.get(idx))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    match PolicyDraft::from_rule(&rule, Some(idx)) {
                        Ok(d) => {
                            self.policy_draft = Some(d);
                        }
                        Err(e) => self.log(format!("policy: cannot edit that rule: {e}")),
                    }
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                }
                Action::None
            }
            'a' => {
                self.policy_draft = Some(PolicyDraft::neutral());
                Action::None
            }
            'd' => {
                if on_rule {
                    let idx = self.policy_focus - 5;
                    let mut rules = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    rules.remove(idx);
                    self.ask_policy_edit(
                        PolicyAction::Set,
                        self.policy_set_params_with_rules(rules),
                        format!("delete rule #{}, account {account}", idx + 1),
                    )
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                    Action::None
                }
            }
            ' ' => {
                if on_rule {
                    let idx = self.policy_focus - 5;
                    let mut rules = self
                        .policy_section
                        .as_ref()
                        .and_then(|s| s.get("rules"))
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let on = rules[idx]
                        .get("enabled")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    rules[idx]["enabled"] = serde_json::json!(!on);
                    self.ask_policy_edit(
                        PolicyAction::Set,
                        self.policy_set_params_with_rules(rules),
                        format!(
                            "{} rule #{}, account {account}",
                            if on { "disable" } else { "enable" },
                            idx + 1
                        ),
                    )
                } else {
                    self.log("no rule row selected (↑/↓ to move onto a rule)".to_string());
                    Action::None
                }
            }
            's' => {
                let params = self.policy_set_params_from_view();
                self.ask_policy_edit(
                    PolicyAction::Set,
                    params,
                    format!("save policy, account {account}"),
                )
            }
            'r' => {
                // Rollback = take the account's own most recent `set`'s
                // BEFORE-state and write it back via `set` (no new backend
                // method: the journal IS the version store).
                match self.policy_rollback_params() {
                    Ok((label, params)) => self.ask_policy_edit(PolicyAction::Set, params, label),
                    Err(e) => {
                        self.log(format!("rollback: {e}"));
                        Action::None
                    }
                }
            }
            _ => Action::None,
        }
    }

    /// The account id under the ←/→ cursor (`"defaults"` for index 0).
    /// The account id under the ←/→ cursor (`"defaults"` for index 0; the
    /// list entries start at chip index 1 = `policy_accounts[0]`).
    pub fn policy_account_id(&self) -> Option<String> {
        if self.policy_account_idx == 0 {
            return Some("defaults".to_string());
        }
        self.policy_accounts
            .get(self.policy_account_idx - 1)
            .and_then(|v| {
                v.get("accountId")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string())
            })
    }

    /// Whether the confirmed `policy.` prefix was armed (used by `on_key`).
    pub fn policy_confirm_armed(&self) -> bool {
        self.policy_pending.is_some()
    }

    /// Arm a policy write: the confirm bar shows the label; `y` fires
    /// [`Action::PolicyEdit`] with the action (params ride along in the
    /// pending slot). Cancel clears both.
    fn ask_policy_edit(
        &mut self,
        action: PolicyAction,
        params: serde_json::Value,
        label: String,
    ) -> Action {
        let verb = match action {
            PolicyAction::Set => "set",
            PolicyAction::Reset => "reset",
        };
        self.policy_pending = Some((action, params, label.clone()));
        self.pending_confirmation = Some(format!("{POLICY_CONFIRM_PREFIX}{verb} {label}"));
        Action::None
    }

    /// The set params for "write the CURRENT effective view back": budget
    /// triple + equity + cap + rules, taken from the section the kernel last
    /// answered with (decimals cross as the strings it sent).
    fn policy_set_params_from_view(&self) -> serde_json::Value {
        let account = self.policy_account_id().unwrap_or_default();
        let s = self
            .policy_section
            .clone()
            .unwrap_or(serde_json::Value::Null);
        let mut params = serde_json::json!({ "accountId": account });
        for (wire, key) in [
            ("budgetRatio", "budget_ratio"),
            ("minBudgetUsd", "min_budget_usd"),
            ("maxBudgetUsd", "max_budget_usd"),
            ("minEquityUsd", "min_equity_usd"),
        ] {
            if let Some(v) = s.get(key).and_then(|v| v.as_str()) {
                params[wire] = serde_json::json!(v);
            }
        }
        if let Some(n) = s.get("max_positions_per_asset").and_then(|v| v.as_u64()) {
            params["maxPositionsPerAsset"] = serde_json::json!(n);
        }
        if let Some(rules) = s.get("rules").and_then(|r| r.as_array()) {
            params["rules"] = serde_json::json!(rules);
        }
        params
    }

    /// Same as above but with the rules replaced (the [d]/[空格] arms).
    fn policy_set_params_with_rules(&self, rules: Vec<serde_json::Value>) -> serde_json::Value {
        let mut params = self.policy_set_params_from_view();
        params["rules"] = serde_json::json!(rules);
        params
    }

    /// The rollback write: find the account's most recent `set` audit line
    /// and return its BEFORE-state as the params to write back. No new
    /// backend method — the journal already holds each version.
    fn policy_rollback_params(&self) -> Result<(String, serde_json::Value), String> {
        let account = self.policy_account_id().unwrap_or_default();
        let last_set = self
            .policy_history
            .iter()
            .rfind(|l| l.get("action").and_then(|a| a.as_str()) == Some("set"))
            .ok_or_else(|| format!("no prior version to roll back to for `{account}`"))?;
        let before = last_set
            .get("before")
            .cloned()
            .filter(|b| !b.is_null())
            .ok_or_else(|| "the last audit line carries no before-state".to_string())?;
        let ts = last_set.get("tsMs").and_then(|t| t.as_i64()).unwrap_or(0);
        let mut params = serde_json::json!({ "accountId": account });
        for (wire, key) in [
            ("budgetRatio", "budget_ratio"),
            ("minBudgetUsd", "min_budget_usd"),
            ("maxBudgetUsd", "max_budget_usd"),
            ("minEquityUsd", "min_equity_usd"),
        ] {
            if let Some(v) = before.get(key).and_then(|v| v.as_str()) {
                params[wire] = serde_json::json!(v);
            }
        }
        if let Some(n) = before
            .get("max_positions_per_asset")
            .and_then(|v| v.as_u64())
        {
            params["maxPositionsPerAsset"] = serde_json::json!(n);
        }
        if let Some(rules) = before.get("rules").and_then(|r| r.as_array()) {
            params["rules"] = serde_json::json!(rules);
        }
        Ok((
            format!("roll back account {account} to the version before {ts}"),
            params,
        ))
    }
}

/// The commands the bar completes against (longest-prefix, one candidate): the
/// gateway's own verbs, so a command it would reject can never be offered.
/// `start` completes to a filled-in example because its arguments are the point
/// of the line.
fn candidates() -> Vec<&'static str> {
    command_verbs()
        .map(|verb| match verb {
            "start" => "start BTC,ETH,SOL,XRP --dry-run",
            other => other,
        })
        .collect()
}

/// Tab-completion: when exactly one known command starts with the current
/// input, fill it; with several, fill their longest common prefix.
fn complete(input: &str) -> String {
    let t = input.trim_start_matches(':').trim();
    if t.is_empty() {
        return input.to_string();
    }
    let cands: Vec<&str> = candidates()
        .into_iter()
        .filter(|c| c.starts_with(t))
        .collect();
    match cands.first() {
        None => input.to_string(),
        Some(first) => {
            let shared = cands.iter().fold(first.to_string(), |acc: String, c| {
                common_prefix(&acc, c).to_string()
            });
            shared
        }
    }
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    std::str::from_utf8(&a[..i]).unwrap_or("")
}

impl App {
    /// Rows in the Plugins tab the cursor can land on: one row per strategy and
    /// one per extension; market plugins are read-only.
    pub fn plugin_row_count(&self) -> usize {
        self.snap.strategies.len() + self.snap.extensions.len()
    }

    /// The command the cursor currently points at, if any. Toggling an ENABLED
    /// entry off needs confirmation (it can stop a strategy that is holding an
    /// open position); enabling is read-safe so it goes straight through.
    pub fn plugin_toggle_command(&self) -> Option<String> {
        let rows = self.plugin_row_count();
        if rows == 0 {
            return None;
        }
        let i = self.plugin_focus.min(rows - 1);
        if i < self.snap.strategies.len() {
            let s = &self.snap.strategies[i];
            Some(format!(
                "strategy {} {}",
                s.name,
                if s.enabled { "off" } else { "on" }
            ))
        } else {
            let e = &self.snap.extensions[i - self.snap.strategies.len()];
            let enable = e.state != "enabled";
            if enable {
                Some(format!("extension {} on", e.name))
            } else {
                Some(format!("extension {} off", e.name))
            }
        }
    }

    /// Pending proposals in display order (the cursor indexes into this list;
    /// `ui.rs` renders the same list, so cursor and screen stay in step).
    pub fn evo_pending(&self) -> Vec<&EvolutionProposalView> {
        self.snap
            .evolution_proposals
            .iter()
            .filter(|p| p.is_pending())
            .collect()
    }

    /// The pending proposal under the cursor, if the list is not empty.
    fn evo_selected(&self) -> Option<&EvolutionProposalView> {
        let pending = self.evo_pending();
        if pending.is_empty() {
            return None;
        }
        let i = self.evo_focus.min(pending.len() - 1);
        Some(pending[i])
    }

    /// accept / reject / defer the proposal under the cursor. Accepting
    /// hot-swaps live strategy parameters — routed through the confirm bar
    /// (same bar as a dangerous plugin toggle); reject/defer move nothing and
    /// go straight through.
    fn evo_decide(&mut self, decision: &str) -> Action {
        let Some(p) = self.evo_selected() else {
            self.log("no pending evolution proposal to decide (nothing waiting)".to_string());
            return Action::None;
        };
        let cmd = format!("decide {} {}", p.id, decision);
        if decision == "accept" {
            Action::ConfirmToggle(cmd)
        } else {
            self.log(format!("> {cmd}"));
            Action::RunCommand(cmd)
        }
    }

    /// Rollback the last accepted promotion for the strategy the cursor points
    /// at (undo restores the previous params live). Dangerous — confirm first.
    fn evo_rollback(&mut self) -> Action {
        let Some(p) = self.evo_selected() else {
            self.log(
                "rollback needs a selection: point ↑/↓ at a pending proposal to name a strategy"
                    .to_string(),
            );
            return Action::None;
        };
        let strategy = p.strategy.clone();
        if strategy.is_empty() {
            self.log("that proposal carries no strategy name — cannot roll back".to_string());
            return Action::None;
        }
        Action::ConfirmToggle(format!("rollback {strategy}"))
    }

    /// Flip the auto-evolve switch from the status the last snapshot carried.
    fn evo_toggle_auto(&mut self) -> Action {
        let on = self
            .snap
            .evolution_status
            .as_ref()
            .map(|s| s.auto_evolve)
            .unwrap_or(false);
        let cmd = format!("auto-evolve {}", if on { "off" } else { "on" });
        self.log(format!("> {cmd}"));
        Action::RunCommand(cmd)
    }

    /// #249: the engine switch — whether anything evolves at all. A different
    /// question from the auto switch (who applies what qualifies), and the one
    /// the page has to answer before "auto-evolve ON" means anything.
    fn evo_toggle_engine(&mut self) -> Action {
        let on = self
            .snap
            .evolution_status
            .as_ref()
            .map(|s| s.enabled)
            .unwrap_or(false);
        let cmd = format!("evolve {}", if on { "off" } else { "on" });
        self.log(format!("> {cmd}"));
        Action::RunCommand(cmd)
    }

    /// True when the pending command must be confirmed before dispatch.
    pub fn toggle_needs_confirmation(&self, cmd: &str) -> bool {
        // Disabling anything swaps routing (may strand an open position);
        // accepting an evolution proposal swaps live strategy parameters;
        // rollback reverts them. Those three are the dangerous verbs here.
        cmd.contains(" off")
            || cmd.contains("disable")
            || cmd.contains(" accept")
            || cmd.starts_with("rollback ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// A settings face with one configured account and one rule, as the
    /// kernel's `list`/`get`/`history` answer them (raw wire shapes).
    fn app_with_policy() -> App {
        let mut app = App::new("test.sock".into(), false);
        app.on_key(key('6'));
        // Enter the policy sub-mode the way the operator does: `p`.
        app.on_key(key('p'));
        app.policy_accounts = vec![serde_json::json!({
            "accountId": "acct-a",
            "section": {}
        })];
        // The seeded section/history below belong to acct-a: move the chip
        // cursor onto it (chip 1 = the list's first entry).
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.policy_section = Some(serde_json::json!({
            "budget_ratio": "0.10",
            "min_budget_usd": "1.00",
            "max_budget_usd": "50.00",
            "min_equity_usd": "1.00",
            "max_positions_per_asset": 1,
            "rules": [{
                "name": "no_btc",
                "priority": 10,
                "enabled": true,
                "when": { "field": "symbol", "op": "eq", "value": "BTC" },
                "then": { "action": "skip" },
                "reason": "no bitcoin"
            }]
        }));
        app.policy_history = vec![serde_json::json!({
            "tsMs": 1,
            "actor": "ipc",
            "action": "set",
            "accountId": "acct-a",
            "before": { "budget_ratio": "0.20", "max_positions_per_asset": 1 },
            "after": { "budget_ratio": "0.10", "max_positions_per_asset": 1 }
        })];
        app
    }

    /// #364: `6` enters Settings and demands a policy refresh; ←/→ walk the
    /// account chips (defaults first, then the kernel's list) and re-read.
    #[test]
    fn settings_tab_policy_refresh_and_account_switch() {
        let mut app = App::new("test.sock".into(), false);
        // Plain `6` does NOT open the editor (the tab keeps its own keys).
        assert!(matches!(app.on_key(key('6')), Action::None));
        assert!(!app.policy_mode);
        // `p` enters the policy sub-mode and demands a refresh.
        assert!(matches!(app.on_key(key('p')), Action::PolicyRefresh));
        assert!(app.policy_mode);
        assert_eq!(app.policy_account_id().as_deref(), Some("defaults"));
        // The kernel's list arrives: one configured account.
        app.policy_accounts = vec![serde_json::json!({ "accountId": "acct-a" })];
        // → lands on acct-a; the id rides the list.
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.policy_account_id().as_deref(), Some("acct-a"));
        // ← back to defaults; ← again saturates (no wraparound into nothing).
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.policy_account_id().as_deref(), Some("defaults"));
        // [q] returns out of the sub-mode (the spec's `[q] return`).
        app.on_key(key('q'));
        assert!(!app.policy_mode);
        assert_eq!(app.tab, Tab::Settings);
        // …and the tab's own [a] (auto-update) is reachable again: it asks.
        app.on_key(key('a'));
        assert_eq!(
            app.pending_confirmation.as_deref(),
            Some("update.auto on"),
            "outside the mode, [a] is the auto-update switch again"
        );
    }

    /// [e] on a rule row opens the builder seeded from that rule; [a] starts
    /// a neutral draft; the draft commits to a valid `rules[]` element the
    /// kernel's own parser will accept (table-form when, one-action then).
    #[test]
    fn rule_edit_and_draft_commit() {
        let mut app = app_with_policy();
        // Focus onto the rule row (rules start at focus 5).
        app.policy_focus = 5;
        assert!(matches!(app.on_key(key('e')), Action::None));
        let draft = app.policy_draft.clone().expect("draft opened");
        assert_eq!(draft.name, "no_btc");
        assert_eq!(draft.field(), "symbol");
        assert_eq!(draft.value, "BTC");
        assert_eq!(draft.edit_idx, Some(0));
        // A fresh draft commits to a shape the kernel validates.
        app.policy_draft = Some(PolicyDraft::neutral());
        let mut d = app.policy_draft.clone().unwrap();
        d.name = "cap_ratio".into();
        d.field_idx = PolicyDraft::FIELDS
            .iter()
            .position(|f| *f == "available_balance")
            .unwrap();
        d.op_idx = PolicyDraft::OPS.iter().position(|o| *o == "<").unwrap();
        d.value = "10".into();
        d.then_idx = PolicyDraft::THENS
            .iter()
            .position(|t| *t == "budget_ratio")
            .unwrap();
        d.then_arg = "0.05".into();
        d.edit_idx = None;
        let rule = d.to_rule_json().expect("draft renders");
        assert_eq!(rule["when"]["op"], "<");
        assert_eq!(rule["then"]["budget_ratio"], "0.05");
        assert_eq!(rule["priority"], 100);
        // A nameless draft refuses to render — the kernel would refuse it too.
        let mut bad = d.clone();
        bad.name = String::new();
        assert!(bad.to_rule_json().is_err());
    }

    /// [d] and [空格] must ASK first: the confirm bar names the change, `y`
    /// routes it to `PolicyEdit` with the params built at ask time.
    #[test]
    fn delete_and_toggle_ask_before_writing() {
        let mut app = app_with_policy();
        app.policy_focus = 5;
        assert!(matches!(app.on_key(key('d')), Action::None));
        assert!(app.pending_confirmation.is_some(), "delete asks y/n");
        let (action, params, _) = app.policy_pending.clone().expect("pending armed");
        assert_eq!(action, PolicyAction::Set);
        assert_eq!(params["rules"].as_array().map(|a| a.len()), Some(0));
        // Any non-`y` cancels and clears the armed write.
        app.on_key(key('n'));
        assert!(app.pending_confirmation.is_none());
        assert!(app.policy_pending.is_none());

        // [空格] flips `enabled` in the staged params (not yet on disk).
        assert!(matches!(app.on_key(key(' ')), Action::None));
        let (_, params, _) = app.policy_pending.clone().expect("toggle armed");
        assert_eq!(params["rules"][0]["enabled"], serde_json::json!(false));
        app.on_key(key('n'));
        assert!(app.policy_pending.is_none());
    }

    /// [r] rollback writes the most recent `set`'s BEFORE-state back through
    /// `set` — no new backend verb, the journal IS the version store.
    #[test]
    fn rollback_takes_the_last_set_before_state() {
        let mut app = app_with_policy();
        assert!(matches!(app.on_key(key('r')), Action::None));
        let (_, params, label) = app.policy_pending.clone().expect("rollback armed");
        assert_eq!(params["accountId"], serde_json::json!("acct-a"));
        assert_eq!(
            params["budgetRatio"],
            serde_json::json!("0.20"),
            "the before-state"
        );
        assert!(label.contains("roll back"));
        // No `set` in the journal → a refusal with a reason, never a write.
        let mut bare = app_with_policy();
        bare.policy_history.clear();
        assert!(matches!(bare.on_key(key('r')), Action::None));
        assert!(bare.pending_confirmation.is_none());
        assert!(bare
            .logs
            .last()
            .is_some_and(|l| l.contains("no prior version")));
    }

    /// [s] save stages the CURRENT effective view as the write (visible round
    /// trip: the face re-reads after the write lands).
    #[test]
    fn save_stages_the_effective_view() {
        let mut app = app_with_policy();
        assert!(matches!(app.on_key(key('s')), Action::None));
        let (action, params, _) = app.policy_pending.clone().expect("save armed");
        assert_eq!(action, PolicyAction::Set);
        assert_eq!(params["accountId"], serde_json::json!("acct-a"));
        assert_eq!(params["budgetRatio"], serde_json::json!("0.10"));
        assert_eq!(params["rules"][0]["name"], serde_json::json!("no_btc"));
    }

    /// With no rule row under the cursor, the row keys say so instead of
    /// guessing (the cursor is the only index the face trusts).
    #[test]
    fn row_keys_need_a_rule_row() {
        let mut app = app_with_policy();
        app.policy_focus = 0; // base-param rows, not a rule
        for c in ['e', 'd', ' '] {
            assert!(matches!(app.on_key(key(c)), Action::None));
            assert!(app.pending_confirmation.is_none());
            assert!(app.policy_draft.is_none());
        }
    }
}
