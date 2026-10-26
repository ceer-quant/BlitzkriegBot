//! `data/runtime/config.json` — the runtime settings cell the WebUI writes
//! through `round_duration.set` (the round-duration card). Same model as
//! `data/update/state.json`: a small durable JSON doc that survives restarts
//! and outranks the factory TOML, but never an explicit CLI flag.
//!
//! Next-session semantics by construction: the ONLY consumer of this cell is
//! the boot resolution in `main.rs`, so a write cannot move the running
//! session's round grid — the grid is a coordinate system (`round_slot` is a
//! UTC grid index derived with the session's duration), and live
//! positions/orders were slotted under the old length.

use std::path::PathBuf;

/// cwd-relative, like every other `data/` writer.
fn state_path() -> PathBuf {
    PathBuf::from("data/runtime/config.json")
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RuntimeConfigDoc {
    /// `None` = the cell holds no round-duration override (either never
    /// written, or cleared by `reset`) — the boot resolution falls through to
    /// env/TOML/default.
    #[serde(skip_serializing_if = "Option::is_none")]
    round_duration_sec: Option<i64>,
    /// Who wrote it (uid:… from the IPC peer identity).
    #[serde(skip_serializing_if = "Option::is_none")]
    round_duration_actor: Option<String>,
    /// Wall-clock of the write.
    #[serde(skip_serializing_if = "Option::is_none")]
    round_duration_at_ms: Option<i64>,
    /// `None` = the cell holds no market-plugin override — the boot
    /// resolution falls through to `--market-plugin`/first-registered.
    /// Next-session: the running session's feed/discovery/executor were all
    /// built around the plugin chosen at ITS boot, so a write cannot move
    /// them (the same reason the round grid cannot move).
    #[serde(skip_serializing_if = "Option::is_none")]
    market_plugin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    market_plugin_actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    market_plugin_at_ms: Option<i64>,
    // The legacy credentials cell (`plugin` → `env var name` → value) lived
    // here until the .env consolidation (operator ruling: `.env` is the single
    // persistent home for UI-submitted credentials). The field is gone from
    // the struct — serde ignores the unknown key on read, and
    // `migrate_credentials_cell_to_env` drains any legacy values into `.env`
    // before the next sibling-cell write would silently drop them.
}

/// What the boot resolution needs to know about the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundDurationCell {
    pub duration_sec: i64,
}

/// Read the persisted round duration, if the cell holds one. A missing or
/// broken file is the normal first-boot case → `None`, no drama (the same
/// posture as `version::load_from`).
///
/// Which durations are legal is NOT this layer's business: the declared union
/// of the market plugins' `supported_round_secs` (`MarketPluginRegistry::
/// allowed_round_secs`) owns that, and the config layer cannot see the
/// registry — the market-plugin cell below follows the same split. This
/// reader therefore only reports what the cell holds; the boot caller
/// (`main.rs`) passes the union in as `allowed` and applies the admission
/// rule itself (out-of-union values are ignored with a warning — a
/// hand-edited cell must not be able to arm a duration no plugin declared).
pub fn load_round_duration() -> Option<RoundDurationCell> {
    load_round_duration_from(&state_path(), &[])
}

pub fn load_round_duration_from(
    path: &std::path::Path,
    allowed: &[i64],
) -> Option<RoundDurationCell> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "{} is not valid JSON — the round-duration cell is ignored: {e}",
                path.display()
            );
            return None;
        }
    };
    let sec = v.get("roundDurationSec").and_then(|s| s.as_i64());
    let Some(sec) = sec else {
        return None; // cell present but holds no round duration — not an error
    };
    if !allowed.contains(&sec) {
        tracing::warn!(
            "{} holds roundDurationSec={sec}, which is not in the market plugins' \
             declared round-duration union {allowed:?} — the cell is ignored",
            path.display()
        );
        return None;
    }
    Some(RoundDurationCell { duration_sec: sec })
}

/// The market-plugin cell the boot resolution reads back. The valid names
/// belong to the compiled-in registry — which the config layer cannot see —
/// so the caller (boot, in `main.rs`) validates the name against
/// `MarketPluginRegistry::names()` itself; this reader only reports what the
/// cell holds (the same reader-reports/caller-validates split the
/// round-duration cell above runs against the plugins' declared union).
#[derive(Debug, Clone, PartialEq)]
pub struct MarketPluginCell {
    pub plugin: String,
}

