use super::*;
use serde_json::Value;

fn payload_json(update: &UpdateSnapshot) -> Value {
    serde_json::to_value(system_version_payload(
        blitzkrieg_build_info::BUILD_INFO,
        update,
    ))
    .expect("payload serialises")
}

#[test]
fn a_bare_tag_is_not_a_version() {
    assert_eq!(parse_release_tag("v0.2.2").as_deref(), Some("0.2.2"));
    assert_eq!(
        parse_release_tag("0.2.2"),
        None,
        "missing v must be refused"
    );
    assert_eq!(parse_release_tag("vlatest"), None);
    assert_eq!(parse_release_tag("v0.2"), None);
    // A pre-release parses; the PROMPT policy is is_newer's business.
    assert_eq!(
        parse_release_tag("v0.3.0-rc.1").as_deref(),
        Some("0.3.0-rc.1")
    );
}

#[test]
fn comparison_is_semver_not_lexicographic() {
    // String compare judges 0.10.0 < 0.9.0 — exactly why it must not be used.
    assert!(is_newer("0.10.0", "0.9.0"));
    assert!(is_newer("0.2.2", "0.2.1"));
    assert!(!is_newer("0.2.1", "0.2.1"));
    assert!(!is_newer("0.1.9", "0.2.1"));
}

#[test]
fn pre_releases_do_not_nag_a_stable_install() {
    assert!(
        !is_newer("0.3.0-rc.1", "0.2.1"),
        "an rc must not prompt a stable build"
    );
    assert!(
        !is_newer("0.3.0-rc.1", "0.3.0"),
        "same-number stable beats rc"
    );
    assert!(is_newer("0.3.0", "0.3.0-rc.1"));
}

#[test]
fn garbage_never_becomes_an_update_notice() {
    assert!(!is_newer("latest", "0.2.1"));
    assert!(!is_newer("0.2.2", "garbage"));
}

/// N5's catcher: "not checked" must stay null on the wire; a FAILED check
/// is also "unknown", not "up to date"; only a real conclusion is a bool.
#[test]
fn the_payload_is_three_state() {
    let b = blitzkrieg_build_info::BUILD_INFO;
    let s = UpdateSnapshot {
        outcome: CheckOutcome::NotChecked,
        latest: None,
        release_url: None,
        last_check_ms: 0,
        check_enabled: false,
        auto_update: false,
        stage: StageState::default(),
    };
    assert_eq!(system_version_payload(b, &s).update_available, None);
    assert_eq!(system_version_payload(b, &s).last_check_ms, None);
    // "not checked" carries no failure reason, and the stage machine rides
    // the payload at its default (Idle).
    assert_eq!(system_version_payload(b, &s).last_check_error, None);
    assert_eq!(system_version_payload(b, &s).stage.phase, StagePhase::Idle);

    let s = UpdateSnapshot {
        outcome: CheckOutcome::Failed {
            detail: "dns".into(),
        },
        latest: None,
        release_url: None,
        last_check_ms: 1,
        check_enabled: true,
        auto_update: false,
        stage: StageState::default(),
    };
    assert_eq!(system_version_payload(b, &s).update_available, None);
    // A FAILED check surfaces its reason — the operator-facing point of
    // the new field.
    assert_eq!(
        system_version_payload(b, &s).last_check_error.as_deref(),
        Some("dns")
    );

    let s = UpdateSnapshot {
        outcome: CheckOutcome::UpToDate,
        latest: None,
        release_url: None,
        last_check_ms: 1,
        check_enabled: true,
        auto_update: false,
        stage: StageState::default(),
    };
    assert_eq!(system_version_payload(b, &s).update_available, Some(false));
    assert_eq!(system_version_payload(b, &s).last_check_error, None);
}

/// The wire keys are the camelCase contract, and the provenance half
/// mirrors the compile-time stamp.
#[test]
fn the_provenance_half_mirrors_the_stamp_and_camel_case() {
    let s = UpdateSnapshot::default();
    let j = payload_json(&s);
    for key in [
        "version",
        "gitHash",
        "gitDirty",
        "buildDate",
        "target",
        "updateAvailable",
        "latestVersion",
        "autoUpdate",
        "checkEnabled",
        "lastCheckMs",
        "releaseUrl",
    ] {
        assert!(j.get(key).is_some(), "missing wire key {key}");
    }
    assert_eq!(j["version"], crate::CORE_VERSION);
    assert_eq!(j["gitHash"], blitzkrieg_build_info::BUILD_INFO.git_hash);
}

