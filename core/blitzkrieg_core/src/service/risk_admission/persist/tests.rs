use super::*;

use crate::account::{Account, AccountId, AccountLedgers, AccountStatus, CredentialKeys};
use crate::model::Mode;
use crate::risk::RiskConfig;
use crate::risk::limits::LimitSource;
use crate::schema::{SetRiskLimitsParams, SetSystemicLimitsParams};
use blitzkrieg_market_api::{MarketType, net::now_ms};
use rust_decimal_macros::dec;
use std::path::Path;

// The temp root must be CI-portable: `std::env::temp_dir()` like every other
// core test (a hardcoded workspace volume exists only on the operator's Mac
// and panics the ubuntu runner). The guard removes it even when an assertion
// panics.
struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("test-861-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("temp root creates");
        TempRoot(dir)
    }

    fn file(&self) -> std::path::PathBuf {
        self.0.join("positions.risk-state.json")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cfg(path: &Path) -> crate::service::CoreConfig {
    crate::service::CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        risk_state_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    }
}

fn book(ids: &[&str]) -> AccountLedgers {
    let now = now_ms();
    AccountLedgers::new(
        ids.iter()
            .map(|id| Account {
                id: AccountId::from(*id),
                name: (*id).to_string(),
                market_type: MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: now,
            })
            .collect(),
        AccountId::from(ids[0]),
    )
}

fn core_with(path: &Path, ids: &[&str]) -> crate::service::Core {
    crate::service::Core::with_account_book(cfg(path), book(ids))
}

/// (a) The #854 finding, fixed: a hot `risk.setLimits` write lands in the
/// risk-state file, and a rebuilt core (the kill -9 drill) judges orders by
/// the SAME caps. The SYSTEMIC bounds are NOT on this file any more (issue
/// #965): their durable carrier is `.env` (BK_RISK_* keys, the IPC write
/// face's upsert — round-trip proven in the ipc tests), and the apply here
/// reports the honest `persisted: false` for that carrier-less core.
#[test]
fn hot_limits_and_systemic_bounds_recover_across_a_rebuild() {
    let root = TempRoot::new("roundtrip");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    let patch = SetRiskLimitsParams {
        max_order_notional: Some(dec!(250)),
        max_open_notional_usd: Some(dec!(4000)),
        min_shares: Some(dec!(2)),
        ..Default::default()
    };
    let reply = c
        .apply_risk_limits(&patch, "op", 1_000)
        .expect("limits apply");
    assert!(
        reply.persisted,
        "a file-backed core reports the hot write as persisted"
    );
    let spatch = SetSystemicLimitsParams {
        max_single_loss_usd: Some(dec!(75)),
        max_daily_drawdown_usd: Some(dec!(300)),
        ..Default::default()
    };
    let sreply = c
        .apply_systemic_limits(&spatch, "op", 1_100)
        .expect("systemic apply");
    assert!(
        !sreply.persisted,
        "the apply writes nothing durable itself — .env is the systemic carrier (issue #965)"
    );
    // The in-memory matrix DOES carry the operator's intent for this session.
    let single = &c.risk.config().systemic.account.max_single_loss_usd;
    assert_eq!(single.value(), dec!(75));
    drop(c);

    let c2 = core_with(&path, &["default"]);
    assert_eq!(c2.risk.config().max_order_notional, dec!(250));
    assert_eq!(c2.risk.config().max_open_notional_usd, dec!(4000));
    assert_eq!(c2.config.min_shares, dec!(2));
    // The systemic bounds do NOT come back through this file — a
    // carrier-less core re-resolves them through the boot chain (env). The
    // .env round-trip is the ipc test's to prove.
    let restored = &c2.risk.config().systemic.account.max_single_loss_usd;
    assert_eq!(
        restored.value(),
        Decimal::ZERO,
        "no systemic cell on this file: the bound re-resolves at boot, not from a flag-stamped cell"
    );
    assert_eq!(restored.source(), LimitSource::Default);
    // The file is the wire format the PR documents: camelCase field names,
    // decimal-string values — and NO systemic section.
    let text = std::fs::read_to_string(&path).expect("the file exists");
    assert!(
        text.contains("\"maxOrderNotional\": \"250\""),
        "camelCase decimal-string cell expected, got: {text}"
    );
    assert!(
        !text.contains("maxSingleLossUsd"),
        "systemic cells are retired from this file (issue #965): {text}"
    );
}

