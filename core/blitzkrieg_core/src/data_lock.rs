//! The data-directory write gate (#199).
//!
//! On 2026-09-21 a **test fixture core** silently appended to the production
//! ledger: `data/trades/trades.jsonl` gained a fixture fill, `summary.json` was
//! rewritten, six fixture orders landed in `data/orders/orders.jsonl` and the
//! drift log picked up fixture rows — while the production core's in-memory
//! count still said 290. Nothing in the process could tell that the directory
//! it was about to append to was already owned by a live core, because the
//! default paths are **cwd-relative** (`data/trades/trades.jsonl`): any second
//! core started from the repo root writes the production ledger unless it
//! happens to be given explicit `--*-log` paths.
//!
//! One incident, two mechanisms. `ipc/server.rs:claim_socket_path` (#196) is the
//! other one and answers a **different question**:
//!
//! | question | mechanism | conflict reported |
//! |---|---|---|
//! | who is LISTENING on this control socket? | `claim_socket_path` (#196) | `another blitzkrieg-core is already listening on <socket>` |
//! | who is WRITING this data directory? | this module (#199) | `already writing this data directory` |
//!
//! They are deliberately not merged: a core with its own socket and the default
//! data paths (the incident's shape, minus the socket takeover) has no socket
//! conflict at all, and two cores on distinct sockets may legitimately share a
//! directory only if they say so.
//!
//! # Which directory is locked, and why
//!
//! The issue text says `data/.core-lock`. This module locks **the parent
//! directory of each effective ledger log** instead — `trade_log_path`,
//! `order_log_path`, `position_log_path`, and everything derived from them
//! (the order log's `applied-fills.jsonl` / `reconcile.jsonl` /
//! `settlements.jsonl` siblings, the position log's `.daily-loss.json` and
//! `.recon` watermarks), each deduplicated to one lock file:
//!
//! ```text
//! data/trades/.core-lock     data/orders/.core-lock     data/positions/.core-lock
//! ```
//!
//! The reasons, in order of weight:
//!
//! 1. **"`data/`" is not a fact, it is a cwd-relative default.** The harm is
//!    "this file got appended to", so the thing to claim is the directory that
//!    *actually* receives the append — the one the operator's `--trade-log`
//!    resolved to. A lock hardcoded to `<cwd>/data` would claim a directory
//!    nobody writes (a deployment with `--trade-log /var/lib/bk/trades.jsonl`)
//!    and leave the real one unguarded.
//! 2. **It keeps fixtures out of each other's way.** Gate scripts spawn cores
//!    into `mkdtemp` directories with explicit `--*-log` paths; anchoring on the
//!    effective paths means two such cores write their locks into two different
//!    temp directories and never refuse each other. An anchor that walked up to
//!    a shared ancestor would make concurrent gates collide the moment they
//!    shared a parent.
//! 3. **It fails only when the ledger is actually shared.** A core that
//!    disables its logs (`--no-trade-log --no-order-log --no-position-log`, what
//!    most harnesses pass) has no anchors and cannot be refused by this gate at
//!    all — the sockets stay the only singleton, exactly as before.
//!
//! The three ledgers are the anchor set because they are the **accounting truth**
//! the issue's blast radius is about: `positions`/`trades`/`reserved` identities,
//! `max_daily_loss_usd`, LossBreakers and `account:drift-check` are all built on
//! them, so a polluted ledger is a disabled risk limit. Capture and telemetry
//! paths (the event archive, near-miss records, the strategy-state file) are
//! *named* in the boot banner but not anchors: a fixture that records its own
//! market data into a temp directory shares no ledger with anyone, and blocking
//! that would only teach people to pass `--allow-shared-data` by reflex.
//!
//! # The escape hatch
//!
//! `--allow-shared-data` (or `BLITZKRIEG_ALLOW_SHARED_DATA=1`) is for fixtures
//! and backtests that share a directory on purpose. It never erases the evidence
//! of a live owner: when a live lock is found, this process proceeds *without
//! rewriting that lock file*, so a third core can still name who holds the
//! directory. Under the hatch the lock is advisory — that is the whole point of
//! having to ask for it.
//!
//! # What is deliberately NOT done
//!
//! A lock is **never removed on exit**. Two reasons: the acceptance criteria for
//! this issue want a cleanly-exited core's directory to be *taken over* (and the
//! takeover announced) rather than to look unused; and there is no shutdown hook
//! that can order a delete after the last in-flight append, so a delete could
//! race a starter into believing the directory is free while the exiting core is
//! still flushing. Leaving the record in place keeps strictly more information:
//! "a core owned this directory, and here is who it was".

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the lock file placed in every anchored directory.
pub const LOCK_FILE_NAME: &str = ".core-lock";

