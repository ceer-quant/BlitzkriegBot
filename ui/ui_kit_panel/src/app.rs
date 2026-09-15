//! Panel application state — pure data + key handling. Rendering is in `ui.rs`.

use blitzkrieg_ui_kit::UiSnapshot;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Positions,
    Trades,
    Plugins,
}

impl Tab {
    pub fn titles() -> Vec<&'static str> {
        vec!["1 Overview", "2 Positions", "3 Trades", "4 Plugins"]
    }
    pub fn index(self) -> usize {
        match self {
            Tab::Overview => 0,
            Tab::Positions => 1,
            Tab::Trades => 2,
            Tab::Plugins => 3,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Positions,
            Tab::Positions => Tab::Trades,
            Tab::Trades => Tab::Plugins,
            Tab::Plugins => Tab::Overview,
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
pub const HINTS: [&str; 6] = [
    "press : to type a command — try `status`",
    "1-5 switch pages (Overview/Positions/Trades/Plugins)",
    "? for the full key & command help",
    "r refresh now · q quit",
    "start with --manage to enable start/stop commands",
    "Plugins: ↑/↓ move · Enter toggle (dangerous toggles ask y/n)",
];

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
    /// Plugin-manager selection: 0=strategies pane column, then row index per pane.
    pub plugin_focus: usize,
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
    /// Non-empty while the core's kill switch is engaged — the body renders a
    /// full-screen red banner until `risk.resume` clears it.
    pub kill_banner: Option<String>,
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
            plugin_focus: 0,
            pending_confirmation: None,
            check: CheckStage::default(),
            hints_used: [false; HINTS.len()],
            history: Vec::new(),
            history_browse: None,
            help_visible: false,
            kill_banner: None,
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
                    return Action::RunCommand(text);
                }
                _ => {
                    self.log("toggle cancelled".to_string());
                    return Action::None;
                }
            }
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
            KeyCode::Char('?') => {
                self.help_visible = !self.help_visible;
                self.hints_used[2] = true;
                Action::None
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
            _ => Action::None,
        }
    }
}

/// The commands the bar completes against (longest-prefix, one candidate).
pub const COMMANDS: [&str; 9] = [
    "status",
    "positions",
    "strategy",
    "extension",
    "markets",
    "help",
    "start BTC,ETH,SOL,XRP --dry-run",
    "stop",
    "risk",
];

/// Tab-completion: when exactly one known command starts with the current
/// input, fill it; with several, fill their longest common prefix.
fn complete(input: &str) -> String {
    let t = input.trim_start_matches(':').trim();
    if t.is_empty() {
        return input.to_string();
    }
    let cands: Vec<&str> = COMMANDS
        .iter()
        .filter(|c| c.starts_with(t))
        .copied()
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

    /// True when the pending command must be confirmed before dispatch.
    pub fn toggle_needs_confirmation(&self, cmd: &str) -> bool {
        // Disabling anything (or enabling a strategy) swaps routing: disabling
        // a strategy may strand an open position; enabling a strategy starts it
        // placing orders. Only disabling is dangerous here.
        cmd.contains(" off") || cmd.contains("disable")
    }
}
