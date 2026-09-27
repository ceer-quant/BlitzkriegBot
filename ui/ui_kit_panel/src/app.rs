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
    /// Run the network self-check (`net.check`) off the render loop — the probe
    /// dials, so it answers in seconds, not milliseconds.
    NetCheck,
    /// `system.update.check` off the render loop (VERSIONING.md §7.4). The
    /// verdict travels in the next snapshot's `system_version`.
    UpdateCheck,
    /// `system.update.configure` for the AUTO switch (the checked side is only
    /// written by the operator's config or the configure verb itself).
    UpdateConfigure(bool),
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
                    return Action::RunCommand(text);
                }
                _ => {
                    self.log("toggle cancelled".to_string());
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