#[tokio::test]
async fn a_disabled_check_makes_no_request() {
    let state = Arc::new(UpdateState::new(false, false));
    // Awaiting this future panics — disabled must not even POLL it.
    let must_not_run = async {
        panic!("check_enabled=false must not issue any request (INV-3)");
        #[allow(unreachable_code)]
        Ok::<String, String>(String::new())
    };
    check_once(&state, "0.2.1", 1, must_not_run).await;
    assert_eq!(state.snapshot().outcome, CheckOutcome::NotChecked);
}

#[tokio::test]
async fn an_api_error_is_recorded_not_raised() {
    let state = Arc::new(UpdateState::new(true, false));
    check_once(&state, "0.2.1", 42, async {
        Err("connection refused".to_string())
    })
    .await;
    let s = state.snapshot();
    assert!(matches!(s.outcome, CheckOutcome::Failed { .. }));
    assert_eq!(
        system_version_payload(blitzkrieg_build_info::BUILD_INFO, &s).update_available,
        None
    );
}

#[tokio::test]
async fn a_newer_release_becomes_an_available_state() {
    let state = Arc::new(UpdateState::new(true, false));
    let body = r#"{"tag_name":"v0.2.2","html_url":"https://example/v0.2.2"}"#;
    check_once(&state, "0.2.1", 7, async move { Ok(body.to_string()) }).await;
    let s = state.snapshot();
    assert_eq!(s.outcome, CheckOutcome::Available);
    assert_eq!(s.latest.as_deref(), Some("0.2.2"));
    assert_eq!(
        system_version_payload(blitzkrieg_build_info::BUILD_INFO, &s).update_available,
        Some(true)
    );
}

