//! Version and update state (VERSIONING.md §5/§7).
//!
//! Three things, with the boundaries written down here:
//!   1. the `system.version` payload (read-only, lock-free, answerable at any time);
//!   2. THE single semver comparison point for a remote release (never string compare);
//!   3. the runtime state of the two update switches (persisted + audited).
//!
//! This module never downloads or replaces a binary. Installing is the
//! launcher's job (§7.5): a position-holding process rewriting the binary it
//! is executing is the classic self-inflicted wound.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use blitzkrieg_market_api::net::now_ms;
use serde::{Deserialize, Serialize};

/// Why a check failed, classified by what the OPERATOR can do about it —
/// never a bare `io::Error`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckOutcome {
    /// Not checked yet (the default; stays this way while checks are disabled).
    #[default]
    NotChecked,
    /// Checked, and the local build is the newest release.
    UpToDate,
    /// Checked, and a newer release exists.
    Available,
    /// The check itself failed: DNS/TLS/timeout/non-2xx/malformed JSON.
    /// `detail` is operator-facing and must never contain credentials.
    Failed { detail: String },
}

impl CheckOutcome {
    /// The token the audit line and logs use for this outcome.
    pub fn token(&self) -> &'static str {
        match self {
            CheckOutcome::NotChecked => "notChecked",
            CheckOutcome::UpToDate => "upToDate",
            CheckOutcome::Available => "available",
            CheckOutcome::Failed { .. } => "failed",
        }
    }
}

/// Runtime update state. Process-level, separate from `Core` — a version
/// question must not queue behind a position update.
#[derive(Debug, Default)]
pub struct UpdateState {
    outcome: Mutex<CheckOutcome>,
    latest: Mutex<Option<String>>,
    release_url: Mutex<Option<String>>,
    last_check_ms: AtomicU64,
    check_enabled: AtomicBool,
    auto_update: AtomicBool,
}

impl UpdateState {
    /// The built-in default is ALWAYS off; the two arguments are where the
    /// resolved configuration (CLI > env > TOML) lands, never a shortcut to
    /// "on by default".
    pub fn new(check_enabled: bool, auto_update: bool) -> Self {
        Self {
            outcome: Mutex::new(CheckOutcome::NotChecked),
            latest: Mutex::new(None),
            release_url: Mutex::new(None),
            last_check_ms: AtomicU64::new(0),
            check_enabled: AtomicBool::new(check_enabled),
            auto_update: AtomicBool::new(auto_update),
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        let outcome = self
            .outcome
            .lock()
            .map(|g| g.clone())
            .unwrap_or(CheckOutcome::NotChecked);
        UpdateSnapshot {
            outcome,
            latest: self.latest.lock().ok().and_then(|g| g.clone()),
            release_url: self.release_url.lock().ok().and_then(|g| g.clone()),
            last_check_ms: self.last_check_ms.load(Ordering::Relaxed),
            check_enabled: self.check_enabled.load(Ordering::Relaxed),
            auto_update: self.auto_update.load(Ordering::Relaxed),
        }
    }

    /// The `system.update.configure` write. `None` leaves a switch unchanged.
    pub fn set_enabled(&self, check: Option<bool>, auto: Option<bool>) {
        if let Some(v) = check {
            self.check_enabled.store(v, Ordering::Relaxed);
        }
        if let Some(v) = auto {
            self.auto_update.store(v, Ordering::Relaxed);
        }
    }

    fn apply(
        &self,
        outcome: CheckOutcome,
        latest: Option<String>,
        url: Option<String>,
        at_ms: u64,
    ) {
        if let Ok(mut g) = self.outcome.lock() {
            *g = outcome;
        }
        if let Ok(mut g) = self.latest.lock() {
            *g = latest;
        }
        if let Ok(mut g) = self.release_url.lock() {
            *g = url;
        }
        self.last_check_ms.store(at_ms, Ordering::Relaxed);
    }
}

/// One consistent copy of the update state.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSnapshot {
    pub outcome: CheckOutcome,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub last_check_ms: u64,
    pub check_enabled: bool,
    pub auto_update: bool,
}

/// `system.update.configure` params. Both keys optional — a request that names
/// only one switch leaves the other exactly as it was.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigureParams {
    pub check_enabled: Option<bool>,
    pub auto_update: Option<bool>,
}

/// The final `system.version` payload. Field names are camelCase — the wire
/// contract (§5): additions allowed, renames/removals are breaking (R1).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemVersion {
    pub version: String,
    pub git_hash: String,
    pub git_dirty: bool,
    pub build_date: String,
    pub target: String,
    /// THREE-STATE: `null` = not checked; `true`/`false` = a conclusion.
    /// Collapsing null into `false` is the lie this whole design exists to
    /// prevent (§5.3, N5).
    pub update_available: Option<bool>,
    pub latest_version: Option<String>,
    pub auto_update: bool,
    pub check_enabled: bool,
    pub last_check_ms: Option<u64>,
    pub release_url: Option<String>,
}

