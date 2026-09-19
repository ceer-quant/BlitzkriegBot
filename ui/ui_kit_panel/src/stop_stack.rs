//! `blitzkrieg stop` — the operator's single switch for the stack on ONE socket.
//!
//! WHY THIS EXISTS
//!   `blitzkrieg run` owns the core it spawns and stops it on SIGINT/SIGTERM —
//!   but a stack can outlive that ownership. Two real shapes on this machine:
//!   a core whose gateway died without a signal handler (the core is orphaned,
//!   PPID 1, still serving the socket), and a launcher that ADOPTED such an
//!   orphan — which, by the adopt-only restraint (E12: a gateway never kills a
//!   core it did not spawn), then reads but cannot stop it. The panel says so,
//!   and the operator was left with two hand-collected pids.
//!
//!   `stop` closes that loop as an EXPLICIT operator action. The restraint is
//!   not violated by it: nothing here fires automatically, and the command only
//!   ever touches blitzkrieg-family processes attached to the ONE socket named
//!   on its command line. A core's parent that is NOT blitzkrieg-family (a
//!   shell, a test driver) is never signalled — the core is signalled directly
//!   instead and the stranger is reported as left alone.
//!
//! MECHANISM
//!   One `ps -ww -axo pid=,ppid=,command=` scan (every argument a compile-time
//!   literal), a decision made purely on that table (unit-tested), and signals
//!   delivered by `libc::kill` — a syscall, not a subprocess, so no shell and
//!   no argument ever crosses a process boundary as text. Ownership is matched
//!   by the SOCKET PATH in the command line, never by a bare process name: on
//!   a machine with several stacks, a name match would kill someone else's
//!   core (the exact mistake `unified-launcher-check.mjs` was taught to avoid).
//!
//! ORDER
//!   Owners first (they cascade to their own children — that is the graceful
//!   path), then any core still alive (orphan from the start, or an owner that
//!   died without taking its child). SIGTERM with a grace period, SIGKILL only
//!   as the last resort, and a stale socket file is removed only once nothing
//!   serves it — a leftover file would block the next boot.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One row of the process table, as `ps` reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    pub command: String,
}

/// The two signals `stop` sends. SIGTERM and SIGKILL have the same numbers on
/// every Unix this workspace builds for, but the constants come from `libc`
/// so the syscall call site never hardcodes a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
}

/// Parse `ps -ww -axo pid=,ppid=,command=` output. Headerless; malformed lines
/// are skipped rather than guessed at — a stop decision must rest on rows it
/// fully understands. `ps` pads columns with runs of spaces and the COMMAND
/// itself contains spaces (this repo's path does), so the first two fields are
/// skipped token-wise and everything after them — verbatim — is the command.
pub fn parse_ps_output(out: &str) -> Vec<ProcRow> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let s = line.trim_start();
        let Some(pid_end) = s.find(char::is_whitespace) else {
            continue;
        };
        let (pid, s2) = (&s[..pid_end], s[pid_end..].trim_start());
        let Some(ppid_end) = s2.find(char::is_whitespace) else {
            continue;
        };
        let (ppid, command) = (&s2[..ppid_end], s2[ppid_end..].trim_start());
        let (Ok(pid), Ok(ppid)) = (pid.parse::<i32>(), ppid.parse::<i32>()) else {
            continue;
        };
        rows.push(ProcRow {
            pid,
            ppid,
            command: command.trim_end().to_string(),
        });
    }
    rows
}

/// Whether `cmd` names `name` as a standalone executable: the whole command,
/// or a token that ENDS a path segment (`/name`). A path with spaces (this
/// repo lives on "/Volumes/Hard Disk") splits tokens mid-path, but never
/// mid-binary-name, so ends-with on the binary name is exact.
fn has_word(cmd: &str, name: &str) -> bool {
    cmd.split_whitespace()
        .any(|t| t == name || t.ends_with(&format!("/{name}")))
}

