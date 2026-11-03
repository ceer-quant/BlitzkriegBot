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
                updated_at_ms: now,
            })
            .collect(),
        AccountId::from(ids[0]),
    )
}

fn core_with(path: &Path, ids: &[&str]) -> crate::service::Core {
    crate::service::Core::with_account_book(cfg(path), book(ids))
}

/// (a) The #854 finding, fixed: a hot `risk.setLimits` / `risk.setSystemic`
/// write lands in the risk-state file, and a rebuilt core (the kill -9
/// drill) judges orders by the SAME caps. Provenance travels with the
/// systemic bound (`flag` — exactly what the write stamped).
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
        sreply.persisted,
        "the systemic write lands in the same file"
    );
    drop(c);

    let c2 = core_with(&path, &["default"]);
    assert_eq!(c2.risk.config().max_order_notional, dec!(250));
    assert_eq!(c2.risk.config().max_open_notional_usd, dec!(4000));
    assert_eq!(c2.config.min_shares, dec!(2));
    let single = &c2.risk.config().systemic.account.max_single_loss_usd;
    assert_eq!(single.value(), dec!(75));
    assert_eq!(single.source(), LimitSource::Flag);
    let drawdown = &c2.risk.config().systemic.account.max_daily_drawdown_usd;
    assert_eq!(drawdown.value(), dec!(300));
    // The file is the wire format the PR documents: camelCase field names,
    // decimal-string values.
    let text = std::fs::read_to_string(&path).expect("the file exists");
    assert!(
        text.contains("\"maxOrderNotional\": \"250\""),
        "camelCase decimal-string cell expected, got: {text}"
    );
    assert!(text.contains("\"maxSingleLossUsd\""));
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
    // posture must not quietly resume trading.
    assert_eq!(
        c2.account_freeze_reason("default"),
        Some(RESTART_FREEZE_REASON)
    );
    // The boot's persist rewrote a valid file (the frozen boot state).
    assert!(
        matches!(load_risk_state_file(&path), RiskStateLoad::Ok(_)),
        "the boot re-lands a parseable file after the loud error"
    );
}

/// (c1) The #685 shape across a restart: an operator's manual freeze comes
/// back with its original reason, venue health NEVER lifts it (no
/// auto_frozen stamp travels with it), and only an explicit resume clears it.
#[test]
fn manual_freeze_survives_restart_and_never_self_lifts() {
    let root = TempRoot::new("manual-freeze");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    c.freeze_account_manual("default", "operator pause".to_string());
    drop(c);

    let mut c2 = core_with(&path, &["default"]);
    assert_eq!(
        c2.account_freeze_reason("default"),
        Some("operator pause"),
        "the original reason rides the restart"
    );
    // Far past the debounce — a stamped auto freeze would lift here. A manual
    // freeze has no stamp, so this must be a no-op.
    c2.auto_lift_if_venue_healthy("default", "venue healthy", now_ms() + 86_400_000);
    assert_eq!(
        c2.account_freeze_reason("default"),
        Some("operator pause"),
        "#685: a manual freeze never self-lifts, across restarts too"
    );
    c2.resume_account("default");
    assert!(c2.account_freeze_reason("default").is_none());
    drop(c2);

    // The lift is durable — but a boot WITH a risk-state file still applies
    // the restart default freeze (「账户默认冻结」): the account comes back
    // frozen under the restart reason, not trading, until resumed again.
    let c3 = core_with(&path, &["default"]);
    assert_eq!(
        c3.account_freeze_reason("default"),
        Some(RESTART_FREEZE_REASON)
    );
}

