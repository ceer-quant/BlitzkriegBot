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
//! # How the claim is made atomic (#233)
//!
//! The record file alone cannot answer "is anyone writing this directory?"
//! atomically, and the first version of this gate tried to: it read every lock
//! file, decided, and then wrote every lock file. Two cores started in the same
//! instant therefore BOTH saw "no lock file", BOTH wrote one, and BOTH served —
//! the #199 incident with an extra witness, because a check and a write are two
//! syscalls and POSIX has no compare-and-swap for a path. Reproduced 8/8 in the
//! issue. The plausible repair — unlink the stale file and re-create it with
//! `O_EXCL` — is not one either: unlinking is exactly the operation two racers
//! can interleave so that each removes the other's freshly created name, and a
//! `rename` "takeover" has the same shape (the loser's rename lands after the
//! winner's read-back).
//!
//! So the claim is the **kernel's advisory lock on the directory**, taken
//! non-blocking, and *only its holder* reads the record and decides:
//!
//! ```text
//! open(dir) → try_lock() → the one process that gets it owns the directory
//! ```
//!
//! Three properties make this the right primitive here rather than a pidfile
//! trick. It is atomic by construction — one open file description gets
//! `LOCK_EX`, everyone else gets `EWOULDBLOCK`, with no window in between. It
//! **cannot outlive its owner**: the kernel drops the lock when the process
//! dies, `SIGKILL` and a crash between the claim and the record write included,
//! so a stale directory is reclaimed by the next core with no cleanup pass, no
//! age heuristic and no way to end up locked forever. And it needs nothing from
//! the record, which stays what it always was — the evidence an operator reads
//! (`who held this, since when`) and the *only* thing that can name a live owner
//! from a build older than this one (see the pid rule in [`DataLock::acquire`],
//! which predates this lock and is kept).
//!
//! The lock is taken on the **directory**, not on `.core-lock`, and that is not
//! a detail: [`write_lock`] replaces the record with `rename`, so the record's
//! inode changes on every write, while a directory keeps its inode for as long
//! as it is that directory. A lock placed on the record would stop guarding the
//! directory the first time its owner rewrote its own record.
//!
//! Nothing is created for the lock itself — no lock file to leave behind, and no
//! artifact for a crashed core to litter — and the record file keeps its atomic
//! `temp + rename` write, so a crash can never leave a half-written record for
//! the next core to trip over.
//!
//! # The escape hatch
//!
//! `--allow-shared-data` (or `BLITZKRIEG_ALLOW_SHARED_DATA=1`) is for fixtures
//! and backtests that share a directory on purpose. It never erases the evidence
//! of a live owner: when a live lock is found, this process proceeds *without
//! rewriting that lock file*, so a third core can still name who holds the
//! directory. Under the hatch the lock is advisory — that is the whole point of
//! having to ask for it — including the kernel's, which is not taken when
//! another process holds it, and which is not required at all on a filesystem
//! whose lock API this build cannot use.
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
//!
//! The kernel claim is the deliberate opposite: it is held for as long as this
//! process lives, and released by the *kernel* when it dies — including when it
//! dies badly. Nothing in this process drops it early, because "released while
//! still flushing" is the state the whole gate exists to prevent. It is also not
//! cloned or passed around: one [`DataLock`] per process, one claim per
//! directory.

