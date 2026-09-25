#![forbid(unsafe_code)]
//! Build-time provenance stamping shared by every crate that surfaces version
//! info. Each such crate's `build.rs` is one line (`build_support::stamp()`),
//! so the injection rules live exactly once and `blitzkrieg version`, its
//! `--json` form, `--version` and the IPC answer cannot disagree (VERSIONING.md
//! §3.2; the runtime half of this contract lives in `core/build_info`).

use std::process::Command;

/// Emit every `cargo::rustc-env` stamp a crate needs. Call from `build.rs`.
///
/// Reads `CARGO_PKG_VERSION` (already resolved through
/// `[workspace.package].version` by cargo at this point) and `TARGET`, so a
/// crate that inherits its version cannot drift from the workspace root.
pub fn stamp() {
    // A changed input must retrigger the build script, or the stamp freezes at
    // its old value — the kind of stale-version bug that is nearly undetectable.
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=BLITZKRIEG_GIT_SHA"); // 兼容 0.4：显式盖章入口名不变
    println!("cargo::rerun-if-env-changed=BLITZKRIEG_BUILD_DATE");
    // The version itself comes from the root Cargo.toml; pointing rerun-if-changed
    // at it is what makes a version bump take effect.
    for p in ["../../Cargo.toml", "../../Cargo.lock"] {
        println!("cargo::rerun-if-changed={p}");
    }

    emit("BLITZKRIEG_VERSION", &version());
    emit("BLITZKRIEG_GIT_HASH", &git_hash());
    emit("BLITZKRIEG_GIT_DIRTY", if git_dirty() { "1" } else { "0" });
    emit("BLITZKRIEG_BUILD_DATE", &build_date());
    emit("BLITZKRIEG_TARGET", &target());

    // HEAD moves on every commit/checkout; refs move on every fetch.
    for name in ["HEAD", "refs"] {
        if let Some(p) = git_path(name) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
}

fn emit(key: &str, value: &str) {
    println!("cargo::rustc-env={key}={value}");
}

/// The workspace version cargo already resolved for this crate.
pub fn version() -> String {
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into())
}

/// The target triple this build is for.
pub fn target() -> String {
    std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
}

/// Build timestamp. `BLITZKRIEG_BUILD_DATE` (an explicit stamp from a
/// reproducible packager) wins over the clock.
pub fn build_date() -> String {
    resolve_date(non_empty_env("BLITZKRIEG_BUILD_DATE"))
}

/// Explicit stamp wins; otherwise read the clock. Split out so the precedence
/// rule is testable without mutating the process environment.
pub fn resolve_date(explicit: Option<String>) -> String {
    explicit.unwrap_or_else(utc_now)
}

/// Revision token: `BLITZKRIEG_GIT_SHA` > `git rev-parse` > `nogit`.
/// Degradation is never a build failure — a source tarball must still build.
pub fn git_hash() -> String {
    env_sha()
        .or_else(git_sha)
        .unwrap_or_else(|| "nogit".to_string())
}

pub fn git_dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn non_empty_env(key: &str) -> Option<String> {
    let raw = std::env::var(key).ok()?;
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn env_sha() -> Option<String> {
    let t = non_empty_env("BLITZKRIEG_GIT_SHA")?;
    let head = t.strip_prefix('g').unwrap_or(&t);
    Some(head.chars().take(12).collect()) // normalize to 12 hex, same shape as the git path
}

fn git_sha() -> Option<String> {
    let out = git(&["rev-parse", "--short=12", "HEAD"])?;
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn git_path(name: &str) -> Option<std::path::PathBuf> {
    let out = git(&["rev-parse", "--git-path", name])?;
    let p = pkg_dir().join(out.trim());
    p.exists().then_some(p)
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(pkg_dir())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pkg_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()))
}

/// Zero-dependency `YYYYMMDDTHHMMSSZ`. `date -u` when available (exact, and
/// what the repo's ops scripts already use); otherwise a civil-calendar
/// conversion so a dated stamp never needs an external binary.
fn utc_now() -> String {
    #[cfg(unix)]
    if let Some(s) = Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.len() == 16)
    {
        return s;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_unix(secs as i64)
}

/// Days-since-epoch → civil date (Hinnant's algorithm) → UTC stamp.
pub fn civil_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epochs_convert_exactly() {
        assert_eq!(civil_from_unix(0), "19700101T000000Z");
        // The same instant the core's data-lock test pins, so time agrees.
        assert_eq!(civil_from_unix(1_758_451_199), "20250921T103959Z");
        assert_eq!(civil_from_unix(951_782_400), "20000229T000000Z");
        assert_eq!(civil_from_unix(2_147_483_647), "20380119T031407Z");
        assert_eq!(civil_from_unix(-1), "19691231T235959Z");
    }

    #[test]
    fn an_explicit_stamp_wins_over_the_clock() {
        assert_eq!(
            resolve_date(Some("20260101T000000Z".to_string())),
            "20260101T000000Z"
        );
        let from_clock = resolve_date(None);
        assert_eq!(from_clock.len(), 16, "{from_clock}");
        assert!(from_clock.ends_with('Z'), "{from_clock}");
    }

    #[test]
    fn version_comes_from_cargo_not_a_literal() {
        assert_eq!(version(), std::env::var("CARGO_PKG_VERSION").unwrap());
    }

    #[test]
    fn a_build_with_no_git_still_names_a_token() {
        assert!(!git_hash().is_empty());
    }
}