pub fn load_market_plugin() -> Option<MarketPluginCell> {
    load_market_plugin_from(&state_path())
}

pub fn load_market_plugin_from(path: &std::path::Path) -> Option<MarketPluginCell> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "{} is not valid JSON — the market-plugin cell is ignored: {e}",
                path.display()
            );
            return None;
        }
    };
    let name = v
        .get("marketPlugin")
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    Some(MarketPluginCell { plugin: name })
}

/// Write (or, with `None`, clear) the persisted round duration. Atomic write:
/// same-directory temp + rename, so a half-written file can never be what the
/// next startup reads. Best-effort at the call sites — a persist failure is
/// surfaced in the reply (`persisted: false`-style), never a kernel stop.
pub fn persist_round_duration(
    duration_sec: Option<i64>,
    actor: &str,
    now_ms: i64,
) -> std::io::Result<()> {
    persist_round_duration_to(&state_path(), duration_sec, actor, now_ms)
}

/// Write (or clear) the persisted market-plugin choice. Same atomic
/// read-modify-write contract as the round-duration writer: both cells share
/// one file, so neither write may clobber the other.
pub fn persist_market_plugin(
    plugin: Option<&str>,
    actor: &str,
    now_ms: i64,
) -> std::io::Result<()> {
    persist_market_plugin_to(&state_path(), plugin, actor, now_ms)
}

pub fn persist_market_plugin_to(
    path: &std::path::Path,
    plugin: Option<&str>,
    actor: &str,
    now_ms: i64,
) -> std::io::Result<()> {
    let market_plugin_actor = plugin.map(|_| actor.to_string());
    let market_plugin_at_ms = plugin.map(|_| now_ms);
    let mut doc: RuntimeConfigDoc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    doc.market_plugin = plugin.map(|p| p.to_string());
    doc.market_plugin_actor = market_plugin_actor;
    doc.market_plugin_at_ms = market_plugin_at_ms;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, path)
}

pub fn persist_round_duration_to(
    path: &std::path::Path,
    duration_sec: Option<i64>,
    actor: &str,
    now_ms: i64,
) -> std::io::Result<()> {
    // Read-modify-write so a future second cell in this file survives an
    // unrelated round-duration write. A broken existing file is overwritten
    // (the load path already warned); there is nothing else in it worth
    // keeping. Every round-duration field is written together — a reset
    // must not leave the old actor/at behind.
    let round_duration_actor = duration_sec.map(|_| actor.to_string());
    let round_duration_at_ms = duration_sec.map(|_| now_ms);
    let mut doc: RuntimeConfigDoc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    doc.round_duration_sec = duration_sec;
    doc.round_duration_actor = round_duration_actor;
    doc.round_duration_at_ms = round_duration_at_ms;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, path)
}

/// One-shot boot migration: drain any legacy credentials cell from the
/// runtime config (the retired `market.credentials.set` landing format) into
/// the `.env` single source, then strip the key from the config file so a
/// later sibling-cell write cannot resurrect it. Returns the number of
/// credential VALUES moved. Idempotent: a file without the key moves zero.
/// A missing config, a missing `.env` path, or an unreadable cell is the
/// normal first-boot case — zero, no drama. Values never appear in logs;
/// the caller reports the migrated KEY names if it wants to.
pub fn migrate_credentials_cell_to_env(
    config_path: &std::path::Path,
    env_path: &std::path::Path,
) -> std::io::Result<usize> {
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(_) => return Ok(0),
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(0);
    };
    let Some(all) = v.get("credentials").and_then(|c| c.as_object()) else {
        return Ok(0);
    };
    let mut moved = 0usize;
    let mut drained: std::collections::BTreeMap<String, String> = Default::default();
    for (_plugin, vars) in all {
        let Some(vars) = vars.as_object() else {
            continue;
        };
        for (name, value) in vars {
            let Some(s) = value.as_str() else { continue };
            if s.is_empty() {
                continue;
            }
            drained.insert(name.clone(), s.to_string());
            moved += 1;
        }
    }
    if drained.is_empty() {
        return Ok(0);
    }
    crate::env_file::upsert_env_vars(env_path, &drained)?;
    // Strip the drained key from the config: read back through the strict
    // doc type (unknown field ignored) and rewrite — the cell cannot come
    // back through a sibling write.
    let doc: RuntimeConfigDoc = serde_json::from_str(&text).unwrap_or_default();
    let tmp = config_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, config_path)?;
    Ok(moved)
}

