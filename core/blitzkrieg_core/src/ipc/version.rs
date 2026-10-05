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

impl StagePhase {
    /// The token the audit line, the persist file and the wire use.
    pub fn token(&self) -> &'static str {
        match self {
            StagePhase::Idle => "idle",
            StagePhase::Downloading => "downloading",
            StagePhase::Staged => "staged",
            StagePhase::Failed => "failed",
        }
    }

    /// Inverse of [`Self::token`]; an unknown token is `Idle`, never a guess.
    pub fn from_token(t: &str) -> Self {
        match t {
            "downloading" => StagePhase::Downloading,
            "staged" => StagePhase::Staged,
            "failed" => StagePhase::Failed,
            _ => StagePhase::Idle,
        }
    }
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
    /// #379 (§7.4): seconds between AUTOMATIC checks. `0` = the scheduler
    /// must not exist (the shipped default — INV-3 governs the default, the
    /// timer is an explicit opt-in).
    auto_check_interval_secs: AtomicU64,
    /// #379 (§7.5): the download/staging state machine. The kernel NEVER
    /// replaces the binary it is running (§7.5's iron rule) — the furthest
    /// this state machine can go is `Staged`, "a restart-ready asset sits in
    /// the staging directory".
    stage: Mutex<StagePhase>,
    stage_detail: Mutex<Option<String>>,
    stage_version: Mutex<Option<String>>,
    stage_asset: Mutex<Option<String>>,
    stage_sha256: Mutex<Option<String>>,
    stage_at_ms: AtomicU64,
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
            auto_check_interval_secs: AtomicU64::new(0),
            stage: Mutex::new(StagePhase::default()),
            stage_detail: Mutex::new(None),
            stage_version: Mutex::new(None),
            stage_asset: Mutex::new(None),
            stage_sha256: Mutex::new(None),
            stage_at_ms: AtomicU64::new(0),
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

    /// The automatic-check interval (secs; `0` = no scheduler).
    pub fn auto_check_interval_secs(&self) -> u64 {
        self.auto_check_interval_secs.load(Ordering::Relaxed)
    }

    /// Set the interval (boot config only — `system.update.configure` writes
    /// the switches, not the clock; the clock is restart-to-change so a UI
    /// toggle can never silently re-arm a timer).
    pub fn set_auto_check_interval_secs(&self, secs: u64) {
        self.auto_check_interval_secs.store(secs, Ordering::Relaxed);
    }

    pub fn stage_snapshot(&self) -> StageState {
        StageState {
            phase: self
                .stage
                .lock()
                .map(|g| g.clone())
                .unwrap_or(StagePhase::Idle),
            detail: self.stage_detail.lock().ok().and_then(|g| g.clone()),
            version: self.stage_version.lock().ok().and_then(|g| g.clone()),
            asset: self.stage_asset.lock().ok().and_then(|g| g.clone()),
            sha256: self.stage_sha256.lock().ok().and_then(|g| g.clone()),
            staged_at_ms: {
                let ms = self.stage_at_ms.load(Ordering::Relaxed);
                (ms > 0).then_some(ms)
            },
        }
    }

    /// Stage-state reset for a new run (a new check that finds nothing newer,
    /// or an explicit re-stage). Previous bytes, if any, are the caller's to
    /// clean — this only moves the machine.
    fn set_stage(&self, s: StageState) {
        if let Ok(mut g) = self.stage.lock() {
            *g = s.phase;
        }
        if let Ok(mut g) = self.stage_detail.lock() {
            *g = s.detail;
        }
        if let Ok(mut g) = self.stage_version.lock() {
            *g = s.version;
        }
        if let Ok(mut g) = self.stage_asset.lock() {
            *g = s.asset;
        }
        if let Ok(mut g) = self.stage_sha256.lock() {
            *g = s.sha256;
        }
        self.stage_at_ms
            .store(s.staged_at_ms.unwrap_or(0), Ordering::Relaxed);
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

/// The download/staging state machine (VERSIONING.md §7.5, #379). FOUR
/// phases, and the machine is READ-only for everyone but `stage_once`: the
/// furthest it can advance is `Staged`. There is no `Installed` phase ON
/// PURPOSE — replacing the binary a position-holding process is executing is
/// the launcher's job (§7.5's iron rule), and a state that invites the core
/// to walk one step further than `Staged` is a state that will eventually be
/// walked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StagePhase {
    /// Nothing staged (the default, and the state after a failed run lands).
    #[default]
    Idle,
    /// A fetch is running right now (one at a time — a second caller during
    /// a download must not start a second one).
    Downloading,
    /// A restart-ready release asset sits in the staging directory, byte-verified
    /// against the release's `SHA256SUMS` entry.
    Staged,
    /// The fetch or the verification failed; `detail` says what happened.
    /// The previous `Staged` state, if any, is NOT clobbered by an unrelated
    /// failure (a failed re-stage must not destroy the good copy it sits on).
    Failed,
}