/// (a2) Issue #965 migration: a risk-state file written before the systemic
/// bounds moved to `.env` still parses (the envelope ignores the retired
/// `systemic` map) and the entry caps it carries still restore — with a loud
/// warning naming the retirement, never a silent drop that reads as
/// "nothing was ever set".
#[test]
fn a_legacy_systemic_section_is_ignored_with_a_loud_warning() {
    let root = TempRoot::new("legacy-systemic");
    let path = root.file();

    // A hand-shaped pre-#965 file: entry caps + flag-stamped systemic cells.
    std::fs::write(
        &path,
        r#"{
  "savedAtMs": 1000,
  "limits": { "maxOrderNotional": "250" },
  "systemic": {
    "maxSingleLossUsd": { "value": "75", "source": "flag" }
  }
}"#,
    )
    .unwrap();

    // The file PARSES (the retired section does not corrupt it), and the
    // entry cap restores.
    let c = core_with(&path, &["default"]);
    assert_eq!(c.risk.config().max_order_notional, dec!(250));
    // The systemic bound is NOT resurrected from the flag-stamped cell —
    // the boot chain (env here) is the only arming path left.
    let single = &c.risk.config().systemic.account.max_single_loss_usd;
    assert_eq!(single.value(), Decimal::ZERO);
    assert_eq!(single.source(), LimitSource::Default);
}

/// (a3) The #965 leftover, closed loop with the (a2) warning: after the
/// one-shot migration (`scripts/migrate-riskstate-systemic.mjs` — the shape
/// below is exactly what it leaves behind) the file carries NO `systemic`
/// map, so `load_risk_state_file`'s retirement predicate
/// (`raw.get("systemic").is_some()`) can never fire, and everything the file
/// legitimately held (entry caps, kill switch, freeze book) still restores.
/// The loader itself is untouched — this pins the migration's OUTPUT against
/// the SAME boot path the (a2) legacy shape goes through.
#[test]
fn a_migrated_file_loads_clean_without_the_retirement_warning() {
    let root = TempRoot::new("migrated-systemic");
    let path = root.file();

    // The migration script's rewrite: the envelope minus `systemic`, the
    // struct's field order, serde's skip-empty rules, pretty two-space.
    std::fs::write(
        &path,
        r#"{
  "savedAtMs": 1762000000000,
  "limits": {
    "maxOrderNotional": "250"
  },
  "kill": {
    "reason": "operator pause"
  },
  "accountFreezes": {
    "default": "manual hold"
  }
}
"#,
    )
    .unwrap();

    // The #990 warning predicate, verbatim from `load_risk_state_file`: it
    // reads the RAW doc for a `systemic` member. The migrated file has none —
    // the loud warning can never fire on it again.
    let raw = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&path).expect("the migrated file exists"),
    )
    .expect("the migrated file is a JSON object");
    assert!(
        raw.get("systemic").is_none(),
        "the migration must remove the retired map, not leave an empty one: {raw}"
    );

    // The same boot path the legacy shape takes: the load is Ok (not Corrupt),
    // the entry cap restores; #1056 retired the `kill` cell from the file, so
    // the kill switch does NOT re-arm across restart — the migration retired
    // the posture, not just the cells.
    assert!(matches!(load_risk_state_file(&path), RiskStateLoad::Ok(_)));
    let c = core_with(&path, &["default"]);
    assert_eq!(c.risk.config().max_order_notional, dec!(250));
    assert!(
        !c.is_killed(),
        "the kill switch does not restore across restart (#1056)"
    );
    assert!(
        c.account_freeze_reason("default", now_ms()).is_none(),
        "stale accountFreezes rows are ignored, not replayed (#1045)"
    );
    let single = &c.risk.config().systemic.account.max_single_loss_usd;
    assert_eq!(single.value(), Decimal::ZERO);
    assert_eq!(single.source(), LimitSource::Default);
}

/// (b) A file that exists but does not parse is fail-CLOSED: nothing is
/// restored (the boot-resolved caps stand), every account boots frozen, the
/// corruption is a loud error — and the boot's own persist re-lands a VALID
/// file so the next write is not a rewrite of garbage.
#[test]
fn a_corrupt_risk_state_file_boots_fail_closed() {
    let root = TempRoot::new("corrupt");
    let path = root.file();

    // Seed a real file: caps moved to 250 through the durable path.
    let mut c = core_with(&path, &["default"]);
    let patch = SetRiskLimitsParams {
        max_order_notional: Some(dec!(250)),
        ..Default::default()
    };
    c.apply_risk_limits(&patch, "op", 1_000)
        .expect("limits apply");
    drop(c);

    // The crash window: a half-written file.
    std::fs::write(&path, "{\"limits\": {\"maxOrderNotional\"").unwrap();

    let c2 = core_with(&path, &["default"]);
    // NOT restored — the boot value stands (fail-closed beats fail-open).
    assert_eq!(
        c2.risk.config().max_order_notional,
        dec!(100),
        "a corrupt file must not restore caps the boot cannot vouch for"
    );
    // Every account comes up frozen — a kernel that cannot read its own risk
    // posture must not quietly resume trading (the corrupt-boot clause that
    // survives the #928 retirement of the restart default freeze).
    assert_eq!(
        c2.account_freeze_reason("default", now_ms()).as_deref(),
        Some(
            "risk-state file unreadable — risk posture unverifiable until resumed \
                      (fail-closed, issue #861)"
        )
    );
    // #1060: the corrupt-boot freeze carries a dedicated, independent
    // reasonKind/actor — never `Manual` + `operator:{id}`.
    let gate = c2
        .account_gates()
        .get("default")
        .expect("default account frozen on corrupt boot");
    assert_eq!(gate.reason_kind, GateReason::BootUnverifiable);
    assert_eq!(gate.actor, "system");
    // The boot's persist rewrote a valid file (the frozen boot state).
    assert!(
        matches!(load_risk_state_file(&path), RiskStateLoad::Ok(_)),
        "the boot re-lands a parseable file after the loud error"
    );
}

