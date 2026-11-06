//! The `.env` write half of issue #966: hot risk/exit knobs persist to the
//! same single source the launcher loads at boot, so a panel edit survives
//! the restart the risk-state file never covered for `risk.setSystemic` and
//! that `risk.setExit` NEVER had.
//!
//! ONE write face (the issue's 「禁止双写」 clause): a successful hot write
//! upserts the frozen `RISK_*` / `EXIT_*` keys into `.env` and nothing here
//! ever writes `[risk]` / `[exit]` back to the TOML. The read side resolves
//! `CLI > env > TOML > default`, so the env key shadows a stale TOML value —
//! the same posture the ACCOUNT_* keys (issue 751) already established.
//!
//! The reply's `persisted` field reports the `.env` write (the risk-state
//! file record rides along unchanged where that kernel has one). A FAILED
//! env write is not a silent memory-only success: it logs at error (the live
//! values stand this session, a restart reverts them) and the reply reports
//! `false` — the operator asked for persistence; half-persistent is the lie
//! this issue forbids.

use std::collections::BTreeMap;
use std::path::Path;

/// The nine frozen systemic key names (`risk.setSystemic`). Frozen by issue
/// #966 — aliases are a second truth, never created.
pub const SYSTEMIC_ENV_KEYS: [&str; 9] = [
    "RISK_MAX_SINGLE_LOSS_USD",
    "RISK_MAX_DAILY_DRAWDOWN_USD",
    "RISK_MAX_POSITION_SIZE",
    "RISK_MAX_CONSECUTIVE_LOSSES",
    "RISK_COOLDOWN_MINUTES",
    "RISK_MAX_TOTAL_POSITION",
    "RISK_MAX_TOTAL_EXPOSURE_USD",
    "RISK_MAX_CORRELATION_USD",
    "RISK_GLOBAL_KILL_SWITCH_LOSS_USD",
];

/// The five frozen exit-discipline key names (`risk.setExit`).
pub const EXIT_ENV_KEYS: [&str; 5] = [
    "EXIT_STOP_LOSS_PCT",
    "EXIT_TAKE_PROFIT_PCT",
    "EXIT_FORCE_EXIT_SEC",
    "EXIT_TRAILING_ENABLED",
    "EXIT_STRATEGY_MAKER_FIRST",
];

/// The pre-#966 spelling some operator `.env` files still carry (`BK_RISK_*`
/// 九 rows hand-written before the frozen vocabulary existed). The boot's
/// migration moves those values onto the frozen keys; the read layer accepts
/// the legacy spelling as a one-window fallback so boot #1 still sees the
/// values the boot #1 pass wrote the file to carry.
const LEGACY_PREFIXES: [&str; 2] = ["BK_RISK_", "BK_EXIT_"];

/// The frozen key of one legacy `BK_RISK_*` / `BK_EXIT_*` key. `None` when
/// the key is not in the legacy families at all (`BK_*` of other
/// subsystems — BK_CONFIG, BK_NTP_HOST — are not legal inputs here).
pub fn frozen_key_of(legacy: &str) -> Option<String> {
    for p in LEGACY_PREFIXES {
        if let Some(rest) = legacy.strip_prefix(p) {
            let new_prefix = match p {
                "BK_RISK_" => "RISK_",
                _ => "EXIT_",
            };
            return Some(format!("{new_prefix}{rest}"));
        }
    }
    None
}

