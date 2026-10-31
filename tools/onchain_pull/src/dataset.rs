use blitzkrieg_data_model::CashflowKind;
use rust_decimal::Decimal;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What one condition's redemption activity proves about its outcome
/// (治 #2). Two kinds of evidence, both taken from the wallet's own REDEEM
/// rows: a WINNING redemption prints the cash received against the winner's
/// token (`usdcSize > 0`), a worthless one burns the loser's shares
/// (`usdcSize == 0` against a token, no cash). A row that carries no asset
/// still carries the outcome index (0 = Up, 1 = Down — gamma's
/// `clobTokenIds` order), which the converter can map onto the market's
/// declared tokens.
#[derive(Debug, Default, Clone)]
pub(crate) struct ConditionVerdict {
    winner_asset: Option<String>,
    winner_index: Option<i64>,
    loser_asset: Option<String>,
    loser_index: Option<i64>,
}

/// Numeric field that tolerates both quoted and bare JSON numbers — the
/// activity export has used both shapes across versions.
fn row_f64(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Fold one REDEEM row into the verdict map. First evidence wins per slot:
/// retried redemptions print one row per attempt and never disagree (they
/// redeem the same outcome), so the fold is order-stable for a given ledger.
pub(crate) fn fold_redemption(out: &mut BTreeMap<String, ConditionVerdict>, v: &Value) {
    let Some(cid) = v.get("conditionId").and_then(Value::as_str) else {
        return;
    };
    if cid.is_empty() {
        return;
    }
    let asset = v
        .get("asset")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let index = v
        .get("outcomeIndex")
        .and_then(Value::as_i64)
        .filter(|i| (0..=1).contains(i));
    let e = out.entry(cid.to_string()).or_default();
    if row_f64(v, "usdcSize") > 0.0 {
        if e.winner_asset.is_none() {
            e.winner_asset = asset;
        }
        if e.winner_index.is_none() {
            e.winner_index = index;
        }
    } else if row_f64(v, "size") > 0.0 {
        if e.loser_asset.is_none() {
            e.loser_asset = asset;
        }
        if e.loser_index.is_none() {
            e.loser_index = index;
        }
    }
}

/// Read the redemption side file back (empty when absent — a `/trades` walk
/// carries no REDEEM rows, so its datasets convert verdict-less and the dry
/// ladder keeps doing the settling, exactly as before).
pub(crate) fn load_redemptions(path: &Path) -> Result<BTreeMap<String, ConditionVerdict>, String> {
    let mut out: BTreeMap<String, ConditionVerdict> = BTreeMap::new();
    if !path.exists() {
        return Ok(out);
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            fold_redemption(&mut out, &v);
        }
    }
    Ok(out)
}

/// The winning token per the redemption evidence, or `None` when the rows
/// prove nothing usable against THIS market's declared tokens (stale gamma
/// tokens, no redeem rows at all). Winner evidence outranks loser evidence;
/// an outcome index substitutes when the row carries no asset; a proven
/// loser names the other side as the winner.
pub(crate) fn resolve_winner(vd: &ConditionVerdict, up: &str, down: &str) -> Option<String> {
    let known = |a: &str| a == up || a == down;
    if let Some(a) = vd.winner_asset.as_deref().filter(|a| known(a)) {
        return Some(a.to_string());
    }
    match vd.winner_index {
        Some(0) => return Some(up.to_string()),
        Some(1) => return Some(down.to_string()),
        _ => {}
    }
    if let Some(a) = vd.loser_asset.as_deref().filter(|a| known(a)) {
        return Some((if a == up { down } else { up }).to_string());
    }
    match vd.loser_index {
        Some(0) => return Some(down.to_string()),
        Some(1) => return Some(up.to_string()),
        _ => {}
    }
    None
}