/// #458: append one JSONL record for a manual, destructive operation
/// (`system.reset_trading`; #456's force-close rides the same file). The
/// manual-ops audit is the ONLY trace a reset leaves — the wiped records are
/// gone — so a write failure is an ERROR, not a warn-and-continue: an
/// unaudited reset must fail loudly (the caller surfaces it as a refusal).
///
/// Path convention: the config carries the explicit path (production
/// `data/audit/manual-ops.jsonl`); the config also has `manual_ops_audit_path`
/// in the shape of the other audit paths. Appending to an existing file is the
/// whole model — audit streams are append-only, never cleared by anything.
pub fn append_manual_op(path: &std::path::Path, record: &serde_json::Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Count non-empty lines of a JSONL record file — the pre-reset summary's
/// "how much is about to be deleted" figure. A missing file counts as 0.
pub fn count_jsonl_lines(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The historical kernel whitelist, now spelled as the plugins' declared
    /// union (polymarket's five families ∪ kalshi's 900). Test-fixture only:
    /// the assertion semantics stay exactly what they were.
    const TEST_ALLOWED: &[i64] = &[300, 900, 3600, 14400, 86400];

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bk-runtime-cfg-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_missing_file_loads_as_none() {
        let d = tmpdir("missing");
        assert_eq!(
            load_round_duration_from(&d.join("state.json"), TEST_ALLOWED),
            None
        );
    }

    #[test]
    fn a_written_round_trips_through_load() {
        let d = tmpdir("roundtrip");
        let p = d.join("config.json");
        persist_round_duration_to(&p, Some(300), "uid:test", 1_000).unwrap();
        assert_eq!(
            load_round_duration_from(&p, TEST_ALLOWED),
            Some(RoundDurationCell { duration_sec: 300 })
        );
    }

    #[test]
    fn reset_clears_the_cell_and_load_falls_through() {
        let d = tmpdir("reset");
        let p = d.join("config.json");
        persist_round_duration_to(&p, Some(300), "uid:test", 1_000).unwrap();
        persist_round_duration_to(&p, None, "uid:test", 2_000).unwrap();
        assert_eq!(load_round_duration_from(&p, TEST_ALLOWED), None);
    }

    #[test]
    fn an_off_whitelist_value_is_ignored_not_armed() {
        let d = tmpdir("whitelist");
        let p = d.join("config.json");
        std::fs::write(
            &p,
            r#"{"roundDurationSec": 60, "roundDurationActor": "hand-edit"}"#,
        )
        .unwrap();
        assert_eq!(load_round_duration_from(&p, TEST_ALLOWED), None);
        // An EMPTY allowed set (no plugin declared anything) refuses every
        // value — fail-closed, never "allow everything".
        std::fs::write(
            &p,
            r#"{"roundDurationSec": 300, "roundDurationActor": "hand-edit"}"#,
        )
        .unwrap();
        assert_eq!(load_round_duration_from(&p, &[]), None);
    }

    #[test]
    fn a_broken_file_is_overwritten_by_the_next_write() {
        let d = tmpdir("broken");
        let p = d.join("config.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_round_duration_from(&p, TEST_ALLOWED), None);
        persist_round_duration_to(&p, Some(900), "uid:test", 3_000).unwrap();
        assert_eq!(
            load_round_duration_from(&p, TEST_ALLOWED),
            Some(RoundDurationCell { duration_sec: 900 })
        );
    }

    #[test]
    fn manual_ops_appends_and_counts_lines() {
        let d = tmpdir("manualops");
        let p = d.join("audit").join("manual-ops.jsonl");
        append_manual_op(&p, &serde_json::json!({ "op": "reset" })).unwrap();
        append_manual_op(&p, &serde_json::json!({ "op": "force_close" })).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.starts_with('{')));
        assert_eq!(count_jsonl_lines(&p), 2);
        // A missing file counts as 0, not an error.
        assert_eq!(count_jsonl_lines(&d.join("nope.jsonl")), 0);
    }

    #[test]
    fn the_market_plugin_cell_round_trips() {
        let d = tmpdir("mktrip");
        let p = d.join("config.json");
        persist_market_plugin_to(&p, Some("kalshi"), "uid:test", 1_000).unwrap();
        assert_eq!(
            load_market_plugin_from(&p),
            Some(MarketPluginCell {
                plugin: "kalshi".into()
            })
        );
    }

    #[test]
    fn the_two_cells_share_one_file_without_clobbering() {
        let d = tmpdir("twocells");
        let p = d.join("config.json");
        persist_round_duration_to(&p, Some(300), "uid:test", 1_000).unwrap();
        persist_market_plugin_to(&p, Some("predictfun"), "uid:test", 2_000).unwrap();
        // The later market-plugin write must NOT erase the round duration…
        assert_eq!(
            load_round_duration_from(&p, TEST_ALLOWED),
            Some(RoundDurationCell { duration_sec: 300 })
        );
        // …and a later round-duration reset must NOT erase the plugin.
        persist_round_duration_to(&p, None, "uid:test", 3_000).unwrap();
        assert_eq!(
            load_market_plugin_from(&p),
            Some(MarketPluginCell {
                plugin: "predictfun".into()
            })
        );
        // A plugin reset clears its own fields and leaves the other cell.
        persist_market_plugin_to(&p, None, "uid:test", 4_000).unwrap();
        assert_eq!(load_market_plugin_from(&p), None);
        assert_eq!(load_round_duration_from(&p, TEST_ALLOWED), None);
    }

    #[test]
    fn an_empty_or_whitespace_plugin_name_loads_as_none() {
        let d = tmpdir("blankplugin");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"marketPlugin": "   "}"#).unwrap();
        assert_eq!(load_market_plugin_from(&p), None);
    }

    #[test]
    fn a_missing_file_yields_no_plugin_cell() {
        let d = tmpdir("noplugin");
        assert_eq!(load_market_plugin_from(&d.join("config.json")), None);
    }

    #[test]
    fn legacy_credentials_cell_migrates_to_env_and_strips_the_key() {
        let d = tmpdir("credmigrate");
        let p = d.join("config.json");
        let env = d.join(".env");
        // The legacy shape: two plugins' cells written by the old kernel.
        // Key names are arbitrary to the migrator (it drains whatever the
        // cell holds); neutral names keep real credential spellings out of
        // the source.
        std::fs::write(
            &p,
            r#"{"credentials": {"kalshi": {"BK_TEST_KEY_A": "key-1",
               "BK_TEST_KEY_B": "begin-marker"},
               "polysimulator": {"BK_TEST_KEY_C": ""}}}"#,
        )
        .unwrap();
        let moved = migrate_credentials_cell_to_env(&p, &env).unwrap();
        assert_eq!(moved, 2, "the empty value is not a credential to move");
        let env_text = std::fs::read_to_string(&env).unwrap();
        assert!(env_text.contains("BK_TEST_KEY_A=\"key-1\""));
        assert!(env_text.contains("BK_TEST_KEY_B=\"begin-marker\""));
        assert!(
            !env_text.contains("BK_TEST_KEY_C"),
            "empty legacy values are noise, not credentials"
        );
        // The config key is stripped — the migration is one-shot.
        let stripped = std::fs::read_to_string(&p).unwrap();
        assert!(
            !stripped.contains("credentials"),
            "the drained key must not survive: {stripped}"
        );
        assert_eq!(migrate_credentials_cell_to_env(&p, &env).unwrap(), 0);
        // The sibling cells still work on the rewritten file.
        persist_round_duration_to(&p, Some(900), "uid:test", 2_000).unwrap();
        assert_eq!(
            load_round_duration_from(&p, TEST_ALLOWED),
            Some(RoundDurationCell { duration_sec: 900 })
        );
    }

    #[test]
    fn a_config_without_the_credentials_cell_migrates_zero() {
        let d = tmpdir("credzero");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"marketPlugin": "polymarket"}"#).unwrap();
        assert_eq!(
            migrate_credentials_cell_to_env(&p, &d.join(".env")).unwrap(),
            0
        );
        assert!(
            !d.join(".env").exists(),
            "zero moved must not create a .env"
        );
    }
}
