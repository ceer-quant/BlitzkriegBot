//! Persisted strategy enablement — the operator's runtime intent, across
//! restarts.
//!
//! WHY THIS EXISTS
//!   The kernel ships ZERO enabled strategies (PR-B): a freshly booted core
//!   registers whatever cdylibs the strategy dir holds, every one of them
//!   disabled. That is the right default — but an operator who enables
//!   `my_strategy` in the panel should not have to do it again after every
//!   restart. This module records the effective enabled-set whenever it
//!   changes (a runtime toggle, or the boot-time CLI lists) and replays it at
//!   the next boot.
//!
//! PRECEDENCE (documented in main.rs too)
//!   persisted file ⊕ CLI `--enable-strategy` − CLI `--disable-strategy`.
//!   The file is rewritten by every toggle, so the LAST explicit intent —
//!   runtime or boot flag — wins. Nothing here can enable a strategy the
//!   kernel has not loaded: the replay applies through the same
//!   `set_strategy_enabled` the RPC uses, which fails silently for unknown
//!   names and is warned about.
//!
//! SHAPE
//!   `{"version":1,"enabled":["a","b"]}` — sorted and deduplicated on save, so
//!   the file diffs stably. Missing or malformed file means "nothing
//!   persisted", never an error: a fresh checkout boots with zero strategies.
//!   Writes are atomic (tmp + rename) so a crash mid-toggle cannot leave a
//!   half-written state behind.

use std::path::Path;

/// Read the persisted enabled-set. Missing or malformed file → empty (a warn
/// for malformed, silence for absent — a fresh checkout is not a problem).
pub fn load(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!(
            path = %path.display(),
            "strategy state file is not valid JSON; starting with no strategies enabled"
        );
        return Vec::new();
    };
    v.get("enabled")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Write the enabled-set atomically. Failures are warned, never fatal — a
/// read-only cwd must not take the trading core down over a bookkeeping file.
pub fn save(path: &Path, enabled: &[String]) {
    let mut names: Vec<String> = enabled.to_vec();
    names.sort();
    names.dedup();
    let doc = serde_json::json!({ "version": 1, "enabled": names });
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(path = %path.display(), error = %e, "cannot create strategy state dir");
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, doc.to_string()) {
        tracing::warn!(path = %path.display(), error = %e, "cannot write strategy state");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(path = %path.display(), error = %e, "cannot finalize strategy state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bk-strategy-state-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_then_load_round_trips_sorted_and_deduped() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("strategy-state.json");
        save(
            &path,
            &[
                "alpha_strategy".into(),
                "spread_arb".into(),
                "alpha_strategy".into(),
                "aaa".into(),
            ],
        );
        assert_eq!(load(&path), vec!["aaa", "alpha_strategy", "spread_arb"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\":1"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_means_nothing_persisted() {
        assert!(load(Path::new("/nonexistent/strategy-state.json")).is_empty());
    }

    #[test]
    fn malformed_file_degrades_to_empty_not_error() {
        let dir = tmpdir("malformed");
        let path = dir.join("strategy-state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_creates_missing_parent_dirs() {
        let dir = tmpdir("parents");
        let path = dir.join("deep/nested/strategy-state.json");
        save(&path, &["alpha_strategy".into()]);
        assert_eq!(load(&path), vec!["alpha_strategy"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
