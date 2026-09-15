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
                    if cmd.is_empty() {
                        return Action::None;
                    }
                    self.log(format!("> {cmd}"));
                    return Action::RunCommand(cmd);
                }
                KeyCode::Esc => {
                    self.input_active = false;
                    self.input.clear();
                    return Action::None;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    return Action::None;
                }
                _ => return Action::None,
            }
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.input_active = true;
                self.input.clear();
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
