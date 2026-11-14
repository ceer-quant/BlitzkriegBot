//! `.env` READ helpers for the kernel (issue #1199 第六轮, 操作者裁决
//! 2026-11-14: `.env` 对内核是**只读**的——一切持久化走 SQLite，唯一的
//! 写入者是安装/生成器，见 `ui_kit_panel` 的 `ensure_env_file_and_credentials`）。
//!
//! The launcher (`ui_kit_panel::env_file`) OWNS the parser contract:
//! `<cwd>/.env` parsed as `KEY=VALUE` lines with `#` comments, one optional
//! matching-quote layer, optional `export ` prefix, malformed lines skipped.
//! This module is the READ side of the same contract — the WRITE primitives
//! (`upsert_env_vars` / `remove_env_vars` / `write_atomic` / `render_line`)
//! were DELETED with the write paths they served; any future attempt to
//! reintroduce them is a contract violation, not a refactor.
//!
//! SECRETS (the same hard line as the launcher): nothing here logs, prints,
//! or echoes a value — callers report key NAMES only.

use std::path::Path;

/// layer stripped — byte-identical to the launcher's parser semantics.
pub fn parse_line(line: &str) -> Option<(String, String)> {
    let key = line_key(line)?;
    let value = line
        .trim()
        .strip_prefix("export ")
        .unwrap_or(line.trim())
        .trim_start()
        .split_once('=')
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let value = if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value
    };
    Some((key, value))
}

/// Read every managed `KEY=value` pair from the `.env` at `path` — the read
/// half of the single-source contract (operator ruling 2026-11-08: the FILE
/// is the authoritative state for the cockpit families; the kernel's
/// in-memory config is a read-only cache derived from it). Returns an empty
/// map for a missing file (a caller that has no env file simply has no file
/// layer) and skips comments/blanks/malformed lines via [`parse_line`].
/// No value is ever logged or echoed by this module.
pub fn read_env_vars(path: &Path) -> std::io::Result<std::collections::BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path)?;
    let mut vars = std::collections::BTreeMap::new();
    for line in text.lines() {
        if let Some((k, v)) = parse_line(line) {
            vars.insert(k, v);
        }
    }
    Ok(vars)
}

/// The key of one `.env` line — the same shape judgment as the launcher's
/// parser (`export ` prefix allowed, identifier-shaped key, first `=` splits).
/// `None` for comments, blanks, and malformed lines (they are never managed).
fn line_key(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, _) = line.split_once('=')?;
    let key = key.trim();
    let valid = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && key
            .chars()
            .all(|c: char| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_is_the_launcher_parser_contract() {
        assert_eq!(parse_line("A=1").unwrap().0, "A");
        assert_eq!(parse_line("A=1").unwrap().1, "1");
        assert_eq!(parse_line("A=\"two words\"").unwrap().1, "two words");
        assert_eq!(parse_line("export A='it'").unwrap().1, "it");
        assert_eq!(
            parse_line("  A = spaced = value ").unwrap().1,
            "spaced = value"
        );
        assert!(parse_line("# comment").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("not-a-line").is_none());
        assert!(parse_line("1BAD=2").is_none());
    }
}