/// Upsert the CHANGED bounds into the kernel's `.env` (the `env_file_path`
/// the launcher configured — the SAME file the boot env layer reads back).
/// Returns the key names actually written; `Ok(vec![])` when persistence is
/// off (backtester/tests carry `env_file_path: None` — the honest no-claim
/// the hot-update contract has always reported there).
pub(crate) fn persist_systemic_to_env(
    env_path: Option<&Path>,
    changes: &[crate::schema::SystemicLimitChange],
) -> std::io::Result<Vec<String>> {
    let Some(path) = env_path else {
        return Ok(Vec::new());
    };
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    // The reply's wire field name → frozen env key. Both lists mirror the
    // issue's frozen contract field-for-field; the pairing is one local
    // table, commented to the order it follows.
    const WIRE_TO_ENV: [(&str, &str); 9] = [
        ("maxSingleLossUsd", SYSTEMIC_ENV_KEYS[0]),
        ("maxDailyDrawdownUsd", SYSTEMIC_ENV_KEYS[1]),
        ("maxPositionSize", SYSTEMIC_ENV_KEYS[2]),
        ("maxConsecutiveLosses", SYSTEMIC_ENV_KEYS[3]),
        ("cooldownMinutes", SYSTEMIC_ENV_KEYS[4]),
        ("maxTotalPosition", SYSTEMIC_ENV_KEYS[5]),
        ("maxTotalExposureUsd", SYSTEMIC_ENV_KEYS[6]),
        ("maxCorrelationUsd", SYSTEMIC_ENV_KEYS[7]),
        ("globalKillSwitchLossUsd", SYSTEMIC_ENV_KEYS[8]),
    ];
    let mut vars = BTreeMap::new();
    for (wire, env) in WIRE_TO_ENV {
        if let Some(c) = changes.iter().find(|c| c.field == wire) {
            vars.insert(env.to_string(), c.to.clone());
        }
    }
    crate::env_file::upsert_env_vars(path, &vars)
}

/// Same face for `risk.setExit` — the five frozen keys, decimal-string /
/// `bool` verbatim (the one value vocabulary across the reply and the file).
pub(crate) fn persist_exit_to_env(
    env_path: Option<&Path>,
    changes: &[crate::schema::ExitSetChange],
) -> std::io::Result<Vec<String>> {
    let Some(path) = env_path else {
        return Ok(Vec::new());
    };
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    const WIRE_TO_ENV: [(&str, &str); 5] = [
        ("stopLossPct", EXIT_ENV_KEYS[0]),
        ("takeProfitPct", EXIT_ENV_KEYS[1]),
        ("forceExitSec", EXIT_ENV_KEYS[2]),
        ("trailingEnabled", EXIT_ENV_KEYS[3]),
        ("strategyExitMakerFirst", EXIT_ENV_KEYS[4]),
    ];
    let mut vars = BTreeMap::new();
    for (wire, env) in WIRE_TO_ENV {
        if let Some(c) = changes.iter().find(|c| c.field == wire) {
            vars.insert(env.to_string(), c.to.clone());
        }
    }
    crate::env_file::upsert_env_vars(path, &vars)
}

/// The two writers' reply half, one line each at the apply_* success tail:
/// the env write decides `persisted` (with the risk-state record folded in
/// for the systemic, whose durable cell predates this issue). A FAILED env
/// write downgrades `persisted` to `false` — the operator-visible lie the
/// issue forbids is a reply that claims persistence the file never recorded.
/// An env-CONFIGURED kernel with empty changes still claims the state-file's
/// verdict (nothing to write; the caps recorded there are the truth).
pub(crate) fn systemic_env_persisted(
    env_path: Option<&Path>,
    changes: &[crate::schema::SystemicLimitChange],
    state_file_persisted: bool,
) -> bool {
    if changes.is_empty() {
        return state_file_persisted;
    }
    match persist_systemic_to_env(env_path, changes) {
        // An env-configured kernel with changes to write has actually
        // persisted; a pathless kernel (tests/backtester) keeps the
        // risk-state file's honest verdict unchanged.
        Ok(written) => !written.is_empty() || state_file_persisted,
        Err(e) => {
            tracing::error!(
                target: "risk",
                error = %e,
                "systemic limit update: .env persistence FAILED — the values are live \
                 this session but a restart reverts them; check the .env write face"
            );
            false
        }
    }
}

pub(crate) fn exit_env_persisted(
    env_path: Option<&Path>,
    changes: &[crate::schema::ExitSetChange],
) -> bool {
    if changes.is_empty() {
        return false;
    }
    match persist_exit_to_env(env_path, changes) {
        Ok(written) => !written.is_empty(),
        Err(e) => {
            tracing::error!(
                target: "risk",
                error = %e,
                "exit discipline update: .env persistence FAILED — the values are live \
                 this session but a restart reverts them; check the .env write face"
            );
            false
        }
    }
}