/// A trading-core process.
pub fn is_core(cmd: &str) -> bool {
    has_word(cmd, "blitzkrieg-core")
}

/// A process that owns/manages cores: the unified launcher, the standalone
/// core-keeper, the web gateway or the TUI. `blitzkrieg-core` is deliberately
/// NOT an owner (a core does not manage cores); `has_word("blitzkrieg")` does
/// not match `/blitzkrieg-core` because the name must end the token.
pub fn is_owner(cmd: &str) -> bool {
    !is_core(cmd)
        && (has_word(cmd, "blitzkrieg")
            || has_word(cmd, "ui_kit_web")
            || has_word(cmd, "ui_kit_panel"))
}

/// Whether `cmd` is attached to the socket `path` we are stopping.
///
/// Processes started WITHOUT an explicit `--socket` resolve the default socket,
/// so they count only when we are stopping that default. A process naming a
/// DIFFERENT socket never matches — that is the whole point of socket-scoping.
fn attached_to(cmd: &str, path: &str, is_default: bool) -> bool {
    if cmd.contains(path) {
        return true;
    }
    is_default && !cmd.contains("--socket")
}

/// The family processes attached to one socket: every owner (launcher /
/// gateway / TUI) and at most one core. `exclude` is removed from the result —
/// at minimum `stop`'s own pid plus its ANCESTOR chain: the shell (or `sh -c`
/// wrapper) that invoked this command has `blitzkrieg` in its command line and
/// often the socket path too, but the process chain that launched us is by
/// definition not the stack we are stopping.
pub fn attached<'a>(
    rows: &'a [ProcRow],
    path: &str,
    is_default: bool,
    exclude: &[i32],
) -> (Vec<&'a ProcRow>, Option<&'a ProcRow>) {
    let mut owners = Vec::new();
    let mut core = None;
    for r in rows {
        if r.pid < 0 || exclude.contains(&r.pid) {
            continue;
        }
        if !attached_to(&r.command, path, is_default) {
            continue;
        }
        if is_core(&r.command) {
            core = Some(r);
        } else if is_owner(&r.command) {
            owners.push(r);
        }
    }
    (owners, core)
}

/// `self_pid` plus every ancestor reachable through the process table — the
/// invoker chain that must never be signalled (see [`attached`]).
pub fn exclude_self_and_ancestors(self_pid: i32, rows: &[ProcRow]) -> Vec<i32> {
    let mut excluded = vec![self_pid];
    let mut cursor = by_pid(rows, self_pid).map(|r| r.ppid);
    while let Some(pid) = cursor {
        if pid <= 1 || excluded.contains(&pid) {
            break;
        }
        excluded.push(pid);
        cursor = by_pid(rows, pid).map(|r| r.ppid);
    }
    excluded
}

/// The row for `pid`, to answer "who is the core's parent and is it ours?".
pub fn by_pid(rows: &[ProcRow], pid: i32) -> Option<&ProcRow> {
    rows.iter().find(|r| r.pid == pid)
}

/// Send `sig` to `pid` via the `kill(2)` syscall. No subprocess, no shell, no
/// string argument anywhere — the pid is an integer from our own scan and the
/// signal a compile-time constant.
pub fn send_signal(pid: u32, sig: Sig) -> bool {
    let num = match sig {
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    };
    // SAFETY: `kill` is async-signal-safe and takes plain integers; there is
    // no memory or string crossing the boundary.
    unsafe { libc::kill(pid as libc::pid_t, num) == 0 }
}

/// Is the process alive?
///
/// `kill(pid, 0)` is NOT enough: a dead-but-unreaped child (zombie) still
/// answers `kill -0`, and a stop running under a parent that has not reaped
/// its children yet — a gate driver blocked in a synchronous exec, for
/// instance — would then wait the full grace, escalate to a pointless SIGKILL,
/// and report failure against a process that can never run code again. So the
/// state letter comes from `ps`: `Z` counts as dead, a missing row counts as
/// dead, everything else is alive.
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .arg("-o")
        .arg("state=")
        .arg("-p")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => !is_dead_state(&String::from_utf8_lossy(&o.stdout)),
        _ => false,
    }
}