/// A5's shape: switches survive persist → fresh state → load. The outcome
/// does not survive, so the payload stays null until a new check runs.
#[test]
fn switches_survive_a_persist_load_round_trip() {
    let dir = std::env::temp_dir().join(format!("bk-update-test-{}", std::process::id()));
    let path = dir.join("state.json");
    let _ = std::fs::remove_dir_all(&dir);

    let first = UpdateState::new(true, true);
    first.set_enabled(None, Some(true));
    first.apply(CheckOutcome::Available, Some("0.9.9".into()), None, 1234);
    persist_to(&path, &first).expect("persist writes");

    let second = UpdateState::new(false, false);
    load_from(&path, &second);
    let s = second.snapshot();
    assert!(s.check_enabled, "checkEnabled must survive the restart");
    assert!(s.auto_update, "autoUpdate must survive the restart");
    assert_eq!(s.last_check_ms, 1234);
    assert_eq!(s.latest.as_deref(), Some("0.9.9"));
    // …but the payload still says NOT CHECKED: this process has not checked.
    assert_eq!(
        system_version_payload(blitzkrieg_build_info::BUILD_INFO, &s).update_available,
        None
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A6's shape at the helper level: a persist that cannot land (unwritable
/// path) is an ERROR, never a silent ok.
#[test]
fn a_failing_persist_is_an_error_not_a_lie() {
    let state = UpdateState::new(true, false);
    let bad = std::path::PathBuf::from("/dev/null/bk-cannot-exist/state.json");
    assert!(persist_to(&bad, &state).is_err());
}

// ── #379: the automatic-check scheduler ─────────────────────────────────

/// The shipped configuration spawns NO task: interval 0 (or the switch
/// off) means the returned handle wraps an immediately-finished future —
/// there is nothing to leak, nothing to dial (INV-3's structural half).
#[tokio::test]
async fn no_interval_means_no_scheduler_task() {
    let state = Arc::new(UpdateState::new(true, false));
    assert_eq!(state.auto_check_interval_secs(), 0);
    let handle =
        spawn_auto_check_scheduler(state.clone(), "0.3.0".into(), None, std::future::pending());
    // The task finishes at once (nothing loops behind a timer).
    tokio::time::timeout(std::time::Duration::from_millis(200), handle)
        .await
        .expect("the no-op scheduler task must end immediately")
        .expect("join");
    // The switch is on but the clock is 0 → same nothing.
    let state_on = Arc::new(UpdateState::new(true, false));
    state_on.set_auto_check_interval_secs(0);
    let handle2 =
        spawn_auto_check_scheduler(state_on, "0.3.0".into(), None, std::future::pending());
    tokio::time::timeout(std::time::Duration::from_millis(200), handle2)
        .await
        .expect("interval 0 must not spawn a looping task")
        .expect("join");
}

/// The switch guards the scheduler too: check_enabled=false + a nonzero
/// interval still spawns nothing. The timer never overrides the switch.
#[tokio::test]
async fn a_disabled_check_switch_disables_the_scheduler() {
    let state = Arc::new(UpdateState::new(false, false));
    state.set_auto_check_interval_secs(60);
    let handle =
        spawn_auto_check_scheduler(state.clone(), "0.3.0".into(), None, std::future::pending());
    tokio::time::timeout(std::time::Duration::from_millis(200), handle)
        .await
        .expect("checkEnabled=false must not spawn a looping task")
        .expect("join");
    assert_eq!(state.snapshot().outcome, CheckOutcome::NotChecked);
}

/// The full loop, with the injected shutdown: an armed scheduler (interval
/// 1s) survives ticks, and shutdown breaks the loop promptly. The
/// round-trip through `check_once` (verdicts, persistence) is covered by
/// the dedicated check_once tests above; here the LOOP mechanics are what
/// is under test — armed vs. task-less, and a clean stop.
#[tokio::test]
async fn an_armed_scheduler_loops_until_shutdown() {
    let state = Arc::new(UpdateState::new(true, false));
    state.set_auto_check_interval_secs(1);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = spawn_auto_check_scheduler(state.clone(), "0.3.0".into(), None, async move {
        let _ = rx.await;
    });
    // The loop is now parked on its first interval sleep (1s); prove it is
    // alive by NOT joining immediately, then break it via shutdown.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!handle.is_finished(), "an armed scheduler stays alive");
    tx.send(()).expect("shutdown send");
    tokio::time::timeout(std::time::Duration::from_millis(500), handle)
        .await
        .expect("shutdown must stop the scheduler promptly")
        .expect("join");
}

/// The clock is restart-to-change: it survives persist → load like the
/// switches do.
#[test]
fn the_interval_survives_a_persist_load_round_trip() {
    let dir = std::env::temp_dir().join(format!("bk-update-int-{}", std::process::id()));
    let path = dir.join("state.json");
    let _ = std::fs::remove_dir_all(&dir);

    let first = UpdateState::new(true, false);
    first.set_auto_check_interval_secs(3600);
    persist_to(&path, &first).expect("persist writes");

    let second = UpdateState::new(false, false);
    load_from(&path, &second);
    assert_eq!(second.auto_check_interval_secs(), 3600);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── #379: the staging state machine ─────────────────────────────────────

fn sums_for(asset: &str, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}  {}\n", Sha256::digest(bytes), asset)
}

/// INV-3's runtime half for staging: autoUpdate=false → the future the
/// caller would await for the download is NEVER constructed, and the
/// machine does not move.
#[tokio::test]
async fn staging_disabled_makes_no_request_at_all() {
    let state = Arc::new(UpdateState::new(true, false));
    // An Available verdict is present, but the autoUpdate switch is off.
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let must_not_run = async {
        panic!("autoUpdate=false must not construct any download (INV-3)");
        #[allow(unreachable_code)]
        Ok::<(Vec<u8>, String), String>((Vec::new(), String::new()))
    };
    let dir = std::env::temp_dir().join(format!("bk-stage-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    stage_once(&state, "aarch64-apple-darwin", 2, &dir, |_, _| must_not_run).await;
    assert_eq!(state.stage_snapshot().phase, StagePhase::Idle);
    assert!(
        std::fs::read_dir(&dir).is_err(),
        "no staging dir must exist"
    );
}

/// No `Available` verdict → nothing to stage (and an existing staged copy
/// is NOT clobbered by the no-op).
#[tokio::test]
async fn staging_without_an_available_verdict_is_a_no_op() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::UpToDate, None, None, 1);
    let dir = std::env::temp_dir().join(format!("bk-stage-noop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keepme"), b"prior bytes").unwrap();
    stage_once(&state, "t", 2, &dir, |tag, _v| async move {
        panic!("no Available verdict must not dial ({tag})");
    })
    .await;
    assert_eq!(state.stage_snapshot().phase, StagePhase::Idle);
    assert!(dir.join("keepme").exists(), "prior bytes untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The happy path: bytes + matching SHA256SUMS land in the staging dir,
/// byte-exact, and the machine ends in `Staged`.
#[tokio::test]
async fn staging_a_matching_asset_ends_in_staged() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let asset = asset_name("0.3.1", "aarch64-apple-darwin");
    let bytes = b"pretend release archive".to_vec();
    let sums = sums_for(&asset, &bytes);
    let dir = std::env::temp_dir().join(format!("bk-stage-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    stage_once(&state, "aarch64-apple-darwin", 7, &dir, |tag, version| {
        assert_eq!(tag, "v0.3.1");
        assert_eq!(version, "0.3.1");
        async move { Ok((bytes.clone(), sums.clone())) }
    })
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Staged);
    assert_eq!(s.version.as_deref(), Some("0.3.1"));
    assert_eq!(s.asset.as_deref(), Some(asset.as_str()));
    assert_eq!(s.staged_at_ms, Some(7));
    // The bytes on disk are exactly the verified bytes (read back, not
    // trusted from the write).
    assert_eq!(
        std::fs::read(dir.join(&asset)).unwrap(),
        b"pretend release archive"
    );
    assert!(dir.join("SHA256SUMS").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A7's shape: one tampered byte → Err-equivalent (Failed), the tampered
/// bytes never stay in the staging dir, and nothing looks installed.
#[tokio::test]
async fn a_hash_mismatch_fails_and_cleans_the_partial_bytes() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let asset = asset_name("0.3.1", "aarch64-apple-darwin");
    let bytes = b"tampered archive".to_vec();
    // Digest of DIFFERENT bytes — the compare must fail (N8).
    let sums = sums_for(&asset, b"the real archive");
    let dir = std::env::temp_dir().join(format!("bk-stage-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    stage_once(&state, "aarch64-apple-darwin", 7, &dir, |tag, version| {
        assert_eq!(tag, "v0.3.1");
        assert_eq!(version, "0.3.1");
        async move { Ok((bytes.clone(), sums.clone())) }
    })
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Failed);
    assert!(!dir.join(&asset).exists(), "tampered bytes must not remain");
    let _ = std::fs::remove_dir_all(&dir);
}

/// N10's shape at the state-machine level: a SHA256SUMS without our
/// asset's entry → Failed, never a "skip verification".
#[tokio::test]
async fn a_missing_sums_entry_fails_the_stage() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let dir = std::env::temp_dir().join(format!("bk-stage-n10-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    stage_once(&state, "aarch64-apple-darwin", 7, &dir, |_, _| async move {
        Ok((
            b"bytes".to_vec(),
            "abcdef  some-other-asset.tar.gz\n".into(),
        ))
    })
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Failed);
    assert!(s.detail.unwrap_or_default().contains("no entry"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed re-stage must not destroy a previously staged good copy: the
/// Failed phase replaces the SUMMARY only when no good copy exists.
#[tokio::test]
async fn a_failed_restage_keeps_the_prior_staged_summary() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let asset = asset_name("0.3.1", "t");
    let bytes = b"good".to_vec();
    let sums = sums_for(&asset, &bytes);
    let dir = std::env::temp_dir().join(format!("bk-stage-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // First run stages successfully.
    stage_once(&state, "t", 1, &dir, |_, _| async move {
        Ok((bytes.clone(), sums.clone()))
    })
    .await;
    assert_eq!(state.stage_snapshot().phase, StagePhase::Staged);
    // Second run for the SAME version: with the already-staged guard the
    // fetch is never constructed, so the re-stage is a NO-OP — the
    // strongest possible form of "a failed re-stage must not destroy a
    // good copy" (there is no attempt left to fail). The different-
    // version failure path is `an_already_staged_version_skips_the_
    // download`'s tail.
    stage_once(&state, "t", 2, &dir, |_, _| async move {
        panic!("the same version must not re-download");
    })
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Staged);
    // …the good copy's identity survives untouched.
    assert_eq!(s.version.as_deref(), Some("0.3.1"));
    assert_eq!(s.asset.as_deref(), Some(asset.as_str()));
    assert_eq!(s.staged_at_ms, Some(1));
    assert_eq!(std::fs::read(dir.join(&asset)).unwrap(), b"good");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One run at a time: a caller arriving while `Downloading` leaves
/// without starting a parallel download.
#[tokio::test]
async fn a_second_stage_call_while_downloading_is_refused() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    state.set_stage(StageState {
        phase: StagePhase::Downloading,
        ..StageState::default()
    });
    stage_once(
        &state,
        "t",
        2,
        std::path::Path::new("/tmp"),
        |_, _| async move {
            panic!("a run already in flight must not be duplicated");
        },
    )
    .await;
    // The machine stays where it was — the second caller changed nothing.
    assert_eq!(state.stage_snapshot().phase, StagePhase::Downloading);
}

/// The same release, already staged → a re-stage is a NO-OP (the fetch is
/// never constructed): a poller poking the verb on a schedule must not
/// push the archive through the network again for bytes already held.
/// A DIFFERENT version still re-runs.
#[tokio::test]
async fn an_already_staged_version_skips_the_download() {
    let state = Arc::new(UpdateState::new(true, true));
    state.apply(CheckOutcome::Available, Some("0.3.1".into()), None, 1);
    let dir = std::env::temp_dir().join(format!("bk-stage-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    stage_once(&state, "t", 1, &dir, |_, _| async move {
        Ok((
            b"good".to_vec(),
            sums_for(&asset_name("0.3.1", "t"), b"good"),
        ))
    })
    .await;
    assert_eq!(state.stage_snapshot().phase, StagePhase::Staged);
    // Same version again: the fetch must not even be polled.
    stage_once(&state, "t", 2, &dir, |_, _| async move {
        panic!("an already-staged version must not re-download");
    })
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Staged);
    assert_eq!(s.staged_at_ms, Some(1), "the first run's summary survives");
    // A different verdict version re-runs the fetch (the panic above would
    // have fired otherwise; here the fetch errors and the phase moves).
    state.apply(CheckOutcome::Available, Some("0.3.2".into()), None, 3);
    stage_once(
        &state,
        "t",
        4,
        &dir,
        |_, _| async move { Err("offline".into()) },
    )
    .await;
    let s = state.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Failed);
    // …and the good 0.3.1 copy survives the failed 0.3.2 attempt.
    assert_eq!(s.version.as_deref(), Some("0.3.1"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The staging summary rides persist → load: a `staged` phase with a
/// version survives as the same summary (the launcher re-verifies bytes).
#[test]
fn the_stage_summary_survives_a_persist_load_round_trip() {
    let dir = std::env::temp_dir().join(format!("bk-stage-p-{}", std::process::id()));
    let path = dir.join("state.json");
    let _ = std::fs::remove_dir_all(&dir);

    let first = UpdateState::new(true, true);
    first.set_stage(StageState {
        phase: StagePhase::Staged,
        detail: Some("sha256 verified".into()),
        version: Some("0.3.1".into()),
        asset: Some("blitzkrieg-0.3.1-t.tar.gz".into()),
        sha256: Some("ab".repeat(32)),
        staged_at_ms: Some(99),
    });
    persist_to(&path, &first).expect("persist writes");

    let second = UpdateState::new(false, false);
    load_from(&path, &second);
    let s = second.stage_snapshot();
    assert_eq!(s.phase, StagePhase::Staged);
    assert_eq!(s.version.as_deref(), Some("0.3.1"));
    assert_eq!(s.asset.as_deref(), Some("blitzkrieg-0.3.1-t.tar.gz"));
    assert_eq!(s.staged_at_ms, Some(99));

    // A lying file (staged without a version) degrades to Idle, never to
    // a half-truth.
    let lying = serde_json::json!({
        "checkEnabled": false,
        "autoUpdate": false,
        "lastCheckMs": 0,
        "stage": {"phase": "staged"},
    });
    std::fs::write(&path, serde_json::to_vec(&lying).unwrap()).unwrap();
    let third = UpdateState::new(false, false);
    load_from(&path, &third);
    assert_eq!(third.stage_snapshot().phase, StagePhase::Idle);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The SHA256SUMS parser's asset line is target-specific: another
/// target's line never matches ours.
#[test]
fn sums_parsing_is_asset_specific() {
    let ours = asset_name("0.3.1", "x86_64-unknown-linux-gnu");
    let sums = sums_for(&ours, b"x");
    assert!(
        expected_sha256(&sums, &ours).is_some(),
        "our own asset line matches"
    );
    assert_eq!(
        expected_sha256(&sums, &asset_name("0.3.1", "aarch64-apple-darwin")),
        None,
        "another target's line is not ours"
    );
}
