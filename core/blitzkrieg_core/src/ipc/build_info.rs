//! Build provenance, readable at runtime (#179).
//!
//! `build.rs` stamps the revision of the compiled tree; this module is the ONE
//! spelling of it, so `--version`, the startup line and `core.ready` cannot
//! disagree about which code is running. A gate compares that stamp against the
//! commit it is checking, which is what turns "the parity gate is green" into
//! "the parity gate is green *for this code*" (#172).
//!
//! The revision is a plain token, so it can be compared byte-for-byte between a
//! CI log, a local build and a deployment. Whether the tree was dirty at build
//! time is reported separately rather than glued onto the token: `<ver>+g<sha>`
//! stays comparable, and "built from a modified tree" is still visible.

/// Short git revision stamped by `build.rs`; `nogit` when the build had no
/// repository and no explicit `BLITZKRIEG_GIT_SHA` stamp.
pub const GIT_SHA: &str = env!("BLITZKRIEG_GIT_SHA");

/// `"1"` when tracked files were modified at build time, `"0"` otherwise.
pub const GIT_DIRTY: &str = env!("BLITZKRIEG_GIT_DIRTY");

/// True when the binary was built from a tree with tracked changes.
pub fn is_dirty() -> bool {
    GIT_DIRTY == "1"
}

/// True when the build could not determine a revision at all.
pub fn revision_unknown() -> bool {
    GIT_SHA == "nogit"
}

/// The version string, e.g. `0.2.0+g96fcf51a1b2` (`<semver>+g<sha>`), or
/// `0.2.0+nogit` for a build with no revision to name.
pub fn version_string() -> String {
    format!("{}+{}", crate::CORE_VERSION, revision_token())
}

/// `g<sha>` for a real revision, `nogit` otherwise — the build-metadata part
/// alone, for callers that spell the version differently.
pub fn revision_token() -> String {
    if revision_unknown() {
        "nogit".to_string()
    } else {
        format!("g{GIT_SHA}")
    }
}

/// One line naming the code this process is: revision, dirty flag and version —
/// what an incident review needs in order to pin a log to a commit.
pub fn provenance_line() -> String {
    format!(
        "version {} (git {}, {} build)",
        version_string(),
        GIT_SHA,
        if is_dirty() { "dirty" } else { "clean" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_semver_plus_revision() {
        let v = version_string();
        let (semver, meta) = v.split_once('+').expect("version must carry +<metadata>");
        assert_eq!(semver, crate::CORE_VERSION);
        assert_eq!(meta, revision_token());
        assert!(
            meta == "nogit" || (meta.starts_with('g') && meta.len() > 1),
            "unexpected revision token: {meta}"
        );
    }

    #[test]
    fn a_missing_revision_is_named_not_faked() {
        // The whole point of the `nogit` spelling: a build that cannot name its
        // code says so, and never invents a claim. Asserted on the pure
        // formatting rule so it holds whatever this build's stamp happens to be.
        let token = if revision_unknown() {
            "nogit".to_string()
        } else {
            format!("g{GIT_SHA}")
        };
        assert!(!token.is_empty());
        assert_ne!(token, "g");
    }

    #[test]
    fn provenance_line_names_the_version_and_the_tree_state() {
        let line = provenance_line();
        assert!(line.contains(&version_string()), "{line}");
        assert!(line.contains(GIT_SHA), "{line}");
        assert!(
            line.contains(if is_dirty() { "dirty" } else { "clean" }),
            "{line}"
        );
    }
}