/// What a core records about itself in a data directory's lock file (#199).
///
/// `started_at` is the human-readable UTC form (what an operator reads out of a
/// `cat` during an incident) and `started_at_ms` the same instant in epoch
/// milliseconds (what the age computation uses, exact and timezone-free). Both
/// come from one clock read so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    pub pid: u32,
    pub started_at: String,
    pub started_at_ms: i64,
    pub socket: String,
    pub mode: String,
    pub version: String,
}

impl LockRecord {
    /// This process's record, as written into every anchor it claims.
    pub fn for_this_process(socket: &str, mode: &str, at_ms: i64) -> Self {
        Self {
            pid: std::process::id(),
            started_at: crate::data_source::utc_stamp(at_ms).unwrap_or_else(|| at_ms.to_string()),
            started_at_ms: at_ms,
            socket: socket.to_string(),
            mode: mode.to_string(),
            version: crate::ipc::build_info::version_string(),
        }
    }

    /// One line naming the owner, for refusals and boot banners.
    pub fn describe(&self, now_ms: i64) -> String {
        // Clamped at zero: a clock step backwards (or a lock written by a host
        // whose clock is ahead) must read "just now", never "-5s ago".
        let age = now_ms.saturating_sub(self.started_at_ms).max(0);
        format!(
            "pid {} (started {}, {} ago; mode {}, socket {}, version {})",
            self.pid,
            self.started_at,
            age_human(age),
            self.mode,
            self.socket,
            self.version
        )
    }
}

/// A directory this process will append into, and the effective logs that put it
/// there (`["trade-log"]`, `["order-log", "position-log"]` when two ledgers share
/// one directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub dir: PathBuf,
    pub logs: Vec<String>,
}

impl Anchor {
    /// The lock file this anchor claims.
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE_NAME)
    }
}

/// Build the anchor set from the *effective* ledger paths.
///
/// `logs` is `(label, path)` pairs where the label names the flag the path came
/// from (`"trade-log"`, …) and `None` means that log is off. Two logs in one
/// directory collapse into a single anchor: one directory, one lock, one claim.
pub fn ledger_anchors(logs: &[(&str, Option<&str>)]) -> Vec<Anchor> {
    let mut out: Vec<Anchor> = Vec::new();
    for (label, path) in logs {
        let Some(path) = path else { continue };
        if path.trim().is_empty() {
            continue;
        }
        let file = absolute(Path::new(path));
        let Some(dir) = file.parent() else { continue };
        let dir = dir.to_path_buf();
        match out.iter_mut().find(|a| a.dir == dir) {
            Some(existing) => {
                if !existing.logs.iter().any(|l| l == label) {
                    existing.logs.push(label.to_string());
                }
            }
            None => out.push(Anchor {
                dir,
                logs: vec![label.to_string()],
            }),
        }
    }
    out
}

/// The state an anchor was in when this process claimed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorState {
    /// No lock file: this process is the directory's first recorded writer.
    Claimed,
    /// A lock file was left by a core that is gone. `previous` is `None` when the
    /// file existed but could not be read as a lock record (taken over only
    /// under the escape hatch — see [`DataLock::acquire`]).
    TookOverStale { previous: Option<LockRecord> },
    /// A LIVE core owns the directory, and the escape hatch was given. That
    /// core's lock file is left exactly as it was.
    Shared { owner: LockRecord },
}

/// One anchored directory and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAnchor {
    pub dir: PathBuf,
    pub state: AnchorState,
}

