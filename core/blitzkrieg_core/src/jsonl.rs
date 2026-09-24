//! One spelling of the durable-JSONL read rule.
//!
//! Four logs in this crate are the same shape — one JSON object per line,
//! appended by the running core, read back at startup — and each loader had
//! grown its own copy of the same dozen lines: read the file, skip blank lines,
//! parse each line, count the ones that would not parse, warn once with the
//! count and the path.
//!
//! The rule they share is the important part, and it is a safety rule:
//! **an unparseable line is skipped, never fatal.** A core that refuses to start
//! because of one corrupt tail is a core that has stopped managing the positions
//! it already holds. With four copies, that rule had four places to be got wrong
//! and four places to be read from; with one, it has one.
//!
//! Callers keep what is genuinely theirs: the order log folds its records into a
//! map (later lines win), the other three return file order.

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
