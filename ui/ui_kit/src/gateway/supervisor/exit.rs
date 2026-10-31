//! Exit classification and the health view: what it means when an owned core stops, and what the UI reads.

use super::*;

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// Whether an owned core's exit was asked for.
///
/// The distinction is the whole point of E12(c): a UI that cannot tell "the
/// operator stopped it" from "it died" will either cry wolf on every clean stop
/// or stay silent through a crash. Classifying by *intent* rather than by exit
/// code is deliberate — a core that exits 0 because it lost a write still died
/// unexpectedly, and a core we SIGKILL after a refused SIGTERM must not be
/// reported as a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// We called `stop()`; the process ended as instructed.
    Clean,
    /// It ended while some part of the system still expected it to be running.
    Crashed,
}

/// Why an owned core is no longer running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    pub pid: u32,
    pub kind: ExitKind,
    /// Shell-style exit code, when it exited normally.
    pub code: Option<i32>,
    /// Terminating signal, when it was killed.
    pub signal: Option<i32>,
}

impl ExitReport {
    pub(crate) fn classify(pid: u32, status: ExitStatus, stopping: bool) -> Self {
        Self {
            pid,
            kind: if stopping {
                ExitKind::Clean
            } else {
                ExitKind::Crashed
            },
            code: status.code(),
            signal: status.signal(),
        }
    }

    /// One line an operator or a panel can show without interpreting fields.
    pub fn describe(&self) -> String {
        let how = match (self.code, self.signal) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(s)) => format!("killed by signal {s}"),
            (None, None) => "ended without a status".to_string(),
        };
        match self.kind {
            ExitKind::Clean => format!("core pid {} stopped ({how})", self.pid),
            ExitKind::Crashed => format!("core pid {} CRASHED ({how})", self.pid),
        }
    }
}

/// What the UI needs to answer "is the core up, and if not, why".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreHealth {
    /// A core is reachable on the socket right now (ours or adopted).
    pub running: bool,
    /// This supervisor spawned the running core.
    pub managed: bool,
    pub pid: Option<u32>,
    /// How many times an unexpected exit has been replaced with a new core.
    pub restarts: u32,
    /// Why the last owned core stopped. `None` until one of ours has ended.
    pub last_exit: Option<ExitReport>,
    /// The restart budget is spent; no further replacement will be attempted.
    pub restart_given_up: bool,
    /// Set once, at the moment the budget was spent. Carries the crash reason and
    /// the core's last stderr lines, so the panel can show WHY instead of only
    /// that it is over.
    pub alert: Option<GiveUpAlert>,
}