use serde::{Deserialize, Serialize};
use std::fs::File;
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
                    o.describe(blitzkrieg_market_api::net::now_ms())
                )?,
                None => writeln!(
                    f,
                    "  {} (lock {}): unreadable lock record ({})",
                    b.dir.display(),
                    b.lock.display(),
                    b.why.as_deref().unwrap_or("unknown")
                )?,
            }
            // #233: a directory can be busy for a reason the record does not
            // show — a live process holds the kernel lock and has not rewritten
            // its (or its predecessor's) record yet. Printing only "held by pid
            // N" would invite an operator to check a pid that has since exited.
            if b.owner.is_some()
                && let Some(why) = &b.why
            {
                writeln!(f, "    {why}")?;
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
/// Dropping it releases the kernel claims and does nothing else on purpose: see
/// the module docs — the record stays on disk so the next core can report who
/// the last writer was. Deliberately not `Clone`: a kernel claim is not something
/// to hand around, and one process holds one claim per directory.
#[derive(Debug)]
pub struct DataLock {
    held: Vec<HeldAnchor>,
    /// #233: the kernel claims, one per anchored directory, alive as long as this
    /// value is — held, never read. The list exists so the claims outlive
    /// `acquire`: the kernel lock must stay in force for as long as this process
    /// may write, and dropping this (or dying) is what releases it.
    _claims: Vec<DirClaim>,
}

impl DataLock {
    /// Claim every anchored directory, or refuse and write nothing at all.
    ///
    /// One anchor at a time (#233): take the kernel lock on the directory, then —
    /// as its holder — read the record and decide. A directory whose kernel lock
    /// another process holds is busy, whatever its record says; a directory whose
    /// kernel lock we got is decided by the record, which either names a live
    /// owner from a build that predates this lock (busy), a dead one (takeover,
    /// announced), or nobody (a plain claim).
    ///
    /// A refusal creates no lock file, writes no ledger line, and releases every
    /// claim it took: the kernel locks are dropped and the directories this call
    /// created are removed again while they are still empty. The record file is
    /// only written in phase 2, after every anchor has been decided, so a refusal
    /// cannot rewrite a stale record either.
    pub fn acquire(
        anchors: &[Anchor],
        record: &LockRecord,
        allow_shared: bool,
    ) -> Result<Self, DataDirsBusy> {
        let mut planned: Vec<(&Anchor, AnchorState)> = Vec::with_capacity(anchors.len());
        let mut busy: Vec<BusyAnchor> = Vec::new();
        let mut claims: Vec<DirClaim> = Vec::with_capacity(anchors.len());
        // Phase 1 — decide everything and take every kernel lock, write no record.
        for anchor in anchors {
            match decide(anchor, allow_shared) {
                Ok((state, claim)) => {
                    if let Some(claim) = claim {
                        claims.push(claim);
                    }
                    planned.push((anchor, state));
                }
                Err(b) => busy.push(*b),
            }
        }
        if !busy.is_empty() {
            for claim in claims {
                claim.release();
            }
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
        Ok(Self {
            held,
            _claims: claims,
        })
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

/// #233: the kernel's claim on one data directory, held for as long as this
/// process may write it.
///
/// The lock is taken on the directory rather than on the `.core-lock` file, and
/// the module docs say why (the record's inode changes on every write, the
/// directory's does not). Nothing is created for it, and nothing has to clean it
/// up: the kernel releases it when this process dies, however it dies.
#[derive(Debug)]
struct DirClaim {
    /// The open directory the lock is held on — held, never read: the lock lives
    /// exactly as long as this file descriptor's open file description, and the
    /// kernel releases it when the process dies. Dropping the claim is the
    /// release, and [`Drop`] performs it explicitly rather than by close alone
    /// (see there for why).
    _file: File,
    dir: PathBuf,
    /// True when this call created the directory. Only a directory this call
    /// created may be removed again on a later refusal, and only while it is
    /// still empty: a second core must take this same lock before it can write
    /// anything into it.
    created_dir: bool,
}

impl DirClaim {
    /// Take `dir`'s kernel lock, creating the directory when it is absent.
    ///
    /// `Ok(None)` is "a live process holds it" — the caller reads the record to
    /// name that process. `Err` is the lock API itself failing, which the caller
    /// treats as busy (a directory that cannot be guarded must not be written
    /// to) unless the escape hatch is in force.
    fn take(dir: &Path) -> Result<Option<Self>, String> {
        let created_dir = if dir.is_dir() {
            false
        } else {
            std::fs::create_dir_all(dir).map_err(|e| {
                format!("could not create the data directory {}: {e}", dir.display())
            })?;
            true
        };
        let file = File::open(dir).map_err(|e| {
            format!(
                "could not open the data directory {} to claim it: {e}",
                dir.display()
            )
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                dir: dir.to_path_buf(),
                created_dir,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(format!(
                "could not take the kernel lock on {}: {e}",
                dir.display()
            )),
        }
    }

    /// Give the claim back: release the kernel lock and, if this claim created
    /// the directory, remove it again while it is still empty (best effort — a
    /// directory something else has since written into is left alone).
    ///
    /// Only ever called on a refusal, i.e. on a path where this process exits
    /// without writing a line.
    fn release(self) {
        if self.created_dir {
            let _ = std::fs::remove_dir(&self.dir);
        }
        // `self` drops here; `Drop` releases the kernel lock explicitly.
    }
}

impl Drop for DirClaim {
    /// Release the kernel lock the moment the claim goes away (#311).
    ///
    /// Closing the descriptor is NOT enough to release it: the lock lives on
    /// the open file description, and a duplicate of this descriptor keeps
    /// that description — and the lock with it — alive past the close. A
    /// concurrent `fork` makes exactly such a duplicate (the child holds it
    /// from fork until exec), which is why the corrupt-lock test saw a lock
    /// this process had already released still held, and flaked as
    /// `WouldBlock`. `unlock` clears the lock itself, immediately, whatever
    /// else still refers to the description.
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Why a directory can be busy in a way its record does not show (#233): the
/// kernel lock is held, so a live process is writing it — and the record it
/// names may be its predecessor's for the moment between the claim and its own
/// record write.
const KERNEL_HELD_NOTE: &str = "note: a live process holds the kernel lock on this directory; the record above \
     is only rewritten after that claim, so its pid can lag by that moment";

/// What one anchored directory's state means for this boot, and the kernel claim
/// taken on it (if any).
type Decision = (AnchorState, Option<DirClaim>);

/// Decide one anchor as the holder of its kernel lock (#233).
///
/// The order is the fix: the kernel lock is taken FIRST, and only its holder is
/// allowed to read the record and decide. Two cores starting in the same instant
/// therefore cannot both pass — the second is stopped by the kernel, not by a
/// read it lost a race against.
///
/// The refusal is boxed: `BusyAnchor` carries a whole `LockRecord` plus two
/// paths, and the happy path returns one of these per anchor — `clippy` is right
/// that the value belongs on the heap on the path that does not use it.
fn decide(anchor: &Anchor, allow_shared: bool) -> Result<Decision, Box<BusyAnchor>> {
    let dir = &anchor.dir;
    let lock = anchor.lock_path();
    let claim = match DirClaim::take(dir) {
        Ok(Some(claim)) => Some(claim),
        // A live process holds it. Whatever the record says, this one does not
        // write this directory: the kernel lock is the claim, and the record is
        // the evidence an operator reads afterwards.
        Ok(None) => {
            let owner = read_owner(&lock);
            return match (allow_shared, owner) {
                // The hatch: the directory is shared on purpose, and the live
                // owner's record is left exactly as it is — this process takes no
                // kernel claim, so a third core still sees that owner.
                (true, Some(owner)) => Ok((AnchorState::Shared { owner }, None)),
                (true, None) => Err(Box::new(BusyAnchor {
                    dir: dir.clone(),
                    lock,
                    owner: None,
                    why: Some(format!(
                        "another process holds the kernel lock on {} and has not recorded itself yet; \
                         it is in the moment between claiming the directory and writing its record",
                        dir.display()
                    )),
                })),
                (false, owner) => Err(Box::new(BusyAnchor {
                    dir: dir.clone(),
                    lock,
                    owner,
                    why: Some(KERNEL_HELD_NOTE.to_string()),
                })),
            };
        }
        // The lock API itself refused (an exotic filesystem, a permission). Fail
        // closed: the alternative is the two-writer state #233 removed. The
        // escape hatch is the way out, because it is the one mode in which this
        // lock is advisory by construction — and then the record below is the
        // only guard left, exactly as it was before #233.
        Err(why) if !allow_shared => {
            return Err(Box::new(BusyAnchor {
                dir: dir.clone(),
                lock,
                owner: None,
                why: Some(why),
            }));
        }
        Err(_) => None,
    };
    // We hold the kernel lock on this directory (or the hatch made it advisory).
    match read_lock(&lock) {
        LockFile::Absent => Ok((AnchorState::Claimed, claim)),
        LockFile::Held(owner) => {
            if pid_alive(owner.pid) {
                // A live owner that holds no kernel lock is a core from a build
                // older than this one (or a hand-written record): the pid rule is
                // the only thing that can refuse it, which is why it is kept.
                if allow_shared {
                    Ok((AnchorState::Shared { owner }, claim))
                } else {
                    Err(Box::new(BusyAnchor {
                        dir: dir.clone(),
                        lock,
                        owner: Some(owner),
                        why: None,
                    }))
                }
            } else {
                Ok((
                    AnchorState::TookOverStale {
                        previous: Some(owner),
                    },
                    claim,
                ))
            }
        }
        LockFile::Unreadable(why) => {
            if allow_shared {
                Ok((AnchorState::TookOverStale { previous: None }, claim))
            } else {
                Err(Box::new(BusyAnchor {
                    dir: dir.clone(),
                    lock,
                    owner: None,
                    why: Some(why),
                }))
            }
        }
    }
}

/// The record naming whoever holds `lock`, with a short bounded retry while the
/// file is ABSENT.
///
/// Absent is the interesting case: this process holds the kernel lock because it
/// just took it, and a claimer writes its record a moment later, so a core
/// arriving in between sees a directory claimed for the first time with no file
/// in it yet. Waiting (at most 100 ms) is what turns "unknown" into the real
/// record in the refusal. An unreadable record is not retried: it will not become
/// readable by waiting.
fn read_owner(lock: &Path) -> Option<LockRecord> {
    for attempt in 0..20 {
        match read_lock(lock) {
            LockFile::Held(record) => return Some(record),
            LockFile::Unreadable(_) => return None,
            LockFile::Absent if attempt < 19 => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            LockFile::Absent => return None,
        }
    }
    None
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
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

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

    /// #311: the release must not be delayed by a descriptor a concurrent
    /// `fork` copied out of this process.
    ///
    /// The lock lives on the open file description, and a duplicated descriptor
    /// keeps that description — and the lock with it — alive past the close the
    /// release performs. A forked child holds exactly such a copy until it
    /// execs, which is why the corrupt-lock test flaked as `WouldBlock` on its
    /// own second `acquire` whenever the parallel suite had a child in flight.
    /// `try_clone` produces the child's copy deterministically, with no fork.
    #[test]
    fn a_release_is_not_delayed_by_a_duplicated_descriptor() {
        let dir = scratch("dup-release");
        let claim = DirClaim::take(&dir)
            .expect("take never fails on a scratch dir")
            .expect("the scratch dir is unclaimed");
        // Stand in for the fd table a concurrent `fork` copied: same open file
        // description, second descriptor, still open when the claim is released.
        let forked = claim._file.try_clone().expect("duplicate the claim's fd");
        drop(claim); // the release the second `acquire` is racing

        let reclaimed = DirClaim::take(&dir)
            .expect("take never fails on a scratch dir")
            .expect("a release must clear the lock itself, not defer it to close");
        drop(reclaimed);
        drop(forked);
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

    // ── #233: the atomic claim ──────────────────────────────────────────────
    //
    // The defect these cover was "read every lock file, decide, write every lock
    // file", which two cores starting in the same instant both pass. The tests
    // below are ordered from the primitive to the whole thing: the kernel lock on
    // a directory outranks a stale record, it survives the same process asking
    // twice, it holds across a real second process, it is released by that
    // process's death, and it never lets two concurrent claimers both win.

    /// The kernel claim is consulted independently of the record: a lock file
    /// whose pid is gone would be taken over by the #199 rule, and must NOT be
    /// while a live process holds the directory.
    #[test]
    fn the_kernel_claim_outranks_a_stale_record() {
        let dir = scratch("kern");
        let mut dead = record();
        dead.pid = DEAD_PID;
        write_record(&dir, &dead);

        // Hold the directory the way a concurrent core does — it is the same
        // call that core makes, on the same directory.
        let held = DirClaim::take(&dir)
            .expect("the claim is available")
            .expect("nothing holds this directory yet");

        let err = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect_err("a live kernel holder must refuse the claim");
        assert_eq!(err.busy.len(), 1);
        let msg = err.to_string();
        assert!(
            msg.contains("kernel lock"),
            "the refusal says WHY the directory is busy: {msg}"
        );
        assert!(
            msg.contains(&format!("pid {DEAD_PID}")),
            "the record is still quoted, so an operator sees what is on disk: {msg}"
        );

        // It was the kernel lock, not the record: release it and the very same
        // stale record is taken over, exactly as #199 specified.
        held.release();
        let lock = DataLock::acquire(&[anchor(&dir)], &record(), false).expect("takeover");
        assert_eq!(
            lock.held()[0].state,
            AnchorState::TookOverStale {
                previous: Some(dead)
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same process asking twice must be refused the second time, and the
    /// second refusal must come from the kernel, not from the record this process
    /// wrote a moment earlier (the record is removed here to prove which one it
    /// was), and the claim must be reclaimable once the first is dropped.
    #[test]
    fn a_second_claim_of_the_same_directory_is_refused_and_released_on_drop() {
        let dir = scratch("twice");
        let first = DataLock::acquire(&[anchor(&dir)], &record(), false).expect("the first claim");
        assert_eq!(first.held()[0].state, AnchorState::Claimed);

        let err = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect_err("the second claim of one directory must fail");
        assert_eq!(err.busy.len(), 1);
        assert!(
            err.to_string()
                .contains(&format!("pid {}", std::process::id())),
            "the occupant is named: {err}"
        );

        // With the record out of the way there is nothing left to read, so this
        // refusal can only be the kernel claim.
        std::fs::remove_file(dir.join(LOCK_FILE_NAME)).expect("the record is removable");
        let err = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect_err("the kernel claim alone must refuse the second claim");
        assert!(
            err.to_string().contains("kernel lock"),
            "the kernel claim is the gate, not the record: {err}"
        );

        // Dropping the holder releases it — which is what makes a clean exit
        // reclaimable without a cleanup pass.
        drop(first);
        let again =
            DataLock::acquire(&[anchor(&dir)], &record(), false).expect("free once the holder is");
        assert_eq!(again.held()[0].state, AnchorState::Claimed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The #233 defect as a regression test. Eight threads claim one fresh
    /// directory at the same instant, over ten fresh directories; the old
    /// read-all-then-write-all claim gave every one of them a file and a "claimed
    /// it" verdict. The count is deterministic either way — always 1 with the
    /// kernel claim — so this cannot flake, and the rounds are only there to give
    /// a broken claimer more windows to lose in.
    #[test]
    fn concurrent_claims_on_one_directory_have_exactly_one_winner() {
        const N: usize = 8;
        const ROUNDS: usize = 10;
        for round in 0..ROUNDS {
            let dir = scratch("race");
            let barrier = Arc::new(Barrier::new(N));
            let mut handles = Vec::with_capacity(N);
            for _ in 0..N {
                let dir = dir.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    // Line every claimer up so the window the old code lost in is
                    // the one this test hits.
                    barrier.wait();
                    DataLock::acquire(&[anchor(&dir)], &record(), false).is_ok()
                }));
            }
            let winners = handles
                .into_iter()
                .map(|h| h.join().expect("no claimer panicked"))
                .filter(|won| *won)
                .count();
            assert_eq!(
                winners, 1,
                "round {round}: two cores writing one data directory is precisely what this gate \
                 exists to prevent"
            );
            // The winner is this (still live) test process, so its record is
            // refused by the pid rule — correctly. Turn it into a dead pid: if a
            // kernel claim had survived the race, this would still be refused.
            let mut stale: LockRecord = serde_json::from_str(
                std::fs::read_to_string(dir.join(LOCK_FILE_NAME))
                    .unwrap()
                    .trim(),
            )
            .expect("the winner's record");
            stale.pid = DEAD_PID;
            write_record(&dir, &stale);
            let after = DataLock::acquire(&[anchor(&dir)], &record(), false)
                .expect("no kernel claim survives the race");
            assert_eq!(
                after.held()[0].state,
                AnchorState::TookOverStale {
                    previous: Some(stale)
                }
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The directory a refused boot created is removed again, so a refusal is
    /// still a no-op on the filesystem (the #199 promise this gate inherited).
    #[test]
    fn a_refusal_removes_the_directory_this_call_created() {
        let parent = scratch("rollback");
        let created = parent.join("data").join("trades");
        let busy = scratch("rollback-busy");
        let owner = record();
        write_record(&busy, &owner);
        let before = std::fs::read_to_string(busy.join(LOCK_FILE_NAME)).unwrap();

        let err = DataLock::acquire(&[anchor(&created), anchor(&busy)], &record(), false)
            .expect_err("the busy anchor refuses the whole boot");
        assert_eq!(err.busy.len(), 1);
        assert_eq!(err.busy[0].dir, busy);
        assert!(
            !created.exists(),
            "a refused boot created {created:?} and left it behind"
        );
        assert_eq!(
            std::fs::read_to_string(busy.join(LOCK_FILE_NAME)).unwrap(),
            before,
            "the live owner's record is byte-identical after a refusal"
        );
        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&busy);
    }

    /// A child process holding the claim, killed and reaped on drop so a failed
    /// assertion cannot leave a process behind (or a locked directory).
    struct ForeignHolder(Child);

    impl ForeignHolder {
        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn kill_and_reap(&mut self) {
            // SIGKILL: no shutdown hook, no cleanup — the way a crash looks. The
            // reaping matters: an unreaped child is still a process `kill(pid, 0)`
            // reports as alive, which is what the reclaim below must not depend on.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for ForeignHolder {
        fn drop(&mut self) {
            self.kill_and_reap();
        }
    }

    /// The env var that turns [`child_process_holds_the_data_lock_until_killed`]
    /// from an inert test into the child half of the test below.
    const CHILD_DIR_ENV: &str = "BK_DATALOCK_TEST_CHILD_DIR";

    /// The marker the child writes once it holds the claim.
    const CHILD_READY: &str = "child-ready";

    /// The child half of the cross-process test. Inert unless the env var is set
    /// (a normal suite run reaches the early return and moves on).
    ///
    /// It exists because the claim's whole job is to be visible ACROSS processes:
    /// a test that only races threads inside one process would also pass for a
    /// claim that is process-local (a mutex, a static flag), which is the class of
    /// implementation this test must not accept.
    #[test]
    fn child_process_holds_the_data_lock_until_killed() {
        let Ok(dir) = std::env::var(CHILD_DIR_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let lock =
            DataLock::acquire(&[anchor(&dir)], &record(), false).expect("the child claims it");
        assert_eq!(lock.held()[0].state, AnchorState::Claimed);
        // Written AFTER the claim, so the parent's wait for it means "the kernel
        // lock is in force", not "the process started".
        std::fs::write(dir.join(CHILD_READY), b"held").expect("the ready marker");
        // Hold it until the parent kills this process: what releases the claim in
        // the second half is the KERNEL, not any code here. The bound is only so a
        // stray child cannot outlive the suite by much.
        std::thread::sleep(Duration::from_secs(60));
        drop(lock);
    }

    /// The cross-process half of #233: a real second process holds the directory,
    /// the claim is refused, the escape hatch shares instead of stealing, and the
    /// holder's death releases everything it held.
    #[test]
    fn a_foreign_process_holding_the_claim_is_refused_and_its_death_releases_it() {
        let dir = scratch("crossproc");
        let mut holder = ForeignHolder(
            Command::new(std::env::current_exe().expect("the test binary's path"))
                .args([
                    "--exact",
                    "data_lock::tests::child_process_holds_the_data_lock_until_killed",
                    "--nocapture",
                ])
                .env(CHILD_DIR_ENV, &dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("the test binary must be spawnable"),
        );
        let ready = dir.join(CHILD_READY);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "pid {} never claimed {:?}",
                holder.pid(),
                dir
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let holder_pid = holder.pid();

        // 1. Refused, and by the kernel: this process shares the machine with the
        //    holder but not its file descriptors.
        let err = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect_err("a live foreign process holds this directory");
        assert!(
            err.to_string().contains("kernel lock"),
            "the refusal names the kernel claim: {err}"
        );

        // 2. The escape hatch still starts — that is what it is for — without
        //    taking the claim, so the holder's record stays readable for a third
        //    core.
        let shared = DataLock::acquire(&[anchor(&dir)], &record(), true)
            .expect("--allow-shared-data must still start");
        match &shared.held()[0].state {
            AnchorState::Shared { owner } => assert_eq!(
                owner.pid, holder_pid,
                "the foreign holder is named as the owner, not this process"
            ),
            other => panic!("expected a shared directory, got {other:?}"),
        }
        drop(shared);

        // 3. Kill it the way a crash does, and the directory comes back: the
        //    kernel released the claim with the process, and the record left on
        //    disk (naming a pid that no longer exists) is taken over and
        //    announced. This is the path that keeps a crash from locking a data
        //    directory forever.
        holder.kill_and_reap();
        let reclaim = DataLock::acquire(&[anchor(&dir)], &record(), false)
            .expect("a dead holder's directory must be reclaimable");
        match &reclaim.held()[0].state {
            AnchorState::TookOverStale {
                previous: Some(previous),
            } => assert_eq!(
                previous.pid, holder_pid,
                "the takeover names the process that died"
            ),
            other => panic!("expected a stale takeover, got {other:?}"),
        }
        assert!(
            reclaim.banner(0)[0].contains("took over stale lock"),
            "{}",
            reclaim.banner(0)[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
