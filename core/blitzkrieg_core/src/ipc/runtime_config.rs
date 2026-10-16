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

/// The switchable whitelist. ONE decoder lives in
/// [`crate::scanner::duration_label_for`]; this list names exactly the values
/// that decoder labels, so the UI can never offer a duration the kernel
/// cannot print (and a new label there must be mirrored here — the compile
/// does not force it, the `round_duration` gate does).
pub const ALLOWED_ROUND_DURATIONS: &[i64] = &[300, 900, 3600, 14400, 86400];

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
    /// Per-plugin credential overrides the WebUI writes through
    /// `market.credentials.set` (`plugin` → `env var name` → value). Applied
    /// at boot BEFORE the executor reads the environment; a real environment
    /// variable always outranks the cell (same posture as .env). Values live
    /// in the user's data directory and never travel back over IPC — the
    /// status readout reports presence only.
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials:
        Option<std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>>,
}

/// What the boot resolution needs to know about the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundDurationCell {
    pub duration_sec: i64,
}

/// Read the persisted round duration, if the cell holds one. A missing or
/// broken file is the normal first-boot case → `None`, no drama (the same
/// posture as `version::load_from`). An out-of-whitelist value is IGNORED
/// with a warning — a hand-edited cell must not be able to arm a duration
/// the kernel cannot label.
pub fn load_round_duration() -> Option<RoundDurationCell> {
    load_round_duration_from(&state_path())
}

pub fn load_round_duration_from(path: &std::path::Path) -> Option<RoundDurationCell> {
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
    if !ALLOWED_ROUND_DURATIONS.contains(&sec) {
        tracing::warn!(
            "{} holds roundDurationSec={sec}, which is not on the whitelist \
             {ALLOWED_ROUND_DURATIONS:?} — the cell is ignored",
            path.display()
        );
        return None;
    }
    Some(RoundDurationCell { duration_sec: sec })
}

/// The market-plugin cell the boot resolution reads back. Unlike the
/// round-duration whitelist, the valid names belong to the compiled-in
/// registry — which the config layer cannot see — so the caller (boot, in
/// `main.rs`) validates the name against `MarketPluginRegistry::names()`
/// itself; this reader only reports what the cell holds.
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

/// Read the persisted credential cell. `plugin=None` returns every plugin's
/// map; a plugin name narrows it to that one entry.
pub fn load_credentials_from(
    path: &std::path::Path,
    plugin: Option<&str>,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Default::default(), // first-boot normal case
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Default::default(),
    };
    let Some(all) = v.get("credentials").and_then(|c| c.as_object()).map(|o| {
        o.iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    m.as_object()
                        .map(|vars| {
                            vars.iter()
                                .filter_map(|(vk, vv)| {
                                    vv.as_str().map(|s| (vk.clone(), s.to_string()))
                                })
                                .collect::<std::collections::BTreeMap<_, _>>()
                        })
                        .unwrap_or_default(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    }) else {
        return Default::default();
    };
    match plugin {
        Some(p) => all.into_iter().filter(|(k, _)| k == p).collect(),
        None => all,
    }
}

