//! One spelling of the durable-JSONL rules.
//!
//! Several logs in this crate are the same shape — one JSON object per line,
//! appended by the running core, read back at startup — and each one had grown
//! its own copy of the same dozen lines on both sides of the file. The loaders:
//! read the file, skip blank lines, parse each line, count the ones that would
//! not parse, warn once with the count and the path. The writers: make the
//! parent directory, serialize, open create+append, write the line.
//!
//! The rules they share are the important part, and both are safety rules:
//! **an unparseable line is skipped, never fatal** — a core that refuses to
//! start because of one corrupt tail is a core that has stopped managing the
//! positions it already holds — and **a failed append is swallowed, never
//! fatal**, because the in-memory state stays authoritative for the run. With
//! several copies, each rule had several places to be got wrong and several
//! places to be read from; with one, it has one.
//!
//! Callers keep what is genuinely theirs: the order log folds its records into a
//! map (later lines win), the other loaders return file order; and a caller that
//! must REPORT a write failure rather than swallow it writes its own bytes
//! instead of calling [`append`].

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Rewrite `path` with `bytes` atomically: write to a unique temp file beside
/// the target, fsync it, then rename over the destination.
///
/// #463: every snapshot-style state file (position book, daily-loss budget,
/// order/compact rewrites, summary.json) must go through HERE, never a bare
/// `std::fs::write` — a truncate-in-place write is exactly the window where a
/// crash leaves an empty or half-written file, and the loader's "missing file =
/// empty set" contract then turns one bad crash into a forgotten position book
/// or a laundered daily-loss budget. With `rename(2)` the destination is either
/// the previous generation or the new one, byte-complete.
///
/// The temp name is unique per writer (pid + in-process counter), same
/// discipline as `onchain::write_json_atomic` — two concurrent writers must not
/// rename the same `X.tmp` out from under each other. Returns `false` when any
/// step failed (the caller decides whether that is warn-and-continue or fatal;
/// memory stays authoritative either way). The write is fsynced so the rename
/// never exposes a page-cache-only generation to a power cut.
pub(crate) fn rewrite_atomic(path: &Path, bytes: &[u8]) -> bool {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let ok = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if ok.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    ok.is_ok()
}

/// Read `path` as one JSON value per line, skipping blank and unparseable lines.
///
/// A missing file is an empty log, not an error — on a fresh data directory
/// nothing has been written yet.
///
/// `skipped_warning` is the message emitted, once, if any line was skipped. It is
/// passed whole rather than assembled from a label so each log keeps the wording
/// an operator already greps for.
pub(crate) fn load<T: serde::de::DeserializeOwned>(path: &Path, skipped_warning: &str) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(v) => out.push(v),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "{skipped_warning}");
    }
    out
}

/// Append one JSON object as a line to `path`, creating the parent directory.
///
/// Best effort by construction: every failure is swallowed. That is the shared
/// contract of the logs that use it — a trade, an order snapshot or an audit row
/// that cannot be persisted must not interrupt trading, because the in-memory
/// state is authoritative for the run. A caller that needs to know about a
/// failure must write its own bytes — or read the return value (E25 / #331:
/// `true` when a line landed; the intent-audit sink warns once on `false`,
/// without ever blocking the trade).
pub(crate) fn append(path: &Path, value: &impl serde::Serialize) -> bool {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut buf) = serde_json::to_vec(value) else {
        return false;
    };
    buf.push(b'\n');
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // ONE write_all per record. With O_APPEND every write() lands at the
        // current end atomically, so concurrent appenders can never fuse two
        // records into one unparseable line. The two-syscall spelling this
        // replaced (`writeln!` writes the text, THEN the newline) is exactly
        // how two interleaved appends destroyed both records at once — the
        // #386 flake, where the loader's skip-unparseable discipline
        // silently swallowed two audit lines two tests were counting.
        Ok(mut f) => f.write_all(&buf).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #463: the atomic rewrite never leaves the destination torn — a reader
    /// concurrent with (or dying during) the write sees either the old
    /// generation or the new one, never an empty or half file.
    #[test]
    fn atomic_rewrite_never_leaves_a_torn_destination() {
        let dir = std::env::temp_dir().join(format!(
            "bk-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("state.json");

        assert!(rewrite_atomic(&path, br#"{"gen":1}"#));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"gen":1}"#);
        // Overwrite with a LONGER payload: the truncate happens on the temp
        // file, never on the destination.
        assert!(rewrite_atomic(
            &path,
            br#"{"gen":2,"much":"longer payload"}"#
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"gen":2,"much":"longer payload"}"#
        );
        // No tmp litter survives a successful run.
        let litter: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(litter.is_empty(), "tmp files cleaned up: {litter:?}");

        // A failed write (unwritable parent) leaves the OLD generation intact.
        let readonly_dir = dir.join("ro");
        std::fs::create_dir_all(&readonly_dir).unwrap();
        let ro_path = readonly_dir.join("state.json");
        assert!(rewrite_atomic(&ro_path, br#"{"gen":1}"#));
        // Make the directory read-only so the temp create fails.
        use std::os::unix::fs::PermissionsExt as _;
        let mut p = std::fs::metadata(&readonly_dir).unwrap().permissions();
        p.set_mode(0o500);
        std::fs::set_permissions(&readonly_dir, p).unwrap();
        assert!(!rewrite_atomic(&ro_path, br#"{"gen":2}"#));
        assert_eq!(
            std::fs::read_to_string(&ro_path).unwrap(),
            r#"{"gen":1}"#,
            "a failed rewrite must not damage the previous generation"
        );
        let mut q = std::fs::metadata(&readonly_dir).unwrap().permissions();
        q.set_mode(0o700);
        std::fs::set_permissions(&readonly_dir, q).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #386: concurrent appenders share one journal file in production's
    /// worst case, and a torn line is a SILENT loss — the loader's
    /// skip-unparseable discipline is a safety rule for trading, which makes
    /// "the line landed whole" the append's own job. Threads × records per
    /// thread go in through [`append`]; the loader must read every one of
    /// them back parseable. (Against the old two-syscall append this went
    /// red under contention: text/newline interleaving fused two records
    /// into one line and both vanished.)
    #[test]
    fn concurrent_appends_never_fuse_lines() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 150;
        let dir = std::env::temp_dir().join(format!(
            "bk-jsonl-fuse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("audit.jsonl");
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    assert!(append(&path, &serde_json::json!({ "thread": t, "i": i }),));
                }
            }));
        }
        for h in handles {
            h.join().expect("append thread");
        }
        let loaded = load::<serde_json::Value>(&path, "test: skipped unparseable lines");
        assert_eq!(
            loaded.len(),
            THREADS * PER_THREAD,
            "every appended record must survive concurrent appends whole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
