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
/// failure must write its own bytes.
pub(crate) fn append(path: &Path, value: &impl serde::Serialize) {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}
