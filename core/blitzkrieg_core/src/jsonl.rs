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
/// discipline as `blitzkrieg_onchain_pull::write_json_atomic` — two concurrent
/// writers must not
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

/// The newest tail of a JSONL log, filtered, WITHOUT reading the whole file —
/// the shape `intent.audit.tail` needs (§12.3: 「读尾部，不读全文件」).
///
/// Reads backwards in ~1 MB chunks from the file end, parses each complete
/// line, keeps the ones where `keep` says yes, and stops as soon as `limit`
/// matching lines are held or the file head is reached. Returns the matches
/// in FILE ORDER (oldest → newest, the contract the tail verb answers in)
/// plus how many matched within the scanned span and whether that span is
/// the WHOLE file — a caller reporting a `total` must omit or qualify it
/// when `whole_file` is false, because earlier (unscanned) rows were never
/// counted.
///
/// The trailing partial line (a torn final record mid-append) is skipped, the
/// same discipline [`load`] applies to unparseable lines; the LAST complete
/// line before it still parses. A record spanning more than one chunk is
/// still found: the scan keeps prepending bytes until a `\n` boundary shows
/// up or the head says there is none.
pub(crate) fn tail_filtered<T: serde::de::DeserializeOwned>(
    path: &Path,
    skipped_warning: &str,
    limit: usize,
    mut keep: impl FnMut(&T) -> bool,
) -> (Vec<T>, usize, bool) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), 0, true);
    };
    let file_len = match file.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(_) => return (Vec::new(), 0, true),
    };
    const CHUNK: u64 = 1 << 20;
    let mut scanned: u64 = 0;
    // The TAIL bytes of the line still being assembled: everything after the
    // last \n of the scanned region, up to (and including, when the record
    // was completed) the region's end. The line's HEAD lies at addresses the
    // scan has not reached yet. Empty = no partial line held.
    let mut pending: Vec<u8> = Vec::new();
    // Parsed matches, NEWEST → OLDEST: every backward round completes lines
    // strictly older than everything already held.
    let mut matched_rev: Vec<T> = Vec::new();
    // Matches seen across the whole scan, BEFORE the window truncation: the
    // honest `total` when the scan reached the file head (one chunk can hold
    // more matches than the window and the count must not shrink with it).
    let mut matched_total = 0usize;
    let mut skipped = 0usize;
    let mut whole_file = file_len == 0;
    while matched_total < limit {
        let take = CHUNK.min(file_len - scanned);
        let start = file_len - scanned - take;
        let mut buf = vec![0u8; take as usize];
        if file.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), 0, false);
        }
        if file.read_exact(&mut buf).is_err() {
            return (Vec::new(), 0, false);
        }
        scanned += take;
        whole_file = scanned == file_len;
        let mut chunk_matched: Vec<T> = Vec::new();
        // The chunk's FIRST \n terminates whatever line contains the chunk's
        // first byte; its LAST \n hands the pending line its missing head.
        // They differ only when the chunk holds complete middle lines.
        if let Some(pos) = buf.iter().rposition(|b| *b == b'\n') {
            let first_nl = buf.iter().position(|b| *b == b'\n').unwrap_or(pos);
            // The pending line starts right after the last \n: glue ONLY
            // those bytes onto its carried tail (gluing across the chunk's
            // internal newlines would fuse unrelated records). Pushed AFTER
            // the middle lines — its bytes sit at the highest addresses, so
            // it is this round's NEWEST line and the round reverses into
            // newest-first below.
            let mut glued = buf[pos + 1..].to_vec();
            glued.extend_from_slice(&pending);
            // The line containing the chunk's first byte keeps only its
            // in-chunk tail, through the first \n; earlier rounds supply its
            // head, and at the file head it parses as the oldest record.
            pending = buf[..=first_nl].to_vec();
            if let Some(middles) = buf.get(first_nl + 1..pos) {
                for line in middles.split(|b| *b == b'\n') {
                    let line = std::str::from_utf8(line).unwrap_or("").trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<T>(line) {
                        Ok(v) if keep(&v) => chunk_matched.push(v),
                        Ok(_) => {}
                        Err(_) => skipped += 1,
                    }
                }
            }
            let glued = std::str::from_utf8(&glued).unwrap_or("").trim();
            if !glued.is_empty() {
                match serde_json::from_str::<T>(glued) {
                    Ok(v) if keep(&v) => chunk_matched.push(v),
                    Ok(_) => {}
                    Err(_) => skipped += 1,
                }
            }
        } else {
            // No \n in the chunk: the whole chunk is one more run of the
            // pending line's middle — the chunk sits at LOWER addresses,
            // so it PREPENDS.
            let mut np = buf;
            np.extend_from_slice(&pending);
            pending = np;
        }
        // This chunk covers an EARLIER span than everything already held and
        // its rows ran old → new: reversing the chunk's matches makes the
        // global vec newest → oldest.
        chunk_matched.reverse();
        matched_total += chunk_matched.len();
        matched_rev.extend(chunk_matched);
        if whole_file {
            break;
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "{skipped_warning}");
    }
    // The head is reached: whatever `pending` holds is the file's FIRST line
    // — complete once its terminating \n has been scanned (a file with no \n
    // at all arrives here whole), or torn, and the parse below skips it like
    // any unparseable line. It is the oldest record, and like the middle
    // lines it counts toward `matched_total` only when `keep` says yes.
    if whole_file && !pending.is_empty() {
        let line = std::str::from_utf8(&pending).unwrap_or("").trim();
        match serde_json::from_str::<T>(line) {
            Ok(v) if keep(&v) => {
                matched_total += 1;
                matched_rev.push(v);
            }
            Ok(_) => {}
            Err(_) if !line.is_empty() => {
                skipped += 1;
                tracing::warn!(skipped, path = %path.display(), "{skipped_warning}");
            }
            Err(_) => {}
        }
    }
    // matched_rev is globally newest → oldest: keeping the first `limit` and
    // reversing yields file order (oldest → newest) within the window. The
    // dropped overflow is the oldest end, exactly what a tail read drops.
    matched_rev.truncate(limit);
    matched_rev.reverse();
    (matched_rev, matched_total, whole_file)
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

    /// Scratch dir for the tail tests, same temp-dir discipline as above.
    fn tail_scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-tail-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Row {
        n: u32,
        tag: String,
    }

    #[test]
    fn tail_returns_the_newest_matches_in_file_order() {
        let dir = tail_scratch("order");
        let path = dir.join("audit.jsonl");
        for n in 0..50u32 {
            assert!(append(
                &path,
                &serde_json::json!({ "n": n, "tag": if n % 2 == 0 { "even" } else { "odd" } }),
            ));
        }
        let (rows, matched, whole) = tail_filtered::<Row>(&path, "test: skipped", 10, |_| true);
        assert!(
            whole,
            "50 small rows fit one chunk — the whole file scanned"
        );
        // `matched` is the SCAN's match count (independent of the window):
        // 50 rows all matched, the window just carries the newest 10.
        assert_eq!(matched, 50);
        let first = rows.first().expect("rows");
        assert_eq!(first.n, 40, "the newest 10, oldest-first within the window");
        assert_eq!(rows.last().expect("rows").n, 49);
        // A filter cuts through: newest 3 even rows; the count still names
        // every even row in the scanned span.
        let (rows, matched, whole) =
            tail_filtered::<Row>(&path, "test: skipped", 3, |r: &Row| r.tag == "even");
        assert!(whole);
        assert_eq!(matched, 25);
        assert_eq!(
            rows.iter().map(|r| r.n).collect::<Vec<_>>(),
            vec![44, 46, 48]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_assembles_lines_that_span_chunks() {
        // Records wider than the 1 MB scan chunk: a line the backward scan
        // meets mid-way must still be assembled whole (the fix the full-read
        // replaced was never wrong about, and the backward scan must not be).
        let dir = tail_scratch("wide");
        let path = dir.join("audit.jsonl");
        let filler = "x".repeat(3 * (1 << 20));
        for n in 0..3u32 {
            assert!(append(&path, &serde_json::json!({ "n": n, "tag": filler }),));
        }
        let (rows, matched, whole) = tail_filtered::<Row>(&path, "test: skipped", 2, |_| true);
        assert!(
            !whole,
            "limit=2 stops the scan at the second match, by design"
        );
        assert_eq!(matched, 2, "the scan count only names what the scan saw");
        assert_eq!(
            rows.iter().map(|r| r.n).collect::<Vec<_>>(),
            vec![1, 2],
            "rows 2 and 1 span four 1 MB chunks each and still come back whole"
        );
        // A window wider than the file lets the scan reach the head: the
        // first record (the line `pending` holds there) is the oldest row.
        let (rows, matched, whole) = tail_filtered::<Row>(&path, "test: skipped", 5, |_| true);
        assert!(whole);
        assert_eq!(matched, 3);
        assert_eq!(rows.iter().map(|r| r.n).collect::<Vec<_>>(), vec![0, 1, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_skips_a_torn_final_line_and_still_parses_the_last_complete_one() {
        let dir = tail_scratch("torn");
        let path = dir.join("audit.jsonl");
        assert!(append(&path, &serde_json::json!({ "n": 1, "tag": "a" })));
        assert!(append(&path, &serde_json::json!({ "n": 2, "tag": "b" })));
        // Simulate a crash mid-append: the final record is cut in half.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(br#"{"n":3,"tag":"c"#).unwrap();
        drop(f);
        let (rows, matched, whole) = tail_filtered::<Row>(&path, "test: skipped", 10, |_| true);
        assert!(whole);
        assert_eq!(matched, 2, "the torn tail line is skipped, not fatal");
        assert_eq!(rows.iter().map(|r| r.n).collect::<Vec<_>>(), vec![1, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_on_a_missing_file_is_an_empty_whole_file_scan() {
        let (rows, matched, whole) = tail_filtered::<Row>(
            std::path::Path::new("/nonexistent/bk-tail/none.jsonl"),
            "test: skipped",
            5,
            |_| true,
        );
        assert!(rows.is_empty() && matched == 0 && whole);
    }
}