/// The `ps` state letter: `Z` (zombie) — and nothing else — means the process
/// can never run again.
fn is_dead_state(state: &str) -> bool {
    state.trim_start().starts_with('Z')
}

/// One `ps` scan. Fails only when the process table itself is unreadable —
/// the caller then reports and refuses to guess.
pub fn scan() -> Result<Vec<ProcRow>, String> {
    let out = Command::new("ps")
        .arg("-ww")
        .arg("-axo")
        .arg("pid=,ppid=,command=")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ps failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("ps exited {:?}", out.status.code()));
    }
    Ok(parse_ps_output(&String::from_utf8_lossy(&out.stdout)))
}

/// Wait until `alive(pid)` turns false. Returns whether it ever did.
fn wait_gone(pid: u32, grace: Duration, alive: impl Fn(u32) -> bool) -> bool {
    let deadline = Instant::now() + grace;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Stop the stack on `socket`. Returns the process exit code: 0 when nothing
/// is left attached, 1 when something survived or the world was not
/// understandable enough to act on.
pub fn run(socket: &str, grace: Duration) -> i32 {
    let self_pid = std::process::id() as i32;
    let is_default = socket == blitzkrieg_ui_kit::default_socket_path();
    let served = || blitzkrieg_ui_kit::socket_served(socket);

    let mut out = std::io::stdout();
    let _ = writeln!(out, "blitzkrieg stop: socket {socket}");

    let rows = match scan() {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(
                out,
                "blitzkrieg stop: cannot read the process table ({e}); stopping nothing"
            );
            return 1;
        }
    };
    let exclude = exclude_self_and_ancestors(self_pid, &rows);
    let (owners, core) = attached(&rows, socket, is_default, &exclude);

    if owners.is_empty() && core.is_none() && !served() {
        let _ = writeln!(
            out,
            "blitzkrieg stop: nothing attached to this socket; nothing to stop"
        );
        return 0;
    }
    if core.is_none() && served() {
        // The socket answers but no blitzkrieg-core process names it. Killing
        // by guesswork is exactly what socket-scoping exists to prevent.
        let _ = writeln!(
            out,
            "blitzkrieg stop: something is serving {socket} but no blitzkrieg-core process names it — refusing to guess"
        );
        return 1;
    }

    // 1. Owners first: they cascade to the children they spawned, which is the
    //    graceful path. An adopted-core launcher exits without touching the
    //    orphan (by design); step 2 handles the orphan.
    for o in &owners {
        let _ = writeln!(
            out,
            "  owner pid {} → SIGTERM ({})",
            o.pid,
            brief(&o.command)
        );
        send_signal(o.pid as u32, Sig::Term);
    }
    for o in &owners {
        if !wait_gone(o.pid as u32, grace, alive) {
            let _ = writeln!(
                out,
                "  owner pid {} still alive after grace → SIGKILL",
                o.pid
            );
            send_signal(o.pid as u32, Sig::Kill);
            let _ = wait_gone(o.pid as u32, grace, alive);
        }
    }

    // 2. The core, if it is still here: either it never had an owner (orphan /
    //    standalone) or its owner died without taking it (a gateway without a
    //    signal handler). Its parent is checked and REPORTED, never signalled
    //    unless it is family — a stranger's pid is not ours to kill.
    let rows = scan().unwrap_or_default();
    let (_, core) = attached(&rows, socket, is_default, &exclude);
    if let Some(c) = core {
        if let Some(parent) = by_pid(&rows, c.ppid) {
            if !is_owner(&parent.command) && !is_core(&parent.command) && c.ppid > 1 {
                let _ = writeln!(
                    out,
                    "  note: core's parent pid {} is not a blitzkrieg process; left untouched",
                    c.ppid
                );
            }
        }
        let _ = writeln!(out, "  core pid {} → SIGTERM", c.pid);
        send_signal(c.pid as u32, Sig::Term);
        if !wait_gone(c.pid as u32, grace, alive) {
            let _ = writeln!(
                out,
                "  core pid {} still alive after grace → SIGKILL",
                c.pid
            );
            send_signal(c.pid as u32, Sig::Kill);
            if !wait_gone(c.pid as u32, grace, alive) {
                let _ = writeln!(out, "blitzkrieg stop: FAILED — core pid {} survived", c.pid);
                return 1;
            }
            let _ = writeln!(
                out,
                "  core killed by SIGKILL (state persists as of the last write)"
            );
        }
    }

    // 3. A socket file left behind (crash or SIGKILL) blocks the next boot;
    //    remove it only once nothing serves it.
    let path = std::path::Path::new(socket);
    if path.exists() && !served() {
        match std::fs::remove_file(path) {
            Ok(()) => {
                let _ = writeln!(out, "  removed stale socket file {socket}");
            }
            Err(e) => {
                let _ = writeln!(out, "  could not remove stale socket file {socket}: {e}");
            }
        }
    }

    let _ = writeln!(out, "blitzkrieg stop: done");
    0
}