/// One consistent copy of the staging state.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageState {
    pub phase: StagePhase,
    /// Operator-facing reason for `Failed`, or the verify summary for `Staged`.
    pub detail: Option<String>,
    /// Which version the staged asset is (the checked-out target).
    pub version: Option<String>,
    /// The asset file name inside the staging directory.
    pub asset: Option<String>,
    /// The digest the bytes were verified against (from the release SHA256SUMS).
    pub sha256: Option<String>,
    /// When the asset was staged (UTC ms); `None` while never staged.
    pub staged_at_ms: Option<u64>,
}

/// The runtime switch (§7.4): "may the kernel stage a newer release at all".
/// ONE guard for BOTH the automatic path (scheduler) and the manual
/// `system.update.stage` call — the switch is about the download, not about
/// who pressed the button. Default off, restored with the other switches.
pub fn stage_enabled(state: &UpdateState) -> bool {
    state.snapshot().auto_update
}

/// The staging directory (cwd-relative, like every other `data/` writer).
pub fn staging_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("data/update/staging")
}

/// Where the release's `SHA256SUMS` asset is kept once fetched.
fn staging_sums_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("SHA256SUMS")
}

/// Fetch the release asset (`asset_name`) and the `SHA256SUMS` manifest from a
/// GitHub release, routed by token presence (P16: the token is a HEADER, never
/// the URL and never a log). Returns the archive bytes and the manifest text.
pub async fn fetch_release_assets(
    tag: &str,
    version: &str,
    target: &str,
    token: Option<String>,
) -> Result<(Vec<u8>, String), String> {
    fetch_release_assets_impl(tag, version, target, token.as_deref()).await
}

async fn fetch_release_assets_impl(
    tag: &str,
    version: &str,
    target: &str,
    token: Option<&str>,
) -> Result<(Vec<u8>, String), String> {
    let asset = asset_name(version, target);
    let base = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{tag}");
    let client = reqwest::Client::builder()
        // A download is bigger than a check but still bounded: two minutes is
        // generous for a release archive and stops a stalled socket from
        // holding the staging state in Downloading forever.
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!(
            "BlitzkriegBot/{}",
            blitzkrieg_build_info::BUILD_INFO.version
        ))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut req = client
        .get(&base)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| format!("GET release: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("GET release: HTTP {status}"));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("release: not JSON: {e}"))?;
    // Both reads key on the asset NAME — anything else (browser URLs, other
    // assets) is a wrong shape, not a fallback.
    let mut archive_url: Option<String> = None;
    let mut sums_url: Option<String> = None;
    if let Some(items) = body.get("assets").and_then(|a| a.as_array()) {
        for it in items {
            let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let url = it
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            if name == asset {
                archive_url = Some(url.to_string());
            }
            if name == SUMS_ASSET {
                sums_url = Some(url.to_string());
            }
        }
    }
    let sums_url =
        sums_url.ok_or_else(|| format!("release {tag}: no {SUMS_ASSET} asset — refusing (N10)"))?;
    let archive_url =
        archive_url.ok_or_else(|| format!("release {tag}: no {asset} asset for this target"))?;
    let (archive, sums) = tokio::try_join!(
        fetch_bytes(&client, &archive_url, token),
        fetch_bytes(&client, &sums_url, token)
    )?;
    Ok((archive, String::from_utf8_lossy(&sums).into_owned()))
}

