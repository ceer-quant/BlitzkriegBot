//! The core's command line, end to end (#228).
//!
//! What these tests add over `src/cli.rs`'s unit tests: they run the REAL
//! binary, so the claim under test is not "the rule says reject" but "this
//! process, started with a misspelled flag, exits 2 without booting and without
//! writing anything". The distinction is the whole defect — a rule that exists
//! and is never reached is what #228 found (the refusal was one `match` arm away
//! and nobody wrote it).
//!
//! Isolation is not optional here, and it is also the assertion. The core's
//! relative defaults (`data/trades/trades.jsonl`, `data/orders/orders.jsonl`,
//! `data/positions/positions.jsonl`, the `.core-lock` the #199 latch writes) all
//! resolve against the working directory, so every child is spawned with its cwd
//! set to a fresh temp directory: a run that booted leaves files THERE, not in
//! the repository. The empty-sandbox assertions are therefore real claims about
//! the boot path.
//!
//! If a future change makes an unknown argument boot again, these tests fail on
//! the exit code first — the child is killed after a grace period rather than
//! waited on forever, so a regression reports "still running" instead of hanging
//! the suite.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A line the boot path prints unconditionally (the #202 sizing echo), used as
/// "this process reached the serving path" evidence. It is not printed by any of
/// the early exits.
const BOOT_MARKER: &str = "per-order bounds in force";

/// A fresh scratch directory, unique per test.
fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bk-args-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// True when the grace period expired and the child had to be killed: it was
    /// still alive, i.e. it had booted and was serving.
    killed: bool,
}