/// The process role as shown in the stop log — the basename that made it count
/// as family, not the full command line (paths here contain spaces).
fn brief(cmd: &str) -> &str {
    for name in [
        "blitzkrieg-core",
        "blitzkrieg",
        "ui_kit_web",
        "ui_kit_panel",
    ] {
        if has_word(cmd, name) {
            return name;
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows shaped like the real machine's — including a repo path with a
    /// space, which is what makes token-boundary matching necessary.
    fn rows() -> Vec<ProcRow> {
        parse_ps_output(
            "\
  46230     1 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core --socket /var/folders/T/blitzkrieg-core-fancer.sock --mode dry
  93616     1 target/release/blitzkrieg run
  97590     1 ./target/release/ui_kit_web --socket /var/folders/T/blitzkrieg-core-fancer.sock --addr 127.0.0.1:51888 --manage
  4100  4100 -zsh
  99999  46230 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core --socket /other/stack.sock
",
        )
    }

    #[test]
    fn ps_rows_parse_with_spacey_paths() {
        let r = rows();
        assert_eq!(r.len(), 5);
        assert_eq!(r[0].pid, 46230);
        assert_eq!(r[0].ppid, 1);
        assert!(r[0].command.contains("Hard Disk"));
    }

    #[test]
    fn family_detection_survives_spaces_and_the_core_suffix() {
        assert!(is_core(rows()[0].command.as_str()));
        assert!(
            !is_owner(rows()[0].command.as_str()),
            "a core is not an owner"
        );
        assert!(is_owner(rows()[1].command.as_str()));
        assert!(is_owner(rows()[2].command.as_str()));
        assert!(!is_core(rows()[1].command.as_str()));
        // A shell is nobody's owner.
        assert!(!is_owner(rows()[3].command.as_str()));
        assert!(!is_core(rows()[3].command.as_str()));
        // The bare name (invoked via PATH) still counts.
        assert!(is_owner("blitzkrieg stop"));
        assert!(is_owner("ui_kit_panel"));
    }

    #[test]
    fn attachment_is_scoped_to_the_named_socket() {
        let r = rows();
        let sock = "/var/folders/T/blitzkrieg-core-fancer.sock";
        let (owners, core) = attached(&r, sock, false, &[1]);
        assert_eq!(owners.len(), 1, "{owners:?}");
        assert_eq!(owners[0].pid, 97590);
        assert_eq!(core.map(|c| c.pid), Some(46230));
        // The other stack's core is invisible to this socket.
        assert_ne!(core.map(|c| c.pid), Some(99999));
    }

    #[test]
    fn processes_without_socket_arg_count_only_for_the_default() {
        let r = rows();
        // `blitzkrieg run` names no --socket: attached only when stopping the default.
        let (owners, _) = attached(&r, "/var/folders/T/blitzkrieg-core-fancer.sock", true, &[1]);
        assert!(owners.iter().any(|o| o.pid == 93616), "{owners:?}");
        let (owners, _) = attached(
            &r,
            "/var/folders/T/blitzkrieg-core-fancer.sock",
            false,
            &[1],
        );
        assert!(!owners.iter().any(|o| o.pid == 93616), "{owners:?}");
        // …unless it explicitly points elsewhere.
        let r2 = parse_ps_output("7000 1 blitzkrieg run --socket /elsewhere.sock\n");
        let (owners, _) = attached(
            &r2,
            "/var/folders/T/blitzkrieg-core-fancer.sock",
            true,
            &[1],
        );
        assert!(owners.is_empty(), "{owners:?}");
    }

    #[test]
    fn stop_never_includes_itself() {
        let r = parse_ps_output("8000 1 target/release/blitzkrieg stop --socket /s.sock\n");
        let (owners, core) = attached(&r, "/s.sock", true, &[8000]);
        assert!(owners.is_empty() && core.is_none());
    }

    #[test]
    fn stop_never_signals_its_invoker_chain() {
        // A gate driver runs `sh -c "…/blitzkrieg stop --socket /s.sock"`: the
        // shell wrapper's command line contains the binary name AND the socket
        // path, which would otherwise make it look like an owner. The ancestor
        // walk (stop → sh → node) must put the whole chain out of reach.
        let r = parse_ps_output(
            "\
  8000  7900 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg stop --socket /s.sock
  7900  7800 sh -c /Volumes/Hard\\ Disk/BlitzkriegBot/target/release/blitzkrieg\\ stop\\ --socket\\ /s.sock
  7800  7700 node scripts/unified-launcher-check.mjs
  5000     1 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core --socket /s.sock
",
        );
        let exclude = exclude_self_and_ancestors(8000, &r);
        // 7700 (node's parent) has no row of its own; the walk still excludes
        // the pid — conservative by construction: what we cannot identify is
        // never a target.
        assert_eq!(exclude, vec![8000, 7900, 7800, 7700], "{exclude:?}");
        let (owners, core) = attached(&r, "/s.sock", false, &exclude);
        assert!(
            owners.is_empty(),
            "no wrapper may look like an owner: {owners:?}"
        );
        assert_eq!(
            core.map(|c| c.pid),
            Some(5000),
            "the real core is still found"
        );
    }

    #[test]
    fn zombie_state_counts_as_dead() {
        assert!(is_dead_state("Z"));
        assert!(is_dead_state(" Z+"));
        assert!(!is_dead_state("S"));
        assert!(!is_dead_state("S+"));
        assert!(!is_dead_state("R"));
    }

    #[test]
    fn adopted_stack_finds_orphan_core_and_its_stranger_parent() {
        // The incident shape: a standalone core (parent = something that is not
        // ours) plus a launcher that adopted it. Stop must plan BOTH, and the
        // stranger parent must be identifiable so it is never signalled.
        let r = parse_ps_output(
            "\
  46230     1 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core --socket /s.sock
  93616     1 target/release/blitzkrieg run --socket /s.sock
",
        );
        let (owners, core) = attached(&r, "/s.sock", false, &[1]);
        assert_eq!(owners.len(), 1);
        assert_eq!(core.map(|c| c.pid), Some(46230));
        let parent = core.and_then(|c| by_pid(&r, c.ppid));
        assert!(parent.is_none(), "ppid 1 has no row; a stranger would");
        let r2 = parse_ps_output(
            "\
  46230  4000 /Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core --socket /s.sock
  4000  3900 -zsh
",
        );
        let (_, core) = attached(&r2, "/s.sock", false, &[1]);
        let parent = core.and_then(|c| by_pid(&r2, c.ppid)).unwrap();
        assert!(
            !is_owner(&parent.command),
            "zsh must not be treated as the owner"
        );
    }
}
