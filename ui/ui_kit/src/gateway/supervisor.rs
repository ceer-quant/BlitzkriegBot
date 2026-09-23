//! Core process supervisor — the Rust replacement for the lifecycle half of the
//! deleted Node client (`BlitzkriegCoreClient`, gone with the source layer in
//! 62b16c88).
//!
//! It does exactly three things: spawn `blitzkrieg-core` with parameters,
//! stop it, or **adopt** a core that is already serving the socket. It holds no
//! trading logic — every decision stays inside the Rust core. This is what let
//! the Node shell be removed (D-4, step ②) without losing the `/crypto-hft
//! start|stop` control channel; the shell itself is now gone too.
//!
//! Safety rules carried over from that Node client:
//!   * **Adopt, never double-spawn.** If the socket is already served we connect
//!     to that core instead of spawning a second one (which would abort with
//!     "already listening"). An adopted core is *not ours* — `stop()` will not
//!     kill it.
//!   * `stop()` only signals a process we spawned.
//!   * On drop, a spawned (owned) core is stopped so the gateway never leaks an
//!     unmanaged core. An adopted one is left running.
//!
//! Zero extra dependencies: SIGTERM is sent by invoking the system `kill(1)`
//! (present at `/bin/kill` on macOS and Linux). If that is unavailable we fall
//! back to the std `Child::kill` (SIGKILL). The core's order/position
//! persistence is per-state-change (verified crash-safe under SIGKILL in
//! `scripts/order-recovery-check.mjs` / `position-recovery-check.mjs`), so
//! either signal is safe.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long to wait for the core to bind its socket after spawn.
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
/// How long to wait for a graceful (SIGTERM) exit before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for readiness / exit.
const POLL: Duration = Duration::from_millis(100);
/// Lines of the dying core's stderr kept for the give-up alert. The tail is what
/// makes an alert actionable: "it crashed 6 times" without the reason is a
/// notification nobody can act on.
const STDERR_TAIL_LINES: usize = 40;
/// Upper bound on one retained stderr line, so a core streaming without newlines
/// cannot grow the ring without bound.
const STDERR_TAIL_LINE_BYTES: usize = 4_000;
/// Opt-in: `<prefix>_ALERT_NOTIFY=1` also pushes the give-up alert to the
/// desktop notifier (`osascript` / `notify-send`). Off by default — a supervisor
/// should not pop windows on a machine that never asked for them — but a
/// keep-alive wrapper on an unattended box wants exactly this.
const ENV_ALERT_NOTIFY: &str = "BLITZKRIEG_ALERT_NOTIFY";

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
    fn classify(pid: u32, status: ExitStatus, stopping: bool) -> Self {
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

/// Bounded ring of the child's most recent stderr lines, shared with the reader
/// thread that drains the pipe.
#[derive(Debug, Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        g.push_back(line);
        while g.len() > STDERR_TAIL_LINES {
            g.pop_front();
        }
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn snapshot(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

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
fn emit_give_up(alert: &GiveUpAlert) {
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

/// How hard the supervisor should try to keep an owned core alive.
///
/// Default is **off**: replacing a process the operator did not ask to replace
/// is a policy, and a supervisor should not acquire one by accident. A launcher
/// whose entire job is "keep the kernel up" turns it on explicitly, and the
/// attempt budget is finite so an instantly-dying core cannot be respawned
/// forever — the attempt counter is surfaced so the UI can say "gave up" rather
/// than showing a healthy-looking process that will never come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub enabled: bool,
    /// Total respawns allowed for this supervisor's lifetime.
    pub max_attempts: u32,
    /// First backoff; doubles per attempt up to `max_backoff`.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl RestartPolicy {
    pub const fn off() -> Self {
        Self {
            enabled: false,
            max_attempts: 0,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
        }
    }

    pub const fn keep_alive() -> Self {
        Self {
            enabled: true,
            max_attempts: 5,
            ..RestartPolicy::off()
        }
    }

    /// Backoff before the `attempt`-th respawn (0-based).
    fn backoff_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(16);
        let ms = self
            .base_backoff
            .as_millis()
            .saturating_mul(1u128 << shift)
            .min(self.max_backoff.as_millis());
        Duration::from_millis(ms as u64)
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

#[derive(Debug)]
pub enum SupervisorError {
    BinaryNotFound(PathBuf),
    Spawn(String),
    NotReady(String),
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisorError::BinaryNotFound(p) => {
                write!(f, "blitzkrieg-core binary not found: {}", p.display())
            }
            SupervisorError::Spawn(e) => write!(f, "failed to spawn blitzkrieg-core: {e}"),
            SupervisorError::NotReady(e) => write!(f, "blitzkrieg-core did not become ready: {e}"),
        }
    }
}
impl std::error::Error for SupervisorError {}

/// Everything needed to spawn a core. Defaults mirror the arguments the deleted
/// Node runner (`src/core/blitzkrieg-core-runner.ts`, gone with the source layer
/// in `62b16c88`) used to assemble for production.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub binary_path: PathBuf,
    pub socket_path: String,
    /// "dry" | "live".
    pub mode: String,
    pub seed_balance: String,
    pub tick_ms: u64,
    pub max_order_notional: String,
    pub assets: Vec<String>,
    pub round_sec: u64,
    pub min_round_age: u64,
    pub min_time_left: u64,
    pub max_positions: u64,
    pub min_shares: u64,
    pub max_shares: u64,
    /// Extra raw args appended verbatim (e.g. `--no-position-log`).
    pub extra_args: Vec<String>,
    /// Working directory for the child (core writes relative `data/` paths here).
    pub cwd: Option<PathBuf>,
}