/// Join the build stamp and the update state into the payload. Pure — the
/// dispatch branch stays trivial and the three-state contract is testable
/// without any process around.
pub fn system_version_payload(
    build: blitzkrieg_build_info::BuildInfo,
    s: &UpdateSnapshot,
) -> SystemVersion {
    // "Checked" means last_check_ms is non-zero AND there is a conclusion.
    // Merely enabling the switch is not a check — otherwise the UI would draw
    // "switch on" as "up to date".
    let (available, latest, url) = match (&s.outcome, s.last_check_ms) {
        (CheckOutcome::UpToDate, ms) if ms > 0 => (Some(false), None, None),
        (CheckOutcome::Available, ms) if ms > 0 => {
            (Some(true), s.latest.clone(), s.release_url.clone())
        }
        _ => (None, None, None), // not checked / failed → the null of three-state, never a guess
    };
    SystemVersion {
        version: build.version.to_string(),
        git_hash: build.git_hash.to_string(),
        git_dirty: build.is_dirty(),
        build_date: build.build_date.to_string(),
        target: build.target.to_string(),
        update_available: available,
        latest_version: latest,
        auto_update: s.auto_update,
        check_enabled: s.check_enabled,
        last_check_ms: (s.last_check_ms > 0).then_some(s.last_check_ms),
        release_url: url,
    }
}

/// Remote tag → version. `v0.2.2` → `Some("0.2.2")`. Strict: anything that is
/// not `v<semver>` is refused — no best-effort digit extraction, because a
/// misread tag becomes a wrong update notice.
pub fn parse_release_tag(tag: &str) -> Option<String> {
    let v = tag.trim().strip_prefix('v')?;
    semver::Version::parse(v).ok().map(|p| p.to_string())
}

/// Is the remote newer than local? THE comparison point (R3); callers must not
/// compare strings themselves. Pre-release semantics are semver's:
/// `0.3.0-rc.1 < 0.3.0`, and a pre-release does not nag a stable install —
/// an rc only prompts an rc line.
pub fn is_newer(remote: &str, local: &str) -> bool {
    let (Ok(r), Ok(l)) = (
        semver::Version::parse(remote),
        semver::Version::parse(local),
    ) else {
        // Either side unparseable → never announce an update. Fewer notices
        // beat wrong notices.
        return false;
    };
    if !r.pre.is_empty() && l.pre.is_empty() {
        // Local is stable, remote is a pre-release: not an update. The rc must
        // graduate to a stable tag before a stable install is asked to move.
        return false;
    }
    r > l
}

/// One check. `fetch` is injected so tests use fake HTTP instead of the network.
///
/// Failure never escapes as an error to the caller: a failed update check must
/// not touch the trading process — it lands in the state and the UI shows
/// "unknown".
pub async fn check_once(
    state: &Arc<UpdateState>,
    local_version: &str,
    at_ms: u64,
    fetch: impl std::future::Future<Output = Result<String, String>>,
) {
    if !state.snapshot().check_enabled {
        // Disabled → not a single request (the structural form of INV-3).
        return;
    }
    match fetch.await {
        Ok(body) => match parse_latest_release(&body) {
            Ok((tag, url)) => match parse_release_tag(&tag) {
                Some(remote) if is_newer(&remote, local_version) => {
                    state.apply(CheckOutcome::Available, Some(remote), Some(url), at_ms)
                }
                Some(_) => state.apply(CheckOutcome::UpToDate, None, None, at_ms),
                None => state.apply(
                    CheckOutcome::Failed {
                        detail: format!("release tag not v<semver>: {tag}"),
                    },
                    None,
                    None,
                    at_ms,
                ),
            },
            Err(detail) => state.apply(CheckOutcome::Failed { detail }, None, None, at_ms),
        },
        Err(detail) => state.apply(CheckOutcome::Failed { detail }, None, None, at_ms),
    }
}

/// Pull `(tag_name, html_url)` out of the GitHub Releases API JSON. Only these
/// two fields are read: every extra field is one more surface upstream format
/// churn can break.
fn parse_latest_release(body: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("releases/latest: not JSON: {e}"))?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "releases/latest: no tag_name".to_string())?;
    let url = v
        .get("html_url")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    Ok((tag.to_string(), url))
}

/// The real fetch, routed by token presence. Kept as one `pub` entry point so
/// the dispatch branch stays one line.
pub async fn fetch_latest(token: Option<String>) -> Result<String, String> {
    match token {
        None => fetch_latest_impl(None).await,
        Some(t) => fetch_latest_impl(Some(&t)).await,
    }
}

