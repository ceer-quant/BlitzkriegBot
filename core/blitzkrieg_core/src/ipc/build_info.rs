//! Build provenance (#179). The stamping and formatting live in
//! `blitzkrieg-build-info`; this module only keeps the kernel's existing names
//! working so the move to the shared crate (VERSIONING.md §3.4) does not touch
//! any call site.
pub use blitzkrieg_build_info::BUILD_INFO;

/// Short git revision; `nogit` when the build had no repository and no explicit
/// `BLITZKRIEG_GIT_SHA` stamp.
pub const GIT_SHA: &str = BUILD_INFO.git_hash;

/// `"1"` when tracked files were modified at build time, `"0"` otherwise.
pub const GIT_DIRTY: &str = BUILD_INFO.git_dirty;

/// True when the binary was built from a tree with tracked changes.
pub fn is_dirty() -> bool {
    BUILD_INFO.is_dirty()
}

/// True when the build could not determine a revision at all.
pub fn revision_unknown() -> bool {
    BUILD_INFO.revision_unknown()
}

/// `g<sha>` for a real revision, `nogit` otherwise — the build-metadata part
/// alone, for callers that spell the version differently.
pub fn revision_token() -> String {
    BUILD_INFO.revision_token()
}

/// The version string, e.g. `0.2.1+g96fcf51a1b2` (`<semver>+g<sha>`), or
/// `0.2.1+nogit` for a build with no revision to name.
pub fn version_string() -> String {
    BUILD_INFO.version_string()
}

/// One line naming the code this process is: revision, dirty flag and version —
/// what an incident review needs in order to pin a log to a commit.
pub fn provenance_line() -> String {
    BUILD_INFO.provenance_line()
}

#[cfg(test)]
mod tests {
    /// The wiring contract of the re-export: `CORE_VERSION` (what the IPC READY
    /// envelope and the lock record use) must be the semver half of
    /// `version_string()` (what `--version` prints and the provenance gate
    /// parses). If these drift, every gate loses its "which code is this" anchor.
    #[test]
    fn core_version_is_the_semver_half_of_the_version_string() {
        let v = super::version_string();
        let (semver, meta) = v.split_once('+').expect("version must carry +<metadata>");
        assert_eq!(semver, crate::CORE_VERSION);
        assert_eq!(meta, super::revision_token());
    }
}