impl SupervisorConfig {
    /// Production-shaped defaults, overridable by the environment exactly like
    /// the `HFT_*` variables the Node command path used (`HFT_ASSETS`,
    /// `HFT_ROUND_SEC`, `HFT_MIN_SHARES`, `HFT_MAX_SHARES`) — those variable
    /// names survive the runner's deletion and are read here directly.
    pub fn from_env(socket_path: String) -> Self {
        let env_num = |k: &str, d: u64| -> u64 {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d)
        };
        let assets = std::env::var("HFT_ASSETS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.split(',').map(|a| a.trim().to_uppercase()).collect())
            .unwrap_or_else(|| vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()]);
        let max_shares = env_num("HFT_MAX_SHARES", 10);
        let min_shares = env_num("HFT_MIN_SHARES", 10).min(max_shares);
        let dry = std::env::var("DRY_RUN")
            .map(|v| v != "false")
            .unwrap_or(true);
        // Per-order notional is a SAFETY bound, not the strategy size: size it
        // from the real worst case (max shares × ~0.6) so a legitimate order is
        // never blocked (mirrors the Node runner's rationale).
        let notional = format!("{:.2}", (max_shares as f64 * 0.6).max(6.0));
        // Ops/test overrides: pin the binary, the child working directory (so a
        // harness can isolate `data/`), and append raw args (e.g. `--no-trade-log`).
        let binary_path = std::env::var("UIKIT_CORE_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(discover_binary);
        let cwd = std::env::var("UIKIT_CORE_CWD")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        let extra_args: Vec<String> = std::env::var("UIKIT_CORE_EXTRA_ARGS")
            .ok()
            .map(|s| split_extra_args(&s))
            .unwrap_or_else(|| vec!["--engine".into(), "--feed-ws".into()]);
        Self {
            binary_path,
            socket_path,
            mode: if dry { "dry".into() } else { "live".into() },
            seed_balance: "1000".into(),
            tick_ms: 50,
            max_order_notional: notional,
            assets,
            round_sec: env_num("HFT_ROUND_SEC", 900),
            min_round_age: 30,
            min_time_left: 180,
            max_positions: 2,
            min_shares,
            max_shares,
            extra_args,
            cwd,
        }
    }

    fn to_args(&self) -> Vec<String> {
        let mut a = vec![
            "--socket".into(),
            self.socket_path.clone(),
            "--mode".into(),
            self.mode.clone(),
            "--tick-ms".into(),
            self.tick_ms.to_string(),
            "--seed-balance".into(),
            self.seed_balance.clone(),
            "--max-order-notional".into(),
            self.max_order_notional.clone(),
            "--assets".into(),
            self.assets.join(","),
            "--round-sec".into(),
            self.round_sec.to_string(),
            "--min-round-age".into(),
            self.min_round_age.to_string(),
            "--min-time-left".into(),
            self.min_time_left.to_string(),
            "--max-positions".into(),
            self.max_positions.to_string(),
            "--min-shares".into(),
            self.min_shares.to_string(),
            "--max-shares".into(),
            self.max_shares.to_string(),
        ];
        a.extend(self.extra_args.iter().cloned());
        a
    }
}

