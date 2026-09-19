//! `.env` self-loading for the `blitzkrieg` launcher.
//!
//! WHY THIS EXISTS
//!   The start recipe used to be three lines — `set -a; source .env; set +a;
//!   blitzkrieg run` — because panel credentials and `HFT_*` knobs arrive as
//!   environment variables and nothing loaded the repo's own `.env`. That is
//!   three lines of ceremony a one-command launcher should not have, and it
//!   silently changed behaviour when forgotten (panel drops to read-only view).
//!   Now the launcher reads `<cwd>/.env` itself before anything else starts.
//!
//! PRECEDENCE (the one rule that must never bend)
//!   A variable already in the environment ALWAYS wins over the file. This
//!   matches dotenv convention and the kernel's documented config precedence
//!   (CLI > environment > file > default): an explicit value on the operator's
//!   shell must never be silently replaced by a stale line in a file.
//!
//! SECRETS (a hard line)
//!   The file carries real credentials. Nothing here prints, logs, or echoes
//!   a value — the only observability is a COUNT of newly-set keys. Parsing is
//!   deliberately tiny: `KEY=VALUE` lines, `#` comments, blank lines, one
//!   optional layer of matching quotes around the value, optional `export `
//!   prefix. Anything else is skipped, not guessed at.

use std::path::Path;

/// Parse `.env` text into `(key, value)` pairs. Malformed lines are skipped,
/// never guessed at.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Optional `export ` prefix, so files written for shells also work.
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // Identifier-shaped: a shell cannot export anything else, so a line
        // like `9starts=3` or `has-dash=1` is a typo, not a variable.
        let valid_key = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            continue;
        }
        let mut value = value.trim().to_string();
        // One optional layer of matching quotes; no escape processing — a
        // value that needs escaping belongs in the shell, not in this file.
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        out.push((key.to_string(), value));
    }
    out
}

/// Load `<cwd>/.env` if present. Returns the number of variables actually set
/// (pre-existing environment wins, so a key already exported does not count).
/// Absent file: perfectly normal (CI, other checkouts) — zero noise.
pub fn load_cwd_env() -> usize {
    load_env_file(Path::new(".env"))
}

/// Load a specific `.env` file. See the module docs for precedence and secrets.
pub fn load_env_file(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    parse_env_file(&text)
        .into_iter()
        .filter(|(k, v)| {
            if std::env::var(k).is_err() {
                std::env::set_var(k, v);
                true
            } else {
                false
            }
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_this_repo_actually_uses() {
        let parsed = parse_env_file(
            "\
# comment line
BLITZKRIEG_PANEL_USER=fancer
BLITZKRIEG_PANEL_PASSWORD=\"quoted pass\"
DRY_RUN='true'

export HFT_ROUND_SEC=900
BAD LINE WITHOUT EQUALS
=NOKEY
OK_EMPTY=
",
        );
        assert_eq!(
            parsed,
            vec![
                ("BLITZKRIEG_PANEL_USER".into(), "fancer".into()),
                ("BLITZKRIEG_PANEL_PASSWORD".into(), "quoted pass".into()),
                ("DRY_RUN".into(), "true".into()),
                ("HFT_ROUND_SEC".into(), "900".into()),
                ("OK_EMPTY".into(), String::new()),
            ]
        );
    }

    #[test]
    fn keys_must_be_identifier_shaped() {
        assert!(parse_env_file("has-dash=1\nhas space=2\n9starts=3").is_empty());
        assert_eq!(parse_env_file("GOOD_KEY_1=v")[0].0, "GOOD_KEY_1");
    }

    #[test]
    fn a_preexisting_variable_wins_over_the_file() {
        // Precedence is the one rule that must never bend, so it is pinned with
        // a throwaway key: whatever the file says, an already-exported value
        // stays. (set/remove in one test to leave the process env untouched.)
        let dir = std::env::temp_dir().join(format!("bk-env-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "BK_ENV_FILE_PRECEDENCE_PROBE=from_file\nBK_ENV_FILE_NEW_PROBE=also_file\n",
        )
        .unwrap();

        std::env::set_var("BK_ENV_FILE_PRECEDENCE_PROBE", "from_shell");
        let set = load_env_file(&path);
        assert_eq!(
            std::env::var("BK_ENV_FILE_PRECEDENCE_PROBE").unwrap(),
            "from_shell",
            "an exported value must beat the file"
        );
        assert_eq!(std::env::var("BK_ENV_FILE_NEW_PROBE").unwrap(), "also_file");
        // Only the genuinely-new key counts as "set".
        assert_eq!(set, 1);

        std::env::remove_var("BK_ENV_FILE_PRECEDENCE_PROBE");
        std::env::remove_var("BK_ENV_FILE_NEW_PROBE");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_silently_zero() {
        assert_eq!(load_env_file(Path::new("/nonexistent/.env-definitely")), 0);
    }
}