impl HeldAnchor {
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE_NAME)
    }
}

/// An anchor this process refused to touch, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyAnchor {
    pub dir: PathBuf,
    pub lock: PathBuf,
    /// The live owner, when its record could be read.
    pub owner: Option<LockRecord>,
    /// Why the lock could not be attributed (a corrupt record), when it could not.
    pub why: Option<String>,
}

/// The gate refused: at least one anchored directory is in use. Carries every
/// busy anchor, not just the first, so one boot tells the operator all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirsBusy {
    pub busy: Vec<BusyAnchor>,
}

impl std::fmt::Display for DataDirsBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "refusing to start: another blitzkrieg-core is already writing this data directory \
             (this is a DATA conflict, not the socket singleton of #196)"
        )?;
        for b in &self.busy {
            match &b.owner {
                Some(o) => writeln!(
                    f,
                    "  {} (lock {}): held by {}",
                    b.dir.display(),
                    b.lock.display(),
                    o.describe(crate::ipc::server::now_ms())
                )?,
                None => writeln!(
                    f,
                    "  {} (lock {}): unreadable lock record ({})",
                    b.dir.display(),
                    b.lock.display(),
                    b.why.as_deref().unwrap_or("unknown")
                )?,
            }
        }
        write!(
            f,
            "This core will not write a single line. Stop the core above, or point this one at its \
             own --trade-log/--order-log/--position-log, or pass --allow-shared-data \
             (BLITZKRIEG_ALLOW_SHARED_DATA=1) if the directory is shared on purpose \
             (a fixture or a backtest)."
        )
    }
}

impl std::error::Error for DataDirsBusy {}

/// The lock this process holds for its lifetime.
///
/// Dropping it does nothing on purpose: see the module docs — the record stays
/// on disk so the next core can report who the last writer was.
#[derive(Debug, Clone)]
pub struct DataLock {
    held: Vec<HeldAnchor>,
}

impl DataLock {
    /// Claim every anchored directory, or refuse and write nothing at all.
    ///
    /// The order is what makes the second half of that sentence true: **all**
    /// anchors are decided (phase 1, read-only) before **any** lock file is
    /// written (phase 2). A refusal therefore creates no directory, no lock file
    /// and no ledger line — verified by
    /// `a_refusal_writes_no_lock_file_anywhere`.
    ///
    /// A lock whose pid is alive blocks the start. A lock whose pid is gone (or
    /// whose record cannot be read under `allow_shared`) is taken over.
    pub fn acquire(
        anchors: &[Anchor],
        record: &LockRecord,
        allow_shared: bool,
    ) -> Result<Self, DataDirsBusy> {
        let mut planned: Vec<(&Anchor, AnchorState)> = Vec::with_capacity(anchors.len());
        let mut busy: Vec<BusyAnchor> = Vec::new();
        // Phase 1 — decide everything, write nothing.
        for anchor in anchors {
            let lock = anchor.lock_path();
            match read_lock(&lock) {
                LockFile::Absent => planned.push((anchor, AnchorState::Claimed)),
                LockFile::Held(owner) => {
                    if pid_alive(owner.pid) {
                        if allow_shared {
                            planned.push((anchor, AnchorState::Shared { owner }));
                        } else {
                            busy.push(BusyAnchor {
                                dir: anchor.dir.clone(),
                                lock,
                                owner: Some(owner),
                                why: None,
                            });
                        }
                    } else {
                        planned.push((
                            anchor,
                            AnchorState::TookOverStale {
                                previous: Some(owner),
                            },
                        ));
                    }
                }
                LockFile::Unreadable(why) => {
                    if allow_shared {
                        planned.push((anchor, AnchorState::TookOverStale { previous: None }));
                    } else {
                        busy.push(BusyAnchor {
                            dir: anchor.dir.clone(),
                            lock,
                            owner: None,
                            why: Some(why),
                        });
                    }
                }
            }
        }
        if !busy.is_empty() {
            return Err(DataDirsBusy { busy });
        }
        // Phase 2 — every anchor is free (or shared on purpose): now write.
        let mut held = Vec::with_capacity(planned.len());
        for (anchor, state) in planned {
            // Never overwrite a live owner's record: under the escape hatch the
            // record of WHO is writing this directory is the only evidence left.
            if !matches!(state, AnchorState::Shared { .. })
                && let Err(e) = write_lock(&anchor.lock_path(), record)
            {
                // A directory that cannot take a lock file must not be written to
                // silently: fail the boot rather than trade into an unguarded
                // ledger.
                return Err(DataDirsBusy {
                    busy: vec![BusyAnchor {
                        dir: anchor.dir.clone(),
                        lock: anchor.lock_path(),
                        owner: None,
                        why: Some(format!("could not write the lock file: {e}")),
                    }],
                });
            }
            held.push(HeldAnchor {
                dir: anchor.dir.clone(),
                state,
            });
        }
        Ok(Self { held })
    }