async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut req = client.get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}"));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("read {url}: {e}"))
}

/// The GitHub repo whose releases are staged from (matches `fetch_latest`).
const GITHUB_REPO: &str = "ceer-quant/BlitzkriegBot";
/// The manifest asset the release workflow publishes (`SHA256SUMS`).
const SUMS_ASSET: &str = "SHA256SUMS";

/// The injected fetch of [`stage_once`]: given the release tag and the
/// checked-out version, produce the (archive bytes, SHA256SUMS text) pair.
/// A named trait rather than an `FnOnce -> impl Future` bound — the latter
/// is not expressible on stable, and this keeps the signature honest about
/// "one fetch per run".
pub trait AsyncStageFetch {
    fn fetch(
        self,
        tag: String,
        version: String,
    ) -> impl std::future::Future<Output = Result<(Vec<u8>, String), String>>;
}

impl<F, Fut> AsyncStageFetch for F
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<u8>, String), String>>,
{
    fn fetch(
        self,
        tag: String,
        version: String,
    ) -> impl std::future::Future<Output = Result<(Vec<u8>, String), String>> {
        self(tag, version)
    }
}

/// One staging run. Mirrors `check_once`'s shape: the fetch is injected so
/// tests use fake HTTP, nothing escapes as an error, and every ending is a
/// state the UI can render. The fetch is a CLOSURE of the release tag (and
/// the checked-out version), because which URL to dial is only known after
/// the guard phase — the same dependency `check_once` hides behind a plain
/// future whose construction is guarded by the switch.
///
/// Guard order is the contract (§7.5):
///   1. `autoUpdate` off → nothing happens at all (the switch guards the
///      download, whoever asks for it — INV-3's runtime half);
///   2. no `Available` verdict → nothing to stage (this is not a failure);
///   3. one run at a time;
///   4. only then bytes move, into `data/update/staging/`, never at the
///      running binary.
pub async fn stage_once(
    state: &Arc<UpdateState>,
    local_target: &str,
    at_ms: u64,
    staging: &std::path::Path,
    fetch: impl AsyncStageFetch,
) {
    let snap = state.snapshot();
    if !snap.auto_update {
        // The switch is off → not a single byte fetched (the structural form
        // of INV-3, same shape as check_once's disabled test).
        return;
    }
    let tag = match (&snap.outcome, snap.latest.as_deref()) {
        (CheckOutcome::Available, Some(_)) => match snapshot_tag(&snap) {
            Some(t) => t,
            None => {
                // An Available verdict without a tag is a state this module
                // does not produce; refuse loudly rather than guess.
                state.set_stage(StageState {
                    phase: StagePhase::Failed,
                    detail: Some("staging refused: no release tag recorded".into()),
                    ..StageState::default()
                });
                return;
            }
        },
        _ => {
            // No newer release known → nothing to do. Not an error, and NOT
            // a clobber of whatever good copy is already staged.
            return;
        }
    };
    let version = snap.latest.clone().unwrap_or_default();
    // One staging run at a time: the second caller sees `Downloading` and
    // leaves without starting a parallel download of the same asset.
    if state.stage_snapshot().phase == StagePhase::Downloading {
        return;
    }
    // A FAILED run must not destroy a previously staged good copy's identity:
    // the operator still holds a restart-ready asset, and the summary saying
    // which version it is survives the failed attempt (only a successful run
    // overwrites it).
    let prior = state.stage_snapshot();
    state.set_stage(StageState {
        phase: StagePhase::Downloading,
        version: Some(version.clone()),
        ..StageState::default()
    });
    let asset = asset_name(&version, local_target);
    let failed = |detail: String| StageState {
        phase: StagePhase::Failed,
        detail: Some(detail),
        version: prior.version.clone(),
        asset: prior.asset.clone(),
        sha256: prior.sha256.clone(),
        staged_at_ms: prior.staged_at_ms,
    };
    match fetch.fetch(tag, version.clone()).await {
        Ok((bytes, sums)) => {
            let want = expected_sha256(&sums, &asset)
                .ok_or_else(|| format!("{SUMS_ASSET} has no entry for {asset} — refusing (N10)"));
            let verified = want.and_then(|want| verify_sha256(&bytes, &want).map(|_| want));
            match verified {
                Ok(sha) => {
                    // Persist the verified bytes: same-directory temp + rename
                    // per file, so a half-written archive can never be what a
                    // restart finds.
                    let fail = (|| -> std::io::Result<()> {
                        std::fs::create_dir_all(staging)?;
                        let tmp = staging.join(format!(".{}.tmp", asset));
                        std::fs::write(&tmp, &bytes)?;
                        std::fs::rename(&tmp, staging.join(&asset))?;
                        let sums_tmp = staging.join(".SHA256SUMS.tmp");
                        std::fs::write(&sums_tmp, &sums)?;
                        std::fs::rename(&sums_tmp, staging_sums_path(staging))
                    })()
                    .map_err(|e| format!("staging write: {e}"));
                    match fail {
                        Ok(()) => state.set_stage(StageState {
                            phase: StagePhase::Staged,
                            detail: Some(format!(
                                "sha256 verified ({}), restart applies it (launcher-side)",
                                &sha[..8.min(sha.len())]
                            )),
                            version: Some(version),
                            asset: Some(asset),
                            sha256: Some(sha),
                            staged_at_ms: Some(at_ms),
                        }),
                        Err(e) => state.set_stage(failed(e)),
                    }
                }
                Err(e) => {
                    // A hash mismatch: nothing of THIS run was written (the
                    // verify happens before any write), and the prior good
                    // copy — if any — is a different asset than the bad one
                    // would have been; leave the directory as it was.
                    state.set_stage(failed(e));
                }
            }
        }
        Err(e) => {
            // Fetch refused before bytes landed: leave any previous good
            // staging copy alone, just record the failure.
            state.set_stage(failed(e));
        }
    }
}

