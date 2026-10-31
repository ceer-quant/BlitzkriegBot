//! Split out of the parent module: pure move.
//! Lifecycle and ingest: construction, the bounded log buffer, and
//! applying core snapshots to the panel state.

use super::*;

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
            orders_focus: 0,
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
            accounts_doc: None,
            accounts_focus: 0,
            accounts_visible: false,
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
}