/// Write (or, with `vars=None`, clear) one plugin's credential overrides in
/// the shared cell. Atomic read-modify-write: the other cells share this file
/// and must survive. Values are operator input for the NEXT session's
/// environment; the caller decides whether any name is acceptable (the IPC
/// layer validates against the plugin table).
pub fn persist_credentials_to(
    path: &std::path::Path,
    plugin: &str,
    vars: Option<&std::collections::BTreeMap<String, String>>,
) -> std::io::Result<()> {
    let mut doc: RuntimeConfigDoc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut all = doc.credentials.take().unwrap_or_default();
    match vars {
        Some(vars) => {
            if vars.is_empty() {
                all.remove(plugin);
            } else {
                all.insert(plugin.to_string(), vars.clone());
            }
        }
        None => {
            all.remove(plugin);
        }
    }
    doc.credentials = if all.is_empty() { None } else { Some(all) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    // Owner-only: the file now carries credential values. On the TEMP file
    // before the rename — the final path may not exist yet, and a chmod on a
    // missing file would silently leave the first write world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
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

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bk-runtime-cfg-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_missing_file_loads_as_none() {
        let d = tmpdir("missing");
        assert_eq!(load_round_duration_from(&d.join("state.json")), None);
    }

    #[test]
    fn a_written_round_trips_through_load() {
        let d = tmpdir("roundtrip");
        let p = d.join("config.json");
        persist_round_duration_to(&p, Some(300), "uid:test", 1_000).unwrap();
        assert_eq!(
            load_round_duration_from(&p),
            Some(RoundDurationCell { duration_sec: 300 })
        );
    }

    #[test]
    fn reset_clears_the_cell_and_load_falls_through() {
        let d = tmpdir("reset");
        let p = d.join("config.json");
        persist_round_duration_to(&p, Some(300), "uid:test", 1_000).unwrap();
        persist_round_duration_to(&p, None, "uid:test", 2_000).unwrap();
        assert_eq!(load_round_duration_from(&p), None);
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
        assert_eq!(load_round_duration_from(&p), None);
    }

    #[test]
    fn a_broken_file_is_overwritten_by_the_next_write() {
        let d = tmpdir("broken");
        let p = d.join("config.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_round_duration_from(&p), None);
        persist_round_duration_to(&p, Some(900), "uid:test", 3_000).unwrap();
        assert_eq!(
            load_round_duration_from(&p),
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
            load_round_duration_from(&p),
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
        assert_eq!(load_round_duration_from(&p), None);
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
    fn credentials_round_trip_and_survive_sibling_cell_writes() {
        let d = tmpdir("credtrip");
        let p = d.join("config.json");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("KALSHI_API_KEY_ID".to_string(), "key-1".to_string());
        vars.insert(
            "KALSHI_RSA_PRIVATE_KEY".to_string(),
            "-----BEGIN".to_string(),
        );
        persist_credentials_to(&p, "kalshi", Some(&vars)).unwrap();
        // 600 is off the round-duration whitelist; arm a legal one.
        persist_round_duration_to(&p, Some(900), "uid:test", 2_000).unwrap();
        // The sibling cell write must not clobber the credentials…
        let got = load_credentials_from(&p, Some("kalshi"));
        assert_eq!(got["kalshi"]["KALSHI_API_KEY_ID"], "key-1");
        assert_eq!(got["kalshi"]["KALSHI_RSA_PRIVATE_KEY"], "-----BEGIN");
        assert_eq!(
            load_round_duration_from(&p),
            Some(RoundDurationCell { duration_sec: 900 })
        );
        // …and a round-duration write must not clobber the credentials.
        persist_round_duration_to(&p, Some(300), "uid:test", 3_000).unwrap();
        assert_eq!(load_credentials_from(&p, None)["kalshi"].len(), 2);
    }

    #[test]
    fn credentials_reset_removes_only_that_plugin() {
        let d = tmpdir("credreset");
        let p = d.join("config.json");
        let mut a = std::collections::BTreeMap::new();
        a.insert("KALSHI_API_KEY_ID".to_string(), "k".to_string());
        persist_credentials_to(&p, "kalshi", Some(&a)).unwrap();
        let mut b = std::collections::BTreeMap::new();
        b.insert("POLYSIM_API_KEY".to_string(), "s".to_string());
        persist_credentials_to(&p, "polysimulator", Some(&b)).unwrap();
        persist_credentials_to(&p, "kalshi", None).unwrap(); // reset arm
        let got = load_credentials_from(&p, None);
        assert!(!got.contains_key("kalshi"));
        assert_eq!(got["polysimulator"]["POLYSIM_API_KEY"], "s");
        // Clearing the last plugin drops the key entirely.
        persist_credentials_to(&p, "polysimulator", None).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("credentials"),
            "empty map must not arm the key: {text}"
        );
    }

    #[test]
    fn credentials_file_is_owner_only_after_write() {
        let d = tmpdir("credperm");
        let p = d.join("config.json");
        let mut v = std::collections::BTreeMap::new();
        v.insert("POLYSIM_API_KEY".to_string(), "secret".to_string());
        persist_credentials_to(&p, "polysimulator", Some(&v)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credential values demand owner-only");
        }
        #[cfg(not(unix))]
        let _ = p;
    }
}