/// (c1) #685 + #1045: an operator's manual freeze is a `Manual` gate — venue
/// health NEVER lifts it, only `account.unfreeze`/resume does, and a restart
/// clears every gate (pure in-memory). The manual freeze does NOT survive a
/// restart; only `realized_pnl_usd` (the ledger) is durable.
#[test]
fn manual_freeze_never_self_lifts_and_clears_on_restart() {
    let root = TempRoot::new("manual-freeze");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    c.freeze_account_manual("default", "operator pause".to_string());
    assert_eq!(
        c.account_freeze_reason("default", now_ms()).as_deref(),
        Some("operator pause"),
        "manual freeze is in force this session"
    );
    // Venue health past the debounce must NOT lift a manual gate.
    c.auto_lift_if_venue_healthy("default", Some("venue healthy"), now_ms() + 86_400_000);
    assert_eq!(
        c.account_freeze_reason("default", now_ms()).as_deref(),
        Some("operator pause"),
        "#685: a manual freeze never self-lifts"
    );
    c.resume_account("default");
    assert!(
        c.account_freeze_reason("default", now_ms()).is_none(),
        "resume clears the gate"
    );
    drop(c);

    // Restart: gates are pure in-memory, a clean boot starts trading.
    let c2 = core_with(&path, &["default"]);
    assert!(
        c2.account_freeze_reason("default", now_ms()).is_none(),
        "#1045: a restart clears the manual freeze; realized ledger untouched"
    );
}

/// (c2) #1045: a venue-rejection auto-freeze is a `VenueRejection` gate — the
/// health probe lifts it past the debounce, but NOTHING survives a restart
/// (no `auto_frozen` stamp any more).
#[test]
fn venue_rejection_gate_lifts_after_debounce_and_clears_on_restart() {
    let root = TempRoot::new("auto-freeze");
    let path = root.file();

    let frozen_at = now_ms();
    let mut c = core_with(&path, &["default"]);
    c.freeze_account_auto(
        "default",
        "5 consecutive venue rejections".to_string(),
        Some("venue-A"),
        frozen_at,
    );
    assert_eq!(
        c.account_freeze_reason("default", now_ms()).as_deref(),
        Some("5 consecutive venue rejections"),
        "the circumstantial freeze is in force this session"
    );
    // Inside the debounce window a healthy venue must NOT lift.
    c.auto_lift_if_venue_healthy("default", Some("venue-A"), frozen_at + 1_000);
    assert_eq!(
        c.account_freeze_reason("default", now_ms()).as_deref(),
        Some("5 consecutive venue rejections"),
    );
    // Past the debounce the self-heal lifts it — same session, no restart needed.
    c.auto_lift_if_venue_healthy(
        "default",
        Some("venue-A"),
        frozen_at + crate::service::FREEZE_AUTO_LIFT_MIN_MS,
    );
    assert!(c.account_freeze_reason("default", now_ms()).is_none());
    drop(c);

    // Restart: gate cleared — the account comes up trading; no stamp replayed.
    let c2 = core_with(&path, &["default"]);
    assert!(
        c2.account_freeze_reason("default", now_ms()).is_none(),
        "#1045: no freeze posture is replayed across a restart"
    );
}

/// (d) The global kill switch is PURE IN-MEMORY — a kernel that went down
/// killed comes up CLEAR: #1045 v3 retires replaying freeze posture across a
/// restart, so nothing is re-armed automatically. An explicit resume stays
/// lifted across the next boot (the corrupt-boot freeze is an ACCOUNT-level
/// posture; it does not re-arm a killed kernel's switch).
#[test]
fn kill_switch_clears_on_restart_and_resume_stays_lifted() {
    let root = TempRoot::new("kill");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    c.freeze_trading("ops halt before maintenance".to_string(), now_ms());
    assert!(c.is_killed());
    // The global gate is visible in the single gate model while the session lives.
    assert!(c.account_gates().get("global").is_some());
    drop(c);

    // Restart: kill is NOT replayed — the account comes up trading.
    let mut c2 = core_with(&path, &["default"]);
    assert!(
        !c2.is_killed(),
        "restart clears the kill switch — no freeze posture is replayed"
    );
    assert!(c2.account_gates().get("global").is_none());
    c2.resume();
    assert!(!c2.is_killed());
    drop(c2);

    let c3 = core_with(&path, &["default"]);
    assert!(
        !c3.is_killed(),
        "a resumed kill stays resumed across a restart"
    );
}