    /// Every anchor this process claimed, in derivation order.
    pub fn held(&self) -> &[HeldAnchor] {
        &self.held
    }

    /// The boot banner lines for this acquisition: the takeover and shared-dir
    /// notices an operator must see without asking. Empty when every anchor was
    /// claimed cleanly (the caller still prints the paths and the pid).
    pub fn banner(&self, now_ms: i64) -> Vec<String> {
        let mut out = Vec::new();
        for h in &self.held {
            match &h.state {
                AnchorState::Claimed => {}
                AnchorState::TookOverStale {
                    previous: Some(prev),
                } => out.push(format!(
                    "took over stale lock from pid {} in {} (lock {}; previous core: {})",
                    prev.pid,
                    h.dir.display(),
                    h.lock_path().display(),
                    prev.describe(now_ms)
                )),
                AnchorState::TookOverStale { previous: None } => out.push(format!(
                    "took over a stale lock with an unreadable record in {} (lock {})",
                    h.dir.display(),
                    h.lock_path().display()
                )),
                AnchorState::Shared { owner } => out.push(format!(
                    "WARNING SHARED DATA DIRECTORY: {} is owned by a LIVE core {} and \
                     --allow-shared-data was given; this core will append to the SAME ledger. \
                     Accounting identities and risk limits (max_daily_loss_usd, LossBreakers, \
                     account:drift-check) read that ledger — do not do this in production.",
                    h.dir.display(),
                    owner.describe(now_ms)
                )),
            }
        }
        out
    }
}

/// A lock file's state on disk.
enum LockFile {
    Absent,
    Held(LockRecord),
    Unreadable(String),
}

fn read_lock(path: &Path) -> LockFile {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockFile::Absent,
        Err(e) => return LockFile::Unreadable(format!("{e}")),
    };
    match serde_json::from_str::<LockRecord>(text.trim()) {
        Ok(rec) => LockFile::Held(rec),
        Err(e) => LockFile::Unreadable(format!("not a lock record: {e}")),
    }
}