/// The disk half of the legacy-key migration, called ONCE from the
/// launcher's boot: every `BK_RISK_*` / `BK_EXIT_*` line in the operator
/// `.env` whose frozen spelling is NOT already on the file moves onto the
/// frozen key (upsert) and the legacy row is DELETED — the frozen vocabulary
/// is the only one the file carries after the pass. Returns the (frozen key,
/// value) moves that happened — the boot log's rename evidence. No-op
/// (empty vector, no error) when the file is absent or already clean.
pub fn migrate_legacy_keys(path: &Path) -> std::io::Result<Vec<(String, String)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let parsed: Vec<(String, String)> = text
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    let frozen_on_file: std::collections::HashSet<String> = parsed
        .iter()
        .filter(|(k, _)| k.starts_with("RISK_") || k.starts_with("EXIT_"))
        .map(|(k, _)| k.clone())
        .collect();
    let mut moves = BTreeMap::new();
    let mut legacy_rows: Vec<String> = Vec::new();
    for (k, v) in &parsed {
        if let Some(frozen) = frozen_key_of(k) {
            legacy_rows.push(k.clone());
            // The frozen spelling is the operator's edit — a legacy row that
            // would move onto an already-present frozen key is dropped, not
            // moved (the operator's value wins).
            if !frozen_on_file.contains(&frozen) {
                moves.insert(frozen, v.clone());
            }
        }
    }
    // Nothing to do at all: no legacy rows, no moves.
    if moves.is_empty() && legacy_rows.is_empty() {
        return Ok(Vec::new());
    }
    if !moves.is_empty() {
        crate::env_file::upsert_env_vars(path, &moves)?;
    }
    if !legacy_rows.is_empty() {
        let refs: Vec<&str> = legacy_rows.iter().map(String::as_str).collect();
        crate::env_file::remove_env_vars(path, &refs)?;
    }
    Ok(moves.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ExitSetChange, SystemicLimitChange};
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-env-persist-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir.join(".env")
    }

    fn sys_change(field: &'static str, to: &str) -> SystemicLimitChange {
        SystemicLimitChange {
            field,
            from: "0".to_string(),
            to: to.to_string(),
            effect: "live",
        }
    }

    fn exit_change(field: &'static str, to: &str) -> ExitSetChange {
        ExitSetChange {
            field,
            from: "0".to_string(),
            to: to.to_string(),
            effect: "live",
        }
    }

    #[test]
    fn frozen_key_contract_names_the_issue_keys() {
        // The frozen spelling is the issue's terrain: NOT a BK_ alias.
        assert!(
            SYSTEMIC_ENV_KEYS.iter().all(|k| !k.starts_with("BK_")),
            "keys must be the plain RISK_* spelling"
        );
        assert_eq!(SYSTEMIC_ENV_KEYS[0], "RISK_MAX_SINGLE_LOSS_USD");
        assert_eq!(SYSTEMIC_ENV_KEYS[8], "RISK_GLOBAL_KILL_SWITCH_LOSS_USD");
        assert_eq!(EXIT_ENV_KEYS[0], "EXIT_STOP_LOSS_PCT");
        assert_eq!(EXIT_ENV_KEYS[4], "EXIT_STRATEGY_MAKER_FIRST");
    }

    #[test]
    fn systemic_persist_upserts_only_changed_keys() {
        let path = tmp_path("systemic");
        std::fs::write(
            &path,
            "ACCOUNT_DEFAULT_VENUE=predict\nRISK_MAX_TOTAL_POSITION=\"3\"\n",
        )
        .unwrap();
        let changes = vec![
            sys_change("maxSingleLossUsd", "0.50"),
            sys_change("maxTotalPosition", "6"),
        ];
        let written = persist_systemic_to_env(Some(&path), &changes).expect("upsert");
        assert_eq!(written.len(), 2, "{written:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        // In place: the pre-existing RISK_MAX_TOTAL_POSITION line is
        // REWRITTEN; every other line survives byte-for-byte (preserved lines
        // keep their original, unquoted spelling).
        assert!(text.contains("ACCOUNT_DEFAULT_VENUE=predict"), "{text}");
        assert!(text.contains("RISK_MAX_SINGLE_LOSS_USD=\"0.50\""), "{text}");
        assert!(text.contains("RISK_MAX_TOTAL_POSITION=\"6\""), "{text}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn exit_persist_carries_bools_verbatim() {
        let path = tmp_path("exit");
        let changes = vec![
            exit_change("stopLossPct", "8"),
            exit_change("trailingEnabled", "true"),
        ];
        persist_exit_to_env(Some(&path), &changes).expect("upsert");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("EXIT_STOP_LOSS_PCT=\"8\""), "{text}");
        assert!(text.contains("EXIT_TRAILING_ENABLED=\"true\""), "{text}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn persistence_off_is_an_honest_nothing() {
        let changes = vec![sys_change("maxSingleLossUsd", "0.50")];
        let written = persist_systemic_to_env(None, &changes).expect("no env configured is an Ok");
        assert!(written.is_empty(), "no path, nothing claimed written");
    }

    #[test]
    fn empty_change_list_never_touches_the_file() {
        let path = tmp_path("empty");
        persist_systemic_to_env(Some(&path), &[]).expect("empty list is an Ok");
        persist_exit_to_env(Some(&path), &[]).expect("empty list is an Ok");
        assert!(!path.exists(), "a no-op write must not create the file");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn persisted_verdict_reflects_the_env_write() {
        let path = tmp_path("verdict");
        let changes = vec![sys_change("maxSingleLossUsd", "0.50")];
        // Pathless core: nothing claimed persisted (memory-only, as before).
        assert!(!systemic_env_persisted(None, &changes, false));
        // The risk-state record rides along; the env write decides.
        assert!(systemic_env_persisted(Some(&path), &changes, true));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn frozen_key_of_maps_only_the_legacy_families() {
        assert_eq!(
            frozen_key_of("BK_RISK_MAX_SINGLE_LOSS_USD").as_deref(),
            Some("RISK_MAX_SINGLE_LOSS_USD")
        );
        assert_eq!(
            frozen_key_of("BK_EXIT_STRATEGY_MAKER_FIRST").as_deref(),
            Some("EXIT_STRATEGY_MAKER_FIRST")
        );
        assert_eq!(frozen_key_of("BK_CONFIG"), None);
        assert_eq!(frozen_key_of("RISK_MAX_TOTAL_POSITION"), None);
    }

    #[test]
    fn migration_moves_legacy_values_and_deletes_the_legacy_rows() {
        let path = tmp_path("migrate");
        std::fs::write(
            &path,
            "# operator seed\nBK_RISK_MAX_SINGLE_LOSS_USD=0.50\nBK_RISK_MAX_TOTAL_POSITION=6\n",
        )
        .unwrap();
        let moves = migrate_legacy_keys(&path).expect("migrate");
        assert_eq!(
            moves.len(),
            2,
            "both legacy rows move onto frozen keys: {moves:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        // The frozen spellings are ON the file, the legacy rows are GONE —
        // one vocabulary only — and the operator's comment survived.
        assert!(text.contains("RISK_MAX_SINGLE_LOSS_USD=\"0.50\""), "{text}");
        assert!(text.contains("RISK_MAX_TOTAL_POSITION=\"6\""), "{text}");
        assert!(!text.contains("BK_RISK_"), "{text}");
        assert!(text.contains("# operator seed"), "{text}");
        // Idempotent: a second pass has nothing to move.
        assert!(
            migrate_legacy_keys(&path)
                .expect("migrate again")
                .is_empty(),
            "the frozen file is already clean"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn migration_yields_to_a_frozen_key_already_on_the_file() {
        let path = tmp_path("migrate-keep");
        std::fs::write(
            &path,
            "BK_RISK_MAX_SINGLE_LOSS_USD=0.50\nRISK_MAX_SINGLE_LOSS_USD=9.00\n",
        )
        .unwrap();
        let moves = migrate_legacy_keys(&path).expect("migrate");
        assert!(
            moves.iter().all(|(k, _)| k != "RISK_MAX_SINGLE_LOSS_USD"),
            "the frozen spelling is the operator's edit — no legacy move over it: {moves:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("RISK_MAX_SINGLE_LOSS_USD=9.00"), "{text}");
        assert!(
            !text.contains("BK_RISK_"),
            "the orphaned legacy row still goes: {text}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn migration_of_an_absent_file_is_a_silent_no_op() {
        let path = tmp_path("migrate-absent");
        assert!(migrate_legacy_keys(&path).expect("absent").is_empty());
        assert!(!path.exists(), "nothing was created");
    }
}