/// GitHub Releases "latest". One builder, used by both routes. GitHub requires
/// a User-Agent; the timeout is short (a failed check must never hold anything
/// up). The token goes in a HEADER only — never the URL, never a log (P16).
async fn fetch_latest_impl(token: Option<&str>) -> Result<String, String> {
    // The stamp lives in blitzkrieg-build-info's compilation unit; from here it
    // is read through BUILD_INFO, not env!() (the core crate has no stamps of
    // its own any more — §3.2: exactly one crate injects).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!(
            "BlitzkriegBot/{}",
            blitzkrieg_build_info::BUILD_INFO.version
        ))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut req = client
        .get(RELEASES_LATEST)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("GET releases/latest: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // 404 = private repo or no release yet (token territory); 403/429 = rate limit.
        return Err(format!("GET releases/latest: HTTP {status}"));
    }
    resp.text().await.map_err(|e| format!("read body: {e}"))
}

const RELEASES_LATEST: &str =
    "https://api.github.com/repos/ceer-quant/BlitzkriegBot/releases/latest";

/// Mount the startup check: never blocks startup, and does NOTHING at all
/// unless the switch is on (INV-3: no spawn, no request, no packet).
pub fn spawn_startup_check(state: Arc<UpdateState>, local_version: String, token: Option<String>) {
    if !state.snapshot().check_enabled {
        return;
    }
    tokio::spawn(async move {
        let at = now_ms() as u64;
        let fetch = fetch_latest(token);
        check_once(&state, &local_version, at, fetch).await;
        if let Err(e) = persist(&state) {
            tracing::warn!("update state persist failed (non-fatal): {e}");
        }
    });
}

/// `data/update/state.json` — the runtime switches (outrank the factory
/// `update.toml`, same model as `data/evolution/state.json`). Stores switches
/// and the last check summary only; NEVER a token.
fn state_path() -> std::path::PathBuf {
    std::path::PathBuf::from("data/update/state.json")
}

/// The caller-facing persist (cwd-relative, like every other `data/` writer).
pub fn persist(state: &UpdateState) -> std::io::Result<()> {
    persist_to(&state_path(), state)
}

pub fn persist_to(path: &std::path::Path, state: &UpdateState) -> std::io::Result<()> {
    let s = state.snapshot();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let doc = serde_json::json!({
        "checkEnabled": s.check_enabled,
        "autoUpdate": s.auto_update,
        "lastCheckMs": s.last_check_ms,
        "latest": s.latest,
    });
    // Atomic write: same-directory temp + rename, so a half-written file can
    // never be what the next startup reads.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, path)
}

/// Restore the switches (and the last-check summary) a previous run persisted.
/// The outcome itself deliberately does NOT survive a restart: "a check I ran
/// yesterday" is not "a check this process ran", so the payload stays null
/// until this process checks again. A missing or broken file is the normal
/// first-boot case → defaults, no drama.
pub fn load_into(state: &UpdateState) {
    load_from(&state_path(), state);
}

pub fn load_from(path: &std::path::Path, state: &UpdateState) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!(
            "{} is not valid JSON — update switches stay at their defaults",
            path.display()
        );
        return;
    };
    let check = v.get("checkEnabled").and_then(|b| b.as_bool());
    let auto = v.get("autoUpdate").and_then(|b| b.as_bool());
    state.set_enabled(check, auto);
    if let Some(ms) = v.get("lastCheckMs").and_then(|m| m.as_u64()) {
        state.last_check_ms.store(ms, Ordering::Relaxed);
    }
    if let Some(l) = v.get("latest").and_then(|l| l.as_str())
        && let Ok(mut g) = state.latest.lock()
    {
        *g = Some(l.to_string());
    }
}

/// One audit line for a switch change or a manual check. Best effort, like
/// every other JSONL trail in this kernel — the state cell is authoritative.
pub(crate) fn audit_event(value: &impl Serialize) {
    crate::jsonl::append(&std::path::PathBuf::from("data/update/audit.jsonl"), value);
}

#[cfg(test)]
mod tests {
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
        };
        assert_eq!(system_version_payload(b, &s).update_available, None);
        assert_eq!(system_version_payload(b, &s).last_check_ms, None);

        let s = UpdateSnapshot {
            outcome: CheckOutcome::Failed {
                detail: "dns".into(),
            },
            latest: None,
            release_url: None,
            last_check_ms: 1,
            check_enabled: true,
            auto_update: false,
        };
        assert_eq!(system_version_payload(b, &s).update_available, None);

        let s = UpdateSnapshot {
            outcome: CheckOutcome::UpToDate,
            latest: None,
            release_url: None,
            last_check_ms: 1,
            check_enabled: true,
            auto_update: false,
        };
        assert_eq!(system_version_payload(b, &s).update_available, Some(false));
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
}