/// Find the workspace-built core binary, mirroring Node's candidate list.
pub fn discover_binary() -> PathBuf {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut candidates = vec![
        root.join("target/release/blitzkrieg-core"),
        root.join("core/blitzkrieg_core/target/release/blitzkrieg-core"),
        root.join("target/debug/blitzkrieg-core"),
        root.join("core/blitzkrieg_core/target/debug/blitzkrieg-core"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("blitzkrieg-core"));
            if let Some(grandparent) = parent.parent() {
                candidates.push(grandparent.join("release/blitzkrieg-core"));
                candidates.push(grandparent.join("debug/blitzkrieg-core"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| root.join("target/release/blitzkrieg-core"))
}

/// Whether a live core is serving `socket_path` right now.
pub fn socket_served(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// We spawned a new core and it became ready.
    Started { pid: u32 },
    /// A core was already serving the socket; we connected to it, we do not own it.
    Adopted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// We killed the core we spawned.
    Stopped { pid: u32 },
    /// We only had an adopted core (or none); nothing was killed.
    NotOwned,
}

/// Owns the core child process. Process lifecycle ONLY — no trading logic.
pub struct Supervisor {
    cfg: SupervisorConfig,
    child: Option<Child>,
    owns: bool,
    /// Set for the duration of a `stop()` so the exit it causes is classified
    /// Clean. Without it, every deliberate stop would be reported as a crash.
    stopping: bool,
    policy: RestartPolicy,
    restarts: u32,
    last_exit: Option<ExitReport>,
    /// Set when a crash could not be replaced (budget spent). Distinguishes
    /// "down and will stay down" from "down, replacement on its way".
    restart_given_up: bool,
    /// The one-shot give-up alert, kept for the panel and for the record.
    give_up_alert: Option<GiveUpAlert>,
    /// Extra delivery channel, installed by the embedding gateway.
    alert_sink: Option<Arc<dyn AlertSink>>,
    /// Tail of the current (or last) child's stderr, drained by a reader thread.
    stderr_tail: StderrTail,
    /// Backoff to honour before the next attempt, set when an attempt fails
    /// immediately so the retry loop does not spin on a core that cannot boot.
    next_attempt_after: Option<Instant>,
    /// When an un-waited (poll-path) spawn started. Only a restart sets this;
    /// it is what lets a later tick tell "still coming up" from "never will".
    spawned_at: Option<Instant>,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            child: None,
            owns: false,
            stopping: false,
            policy: RestartPolicy::off(),
            restarts: 0,
            last_exit: None,
            restart_given_up: false,
            give_up_alert: None,
            alert_sink: None,
            stderr_tail: StderrTail::default(),
            next_attempt_after: None,
            spawned_at: None,
        }
    }

    /// Install the gateway's alert channel. Called once, at wiring time; the
    /// sink is used for every give-up (and only then — a crash that is being
    /// retried is not an alert, it is a restart).
    pub fn set_alert_sink(&mut self, sink: Arc<dyn AlertSink>) {
        self.alert_sink = Some(sink);
    }

    /// The give-up alert, if the budget has been spent. Same value as
    /// [`CoreHealth::alert`], for callers that hold the supervisor directly.
    pub fn give_up_alert(&self) -> Option<&GiveUpAlert> {
        self.give_up_alert.as_ref()
    }

    /// Record that a core is (or is coming) up.
    ///
    /// Clears the give-up state: `restart_given_up` and its alert describe the
    /// CURRENT absence of a kernel, so once one is spawned or adopted they are
    /// history — a panel that kept showing "已放弃重启" over a live core would be
    /// reporting a problem that no longer exists. A core that dies again gets a
    /// fresh alert from [`Supervisor::alert_give_up`].
    fn mark_core_up(&mut self) {
        self.restart_given_up = false;
        self.give_up_alert = None;
    }

    /// Raise the one-shot give-up alert. Idempotent: the transition into
    /// "given up" happens in two places (the pre-attempt budget check and the
    /// post-attempt check), and an alert that fired twice would look like two
    /// separate failures.
    fn alert_give_up(&mut self) {
        if self.give_up_alert.is_some() {
            return;
        }
        let alert = GiveUpAlert {
            attempts: self.restarts,
            last_exit: self.last_exit.clone(),
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            stderr_tail: self.stderr_tail.snapshot(),
            message: format!(
                "内核连崩 {} 次后已放弃重启（预算 {} 次已用尽），进程不会自行恢复",
                self.restarts + 1,
                self.policy.max_attempts
            ),
        };
        emit_give_up(&alert);
        if let Some(sink) = &self.alert_sink {
            sink.give_up(&alert);
        }
        self.give_up_alert = Some(alert);
    }

    /// Turn crash-replacement on or off. Off by default: silently respawning a
    /// process the operator did not ask to respawn is a policy decision, and a
    /// supervisor should only hold one when it was told to.
    pub fn set_restart_policy(&mut self, policy: RestartPolicy) {
        self.policy = policy;
    }

    /// The policy currently in force. Read by tests and by callers that want to
    /// report "replacement is on" rather than leave the operator to infer it.
    pub fn restart_policy(&self) -> RestartPolicy {
        self.policy
    }

    pub fn config(&self) -> &SupervisorConfig {
        &self.cfg
    }

    /// Replace the spawn parameters. Callers must only do this while no core is
    /// owned; changing parameters takes effect on the next `start()` (a running
    /// core must be stopped first, exactly as with the Node command path).
    pub fn set_config(&mut self, cfg: SupervisorConfig) {
        self.cfg = cfg;
    }

    /// True when a core is reachable over the socket (spawned by us or adopted).
    pub fn is_running(&self) -> bool {
        socket_served(&self.cfg.socket_path)
    }

    /// True when *this* supervisor spawned the running core.
    pub fn owns(&self) -> bool {
        self.owns
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Spawn the core (or adopt an existing one) and wait until it serves it
    /// within the interactive deadline. Idempotent.
    pub fn start(&mut self) -> Result<StartOutcome, SupervisorError> {
        self.start_within(STARTUP_DEADLINE)
    }

    /// The body of `start()`, with the readiness wait parameterised.
    ///
    /// `budget` exists because two callers want opposite things from one spawn.
    /// An operator who typed `start` is owed a synchronous answer, so the
    /// interactive path blocks up to `STARTUP_DEADLINE` and reports "did not
    /// become ready" as a failure. The restart policy instead runs on the UI's
    /// refresh tick, where blocking is a defect: that tick is how the UI *finds
    /// out* what happened, so freezing it for the length of a handshake would
    /// hide the very crash being recovered from. Passing `Duration::ZERO` spawns
    /// without waiting and lets a later tick observe readiness — or retire a
    /// replacement that never came up (`retire_unready_restart`).
    ///
    /// Note the ordering: a child that died on its own is observed first, so an
    /// explicit `start()` after a crash records that crash instead of quietly
    /// overwriting it with a fresh spawn.
    fn start_within(&mut self, budget: Duration) -> Result<StartOutcome, SupervisorError> {
        let _ = self.reap();
        if socket_served(&self.cfg.socket_path) {
            // A core already owns the socket — adopt it. Spawning here would only
            // produce a duplicate-core abort (the Node client makes the same
            // choice, see `tryAdopt`).
            self.child = None;
            self.owns = false;
            self.mark_core_up();
            return Ok(StartOutcome::Adopted);
        }
        if !self.cfg.binary_path.exists() {
            return Err(SupervisorError::BinaryNotFound(
                self.cfg.binary_path.clone(),
            ));
        }
        let mut cmd = Command::new(&self.cfg.binary_path);
        cmd.args(self.cfg.to_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Piped rather than inherited: the core's own stderr is where it
            // explains a fatal boot error, and it is the only evidence left when
            // the restart budget runs out. A reader thread re-emits every line to
            // our stderr (so the run log is unchanged) while keeping the tail.
            .stderr(Stdio::piped());
        if let Some(dir) = &self.cfg.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError::Spawn(e.to_string()))?;
        let pid = child.id();
        // The tail belongs to the child that just died, not to its predecessors:
        // an alert showing the stderr of attempt 1 while attempt 6 is the one
        // that mattered would point the operator at the wrong failure.
        self.stderr_tail.clear();
        if let Some(err) = child.stderr.take() {
            drain_stderr(err, self.stderr_tail.clone());
        }
        self.child = Some(child);
        self.owns = true;
        self.mark_core_up();

        // Poll path: report the spawn and let a later tick observe it. Waiting
        // here would block the UI on the handshake it uses to discover state, and
        // tearing the child down on a deadline the caller never asked for would
        // turn every restart into a kill-and-retry. A replacement that never
        // binds is caught by the next `reap()` (it exits → recorded as a crash)
        // or by `retire_unready_restart`.
        if budget.is_zero() {
            self.spawned_at = Some(Instant::now());
            return Ok(StartOutcome::Started { pid });
        }

        // Readiness handshake: the core binds the socket shortly after spawn.
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if let Some(status) = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok())
                .flatten()
            {
                // It started and died before binding. Record the status — "never
                // opened the socket" is useless to an operator without the exit
                // reason, and discarding it here was the old silent-loss path.
                self.child = None;
                self.owns = false;
                self.last_exit = Some(ExitReport::classify(pid, status, self.stopping));
                return Err(SupervisorError::NotReady(
                    "process exited during startup".into(),
                ));
            }
            if socket_served(&self.cfg.socket_path) {
                return Ok(StartOutcome::Started { pid });
            }
            std::thread::sleep(POLL);
        }
        // Timed out — do not leave a half-started core behind.
        self.stop();
        Err(SupervisorError::NotReady(
            "socket not bound within 15s".into(),
        ))
    }

    /// Stop the core **we spawned**. An adopted core is never killed.
    pub fn stop(&mut self) -> StopOutcome {
        // Observe first: a child that already died on its own must be classified
        // as a crash, not reported as "stopped" by us.
        let _ = self.reap();
        self.spawned_at = None;
        let Some(mut child) = self.child.take() else {
            self.owns = false;
            return StopOutcome::NotOwned;
        };
        let pid = child.id();
        // Mark the intent BEFORE signalling: `stopping` is what makes the exit
        // this causes classify as Clean instead of Crashed.
        self.stopping = true;
        send_sigterm(pid);
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.owns = false;
                    self.stopping = false;
                    self.last_exit = Some(ExitReport::classify(pid, status, true));
                    return StopOutcome::Stopped { pid };
                }
                Ok(None) => std::thread::sleep(POLL),
                Err(_) => break,
            }
        }
        // Escalate: SIGKILL. Crash recovery makes this safe (see module docs).
        let _ = child.kill();
        let status = child.wait().ok();
        self.owns = false;
        self.stopping = false;
        if let Some(status) = status {
            self.last_exit = Some(ExitReport::classify(pid, status, true));
        }
        StopOutcome::Stopped { pid }
    }

    /// Clear our handle if the child exited on its own (no kill), reporting why.
    ///
    /// This is the observation half of E12(c) and it deliberately does NOT
    /// restart: `pump()` owns that decision. Keeping them apart means a caller
    /// that only wants to know state (a status read) cannot trigger a respawn.
    fn reap(&mut self) -> Option<ExitReport> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                let pid = child.id();
                let report = ExitReport::classify(pid, status, self.stopping);
                self.child = None;
                self.owns = false;
                self.spawned_at = None;
                self.last_exit = Some(report.clone());
                Some(report)
            }
            _ => None,
        }
    }

    /// Kill and report a replacement that never bound the socket.
    ///
    /// A restart is spawned without waiting (see `start_within`), so at that
    /// moment "the process exists" is all we know. If it is *still* alive after
    /// the interactive startup deadline without serving, it is not a recovery —
    /// leaving it would put a managed pid on the wire over a socket nothing
    /// answers on, indefinitely, which is the same lie as reporting a crash that
    /// was never noticed. Classified as a crash because nobody asked for this
    /// exit, so it consumes the restart budget like any other.
    fn retire_unready_restart(&mut self) -> Option<ExitReport> {
        if self.spawned_at?.elapsed() < STARTUP_DEADLINE {
            return None;
        }
        if socket_served(&self.cfg.socket_path) {
            return None;
        }
        let mut child = self.child.take()?;
        let pid = child.id();
        let _ = child.kill();
        let status = child.wait().ok();
        self.owns = false;
        self.spawned_at = None;
        let report = ExitReport {
            pid,
            kind: ExitKind::Crashed,
            code: status.and_then(|s| s.code()),
            signal: status.and_then(|s| s.signal()),
        };
        self.last_exit = Some(report.clone());
        Some(report)
    }

    /// Observe the core and apply the restart policy. Call this on the UI's
    /// refresh tick: it is what turns "the process we started is gone" into
    /// either a reported crash or a replacement, instead of a status line that
    /// keeps saying "running" because nobody looked.
    ///
    /// Returns the exit report if an owned core ended during this call.
    pub fn pump(&mut self) -> Option<ExitReport> {
        if let Some(report) = self.reap() {
            if report.kind == ExitKind::Crashed {
                self.try_restart();
            }
            return Some(report);
        }
        // Still alive — but a restart we did not wait for may have failed to come
        // up. Retiring it here (rather than in `reap`) keeps the observation and
        // the decision separate for the normal case while still giving a hung
        // replacement an ending.
        let report = self.retire_unready_restart()?;
        self.try_restart();
        Some(report)
    }

    /// Replace a crashed core, honouring the attempt budget and backoff. A
    /// no-op unless a policy was set: `RestartPolicy::off()` is the default and
    /// means "report the crash, do not invent a replacement".
    fn try_restart(&mut self) {
        if !self.policy.enabled || self.restart_given_up {
            return;
        }
        if let Some(after) = self.next_attempt_after {
            if Instant::now() < after {
                return; // still backing off; the next tick will retry
            }
        }
        if self.restarts >= self.policy.max_attempts {
            self.restart_given_up = true;
            self.alert_give_up();
            return;
        }
        let attempt = self.restarts;
        self.next_attempt_after = Some(Instant::now() + self.policy.backoff_for(attempt));
        self.restarts += 1;
        // `start()` re-checks adoption, so if something else already brought a
        // core up on this socket we attach to that instead of racing it. A
        // failed attempt is NOT recorded as an exit: the real crash stays in
        // `last_exit`, because "why it died" is what the operator needs, and a
        // failed respawn has no pid or status to report.
        //
        // `Duration::ZERO`: this runs on the UI's refresh tick, so it must not
        // block on the handshake (see `start_within`). Readiness is observed by
        // the next tick.
        if self.start_within(Duration::ZERO).is_ok() {
            self.next_attempt_after = None;
        }
        // Only give up when the budget is spent AND nothing is in flight. A
        // replacement we just spawned has not bound yet, so `is_running()` is
        // false — reading that as "gave up" would abandon a core that is seconds
        // away from serving. A genuinely hung one is retired by the next `pump()`.
        if self.restarts >= self.policy.max_attempts && self.child.is_none() {
            self.restart_given_up = true;
            self.alert_give_up();
        }
    }

    /// What the UI renders. Answers "up or down, whose is it, and if it went
    /// down — why, and are we still trying".
    pub fn health(&self) -> CoreHealth {
        CoreHealth {
            running: self.is_running(),
            managed: self.owns,
            pid: self.pid(),
            restarts: self.restarts,
            last_exit: self.last_exit.clone(),
            restart_given_up: self.restart_given_up,
            alert: self.give_up_alert.clone(),
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Never leak an unmanaged core we spawned; leave an adopted one alone.
        if self.owns {
            let _ = self.stop();
        }
    }
}

