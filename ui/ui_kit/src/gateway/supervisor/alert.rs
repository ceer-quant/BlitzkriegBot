//! The give-up alert: raised once when the restart budget is spent, on every channel an operator might watch.

use super::*;

use std::process::{Command, Stdio};

/// Opt-in: `<prefix>_ALERT_NOTIFY=1` also pushes the give-up alert to the
/// desktop notifier (`osascript` / `notify-send`). Off by default — a supervisor
/// should not pop windows on a machine that never asked for them — but a
/// keep-alive wrapper on an unattended box wants exactly this.
const ENV_ALERT_NOTIFY: &str = "BLITZKRIEG_ALERT_NOTIFY";

/// The supervisor has stopped trying to keep the core up.
///
/// This exists because the alternative was silent: the restart budget ran out,
/// `restart_given_up` flipped to `true` in a status struct, and an unattended
/// deployment sat with a dead kernel and no notification anywhere. The alert is
/// produced exactly once per give-up, carries the last exit AND the tail of the
/// core's own stderr (the only place the reason for the crash is written), and is
/// delivered on three channels: our stderr banner, [`AlertSink`] (whatever the
/// embedding gateway already uses for alerts), and [`CoreHealth::alert`] — which
/// is what the panel renders as "已放弃重启".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiveUpAlert {
    /// Replacement attempts made before giving up.
    pub attempts: u32,
    /// Why the last owned core stopped.
    pub last_exit: Option<ExitReport>,
    /// Unix milliseconds, so a panel can show when this happened.
    pub at_ms: i64,
    /// Last lines the core wrote to stderr before it died.
    pub stderr_tail: Vec<String>,
    /// Operator-readable one-liner.
    pub message: String,
}

impl GiveUpAlert {
    /// The multi-line banner written to the gateway's own stderr (and thus the
    /// run log). Loud on purpose: this is the log line an operator greps for
    /// after finding a dead core.
    pub fn banner(&self) -> String {
        let mut out = format!(
            "\n=== BLITZKRIEG ALERT: core NOT running — supervisor gave up ===\n\
             {}\n\
             attempts={} last_exit={}\n",
            self.message,
            self.attempts,
            self.last_exit
                .as_ref()
                .map(ExitReport::describe)
                .unwrap_or_else(|| "no recorded exit".to_string()),
        );
        if self.stderr_tail.is_empty() {
            out.push_str("the core wrote nothing to stderr before it died\n");
        } else {
            out.push_str("last stderr from the core:\n");
            for line in &self.stderr_tail {
                out.push_str("  | ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(
            "the kernel is DOWN and will not be restarted; trading is stopped until it is \
             started again\n===\n",
        );
        out
    }
}

/// Extra delivery channel for a give-up alert.
///
/// The supervisor is deliberately transport-free — it does not know about the
/// panel, the event bus or a webhook — so a gateway that already has an alert
/// channel installs one sink and every give-up reaches it. Implementations must
/// not block: this is called from the UI's refresh tick.
pub trait AlertSink: Send + Sync {
    fn give_up(&self, alert: &GiveUpAlert);
}

/// The always-on channels: a loud stderr banner, plus the desktop notifier when
/// the operator opted in with `<prefix>_ALERT_NOTIFY=1`.
pub(crate) fn emit_give_up(alert: &GiveUpAlert) {
    eprint!("{}", alert.banner());
    if std::env::var(ENV_ALERT_NOTIFY)
        .map(|v| v != "false")
        .unwrap_or(false)
    {
        desktop_notify(alert);
    }
}

/// Best-effort OS notification. Fire-and-forget on a thread so the supervisor's
/// refresh tick never waits on a notifier that may not exist.
fn desktop_notify(alert: &GiveUpAlert) {
    let title = "BlitzkriegBot: 内核已停机，停止重启";
    let body = format!("{}（尝试 {} 次）", alert.message, alert.attempts);
    std::thread::spawn(move || {
        let cmds: [(&str, Vec<String>); 2] = [
            (
                "osascript",
                vec![
                    "-e".into(),
                    format!(
                        "display notification {} with title {}",
                        applescript_quote(&body),
                        applescript_quote(title)
                    ),
                ],
            ),
            ("notify-send", vec![title.into(), body]),
        ];
        for (bin, args) in cmds {
            let Ok(mut child) = Command::new(bin)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                break;
            }
        }
    });
}

/// Quote a string for an AppleScript literal (only `\` and `"` are special).
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
