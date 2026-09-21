//! Build provenance (#179): stamp the revision this binary was compiled from, so
//! a running core can name its own code instead of being identified by `nm` and
//! guesswork.
//!
//! Why a build script instead of a `GIT_SHA=...` convention: a stamp that only
//! exists when whoever built the binary remembered to export it proves nothing,
//! and the failure it exists to prevent (#172) was exactly a gate whose
//! conclusion described a DIFFERENT code state than the one under test. Here the
//! value is derived from the work tree being compiled, at compile time, by the
//! compiler — not by the operator.
//!
//! Precedence: `BLITZKRIEG_GIT_SHA` (an explicit stamp from a packager or a
//! sandbox that has no `.git`) > `git rev-parse` in the package directory >
//! `nogit`. Degradation is deliberately not a build failure: a source tarball,
//! a vendored copy or a git-less CI image must still build, and `nogit` is
//! visibly not a revision.
//!
//! Dirty state is tracked separately from the revision token (`GIT_DIRTY`), so
//! `--version` keeps printing a *comparable* `<semver>+g<sha>` between CI, a
//! local build and the deployment — a `-dirty` suffix would silently break every
//! byte-for-byte comparison the provenance gate makes. Tracked files only: an
//! untracked scratch file does not make the built revision suspect.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // An explicit stamp changes what we compile, so a change to it must rebuild.
    println!("cargo:rerun-if-env-changed=BLITZKRIEG_GIT_SHA");

    let sha = env_sha()
        .or_else(git_sha)
        .unwrap_or_else(|| "nogit".to_string());
    let dirty = git_dirty();

    println!("cargo:rustc-env=BLITZKRIEG_GIT_SHA={sha}");
    println!(
        "cargo:rustc-env=BLITZKRIEG_GIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );

    // Re-stamp when the checkout moves: HEAD changes on every commit/checkout and
    // the refs change on every fetch. Both live outside this package (the repo
    // root owns `.git`), which is why absolute paths are printed explicitly.
    for name in ["HEAD", "refs"] {
        if let Some(p) = git_path(name) {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
}

/// The packager's explicit revision, normalised to the 12-char short form the
/// git path produces so the two sources are the same shape.
fn env_sha() -> Option<String> {
    let raw = std::env::var("BLITZKRIEG_GIT_SHA").ok()?;
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let head = match t.strip_prefix("g") {
        Some(rest) => rest,
        None => t,
    };
    Some(head.chars().take(12).collect())
}

fn git_sha() -> Option<String> {
    let out = git(&["rev-parse", "--short=12", "HEAD"])?;
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Any tracked modification means this binary does not correspond to the commit
/// it names. Untracked files are excluded on purpose: a scratch file next to the
/// checkout is not a modified build.
fn git_dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Absolute path of a path inside the repository's git dir, or `None` when there
/// is no repository (then there is nothing to watch).
fn git_path(name: &str) -> Option<PathBuf> {
    let out = git(&["rev-parse", "--git-path", name])?;
    let p = pkg_dir().join(out.trim());
    p.exists().then_some(p)
}

/// Run git in the package directory. Failure (no git, not a repository, a
/// non-zero exit) is `None`, never an error: provenance must not be able to
/// break a build.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(pkg_dir())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pkg_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()))
}
