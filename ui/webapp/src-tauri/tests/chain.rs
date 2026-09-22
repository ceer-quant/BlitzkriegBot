//! E8-d acceptance: the desktop command seam reaches a real dry core.
//!
//! `desktop_snapshot` / `desktop_command` are Tauri `#[command]`s, but they
//! stay callable as plain functions — so the full chain (socket → IpcClient →
//! AppViewModel, and socket → Dispatcher → CommandOutcome) is verifiable
//! headlessly, without opening a window.
//!
//! Self-contained: the test spawns its own `blitzkrieg-core` (dry mode) on a
//! temp socket and reaps it in a Drop guard. Skips cleanly when
//! `BLITZKRIEG_CORE_BIN` is unset — a plain `cargo test` on a machine without
//! a built core still passes. The ui:webapp gate exports the var to make this
//! assertion real.

use serde_json::Value;
use std::time::{Duration, Instant};

/// Kill the core no matter how the test ends — asserts unwind, panics unwind,
/// and Drop runs on unwind. Losing it would leak a live (dry) engine loop. The
/// core's private working directory goes with it.
struct KillOnDrop {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Per-call suffix: `cargo test` runs the tests in parallel threads of one
/// process, so the pid alone would collide.
static SOCK_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Spawn a dry core on a fresh temp socket; returns (guard, socket path).
///
/// Each core gets its OWN working directory, for two reasons. The tests run in
/// parallel threads, and a core claims the `data/` directories under its cwd for
/// the life of the process — so two cores sharing an inherited cwd made the
/// second one refuse to start ("another blitzkrieg-core is already writing this
/// data directory") and this test fail on a socket that never appeared: the
/// first core held the directory. The inherited cwd also littered the
/// developer's checkout with `ui/webapp/src-tauri/data/`, which is the same
/// defect seen from the other side.
fn spawn_core() -> Option<(KillOnDrop, String)> {
    let bin = std::env::var("BLITZKRIEG_CORE_BIN").ok()?;
    if bin.trim().is_empty() {
        return None;
    }
    let seq = SOCK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bk-chain-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp working directory for the core");
    let sock = dir.join("core.sock").to_string_lossy().into_owned();
    let child = std::process::Command::new(bin.trim())
        .current_dir(&dir)
        .args([
            "--socket",
            &sock,
            "--mode",
            "dry",
            "--tick-ms",
            "100",
            "--seed-balance",
            "1000",
            "--engine",
            "--no-trade-log",
            "--no-event-archive",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("BLITZKRIEG_CORE_BIN points at a runnable blitzkrieg-core");
    let guard = KillOnDrop { child, dir };

    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(&sock).exists() {
        if Instant::now() > deadline {
            panic!("core did not create socket {sock} within 5s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    Some((guard, sock))
}

#[test]
fn desktop_snapshot_reaches_live_core() {
    // `_core_guard` and not `_`: the guard owns the core AND its temp directory,
    // so binding it is what keeps both alive until this scope ends.
    let Some((_core_guard, sock)) = spawn_core() else {
        eprintln!("BLITZKRIEG_CORE_BIN unset — skipping live-chain assertion");
        return;
    };
    let out = blitzkrieg_webapp::snapshot_json(Some(sock.clone()))
        .expect("desktop_snapshot returns the AppView JSON");
    let v: Value = serde_json::from_str(&out).expect("desktop_snapshot emits valid JSON");

    assert!(v.is_object(), "snapshot is an object, got: {out}");
    assert_eq!(
        v.get("connected").and_then(Value::as_bool),
        Some(true),
        "AppView reaches the live core: {out}"
    );
    assert!(
        v.get("mode")
            .and_then(Value::as_str)
            .map(|m| !m.is_empty())
            .unwrap_or(false),
        "snapshot carries a mode: {out}"
    );
}

#[test]
fn desktop_command_status_answers() {
    let Some((_core_guard, sock)) = spawn_core() else {
        eprintln!("BLITZKRIEG_CORE_BIN unset — skipping live-chain assertion");
        return;
    };
    let out = blitzkrieg_webapp::command_json("status", Some(sock))
        .expect("desktop_command returns the CommandOutcome JSON");
    let v: Value = serde_json::from_str(&out).expect("desktop_command emits valid JSON");

    assert_eq!(
        v.get("action").and_then(Value::as_str),
        Some("status"),
        "the verb is recognised: {out}"
    );
    assert_eq!(
        v.get("ok").and_then(Value::as_bool),
        Some(true),
        "status succeeds against the live core: {out}"
    );
}
