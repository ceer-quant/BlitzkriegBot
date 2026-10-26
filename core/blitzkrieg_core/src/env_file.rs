//! `.env` writing for the kernel's credential path (`market.credentials.set`).
//!
//! The launcher (`ui_kit_panel::env_file`) OWNS the read side: `<cwd>/.env`
//! parsed as `KEY=VALUE` lines with `#` comments, one optional matching-quote
//! layer, optional `export ` prefix, malformed lines skipped. This module is
//! the write side of the same contract — a value this module writes must read
//! back identically through the launcher's parser.
//!
//! SINGLE SOURCE (the ruling this serves): `.env` is the only persistent home
//! for UI-submitted credentials. The old `data/runtime/config.json`
//! credentials cell is retired (#649's ruling — operator picked full
//! consolidation); a boot-time one-shot migrates any legacy cell into this
//! file and strips the key.
//!
//! SECRETS (the same hard line as the read side): nothing here logs, prints,
//! or echoes a value — callers report key NAMES only. Writes are atomic
//! (temp + rename) and owner-only 0600, and every line outside the managed
//! keys is preserved byte-for-byte.

use std::path::Path;

/// Render one `KEY=value` line the launcher's parser round-trips: double
/// quotes by default, single quotes when the value itself carries a `"`,
/// and a refusal when it carries both (no escaping exists on the read side —
/// such a value cannot be expressed in this format).
fn render_line(key: &str, value: &str) -> Result<String, String> {
    let has_dq = value.contains('"');
    let has_sq = value.contains('\'');
    if has_dq && has_sq {
        return Err(format!(
            "value for {key} contains both quote kinds — the .env format has no escapes; \
             rephrase the credential"
        ));
    }
    let (q, v) = if has_dq { ('\'', value) } else { ('"', value) };
    Ok(format!("{key}={q}{v}{q}"))
}

/// Upsert `vars` into the `.env` at `path`: an existing managed line is
/// replaced in place (position and the rest of the file preserved); a new key
/// is appended at the end. Returns the key names actually written. Creates
/// the file (and its parent directory) when absent.
pub fn upsert_env_vars(
    path: &Path,
    vars: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<Vec<String>> {
    if vars.is_empty() {
        return Ok(Vec::new());
    }
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .map(|t| t.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();
    let existing = env_file_key_set(&lines);
    for (key, value) in vars {
        let line = render_line(key, value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        if existing.contains(key.as_str()) {
            for l in lines.iter_mut() {
                if line_key(l).as_deref() == Some(key.as_str()) {
                    *l = line.clone();
                }
            }
        } else {
            lines.push(line);
        }
    }
    write_atomic(path, &lines)?;
    Ok(vars.keys().cloned().collect())
}

/// Remove every line whose key is in `keys`. Returns the key names actually
/// removed. All other lines survive byte-for-byte.
pub fn remove_env_vars(path: &Path, keys: &[&str]) -> std::io::Result<Vec<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let mut removed = Vec::new();
    let lines: Vec<String> = text
        .lines()
        .filter(|l| match line_key(l) {
            Some(k) if keys.contains(&k.as_str()) => {
                removed.push(k);
                false
            }
            _ => true,
        })
        .map(|l| l.to_string())
        .collect();
    if removed.is_empty() {
        return Ok(removed);
    }
    write_atomic(path, &lines)?;
    Ok(removed)
}

/// The key set of a rendered line list (the upsert fast path).
fn env_file_key_set(lines: &[String]) -> std::collections::HashSet<String> {
    lines.iter().filter_map(|l| line_key(l)).collect()
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

fn write_atomic(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = lines.join("\n");
    text.push('\n');
    let tmp = path.with_extension("env.tmp");
    std::fs::write(&tmp, text)?;
    // Owner-only: the file carries credentials. On the TEMP file before the
    // rename — the final path may not exist yet, and a chmod on a missing
    // file would silently leave the first write world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_env(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bk-env-write-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(".env")
    }

    #[test]
    fn upsert_replaces_in_place_and_appends_new_keys() {
        let path = tmp_env("upsert");
        std::fs::write(&path, "# operator comment\nA=1\nB=\"two words\"\n").unwrap();
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("A".to_string(), "9".to_string());
        vars.insert("C".to_string(), "x y".to_string());
        let written = upsert_env_vars(&path, &vars).unwrap();
        assert_eq!(written.len(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# operator comment\nA=\"9\"\nB=\"two words\"\nC=\"x y\"\n"
        );
        // Round-trips through the launcher's read side.
        let parsed = ui_kit_panel_env_semantics(&text);
        assert_eq!(parsed.iter().find(|(k, _)| k == "A").unwrap().1, "9");
        assert_eq!(parsed.iter().find(|(k, _)| k == "C").unwrap().1, "x y");
        assert_eq!(
            parsed.iter().find(|(k, _)| k == "B").unwrap().1,
            "two words"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn remove_drops_only_managed_lines() {
        let path = tmp_env("remove");
        std::fs::write(&path, "KEEP=1\nDROP=2\n# c\nexport DROP=3\n").unwrap();
        let removed = remove_env_vars(&path, &["DROP"]).unwrap();
        assert_eq!(removed, vec!["DROP".to_string(), "DROP".to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "KEEP=1\n# c\n");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn both_quote_kinds_is_a_refusal_not_a_silent_corruption() {
        let path = tmp_env("quotes");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("K".to_string(), "he said \"a\" and 'b'".to_string());
        let err = upsert_env_vars(&path, &vars).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!path.exists(), "the refused write must not create the file");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn writes_are_owner_only() {
        let path = tmp_env("perm");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("K".to_string(), "v".to_string());
        upsert_env_vars(&path, &vars).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// The launcher's parser semantics (ui_kit_panel::env_file::parse_env_file),
    /// re-stated here so the round-trip test does not depend on the panel
    /// crate. Keep in byte-level sync with that file.
    fn ui_kit_panel_env_semantics(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            if !(k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && k.chars()
                    .all(|c: char| c.is_ascii_alphanumeric() || c == '_'))
            {
                continue;
            }
            let mut v = v.trim().to_string();
            if v.len() >= 2
                && ((v.starts_with('"') && v.ends_with('"'))
                    || (v.starts_with('\'') && v.ends_with('\'')))
            {
                v = v[1..v.len() - 1].to_string();
            }
            out.push((k.to_string(), v));
        }
        out
    }
}