/// Drain a child's stderr on its own thread: every line goes to our stderr (so
/// Split `UIKIT_CORE_EXTRA_ARGS` into the child's arguments.
///
/// Whitespace-separated, except inside single or double quotes. Plain
/// `split_whitespace` cannot express a value that contains a space, and an
/// absolute path is exactly that on a checkout whose directory name has one:
/// `--strategy-dir /Volumes/Hard Disk/…` would arrive as two arguments, and the
/// core (correctly) refuses to start on an argument it does not understand.
fn split_extra_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// the gateway's run log keeps showing core output exactly as before) and into
/// the bounded tail ring that the give-up alert reports.
fn drain_stderr(err: impl std::io::Read + Send + 'static, tail: StderrTail) {
    std::thread::spawn(move || {
        let reader = BufReader::new(err);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            eprintln!("[core] {line}");
            let line = if line.len() > STDERR_TAIL_LINE_BYTES {
                let mut cut = STDERR_TAIL_LINE_BYTES;
                while cut > 0 && !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}… (truncated)", &line[..cut])
            } else {
                line
            };
            tail.push(line);
        }
    });
}

/// Send SIGTERM via `kill(1)`. Dependency-free; falls back to nothing on failure
/// (the caller then escalates to SIGKILL).
fn send_sigterm(pid: u32) {
    for path in ["/bin/kill", "kill"] {
        let ok = Command::new(path)
            .arg("-TERM")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_include_engine_and_assets() {
        let mut cfg = SupervisorConfig::from_env("/tmp/x.sock".into());
        cfg.assets = vec!["BTC".into(), "ETH".into()];
        let args = cfg.to_args();
        assert!(args.contains(&"--engine".to_string()));
        assert!(args.contains(&"--feed-ws".to_string()));
        let i = args.iter().position(|a| a == "--assets").unwrap();
        assert_eq!(args[i + 1], "BTC,ETH");
    }

    #[test]
    fn socket_served_false_for_missing_path() {
        assert!(!socket_served("/tmp/definitely-not-a-socket-12345.sock"));
    }

    #[test]
    fn extra_args_split_on_whitespace() {
        assert_eq!(
            split_extra_args("--engine --feed-ws --no-trade-log"),
            ["--engine", "--feed-ws", "--no-trade-log"]
        );
        assert_eq!(split_extra_args("   "), Vec::<String>::new());
    }

    #[test]
    fn extra_args_keep_quoted_paths_whole() {
        // A checkout under a directory whose name contains a space is ordinary on
        // macOS; the strategy dir has to survive as ONE argument.
        assert_eq!(
            split_extra_args("--strategy-dir \"/Volumes/Hard Disk/repo/ul/parity\" --engine"),
            [
                "--strategy-dir",
                "/Volumes/Hard Disk/repo/ul/parity",
                "--engine"
            ]
        );
        assert_eq!(
            split_extra_args("--strategy-dir '/a b c'"),
            ["--strategy-dir", "/a b c"]
        );
    }

    #[test]
    fn adopted_stop_does_not_kill() {
        // No child ever spawned → stop must be a no-op (NotOwned), never a signal.
        let cfg = SupervisorConfig::from_env("/tmp/x.sock".into());
        let mut sup = Supervisor::new(cfg);
        assert_eq!(sup.stop(), StopOutcome::NotOwned);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = RestartPolicy::keep_alive();
        assert_eq!(p.backoff_for(0), Duration::from_millis(250));
        assert_eq!(p.backoff_for(1), Duration::from_millis(500));
        assert_eq!(p.backoff_for(2), Duration::from_millis(1000));
        // Far enough out it saturates at max_backoff rather than overflowing.
        assert_eq!(p.backoff_for(40), p.max_backoff);
    }

    #[test]
    fn restart_is_off_by_default() {
        // The default policy must not acquire the ability to respawn anything:
        // replacing a process the operator did not ask to replace is a policy,
        // and a supervisor should not hold one by accident.
        let cfg = SupervisorConfig::from_env("/tmp/x.sock".into());
        let sup = Supervisor::new(cfg);
        assert!(!sup.policy.enabled);
        assert_eq!(sup.policy.max_attempts, 0);
    }

    /// Give this supervisor a real, controllable child. `sleep` is used because
    /// it can be killed deterministically and binds no socket, so the tests below
    /// exercise reap/classify without the 15-second readiness handshake.
    fn give_it_a_sleeper(sup: &mut Supervisor) {
        let child = Command::new("/bin/sleep").arg("120").spawn().unwrap();
        sup.child = Some(child);
        sup.owns = true;
    }

    #[test]
    fn a_restart_is_not_retired_before_it_has_had_time_to_bind() {
        // The poll path spawns without waiting, so right after a restart "alive
        // but not yet serving" is the normal state — a core needs a moment to
        // bind. Retiring it here would turn every successful restart into a kill
        // a fraction of a second later: a restart loop the operator sees as
        // instability, caused entirely by looking too early.
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-6.sock".into());
        let mut sup = Supervisor::new(cfg);
        give_it_a_sleeper(&mut sup);
        sup.spawned_at = Some(Instant::now());

        assert!(
            sup.retire_unready_restart().is_none(),
            "a replacement that is still within its startup window must be left alone"
        );
        assert!(sup.owns(), "and it is still ours while it comes up");
        // The next tick must still be able to observe it normally.
        assert!(sup.pump().is_none(), "a live child is not an exit");

        // Past the deadline with the socket still unserved, it is not a recovery.
        // A hung pid that the UI reports as managed is the same lie as a crash
        // nobody noticed, so it gets an ending and is charged to the budget.
        sup.spawned_at = Some(Instant::now() - STARTUP_DEADLINE - Duration::from_millis(1));
        let report = sup
            .retire_unready_restart()
            .expect("a hung replacement must be retired");
        assert_eq!(report.kind, ExitKind::Crashed, "nobody asked for this exit");
        assert!(!sup.owns());
        assert_eq!(sup.health().last_exit.unwrap().kind, ExitKind::Crashed);
    }

    #[test]
    fn the_poll_path_does_not_wait_for_the_handshake() {
        // `try_restart` must return promptly: it runs on the UI's refresh tick,
        // and a tick that blocks for up to 15 s would stall the very channel the
        // panel uses to learn that the core is back. Measured rather than
        // asserted by reading the code, because the expensive path is the one
        // that already exists.
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-7.sock".into());
        let mut sup = Supervisor::new(cfg);
        // A real, long-lived binary that will never bind this socket.
        sup.cfg.binary_path = PathBuf::from("/bin/sleep");
        sup.cfg.extra_args = vec!["120".into()];

        let t0 = Instant::now();
        let out = sup.start_within(Duration::ZERO);
        let took = t0.elapsed();

        assert!(
            matches!(out, Ok(StartOutcome::Started { .. })),
            "the spawn itself succeeded; readiness is a later observation"
        );
        assert!(
            took < Duration::from_secs(2),
            "a poll-path spawn waited {took:?} — it must not block on the handshake"
        );
        assert!(
            !sup.health().restart_given_up,
            "a spawn that has not had time to fail yet must not report giving up"
        );
        let _ = sup.stop();
    }

    #[test]
    fn a_killed_core_is_reported_as_a_crash() {
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-1.sock".into());
        let mut sup = Supervisor::new(cfg);
        give_it_a_sleeper(&mut sup);
        let pid = sup.pid().unwrap();

        // Kill it behind the supervisor's back — this is the crash case, and it
        // is exactly what the old `reap()` swallowed (it cleared the handle and
        // kept no record, so the UI went on reporting a process that was gone).
        sup.child.as_mut().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let report = sup.pump().expect("a killed child must be observed");
        assert_eq!(report.pid, pid);
        assert_eq!(report.kind, ExitKind::Crashed);
        assert_eq!(
            report.signal,
            Some(9),
            "SIGKILL must be named, not inferred"
        );
        assert!(!sup.owns());
        assert!(report.describe().contains("CRASHED"));
        // The health view must carry the same fact, since that is what the UI reads.
        let health = sup.health();
        assert!(!health.running);
        assert_eq!(health.last_exit.as_ref().unwrap().kind, ExitKind::Crashed);
    }

    #[test]
    fn a_stop_we_asked_for_is_not_a_crash() {
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-2.sock".into());
        let mut sup = Supervisor::new(cfg);
        give_it_a_sleeper(&mut sup);

        assert!(matches!(sup.stop(), StopOutcome::Stopped { .. }));
        let report = sup
            .health()
            .last_exit
            .expect("a stop must still be recorded — just as Clean");
        assert_eq!(
            report.kind,
            ExitKind::Clean,
            "an operator-requested stop must never be reported as a crash: \
             a UI that cannot tell them apart either cries wolf or stays silent"
        );
        assert!(report.describe().contains("stopped"));
    }

    #[test]
    fn a_crash_is_not_replaced_when_the_policy_is_off() {
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-3.sock".into());
        let mut sup = Supervisor::new(cfg);
        give_it_a_sleeper(&mut sup);
        sup.child.as_mut().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(150));

        sup.pump();
        assert_eq!(sup.restarts, 0, "off means off");
        assert!(!sup.health().restart_given_up);
    }

    #[test]
    fn a_late_crash_is_repaired_by_pump_when_the_policy_is_on() {
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-4.sock".into());
        let mut sup = Supervisor::new(cfg);
        // A non-existent binary makes the respawn attempt fail fast, so this test
        // measures the DECISION (attempt counted, budget respected) rather than
        // paying a 15-second readiness handshake.
        sup.cfg.binary_path = PathBuf::from("/nonexistent/blitzkrieg-core-xyz");
        sup.set_restart_policy(RestartPolicy {
            max_attempts: 1,
            ..RestartPolicy::keep_alive()
        });
        give_it_a_sleeper(&mut sup);
        sup.child.as_mut().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(150));

        sup.pump();
        assert_eq!(
            sup.restarts, 1,
            "the crash must be answered with one attempt"
        );
        // The original crash stays in last_exit: a failed respawn has no status
        // of its own to report, and the reason it died is what an operator needs.
        assert_eq!(
            sup.health().last_exit.unwrap().kind,
            ExitKind::Crashed,
            "the recorded exit must remain the crash, not the failed retry"
        );
        assert!(
            sup.health().restart_given_up,
            "a spent budget must be visible, or the UI shows a core that will never return"
        );

        // And a given-up supervisor must stop trying rather than spin.
        sup.pump();
        assert_eq!(sup.restarts, 1);
    }

    #[test]
    fn restarts_are_spaced_out_while_backing_off() {
        let cfg = SupervisorConfig::from_env("/tmp/bk-not-served-5.sock".into());
        let mut sup = Supervisor::new(cfg);
        sup.cfg.binary_path = PathBuf::from("/nonexistent/blitzkrieg-core-xyz");
        sup.set_restart_policy(RestartPolicy::keep_alive());
        give_it_a_sleeper(&mut sup);
        sup.child.as_mut().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(150));

        sup.pump();
        assert_eq!(sup.restarts, 1);
        // Immediately trying again must be held off by the backoff: a core that
        // cannot boot must not be respawned in a tight loop.
        sup.child = Some(Command::new("/bin/sleep").arg("120").spawn().unwrap());
        sup.owns = true;
        sup.child.as_mut().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        sup.pump();
        assert_eq!(
            sup.restarts, 1,
            "the second attempt must wait for the backoff"
        );
    }

    /// A real executable that writes one line to stderr and exits non-zero —
    /// the cheapest faithful stand-in for a core that dies on boot with an
    /// explanation. Returns its path.
    fn a_dying_binary(tag: &str, stderr_line: &str, code: i32) -> PathBuf {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("bk-sup-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dying-core.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "echo '{stderr_line}' >&2").unwrap();
        writeln!(f, "exit {code}").unwrap();
        drop(f);
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        path
    }

    /// Records every alert it is handed, so the test asserts the delivery
    /// channel rather than only the internal field.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<GiveUpAlert>>);

    impl AlertSink for RecordingSink {
        fn give_up(&self, alert: &GiveUpAlert) {
            self.0.lock().unwrap().push(alert.clone());
        }
    }

    /// A supervisor whose respawns are instant (no backoff) and whose child dies
    /// immediately, so "N crashes in a row" is a fast, deterministic test.
    fn a_flapping_core(tag: &str, stderr_line: &str, max_attempts: u32) -> Supervisor {
        let cfg = SupervisorConfig::from_env(format!("/tmp/bk-flap-{tag}.sock"));
        let mut sup = Supervisor::new(cfg);
        sup.cfg.binary_path = a_dying_binary(tag, stderr_line, 3);
        sup.cfg.extra_args = Vec::new();
        sup.set_restart_policy(RestartPolicy {
            max_attempts,
            base_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            ..RestartPolicy::keep_alive()
        });
        sup
    }

    /// Pump until an exit is observed, or fail. A bounded poll rather than a
    /// fixed sleep: these tests are about the alert, and a fixed sleep would make
    /// them flaky on a loaded machine for a reason unrelated to the alert.
    fn pump_until_crash(sup: &mut Supervisor) -> ExitReport {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(report) = sup.pump() {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "no exit was observed within 5s of a crash"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn six_consecutive_crashes_raise_a_visible_give_up_alert() {
        // The acceptance case for #186: a core that crashes on every start must
        // not simply stop being restarted in silence. Six crashes, a budget of
        // five replacements, and the alert has to be on every channel — the
        // internal field the panel reads, the stderr banner, and the sink the
        // embedding gateway installed.
        let mut sup = a_flapping_core("giveup", "boom: no market plugin", 5);
        let sink = Arc::new(RecordingSink::default());
        sup.set_alert_sink(sink.clone());
        assert!(sup.start_within(Duration::ZERO).is_ok());

        // Six crashes, observed one at a time. The wait is a bounded poll rather
        // than a fixed sleep: the point of the test is the ALERT, and a fixed
        // sleep would make it fail on a loaded machine for a reason that has
        // nothing to do with the alert.
        for crash in 1..=6 {
            let report = pump_until_crash(&mut sup);
            assert_eq!(report.kind, ExitKind::Crashed);
            if crash < 6 {
                assert!(
                    sup.health().alert.is_none(),
                    "crash {crash} of 6 must be answered by a restart, not an alert"
                );
            }
        }

        let health = sup.health();
        assert!(
            health.restart_given_up,
            "a spent budget must be visible in the health view"
        );
        let alert = health
            .alert
            .expect("giving up must produce an alert, not only a status flag");
        assert_eq!(alert.attempts, 5, "the budget spent is what is reported");
        assert_eq!(
            alert.last_exit.as_ref().unwrap().code,
            Some(3),
            "the alert names how the core died"
        );
        assert!(alert.at_ms > 0, "an alert without a time is not actionable");
        assert!(
            alert.message.contains("放弃重启"),
            "the message must say the supervisor gave up: {}",
            alert.message
        );
        assert!(alert.banner().contains("core NOT running"));
        assert!(
            alert.stderr_tail.iter().any(|l| l.contains("boom")),
            "the core's own stderr is the reason; the alert must carry it: {:?}",
            alert.stderr_tail
        );

        // Exactly one alert per give-up: a sink that fires on every tick would
        // page an operator forever.
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        sup.pump();
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_core_that_comes_back_clears_the_give_up_alert() {
        // The alert is a current-state notice, not a permanent scar: once a core
        // answers on the socket again the panel must stop saying "gave up".
        let mut sup = a_flapping_core("recover", "boom", 1);
        assert!(sup.start_within(Duration::ZERO).is_ok());
        pump_until_crash(&mut sup); // crash 1 → attempt 1 (the dying binary)
        pump_until_crash(&mut sup); // crash 2 → budget spent → alert
        assert!(sup.health().restart_given_up);
        assert!(sup.health().alert.is_some());

        // Something else brings a core up on the same socket (the operator ran
        // `start` again, or another launcher did); the give-up notice must go.
        sup.cfg.binary_path = PathBuf::from("/bin/sleep");
        sup.cfg.extra_args = vec!["120".into()];
        let out = sup.start_within(Duration::ZERO);
        assert!(out.is_ok(), "the operator's start must succeed");
        let health = sup.health();
        assert!(
            health.alert.is_none() && !health.restart_given_up,
            "a core that is up again must not keep showing a give-up alert"
        );
        let _ = sup.stop();
    }

    #[test]
    fn the_alert_is_only_raised_while_the_budget_is_spent() {
        // Off means off: with no restart policy a crash is reported (as before)
        // but never escalated into a give-up alert.
        let mut sup = a_flapping_core("noalert", "boom", 0);
        sup.set_restart_policy(RestartPolicy::off());
        assert!(sup.start_within(Duration::ZERO).is_ok());
        pump_until_crash(&mut sup);
        let health = sup.health();
        assert_eq!(health.last_exit.as_ref().unwrap().kind, ExitKind::Crashed);
        assert!(!health.restart_given_up);
        assert!(health.alert.is_none());
    }
}