/// #388: fold one MAKER_REBATE / REWARD row into the cash list — one event
/// per LEDGER ROW, at the row's own timestamp. The amount is `usdcSize` via
/// the exact decimal path (quoted and bare numbers both seen in the export);
/// a row whose amount cannot be parsed exactly is refused, never zeroed —
/// a silently-dropped rebate row would corrupt the wallet's cash view by
/// exactly the money it earned. The timestamp comes through as seconds
/// (epoch); a zero/negative one is a torn row, refused like any other.
pub(crate) fn fold_cashflow(out: &mut Vec<TimedCashflow>, v: &Value) -> Result<(), String> {
    let which = match v.get("type").and_then(Value::as_str) {
        Some("MAKER_REBATE") => CashflowKind::Rebate,
        Some("REWARD") => CashflowKind::Reward,
        other => return Err(format!("not a cash row: {other:?}")),
    };
    let ts = v.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    if ts <= 0 {
        return Err(format!("cash row with no usable timestamp: {v}"));
    }
    let raw = match v.get("usdcSize") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => return Err(format!("cash row with no usdcSize: {v}")),
    };
    let amount = Decimal::from_str_exact(&raw)
        .map_err(|e| format!("cash row usdcSize `{raw}` is not exact: {e}"))?;
    out.push(TimedCashflow {
        at_ms: ts * 1000,
        which,
        amount_usd: amount,
    });
    Ok(())
}

/// Read the cash side file back (empty when absent — a `/trades` walk
/// carries no cash rows, so its datasets convert cashflow-less and the
/// replay stays exactly the pre-#388 stream). Malformed rows fail the pull
/// (fail-closed: a dataset whose cash was silently dropped would misstate
/// the wallet's balance) — unlike the verdict fold, where missing evidence
/// merely leaves the dry ladder in charge.
pub(crate) fn load_cashflows(path: &Path) -> Result<Vec<TimedCashflow>, String> {
    let mut out: Vec<TimedCashflow> = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => fold_cashflow(&mut out, &v)?,
            Err(e) => return Err(format!("cash row is not valid JSON: {e}")),
        }
    }
    Ok(out)
}

// ── sha256 ──────────────────────────────────────────────────────────────────

/// Hex SHA256 of a file's bytes. The manifest carries one per dataset; a
/// file that no longer hashes to its pin is corruption, detected on read
/// (fail-closed, the same contract as the frozen corpus).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let mut out = String::new();
    for b in h.finalize() {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Verify a dataset against its manifest. A manifest without sha256 entries
/// is itself a refusal — the digest is the corruption detector, and a pull
/// that skipped it would be an unverifiable dataset.
pub fn verify_dataset(manifest: &Path) -> Result<(), String> {
    let v: Value = read_json(manifest)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let pins = [
        ("trades", trades_file_from_manifest(&v, manifest)),
        ("events", events_file_from_manifest(&v, manifest)),
    ];
    for (name, path) in pins {
        let path = path?;
        let pin = v
            .pointer(&format!("/sha256/{name}"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "manifest {} has no sha256 for {name}: an un-hashed dataset is not a dataset",
                    manifest.display()
                )
            })?;
        let actual = sha256_file(&path).map_err(|e| format!("{name}: {e}"))?;
        if actual != pin {
            return Err(format!(
                "{name} sha256 mismatch: file {actual} != pinned {pin} — the dataset at {} is corrupted",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn trades_file_from_manifest(v: &Value, manifest: &Path) -> Result<PathBuf, String> {
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let name = v
        .pointer("/files/trades")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest missing files.trades".to_string())?;
    Ok(dir.join(name))
}
fn events_file_from_manifest(v: &Value, manifest: &Path) -> Result<PathBuf, String> {
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let name = v
        .pointer("/files/events")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest missing files.events".to_string())?;
    Ok(dir.join(name))
}

pub(crate) fn read_json(path: &Path) -> Result<Value, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Atomic-ish JSON write: temp file + rename, so a crash never leaves a
/// half-written manifest (the resume state is the one thing that must not
/// corrupt).
///
/// The tmp name is unique per writer (pid + in-process counter): two
/// concurrent pulls sharing a cache dir would otherwise rename the same
/// `X.tmp` out from under each other and one dies with ENOENT. A crashed
/// run may leave an orphaned tmp behind — harmless garbage, the next
/// successful write replaces the canonical file.
pub(crate) fn write_json_atomic(path: &Path, v: &Value) -> Result<(), String> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, s).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

pub(crate) fn append_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("append {}: {e}", path.display()))
}

/// #388: one wallet-level cash row ready for emission — the ledger row's
/// own timestamp, its kind, and its exact amount.
#[derive(Debug, Clone)]
pub(crate) struct TimedCashflow {
    pub(crate) at_ms: i64,
    pub(crate) which: CashflowKind,
    pub(crate) amount_usd: Decimal,
}