/// (c2) The kernel's auto-freeze stamp survives a restart, so the self-heal
/// still debounces from the instant the kernel FIRST lost the account — and
/// a healthy venue past `FREEZE_AUTO_LIFT_MIN_MS` lifts it, exactly as it
/// would have without the restart in between.
#[test]
fn auto_frozen_stamp_survives_and_still_self_lifts() {
    let root = TempRoot::new("auto-freeze");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    let frozen_at = now_ms();
    c.freeze_account_auto(
        "default",
        "5 consecutive venue rejections".to_string(),
        frozen_at,
    );
    assert!(c.freeze_book.auto_frozen.contains_key("default"));
    drop(c);

    let mut c2 = core_with(&path, &["default"]);
    assert_eq!(
        c2.account_freeze_reason("default"),
        Some("5 consecutive venue rejections"),
        "the circumstantial freeze comes back with its cause"
    );
    assert_eq!(
        c2.freeze_book.auto_frozen.get("default"),
        Some(&frozen_at),
        "the stamp restores — the debounce counts from the original freeze"
    );
    // Inside the debounce window a healthy venue must NOT lift (the stamp is
    // what makes the check possible at all).
    c2.auto_lift_if_venue_healthy("default", "venue healthy", frozen_at + 1_000);
    assert!(c2.account_freeze_reason("default").is_some());
    // Past the debounce, the same self-heal lifts it — the restart did not
    // turn a circumstantial freeze into a life sentence.
    c2.auto_lift_if_venue_healthy(
        "default",
        "venue healthy",
        frozen_at + crate::service::FREEZE_AUTO_LIFT_MIN_MS,
    );
    assert!(c2.account_freeze_reason("default").is_none());
}

/// (d) The global kill switch outlives the process — with the reason an
/// operator must see — and an explicit resume stays lifted across the next
/// boot (the restart default freeze is an ACCOUNT-level posture; it does not
/// re-arm a killed kernel's switch).
#[test]
fn kill_switch_survives_restart_and_resume_stays_lifted() {
    let root = TempRoot::new("kill");
    let path = root.file();

    let mut c = core_with(&path, &["default"]);
    c.kill("ops halt before maintenance".to_string());
    assert!(c.is_killed());
    drop(c);

    let mut c2 = core_with(&path, &["default"]);
    assert!(
        c2.is_killed(),
        "a kernel that went down killed stays killed"
    );
    assert!(
        c2.risk
            .kill_reason()
            .is_some_and(|r| r.contains("ops halt before maintenance")),
        "the operator's reason survives, not a paraphrase"
    );
    c2.resume();
    assert!(!c2.is_killed());
    drop(c2);

    let c3 = core_with(&path, &["default"]);
    assert!(
        !c3.is_killed(),
        "a resumed kill stays resumed across a restart"
    );
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
        c.account_freeze_reason("default").is_none(),
        "no risk-state file was ever present: no restart default freeze"
    );
    // And nothing was written under the (unused) temp root.
    assert!(!root.file().exists());
}

/// The restart default freeze (「账户默认冻结」) covers EVERY account and
/// re-applies on EVERY boot that finds a risk-state file — including a clean
/// restart after a full resume. A restart must never quietly resume trading.
#[test]
fn restart_default_freeze_covers_every_account_on_every_boot() {
    let root = TempRoot::new("default-freeze");
    let path = root.file();

    // Boot 1: the file does not exist yet — first boot keeps today's
    // behaviour (no freeze). A hot write creates the file.
    let mut c = core_with(&path, &["default", "paper"]);
    assert!(c.account_freeze_reason("default").is_none());
    assert!(c.account_freeze_reason("paper").is_none());
    let patch = SetRiskLimitsParams {
        max_order_notional: Some(dec!(250)),
        ..Default::default()
    };
    c.apply_risk_limits(&patch, "op", 1_000)
        .expect("limits apply");
    drop(c);

    // Boot 2: the file exists — every account that is not already frozen
    // comes up frozen.
    let mut c2 = core_with(&path, &["default", "paper"]);
    assert_eq!(
        c2.account_freeze_reason("default"),
        Some(RESTART_FREEZE_REASON)
    );
    assert_eq!(
        c2.account_freeze_reason("paper"),
        Some(RESTART_FREEZE_REASON)
    );
    // An operator resumes both.
    c2.resume_account("default");
    c2.resume_account("paper");
    drop(c2);

    // Boot 3: a CLEAN restart (nothing frozen on disk) still defaults to
    // frozen — the file's presence, not a crash, is the trigger.
    let c3 = core_with(&path, &["default", "paper"]);
    assert_eq!(
        c3.account_freeze_reason("default"),
        Some(RESTART_FREEZE_REASON),
        "a clean restart must not resume trading on its own either"
    );
    assert_eq!(
        c3.account_freeze_reason("paper"),
        Some(RESTART_FREEZE_REASON)
    );
    // The operator's caps still recovered through all of it.
    assert_eq!(c3.risk.config().max_order_notional, dec!(250));
}
