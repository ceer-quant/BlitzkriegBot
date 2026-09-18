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

use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for the core to bind its socket after spawn.
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
/// How long to wait for a graceful (SIGTERM) exit before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for readiness / exit.
const POLL: Duration = Duration::from_millis(100);

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
            .map(|s| s.split_whitespace().map(|a| a.to_string()).collect())
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
    let candidates = [
        root.join("target/release/blitzkrieg-core"),
        root.join("core/blitzkrieg_core/target/release/blitzkrieg-core"),
        root.join("target/debug/blitzkrieg-core"),
        root.join("core/blitzkrieg_core/target/debug/blitzkrieg-core"),
    ];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
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
            next_attempt_after: None,
            spawned_at: None,
        }
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
            .stderr(Stdio::inherit());
        if let Some(dir) = &self.cfg.cwd {
            cmd.current_dir(dir);
        }
        let child = cmd
            .spawn()
            .map_err(|e| SupervisorError::Spawn(e.to_string()))?;
        let pid = child.id();
        self.child = Some(child);
        self.owns = true;

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
            self.restart_given_up = false;
        }
        // Only give up when the budget is spent AND nothing is in flight. A
        // replacement we just spawned has not bound yet, so `is_running()` is
        // false — reading that as "gave up" would abandon a core that is seconds
        // away from serving. A genuinely hung one is retired by the next `pump()`.
        if self.restarts >= self.policy.max_attempts && self.child.is_none() {
            self.restart_given_up = true;
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
}