/// The release tag for the current `Available` verdict. The check side keeps
/// `latest` as the bare semver; the tag it came from is recoverable by the
/// same strict rule (`v` + version) the check used going the other way.
fn snapshot_tag(snap: &UpdateSnapshot) -> Option<String> {
    snap.latest.as_deref().map(|v| format!("v{v}"))
}

/// Asset name for a release archive: `blitzkrieg-<version>-<target>.tar.gz`,
/// the exact spelling the release workflow stages (§7.5). The target TRIPLE
/// is matched, never "guessed per platform" — guessing wrong would stage a
/// binary that cannot run.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("blitzkrieg-{version}-{target}.tar.gz")
}

/// The expected SHA256 for `asset` out of a `SHA256SUMS` document (one
/// `<hex>  <name>` line per file, `*`-prefixed binary-mode names tolerated).
/// NO entry = verification is impossible = refuse (N10): the most common
/// downgrade path is "missing digest → skip verification", and it must not
/// exist here.
pub fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hex = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        let hex_plausible = hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit());
        (name == asset && hex_plausible).then(|| hex.to_ascii_lowercase())
    })
}

/// Full 64-hex comparison (sha2). Constant time is unnecessary (this is not a
/// key); comparing the COMPLETE digest is not negotiable — an 8-char compare
/// "looks verified" and verifies nothing (N8).
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::digest(bytes));
    if got == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!("sha256 mismatch: got {got}, want {expected}"))
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

