//! Core process supervisor — the UI Kit's counterpart to Node's
//! `BlitzkriegCoreClient` *lifecycle* half only.
//!
//! It does exactly three things: spawn `blitzkrieg-core` with parameters,
//! stop it, or **adopt** a core that is already serving the socket. It holds no
//! trading logic — every decision stays inside the Rust core. This is what lets
//! the Node shell be removed (D-4, step ②) without losing the `/crypto-hft
//! start|stop` control channel.
//!
//! Safety rules carried over from the Node client (see
//! `src/core/blitzkrieg-core-client.ts`):
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
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for the core to bind its socket after spawn.
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
/// How long to wait for a graceful (SIGTERM) exit before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for readiness / exit.
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub enum SupervisorError {
    BinaryNotFound(PathBuf),
    Spawn(String),
    NotReady(String),
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisorError::BinaryNotFound(p) => write!(f, "blitzkrieg-core binary not found: {}", p.display()),
            SupervisorError::Spawn(e) => write!(f, "failed to spawn blitzkrieg-core: {e}"),
            SupervisorError::NotReady(e) => write!(f, "blitzkrieg-core did not become ready: {e}"),
        }
    }
}
impl std::error::Error for SupervisorError {}

/// Everything needed to spawn a core. Defaults mirror the production arguments
/// assembled by `src/core/blitzkrieg-core-runner.ts` (see `HANDOFF.md` §3).
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
    /// the Node command path (`HFT_ASSETS`, `HFT_ROUND_SEC`, `HFT_MIN_SHARES`,
    /// `HFT_MAX_SHARES`).
    pub fn from_env(socket_path: String) -> Self {
        let env_num = |k: &str, d: u64| -> u64 {
            std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
        };
        let assets = std::env::var("HFT_ASSETS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.split(',').map(|a| a.trim().to_uppercase()).collect())
            .unwrap_or_else(|| vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()]);
        let max_shares = env_num("HFT_MAX_SHARES", 10);
        let min_shares = env_num("HFT_MIN_SHARES", 10).min(max_shares);
        let dry = std::env::var("DRY_RUN").map(|v| v != "false").unwrap_or(true);
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
        let cwd = std::env::var("UIKIT_CORE_CWD").ok().filter(|s| !s.trim().is_empty()).map(PathBuf::from);
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
        root.join("Blitzkrieg_core/target/release/blitzkrieg-core"),
        root.join("target/debug/blitzkrieg-core"),
        root.join("Blitzkrieg_core/target/debug/blitzkrieg-core"),
    ];
    candidates.iter().find(|p| p.exists()).cloned().unwrap_or_else(|| candidates[0].clone())
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
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self { cfg, child: None, owns: false }
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

    /// Spawn the core (or adopt an existing one). Idempotent.
    pub fn start(&mut self) -> Result<StartOutcome, SupervisorError> {
        self.reap();
        if socket_served(&self.cfg.socket_path) {
            // A core already owns the socket — adopt it. Spawning here would only
            // produce a duplicate-core abort (the Node client makes the same
            // choice, see `tryAdopt`).
            self.child = None;
            self.owns = false;
            return Ok(StartOutcome::Adopted);
        }
        if !self.cfg.binary_path.exists() {
            return Err(SupervisorError::BinaryNotFound(self.cfg.binary_path.clone()));
        }
        let mut cmd = Command::new(&self.cfg.binary_path);
        cmd.args(self.cfg.to_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(dir) = &self.cfg.cwd {
            cmd.current_dir(dir);
        }
        let child = cmd.spawn().map_err(|e| SupervisorError::Spawn(e.to_string()))?;
        let pid = child.id();
        self.child = Some(child);
        self.owns = true;

        // Readiness handshake: the core binds the socket shortly after spawn.
        let deadline = Instant::now() + STARTUP_DEADLINE;
        while Instant::now() < deadline {
            if self.child.as_mut().and_then(|c| c.try_wait().ok()).flatten().is_some() {
                self.child = None;
                self.owns = false;
                return Err(SupervisorError::NotReady("process exited during startup".into()));
            }
            if socket_served(&self.cfg.socket_path) {
                return Ok(StartOutcome::Started { pid });
            }
            std::thread::sleep(POLL);
        }
        // Timed out — do not leave a half-started core behind.
        self.stop();
        Err(SupervisorError::NotReady("socket not bound within 15s".into()))
    }

    /// Stop the core **we spawned**. An adopted core is never killed.
    pub fn stop(&mut self) -> StopOutcome {
        self.reap();
        let Some(mut child) = self.child.take() else {
            self.owns = false;
            return StopOutcome::NotOwned;
        };
        let pid = child.id();
        send_sigterm(pid);
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.owns = false;
                    return StopOutcome::Stopped { pid };
                }
                Ok(None) => std::thread::sleep(POLL),
                Err(_) => break,
            }
        }
        // Escalate: SIGKILL. Crash recovery makes this safe (see module docs).
        let _ = child.kill();
        let _ = child.wait();
        self.owns = false;
        StopOutcome::Stopped { pid }
    }

    /// Clear our handle if the child exited on its own (no kill).
    fn reap(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.child = None;
                self.owns = false;
            }
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
}