/// Spawn the core with `args` in its own sandbox and wait at most `grace` for it
/// to exit.
///
/// Every path the process could write is redirected into the sandbox and the
/// mode is dry, so even a deliberately booting child is contained: no venue, no
/// repository writes, and it is killed rather than left behind.
fn run_core(sandbox: &Path, args: &[&str], grace: Duration) -> Run {
    let mut argv: Vec<String> = vec![
        "--socket".into(),
        sandbox.join("core.sock").to_string_lossy().into_owned(),
        "--mode".into(),
        "dry".into(),
        // No market discovery: a hermetic child reaches the serving path without
        // the network being involved at all.
        "--no-discovery".into(),
        "--trade-log".into(),
        sandbox.join("trades.jsonl").to_string_lossy().into_owned(),
        "--order-log".into(),
        sandbox.join("orders.jsonl").to_string_lossy().into_owned(),
        "--position-log".into(),
        sandbox
            .join("positions.jsonl")
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(args.iter().map(|a| (*a).to_string()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_blitzkrieg-core"))
        .args(&argv)
        .current_dir(sandbox)
        // `BK_CONFIG=none` is the documented way to run without a config file: an
        // operator's local TOML edit must not decide what this test observes.
        .env("BK_CONFIG", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the core binary built for this test must be spawnable");
    wait_gracefully(&mut child, grace)
}

/// Poll for the exit; on timeout, kill and reap, so a child that booted cannot
/// outlive the test. The pipes are drained only after the exit — nothing this
/// test asserts depends on a banner arriving before the process ends.
fn wait_gracefully(child: &mut Child, grace: Duration) -> Run {
    let deadline = Instant::now() + grace;
    let mut killed = false;
    let status = loop {
        match child.try_wait().expect("try_wait must not fail") {
            Some(s) => break Some(s),
            None if Instant::now() >= deadline => {
                killed = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut o, &mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut e, &mut stderr);
    }
    Run {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        killed,
    }
}

/// Anything the child wrote into its sandbox.
fn sandbox_entries(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .expect("the sandbox exists")
        .map(|e| {
            e.expect("readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    out.sort();
    out
}

fn assert_refused(run: &Run, what: &str) {
    assert!(
        !run.killed,
        "{what}: the core was still running after the grace period — it BOOTED. stderr:\n{}",
        run.stderr
    );
    assert_eq!(
        run.code,
        Some(2),
        "{what}: must exit 2. stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains(BOOT_MARKER),
        "{what}: the refusal must land before the boot banner. stderr:\n{}",
        run.stderr
    );
}

/// Acceptance criterion #4 of #228, and the reason the issue is urgent: an
/// unknown argument must not produce a boot. The old behaviour printed
/// `ignoring unknown argument: --definitely-not-a-flag` and then served a core.
#[test]
fn an_unknown_argument_exits_2_without_booting() {
    let dir = sandbox("unknown");
    let run = run_core(&dir, &["--definitely-not-a-flag"], Duration::from_secs(10));
    assert_refused(&run, "--definitely-not-a-flag");
    assert!(
        run.stderr.contains("--definitely-not-a-flag"),
        "the argument is named. stderr:\n{}",
        run.stderr
    );
    let entries = sandbox_entries(&dir);
    assert!(
        entries.is_empty(),
        "a refused start writes NOTHING at all (no ledger, no lock, no socket): {entries:?}"
    );
    assert!(run.stdout.is_empty(), "stdout: {}", run.stdout);
}

/// Acceptance criterion #2 of #228: the spelling an operator is most likely to
/// reach for, and the one that turns a read-only intention into a write-mode
/// core. The suggestion and the consequence are both part of the contract —
/// "unknown argument" alone does not tell anyone what they almost ran.
#[test]
fn a_misspelled_readonly_is_refused_and_says_what_it_would_have_meant() {
    for typo in ["--redonly", "--read-only"] {
        let dir = sandbox("readonly");
        let run = run_core(&dir, &[typo], Duration::from_secs(10));
        assert_refused(&run, typo);
        assert!(
            run.stderr.contains("did you mean --readonly?"),
            "{typo}: the flag that was meant is named. stderr:\n{}",
            run.stderr
        );
        assert!(
            run.stderr.contains("WRITE mode"),
            "{typo}: what leaving it out means is stated. stderr:\n{}",
            run.stderr
        );
        let entries = sandbox_entries(&dir);
        assert!(entries.is_empty(), "{typo}: wrote {entries:?}");
    }
}

/// A misspelled limit is the other half of the loss path: the shipped caps stay
/// in force and the operator's number is simply not there. The refusal must name
/// the flag and the value the silent version would have used.
#[test]
fn a_misspelled_limit_is_refused_with_its_consequence() {
    let dir = sandbox("limit");
    let run = run_core(&dir, &["--max-notinal", "50"], Duration::from_secs(10));
    assert_refused(&run, "--max-notinal");
    assert!(
        run.stderr.contains("did you mean --max-order-notional?"),
        "stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("100 USD"),
        "the cap that WOULD have been in force is named. stderr:\n{}",
        run.stderr
    );
}

/// Acceptance criterion #3 of #228: `--help` prints every flag and exits 0
/// without touching anything. The expected list is the crate's own table, so
/// this compares the binary's output against the source of truth rather than
/// against a copy that can rot.
#[test]
fn help_prints_every_flag_and_exits_0() {
    let dir = sandbox("help");
    let run = run_core(&dir, &["--help"], Duration::from_secs(10));
    assert!(!run.killed, "--help must return immediately");
    assert_eq!(run.code, Some(0), "--help exits 0. stderr:\n{}", run.stderr);
    for spec in blitzkrieg_core::cli::FLAGS {
        assert!(run.stdout.contains(spec.flag), "--help omits {}", spec.flag);
    }
    assert!(
        run.stdout.contains("usage: blitzkrieg-core"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS),
        "--help must state the opt-out it offers"
    );
    let entries = sandbox_entries(&dir);
    assert!(entries.is_empty(), "--help writes {entries:?}");
}

/// `-h` is the same answer as `--help`, like `-V` is for `--version`.
#[test]
fn the_short_help_spelling_works() {
    let dir = sandbox("shorth");
    let run = run_core(&dir, &["-h"], Duration::from_secs(10));
    assert_eq!(run.code, Some(0), "stderr:\n{}", run.stderr);
    assert!(
        run.stdout.contains("usage: blitzkrieg-core"),
        "{}",
        run.stdout
    );
}

/// #197 must keep working exactly as it did: `--version` answers first, with the
/// built revision, and starts nothing. This is the guard against fixing #228 by
/// reordering the early exits.
#[test]
fn version_still_short_circuits_before_anything_else() {
    let dir = sandbox("version");
    for flag in ["--version", "-V"] {
        let run = run_core(&dir, &[flag], Duration::from_secs(10));
        assert!(!run.killed, "{flag} must return immediately");
        assert_eq!(run.code, Some(0), "{flag}: stderr:\n{}", run.stderr);
        assert!(
            run.stdout.contains("+g"),
            "{flag} prints `<semver>+g<sha>`: {:?}",
            run.stdout
        );
        assert!(
            !run.stderr.contains(BOOT_MARKER),
            "{flag} must not reach the boot path. stderr:\n{}",
            run.stderr
        );
    }
    let entries = sandbox_entries(&dir);
    assert!(
        entries.is_empty(),
        "neither spelling writes anything: {entries:?}"
    );
}

/// The escape hatch, verified without letting anything boot: the unknown
/// argument is followed by a flag whose VALUE is invalid, so the parse loop
/// reaches the ignored argument first and the invalid value stops the process
/// before the serving path. The exit code alone would not distinguish this from
/// a refusal (both are 2), which is why the assertions are on the messages.
#[test]
fn the_escape_hatch_ignores_a_benign_unknown_argument_in_any_position() {
    for args in [
        vec![
            blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS,
            "--definitely-not-a-flag",
            "--max-orderbook-stale-ms",
            "-1",
        ],
        vec![
            "--definitely-not-a-flag",
            "--max-orderbook-stale-ms",
            "-1",
            blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS,
        ],
    ] {
        let dir = sandbox("hatch");
        let run = run_core(&dir, &args, Duration::from_secs(10));
        assert!(!run.killed, "{args:?} booted. stderr:\n{}", run.stderr);
        assert!(
            run.stderr.contains("ignoring it")
                && run.stderr.contains("--definitely-not-a-flag")
                && run
                    .stderr
                    .contains(blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS),
            "the hatch warns instead of refusing, wherever it is written. stderr:\n{}",
            run.stderr
        );
        assert!(
            !run.stderr.contains("refusing to start"),
            "the hatch was given, so this is not a refusal. stderr:\n{}",
            run.stderr
        );
        assert!(
            run.stderr.contains("orderbook freshness budget"),
            "the boot was stopped by the invalid VALUE, i.e. AFTER the ignored argument was \
             parsed. stderr:\n{}",
            run.stderr
        );
    }
}

/// ...and the hatch does NOT cover a misspelled safety flag. This is the property
/// that keeps the escape hatch from re-opening #228: an operator who adds it to a
/// script cannot thereby turn `--redonly` back into a write-mode boot.
#[test]
fn the_escape_hatch_does_not_cover_a_safety_flag_near_miss() {
    let dir = sandbox("hatchsafe");
    let run = run_core(
        &dir,
        &[
            blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS,
            "--redonly",
            "--max-orderbook-stale-ms",
            "-1",
        ],
        Duration::from_secs(10),
    );
    assert_refused(&run, "--allow-unknown-args --redonly");
    assert!(
        run.stderr.contains("refusing to start") && run.stderr.contains("--readonly"),
        "a safety near-miss is refused even under the hatch. stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("--max-orderbook-stale-ms"),
        "the refusal came BEFORE the later argument was parsed. stderr:\n{}",
        run.stderr
    );
    let entries = sandbox_entries(&dir);
    assert!(entries.is_empty(), "wrote {entries:?}");
}

/// A positional argument is refused too: the core takes none, and the old
/// fallback ignored it in the same silent way.
#[test]
fn a_stray_positional_argument_is_refused() {
    let dir = sandbox("positional");
    let run = run_core(&dir, &["start"], Duration::from_secs(10));
    assert_refused(&run, "start");
    assert!(run.stderr.contains("start"), "stderr:\n{}", run.stderr);
}

/// A sanity check on the harness itself: with valid arguments the child really
/// does boot, serve and write into the sandbox — so "the sandbox is empty" above
/// is a statement about the refusal, not about a child that never starts. Without
/// this, every other assertion here could pass against a binary that exits
/// immediately for an unrelated reason.
#[test]
fn the_harness_can_observe_a_real_boot() {
    let dir = sandbox("boot");
    let run = run_core(&dir, &[], Duration::from_secs(10));
    assert!(
        run.killed,
        "a core started with valid arguments should still be running, so the grader can tell \
         'refused' from 'booted'. stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(BOOT_MARKER),
        "the boot banner is observable. stderr:\n{}",
        run.stderr
    );
    let entries = sandbox_entries(&dir);
    assert!(
        !entries.is_empty(),
        "a booting core writes into its cwd sandbox, which is what makes the empty-sandbox \
         assertions meaningful"
    );
}