/// Write the record as a single small file, via a temp file + rename.
///
/// The rename is what keeps [`read_lock`]'s "unreadable ⇒ fail closed" rule from
/// becoming a self-inflicted outage: a half-written record can never be
/// observed, because the name it is read from only ever points at a complete
/// one. The leftover temp name is pid-suffixed so two processes cannot collide,
/// and is best-effort removed if the rename fails.
fn write_lock(path: &Path, record: &LockRecord) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = serde_json::to_vec(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    let tmp = path.with_file_name(format!("{LOCK_FILE_NAME}.{}.tmp", record.pid));
    std::fs::write(&tmp, &bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Is `pid` a live process?
///
/// `kill(pid, 0)` sends no signal: it returns 0 when the process exists, `EPERM`
/// when it exists but belongs to another user (still alive — which is why
/// `EPERM` must count as alive), and `ESRCH` when there is no such process.
///
/// Two spellings are refused rather than probed: `0` (which `kill` reads as "the
/// whole process group", i.e. always alive) and anything above `i32::MAX` (not
/// representable as a pid). A record carrying either is treated as gone, so a
/// corrupt or hand-edited pid fails toward *taking over* — see the module docs
/// for why an unattributable lock is the one case that fails closed instead.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only the existence/permission check
    // and cannot deliver a signal or mutate state.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Absolute form of `path`, lexically resolved against the current directory.
///
/// Used for the boot banner ("where does this core actually write?") and for
/// anchor de-duplication. Deliberately not `canonicalize`: the banner must name
/// paths that do not exist yet, and a symlinked `/tmp` on macOS is not what an
/// operator typed. Lexical resolution means two spellings that differ only by
/// `.`/`..` collapse, while a symlink stays spelled as given.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `12s` / `3m` / `2h` / `5d` for a duration in milliseconds.
fn age_human(ms: i64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A pid no process can have: Linux's default `pid_max` is 4 194 304 and
    /// macOS caps pids at 99 998, so this is far above both while still fitting
    /// in `pid_t`. Used instead of spawning-and-reaping a child so the test is
    /// deterministic and cannot leave a process behind.
    const DEAD_PID: u32 = 1_000_000_000;

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bk-datalock-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn record() -> LockRecord {
        // A fixed instant, so `started_at` assertions do not race the clock.
        LockRecord::for_this_process("/tmp/test.sock", "dry", 1_758_451_199_000)
    }

    fn anchor(dir: &Path) -> Anchor {
        Anchor {
            dir: dir.to_path_buf(),
            logs: vec!["trade-log".to_string()],
        }
    }

    fn write_record(dir: &Path, rec: &LockRecord) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(LOCK_FILE_NAME),
            serde_json::to_string(rec).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn pid_alive_sees_this_process_and_not_a_pid_that_cannot_exist() {
        assert!(pid_alive(std::process::id()), "our own pid is alive");
        assert!(!pid_alive(DEAD_PID), "no process can hold this pid");
        // Not a probe: `kill(0, 0)` would target the whole process group and
        // always report "alive", which would make every zeroed record block a
        // directory forever.
        assert!(
            !pid_alive(0),
            "pid 0 is not a process, it is a process group"
        );
        assert!(!pid_alive(u32::MAX), "not representable as a pid_t");
    }

    #[test]
    fn a_live_owner_refuses_the_second_core_and_changes_nothing_on_disk() {
        let dir = scratch("live");
        // Our own pid is the strongest available "definitely alive".
        let owner = record();
        write_record(&dir, &owner);
        let lock = dir.join(LOCK_FILE_NAME);
        let before = std::fs::read_to_string(&lock).unwrap();

        let err = DataLock::acquire(&[anchor(&dir)], &owner, false)
            .expect_err("a live owner must refuse the second core");
        assert_eq!(err.busy.len(), 1);
        assert_eq!(err.busy[0].dir, dir);
        assert_eq!(err.busy[0].owner.as_ref().map(|o| o.pid), Some(owner.pid));
        // The refusal names the occupant: pid, socket, mode and start time.
        let msg = err.to_string();
        assert!(msg.contains(&format!("pid {}", owner.pid)), "{msg}");
        assert!(msg.contains("/tmp/test.sock"), "{msg}");
        assert!(msg.contains("mode dry"), "{msg}");
        assert!(msg.contains(&owner.started_at), "{msg}");
        // "must not write a single line" includes the lock file: byte-identical.
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "nothing new in the dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_writes_no_lock_file_anywhere() {
        // Anchors are decided in full before any of them is written, so a free
        // directory earlier in the list must stay free when a later one is busy.
        let free = scratch("free");
        let busy = scratch("busy");
        write_record(&busy, &record());
        let err = DataLock::acquire(&[anchor(&free), anchor(&busy)], &record(), false)
            .expect_err("the busy anchor must refuse the whole start");
        assert_eq!(err.busy.len(), 1);
        assert!(
            !free.join(LOCK_FILE_NAME).exists(),
            "a partial acquisition leaked a lock file into a free directory"
        );
        let _ = std::fs::remove_dir_all(&free);
        let _ = std::fs::remove_dir_all(&busy);
    }

    #[test]
    fn a_dead_pid_is_taken_over_and_announced() {
        let dir = scratch("stale");
        let mut dead = record();
        dead.pid = DEAD_PID;
        write_record(&dir, &dead);

        let lock = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect("a lock whose pid is gone must be taken over");
        assert_eq!(
            lock.held()[0].state,
            AnchorState::TookOverStale {
                previous: Some(dead.clone())
            }
        );
        let banner = lock.banner(1_758_451_199_000);
        assert_eq!(banner.len(), 1);
        assert!(
            banner[0].contains(&format!("took over stale lock from pid {DEAD_PID}")),
            "{}",
            banner[0]
        );
        // The takeover rewrites the record: the directory is now OURS, so a third
        // core sees the live owner, not the stale one.
        let now = read_lock(&dir.join(LOCK_FILE_NAME));
        match now {
            LockFile::Held(rec) => assert_eq!(rec.pid, std::process::id()),
            other => panic!(
                "expected our own record after takeover, got {}",
                match other {
                    LockFile::Absent => "absent".to_string(),
                    LockFile::Unreadable(w) => format!("unreadable: {w}"),
                    LockFile::Held(r) => format!("held by {}", r.pid),
                }
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cleanly_exited_core_leaves_a_stale_lock_the_next_one_takes_over() {
        // Requirement 2 of the issue: a normal exit must NOT delete the lock —
        // the next core takes it over and says so. Modelled with a dead pid.
        let dir = scratch("exit");
        let mut previous = record();
        previous.pid = DEAD_PID;
        write_record(&dir, &previous);
        let lock = DataLock::acquire(&[anchor(&dir)], &record(), false).expect("takeover");
        assert!(lock.banner(0)[0].contains("took over stale lock"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_escape_hatch_starts_anyway_and_says_so_without_erasing_the_owner() {
        let dir = scratch("shared");
        let owner = record();
        write_record(&dir, &owner);
        let lock_file = dir.join(LOCK_FILE_NAME);
        let before = std::fs::read_to_string(&lock_file).unwrap();

        let lock = DataLock::acquire(&[anchor(&dir)], &owner, true)
            .expect("--allow-shared-data must let the core start");
        assert_eq!(
            lock.held()[0].state,
            AnchorState::Shared {
                owner: owner.clone()
            }
        );
        let banner = lock.banner(1_758_451_199_000);
        assert_eq!(banner.len(), 1);
        assert!(
            banner[0].contains("WARNING SHARED DATA DIRECTORY"),
            "{}",
            banner[0]
        );
        assert!(
            banner[0].contains(&format!("pid {}", owner.pid)),
            "{}",
            banner[0]
        );
        // The live owner's record survives: a third core can still name it.
        assert_eq!(std::fs::read_to_string(&lock_file).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_lock_is_refused_with_the_remedy_named() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join(LOCK_FILE_NAME), b"{ not json").unwrap();
        let err = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect_err("an unattributable lock must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("unreadable lock record"), "{msg}");
        assert!(msg.contains("--allow-shared-data"), "{msg}");
        // Under the hatch it proceeds (the operator said they know).
        let lock = DataLock::acquire(&[anchor(&dir)], &record(), true).expect("shared");
        assert_eq!(
            lock.held()[0].state,
            AnchorState::TookOverStale { previous: None }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_directory_is_claimed_and_recorded() {
        let dir = scratch("fresh");
        let rec = record();
        let lock = DataLock::acquire(&[anchor(&dir)], &rec, false).expect("first writer");
        assert_eq!(lock.held()[0].state, AnchorState::Claimed);
        assert!(lock.banner(0).is_empty(), "a clean claim needs no notice");
        let on_disk: LockRecord = serde_json::from_str(
            std::fs::read_to_string(dir.join(LOCK_FILE_NAME))
                .unwrap()
                .trim(),
        )
        .expect("the lock file is a lock record");
        assert_eq!(on_disk, rec);
        // Every field the issue asks the lock to carry.
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(LOCK_FILE_NAME)).unwrap())
                .unwrap();
        for key in ["pid", "started_at", "socket", "mode", "version"] {
            assert!(
                raw.get(key).is_some(),
                "lock record is missing {key}: {raw}"
            );
        }
        assert_eq!(raw["started_at"], serde_json::json!("20250921T103959Z"));
        assert_eq!(raw["mode"], serde_json::json!("dry"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_parent_directory_is_created_for_the_lock() {
        let dir = scratch("nested").join("data").join("trades");
        let lock = DataLock::acquire(&[anchor(&dir)], &record(), false).expect("claim");
        assert_eq!(lock.held()[0].state, AnchorState::Claimed);
        assert!(dir.join(LOCK_FILE_NAME).exists());
        // No temp file survives a successful write.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![LOCK_FILE_NAME.to_string()], "{names:?}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn anchors_follow_the_effective_paths_and_dedup_per_directory() {
        // Default production shape: three ledgers, three sibling directories.
        let a = ledger_anchors(&[
            ("trade-log", Some("data/trades/trades.jsonl")),
            ("order-log", Some("data/orders/orders.jsonl")),
            ("position-log", Some("data/positions/positions.jsonl")),
        ]);
        assert_eq!(a.len(), 3);
        assert!(a[0].dir.ends_with("data/trades"));
        assert!(a[0].lock_path().ends_with("data/trades/.core-lock"));
        // A harness that points every ledger at one temp directory gets ONE lock.
        let b = ledger_anchors(&[
            ("trade-log", Some("/tmp/bk/trades.jsonl")),
            ("order-log", Some("/tmp/bk/orders.jsonl")),
            ("position-log", Some("/tmp/bk/positions.jsonl")),
        ]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].logs, vec!["trade-log", "order-log", "position-log"]);
        // Logs that are off contribute nothing: a harness with all three disabled
        // cannot be refused by this gate at all.
        assert!(
            ledger_anchors(&[
                ("trade-log", None),
                ("order-log", None),
                ("position-log", None)
            ])
            .is_empty()
        );
        // Two spellings of one directory are one anchor.
        let c = ledger_anchors(&[
            ("trade-log", Some("/tmp/bk/./trades.jsonl")),
            ("order-log", Some("/tmp/bk/orders.jsonl")),
        ]);
        assert_eq!(c.len(), 1, "{c:?}");
        // The anchor is the log's PARENT, not the cwd-relative "data/".
        let d = ledger_anchors(&[("trade-log", Some("/var/lib/bk/ledger/trades.jsonl"))]);
        assert_eq!(d[0].dir, PathBuf::from("/var/lib/bk/ledger"));
    }

    #[test]
    fn absolute_paths_are_what_the_banner_prints() {
        let dir = scratch("abs");
        let rel = PathBuf::from("data/trades/trades.jsonl");
        let abs = absolute(&rel);
        assert!(abs.is_absolute(), "{abs:?}");
        assert!(abs.ends_with("data/trades/trades.jsonl"));
        assert_eq!(
            absolute(&dir.join("x")),
            dir.join("x"),
            "an absolute path is unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_record_carries_a_human_readable_utc_start_time() {
        // 1_758_451_199_000 ms = 2025-09-21T10:39:59Z.
        let rec = LockRecord::for_this_process("/tmp/s.sock", "live", 1_758_451_199_000);
        assert_eq!(rec.started_at, "20250921T103959Z");
        assert_eq!(rec.started_at_ms, 1_758_451_199_000);
        assert_eq!(rec.pid, std::process::id());
        assert!(rec.version.starts_with(crate::CORE_VERSION));
        // The age is the operator-facing part of the refusal: "held by pid N,
        // started 2h ago" is what tells them whether the occupant is the
        // production core or yesterday's fixture.
        let at = |plus_ms: i64| rec.describe(1_758_451_199_000 + plus_ms);
        assert!(at(0).contains("0s ago"), "{}", at(0));
        assert!(at(59_000).contains("59s ago"), "{}", at(59_000));
        assert!(at(60_000).contains("1m ago"), "{}", at(60_000));
        assert!(at(7_200_000).contains("2h ago"), "{}", at(7_200_000));
        assert!(at(172_800_000).contains("2d ago"), "{}", at(172_800_000));
        // A clock that went backwards (an NTP step, a lock written on another
        // host) must not print a negative age.
        assert!(at(-5_000).contains("0s ago"), "{}", at(-5_000));
    }
}