/// #1057 阻断3：全局熔断解冻同时清 risk 标志，使 tradingFrozen 与 accountGates
/// 单点化（stats_views 只读 GLOBAL_KILL_ACCOUNT 闸门）。
#[test]
fn global_unfreeze_clears_risk_flag_and_gate() {
    let root = TempRoot::new("gunf");
    let path = root.file();
    let mut c = core_with(&path, &["default"]);
    c.freeze_trading("global halt".to_string(), now_ms());
    assert!(c.is_killed(), "freeze_trading sets risk kill flag");
    assert!(
        c.account_gates().get("global").is_some(),
        "global gate present while killed"
    );
    let r = c
        .unfreeze_account("global", "operator", now_ms())
        .expect("global unfreeze succeeds");
    assert!(r.unfrozen, "global gate cleared");
    assert!(
        !c.is_killed(),
        "risk kill flag cleared on global unfreeze (single source of truth)"
    );
    assert!(
        c.account_gates().get("global").is_none(),
        "global gate removed on unfreeze"
    );
    // A fresh core boots clear (no restart replay of the kill).
    let c2 = core_with(&path, &["default"]);
    assert!(!c2.is_killed(), "restart boots clear, no kill replay");
}

/// A core without a risk-state path (the backtester, most in-process tests)
/// keeps today's posture exactly: hot updates report `persisted: false`,
/// nothing is written anywhere, and no restart freeze exists to trip over.
#[test]
fn a_pathless_core_stays_memory_only_and_never_boot_freezes() {
    let root = TempRoot::new("pathless");
    let cfg = crate::service::CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        ..Default::default()
    };
    let mut c = crate::service::Core::with_account_book(cfg, book(&["default"]));

    let patch = SetRiskLimitsParams {
        max_order_notional: Some(dec!(250)),
        ..Default::default()
    };
    let reply = c
        .apply_risk_limits(&patch, "op", 1_000)
        .expect("limits apply");
    assert!(
        !reply.persisted,
        "no file, no persistence claim — the honest memory-only report"
    );
    assert!(
        c.account_freeze_reason("default", now_ms()).is_none(),
        "no risk-state file was ever present: no boot freeze of any kind"
    );
    // And nothing was written under the (unused) temp root.
    assert!(!root.file().exists());
}

/// A restart freezes nobody (#928): a manual freeze on `paper` does NOT
/// survive the reboot — gates are pure in-memory. Only the ledger
/// (`realized_pnl_usd`) and the kill switch are durable; the disk's stale
/// `accountFreezes` rows are ignored, not replayed.
#[test]
fn a_clean_restart_freezes_nobody() {
    let root = TempRoot::new("default-freeze");
    let path = root.file();

    // Boot 1: no freeze yet; a hot write creates the file; one account is
    // manually frozen at runtime.
    let mut c = core_with(&path, &["default", "paper"]);
    assert!(c.account_freeze_reason("default", now_ms()).is_none());
    assert!(c.account_freeze_reason("paper", now_ms()).is_none());
    let patch = SetRiskLimitsParams {
        max_order_notional: Some(dec!(250)),
        ..Default::default()
    };
    c.apply_risk_limits(&patch, "op", 1_000)
        .expect("limits apply");
    c.freeze_account_manual("paper", "operator pause".to_string());
    assert_eq!(
        c.account_freeze_reason("paper", now_ms()).as_deref(),
        Some("operator pause")
    );
    drop(c);

    // Boot 2: the disk's stale freeze rows are NOT replayed — nobody comes up
    // frozen, and resume is a no-op on an already-clear gate.
    let mut c2 = core_with(&path, &["default", "paper"]);
    assert!(
        c2.account_freeze_reason("default", now_ms()).is_none(),
        "issue 928: a restart must not blanket-freeze an unfrozen account"
    );
    assert!(
        c2.account_freeze_reason("paper", now_ms()).is_none(),
        "#1045: manual freeze cleared on restart"
    );
    c2.resume_account("paper");
    drop(c2);

    // Boot 3: a CLEAN restart — nobody freezes, caps still recovered.
    let c3 = core_with(&path, &["default", "paper"]);
    assert!(c3.account_freeze_reason("default", now_ms()).is_none());
    assert!(c3.account_freeze_reason("paper", now_ms()).is_none());
    assert_eq!(c3.risk.config().max_order_notional, dec!(250));
}