/// The automatic-check scheduler (VERSIONING.md §7.4, #379): when the interval
/// is configured (secs > 0) and `check_enabled` is on, re-runs the check every
/// interval until the server shuts down. Guarded by the same two conditions as
/// everything else in this module:
///   * `check_enabled=false` → the task does not exist (no spawn, no request);
///   * interval 0 (the default) → the task does not exist either — the timer
///     is an explicit configuration choice, never a default.
///
/// A check failure never touches the trading process: the outcome lands in the
/// state cell (check_once), the persist is best-effort, and the loop sleeps on.
/// The interval is read from the cell each round, so a future operator surface
/// that changes it takes effect on the next round without a restart.
pub fn spawn_auto_check_scheduler(
    state: Arc<UpdateState>,
    local_version: String,
    token: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let interval = state.auto_check_interval_secs();
    let check_enabled = state.snapshot().check_enabled;
    if interval == 0 || !check_enabled {
        // Deliberately NO task: with the shipped defaults the stack makes zero
        // outbound connections (INV-3 is about tasks, not just packets).
        return tokio::spawn(std::future::ready(()));
    }
    let secs = interval;
    tokio::spawn(async move {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let sleep = tokio::time::sleep(std::time::Duration::from_secs(secs.max(1)));
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut shutdown => break,
                _ = &mut sleep => {}
            }
            // Re-read the switch each round: an operator who turns checking
            // off via `system.update.configure` stops the outbound traffic on
            // the next boundary without waiting for a restart.
            if !state.snapshot().check_enabled {
                continue;
            }
            let at = now_ms() as u64;
            let fetch = fetch_latest(token.clone());
            check_once(&state, local_version.as_str(), at, fetch).await;
            if let Err(e) = persist(&state) {
                tracing::warn!("update state persist failed (non-fatal): {e}");
            }
        }
    })
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
    let stage = state.stage_snapshot();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let doc = serde_json::json!({
        "checkEnabled": s.check_enabled,
        "autoUpdate": s.auto_update,
        "lastCheckMs": s.last_check_ms,
        "latest": s.latest,
        // #379: the configured interval is restart-to-change configuration and
        // survives like the switches; the staging SUMMARY survives so the UI
        // can say "a restart-ready copy of vX sat here" — the staging BYTES
        // themselves live in data/update/staging/ and are only trusted by a
        // launcher that re-verifies them (§7.5).
        "autoCheckIntervalSecs": state.auto_check_interval_secs(),
        "stage": {
            "phase": stage.phase.token(),
            "version": stage.version,
            "asset": stage.asset,
            "sha256": stage.sha256,
            "stagedAtMs": stage.staged_at_ms,
        },
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
    // #379: the interval and the staging summary ride the same file. A
    // persisted `staged` phase is restored as `staged` ONLY as a summary —
    // the launcher re-verifies the bytes before it installs anything, so a
    // lying file can waste a restart but never install wrong bytes.
    if let Some(secs) = v.get("autoCheckIntervalSecs").and_then(|m| m.as_u64()) {
        state.set_auto_check_interval_secs(secs);
    }
    if let Some(stage) = v.get("stage").and_then(|s| s.as_object()) {
        let phase = stage
            .get("phase")
            .and_then(|p| p.as_str())
            .map(StagePhase::from_token)
            .unwrap_or_default();
        let version = stage
            .get("version")
            .and_then(|x| x.as_str())
            .map(String::from);
        let asset = stage
            .get("asset")
            .and_then(|x| x.as_str())
            .map(String::from);
        let sha = stage
            .get("sha256")
            .and_then(|x| x.as_str())
            .map(String::from);
        let at = stage.get("stagedAtMs").and_then(|x| x.as_u64());
        let has_version = version.is_some();
        state.set_stage(StageState {
            detail: None,
            version,
            asset,
            sha256: sha,
            staged_at_ms: at,
            phase: match (phase, has_version) {
                (StagePhase::Staged, true) => StagePhase::Staged,
                (StagePhase::Staged, false) => StagePhase::Idle,
                (p, _) => p,
            },
        });
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
        // Second run's fetch fails at the network level.
        stage_once(&state, "t", 2, &dir, |_, _| async move {
            Err("connection refused".into())
        })
        .await;
        let s = state.stage_snapshot();
        assert_eq!(s.phase, StagePhase::Failed);
        // …but the good copy's identity survives the failed attempt.
        assert_eq!(s.version.as_deref(), Some("0.3.1"));
        assert_eq!(s.asset.as_deref(), Some(asset.as_str()));
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
}
